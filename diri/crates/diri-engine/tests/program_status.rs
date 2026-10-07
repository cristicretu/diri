//! A program that reports its own status with `OSC 7501` is believed over
//! anything inferred about it, its query for support is answered, and what it
//! reported ends with it.
//!
//! Real shells on real PTYs, for both ways a local session owns one.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::{
    NeedsInputKind, NeedsInputSource, SessionStatus, TerminalProgress, TerminalProgressState,
};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-osc7501-{tag}-{}", std::process::id()));
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
            "timed out waiting for {what}: {:?} {:?}",
            session.view().status,
            session.screen_lines()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn at_prompt(session: &Session) -> bool {
    session.status() == SessionStatus::Idle
        && session
            .screen_lines()
            .last()
            .is_some_and(|line| line.trim() == "$")
}

/// A script that reports, waiting for a line of input between reports so
/// the test, not a timer, moves it on. `\\033` survives `sh -c` for printf.
const SCRIPT: &str = concat!(
    "sh -c '",
    "r() { printf \"\\033]7501;%s\\007\" \"$1\"; read x; }; ",
    "r state=working:progress=40; ",
    // "Deploy to prod?"
    "r state=blocked:kind=permission:app=deploy:msg=RGVwbG95IHRvIHByb2Q/; ",
    "r state=done; ",
    "sleep 30",
    "'\r"
);

fn exercise(session: &mut Session) {
    wait_until(session, "the prompt", || at_prompt(session));

    // Support detection: the query comes back as the program's input.
    session
        .write_input(
            b"stty raw -echo; printf '\\033]7501;?\\033\\\\'; dd bs=1 count=10 2>/dev/null | od -An -c; stty sane\r",
        )
        .unwrap();
    wait_until(session, "the query's answer", || {
        session
            .screen_lines()
            .iter()
            .any(|line| line.contains("]   7   5   0   1   ;   ?"))
    });
    wait_until(session, "the prompt after the query", || at_prompt(session));

    session.write_input(SCRIPT.as_bytes()).unwrap();
    wait_until(session, "working with progress", || {
        session.view().terminal_progress
            == Some(TerminalProgress {
                state: TerminalProgressState::Normal,
                percent: 40,
            })
    });
    assert_eq!(session.status(), SessionStatus::Working);
    // The job is waiting on a line, which by itself would read as a
    // question; the program has said it is working.
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(session.status(), SessionStatus::Working);

    session.write_input(b"\r").unwrap();
    wait_until(session, "the permission request", || {
        session.status() == SessionStatus::NeedsInput(NeedsInputKind::Permission)
    });
    let detail = session.view().needs_input.expect("detail");
    assert_eq!(detail.source, NeedsInputSource::ProgramStatus);
    assert_eq!(detail.summary, "Deploy to prod?");
    assert_eq!(detail.tool_name.as_deref(), Some("deploy"));

    session.write_input(b"\r").unwrap();
    wait_until(session, "the result", || {
        session.status() == SessionStatus::Idle
    });
    let completed = session.view().last_turn_completed_at;
    assert!(completed.is_some(), "done completes the turn");
    // `sleep` holds the foreground; the program's result still stands.
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(session.status(), SessionStatus::Idle);

    // The job ends, and with it what it reported: the shell is the shell
    // again, and the next job reads as work.
    session.write_input(b"\x03").unwrap();
    wait_until(session, "the prompt after the job", || at_prompt(session));
    session.write_input(b"sleep 30\r").unwrap();
    wait_until(session, "an ordinary job to read as work", || {
        session.status() == SessionStatus::Working
    });
    assert_eq!(session.view().last_turn_completed_at, completed);
    session.write_input(b"\x03").unwrap();

    let _ = session.terminate(Duration::from_secs(2));
}

#[test]
fn program_status_in_a_directly_owned_terminal() {
    let root = root("direct");
    let mut session = shell("osc7501_direct", &root, None);
    exercise(&mut session);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn program_status_in_a_held_terminal() {
    let root = root("held");
    let holder = HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    };
    let mut session = shell("osc7501_held", &root, Some(holder));
    exercise(&mut session);
    let _ = std::fs::remove_dir_all(&root);
}
