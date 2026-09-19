//! Password-prompt awareness: the PTY owner's termios, sampled at the points
//! the Engine already visits, for every way a local session can own a PTY.
//!
//! Kept apart from `holder_session.rs` so these sessions do not compete for
//! the CPU with its timing-sensitive short-lived-child tests.

#![cfg(unix)]

use std::path::{Path, PathBuf};
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

fn holders_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diri-secret-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn holder_config(root: &Path) -> HolderConfig {
    HolderConfig {
        holders_dir: root.join("holders"),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
    }
}

fn spec(id: &str, manifest_id: &str, logs: &Path, holder: Option<HolderConfig>) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(
            vec!["/bin/sh".into(), "-c".into(), SECRET_PROMPT_SCRIPT.into()],
            "/tmp",
        )
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .size(80, 24),
        manifest_id: manifest_id.into(),
        authority: Authority::ProcessOnly,
        logs_dir: logs.to_path_buf(),
        holder,
        remote: None,
        defer_launch: false,
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

/// Each marker follows the `stty` it names, so seeing it on the screen means
/// that mode is already in force. The alternate screen is left only after raw
/// mode is on, so no step passes through a silenced line prompt by accident.
const SECRET_PROMPT_SCRIPT: &str = "read a; \
    stty -echo; printf hidden; read secret; \
    stty echo; printf shown; read b; \
    printf '\\033[?1049h'; stty -echo; printf altprompt; read c; \
    stty raw -echo; printf '\\033[?1049l'; printf rawmode; read d";

fn screen_shows(session: &Session, needle: &str) -> bool {
    session
        .screen_lines()
        .iter()
        .any(|line| line.contains(needle))
}

/// Long enough for several pump ticks to have sampled the PTY owner.
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

fn exercise_secret_input(id: &str, manifest_id: &str, holder: Option<HolderConfig>, logs: &Path) {
    let mut session = Session::spawn(spec(id, manifest_id, logs, holder), engine()).expect("spawn");
    settle();
    assert!(
        !session.secret_input(),
        "an echoing line prompt is not secret"
    );

    session
        .write_input(b"\n")
        .expect("reach the password prompt");
    // The termios sample and the emulator are fed by different paths, so the
    // mode can be known a moment before the prompt text is on the screen.
    wait_until("secret input", Duration::from_secs(5), || {
        session.secret_input() && screen_shows(&session, "hidden")
    });

    for byte in b"hunter2" {
        session.write_input(&[*byte]).expect("type the secret");
    }
    session.write_input(b"\r").expect("submit the secret");
    wait_until("echo restored", Duration::from_secs(5), || {
        screen_shows(&session, "shown") && !session.secret_input()
    });
    assert!(
        !screen_shows(&session, "hunter2"),
        "a silenced line never reaches the grid"
    );
    assert_eq!(
        session.view().title,
        None,
        "a typed password must not name the session"
    );

    session
        .write_input(b"\n")
        .expect("enter the alternate screen");
    wait_until("alternate-screen prompt", Duration::from_secs(5), || {
        screen_shows(&session, "altprompt")
    });
    settle();
    assert!(
        !session.secret_input(),
        "a full-screen program is never a password prompt"
    );

    session.write_input(b"\n").expect("enter raw mode");
    wait_until("raw mode", Duration::from_secs(5), || {
        screen_shows(&session, "rawmode")
    });
    settle();
    assert!(!session.secret_input(), "raw mode without echo is a TUI");

    let _ = session.terminate(Duration::from_secs(2));
    assert!(!session.secret_input(), "an exited child reads nothing");
}

#[test]
fn secret_input_tracks_a_password_prompt_on_a_held_shell() {
    let root = holders_dir("secret-shell");
    let holder = holder_config(&root);
    exercise_secret_input("s_sec_shell", "shell", Some(holder), &root.join("logs"));
}

#[test]
fn secret_input_tracks_a_password_prompt_on_a_held_agent() {
    // Not a shell, so this session is only asked while a line prompt is
    // possible, and its typed input is a prompt-title candidate.
    let root = holders_dir("secret-agent");
    let holder = holder_config(&root);
    exercise_secret_input("s_sec_agent", "codex", Some(holder), &root.join("logs"));
}

#[test]
fn secret_input_tracks_a_password_prompt_on_a_directly_owned_pty() {
    let root = holders_dir("secret-direct");
    exercise_secret_input("s_sec_direct", "codex", None, &root.join("logs"));
}
