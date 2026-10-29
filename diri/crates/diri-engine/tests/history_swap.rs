//! Idle history of a held session moves to its encrypted swap file and back.
//!
//! One test per binary: the page-out idle period comes from the process
//! environment, read once.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use diri_engine::session::{HISTORY_PAGED_OUT_BYTES, HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn swap_files(logs: &Path) -> Vec<PathBuf> {
    let root = logs.join(diri_engine::history_swap::DIR_NAME);
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|dir| {
            std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|file| file.path())
        .collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn idle_history_pages_out_reads_back_and_leaves_with_its_session() {
    // SAFETY: set before any session (or other thread) reads it; this
    // binary runs this one test.
    unsafe { std::env::set_var("DIRI_HISTORY_SWAP_IDLE_MS", "400") };
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    let history = root.path().join("history.txt");
    let mut text = String::new();
    for line in 0..8_000 {
        text.push_str(&format!(
            "\x1b[2m{line:06}\x1b[0m \x1b[38;5;{}mmodule::{}\x1b[0m idle history line {}\r\n",
            16 + line % 200,
            line % 97,
            "x".repeat(line % 60)
        ));
    }
    std::fs::write(&history, text).unwrap();
    // A second burst, printed once the test says so: new history that
    // compresses poorly, so more than the 64 KiB minimum moves.
    let more = root.path().join("more.txt");
    let go = root.path().join("go");
    let mut text = String::new();
    let mut seed = 0x5eed_u64;
    for line in 0..4_000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = seed ^ (seed >> 29);
        text.push_str(&format!(
            "more output {line:05} {noise:016x}{:016x}\r\n",
            noise.rotate_left(17)
        ));
    }
    std::fs::write(&more, text).unwrap();
    let spec = SessionSpec {
        id: "s_swap_idle".into(),
        pty: PtySpec::new(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "cat {}; echo ready; while [ ! -e {} ]; do sleep 0.05; done; cat {}; echo again; exec cat",
                    history.display(),
                    go.display(),
                    more.display()
                ),
            ],
            "/tmp",
        )
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .size(100, 30),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: logs.clone(),
        holder: Some(HolderConfig {
            holders_dir: root.path().join("holders"),
            executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
        }),
        remote: None,
        defer_launch: false,
    };
    let mut session = Session::spawn(spec, engine()).expect("spawn held shell");
    wait_until("output", Duration::from_secs(30), || {
        session
            .screen_lines()
            .iter()
            .any(|line| line.trim_end() == "ready")
    });
    let rows = session.inspect_screen(|screen| screen.history_rows()) as i64;
    let resident_oldest = session.read_scrollback_cells(0, 30);

    // Idle: the pump parks, and its park timer pages history out.
    wait_until("history paged out", Duration::from_secs(20), || {
        session.inspect_screen(|screen| screen.history_residency().stored) > 0
    });
    let first_transition = HISTORY_PAGED_OUT_BYTES.load(Ordering::Relaxed);
    let residency = session.inspect_screen(|screen| screen.history_residency());
    assert!(residency.stored_bytes > residency.resident_bytes);
    let files = swap_files(&logs);
    assert_eq!(files.len(), 1, "one swap file per session");
    assert_eq!(mode(&files[0]), 0o600);
    assert_eq!(mode(files[0].parent().unwrap()), 0o700);
    assert_eq!(mode(&logs.join(diri_engine::history_swap::DIR_NAME)), 0o700);
    let disk = std::fs::read(&files[0]).unwrap();
    assert!(
        !disk
            .windows(17)
            .any(|window| window == b"idle history line"),
        "nothing on disk is plaintext"
    );

    // Paged-out history reads back exactly, and stays paged out.
    let paged_oldest = session.read_scrollback_cells(0, 30);
    assert_eq!(paged_oldest.payload, resident_oldest.payload);
    assert_eq!(paged_oldest.metadata, resident_oldest.metadata);
    assert_eq!(
        session.inspect_screen(|screen| screen.history_rows()) as i64,
        rows
    );
    assert_eq!(
        session.inspect_screen(|screen| screen.history_residency()),
        residency
    );

    // New output brings history back to life; the next idle transition
    // writes only what was sealed since (under 64 KiB of it would stay in
    // memory).
    std::fs::write(&go, b"").unwrap();
    wait_until("second burst", Duration::from_secs(30), || {
        session
            .screen_lines()
            .iter()
            .any(|line| line.trim_end() == "again")
    });
    wait_until("second page-out", Duration::from_secs(20), || {
        HISTORY_PAGED_OUT_BYTES.load(Ordering::Relaxed) > first_transition
    });
    let second = HISTORY_PAGED_OUT_BYTES.load(Ordering::Relaxed) - first_transition;
    assert!(
        second < first_transition,
        "the second transition wrote {second} bytes, the first {first_transition}: \
         only new history is written"
    );
    assert_eq!(swap_files(&logs).len(), 1, "added to the same file");

    // The checkpoint taken after the second burst read the paged-out
    // history: an Engine restart still restores it from there.
    let checkpoint =
        diri_engine::checkpoint::ScreenCheckpoint::path_for_log(&logs.join("s_swap_idle.bin"));
    let saved = diri_engine::checkpoint::ScreenCheckpoint::load(&checkpoint).expect("checkpoint");
    let history = session.inspect_screen(|screen| screen.history_snapshot());
    assert_eq!(saved.history.len(), history.len());
    assert_eq!(saved.history, history);
    assert!(history.len() > 8_000, "both bursts are in the checkpoint");

    // The session ends: its swap goes with its history.
    session
        .terminate(Duration::from_secs(2))
        .expect("terminate");
    drop(session);
    wait_until("swap removed", Duration::from_secs(5), || {
        swap_files(&logs).is_empty()
    });
}
