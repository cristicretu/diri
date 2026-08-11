//! A session watching a real child, end to end and unattended.
//!
//! Unlike the pipeline test, nothing here is pumped by hand: the session's own
//! thread reads the PTY, emulates, detects and reduces. These assert on what an
//! outside observer — the app, the CLI, an MCP tool — would actually see.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::detect::ManifestEngine;
use diri_engine::pty::PtySpec;
use diri_engine::session::{Session, SessionSpec, authority_for};
use diri_engine::status::{Authority, ClaudeHook};
use diri_proto::{NeedsInputKind, SessionStatus};

fn manifest_dir() -> PathBuf {
    diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests directory")
}

fn engine() -> Arc<ManifestEngine> {
    let (engine, failed) = ManifestEngine::load_dir(&manifest_dir()).expect("load manifests");
    assert!(failed.is_empty(), "manifests failed: {failed:?}");
    Arc::new(engine)
}

/// Waits for a condition the session's own thread is responsible for reaching.
fn wait_until(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

fn spec(
    id: &str,
    script: &str,
    logs: &Path,
    manifest_id: &str,
    authority: Authority,
) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
        manifest_id: manifest_id.into(),
        authority,
        logs_dir: logs.to_path_buf(),
        holder: None,
        remote: None,
        defer_launch: false,
    }
}

#[test]
fn a_session_reaches_needs_input_on_its_own() {
    let temp = tempfile::tempdir().expect("temp");
    let script = "printf 'Do you want to proceed?\\n\\342\\235\\257 1. Yes\\n  2. No\\nesc to cancel\\n'; sleep 30";
    let session = Session::spawn(
        spec(
            "s_needs",
            script,
            temp.path(),
            "claude-code",
            Authority::HooksPrimary,
        ),
        engine(),
    )
    .expect("spawn");

    let reached = wait_until(Duration::from_secs(15), || {
        matches!(session.status(), SessionStatus::NeedsInput(_))
    });
    assert!(
        reached,
        "session never noticed the prompt; screen was {:?}",
        session.screen_lines()
    );

    let view = session.view();
    assert_eq!(
        view.status,
        SessionStatus::NeedsInput(NeedsInputKind::Permission)
    );
    let detail = view.needs_input.expect("a needs-input detail");
    assert!(
        detail.options.is_some_and(|options| options.len() >= 2),
        "the options should have been scraped"
    );
    assert!(view.tail_offset > 0, "output was recorded");
}

#[test]
fn output_is_replayable_by_offset_while_the_session_runs() {
    let temp = tempfile::tempdir().expect("temp");
    let session = Session::spawn(
        spec(
            "s_replay",
            "printf 'first-chunk\\n'; sleep 30",
            temp.path(),
            "shell",
            Authority::ProcessOnly,
        ),
        engine(),
    )
    .expect("spawn");

    assert!(
        wait_until(Duration::from_secs(10), || session.view().tail_offset > 0),
        "no output recorded"
    );

    let (offset, bytes) = session.read_output(0, 4096);
    assert_eq!(offset, 0, "replay starts at the beginning of the stream");
    assert!(
        String::from_utf8_lossy(&bytes).contains("first-chunk"),
        "replayed bytes should carry the output"
    );
}

#[test]
fn typed_input_reaches_the_child_and_the_session_moves_on() {
    let temp = tempfile::tempdir().expect("temp");
    let script = "read answer; printf 'answered:%s\\n' \"$answer\"; sleep 30";
    let session = Session::spawn(
        spec(
            "s_input",
            script,
            temp.path(),
            "shell",
            Authority::ProcessOnly,
        ),
        engine(),
    )
    .expect("spawn");

    session.write_input(b"yes\n").expect("write");

    let echoed = wait_until(Duration::from_secs(10), || {
        session
            .screen_lines()
            .iter()
            .any(|line| line.contains("answered:yes"))
    });
    assert!(
        echoed,
        "the child never saw the input; screen was {:?}",
        session.screen_lines()
    );
}

#[test]
fn a_child_that_exits_is_reported_with_its_code() {
    let temp = tempfile::tempdir().expect("temp");
    let session = Session::spawn(
        spec(
            "s_exit",
            "printf 'bye\\n'; exit 7",
            temp.path(),
            "shell",
            Authority::ProcessOnly,
        ),
        engine(),
    )
    .expect("spawn");

    let exited = wait_until(Duration::from_secs(10), || session.view().exited);
    assert!(exited, "the session never noticed the child exit");

    match session.status() {
        SessionStatus::Exited(info) => assert_eq!(info.code, Some(7)),
        other => panic!("expected exit 7, got {other:?}"),
    }
}

#[test]
fn a_hook_can_drive_status_without_any_screen_change() {
    // Claude's authority is hooks-first: a prompt-submit means working even
    // though nothing has been painted yet.
    let temp = tempfile::tempdir().expect("temp");
    let session = Session::spawn(
        spec(
            "s_hook",
            "sleep 30",
            temp.path(),
            "claude-code",
            Authority::HooksPrimary,
        ),
        engine(),
    )
    .expect("spawn");

    let outcome = session.claude_hook(ClaudeHook::UserPromptSubmit, false);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert_eq!(session.status(), SessionStatus::Working);

    // A subagent's Stop must not end the parent's turn.
    let outcome = session.claude_hook(ClaudeHook::Stop, true);
    assert_eq!(outcome.status_change, None);
    assert_eq!(session.status(), SessionStatus::Working);
}

#[test]
fn a_silent_session_still_settles_after_a_stop_hook() {
    // The debounce timers have to keep advancing when the child says nothing —
    // that is precisely when idle confirmation and staleness matter. A pump
    // that blocks in read() only ticks when output arrives, so this session
    // would sit in `working` forever.
    let temp = tempfile::tempdir().expect("temp");
    let session = Session::spawn(
        spec(
            "s_quiet",
            "sleep 30",
            temp.path(),
            "claude-code",
            Authority::HooksPrimary,
        ),
        engine(),
    )
    .expect("spawn");

    session.claude_hook(ClaudeHook::UserPromptSubmit, false);
    assert_eq!(session.status(), SessionStatus::Working);

    // Stop is a strong idle but needs one more confirmation, and the only
    // thing that can supply it here is a tick — the child never writes again.
    session.claude_hook(ClaudeHook::Stop, false);
    let settled = wait_until(Duration::from_secs(5), || {
        session.status() == SessionStatus::Idle
    });
    assert!(
        settled,
        "a quiet session never settled; status is {:?}",
        session.status()
    );
}

#[test]
fn terminating_a_session_kills_the_child() {
    let temp = tempfile::tempdir().expect("temp");
    let mut session = Session::spawn(
        spec(
            "s_kill",
            "sleep 300",
            temp.path(),
            "shell",
            Authority::ProcessOnly,
        ),
        engine(),
    )
    .expect("spawn");

    session
        .terminate(Duration::from_secs(3))
        .expect("terminate");
    assert!(session.view().exited);
}

#[test]
fn authority_is_derived_from_the_manifest_status_model() {
    let engine = engine();
    assert_eq!(
        authority_for("claude-code", &engine),
        Authority::HooksPrimary
    );
    assert_eq!(authority_for("codex", &engine), Authority::ScreenPrimary);
    assert_eq!(authority_for("shell", &engine), Authority::ProcessOnly);
    assert_eq!(
        authority_for("no-such-agent", &engine),
        Authority::ProcessOnly,
        "an unknown agent gets the conservative model"
    );
}
