#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

use diri_proto::recovery::{HookActivitySeed, SessionRecoveryStore};

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
