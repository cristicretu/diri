//! Reducer behavior.
//!
//! These encode *why* the state machine is shaped the way it is: each test
//! names a real failure mode the daemon had to stop having. Time is passed in,
//! so debounce behavior is exercised without sleeping.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::{
    ExitReason, NeedsInputKind, RiskHint, SessionStatus, StatusEvidenceSource, StatusFallbackReason,
};

use super::*;
use crate::detect::{ManifestState, ScreenObservation};

fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn observation(state: ManifestState, seq: u64) -> ScreenObservation {
    ScreenObservation {
        state,
        matched_rule_id: "test".into(),
        priority: 100,
        content_seq: seq,
        prompt_excerpt: None,
        options: None,
    }
}

#[test]
fn evidence_covers_every_authority_without_terminal_content() {
    let mut hooks =
        StatusReducer::new(Authority::HooksPrimary, t0()).with_manifest("claude-code", Some("4"));
    let now = settled(&mut hooks, t0());
    let hook_outcome = hooks.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    let hook_evidence = hook_outcome.status_evidence.expect("hook evidence");
    assert_eq!(hook_evidence.source, StatusEvidenceSource::Hook);
    assert_eq!(hook_evidence.status, SessionStatus::Working);
    assert_eq!(hook_evidence.manifest_id.as_deref(), Some("claude-code"));
    assert_eq!(hook_evidence.manifest_version.as_deref(), Some("4"));
    assert_eq!(hook_evidence.matched_rule_id, None);

    let mut screen =
        StatusReducer::new(Authority::ScreenPrimary, t0()).with_manifest("codex", Some("8"));
    let now = settled(&mut screen, t0());
    let screen_outcome = screen.reduce(
        StatusSignal::Screen(ScreenObservation {
            matched_rule_id: "codex-working-spinner".into(),
            ..observation(ManifestState::Working, 1)
        }),
        now,
    );
    let screen_evidence = screen_outcome.status_evidence.expect("screen evidence");
    assert_eq!(screen_evidence.source, StatusEvidenceSource::ScreenRule);
    assert_eq!(
        screen_evidence.matched_rule_id.as_deref(),
        Some("codex-working-spinner")
    );

    let mut process =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("shell", Some("1"));
    let process_outcome = process.reduce(StatusSignal::PtyOutputActivity, t0());
    let process_evidence = process_outcome.status_evidence.expect("process evidence");
    assert_eq!(
        process_evidence.source,
        StatusEvidenceSource::ProcessLiveness
    );
    assert_eq!(
        process_evidence.fallback_reason,
        Some(StatusFallbackReason::ProcessOnly)
    );

    let serialized = serde_json::to_string(&[hook_evidence, screen_evidence, process_evidence])
        .expect("serialize evidence");
    for forbidden in ["prompt", "terminal", "/Users/", "SECRET="] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn stale_working_status_gets_staleness_evidence() {
    let mut reducer =
        StatusReducer::new(Authority::HooksPrimary, t0()).with_manifest("claude-code", Some("4"));
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    let outcome = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(61));
    assert_eq!(outcome.status_change, Some(SessionStatus::Unknown));
    let evidence = outcome.status_evidence.expect("staleness evidence");
    assert_eq!(evidence.source, StatusEvidenceSource::Staleness);
    assert_eq!(
        evidence.fallback_reason,
        Some(StatusFallbackReason::StaleSignals)
    );
    assert_eq!(evidence.status, SessionStatus::Unknown);
}

#[test]
fn anti_flicker_is_visible_in_evidence_before_idle_commits() {
    let mut reducer =
        StatusReducer::new(Authority::ScreenPrimary, t0()).with_manifest("codex", Some("8"));
    let now = settled(&mut reducer, t0());
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    let pending = reducer.reduce(
        StatusSignal::Screen(ScreenObservation {
            matched_rule_id: "codex-idle".into(),
            ..observation(ManifestState::Idle, 2)
        }),
        now + Duration::from_millis(100),
    );
    let evidence = pending.status_evidence.expect("anti-flicker evidence");
    assert_eq!(evidence.status, SessionStatus::Working);
    assert!(evidence.anti_flicker_active);
    assert_eq!(evidence.matched_rule_id.as_deref(), Some("codex-idle"));
}

fn blocker(seq: u64, excerpt: &str) -> ScreenObservation {
    ScreenObservation {
        state: ManifestState::BlockedPermission,
        matched_rule_id: "permission".into(),
        priority: 1000,
        content_seq: seq,
        prompt_excerpt: Some(excerpt.into()),
        options: Some(vec!["Yes".into(), "No".into()]),
    }
}

/// Past the startup grace, so screen observations are honored.
fn settled(reducer: &mut StatusReducer, now: SystemTime) -> SystemTime {
    let later = now + Duration::from_secs(5);
    reducer.reduce(StatusSignal::Tick, later);
    later
}

fn hook(hook: ClaudeHook) -> StatusSignal {
    StatusSignal::ClaudeHook {
        hook,
        is_subagent: false,
        pending_work: None,
    }
}

#[test]
fn a_normal_claude_turn_completes_exactly_once() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());

    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    assert_eq!(*reducer.status(), SessionStatus::Working);

    // Stop is a strong idle: one confirmation is enough.
    let outcome = reducer.reduce(hook(ClaudeHook::Stop), now + Duration::from_millis(100));
    let outcome = if outcome.status_change.is_none() {
        reducer.reduce(StatusSignal::Tick, now + Duration::from_millis(200))
    } else {
        outcome
    };

    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed, "the turn should report completion");

    // A second Stop must not fire another completion.
    let again = reducer.reduce(hook(ClaudeHook::Stop), now + Duration::from_millis(300));
    assert!(!again.turn_completed, "completion fires once per turn");
}

#[test]
fn idle_needs_three_screen_confirmations_without_a_strong_signal() {
    // Anti-flicker: a single idle-looking frame mid-turn must not flip the
    // session to idle, or the sidebar strobes while an agent is working.
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let mut now = settled(&mut reducer, t0());

    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    assert_eq!(*reducer.status(), SessionStatus::Working);

    for seq in 2..=3 {
        now += Duration::from_millis(100);
        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, seq)),
            now,
        );
        assert_eq!(
            *reducer.status(),
            SessionStatus::Working,
            "still working after {} idle frames",
            seq - 1
        );
    }

    now += Duration::from_millis(100);
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 4)),
        now,
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
}

