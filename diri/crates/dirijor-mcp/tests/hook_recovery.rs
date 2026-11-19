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
            native_request_id: None,
            native_turn_id: None,
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

/// Answers Hello like the Engine, then returns the one `hook.report` params
/// the CLI delivered.
fn deliver_hook(event: &str, payload: &[u8]) -> (serde_json::Value, HookActivitySeed) {
    deliver(&["hook", event], payload, b"{}\n")
}

fn deliver(args: &[&str], payload: &[u8], stdout: &[u8]) -> (serde_json::Value, HookActivitySeed) {
    deliver_with_protocol(args, payload, stdout, false)
}

fn deliver_with_protocol(
    args: &[&str],
    payload: &[u8],
    stdout: &[u8],
    reserved: bool,
) -> (serde_json::Value, HookActivitySeed) {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let engine = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut delivered = None;
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let result = if request["method"] == "hello" {
                let mut hello = serde_json::json!({"proto": diri_proto::WIRE_VERSION, "engineKind": diri_proto::RUST_ENGINE_KIND});
                if reserved {
                    assert_eq!(request["params"]["reserveHook"]["sessionID"], "s_hook");
                    hello["hookReservation"] = serde_json::json!({"sequence": 73, "response": {"additionalContext": "speak now"}});
                }
                hello
            } else {
                delivered = Some(request["params"].clone());
                if reserved {
                    assert_eq!(request["method"], "hook.commit");
                    assert_eq!(request["id"], 73);
                    // Wait for the CLI to close without replying. A synchronous
                    // client would time out and lose the current-turn context.
                    line.clear();
                    assert_eq!(reader.read_line(&mut line).unwrap(), 0);
                    break;
                }
                assert_eq!(request["method"], "hook.report");
                serde_json::json!({})
            };
            let response = serde_json::json!({"id": request["id"], "ok": result});
            writer
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
        delivered.unwrap()
    });
    let recovery = directory.path().join("s_hook");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(args)
        .env("DIRIJOR_SESSION_ID", "s_hook")
        .env("DIRIJOR_SESSION_RECOVERY_DIR", &recovery)
        .env("DIRIJOR_SOCKET", &socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(payload).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, stdout);
    let seed = SessionRecoveryStore::new(recovery)
        .read_activity()
        .unwrap()
        .unwrap();
    (engine.join().unwrap(), seed)
}

#[test]
fn codex_completion_keeps_the_identity_and_title_without_forwarding_the_answer() {
    let original = serde_json::json!({
        "type": "agent-turn-complete", "thread-id": "thread-1", "turn-id": "turn-4",
        "cwd": "/repo", "input-messages": ["first prompt", "later message"],
        "last-assistant-message": "x".repeat(32768),
    });
    let (report, seed) = deliver(&["notify", &original.to_string()], &[], b"");
    let payload = &report["payload"];
    assert!(payload.get("last-assistant-message").is_none());
    assert_eq!(
        payload["input-messages"],
        serde_json::json!(["first prompt"])
    );
    let (_, expected) = diri_engine::hooks::parse_codex_notify(&original).unwrap();
    let (_, actual) = diri_engine::hooks::parse_codex_notify(payload).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(seed.native_turn_id.as_deref(), Some("turn-4"));
    assert_eq!(seed.agent_session_id.as_deref(), Some("thread-1"));
}

#[test]
fn large_tool_payloads_are_read_whole_but_not_forwarded() {
    // A Read of a >1 MiB file: identity must survive and the file contents
    // must not be re-sent to the Engine on every PostToolUse.
    let contents = "x".repeat(3 << 20);
    let payload = serde_json::json!({
        "session_id": "conversation-9", "hook_event_name": "PostToolUse",
        "tool_name": "Read", "tool_use_id": "toolu_1", "transcript_path": "/tmp/t.jsonl",
        "tool_input": {"file_path": "/tmp/big.txt"},
        "tool_response": {"file": {"content": contents}},
    });
    let (delivered, seed) = deliver_hook("PostToolUse", payload.to_string().as_bytes());
    assert_eq!(delivered["event"], "PostToolUse");
    assert_eq!(delivered["dirijorSessionID"], "s_hook");
    assert_eq!(delivered["payload"]["session_id"], "conversation-9");
    assert_eq!(delivered["payload"]["tool_use_id"], "toolu_1");
    assert_eq!(delivered["payload"]["transcript_path"], "/tmp/t.jsonl");
    assert!(delivered["payload"].get("tool_response").is_none());
    assert!(delivered["payload"].get("tool_input").is_none());
    assert!(delivered.to_string().len() < 4096);
    assert_eq!(seed.agent_session_id.as_deref(), Some("conversation-9"));
    assert_eq!(seed.native_request_id.as_deref(), Some("toolu_1"));

    // A permission request keeps the input the Engine summarizes.
    let payload = serde_json::json!({
        "session_id": "conversation-9", "hook_event_name": "PermissionRequest",
        "tool_name": "Bash", "tool_use_id": "toolu_2",
        "tool_input": {"command": "cargo test"},
    });
    let (delivered, _) = deliver_hook("PermissionRequest", payload.to_string().as_bytes());
    assert_eq!(delivered["payload"]["tool_input"]["command"], "cargo test");
}

#[test]
fn reserved_submit_exits_with_current_context_before_any_commit_reply() {
    let (report, seed) = deliver_with_protocol(
        &["hook", "UserPromptSubmit"],
        br#"{"session_id":"conversation","prompt":"test"}"#,
        b"{\"hookSpecificOutput\":{\"additionalContext\":\"speak now\",\"hookEventName\":\"UserPromptSubmit\"}}\n",
        true,
    );
    assert_eq!(report["event"], "UserPromptSubmit");
    assert_eq!(seed.agent_session_id.as_deref(), Some("conversation"));
}
