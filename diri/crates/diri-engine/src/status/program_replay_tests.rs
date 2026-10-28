//! `OSC 7501` as real agents send it, replayed byte for byte through the
//! terminal parser into the reducer and its attention lifecycle.
//!
//! The sequences were captured on 2026-10-27 from Claude Code 2.1.311 and
//! Codex 0.163.0 on a PTY that answered their `OSC 7501;?` probe. Only the
//! reports are kept; the screens around them are not needed to read them.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::attention::AttentionKind;
use diri_proto::{NeedsInputKind, SessionStatus, StatusEvidenceSource};
use diri_terminal_state::{HeadlessScreen, ProgramRemoval};

use super::{Authority, ProgramReporters, ReducerOutcome, StatusReducer, StatusSignal};
use crate::detect::ManifestEngine;

/// Claude Code 2.1.311, BEL-terminated.
mod claude {
    pub const IDLE: &[u8] = b"\x1b]7501;state=idle:app=claude-code\x07";
    pub const WORKING: &[u8] = b"\x1b]7501;state=working:app=claude-code\x07";
    pub const DONE: &[u8] = b"\x1b]7501;state=done:app=claude-code\x07";
    /// "Writing current date to /tmp/statlab/g.txt"
    pub const WORKING_TOOL: &[u8] = b"\x1b]7501;state=working:app=claude-code:msg=V3JpdGluZyBjdXJyZW50IGRhdGUgdG8gL3RtcC9zdGF0bGFiL2cudHh0\x07";
    /// "approve Bash: date > /tmp/statlab/g.txt"
    pub const BLOCKED: &[u8] = b"\x1b]7501;state=blocked:app=claude-code:kind=permission:msg=YXBwcm92ZSBCYXNoOiBkYXRlID4gL3RtcC9zdGF0bGFiL2cudHh0\x07";
    pub const CLEAR: &[u8] = b"\x1b]7501;state=clear\x07";
}

/// Codex 0.163.0, ST-terminated. It never reports `done`, and its approval
/// carries neither `kind` nor `msg`.
mod codex {
    pub const IDLE: &[u8] = b"\x1b]7501;state=idle:app=codex\x1b\\";
    pub const WORKING: &[u8] = b"\x1b]7501;state=working:app=codex\x1b\\";
    pub const BLOCKED: &[u8] = b"\x1b]7501;state=blocked:app=codex\x1b\\";
    pub const CLEAR: &[u8] = b"\x1b]7501;state=clear\x1b\\";
}

fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn reporters() -> Arc<ProgramReporters> {
    let (engine, failed) = ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
        .expect("bundled manifests");
    assert!(failed.is_empty(), "{failed:?}");
    engine.program_reporters()
}

/// A terminal and the reducer reading it, wired the way a session wires
/// them: a changed summary, or why there is none, goes to the reducer.
struct Replay {
    screen: HeadlessScreen,
    reducer: StatusReducer,
    generation: u64,
    published: Option<diri_terminal_state::ProgramRecord>,
    now: SystemTime,
}

impl Replay {
    fn new(authority: Authority, manifest: &str) -> Self {
        let mut reducer = StatusReducer::new(authority, t0())
            .with_manifest(manifest, Some("1"))
            .with_program_reporters(reporters());
        let now = t0() + Duration::from_secs(30);
        // Past startup: the agent drew its UI.
        reducer.reduce(StatusSignal::PtyOutputActivity, now);
        Self {
            screen: HeadlessScreen::new(120, 40).with_notifications(),
            reducer,
            generation: 0,
            published: None,
            now,
        }
    }

    fn step(&mut self) {
        self.now += Duration::from_millis(500);
    }

    fn observe(&mut self) -> ReducerOutcome {
        let generation = self.screen.program_status_generation();
        if generation == self.generation {
            return ReducerOutcome::default();
        }
        self.generation = generation;
        let next = self.screen.program_status();
        if next == self.published {
            return ReducerOutcome::default();
        }
        self.published = next.clone();
        let signal = match (next, self.screen.program_status_removal()) {
            (None, Some(reason)) => StatusSignal::ProgramStatusRemoved(reason),
            (report, _) => StatusSignal::ProgramStatus(report),
        };
        self.reducer.reduce(signal, self.now)
    }