#[test]
fn a_work_signal_cancels_a_pending_idle_candidacy() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let mut now = settled(&mut reducer, t0());

    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    now += Duration::from_millis(100);
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 2)),
        now,
    );

    // Work resumes: the two idle confirmations so far must be discarded.
    now += Duration::from_millis(100);
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 3)),
        now,
    );
    now += Duration::from_millis(100);
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 4)),
        now,
    );

    assert_eq!(
        *reducer.status(),
        SessionStatus::Working,
        "idle confirmations restart after work resumes"
    );
}

#[test]
fn a_visible_blocker_outranks_a_working_hook() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());

    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    let outcome = reducer.reduce(
        StatusSignal::Screen(blocker(1, "Do you want to proceed?")),
        now + Duration::from_millis(50),
    );

    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Permission))
    );
    let detail = outcome.needs_input.expect("a detail for the prompt");
    assert_eq!(detail.summary, "Do you want to proceed?");
    assert_eq!(
        detail.options.as_deref(),
        Some(&["Yes".to_string(), "No".to_string()][..])
    );
}

#[test]
fn a_blocker_survives_one_stray_non_blocker_frame() {
    // Releasing on a single miss made prompts flicker away while the user was
    // still reading them.
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let mut now = settled(&mut reducer, t0());

    reducer.reduce(StatusSignal::Screen(blocker(1, "proceed?")), now);
    assert!(matches!(*reducer.status(), SessionStatus::NeedsInput(_)));

    now += Duration::from_millis(100);
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 2)),
        now,
    );
    assert!(
        matches!(*reducer.status(), SessionStatus::NeedsInput(_)),
        "one miss is not enough to clear the prompt"
    );

    now += Duration::from_millis(100);
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 3)),
        now,
    );
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::Idle),
        "two consecutive misses release it"
    );
}

#[test]
fn a_dismissed_blocker_clears_when_the_composer_stops_redrawing() {
    // Kimi paints the idle composer once after workspace trust, then goes
    // quiet. A second changed content sequence may never arrive.
    for authority in [Authority::ScreenPrimary, Authority::HooksPrimary] {
        let mut reducer = StatusReducer::new(authority, t0());
        let now = settled(&mut reducer, t0());
        if authority == Authority::HooksPrimary {
            reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
        }
        reducer.reduce(StatusSignal::Screen(blocker(1, "Trust this folder?")), now);
        let idle_at = now + Duration::from_millis(100);
        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, 2)),
            idle_at,
        );
        reducer.reduce(StatusSignal::Tick, idle_at + Duration::from_millis(100));
        assert!(matches!(reducer.status(), SessionStatus::NeedsInput(_)));
        let outcome = reducer.reduce(StatusSignal::Tick, idle_at + Duration::from_millis(701));
        assert_eq!(
            outcome.status_change,
            Some(if authority == Authority::HooksPrimary {
                SessionStatus::Working // An unfinished hook-owned turn stays alive.
            } else {
                SessionStatus::Idle
            })
        );
    }
}

#[test]
fn a_reappearing_or_hidden_blocker_cancels_the_quiet_clear() {
    for state in [ManifestState::BlockedPermission, ManifestState::Skip] {
        let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
        let now = settled(&mut reducer, t0());
        reducer.reduce(StatusSignal::Screen(blocker(1, "proceed?")), now);
        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, 2)),
            now,
        );
        reducer.reduce(
            StatusSignal::Screen(observation(state, 3)),
            now + Duration::from_millis(100),
        );
        reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(2));
        assert!(matches!(reducer.status(), SessionStatus::NeedsInput(_)));
    }
}

#[test]
fn a_skip_screen_holds_the_current_state() {
    // The transcript viewer covers the prompt; the session has not changed.
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());

    reducer.reduce(StatusSignal::Screen(blocker(1, "proceed?")), now);
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Skip, 2)),
        now + Duration::from_millis(100),
    );

    assert_eq!(outcome.status_change, None);
    assert!(matches!(*reducer.status(), SessionStatus::NeedsInput(_)));
}

#[test]
fn startup_grace_holds_starting_until_something_definitive() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());

    // Inside the grace window an idle screen proves nothing: the agent may
    // simply not have painted yet.
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 1)),
        t0() + Duration::from_millis(500),
    );
    assert_eq!(*reducer.status(), SessionStatus::Starting);

    // SessionStart is definitive.
    let outcome = reducer.reduce(
        hook(ClaudeHook::SessionStart),
        t0() + Duration::from_secs(1),
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
}

#[test]
fn an_idle_snapshot_inside_startup_grace_is_reconsidered_after_the_grace() {
    for authority in [Authority::ScreenPrimary, Authority::HooksPrimary] {
        let mut reducer = StatusReducer::new(authority, t0());

        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, 1)),
            t0() + Duration::from_millis(100),
        );
        assert_eq!(*reducer.status(), SessionStatus::Starting);

        let outcome = reducer.reduce(StatusSignal::Tick, t0() + Duration::from_secs(4));
        assert_eq!(
            outcome.status_change,
            Some(SessionStatus::Idle),
            "a reconnect snapshot may be the only screen frame after startup"
        );
    }
}

#[test]
fn an_adopted_session_honors_its_first_snapshot_without_launch_grace() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    reducer.finish_startup_grace(t0());

    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 1)),
        t0(),
    );

    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
}

#[test]
fn a_working_screen_ends_startup_early_for_screen_primary_agents() {
    // Codex has no hooks, so a working screen is the definitive signal.
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        t0() + Duration::from_millis(500),
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
}

#[test]
fn subagent_events_never_move_the_parent() {
    // A subagent finishing is not the parent finishing — this is what made
    // sessions report done while still working.
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);

    let outcome = reducer.reduce(
        StatusSignal::ClaudeHook {
            hook: ClaudeHook::Stop,
            is_subagent: true,
            pending_work: None,
        },
        now + Duration::from_millis(100),
    );

    assert_eq!(outcome.status_change, None);
    assert_eq!(*reducer.status(), SessionStatus::Working);
}

