//! Real Helper subprocess contract with a disposable child and synthetic state.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Output, Stdio};

use diri_proto::remote_pty::{RemoteManagementFailure, RemoteProcessState, SessionInspection};
use sha2::{Digest, Sha256};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn missing_owner_is_a_nonzero_typed_failure_and_never_a_fabricated_exit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("state");
    let session = root.join("sessions/owner-loss");
    fs::create_dir_all(&session).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(root.join("sessions"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&session, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = ChildGuard(
        Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let token = "0123456789abcdef0123456789abcdef";
    fs::write(
        session.join("auth.sha256"),
        Sha256::digest(token)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .unwrap();
    let mut state = serde_json::json!({
        "schema": 1, "sessionId": "owner-loss", "sessionIncarnation": "incarnation",
        "holderBuildId": "synthetic-build", "holderPid": 1,
        "processState": {"state":"running", "pid":child.0.id()},
        "cols":80, "rows":24, "outputOffset":0, "snapshotSequence":0,
        "controllerEpoch":0, "persistence":"non-persistent", "createdAtUnixMs":0
    });
    let invoke = |token: &str| -> Output {
        let mut helper = Command::new(env!("CARGO_BIN_EXE_diri-remote"))
            .arg("inspect")
            .env("DIRI_REMOTE_STATE_DIR", &root)
            .env("HOME", temporary.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let request = serde_json::json!({
            "sessionId":"owner-loss", "sessionToken":token,
            "expectedIncarnation":"incarnation"
        });
        helper
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        helper.wait_with_output().unwrap()
    };
    let bytes = serde_json::to_vec(&state).unwrap();
    fs::write(session.join("session.json"), &bytes).unwrap();
    let failed = invoke(token);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<RemoteManagementFailure>(&failed.stdout).unwrap(),
        RemoteManagementFailure::HolderUnavailable
    );
    assert_eq!(fs::read(session.join("session.json")).unwrap(), bytes);
    assert!(child.0.try_wait().unwrap().is_none());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains(token));
    let unauthorized = invoke("fedcba9876543210fedcba9876543210");
    assert!(!unauthorized.status.success());
    assert!(
        unauthorized.stdout.is_empty(),
        "authentication precedes owner facts"
    );

    state["processState"] = serde_json::to_value(RemoteProcessState::Exited {
        code: Some(126),
        signal: None,
    })
    .unwrap();
    fs::write(
        session.join("session.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let exited = invoke(token);
    assert!(
        exited.status.success(),
        "{}",
        String::from_utf8_lossy(&exited.stderr)
    );
    let inspection: SessionInspection = serde_json::from_slice(&exited.stdout).unwrap();
    assert_eq!(
        inspection.process_state,
        RemoteProcessState::Exited {
            code: Some(126),
            signal: None
        }
    );
    eprintln!(
        "Helper contract: owner missing => exit1 / holder_unavailable; stored Running unchanged; child alive; recorded exit126 preserved"
    );
}
