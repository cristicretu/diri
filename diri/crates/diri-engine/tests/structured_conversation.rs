//! Structured conversations driven through real pipes. The Agent is a
//! scripted shell stand-in that replays lines recorded from the installed
//! CLIs (diri-proto fixtures), so these run without an account or network.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diri_engine::structured::{
    SequencedEvent, StructuredConversation, StructuredError, StructuredHarness,
};
use diri_proto::structured::StructuredEvent;

const CLAUDE_TEXT: &str =
    include_str!("../../diri-proto/tests/fixtures/structured/claude-text-stream.jsonl");
const CLAUDE_TOOL: &str =
    include_str!("../../diri-proto/tests/fixtures/structured/claude-tool-use.jsonl");
const CODEX_TURN: &str =
    include_str!("../../diri-proto/tests/fixtures/structured/codex-turn.jsonl");

/// Writes an executable shell script into a fresh directory.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// A fake `claude` that replays `fixture` once per turn, then waits for
/// stdin. Its arguments are ignored; the Engine's argv is tested separately.
fn fake_claude(dir: &Path, fixture: &str) -> PathBuf {
    let replay = dir.join("claude-replay.jsonl");
    fs::write(&replay, fixture).unwrap();
    script(
        dir,
        "claude",
        &format!("cat '{}'\ncat > /dev/null", replay.display()),
    )
}

/// A fake `codex app-server`: answers the handshake by request id and
/// replays the recorded turn when `turn/start` arrives.
fn fake_codex(dir: &Path) -> PathBuf {
    let notifications = dir.join("codex-notifications.jsonl");
    let lines: String = CODEX_TURN
        .lines()
        .filter(|line| line.contains("\"method\""))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&notifications, lines).unwrap();
    script(
        dir,
        "codex",
        &format!(
            r#"while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{{"id":1,"result":{{}}}}' ;;
    *'"method":"thread/start"'*) printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-fake"}}}}}}' ;;
    *'"method":"turn/start"'*) printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-fake"}}}}}}'; cat '{}' ;;
  esac
done"#,
            notifications.display()
        ),
    )
}

/// Polls until the turn completes (or the Agent errors), collecting events.
fn run_turn(conversation: &mut StructuredConversation, text: &str) -> Vec<SequencedEvent> {
    conversation.send_turn(text).expect("turn sends");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        let batch = conversation.poll();
        let finished = batch.iter().any(|item| {
            matches!(
                item.event,
                StructuredEvent::TurnCompleted { .. } | StructuredEvent::Error { .. }
            )
        });
        events.extend(batch);
        if finished {
            return events;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("turn did not finish; events so far: {events:?}");
}

fn text_of(events: &[SequencedEvent]) -> String {
    events
        .iter()
        .filter_map(|item| match &item.event {
            StructuredEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn nothing_spawns_before_the_first_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path())
        .with_program(fake_claude(dir.path(), CLAUDE_TEXT));
    conversation.set_model(Some("claude-opus-5-5".to_owned()), Some("high".to_owned()));
    conversation.set_model(Some("claude-sonnet-5-5".to_owned()), None);
    conversation.set_model(Some("claude-haiku-5-5".to_owned()), None);

    assert_eq!(
        conversation.spawn_count(),
        0,
        "picking models is not a process"
    );
    assert_eq!(conversation.model(), Some("claude-haiku-5-5"));
    assert!(conversation.poll().is_empty());
}

#[test]
fn claude_turn_streams_text_with_monotonic_sequence_and_a_preset_uuid() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path())
        .with_program(fake_claude(dir.path(), CLAUDE_TEXT));
    let events = run_turn(&mut conversation, "Reply with exactly: pong");

    assert_eq!(text_of(&events), "pong");
    let sequences: Vec<u64> = events.iter().map(|item| item.sequence).collect();
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "{sequences:?}"
    );
    assert_eq!(sequences.first(), Some(&1));
    assert_eq!(conversation.spawn_count(), 1);

    // Diri chose the UUID before spawn; the child's own init does not replace it.
    let native = conversation.native_id().expect("native id");
    assert_eq!(native.len(), 36);
    assert_ne!(native, "11111111-2222-4333-8444-555555555555");
}

#[test]
fn a_tool_start_repeated_by_the_agent_is_delivered_once() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = format!("{CLAUDE_TOOL}{CLAUDE_TOOL}");
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path())
        .with_program(fake_claude(dir.path(), &fixture));
    // The fixture completes the turn twice: keep polling past the first
    // completion so the repeated tool_use is actually read.
    let mut events = run_turn(&mut conversation, "run it");
    std::thread::sleep(Duration::from_millis(300));
    events.extend(conversation.poll());
    let starts = events
        .iter()
        .filter(|item| matches!(item.event, StructuredEvent::ToolStarted { .. }))
        .count();
    assert_eq!(starts, 1);
    assert!(events.iter().any(|item| matches!(
        item.event,
        StructuredEvent::ToolFinished {
            is_error: false,
            ..
        }
    )));
}