#[test]
fn subagent_lifecycle_is_counted_but_not_canonical() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());

    reducer.reduce(hook(ClaudeHook::SubagentStart("a".into())), now);
    reducer.reduce(hook(ClaudeHook::SubagentStart("b".into())), now);
    assert_eq!(reducer.active_subagents(), 2);

    reducer.reduce(hook(ClaudeHook::SubagentStop("a".into())), now);
    assert_eq!(reducer.active_subagents(), 1);
    assert_eq!(
        *reducer.status(),
        SessionStatus::Starting,
        "state untouched"
    );
}

#[test]
fn a_permission_hook_produces_a_detail_with_risk() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());

    let outcome = reducer.reduce(
        hook(ClaudeHook::PermissionRequest {
            tool_name: Some("Bash".into()),
            input_summary: Some("rm -rf build".into()),
        }),
        now,
    );

    let detail = outcome.needs_input.expect("detail");
    assert_eq!(detail.summary, "wants to run `rm -rf build`");
    assert_eq!(detail.risk_hint, RiskHint::Destructive);
    assert_eq!(detail.kind, NeedsInputKind::Permission);
}

#[test]
fn a_notification_asking_a_question_needs_input() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());

    let outcome = reducer.reduce(
        hook(ClaudeHook::Notification {
            notification_type: Some("agent_needs_input".into()),
            message: Some("Waiting for your answer".into()),
        }),
        now,
    );

    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Question))
    );
}

#[test]
fn codex_turn_complete_then_a_tick_settles_to_idle() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let mut now = settled(&mut reducer, t0());

    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    now += Duration::from_millis(100);

    // Turn-complete alone is a strong signal but the screen has not confirmed.
    let outcome = reducer.reduce(StatusSignal::CodexTurnComplete, now);
    assert_eq!(outcome.status_change, None);

    now += Duration::from_millis(100);
    let outcome = reducer.reduce(StatusSignal::Tick, now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);
    let evidence = outcome
        .status_evidence
        .expect("the delayed decision keeps its notify authority");
    assert_eq!(evidence.status, SessionStatus::Idle);
    assert_eq!(evidence.source, StatusEvidenceSource::Notify);
}

#[test]
fn late_codex_output_keeps_completion_without_completing_twice() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );

    let completed_at = now + Duration::from_millis(100);
    reducer.reduce(StatusSignal::CodexTurnComplete, completed_at);
    let settled = reducer.reduce(
        StatusSignal::Tick,
        completed_at + Duration::from_millis(100),
    );
    assert_eq!(settled.status_change, Some(SessionStatus::Idle));
    assert!(settled.turn_completed);

    let repaint = reducer.reduce(
        StatusSignal::PtyOutputActivity,
        completed_at + Duration::from_secs(4),
    );
    assert_eq!(
        repaint.status_change, None,
        "ordinary stop repaint is ignored"
    );

    let continuing = reducer.reduce(
        StatusSignal::PtyOutputActivity,
        completed_at + Duration::from_secs(6),
    );
    assert_eq!(continuing.status_change, None);
    assert!(!continuing.turn_completed);

    reducer.reduce(
        StatusSignal::CodexTurnComplete,
        completed_at + Duration::from_secs(7),
    );
    let settled_again = reducer.reduce(
        StatusSignal::Tick,
        completed_at + Duration::from_secs(7) + Duration::from_millis(100),
    );
    assert_eq!(settled_again.status_change, None);
    assert!(
        !settled_again.turn_completed,
        "rearming the same logical turn must not notify twice"
    );
}

#[test]
fn a_process_only_agent_goes_working_on_first_output_then_exits() {
    let mut reducer = StatusReducer::new(Authority::ProcessOnly, t0());
    let now = t0() + Duration::from_secs(1);

    let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));

    // Screens mean nothing for this authority.
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 1)),
        now + Duration::from_secs(1),
    );
    assert_eq!(outcome.status_change, None);

    let outcome = reducer.reduce(
        StatusSignal::ProcessExit {
            code: Some(0),
            signal: None,
            interrupted: false,
        },
        now + Duration::from_secs(2),
    );
    match outcome.status_change {
        Some(SessionStatus::Exited(info)) => {
            assert_eq!(info.reason, ExitReason::Exited);
            assert_eq!(info.code, Some(0));
        }
        other => panic!("expected an exit, got {other:?}"),
    }
}

#[test]
fn a_shell_is_idle_at_a_prompt_and_working_only_for_a_foreground_job() {
    let mut reducer =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("shell", Some("1"));
    let now = t0() + Duration::from_secs(1);

    let outcome = reducer.reduce(StatusSignal::PtyOutputActivity, now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(!outcome.turn_completed);

    let outcome = reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert!(!outcome.turn_completed);

    // Output from `sleep` is not required; the job itself is the signal.
    let outcome = reducer.reduce(
        StatusSignal::ForegroundJob { running: true },
        now + Duration::from_secs(1),
    );
    assert_eq!(outcome.status_change, None);

    let outcome = reducer.reduce(
        StatusSignal::ForegroundJob { running: false },
        now + Duration::from_secs(2),
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(!outcome.turn_completed);
}

/// `claude` typed at a shell prompt has no hooks, so while it holds the
/// foreground the shell's status is read from Claude's screen rules. Being
/// recognised is not a finished turn; a turn it then runs is.
#[test]
fn a_shell_lends_its_status_to_an_agent_in_its_foreground() {
    let mut reducer =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("shell", Some("1"));
    let now = t0() + Duration::from_secs(1);
    reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);
    assert_eq!(reducer.status(), &SessionStatus::Working);

    let outcome = reducer.lend_to_agent("claude-code", Some("7"), now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(!outcome.turn_completed);
    assert_eq!(reducer.foreground_agent(), Some("claude-code"));
    assert_eq!(reducer.authority(), Authority::ScreenPrimary);
    let evidence = reducer.evidence().expect("evidence");
    assert_eq!(evidence.manifest_id.as_deref(), Some("claude-code"));
    // Lending twice to the same Agent changes nothing.
    assert_eq!(
        reducer.lend_to_agent("claude-code", Some("7"), now),
        ReducerOutcome::default()
    );
    // Job samples belong to the shell; the Agent's screen decides now.
    let outcome = reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);
    assert_eq!(outcome.status_change, None);

    let mut at = now + Duration::from_millis(100);
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        at,
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    let mut completed = false;
    for seq in 2..20 {
        at += Duration::from_millis(200);
        completed |= reducer
            .reduce(
                StatusSignal::Screen(observation(ManifestState::Idle, seq)),
                at,
            )
            .turn_completed;
        completed |= reducer.reduce(StatusSignal::Tick, at).turn_completed;
    }
    assert_eq!(reducer.status(), &SessionStatus::Idle);
    assert!(completed, "the Agent's own turn completes");

    let outcome = reducer.return_from_agent(false, at);
    assert_eq!(reducer.foreground_agent(), None);
    assert_eq!(reducer.authority(), Authority::ProcessOnly);
    assert_eq!(outcome.status_change, None, "already idle at the prompt");
    let evidence = reducer.evidence().expect("evidence");
    assert_eq!(evidence.manifest_id.as_deref(), Some("shell"));
    let outcome = reducer.reduce(StatusSignal::ForegroundJob { running: true }, at);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
}

fn shell_running_a_job(now: SystemTime) -> StatusReducer {
    let mut reducer =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("shell", Some("1"));
    reducer.reduce(StatusSignal::PtyOutputActivity, now);
    reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);
    assert_eq!(reducer.status(), &SessionStatus::Working);
    reducer
}

