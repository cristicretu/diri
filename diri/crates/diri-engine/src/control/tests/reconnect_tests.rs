use super::*;
use crate::remote::{
    binding::RemoteBindingStore,
    bootstrap::RemoteTarget,
    executor::ProcessExecutor,
    manager::{ArtifactCatalog, InstalledHelper, RemoteManager},
    ssh::SshTransport,
};
use diri_proto::remote_pty::*;
use diri_proto::{RemoteConnectionState as State, SessionReconnectResult};
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "fixture progress");
        std::thread::sleep(Duration::from_millis(2));
    }
}
struct AgentProcess(std::process::Child);
impl std::ops::Deref for AgentProcess {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for AgentProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    server: Arc<ControlServer>,
    child: AgentProcess,
    inspection: SessionInspection,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let child = AgentProcess(
            std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "while [ ! -f agent-exit ]; do sleep 0.01; done; exit 126",
                ])
                .current_dir(temp.path())
                .spawn()
                .unwrap(),
        );
        let pid = child.id();
        let hello = HelloAck {
            protocol: ProtocolVersion::CURRENT,
            holder_build_id: "fixture".into(),
            session_incarnation: "same-incarnation".into(),
            capabilities: PHASE_ONE_HOLDER_CAPABILITIES.to_vec(),
            controller_epoch: 1,
            process_state: RemoteProcessState::Running { pid },
            output_offset: 0,
            snapshot_sequence: 1,
            foreground_pid: Some(pid as i32),
            child_identity: None,
        };
        let mut screen = crate::screen::HeadlessScreen::new(80, 24);
        screen.feed(b"preserved remote screen");
        let snapshot = FullSnapshot {
            sequence: 1,
            alt_screen: false,
            bracketed_paste: false,
            mouse: Default::default(),
            grid: screen.full_snapshot(),
        };
        for (name, message) in [
            ("hello", RemoteMessage::HelloAck(hello.clone())),
            ("snapshot", RemoteMessage::FullSnapshot(snapshot)),
            (
                "fatal",
                RemoteMessage::Terminal(diri_proto::frames::Frame::input(
                    b"wrong direction".to_vec(),
                )),
            ),
        ] {
            std::fs::write(
                temp.path().join(name),
                RemoteCodec::encode(&message).unwrap(),
            )
            .unwrap();
        }
        let mut retry_hello = hello;
        retry_hello.controller_epoch = 2;
        std::fs::write(
            temp.path().join("retry-hello"),
            RemoteCodec::encode(&RemoteMessage::HelloAck(retry_hello)).unwrap(),
        )
        .unwrap();
        let inspection = SessionInspection {
            session_id: "fixture".into(),
            session_incarnation: "same-incarnation".into(),
            holder_build_id: "fixture".into(),
            holder_pid: pid,
            process_state: RemoteProcessState::Running { pid },
            child_identity: None,
            process_facts: None,
            cols: 80,
            rows: 24,
            output_offset: 0,
            snapshot_sequence: 1,
            controller_epoch: 1,
            persistence: PersistenceCapability::NativeDetach,
        };
        std::fs::write(
            temp.path().join("inspection"),
            serde_json::to_vec(&inspection).unwrap(),
        )
        .unwrap();
        let fake = temp.path().join("ssh");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
for last; do :; done
case "$last" in
  *' inspect'*)
    : > inspect-started
    while [ -f hold-inspect ] && [ ! -f release-inspect ]; do sleep 0.01; done
    cat inspection
    exit 0;;
esac
printf x >> attaches
if mkdir first 2>/dev/null; then
  cat hello snapshot fatal
