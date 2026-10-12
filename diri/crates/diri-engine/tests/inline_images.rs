//! A local terminal answers Kitty's support query, places an image a
//! program prints, and serves its bytes by the anchor key in the grid.
//!
//! Real shells on real PTYs, for both ways a local session owns one.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{GridSignature, HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::inline_image::{ImageAnchor, InlineImageFormat};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-images-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn shell(id: &str, root: &Path, holder: Option<HolderConfig>) -> Session {
    let spec = SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-i".into()], root)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("PS1", "$ ")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder,
        remote: None,
        defer_launch: false,
    };
    Session::spawn(spec, engine()).expect("spawn")
}

fn wait_until(session: &Session, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            session.screen_lines()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn at_prompt(session: &Session) -> bool {
    session
        .screen_lines()
        .last()
        .is_some_and(|line| line.trim() == "$")
}

fn anchors(session: &Session) -> Vec<(u16, u16, u16, ImageAnchor)> {
    let grid = session
        .grid_update_if_changed(&mut GridSignature::default())
        .expect("grid");
    grid.changed_rows
        .iter()
        .flat_map(|row| {
            row.metadata.links.iter().filter_map(move |link| {
                ImageAnchor::parse(&link.uri).map(|anchor| (row.y, link.start, link.end, anchor))
            })
        })
        .collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

fn exercise(session: &Session, root: &Path) {
    wait_until(session, "the prompt", || at_prompt(session));

    // Support detection, as kitty's own tools do it: the reply comes back
    // as the program's input, ahead of the device attributes.
    session
        .write_input(
            b"stty raw -echo; printf '\\033_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\\033\\\\'; dd bs=1 count=12 2>/dev/null | od -An -c; stty sane\r",
        )
        .unwrap();
    wait_until(session, "the query's answer", || {
        session
            .screen_lines()
            .iter()
            .any(|line| line.contains("_   G   i   =   3   1   ;   O   K"))
    });
    wait_until(session, "the prompt after the query", || at_prompt(session));

    // Larger than one read and one control-line chunk, so the stream and
    // the chunked read both split it.
    let mut file = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    file.extend_from_slice(&64u32.to_be_bytes());
    file.extend_from_slice(&68u32.to_be_bytes());
    file.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    file.extend((0..3_000_000u32).map(|i| (i * 7) as u8));
    let sequence = format!("\x1b]1337;File=inline=1:{}\x07", base64(&file));
    std::fs::write(root.join("image.seq"), sequence).unwrap();
    session
        .write_input(b"clear; cat image.seq; echo done\r")
        .unwrap();
    // The cursor stays after the image's last column on its last row.
    wait_until(session, "the image", || {
        session.screen_lines().iter().any(|line| line == "    done")
    });

    let found = anchors(session);
    // 64×68 px at the default 16×34 px cell: 4 columns by 2 rows.
    assert_eq!(found.len(), 2, "{found:?}");
    let (y, start, end, anchor) = &found[0];
    assert_eq!((*y, *start, *end), (0, 0, 4));
    assert_eq!((anchor.cols, anchor.rows), (4, 2));

    let stored = session.inline_image(&anchor.key).expect("stored");
    assert_eq!(stored.format, InlineImageFormat::Png);
    assert_eq!((stored.width, stored.height), (64, 68));
    assert_eq!(&*stored.data, file.as_slice());
}

#[test]
fn inline_images_in_a_direct_terminal() {
    let root = root("direct");
    let session = shell("images_direct", &root, None);
    exercise(&session, &root);
    drop(session);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn inline_images_in_a_held_terminal() {
    let root = root("held");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };
    let session = shell("images_held", &root, Some(holder));
    exercise(&session, &root);
    drop(session);
    let _ = std::fs::remove_dir_all(&root);
}
