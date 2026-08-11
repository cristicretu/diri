#![cfg(unix)]

//! Opt-in acceptance test for the system OpenSSH path.
//!
//! This test is ignored by default because it installs the exact test Helper
//! into a real remote account and deliberately drops/reopens its SSH attach.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::remote::binding::RemoteBindingStore;
use diri_engine::remote::executor::ProcessExecutor;
use diri_engine::remote::manager::{ArtifactCatalog, InstalledHelper, RemoteManager};
use diri_engine::{
    Authority, ManifestEngine, PtySpec, RemoteAdoptSpec, RemoteSessionSpec, Session, SessionSpec,
};
use diri_proto::HostEntry;
use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, LaunchRequest, PersistenceCapability, RemoteProcessState,
    SessionSelector, SessionToken,
};

#[test]
#[ignore = "requires DIRI_REMOTE_SSH_TARGET and a disposable real SSH account"]
fn real_ssh_detach_soak_reconnects_the_same_process() {
    let target = required_env("DIRI_REMOTE_SSH_TARGET");
    let helper_path = env::var_os("DIRI_REMOTE_HELPER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_diri-remote")));
    let ssh = env::var_os("DIRI_REMOTE_SSH_EXECUTABLE").unwrap_or_else(|| "ssh".into());
    let requested_cwd = env::var("DIRI_REMOTE_CWD").unwrap_or_else(|_| "~".into());
    let soak = env::var("DIRI_REMOTE_SOAK_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>().expect("valid soak seconds"))
        .unwrap_or(180);

    let temporary = tempfile::tempdir().expect("local acceptance state");
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(ssh),
            ArtifactCatalog::from_native_helper(&helper_path).expect("native Helper catalog"),
            temporary.path().join("ssh-control"),
        )
        .expect("remote manager"),
    );
    let host = HostEntry {
        id: "real-ssh-soak".into(),
        name: None,
        ssh: target,
        default_cwd: Some(requested_cwd.clone()),
        node: None,
    };
    let helper = manager.ensure_helper(&host).expect("install exact Helper");
    let persistence = manager
        .probe_persistence(&host, &helper)
        .expect("probe remote persistence");
    assert_ne!(
        persistence,
        PersistenceCapability::NonPersistent,
        "real detach soak requires native-detach or an existing transient user supervisor"
    );
    let captured = manager
        .capture_environment(
            &helper,
            &EnvironmentCaptureRequest {
                cwd: Some(requested_cwd),
                timeout_millis: 10_000,
            },
        )
        .expect("capture remote login/cwd environment");

    let nonce = random_hex(8);
    let session_id = format!("real-ssh-soak-{}-{nonce}", std::process::id());
    let token = SessionToken::new(random_hex(32)).expect("session token");
    eprintln!("remote soak session id: {session_id}");
    let request = LaunchRequest {
        session_id: session_id.clone(),
        session_token: token.clone(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'soak-ready>'; IFS= read -r first; printf 'first:%s\\nsoak-next>' \"$first\"; IFS= read -r second; printf 'second:%s\\n' \"$second\"".into(),
        ],
        cwd: captured.cwd.clone(),
        environment: captured.environment,
        cols: 80,
        rows: 24,
        persistence,
    };
    let bindings = RemoteBindingStore::new(temporary.path().join("bindings")).expect("bindings");
    let engine = Arc::new(ManifestEngine::new(Vec::new()));
    let mut session = Session::spawn(
        session_spec(
            &session_id,
            temporary.path(),
            Some(RemoteSessionSpec {
                manager: Arc::clone(&manager),
                helper: helper.clone(),
                launch: request,
                host_id: host.id.clone(),
                binding_store: bindings.clone(),
            }),
        ),
        Arc::clone(&engine),
    )
    .expect("launch remote soak session");
    let binding = load_binding(&bindings, &session_id);
    let selector = SessionSelector {
        session_id: session_id.clone(),
        session_token: token,
        expected_incarnation: Some(binding.session_incarnation.clone()),
    };
    let mut cleanup = RemoteCleanup {
        manager: Arc::clone(&manager),
        helper: helper.clone(),
        selector: selector.clone(),
        armed: true,
    };

    wait_for_grid(&session, "soak-ready>");
    session.write_input(b"before-drop\n").expect("first input");
    wait_for_grid(&session, "soak-next>");
    let process_pid = running_pid(
        manager
            .inspect(&helper, &selector)
            .expect("inspect before detach")
            .process_state,
    );

    // Closing Session drops the SSH Bridge. Closing the ControlMaster then
    // tears down Diri's final underlying SSH connection; the remote Holder
    // and process must survive independently for the requested soak interval.
    drop(session);
    manager
        .close_control_masters()
        .expect("close every Diri SSH connection before soak");
    std::thread::sleep(Duration::from_secs(soak));

    let binding = load_binding(&bindings, &session_id);
    assert_eq!(
        running_pid(
            manager
                .inspect(&helper, &selector)
                .expect("inspect after detach")
                .process_state,
        ),
        process_pid,
        "detach must preserve the exact Agent process identity"
    );
    let exact_helper = manager
        .existing_helper(&host, &binding.helper_build_id, binding.protocol)
        .expect("reopen exact live Helper build");
    session = Session::adopt_remote(
        session_spec(&session_id, temporary.path(), None),
        RemoteAdoptSpec {
            manager: Arc::clone(&manager),
            helper: exact_helper,
            token: binding.session_token,
            incarnation: binding.session_incarnation,
            binding_store: bindings,
            output_offset: binding.last_output_offset,
        },
        engine,
    )
    .expect("reattach remote soak session");
    wait_for_grid(&session, "soak-next>");
    session
        .write_input(b"after-reconnect\n")
        .expect("second input");
    wait_until("remote process exit", Duration::from_secs(15), || {
        session.view().exited
    });
    assert!(
        session
            .screen_lines()
            .join("\n")
            .contains("second:after-reconnect")
    );
    session
        .terminate(Duration::from_millis(500))
        .expect("clean Holder state");
    cleanup.armed = false;
}