    fn feed(&mut self, bytes: &[u8]) -> ReducerOutcome {
        self.step();
        self.screen.feed(bytes);
        self.observe()
    }

    fn end_program(&mut self, reason: ProgramRemoval) -> ReducerOutcome {
        self.step();
        self.screen.end_program_status(reason);
        self.observe()
    }

    fn submit(&mut self) {
        self.step();
        self.reducer.reduce(StatusSignal::UserSubmission, self.now);
    }

    fn unresolved(&self, kind: AttentionKind) -> usize {
        self.reducer
            .attention_state()
            .expect("attention")
            .events
            .iter()
            .filter(|event| event.kind == kind && !event.resolved)
            .count()
    }

    fn events(&self, kind: AttentionKind) -> usize {
        self.reducer
            .attention_state()
            .expect("attention")
            .events
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }
}

/// A finished Claude turn reports `done`: one completion, announced once.
#[test]
fn claude_done_completes_its_turn() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    replay.feed(claude::IDLE);
    assert_eq!(*replay.reducer.status(), SessionStatus::Idle);

    replay.submit();
    let outcome = replay.feed(claude::WORKING);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    let outcome = replay.feed(claude::DONE);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);
    assert!(!outcome.turn_failed);
    assert_eq!(replay.unresolved(AttentionKind::Completion), 1);
    let evidence = replay.reducer.evidence().expect("evidence");
    assert_eq!(evidence.source, StatusEvidenceSource::ProgramStatus);
    assert_eq!(evidence.matched_rule_id.as_deref(), Some("done"));
}

/// Esc at Claude's permission prompt makes it report `idle`, which the
/// specification reserves for a turn cancelled without a result. It used to
/// be read like `done`: a "Claude finished" notification for a denial.
#[test]
fn claude_denied_permission_is_not_a_finished_turn() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    replay.feed(claude::IDLE);
    replay.submit();
    replay.feed(claude::WORKING);
    replay.feed(claude::WORKING_TOOL);
    let outcome = replay.feed(claude::BLOCKED);
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Permission))
    );
    let detail = outcome.needs_input.expect("request");
    assert_eq!(detail.summary, "approve Bash: date > /tmp/statlab/g.txt");
    assert_eq!(detail.tool_name, None, "claude-code is the app, not a tool");
    assert_eq!(replay.unresolved(AttentionKind::Request), 1);

    // Esc.
    let outcome = replay.feed(claude::IDLE);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(!outcome.turn_completed, "a denial is not a result");
    assert!(outcome.turn_abandoned);
    assert_eq!(replay.events(AttentionKind::Completion), 0);
    assert_eq!(
        replay.unresolved(AttentionKind::Request),
        0,
        "the denied request is settled"
    );

    // The next turn still completes normally.
    replay.submit();
    replay.feed(claude::WORKING);
    let outcome = replay.feed(claude::DONE);
    assert!(outcome.turn_completed);
    assert_eq!(replay.unresolved(AttentionKind::Completion), 1);

    // ^C^C: Claude clears its record on the way out.
    let outcome = replay.feed(claude::CLEAR);
    assert!(!outcome.turn_completed);
    assert_eq!(
        replay
            .reducer
            .evidence()
            .and_then(|e| e.matched_rule_id.as_deref()),
        Some("removed:clear")
    );
}

