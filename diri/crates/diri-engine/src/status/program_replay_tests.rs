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

    /// A Claude hook, parsed from its payload the way the Engine parses it.
    fn hook(&mut self, event: &str, payload: serde_json::Value) -> ReducerOutcome {
        self.step();
        let (signal, meta) =
            crate::hooks::parse_claude_hook(event, &payload, self.now).expect("modelled hook");
        self.reducer
            .reduce_identified(signal, meta.identity, self.now)
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

/// Claude Code 2.1.312 with one background subagent running, captured
/// 2026-10-27: the root record stays `working` until the subagent's record
/// clears, and `done` follows only once all of it is over.
mod background {
    pub const WORKING_TOOL: &[u8] = b"\x1b]7501;state=working:app=claude-code:msg=UnVubmluZyBzbGVlcCAzNSBhbmQgcmVwbHlpbmcgb2s=\x07";
    pub const AGENT: &[u8] = b"\x1b]7501;state=working:app=claude-code:id=a59741d8bd32f0e49:title=UnVuIHNsZWVwIDM1IGFuZCByZXBseSBvaw==\x07";
    pub const AGENT_CLEAR: &[u8] = b"\x1b]7501;state=clear:id=a59741d8bd32f0e49\x07";
}

/// The `Stop` payload Claude Code 2.1.312 sends (paths and ids replaced),
/// with `background_tasks` as it reports a still-running subagent.
fn stop_payload(last_message: &str, background_running: bool) -> serde_json::Value {
    let tasks = if background_running {
        serde_json::json!([{"id": "a59741d8bd32f0e49", "type": "subagent", "status": "running",
            "description": "Run the migration", "agent_type": "general-purpose"}])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "session_id": "8d2c128b-bd49-4562-a87a-8bfb8a10f11c",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": "/tmp/project",
        "permission_mode": "default",
        "hook_event_name": "Stop",
        "stop_hook_active": false,
        "last_assistant_message": last_message,
        "background_tasks": tasks,
        "session_crons": [],
    })
}

fn prompt_payload() -> serde_json::Value {
    serde_json::json!({"session_id": "8d2c128b-bd49-4562-a87a-8bfb8a10f11c",
        "hook_event_name": "UserPromptSubmit", "prompt": "go"})
}

/// A turn that starts a background subagent and then ends on a question.
fn background_turn(replay: &mut Replay) {
    replay.feed(claude::IDLE);
    replay.submit();
    replay.hook("UserPromptSubmit", prompt_payload());
    replay.feed(claude::WORKING);
    replay.feed(background::WORKING_TOOL);
    replay.feed(background::AGENT);
    assert_eq!(*replay.reducer.status(), SessionStatus::Working);
}

/// The owner's "it waits for input and we miss it": an orchestrating Claude
/// launches background agents and hands the turn back with a question. Its
/// program status stays `working` for as long as the agents run, so the tab
/// read Working, for minutes, while Claude waited on the user.
#[test]
fn a_turn_ending_on_a_question_during_background_work_needs_input() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    background_turn(&mut replay);

    let outcome = replay.hook(
        "Stop",
        stop_payload(
            "I started the migration agent in the background.\n\nShould I also update the **docs**?",
            true,
        ),
    );
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Question))
    );
    let detail = outcome.needs_input.expect("question");
    assert_eq!(detail.source, diri_proto::NeedsInputSource::ClaudeStopHook);
    assert_eq!(detail.summary, "Should I also update the **docs**?");
    assert!(!outcome.turn_completed, "the background work is not done");
    assert_eq!(replay.unresolved(AttentionKind::Request), 1);

    // The program keeps reporting the background agent's work, and Claude
    // reminds that it is still running: neither answers the question.
    let outcome = replay.feed(claude::WORKING);
    assert_eq!(outcome.status_change, None);
    replay.hook(
        "Notification",
        serde_json::json!({"session_id": "8d2c128b-bd49-4562-a87a-8bfb8a10f11c",
            "hook_event_name": "Notification", "notification_type": "idle_prompt",
            "background_tasks": [{"status": "running"}]}),
    );
    assert_eq!(
        *replay.reducer.status(),
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );

    // The user answers: a new turn.
    replay.submit();
    let outcome = replay.hook("UserPromptSubmit", prompt_payload());
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert_eq!(replay.unresolved(AttentionKind::Request), 0);

    // The agent finishes; Claude reports its result.
    replay.feed(background::AGENT_CLEAR);
    let outcome = replay.feed(claude::DONE);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);
}

