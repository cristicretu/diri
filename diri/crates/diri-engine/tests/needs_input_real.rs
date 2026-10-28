//! Opt-in: every kind of "waiting on you" screen the real agent CLIs show,
//! typed at a shell prompt the way a user starts them, must read as
//! NeedsInput — and promptly.
//!
//! - `DIRI_REAL_CLAUDE=1`: a signed-in `claude` in `~/.local/bin`. Sends a
//!   handful of tiny haiku prompts (a multiple-choice question, a plan
//!   approval, a permission, a subagent's permission, a turn that ends on a
//!   question while a background agent runs). Runs in fresh temp
//!   directories, which it removes; the user's own settings and hooks load.
//! - `DIRI_REAL_CODEX=1` plus `DIRI_CODEX_HOME=<dir>`: a `codex` on PATH and
//!   a scratch `CODEX_HOME` holding a copy of `auth.json` and a
//!   `config.toml` with `check_for_update_on_startup = false` (otherwise its
//!   update chooser can take the first prompt's Enter and update Codex).
//!   One command approval and one question. Leaves the home to the caller.
//! - `DIRI_NEEDS_INPUT_CAPTURE=<dir>`: also copies each run's raw terminal
//!   output there, for replay fixtures.
//!
//! `cargo test -p diri-engine --test needs_input_real -- --nocapture --test-threads=1`

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::session::{Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::{NeedsInputKind, SessionStatus};

fn session(root: &Path, id: &str) -> Session {
    let home = std::env::var("HOME").unwrap();
    let path = format!(
        "{home}/.local/bin:{}:/opt/homebrew/bin:/usr/bin:/bin",
        std::env::var("PATH").unwrap_or_default()
    );
    let pty = PtySpec::new(vec!["/bin/zsh".into(), "-f".into(), "-i".into()], root)
        .env("HOME", &home)
        .env("USER", &std::env::var("USER").unwrap_or_default())
        .env("PATH", &path)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("PS1", "$ ")
        .size(120, 40);
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let logs = root.join(".logs");
    std::fs::create_dir_all(&logs).unwrap();
    Session::spawn(
        SessionSpec {
            id: id.into(),
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
fn wait(
    session: &Session,
    what: &str,
    secs: u64,
    mut check: impl FnMut(&Session) -> bool,
) -> Option<Duration> {
    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < Duration::from_secs(secs) {
        let status = session.status();
        if last.as_ref() != Some(&status) {
            let evidence = session.view().status_evidence;
            eprintln!(
                "{:>6.2}s {status:?} src={:?} rule={:?}",
                started.elapsed().as_secs_f32(),
                evidence.as_ref().map(|e| e.source),
                evidence.and_then(|e| e.matched_rule_id),
            );
            last = Some(status);
        }
        if check(session) {
            eprintln!("--- {what}: {:.2}s", started.elapsed().as_secs_f32());
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("--- {what}: timed out");
    for line in session.screen_lines() {
        eprintln!("| {line}");
    }
    None
}

/// Waits until `needle` is on screen and returns when it appeared.
fn screen_shows(session: &Session, needle: &str, secs: u64) -> Option<Instant> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(secs) {
        if session
            .screen_lines()
            .iter()
            .any(|line| line.to_lowercase().contains(needle))
        {
            return Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
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
    .is_some()
}

fn type_line(session: &Session, text: &str) {
    session.write_input(text.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    session.write_input(b"\r").unwrap();
}

fn blocked(session: &Session) -> bool {
    matches!(session.status(), SessionStatus::NeedsInput(_))
}

/// Starts Claude in a fresh folder, answering its trust picker.
fn start_claude(root: &Path, id: &str, args: &str) -> Session {
    let session = session(root, id);
    std::thread::sleep(Duration::from_millis(500));
    type_line(&session, &format!("claude --model haiku {args}"));
    if wait(&session, "trust prompt", 20, blocked).is_some() {
        std::thread::sleep(Duration::from_millis(500));
        session.write_input(b"\x1b[B").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        session.write_input(b"\r").unwrap();
    }
    assert!(settled_idle(&session, 40));
    session
}

fn finish(mut session: Session, root: &Path, id: &str) {
    session.write_input(b"\x03").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    session.write_input(b"\x03").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let _ = session.terminate(Duration::from_secs(2));
    if let Some(dir) = std::env::var_os("DIRI_NEEDS_INPUT_CAPTURE") {
        let logs = root.join(".logs");
        for entry in std::fs::read_dir(&logs).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(id) {
                let _ = std::fs::copy(entry.path(), Path::new(&dir).join(&*name));
            }
        }
    }
}

/// Reports how long after the screen showed the prompt Diri flagged it.
fn flagged_after(
    session: &Session,
    needle: &str,
    secs: u64,
) -> (Option<Duration>, Option<Duration>) {
    let Some(shown) = screen_shows(session, needle, secs) else {
        eprintln!("--- screen never showed {needle:?}");
        for line in session.screen_lines() {
            eprintln!("| {line}");
        }
        return (None, None);
    };
    let flagged = wait(session, "flagged", 15, blocked);
    let lag = flagged.map(|_| shown.elapsed());
    let view = session.view();
    eprintln!(
        "--- {needle:?}: status {:?} lag {lag:?} detail {:?}",
        view.status,
        view.needs_input
            .map(|d| (d.kind, d.source, d.summary, d.options))
    );
    (Some(shown.elapsed()), flagged)
}

#[test]
fn real_claude_multiple_choice_question_needs_input() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let session = start_claude(&root, "ask", "--permission-mode default");
    type_line(
        &session,
        "Use the AskUserQuestion tool to ask me one question: red or blue? Do nothing else.",
    );
    let (_, flagged) = flagged_after(&session, "enter to select", 90);
    assert!(flagged.is_some(), "a multiple-choice question needs input");
    assert_eq!(
        session.status(),
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(blocked(&session), "it stays flagged while it waits");
    session.write_input(b"\r").unwrap();
    assert!(wait(&session, "answered", 30, |s| !blocked(s)).is_some());
    assert!(settled_idle(&session, 90));
    finish(session, &root, "ask");
}

#[test]
fn real_claude_plan_approval_needs_input() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let session = start_claude(&root, "plan", "--permission-mode plan");
    type_line(
        &session,
        "Plan creating an empty file named hello.txt. One-line plan, then present it for approval.",
    );
    let (_, flagged) = flagged_after(&session, "would you like to proceed", 120);
    assert!(flagged.is_some(), "a plan approval needs input");
    std::thread::sleep(Duration::from_secs(3));
    assert!(blocked(&session), "it stays flagged while it waits");
    finish(session, &root, "plan");
}

#[test]
fn real_claude_permission_needs_input() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let session = start_claude(&root, "perm", "--permission-mode default");
    type_line(&session, "Run this exact bash command: date > perm.txt");
    let (_, flagged) = flagged_after(&session, "do you want to proceed", 90);
    assert!(flagged.is_some());
    std::thread::sleep(Duration::from_secs(3));
    assert!(blocked(&session), "it stays flagged while it waits");
    session.write_input(b"\x1b").unwrap();
    finish(session, &root, "perm");
}

/// Starts Codex with a scratch `CODEX_HOME` (`DIRI_CODEX_HOME`, holding a
/// copy of `auth.json`), trusting the folder if asked.
fn start_codex(root: &Path, id: &str, args: &str) -> Option<Session> {
    std::env::var_os("DIRI_REAL_CODEX")?;
    let codex_home = std::env::var("DIRI_CODEX_HOME").expect("DIRI_CODEX_HOME");
    let session = session(root, id);
    std::thread::sleep(Duration::from_millis(500));
    type_line(
        &session,
        &format!("export CODEX_HOME={codex_home}; codex -c model_reasoning_effort=low {args}"),
    );
    if screen_shows(&session, "do you trust", 20).is_some() {
        std::thread::sleep(Duration::from_millis(500));
        session.write_input(b"\r").unwrap();
    }
    assert!(settled_idle(&session, 40));
    Some(session)
}

#[test]
fn real_codex_command_approval_needs_input() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let Some(session) = start_codex(
        &root,
        "cxapprove",
        "-a on-request -s read-only -c approvals_reviewer=user",
    ) else {
        return;
    };
    type_line(
        &session,
        "Run exactly this shell command and nothing else: touch codex-probe.txt",
    );
    let (_, flagged) = flagged_after(&session, "would you like to", 120);
    assert!(flagged.is_some(), "a command approval needs input");
    std::thread::sleep(Duration::from_secs(3));
    assert!(blocked(&session), "it stays flagged while it waits");
    session.write_input(b"\x1b").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    finish(session, &root, "cxapprove");
}

#[test]
fn real_codex_question_needs_input() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let Some(session) = start_codex(
        &root,
        "cxask",
        "-a on-request -s read-only --enable default_mode_request_user_input",
    ) else {
        return;
    };
    type_line(
        &session,
        "Use your request_user_input tool to ask me one question: red or blue? Do nothing else.",
    );
    let (_, flagged) = flagged_after(&session, "enter to submit", 120);
    assert!(flagged.is_some(), "a question needs input");
    std::thread::sleep(Duration::from_secs(3));
    assert!(blocked(&session), "it stays flagged while it waits");
    finish(session, &root, "cxask");
}

#[test]
fn real_claude_subagent_permission_needs_input() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let session = start_claude(&root, "subperm", "--permission-mode default");
    type_line(
        &session,
        "Use the Task tool to launch one general-purpose subagent whose only job is to run this exact bash command: date > sub.txt",
    );
    let (_, flagged) = flagged_after(&session, "do you want to proceed", 120);
    assert!(
        flagged.is_some(),
        "a subagent's permission prompt needs input"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(blocked(&session), "it stays flagged while it waits");
    session.write_input(b"\x1b").unwrap();
    finish(session, &root, "subperm");
}

/// Claude the way a Diri tab runs it: the `claude-code` manifest with hooks
/// as its authority. Its hooks are written to `hooks/` by a `--settings`
/// file and fed to the session the way the Engine's `hook.report` does.
struct ClaudeTab {
    session: Session,
    hooks: std::path::PathBuf,
    seen: std::collections::HashSet<std::path::PathBuf>,
}

impl ClaudeTab {
    fn start(root: &Path, id: &str) -> Self {
        let home = std::env::var("HOME").unwrap();
        let hooks = root.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let mut config = serde_json::Map::new();
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Notification",
            "Stop",
            "SubagentStart",
            "SubagentStop",
        ] {
            let command = format!("cat > \"$(mktemp {}/{event}.XXXXXX)\"", hooks.display());
            config.insert(
                event.into(),
                serde_json::json!([{"hooks": [{"type": "command", "command": command}]}]),
            );
        }
        let settings = root.join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({ "hooks": config }).to_string(),
        )
        .unwrap();
        let path = format!("{home}/.local/bin:/opt/homebrew/bin:/usr/bin:/bin");
        let pty = PtySpec::new(
            vec![
                format!("{home}/.local/bin/claude"),
                "--model".into(),
                "haiku".into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
                "--settings".into(),
                settings.display().to_string(),
            ],
            root,
        )
        .env("HOME", &home)
        .env("USER", &std::env::var("USER").unwrap_or_default())
        .env("PATH", &path)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .size(120, 40);
        let (engine, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let logs = root.join(".logs");
        std::fs::create_dir_all(&logs).unwrap();
        let session = Session::spawn(
            SessionSpec {
                id: id.into(),
                pty,
                manifest_id: "claude-code".into(),
                authority: Authority::HooksPrimary,
                logs_dir: logs,
                holder: None,
                remote: None,
                defer_launch: false,
            },
            Arc::new(engine),
        )
        .expect("spawn");
        Self {
            session,
            hooks,
            seen: Default::default(),
        }
    }

    /// Delivers every hook written since the last call, oldest first.
    fn pump(&mut self) {
        let mut fresh: Vec<_> = std::fs::read_dir(&self.hooks)
            .unwrap()
            .flatten()
            .filter(|entry| !self.seen.contains(&entry.path()))
            .filter_map(|entry| {
                let text = std::fs::read_to_string(entry.path()).ok()?;
                let payload: serde_json::Value = serde_json::from_str(&text).ok()?;
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.path(), payload))
            })
            .collect();
        fresh.sort_by_key(|(modified, ..)| *modified);
        for (_, path, payload) in fresh {
            self.seen.insert(path.clone());
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let event = name.split('.').next().unwrap().to_owned();
            if let Some((signal, meta)) = diri_engine::hooks::parse_claude_hook(
                &event,
                &payload,
                std::time::SystemTime::now(),
            ) {
                self.session.feed_identified_signal(signal, meta.identity);
            }
        }
    }

    fn wait(&mut self, what: &str, secs: u64, check: impl Fn(&Session) -> bool) -> Option<Instant> {
        let started = Instant::now();
        let mut last = None;
        while started.elapsed() < Duration::from_secs(secs) {
            self.pump();
            let status = self.session.status();
            if last.as_ref() != Some(&status) {
                eprintln!("{:>6.2}s {status:?}", started.elapsed().as_secs_f32());
                last = Some(status);
            }
            if check(&self.session) {
                return Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("--- {what}: timed out");
        None
    }
}

/// The owner's report, end to end: a turn that launches a background agent
/// and hands back a question must read NeedsInput while the agent runs.
#[test]
fn real_claude_tab_question_during_background_agent_needs_input() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut tab = ClaudeTab::start(&root, "tabq");
    assert!(
        tab.wait("idle", 40, |s| s.status() == SessionStatus::Idle)
            .is_some()
    );
    std::thread::sleep(Duration::from_secs(2));
    type_line(
        &tab.session,
        "Launch one general-purpose subagent with the Agent tool, in the background, whose only job is to run the bash command `python3 -c 'import time; time.sleep(45)'` and then reply ok. Do not wait for it. Your final message must end with exactly this question: Should I also check the logs?",
    );
    // The prompt's echo holds the question once; the reply makes it two.
    let asked = tab.wait("asked", 120, |s| {
        let lines = s.screen_lines();
        lines
            .iter()
            .filter(|line| line.contains("Should I also check the logs?"))
            .count()
            >= 2
            && !lines.iter().any(|line| line.contains("esc to interrupt"))
    });
    assert!(asked.is_some(), "the turn ended on the question");
    let flagged = tab.wait("flagged", 20, |s| {
        s.status() == SessionStatus::NeedsInput(NeedsInputKind::Question)
    });
    let view = tab.session.view();
    eprintln!(
        "--- flagged {:?} after the question showed; detail {:?}",
        flagged
            .zip(asked)
            .map(|(f, a)| f.saturating_duration_since(a)),
        view.needs_input.map(|d| (d.source, d.summary))
    );
    assert!(
        flagged.is_some(),
        "a question asked during background work needs input"
    );
    std::thread::sleep(Duration::from_secs(5));
    tab.pump();
    assert_eq!(
        tab.session.status(),
        SessionStatus::NeedsInput(NeedsInputKind::Question),
        "it stays flagged while the agent runs"
    );
    finish(tab.session, &root, "tabq");
}