/// Codex reports `idle`, never `done`, when a turn finishes. Its manifest
/// says so, and that keeps its turns completing.
#[test]
fn codex_idle_after_work_completes_its_turn() {
    let mut replay = Replay::new(Authority::ScreenPrimary, "codex");
    // Startup: idle, a moment of its own work, idle.
    replay.feed(codex::IDLE);
    replay.feed(codex::WORKING);
    replay.feed(codex::IDLE);
    assert_eq!(*replay.reducer.status(), SessionStatus::Idle);

    replay.submit();
    let outcome = replay.feed(codex::WORKING);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    let outcome = replay.feed(codex::IDLE);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed, "idle is how Codex finishes a turn");
    assert!(!outcome.turn_abandoned);
    assert_eq!(replay.unresolved(AttentionKind::Completion), 1);

    // An approval names neither kind nor message: a question.
    replay.submit();
    replay.feed(codex::WORKING);
    let outcome = replay.feed(codex::BLOCKED);
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Question))
    );
    assert_eq!(outcome.needs_input.expect("request").tool_name, None);

    let outcome = replay.feed(codex::CLEAR);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
}

/// The same Codex reports from an agent typed at a shell prompt: the record's
/// `app` is what identifies the program, not the session's manifest.
#[test]
fn codex_typed_in_a_shell_is_read_by_its_app() {
    let mut replay = Replay::new(Authority::ProcessOnly, "shell");
    replay
        .reducer
        .reduce(StatusSignal::ForegroundJob { running: true }, replay.now);
    replay.feed(codex::IDLE);
    replay.submit();
    replay.feed(codex::WORKING);
    let outcome = replay.feed(codex::IDLE);
    assert!(outcome.turn_completed);

    // Claude in the same shell: its idle is not a result.
    let mut replay = Replay::new(Authority::ProcessOnly, "shell");
    replay
        .reducer
        .reduce(StatusSignal::ForegroundJob { running: true }, replay.now);
    replay.feed(claude::IDLE);
    replay.submit();
    replay.feed(claude::WORKING);
    let outcome = replay.feed(claude::IDLE);
    assert!(!outcome.turn_completed);
    assert!(outcome.turn_abandoned);
}

/// `error` is a result, and a failed one: the turn ends with a Failure in
/// place of a Completion, which notifications show as stopped, not finished.
#[test]
fn a_reported_error_fails_the_turn() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    replay.feed(claude::IDLE);
    replay.submit();
    replay.feed(claude::WORKING);
    let outcome = replay.feed(b"\x1b]7501;state=error:app=claude-code\x07");
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);
    assert!(outcome.turn_failed);
    assert_eq!(replay.unresolved(AttentionKind::Failure), 1);
    assert_eq!(replay.events(AttentionKind::Completion), 0);

    // The next turn settles it.
    replay.submit();
    replay.feed(claude::WORKING);
    assert_eq!(replay.unresolved(AttentionKind::Failure), 0);
    let outcome = replay.feed(claude::DONE);
    assert!(!outcome.turn_failed);
    assert_eq!(replay.unresolved(AttentionKind::Completion), 1);
}

/// `kind=auth` stays a sign-in blocker: no Approve/Deny, no typed reply.
#[test]
fn an_auth_blocker_is_not_a_question() {
    let mut replay = Replay::new(Authority::ProcessOnly, "shell");
    replay
        .reducer
        .reduce(StatusSignal::ForegroundJob { running: true }, replay.now);
    let outcome = replay.feed(b"\x1b]7501;state=blocked:kind=auth:app=gh\x07");
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Auth))
    );
    let detail = outcome.needs_input.expect("request");
    assert_eq!(detail.kind, NeedsInputKind::Auth);
    assert_eq!(detail.summary, "Waiting for sign-in");
    assert_eq!(detail.tool_name, None);
}

/// The evidence says why a program's status went away.
#[test]
fn removal_reasons_reach_the_evidence() {
    for (reason, rule) in [
        (ProgramRemoval::Foreground, "removed:foreground"),
        (ProgramRemoval::ChildExited, "removed:child_exited"),
    ] {
        let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
        replay.feed(claude::WORKING);
        replay.end_program(reason);
        assert_eq!(
            replay
                .reducer
                .evidence()
                .and_then(|e| e.matched_rule_id.as_deref()),
            Some(rule)
        );
    }
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    replay.feed(claude::WORKING);
    replay.feed(b"\x1bc");
    assert_eq!(
        replay
            .reducer
            .evidence()
            .and_then(|e| e.matched_rule_id.as_deref()),
        Some("removed:reset")
    );
}