/// The background agent finishing first starts Claude's next turn on its
/// own (its report arrives as a prompt): the question is superseded.
#[test]
fn a_background_result_supersedes_the_question() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    background_turn(&mut replay);
    replay.hook("Stop", stop_payload("Want me to run the tests too?", true));
    replay.feed(background::AGENT_CLEAR);
    let outcome = replay.hook("UserPromptSubmit", prompt_payload());
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert_eq!(replay.unresolved(AttentionKind::Request), 0);
    let outcome = replay.feed(claude::DONE);
    assert!(outcome.turn_completed);
}

/// A turn that hands back no question while its agents run is still the
/// agents' work: Working, as before, and nothing raised.
#[test]
fn background_work_without_a_question_stays_working() {
    for last in [
        "I started the migration agent in the background. I'll report back when it finishes.",
        "Launched:\n\n```sh\nmake migrate # done?\n```",
        "",
    ] {
        let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
        background_turn(&mut replay);
        let outcome = replay.hook("Stop", stop_payload(last, true));
        assert_eq!(outcome.needs_input, None, "{last:?}");
        assert_eq!(*replay.reducer.status(), SessionStatus::Working, "{last:?}");
        assert_eq!(replay.unresolved(AttentionKind::Request), 0);
    }
}

/// With nothing left running, a turn that ends on a question is a finished
/// turn, Done as it always was, whichever of the hook and the report lands
/// first.
#[test]
fn a_question_with_nothing_in_the_background_is_still_done() {
    for hook_first in [true, false] {
        let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
        replay.feed(claude::IDLE);
        replay.submit();
        replay.hook("UserPromptSubmit", prompt_payload());
        replay.feed(claude::WORKING);
        let mut completed = false;
        if hook_first {
            let outcome = replay.hook("Stop", stop_payload("Shall I open a PR?", false));
            assert_eq!(outcome.needs_input, None);
        }
        completed |= replay.feed(claude::DONE).turn_completed;
        if !hook_first {
            let outcome = replay.hook("Stop", stop_payload("Shall I open a PR?", false));
            assert_eq!(outcome.needs_input, None);
        }
        assert!(completed);
        assert_eq!(*replay.reducer.status(), SessionStatus::Idle);
        assert_eq!(replay.unresolved(AttentionKind::Request), 0);
        assert_eq!(replay.unresolved(AttentionKind::Completion), 1);
    }
}

/// A real blocker after the question replaces it, and is cleared by the
/// program as usual.
#[test]
fn a_blocker_replaces_the_turn_end_question() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    background_turn(&mut replay);
    replay.hook("Stop", stop_payload("Should I continue?", true));
    let outcome = replay.feed(claude::BLOCKED);
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Permission))
    );
    let outcome = replay.feed(claude::WORKING);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
}

/// Without `OSC 7501` (an older Claude, or an Engine restart that lost the
/// records), hooks and the screen read the same turn the same way.
#[test]
fn hooks_alone_raise_the_turn_end_question() {
    let mut replay = Replay::new(Authority::HooksPrimary, "claude-code");
    replay.submit();
    replay.hook("UserPromptSubmit", prompt_payload());
    assert_eq!(*replay.reducer.status(), SessionStatus::Working);
    let outcome = replay.hook("Stop", stop_payload("Which branch should I use?", true));
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Question))
    );
    // Claude's footer then reads "Waiting for 1 background agent to finish".
    replay.step();
    let outcome = replay.reducer.reduce(
        StatusSignal::Screen(crate::detect::ScreenObservation {
            state: crate::detect::ManifestState::Working,
            matched_rule_id: "working-background-agents".into(),
            priority: 880,
            content_seq: 7,
            prompt_excerpt: None,
            options: None,
            stale_after_submit: false,
            turn_started: false,
        }),
        replay.now,
    );
    assert_eq!(outcome.status_change, None);
    // An `agent_completed` reminder with work still running.
    replay.hook(
        "Notification",
        serde_json::json!({"session_id": "8d2c128b-bd49-4562-a87a-8bfb8a10f11c",
            "hook_event_name": "Notification", "notification_type": "agent_completed",
            "background_tasks": [{"status": "running"}]}),
    );
    assert_eq!(
        *replay.reducer.status(),
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );
    let outcome = replay.hook("UserPromptSubmit", prompt_payload());
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
}
