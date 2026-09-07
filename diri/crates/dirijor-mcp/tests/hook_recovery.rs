#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

use diri_proto::recovery::{HookActivitySeed, SessionRecoveryStore};

fn run_hook(directory: &std::path::Path, event: &str, payload: serde_json::Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["hook", event])
        .env("DIRIJOR_SESSION_ID", "s_hook")
        .env("DIRIJOR_SESSION_RECOVERY_DIR", directory)
        .env("DIRIJOR_SOCKET", directory.join("missing.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
}

#[test]
fn claude_background_work_survives_recovery_and_child_callbacks() {
    use diri_engine::hooks::{parse_activity_seed, parse_claude_hook};
    use diri_engine::status::{Authority, StatusReducer, StatusSignal};
    use diri_proto::SessionStatus;
    use serde_json::json;

    let directory = tempfile::tempdir().unwrap();
    let store = SessionRecoveryStore::new(directory.path());
    run_hook(
        directory.path(),
        "Stop",
        json!({
            "session_id": "parent", "hook_event_name": "Stop",
            "background_tasks": [{"status": "running", "prompt": "secret task"}],
            "session_crons": [{"prompt": "secret schedule"}],
        }),
    );
    let original = store.read_activity().unwrap().unwrap();
    assert_eq!(original.claude_pending_work, Some(true));
    assert!(
        !std::fs::read_to_string(directory.path().join("last-activity.json"))
            .unwrap()
            .contains("secret")
    );
    for (event, payload) in [
        ("Stop", json!({"session_id": "parent", "agent_id": "child"})),
        ("SubagentStop", json!({"session_id": "parent"})),
        (
            "Stop",
            json!({"session_id": "parent", "hook_event_name": "SubagentStop"}),
        ),
    ] {
        run_hook(directory.path(), event, payload);
        assert_eq!(store.read_activity().unwrap().unwrap(), original);
    }
    run_hook(
        directory.path(),
        "PreToolUse",
        json!({"session_id": "parent", "tool_name": "Bash"}),
    );
    let work = store.read_activity().unwrap().unwrap();
    assert_eq!(work.claude_pending_work, Some(true));
    run_hook(
        directory.path(),
        "Notification",
        json!({
            "session_id": "parent", "notification_type": "idle_prompt",
        }),
    );
    let reminder = store.read_activity().unwrap().unwrap();
    assert_eq!(reminder.claude_pending_work, Some(true));
    let bytes = std::fs::read_to_string(directory.path().join("last-activity.json")).unwrap();
    assert!(!bytes.contains("secret"));

    for seed in [original, work, reminder] {
        let now = std::time::SystemTime::now();
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, now);
        let (signal, _) = parse_activity_seed(&seed).unwrap();
        assert!(!reducer.reduce(signal, now).turn_completed);
        assert_eq!(reducer.status(), &SessionStatus::Working);
        let (completed, _) = parse_claude_hook(
            "Notification",
            &json!({"notification_type": "agent_completed"}),
            now,
        )
        .unwrap();
        reducer.reduce(completed, now);
        assert!(!reducer.reduce(StatusSignal::Tick, now).turn_completed);
        let (stop, _) = parse_claude_hook(
            "Stop",
            &json!({"background_tasks": [], "session_crons": []}),
            now,
        )
        .unwrap();
        reducer.reduce(stop, now);
        assert!(reducer.reduce(StatusSignal::Tick, now).turn_completed);
    }
    run_hook(
        directory.path(),
        "Stop",
        json!({"session_id": "parent", "background_tasks": [], "session_crons": []}),
    );
    assert_eq!(
        store.read_activity().unwrap().unwrap().claude_pending_work,
        Some(false)
    );

    run_hook(
        directory.path(),
        "Stop",
        json!({"session_id": "parent", "session_crons": [{}]}),
    );
    run_hook(
        directory.path(),
        "Notification",
        json!({"session_id": "different", "notification_type": "idle_prompt"}),
    );
    assert_eq!(
        store.read_activity().unwrap().unwrap().claude_pending_work,
        None
    );
}

#[test]
fn a_hook_records_its_safe_seed_before_unreachable_daemon_delivery() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let session_dir = directory.path().join("s_hook");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["hook", "PermissionRequest"])
        .env("DIRIJOR_SESSION_ID", "s_hook")
        .env("DIRIJOR_SESSION_RECOVERY_DIR", &session_dir)
        .env("DIRIJOR_SOCKET", directory.path().join("missing.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hook CLI");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(
            br#"{"session_id":"conversation-7","transcript_path":"/tmp/t.jsonl","tool_name":"Bash","tool_input":{"command":"secret command"},"prompt":"secret prompt"}"#,
        )
        .expect("payload");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success(), "hooks fail open: {output:?}");

    let seed = SessionRecoveryStore::new(session_dir)
        .read_activity()
        .expect("read seed")
        .expect("seed exists");
    assert_eq!(
        seed,
        HookActivitySeed {
            claude_pending_work: None,
            version: HookActivitySeed::VERSION,
            kind: "claude-hook".into(),
            event: Some("PermissionRequest".into()),
            occurred_at_ms: seed.occurred_at_ms,
            agent_session_id: Some("conversation-7".into()),
            transcript_path: Some("/tmp/t.jsonl".into()),
            notification_type: None,
            tool_name: Some("Bash".into()),
        }
    );
    let bytes =
        std::fs::read(directory.path().join("s_hook/last-activity.json")).expect("activity bytes");
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(!text.contains("secret command"));
    assert!(!text.contains("secret prompt"));
}
