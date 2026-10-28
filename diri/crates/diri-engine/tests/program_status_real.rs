//! Opt-in: the real Claude Code and Codex CLIs, typed at a shell prompt the
//! way a user starts them, read through `OSC 7501` and the screen rules.
//!
//! - `DIRI_REAL_CLAUDE=1`: a signed-in `claude` on PATH. Sends two tiny
//!   haiku prompts ("reply with ok", and one Bash command it is denied).
//!   Runs in a fresh temp directory, which it removes; Claude asks to trust
//!   it first.
//! - `DIRI_REAL_CODEX=1` plus `DIRI_CODEX_HOME=<dir>`: a `codex` on PATH and
//!   a scratch `CODEX_HOME` holding a copy of `auth.json` and a `hooks.json`
//!   with one untrusted hook, so Codex opens its "Hooks need review"
//!   chooser. Sends one tiny prompt. Leaves the scratch home to the caller.
//!
//! `cargo test -p diri-engine --test program_status_real -- --nocapture`

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::{NeedsInputKind, SessionStatus};

fn session(root: &Path, extra_env: &[(&str, &str)]) -> Session {
    let home = std::env::var("HOME").unwrap();
    let path = format!(
        "{home}/.local/bin:{}:/opt/homebrew/bin:/usr/bin:/bin",
        std::env::var("PATH").unwrap_or_default()
    );
    let mut pty = PtySpec::new(vec!["/bin/zsh".into(), "-f".into(), "-i".into()], root)
        .env("HOME", &home)
        .env("USER", &std::env::var("USER").unwrap_or_default())
        .env("PATH", &path)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("PS1", "$ ")
        .size(120, 40);
    for (key, value) in extra_env {
        pty = pty.env(key, value);
    }
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let logs = root.join(".logs");
    std::fs::create_dir_all(&logs).unwrap();
    Session::spawn(
        SessionSpec {
            id: "real".into(),
            pty,
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: logs,
            holder: None,
            remote: None,
            defer_launch: false,
        },
        Arc::new(engine),
    )
    .expect("spawn")
}

/// Waits for `check`, logging every status change on the way.
fn wait(session: &Session, what: &str, secs: u64, mut check: impl FnMut(&Session) -> bool) -> bool {
    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < Duration::from_secs(secs) {
        let status = session.status();
        if last.as_ref() != Some(&status) {
            let rule = session
                .view()
                .status_evidence
                .and_then(|e| e.matched_rule_id);
            eprintln!(
                "{:>6.2}s {status:?} rule={rule:?}",
                started.elapsed().as_secs_f32()
            );
            last = Some(status);
        }
        if check(session) {
            eprintln!("--- {what}: {:.2}s", started.elapsed().as_secs_f32());
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("--- {what}: timed out; screen {:?}", session.screen_lines());
    false
}

fn settled_idle(session: &Session, secs: u64) -> bool {
    let mut since = None;
    wait(session, "settled idle", secs, |session| {
        if session.status() == SessionStatus::Idle {
            since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(2)
        } else {
            since = None;
            false
        }
    })
}

fn type_line(session: &Session, text: &str) {
    session.write_input(text.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    session.write_input(b"\r").unwrap();
}

#[test]
fn real_claude_denied_permission_is_not_a_finished_turn() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut session = session(&root, &[]);
    std::thread::sleep(Duration::from_millis(500));
    type_line(&session, "claude --model haiku --permission-mode default");
    if wait(&session, "trust prompt", 20, |s| {
        matches!(s.status(), SessionStatus::NeedsInput(_))
    }) {
        std::thread::sleep(Duration::from_millis(500));
        session.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        session.write_input(b"\r").unwrap();
    }
    assert!(settled_idle(&session, 40));

    type_line(&session, "Reply with only the word ok.");
    assert!(wait(&session, "working", 30, |s| s.status()
        == SessionStatus::Working));
    assert!(settled_idle(&session, 90));
    let finished = session.view().last_turn_completed_at;
    assert!(finished.is_some(), "done completed the turn");

    type_line(&session, "Run this exact bash command: date > deny-me.txt");
    assert!(wait(&session, "permission", 90, |s| {
        s.status() == SessionStatus::NeedsInput(NeedsInputKind::Permission)
    }));
    let detail = session.view().needs_input.expect("detail");
    eprintln!("--- request: {detail:?}");
    assert_ne!(detail.tool_name.as_deref(), Some("claude-code"));
    std::thread::sleep(Duration::from_millis(1_000));
    session.write_input(b"\x1b").unwrap();
    assert!(wait(&session, "denied", 10, |s| s.status()
        == SessionStatus::Idle));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(session.status(), SessionStatus::Idle);
    assert_eq!(
        session.view().last_turn_completed_at,
        finished,
        "a denial is not a finished turn"
    );
    assert!(!root.join("deny-me.txt").exists());

    session.write_input(b"\x03").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    session.write_input(b"\x03").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let _ = session.terminate(Duration::from_secs(2));
}

#[test]
fn real_codex_hooks_chooser_and_idle_turn_end() {
    if std::env::var_os("DIRI_REAL_CODEX").is_none() {
        return;
    }
    let codex_home = std::env::var("DIRI_CODEX_HOME").expect("DIRI_CODEX_HOME");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut session = session(&root, &[("CODEX_HOME", &codex_home)]);
    std::thread::sleep(Duration::from_millis(500));
    type_line(
        &session,
        "codex -a on-request -s read-only -c approvals_reviewer=user -c model_reasoning_effort=low",
    );
    assert!(wait(&session, "hooks chooser", 30, |s| {
        s.view()
            .status_evidence
            .and_then(|evidence| evidence.matched_rule_id)
            .as_deref()
            == Some("hooks-review-prompt")
    }));
    let view = session.view();
    assert_eq!(
        view.status,
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );
    eprintln!("--- chooser: {:?}", view.needs_input);
    assert_eq!(
        view.needs_input
            .and_then(|detail| detail.options)
            .map(|o| o.len()),
        Some(3)
    );
    // "esc skip": continue without trusting the hook.
    std::thread::sleep(Duration::from_millis(500));
    session.write_input(b"\x1b").unwrap();
    // Trust the temp directory if Codex asks.
    std::thread::sleep(Duration::from_secs(2));
    if session
        .screen_lines()
        .iter()
        .any(|line| line.to_lowercase().contains("do you trust"))
    {
        session.write_input(b"\r").unwrap();
    }
    assert!(settled_idle(&session, 40));
    let before = session.view().last_turn_completed_at;

    type_line(&session, "Reply with only the word ok.");
    assert!(wait(&session, "working", 30, |s| s.status()
        == SessionStatus::Working));
    assert!(settled_idle(&session, 90));
    assert_ne!(
        session.view().last_turn_completed_at,
        before,
        "Codex's idle completed the turn"
    );

    session.write_input(b"\x03").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    session.write_input(b"\x03").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let _ = session.terminate(Duration::from_secs(2));
}