fn line(text: &str) -> StatusSignal {
    StatusSignal::TerminalLine(Some(TerminalPrompt {
        line: Some(text.into()),
        secret: false,
    }))
}

fn open_requests(reducer: &StatusReducer) -> usize {
    reducer
        .attention_state()
        .expect("attention")
        .active_requests()
        .count()
}

fn requests(reducer: &StatusReducer) -> usize {
    reducer
        .attention_state()
        .expect("attention")
        .events
        .iter()
        .filter(|event| event.kind == diri_proto::attention::AttentionKind::Request)
        .count()
}

/// `Proceed? [y/N]` in a terminal flags it the way a permission prompt flags
/// an Agent, and only once its output has settled. Typing clears the mark at
/// once; a pause mid-answer marks it again without a second request, and
/// Enter settles the request.
#[test]
fn a_shell_job_reading_a_line_needs_input_until_it_is_answered() {
    let now = t0() + Duration::from_secs(1);
    let mut reducer = shell_running_a_job(now);
    let settle = ReducerTiming::default().line_prompt_settle;

    // Still printing: a read between bursts of output is not a question.
    assert!(!reducer.wants_line_probe(now + settle / 2));
    let outcome = reducer.reduce(line("Proceed? [y/N]"), now + settle / 2);
    assert_eq!(outcome.status_change, None);

    let at = now + settle;
    assert!(reducer.wants_line_probe(at));
    let outcome = reducer.reduce(line("  Proceed? [y/N]"), at);
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Question))
    );
    let detail = outcome.needs_input.expect("detail");
    assert_eq!(detail.source, NeedsInputSource::TerminalLine);
    assert_eq!(detail.summary, "Proceed? [y/N]");
    assert_eq!(detail.prompt_excerpt.as_deref(), Some("Proceed? [y/N]"));
    assert!(!detail.secret);
    assert_eq!(open_requests(&reducer), 1);

    // The same question sampled again, and the job still in the
    // foreground, change nothing.
    let later = at + Duration::from_millis(100);
    assert!(reducer.wants_line_probe(later));
    assert_eq!(
        reducer.reduce(line("Proceed? [y/N]"), later),
        ReducerOutcome::default()
    );
    let outcome = reducer.reduce(StatusSignal::ForegroundJob { running: true }, later);
    assert_eq!(outcome.status_change, None);

    // A keystroke is the user answering: the mark goes at once.
    let typed = later + Duration::from_millis(100);
    let outcome = reducer.reduce(StatusSignal::UserKeystroke, typed);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert_eq!(open_requests(&reducer), 1, "not answered until Enter");

    // They stop half way: marked again, still one request.
    let paused = typed + settle;
    let outcome = reducer.reduce(line("Proceed? [y/N] y"), paused);
    assert!(matches!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(_))
    ));
    assert_eq!(requests(&reducer), 1);

    let outcome = reducer.reduce(StatusSignal::UserSubmission, paused);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert_eq!(open_requests(&reducer), 0);
    assert!(!outcome.turn_completed);
}

/// A password prompt is flagged with no terminal text at all.
#[test]
fn a_secret_line_prompt_carries_no_text() {
    let now = t0() + Duration::from_secs(1);
    let mut reducer = shell_running_a_job(now);
    let at = now + ReducerTiming::default().line_prompt_settle;
    let outcome = reducer.reduce(
        StatusSignal::TerminalLine(Some(TerminalPrompt {
            // Whatever the screen says, echo is off.
            line: Some("Password: hunter2".into()),
            secret: true,
        })),
        at,
    );
    let detail = outcome.needs_input.expect("detail");
    assert_eq!(detail.summary, "Waiting for a password");
    assert_eq!(detail.prompt_excerpt, None);
    assert!(detail.secret);
    assert_eq!(detail.risk_hint, RiskHint::Neutral);
}

