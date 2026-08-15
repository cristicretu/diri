//! Runs a command inside a real held session and waits for it to finish.
//!
//! This is how the terminal benchmark suite is pointed at diri: the same
//! scripts that run inside another terminal's window run inside a session
//! here, driven by the same engine the daemon uses. What differs from the
//! shipped app is only that no window is attached — the PTY, holder, log and
//! emulator are the production path.
//!
//! Usage: runbench <cols> <rows> <shell-command>

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};

fn main() {
    let mut args = std::env::args().skip(1);
    let cols: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(153);
    let rows: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(39);
    let command = args
        .next()
        .expect("usage: runbench <cols> <rows> <command>");
    let timeout = Duration::from_secs(
        std::env::var("RUNBENCH_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900),
    );

    let root = std::env::temp_dir().join(format!("diri-runbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create dir");

    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");

    let spec = SessionSpec {
        id: "s_runbench".into(),
        pty: PtySpec::new(
            vec!["/bin/bash".into(), "-c".into(), command],
            std::env::var("RUNBENCH_CWD").unwrap_or_else(|_| "/tmp".into()),
        )
        .env(
            "PATH",
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .env("TERM", "xterm-256color")
        .size(cols, rows),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(HolderConfig {
            holders_dir: root.join("holders"),
            // Built alongside this example; point DIRI_HOLDER_BIN at it.
            executable: PathBuf::from(
                std::env::var("DIRI_HOLDER_BIN")
                    .expect("set DIRI_HOLDER_BIN to a built diri-holder"),
            ),
        }),
        remote: None,
        defer_launch: false,
    };

    let session = Session::spawn(spec, Arc::new(engine)).expect("spawn");
    let deadline = Instant::now() + timeout;
    while !session.view().exited && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !session.view().exited {
        eprintln!("runbench: command did not finish within the timeout");
        std::process::exit(1);
    }
    // Give the pump a moment to drain the tail before the process goes away.
    std::thread::sleep(Duration::from_millis(200));
}