#[test]
#[ignore = "requires a disposable PAM/logind SSH account that kills logout processes"]
fn real_ssh_pam_logout_is_classified_non_persistent() {
    let target = required_env("DIRI_REMOTE_SSH_TARGET");
    let helper_path = env::var_os("DIRI_REMOTE_HELPER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_diri-remote")));
    let ssh = env::var_os("DIRI_REMOTE_SSH_EXECUTABLE").unwrap_or_else(|| "ssh".into());
    let temporary = tempfile::tempdir().expect("local acceptance state");
    let manager = RemoteManager::new(
        ProcessExecutor::new(ssh),
        ArtifactCatalog::from_native_helper(&helper_path).expect("native Helper catalog"),
        temporary.path().join("ssh-control"),
    )
    .expect("remote manager");
    let host = HostEntry {
        id: "real-ssh-pam-logout".into(),
        name: None,
        ssh: target,
        default_cwd: Some("~".into()),
        node: None,
    };
    let helper = manager.ensure_helper(&host).expect("install exact Helper");

    assert_eq!(
        manager
            .probe_persistence(&host, &helper)
            .expect("probe remote persistence"),
        PersistenceCapability::NonPersistent,
        "a PAM/logind account that kills logout processes must never claim detach support"
    );
    manager
        .close_control_masters()
        .expect("close PAM probe ControlMaster");
}

fn session_spec(id: &str, local_root: &Path, remote: Option<RemoteSessionSpec>) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(Vec::new(), "/").size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: local_root.join("logs"),
        holder: None,
        remote,
        defer_launch: false,
    }
}

fn running_pid(state: RemoteProcessState) -> u32 {
    match state {
        RemoteProcessState::Running { pid } => pid,
        other => panic!("expected running remote process, got {other:?}"),
    }
}

fn wait_for_grid(session: &Session, needle: &str) {
    wait_until(needle, Duration::from_secs(15), || {
        session.screen_lines().join("\n").contains(needle)
    });
}

fn wait_until(what: &str, timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must name a disposable SSH target"))
}

fn load_binding(
    store: &RemoteBindingStore,
    session_id: &str,
) -> diri_engine::remote::binding::RemoteBinding {
    store
        .load_all()
        .expect("load owner bindings")
        .into_iter()
        .find(|binding| binding.session_id == session_id)
        .expect("owner binding exists")
}

fn random_hex(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random).expect("secure random");
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct RemoteCleanup {
    manager: Arc<RemoteManager>,
    helper: InstalledHelper,
    selector: SessionSelector,
    armed: bool,
}

impl Drop for RemoteCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.manager.kill(&self.helper, &self.selector);
        }
    }
}