/// The question ends without a keystroke here: the job read something
/// else, timed out, or left. Either way the request is settled.
#[test]
fn a_line_prompt_ends_when_the_job_stops_reading_or_leaves() {
    let now = t0() + Duration::from_secs(1);
    let settle = ReducerTiming::default().line_prompt_settle;

    let mut reducer = shell_running_a_job(now);
    reducer.reduce(line("Continue?"), now + settle);
    let outcome = reducer.reduce(StatusSignal::TerminalLine(None), now + settle * 2);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert!(outcome.line_prompt_ended);
    assert_eq!(open_requests(&reducer), 0);
    // A job that is not waiting and never was: nothing to end.
    let outcome = reducer.reduce(StatusSignal::TerminalLine(None), now + settle * 3);
    assert_eq!(outcome, ReducerOutcome::default());

    let mut reducer = shell_running_a_job(now);
    reducer.reduce(line("Continue?"), now + settle);
    let outcome = reducer.reduce(
        StatusSignal::ForegroundJob { running: false },
        now + settle * 2,
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert_eq!(open_requests(&reducer), 0);
    assert!(!reducer.wants_line_probe(now + settle * 3));
}

/// Only a shell's own job is read this way: not the shell at its prompt,
/// not an Agent lent the reducer, not an Agent session.
#[test]
fn only_a_shells_own_job_is_asked_about_its_line() {
    let now = t0() + Duration::from_secs(1);
    let settle = ReducerTiming::default().line_prompt_settle;

    let mut idle =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("shell", Some("1"));
    idle.reduce(StatusSignal::PtyOutputActivity, now);
    assert!(!idle.wants_line_probe(now + settle));
    assert_eq!(idle.reduce(line("$"), now + settle).status_change, None);

    let mut lent = shell_running_a_job(now);
    lent.lend_to_agent("claude-code", Some("7"), now);
    assert!(!lent.wants_line_probe(now + settle));
    assert_eq!(lent.reduce(line("> "), now + settle).status_change, None);

    let mut agent =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("aider", Some("1"));
    agent.reduce(StatusSignal::PtyOutputActivity, now);
    assert!(!agent.wants_line_probe(now + settle));
    assert_eq!(agent.reduce(line("> "), now + settle).status_change, None);
}

#[test]
fn foreground_job_running_is_the_child_process_group_test() {
    assert_eq!(super::foreground_job_running(0, Some(12)), None);
    assert_eq!(super::foreground_job_running(42, None), None);
    assert_eq!(super::foreground_job_running(42, Some(42)), Some(false));
    assert_eq!(super::foreground_job_running(42, Some(99)), Some(true));
}

#[test]
fn a_signalled_exit_is_reported_as_signalled() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let outcome = reducer.reduce(
        StatusSignal::ProcessExit {
            code: None,
            signal: Some(9),
            interrupted: false,
        },
        t0(),
    );
    match outcome.status_change {
        Some(SessionStatus::Exited(info)) => {
            assert_eq!(info.reason, ExitReason::Signaled);
            assert_eq!(info.signal, Some(9));
        }
        other => panic!("expected a signalled exit, got {other:?}"),
    }
}

#[test]
fn an_interrupted_exit_carries_the_flag_to_the_status() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let outcome = reducer.reduce(
        StatusSignal::ProcessExit {
            code: None,
            signal: None,
            interrupted: true,
        },
        t0(),
    );
    match outcome.status_change {
        Some(SessionStatus::Exited(info)) => {
            assert!(info.interrupted);
            assert!(info.ended_by_interruption());
        }
        other => panic!("expected an interrupted exit, got {other:?}"),
    }
}

#[test]
fn exited_is_absorbing() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    reducer.reduce(
        StatusSignal::ProcessExit {
            code: Some(0),
            signal: None,
            interrupted: false,
        },
        t0(),
    );

    let outcome = reducer.reduce(
        hook(ClaudeHook::UserPromptSubmit),
        t0() + Duration::from_secs(1),
    );
    assert_eq!(
        outcome.status_change, None,
        "nothing revives a dead session"
    );
    assert!(matches!(*reducer.status(), SessionStatus::Exited(_)));
}

#[test]
fn a_long_silence_while_working_becomes_unknown_rather_than_a_lie() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);

    let outcome = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(61));
    assert_eq!(outcome.status_change, Some(SessionStatus::Unknown));
}

#[test]
fn pty_output_keeps_a_working_session_from_going_stale() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let mut now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);

    // Output every 30s keeps refreshing recency, so the 60s staleness timeout
    // never fires.
    for _ in 0..4 {
        now += Duration::from_secs(30);
        reducer.reduce(StatusSignal::PtyOutputActivity, now);
        let outcome = reducer.reduce(StatusSignal::Tick, now);
        assert_eq!(outcome.status_change, None);
    }
    assert_eq!(*reducer.status(), SessionStatus::Working);
}

#[test]
fn a_repeated_screen_sequence_is_not_reprocessed() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let now = settled(&mut reducer, t0());

    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 7)),
        now,
    );
    // Same content_seq: the frame is unchanged, so it must not count as another
    // observation.
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 7)),
        now + Duration::from_millis(100),
    );
    assert_eq!(outcome.status_change, None);
    assert_eq!(*reducer.status(), SessionStatus::Working);
}

#[test]
fn cursor_transcript_idle_commits_even_when_the_osc_still_says_working() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let mut now = settled(&mut reducer, t0());

    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    now += Duration::from_millis(100);
    reducer.reduce(StatusSignal::CursorTranscriptIdle, now);
    now += Duration::from_millis(100);
    let still_working = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 2)),
        now,
    );
    assert_eq!(still_working.status_change, None);
    assert_eq!(*reducer.status(), SessionStatus::Working);

    now += Duration::from_millis(100);
    let outcome = reducer.reduce(StatusSignal::Tick, now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);

    now += Duration::from_millis(100);
    let stale_osc = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 3)),
        now,
    );
    assert_eq!(stale_osc.status_change, None);
    assert_eq!(*reducer.status(), SessionStatus::Idle);

    now += Duration::from_millis(100);
    reducer.reduce(StatusSignal::CursorTranscriptWorking, now);
    assert_eq!(*reducer.status(), SessionStatus::Working);
}

#[test]
fn idle_reminder_does_not_turn_a_finished_agent_into_a_question() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    reducer.reduce(hook(ClaudeHook::Stop), now + Duration::from_millis(10));
    reducer.reduce(StatusSignal::Tick, now + Duration::from_millis(100));
    reducer.reduce(
        hook(ClaudeHook::Notification {
            notification_type: Some("idle_prompt".into()),
            message: Some("Claude is waiting for your input".into()),
        }),
        now + Duration::from_secs(60),
    );
    assert_eq!(*reducer.status(), SessionStatus::Idle);
}

#[test]
fn stop_resolves_a_stale_permission_and_completes_the_turn() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    reducer.reduce(
        hook(ClaudeHook::PermissionRequest {
            tool_name: None,
            input_summary: None,
        }),
        now,
    );
    let stopped = reducer.reduce(hook(ClaudeHook::Stop), now + Duration::from_secs(1));
    let tick = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(2));
    assert_eq!(*reducer.status(), SessionStatus::Idle);
    assert!(stopped.turn_completed || tick.turn_completed);
}

