//! A held session whose program floods output faster than the Engine parses
//! it must still have every byte parsed: the Engine's terminal state is what
//! the user's screen, scrollback and status detection are built from.
//!
//! In its own test binary because it reads a process-wide counter.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// The session log this test's Holder keeps, an eighth of the default, so a
/// debug build reaches its bound quickly. It keeps an eighth of this when it
/// rotates.
const LOG_BYTES: u64 = 4 << 20;
/// Several times the log, so an Engine reading behind the program would have
/// to stay within 512 KiB of it to lose nothing.
const FLOOD_BYTES: u64 = 24 << 20;
const BLOCK_BYTES: usize = 1 << 20;

#[test]
fn a_flood_faster_than_the_engine_parses_loses_no_output() {
    // SAFETY: this test binary runs this one test, and nothing else reads
    // the environment concurrently.
    unsafe { std::env::set_var("DIRIJOR_HOLDER_LOG_BYTES", LOG_BYTES.to_string()) };
    let root = std::env::temp_dir().join(format!("diri-flood-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };
    // Wide characters and truecolor runs: cheap for the Holder to pass on,
    // costly for the Engine to parse, so the program outruns the Engine.
    let block = root.join("block");
    {
        let mut text = String::new();
        let mut line = 0u32;
        while text.len() < BLOCK_BYTES {
            line += 1;
            text.push_str(&format!(
                "\x1b[38;2;{};{};{}m{line:06} 日本語のテキスト 你好世界 한국어 텍스트 \x1b[0m\r\n",
                line % 256,
                line * 7 % 256,
                line * 13 % 256,
            ));
        }
        std::fs::write(&block, text).expect("block");
    }
    let copies = FLOOD_BYTES.div_ceil(BLOCK_BYTES as u64);
    // Starts printing the moment it launches, before the Engine has read
    // anything.
    let script = format!(
        "i=0; while [ $i -lt {copies} ]; do cat '{}'; i=$((i+1)); done; \
         echo FLOOD_DONE; exec sleep 600",
        block.display()
    );
    let spec = SessionSpec {
        id: "s_flood".into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(holder),
        remote: None,
        defer_launch: false,
    };
    let mut session = Session::spawn(spec, engine()).expect("spawn held");

    let deadline = Instant::now() + Duration::from_secs(240);
    while !session.screen_lines().join("\n").contains("FLOOD_DONE") {
        assert!(
            Instant::now() < deadline,
            "the flood never reached the screen"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let skipped = diri_engine::session::held_output_skipped_bytes();
    let _ = session.terminate(Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        skipped, 0,
        "the Engine skipped {skipped} bytes of output the program printed"
    );
}
