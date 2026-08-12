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
            notification_type: Some("idle_prompt".into()),
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
fn a_signalled_exit_is_reported_as_signalled() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    let outcome = reducer.reduce(
        StatusSignal::ProcessExit {
            code: None,
            signal: Some(9),
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
fn exited_is_absorbing() {
    let mut reducer = StatusReducer::new(Authority::HooksPrimary, t0());
    reducer.reduce(
        StatusSignal::ProcessExit {
            code: Some(0),
            signal: None,
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