#[test]
fn redraw_after_codex_completion_does_not_invent_work() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    reducer.reduce(StatusSignal::CodexTurnComplete, now);
    reducer.reduce(StatusSignal::Tick, now + Duration::from_millis(100));
    reducer.reduce(
        StatusSignal::PtyOutputActivity,
        now + Duration::from_secs(6),
    );
    assert_eq!(*reducer.status(), SessionStatus::Idle);
}

#[test]
fn a_completed_claude_turn_is_not_reopened_by_a_stale_working_title() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    reducer.reduce(hook(ClaudeHook::Stop), now);
    reducer.reduce(StatusSignal::Tick, now + Duration::from_millis(100));
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 2)),
        now + Duration::from_secs(1),
    );
    assert_eq!(*reducer.status(), SessionStatus::Idle);
    reducer.reduce(
        hook(ClaudeHook::UserPromptSubmit),
        now + Duration::from_secs(2),
    );
    assert_eq!(*reducer.status(), SessionStatus::Working);
}

#[test]
fn a_quiet_screen_completes_once_without_waiting_for_more_redraws() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        t0(),
    );
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 2)),
        t0() + Duration::from_secs(1),
    );
    assert_eq!(reducer.status(), &SessionStatus::Working);
    let completed = reducer.reduce(StatusSignal::Tick, t0() + Duration::from_secs(2));
    assert!(completed.turn_completed);
    assert_eq!(reducer.status(), &SessionStatus::Idle);
    assert!(
        !reducer
            .reduce(
                StatusSignal::CodexTurnComplete,
                t0() + Duration::from_secs(3)
            )
            .turn_completed
    );
}

#[test]
fn a_recent_work_hook_outranks_an_idle_prompt_during_a_tool_call() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), t0());
    for seq in 1..5 {
        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, seq)),
            t0() + Duration::from_millis(seq * 200),
        );
    }
    assert_eq!(reducer.status(), &SessionStatus::Working);
    assert!(
        !reducer
            .reduce(StatusSignal::Tick, t0() + Duration::from_secs(2))
            .turn_completed
    );
    assert!(
        reducer
            .reduce(hook(ClaudeHook::Stop), t0() + Duration::from_secs(3))
            .turn_completed
    );
}

#[test]
fn claude_long_tool_calls_do_not_publish_finished_between_tools() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let mut now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    let mut completions = 0;
    for seq in 1..=3 {
        reducer.reduce(hook(ClaudeHook::PreToolUse), now);
        // Claude's input box can remain visible throughout a long tool call.
        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, seq)),
            now + Duration::from_millis(100),
        );
        for seconds in 1..=30 {
            let outcome = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(seconds));
            completions += usize::from(outcome.turn_completed);
        }
        now += Duration::from_secs(30);
    }
    assert_eq!(
        completions, 0,
        "active Claude tools must not emit finished notifications"
    );
    assert_eq!(reducer.status(), &SessionStatus::Working);
    assert!(reducer.reduce(hook(ClaudeHook::Stop), now).turn_completed);
    assert!(!reducer.reduce(hook(ClaudeHook::Stop), now).turn_completed);
}

#[test]
fn claude_idle_redraws_and_subagent_completion_do_not_finish_the_parent_turn() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    reducer.reduce(hook(ClaudeHook::SubagentStart("child".into())), now);
    for seq in 1..=120 {
        let at = now + Duration::from_secs(seq);
        if seq == 30 {
            reducer.reduce(hook(ClaudeHook::SubagentStop("child".into())), at);
        }
        let frame = reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, seq)),
            at,
        );
        let tick = reducer.reduce(StatusSignal::Tick, at);
        assert!(!frame.turn_completed && !tick.turn_completed);
        assert_eq!(reducer.status(), &SessionStatus::Working);
    }
    assert!(
        reducer
            .reduce(hook(ClaudeHook::Stop), now + Duration::from_secs(121))
            .turn_completed
    );
}

#[test]
fn claude_screen_fallback_still_completes_when_no_work_hook_was_received() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    // Remote/adopted sessions may only have terminal observations.
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Working, 1)),
        now,
    );
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 2)),
        now,
    );
    let completed = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(1));
    assert!(completed.turn_completed);
    assert_eq!(reducer.status(), &SessionStatus::Idle);
    assert!(
        !reducer
            .reduce(StatusSignal::Tick, now + Duration::from_secs(2))
            .turn_completed
    );
}

#[test]
fn claude_answering_a_screen_blocker_resumes_the_hook_owned_turn() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(hook(ClaudeHook::UserPromptSubmit), now);
    reducer.reduce(StatusSignal::Screen(blocker(1, "Allow tool?")), now);
    for seq in 2..=4 {
        let outcome = reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, seq)),
            now + Duration::from_secs(seq * 10),
        );
        assert!(!outcome.turn_completed);
    }
    assert_eq!(reducer.status(), &SessionStatus::Working);
    assert!(
        reducer
            .reduce(hook(ClaudeHook::Stop), now + Duration::from_secs(41))
            .turn_completed
    );
}

#[test]
fn unavailable_transport_is_not_a_process_exit_and_cannot_complete_a_turn() {
    let mut reducer = StatusReducer::new(Authority::ProcessOnly, t0());
    reducer.reduce(StatusSignal::PtyOutputActivity, t0());
    let outcome = reducer.reduce(StatusSignal::TransportUnavailable, t0());
    assert_eq!(outcome.status_change, Some(SessionStatus::Unknown));
    assert!(!outcome.turn_completed);
    assert_eq!(
        outcome.status_evidence.unwrap().fallback_reason,
        Some(StatusFallbackReason::TransportUnavailable)
    );
    for signal in [
        StatusSignal::Tick,
        StatusSignal::PtyOutputActivity,
        StatusSignal::UserSubmission,
    ] {
        reducer.reduce(signal, t0() + Duration::from_secs(120));
        assert_eq!(*reducer.status(), SessionStatus::Unknown);
    }
    let actual = reducer.reduce(
        StatusSignal::ProcessExit {
            code: Some(126),
            signal: None,
            interrupted: false,
        },
        t0(),
    );
    assert!(
        matches!(actual.status_change, Some(SessionStatus::Exited(info)) if info.code == Some(126))
    );
}