#[test]
fn model_change_mid_conversation_restarts_claude_on_the_same_uuid() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path())
        .with_program(fake_claude(dir.path(), CLAUDE_TEXT));
    run_turn(&mut conversation, "one");
    let native = conversation.native_id().unwrap().to_owned();

    conversation.set_model(Some("claude-opus-5-5".to_owned()), None);
    run_turn(&mut conversation, "two");

    assert_eq!(
        conversation.spawn_count(),
        2,
        "one restart for the new model"
    );
    assert_eq!(
        conversation.native_id(),
        Some(native.as_str()),
        "same conversation"
    );
    assert_eq!(conversation.model(), Some("claude-opus-5-5"));
}

#[test]
fn a_startup_report_never_overrides_the_chosen_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path())
        .with_program(fake_claude(dir.path(), CLAUDE_TEXT));
    conversation.set_model(Some("claude-opus-5-5".to_owned()), None);
    run_turn(&mut conversation, "hi");

    // The fixture's init reports haiku; the user's choice stays.
    assert_eq!(conversation.reported_model(), Some("claude-haiku-5-5"));
    assert_eq!(conversation.model(), Some("claude-opus-5-5"));
}

#[test]
fn codex_handshake_then_turn_completes_on_idle_with_model_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Codex, dir.path())
        .with_program(fake_codex(dir.path()));
    conversation.set_model(Some("gpt-6.2-sol".to_owned()), Some("low".to_owned()));
    let events = run_turn(&mut conversation, "Reply with exactly: pong");

    assert_eq!(text_of(&events), "pong");
    assert_eq!(conversation.native_id(), Some("thread-fake"));
    assert!(!conversation.is_turn_in_flight());
    assert_eq!(conversation.spawn_count(), 1);
}

#[test]
fn a_second_turn_on_codex_reuses_the_thread_without_a_new_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Codex, dir.path())
        .with_program(fake_codex(dir.path()));
    run_turn(&mut conversation, "one");
    run_turn(&mut conversation, "two");
    assert_eq!(conversation.spawn_count(), 1);
}

#[test]
fn antigravity_fails_closed_without_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Antigravity, dir.path());
    match conversation.send_turn("hi") {
        Err(StructuredError::Unsupported { harness, reason }) => {
            assert_eq!(harness, StructuredHarness::Antigravity);
            assert!(reason.contains("agy acp"));
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert_eq!(conversation.spawn_count(), 0);
}

#[test]
fn an_agent_that_exits_mid_turn_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let program = script(dir.path(), "claude-dies", "exit 0");
    let mut conversation =
        StructuredConversation::new(StructuredHarness::Claude, dir.path()).with_program(program);
    // The child may exit before the write lands; either outcome must surface
    // as an error, never as a hung turn.
    let _ = conversation.send_turn("hi");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while Instant::now() < deadline
        && !events
            .iter()
            .any(|item: &SequencedEvent| matches!(item.event, StructuredEvent::Error { .. }))
    {
        events.extend(conversation.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        events
            .iter()
            .any(|item| matches!(item.event, StructuredEvent::Error { .. })),
        "{events:?}"
    );
    assert!(!conversation.is_turn_in_flight());
}

#[test]
fn switching_harness_keeps_the_conversation_and_drops_the_native_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path())
        .with_program(fake_claude(dir.path(), CLAUDE_TEXT));
    run_turn(&mut conversation, "one");
    conversation.switch_harness(StructuredHarness::Codex);
    assert_eq!(conversation.harness(), StructuredHarness::Codex);
    assert_eq!(conversation.native_id(), None);
}

/// Opt-in: drives the installed `claude` for one turn. Costs a few tokens.
/// `DIRI_REAL_CLAUDE=1 cargo test -p diri-engine --test structured_conversation -- --ignored`
#[test]
#[ignore]
fn real_claude_one_turn() {
    if std::env::var_os("DIRI_REAL_CLAUDE").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = StructuredConversation::new(StructuredHarness::Claude, dir.path());
    conversation.set_model(Some("claude-haiku-5-5".to_owned()), None);
    let events = run_turn(&mut conversation, "Reply with exactly: pong");
    assert_eq!(text_of(&events).trim(), "pong");
}
