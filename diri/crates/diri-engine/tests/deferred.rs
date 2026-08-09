//! Deferred launch: a holder spawn with `defer_launch` must not exec until
//! the first client size settles, so the agent's one-shot banner renders at
//! the real viewport width. `stty size` inside the session is the witness —
//! it prints the geometry the child was ACTUALLY given at exec time.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, OutputLog, PtySpec};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn holders_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-defer-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn deferred_spec(id: &str, script: &str, root: &Path) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(HolderConfig {
            holders_dir: root.join("holders"),
            executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
        }),
        remote: None,
        defer_launch: true,
    }
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

fn log_text(logs: &Path, id: &str) -> String {
    let Ok(mut log) = OutputLog::reader(logs, id) else {
        return String::new();
    };
    log.refresh_from_disk();
    let tail = log.tail_offset();
    let (_, bytes) = log.read(0, tail as usize);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The client's settled size — not the spec's estimate — is what the child
/// sees at exec: the whole point of deferring.
#[test]
fn the_first_client_size_decides_the_launch_geometry() {
    let root = holders_dir("size");
    let logs = root.join("logs");

    let mut session = Session::spawn(
        deferred_spec("s_size", "stty size; exec cat", &root),
        engine(),
    )
    .expect("spawn returns before any exec");
    // The client attaches and reports its real viewport before the fallback
    // window closes; this must become the exec geometry.
    session.resize(100, 30).expect("propose size");

    wait_until("stty to report", Duration::from_secs(5), || {
        log_text(&logs, "s_size").contains("30 100")
    });
    assert!(
        !log_text(&logs, "s_size").contains("24 80"),
        "the child must never have existed at the estimated size"
    );

    session
        .terminate(Duration::from_secs(2))
        .expect("terminate");
}

/// No client size at all (an MCP-spawned agent): the fallback window closes
/// and the exec happens at the estimated size.
#[test]
fn without_a_client_size_the_fallback_launches_at_the_estimate() {
    let root = holders_dir("fallback");
    let logs = root.join("logs");

    let mut session = Session::spawn(
        deferred_spec("s_fall", "stty size; exec cat", &root),
        engine(),
    )
    .expect("spawn");

    wait_until("the fallback exec", Duration::from_secs(5), || {
        log_text(&logs, "s_fall").contains("24 80")
    });

    session
        .terminate(Duration::from_secs(2))
        .expect("terminate");
}

/// Keystrokes racing the exec are queued and flushed after it — the Swift
/// daemon's `queuedLaunchInput` — so nothing a fast typist sends is lost.
#[test]
fn input_typed_before_the_exec_is_queued_and_flushed() {
    let root = holders_dir("input");
    let logs = root.join("logs");

    let mut session =
        Session::spawn(deferred_spec("s_in", "exec cat", &root), engine()).expect("spawn");
    session
        .write_input(b"typed-before-exec\n")
        .expect("queued while unlaunched");

    wait_until("the queued input to echo", Duration::from_secs(5), || {
        log_text(&logs, "s_in").contains("typed-before-exec")
    });

    session
        .terminate(Duration::from_secs(2))
        .expect("terminate");
}

/// A kill during the deferral window means the child must never exist.
#[test]
fn terminating_before_the_exec_prevents_the_launch() {
    let root = holders_dir("cancel");
    let marker = root.join("ran");
    let script = format!("touch {}; exec cat", marker.display());

    let mut session =
        Session::spawn(deferred_spec("s_no", &script, &root), engine()).expect("spawn");
    let exit = session
        .terminate(Duration::from_secs(2))
        .expect("terminate");
    assert!(matches!(exit, diri_engine::Exit::Signal(_)));

    // Well past the fallback window: the exec must not have happened.
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !marker.exists(),
        "a cancelled deferred launch still ran its child"
    );
}