#[test]
fn answered_cursor_trust_can_settle_on_one_unchanged_idle_frame() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(
        StatusSignal::Screen(blocker(1, "Workspace Trust Required")),
        now,
    );
    reducer.reduce(StatusSignal::UserKeystroke, now + Duration::from_millis(10));
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 2)),
        now + Duration::from_millis(20),
    );
    reducer.reduce(StatusSignal::Tick, now + Duration::from_millis(100));
    assert!(matches!(reducer.status(), SessionStatus::NeedsInput(_)));
    reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(1));
    assert_eq!(reducer.status(), &SessionStatus::Idle);
}

#[test]
fn a_reappearing_blocker_cancels_the_quiet_idle_frame() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let now = settled(&mut reducer, t0());
    reducer.reduce(
        StatusSignal::Screen(blocker(1, "Workspace Trust Required")),
        now,
    );
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::Idle, 2)),
        now + Duration::from_millis(20),
    );
    reducer.reduce(
        StatusSignal::Screen(blocker(3, "Workspace Trust Required")),
        now + Duration::from_millis(40),
    );
    reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(1));
    assert!(matches!(reducer.status(), SessionStatus::NeedsInput(_)));
}

#[test]
fn cursor_transcript_cannot_finish_a_live_spinner() {
    let mut reducer = StatusReducer::new(Authority::ScreenPrimary, t0());
    let now = settled(&mut reducer, t0());
    let mut working = observation(ManifestState::Working, 1);
    working.matched_rule_id = "working-status-line".into();
    reducer.reduce(StatusSignal::Screen(working), now);
    reducer.reduce(
        StatusSignal::CursorTranscriptIdle,
        now + Duration::from_millis(100),
    );
    let outcome = reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(1));
    assert_eq!(reducer.status(), &SessionStatus::Working);
    assert!(!outcome.turn_completed);
}

fn program(state: ProgramState) -> StatusSignal {
    StatusSignal::ProgramStatus(Some(ProgramRecord {
        state,
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
    }))
}

/// `OSC 7501` is the program saying what it is doing. While it reports work,
/// a screen that happens to read idle (a spinner between frames, a finished
/// sub-step) does not end the turn; its own `done` does, exactly once.
#[test]
fn a_program_reporting_work_outranks_the_screen_until_it_reports_done() {
    let mut reducer =
        StatusReducer::new(Authority::ScreenPrimary, t0()).with_manifest("codex", Some("1"));
    let now = settled(&mut reducer, t0());

    let outcome = reducer.reduce(program(ProgramState::Working), now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert_eq!(
        outcome.status_evidence.map(|evidence| evidence.source),
        Some(StatusEvidenceSource::ProgramStatus)
    );

    for seq in 1..=5 {
        let at = now + Duration::from_secs(seq);
        reducer.reduce(
            StatusSignal::Screen(observation(ManifestState::Idle, seq)),
            at,
        );
        reducer.reduce(StatusSignal::Tick, at);
    }
    // Quiet for longer than staleness allows, and still working.
    reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(600));
    assert_eq!(*reducer.status(), SessionStatus::Working);

    let outcome = reducer.reduce(program(ProgramState::Done), now + Duration::from_secs(601));
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);
    let again = reducer.reduce(program(ProgramState::Idle), now + Duration::from_secs(602));
    assert!(!again.turn_completed);
    // A stale prompt still on screen does not outvote the program's result.
    reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::BlockedQuestion, 98)),
        now + Duration::from_secs(602),
    );
    assert_eq!(*reducer.status(), SessionStatus::Idle);

    // Once the program stops reporting, the screen is read again.
    reducer.reduce(
        StatusSignal::ProgramStatus(None),
        now + Duration::from_secs(603),
    );
    let outcome = reducer.reduce(
        StatusSignal::Screen(observation(ManifestState::BlockedQuestion, 99)),
        now + Duration::from_secs(603),
    );
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Question))
    );
}

#[test]
fn a_blocked_program_needs_input_with_its_own_message() {
    let mut reducer =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("aider", Some("1"));
    reducer.reduce(StatusSignal::PtyOutputActivity, t0());
    let report = StatusSignal::ProgramStatus(Some(ProgramRecord {
        state: ProgramState::Blocked,
        kind: Some(BlockedKind::Permission),
        progress: None,
        app: Some("terraform".into()),
        title: None,
        msg: Some("Apply 3 to add, 1 to change, 0 to destroy?".into()),
    }));
    let outcome = reducer.reduce(report.clone(), t0());
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Permission))
    );
    let detail = outcome.needs_input.expect("a needs-input detail");
    assert_eq!(detail.source, NeedsInputSource::ProgramStatus);
    assert_eq!(detail.summary, "Apply 3 to add, 1 to change, 0 to destroy?");
    assert_eq!(detail.tool_name.as_deref(), Some("terraform"));

    // The same report again is not a second question.
    assert_eq!(reducer.reduce(report, t0()).needs_input, None);

    // A process-only agent that stops reporting is running, not idle.
    let outcome = reducer.reduce(StatusSignal::ProgramStatus(None), t0());
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert!(!outcome.turn_completed);
}

#[test]
fn a_shell_follows_a_reporting_job_and_returns_to_idle_when_it_stops() {
    let mut reducer =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("shell", Some("1"));
    let now = t0() + Duration::from_secs(1);
    reducer.reduce(StatusSignal::PtyOutputActivity, now);
    reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);

    let outcome = reducer.reduce(
        StatusSignal::ProgramStatus(Some(ProgramRecord {
            state: ProgramState::Blocked,
            kind: Some(BlockedKind::Auth),
            progress: None,
            app: None,
            title: None,
            msg: None,
        })),
        now,
    );
    assert_eq!(outcome.needs_input.unwrap().summary, "Waiting for sign-in");
    // The job is still in the foreground and not reading a line, which on
    // its own would read as Working.
    reducer.reduce(StatusSignal::TerminalLine(None), now);
    reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);
    assert_eq!(
        *reducer.status(),
        SessionStatus::NeedsInput(NeedsInputKind::Question)
    );

    // The job ended without a result: the Engine drops its records.
    let outcome = reducer.reduce(StatusSignal::ProgramStatus(None), now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(!outcome.turn_completed);

    // Inference is back.
    let outcome = reducer.reduce(StatusSignal::ForegroundJob { running: true }, now);
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
}