else
  while [ ! -f allow-retry ]; do sleep 0.01; done
  cat retry-hello
  while [ -f hold-seed ] && [ ! -f release-seed ]; do sleep 0.01; done
  cat snapshot
  if [ -f retry-fatal ]; then cat fatal; else cat > retry-input; fi
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let manager = Arc::new(
            RemoteManager::new(
                ProcessExecutor::new(&fake),
                ArtifactCatalog::without_artifacts_for_test(),
                temp.path().join("control"),
            )
            .unwrap(),
        );
        let host = diri_proto::HostEntry {
            id: "fixture".into(),
            name: None,
            ssh: "fixture".into(),
            default_cwd: None,
            node: None,
        };
        let helper = InstalledHelper {
            target: RemoteTarget::MacosAarch64,
            build_id: "fixture".into(),
            protocol: ProtocolVersion::CURRENT,
            transport: SshTransport::new(&host, temp.path().join("control/socket"))
                .with_executable(&fake),
        };
        let server = server(temp.path());
        let mut record = test_record("fixture");
        record.host = Some("fixture".into());
        let mut registry = server.registry.lock().unwrap();
        registry.insert_record(record);
        registry
            .adopt_remote(
                crate::session::SessionSpec {
                    id: "fixture".into(),
                    pty: crate::PtySpec::new(Vec::new(), temp.path()),
                    manifest_id: "shell".into(),
                    authority: crate::Authority::ProcessOnly,
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                crate::session::RemoteAdoptSpec {
                    manager,
                    helper,
                    token: SessionToken::new("fixture-remote-token").unwrap(),
                    incarnation: "same-incarnation".into(),
                    binding_store: RemoteBindingStore::new(temp.path().join("bindings")).unwrap(),
                    output_offset: 0,
                },
            )
            .unwrap();
        drop(registry);
        let fixture = Self {
            temp,
            server,
            child,
            inspection,
        };
        wait_for(|| fixture.state() == State::Failed);
        fixture
    }
    fn state(&self) -> State {
        self.server
            .registry
            .lock()
            .unwrap()
            .record("fixture")
            .unwrap()
            .remote_connection
            .unwrap()
            .state
    }
    fn reconnect(&self) -> Result<SessionReconnectResult, ControlError> {
        self.server
            .dispatch(
                Method::SESSION_RECONNECT,
                Some(json!({"sessionID":"fixture"})),
            )
            .map(|value| serde_json::from_value(value).unwrap())
    }
    fn marker(&self, name: &str) {
        std::fs::write(self.temp.path().join(name), "").unwrap();
    }
    fn write_inspection(&self, value: &SessionInspection) {
        std::fs::write(
            self.temp.path().join("inspection"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn explicit_reconnect_preserves_owner_and_grid_until_a_validated_seed() {
    let mut f = Fixture::new();
    let before = f
        .server
        .registry
        .lock()
        .unwrap()
        .get("fixture")
        .unwrap()
        .preview_seed()
        .grid;
    f.marker("hold-seed");
    f.marker("allow-retry");
    let result = f.reconnect().unwrap();
    assert!(result.started);
    assert!(!result.uncertain_input_discarded);
    wait_for(|| std::fs::read(f.temp.path().join("attaches")).unwrap().len() == 2);
    assert_eq!(f.state(), State::Reconnecting);
    assert_eq!(
        f.server
            .registry
            .lock()
            .unwrap()
            .get("fixture")
            .unwrap()
            .preview_seed()
            .grid,
        before
    );
    assert!(!f.reconnect().unwrap().started);
    f.marker("release-seed");
    wait_for(|| f.state() == State::Connected);
    let registry = f.server.registry.lock().unwrap();
    let session = registry.get("fixture").unwrap();
    assert_eq!(session.child_pid(), f.child.id() as i32);
    assert_eq!(session.preview_seed().grid, before);
    assert!(f.child.try_wait().unwrap().is_none());
}

#[test]
fn inspection_does_not_lock_registry_and_reserves_reconnect_stop_and_remove() {
    let mut f = Fixture::new();
    f.marker("hold-inspect");
    let server = Arc::clone(&f.server);
    let worker = std::thread::spawn(move || {
        server.dispatch(
            Method::SESSION_RECONNECT,
            Some(json!({"sessionID":"fixture"})),
        )
    });
    wait_for(|| f.temp.path().join("inspect-started").exists());
    assert!(f.server.registry.try_lock().is_ok());
    for method in [
        Method::SESSION_RECONNECT,
        Method::SESSION_KILL,
        Method::SESSION_REMOVE,
    ] {
        assert!(
            f.server
                .dispatch(method, Some(json!({"sessionID":"fixture"})))
                .unwrap_err()
                .message
                .contains("already changing")
        );
    }
    f.marker("agent-exit");
    assert_eq!(f.child.wait().unwrap().code(), Some(126));
    let mut exit = f.inspection.clone();
    exit.process_state = RemoteProcessState::Exited {
        code: Some(126),
        signal: None,
    };
    f.write_inspection(&exit);
    f.marker("release-inspect");
    let result: SessionReconnectResult =
        serde_json::from_value(worker.join().unwrap().unwrap()).unwrap();
    assert!(!result.started);
    assert_eq!(f.state(), State::Exited);
    assert!(
        matches!(result.session.status,diri_proto::SessionStatus::Exited(info) if info.code==Some(126))
    );
    assert_eq!(std::fs::read(f.temp.path().join("attaches")).unwrap(), b"x");
}

#[test]
fn wrong_inspection_identity_and_repeated_failed_retry_preserve_the_session() {
    let mut f = Fixture::new();
    for field in ["build", "incarnation", "pid"] {
        let mut wrong = f.inspection.clone();
        match field {
            "build" => wrong.holder_build_id = "wrong".into(),
            "incarnation" => wrong.session_incarnation = "wrong".into(),
            _ => {
                wrong.process_state = RemoteProcessState::Running {
                    pid: f.child.id() + 1,
                }
            }
        }
        f.write_inspection(&wrong);
        assert_eq!(f.reconnect().unwrap_err().code, "remote_reconnect_failed");
        assert_eq!(f.state(), State::Failed);
    }
    f.write_inspection(&f.inspection);
    f.marker("allow-retry");
    f.marker("retry-fatal");
    for count in [2, 3] {
        assert!(f.reconnect().unwrap().started);
        wait_for(|| {
            f.state() == State::Failed
                && std::fs::read(f.temp.path().join("attaches")).unwrap().len() == count
        });
    }
    assert!(f.child.try_wait().unwrap().is_none());
    assert!(
        f.server
            .registry
            .lock()
            .unwrap()
            .get("fixture")
            .unwrap()
            .write_input(b"never replay")
            .is_err()
    );
}

#[test]
fn attach_rechecks_identity_after_inspection_without_replacing_the_preserved_grid() {
    for field in ["build", "incarnation", "pid", "epoch", "exited"] {
        let mut f = Fixture::new();
        let before = f
            .server
            .registry
            .lock()
            .unwrap()
            .get("fixture")
            .unwrap()
            .preview_seed()
            .grid;
        f.marker("hold-inspect");
        f.marker("allow-retry");
        let server = Arc::clone(&f.server);
        let worker = std::thread::spawn(move || {
            server.dispatch(
                Method::SESSION_RECONNECT,
                Some(json!({"sessionID":"fixture"})),
            )
        });
        wait_for(|| f.temp.path().join("inspect-started").exists());
        let bytes = std::fs::read(f.temp.path().join("retry-hello")).unwrap();
        let RemoteMessage::HelloAck(mut hello) =
            RemoteCodec::new().feed(&bytes).unwrap().pop().unwrap()
        else {
            panic!("hello fixture");
        };
        match field {
            "build" => hello.holder_build_id = "wrong".into(),
            "incarnation" => hello.session_incarnation = "wrong".into(),
            "epoch" => {
                hello.controller_epoch = 1;
            }
            "pid" => {
                hello.process_state = RemoteProcessState::Running {
                    pid: f.child.id() + 1,
                }
            }
            _ => {
                f.marker("agent-exit");
                assert_eq!(f.child.wait().unwrap().code(), Some(126));
                hello.process_state = RemoteProcessState::Exited {
                    code: Some(126),
                    signal: None,
                }
            }
        }
        std::fs::write(
            f.temp.path().join("retry-hello"),
            RemoteCodec::encode(&RemoteMessage::HelloAck(hello)).unwrap(),
        )
        .unwrap();
        f.marker("release-inspect");
        assert!(worker.join().unwrap().is_ok());
        wait_for(|| {
            if field == "exited" {
                f.state() == State::Exited
            } else {
                f.state() == State::Failed
            }
        });
        let registry = f.server.registry.lock().unwrap();
        let session = registry.get("fixture").unwrap();
        assert_eq!(session.child_pid(), f.child.id() as i32);
        assert_eq!(session.preview_seed().grid, before);
        assert!(
            session
                .write_input(b"no input after invalid or exited attach")
                .is_err()
        );
    }
}

#[test]
fn reconnect_rejects_local_and_missing_remote_owners_without_relaunch() {
    let temp = tempfile::tempdir().unwrap();
    let server = server(temp.path());
    let mut record = test_record("fixture");
    server
        .registry
        .lock()
        .unwrap()
        .insert_record(record.clone());
    assert_eq!(
        server
            .dispatch(
                Method::SESSION_RECONNECT,
                Some(json!({"sessionID":"fixture"}))
            )
            .unwrap_err()
            .code,
        "bad_request"
    );
    record.host = Some("fixture".into());
    record.remote_connection = Some(diri_proto::RemoteConnection {
        state: State::Failed,
        since: diri_proto::DateMillis(0.0),
    });
    server.registry.lock().unwrap().insert_record(record);
    assert_eq!(
        server
            .dispatch(
                Method::SESSION_RECONNECT,
                Some(json!({"sessionID":"fixture"}))
            )
            .unwrap_err()
            .code,
        "remote_owner_unavailable"
    );
    assert!(server.registry.lock().unwrap().get("fixture").is_none());
}

#[test]
fn process_facts_are_remote_read_only_and_leave_registry_available() {
    use diri_proto::process::{BootId, ProcessBirth, ProcessIdentity};
    use diri_proto::process_facts::{ProcessFacts, ProcessValue as Value, UnavailableReason};
    let mut fixture = Fixture::new();
    // Foreign-platform birth deliberately cannot be verified by local proc APIs.
    let identity = ProcessIdentity::new(
        fixture.child.id(),
        ProcessBirth::Linux {
            boot_id: BootId::parse("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            start_ticks: 77,
            clock_ticks_per_second: 100,
        },
    )
    .unwrap();
    fixture.inspection.child_identity = Some(identity);
    fixture.inspection.process_facts = Some(ProcessFacts {
        identity,
        executable: Value::available("/remote/bin/fixture".into()),
        working_directory: Value::available("/remote/work".into()),
        user_ids: Value::unavailable(UnavailableReason::PermissionDenied),
        account: Value::unavailable(UnavailableReason::PermissionDenied),
        process_group: Value::available(fixture.child.id()),
        foreground_process_group: Value::available(Some(123)),
    });
    fixture.write_inspection(&fixture.inspection);
    fixture.marker("hold-inspect");
    let attaches = std::fs::read(fixture.temp.path().join("attaches")).unwrap();
    let server = Arc::clone(&fixture.server);
    let pending = std::thread::spawn(move || {
        server.dispatch(
            Method::SESSION_PROCESS_INFO,
            Some(json!({"sessionID":"fixture"})),
        )
    });
    wait_for(|| fixture.temp.path().join("inspect-started").exists());
    assert!(
        fixture.server.registry.try_lock().is_ok(),
        "slow remote lookup held Registry"
    );
    fixture.marker("release-inspect");
    let result = pending.join().unwrap().unwrap();
    assert_eq!(
        result["process"]["executable"]["value"],
        "/remote/bin/fixture"
    );
    assert_eq!(result["process"]["foregroundProcessGroup"]["value"], 123);
    assert_eq!(
        std::fs::read(fixture.temp.path().join("attaches")).unwrap(),
        attaches
    );
    assert_eq!(
        fixture.state(),
        State::Failed,
        "inspection must not reconnect"
    );
    // A record moved to a different host while I/O is in flight cannot
    // receive facts from the old captured session binding.
    std::fs::remove_file(fixture.temp.path().join("inspect-started")).unwrap();
    std::fs::remove_file(fixture.temp.path().join("release-inspect")).unwrap();
    let server = Arc::clone(&fixture.server);
    let pending = std::thread::spawn(move || {
        server.dispatch(
            Method::SESSION_PROCESS_INFO,
            Some(json!({"sessionID":"fixture"})),
        )
    });
    wait_for(|| fixture.temp.path().join("inspect-started").exists());
    let original = fixture
        .server
        .registry
        .lock()
        .unwrap()
        .record("fixture")
        .unwrap();
    let mut changed = original.clone();
    changed.host = Some("replacement-host".into());
    fixture
        .server
        .registry
        .lock()
        .unwrap()
        .insert_record(changed);
    fixture.marker("release-inspect");
    assert_eq!(pending.join().unwrap().unwrap_err().code, "stale_session");
    fixture
        .server
        .registry
        .lock()
        .unwrap()
        .insert_record(original);
    fixture.inspection.process_facts = None;
    fixture.write_inspection(&fixture.inspection);
    assert_eq!(
        fixture
            .server
            .dispatch(
                Method::SESSION_PROCESS_INFO,
                Some(json!({"sessionID":"fixture"}))
            )
            .unwrap_err()
            .code,
        "process_facts_unsupported"
    );
}