#[test]
fn a_program_error_is_a_result_that_ends_the_turn() {
    let mut reducer =
        StatusReducer::new(Authority::ProcessOnly, t0()).with_manifest("aider", Some("1"));
    reducer.reduce(StatusSignal::PtyOutputActivity, t0());
    reducer.reduce(program(ProgramState::Working), t0());
    let outcome = reducer.reduce(program(ProgramState::Error), t0() + Duration::from_secs(1));
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);
    // Still running and no longer reporting: process-only reads as working.
    let outcome = reducer.reduce(
        StatusSignal::ProgramStatus(None),
        t0() + Duration::from_secs(2),
    );
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
    assert!(!outcome.turn_completed);
}

/// Claude Code and Codex are adding `OSC 7501` while keeping their hooks and
/// notify events. Two sources racing would flip a tab between working and
/// idle; the program's report is the one it keeps current, so it decides.
#[test]
fn hooks_do_not_race_a_program_that_reports_its_status() {
    let mut reducer =
        StatusReducer::new(Authority::HooksPrimary, t0()).with_manifest("claude-code", Some("4"));
    let now = settled(&mut reducer, t0());
    reducer.reduce(program(ProgramState::Working), now);

    // A Stop for the foreground response while background work still runs.
    let outcome = reducer.reduce(hook(ClaudeHook::Stop), now + Duration::from_millis(10));
    assert_eq!(outcome.status_change, None);
    assert!(!outcome.turn_completed);
    reducer.reduce(StatusSignal::Tick, now + Duration::from_secs(5));
    assert_eq!(*reducer.status(), SessionStatus::Working);

    let outcome = reducer.reduce(program(ProgramState::Done), now + Duration::from_secs(6));
    assert_eq!(outcome.status_change, Some(SessionStatus::Idle));
    assert!(outcome.turn_completed);

    // A late tool hook from the finished turn does not reopen it.
    let outcome = reducer.reduce(hook(ClaudeHook::PostToolUse), now + Duration::from_secs(7));
    assert_eq!(outcome.status_change, None);
    let outcome = reducer.reduce(
        StatusSignal::CodexTurnComplete,
        now + Duration::from_secs(7),
    );
    assert_eq!(outcome.status_change, None);
    assert_eq!(*reducer.status(), SessionStatus::Idle);
}

/// A permission hook names the tool and command; a report that follows says
/// only that the program is blocked. The richer detail stays.
#[test]
fn a_blocking_hook_keeps_its_detail_under_the_programs_report() {
    let mut reducer =
        StatusReducer::new(Authority::HooksPrimary, t0()).with_manifest("claude-code", Some("4"));
    let now = settled(&mut reducer, t0());
    reducer.reduce(program(ProgramState::Working), now);

    let outcome = reducer.reduce(
        hook(ClaudeHook::PermissionRequest {
            tool_name: Some("Bash".into()),
            input_summary: Some("rm -rf build".into()),
        }),
        now + Duration::from_millis(5),
    );
    assert_eq!(
        outcome.status_change,
        Some(SessionStatus::NeedsInput(NeedsInputKind::Permission))
    );

    let blocked = StatusSignal::ProgramStatus(Some(ProgramRecord {
        state: ProgramState::Blocked,
        kind: Some(BlockedKind::Permission),
        progress: None,
        app: Some("claude-code".into()),
        title: None,
        msg: Some("Allow Bash?".into()),
    }));
    let outcome = reducer.reduce(blocked, now + Duration::from_millis(10));
    assert_eq!(outcome.status_change, None);
    assert_eq!(outcome.needs_input, None, "not a second request");

    // Approved: the program says it is working again.
    let outcome = reducer.reduce(program(ProgramState::Working), now + Duration::from_secs(1));
    assert_eq!(outcome.status_change, Some(SessionStatus::Working));
}

/// A report says the program waits; the screen says what it offers. The
/// choices reach the request whichever arrives first.
#[test]
fn a_blocked_report_borrows_the_choices_on_screen() {
    let choices = vec!["Yes".to_owned(), "No".to_owned()];
    let blocked = || {
        StatusSignal::ProgramStatus(Some(ProgramRecord {
            state: ProgramState::Blocked,
            kind: Some(BlockedKind::Permission),
            progress: None,
            app: None,
            title: None,
            msg: Some("Run the migration?".into()),
        }))
    };
    let dialog = |seq| {
        let mut shown = observation(ManifestState::BlockedPermission, seq);
        shown.options = Some(vec!["Yes".to_owned(), "No".to_owned()]);
        StatusSignal::Screen(shown)
    };

    // The dialog renders first.
    let mut reducer =
        StatusReducer::new(Authority::ScreenPrimary, t0()).with_manifest("codex", Some("1"));
    let now = settled(&mut reducer, t0());
    reducer.reduce(program(ProgramState::Working), now);
    assert_eq!(reducer.reduce(dialog(1), now).status_change, None);
    let outcome = reducer.reduce(blocked(), now);
    let detail = outcome.needs_input.expect("a request");
    assert_eq!(detail.summary, "Run the migration?");
    assert_eq!(detail.options.as_ref(), Some(&choices));

    // The report arrives first.
    let mut reducer =
        StatusReducer::new(Authority::ScreenPrimary, t0()).with_manifest("codex", Some("1"));
    let now = settled(&mut reducer, t0());
    reducer.reduce(program(ProgramState::Working), now);
    assert_eq!(
        reducer.reduce(blocked(), now).needs_input.unwrap().options,
        None
    );
    let outcome = reducer.reduce(dialog(2), now);
    assert_eq!(outcome.status_change, None);
    assert_eq!(
        outcome.needs_input.unwrap().options.as_ref(),
        Some(&choices)
    );
    // Once only.
    assert_eq!(reducer.reduce(dialog(3), now).needs_input, None);
}
