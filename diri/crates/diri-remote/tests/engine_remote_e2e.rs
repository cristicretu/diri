#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::remote::binding::RemoteBindingStore;
use diri_engine::remote::executor::ProcessExecutor;
use diri_engine::remote::manager::{ArtifactCatalog, RemoteManager};
use diri_engine::{
    Authority, ManifestEngine, PtySpec, RemoteAdoptSpec, RemoteSessionSpec, Session, SessionSpec,
};
use diri_proto::remote_pty::{
    DirectoryListRequest, EnvironmentVariable, LaunchRequest, PersistenceCapability,
    SessionSelector, SessionToken,
};
use diri_proto::{HostEntry, SessionStatus};

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_diri-remote")
}

#[test]
fn engine_lists_remote_directories_through_the_verified_helper() {
    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    fs::create_dir_all(remote_home.join("zeta")).expect("zeta");
    fs::create_dir_all(remote_home.join("alpha")).expect("alpha");
    fs::write(remote_home.join("notes.txt"), b"not a directory").expect("file");
    let fake_ssh = write_fake_ssh(temporary.path(), &remote_home, &remote_state);
    let manager = RemoteManager::new(
        ProcessExecutor::new(fake_ssh),
        ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
        temporary.path().join("ssh-control"),
    )
    .expect("manager");
    let host = HostEntry {
        id: "directory-fixture".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: None,
        node: None,
    };

    manager.ensure_helper(&host).expect("bootstrap");
    let listing = manager
        .list_directories(&host, &DirectoryListRequest { path: "~".into() })
        .expect("remote directory listing");
    let names = listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"zeta"));
    assert!(!names.contains(&"notes.txt"));
    assert_eq!(
        listing.path,
        fs::canonicalize(&remote_home)
            .expect("canonical remote home")
            .to_string_lossy()
    );
}

#[test]
fn engine_bootstraps_detaches_and_adopts_the_same_remote_process() {
    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    fs::create_dir(&remote_home).expect("remote home");
    let fake_ssh = write_fake_ssh(temporary.path(), &remote_home, &remote_state);
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(fake_ssh),
            ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
            temporary.path().join("ssh-control"),
        )
        .expect("manager"),
    );
    let host = HostEntry {
        id: "fixture".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    assert_eq!(
        manager
            .probe_persistence(&host, &installed)
            .expect("persistence"),
        PersistenceCapability::NativeDetach
    );

    let session_id = "engine-remote-e2e".to_string();
    let token = SessionToken::new("0123456789abcdef0123456789abcdef").expect("token");
    let request = LaunchRequest {
        session_id: session_id.clone(),
        session_token: token.clone(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'ready>'; IFS= read -r first; printf 'first:%s\\nnext>' \"$first\"; IFS= read -r second; printf 'second:%s\\n' \"$second\"".into(),
        ],
        cwd: "/".into(),
        environment: vec![
            EnvironmentVariable {
                name: "PATH".into(),
                value: "/usr/bin:/bin".into(),
            },
            EnvironmentVariable {
                name: "TERM".into(),
                value: "xterm-256color".into(),
            },
        ],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NativeDetach,
    };
    let bindings = RemoteBindingStore::new(temporary.path().join("bindings")).expect("bindings");
    let engine = Arc::new(ManifestEngine::new(Vec::new()));
    let mut session = Session::spawn(
        SessionSpec {
            id: session_id.clone(),
            pty: PtySpec::new(request.argv.clone(), "/").size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temporary.path().join("logs"),
            holder: None,
            remote: Some(RemoteSessionSpec {
                manager: Arc::clone(&manager),
                helper: installed.clone(),
                launch: request,
                host_id: host.id.clone(),
                binding_store: bindings.clone(),
            }),
            defer_launch: false,
        },
        Arc::clone(&engine),
    )
    .expect("spawn remote Session");
    wait_for_grid(&session, "ready>");
    session.write_input(b"alpha\n").expect("first input");
    wait_for_grid(&session, "next>");

    let binding = bindings
        .load_all()
        .expect("load binding")
        .into_iter()
        .next()
        .expect("saved binding");
    let before = manager
        .inspect(
            &installed,
            &SessionSelector {
                session_id: session_id.clone(),
                session_token: token.clone(),
                expected_incarnation: Some(binding.session_incarnation.clone()),
            },
        )
        .expect("inspect before detach");
    let process_pid = match before.process_state {
        diri_proto::remote_pty::RemoteProcessState::Running { pid } => pid,
        state => panic!("unexpected process state: {state:?}"),
    };
    drop(session);

    let binding = bindings
        .load_all()
        .expect("reload binding after detach")
        .into_iter()
        .next()
        .expect("persisted binding");
    assert!(binding.last_output_offset > 0);

    let helper = manager
        .existing_helper(&host, &binding.helper_build_id, binding.protocol)
        .expect("old build");
    session = Session::adopt_remote_with_status(
        SessionSpec {
            id: session_id.clone(),
            pty: PtySpec::new(Vec::new(), "/").size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temporary.path().join("logs"),
            holder: None,
            remote: None,
            defer_launch: false,
        },
        RemoteAdoptSpec {
            manager: Arc::clone(&manager),
            helper,
            token: binding.session_token,
            incarnation: binding.session_incarnation.clone(),
            binding_store: bindings.clone(),
            output_offset: binding.last_output_offset,
        },
        engine,
        Some((SessionStatus::Idle, None)),
    )
    .expect("adopt remote Session");
    assert_eq!(
        session.view().status,
        SessionStatus::Idle,
        "reattaching an existing remote Agent must not look like a new launch"
    );
    wait_for_grid(&session, "next>");
    let after = manager
        .inspect(
            &installed,
            &SessionSelector {
                session_id: session_id.clone(),
                session_token: token,
                expected_incarnation: Some(binding.session_incarnation),
            },
        )
        .expect("inspect after adopt");
    assert!(matches!(
        after.process_state,
        diri_proto::remote_pty::RemoteProcessState::Running { pid } if pid == process_pid
    ));
    session.write_input(b"omega\n").expect("second input");
    wait_until("remote exit", Duration::from_secs(5), || {
        session.view().exited
    });
    assert!(session.screen_lines().join("\n").contains("second:omega"));
    let _ = session.terminate(Duration::from_millis(100));
}

#[test]
fn launch_response_disconnect_recovers_the_existing_holder_idempotently() {
    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    fs::create_dir(&remote_home).expect("remote home");
    let disconnect_marker = temporary.path().join("launch-response-lost");
    let fake_ssh = write_fake_ssh_with_launch_disconnect(
        temporary.path(),
        &remote_home,
        &remote_state,
        &disconnect_marker,
    );
    let manager = RemoteManager::new(
        ProcessExecutor::new(fake_ssh),
        ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
        temporary.path().join("ssh-control"),
    )
    .expect("manager");
    let host = HostEntry {
        id: "fixture-retry".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    let request = LaunchRequest {
        session_id: "launch-idempotency".into(),
        session_token: token_for_retry(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'retry-ready>'; IFS= read -r _".into(),
        ],
        cwd: "/".into(),
        environment: vec![EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launched = manager
        .launch(&installed, &request)
        .expect("idempotent launch retry");
    assert!(disconnect_marker.is_file());
    let selector = SessionSelector {
        session_id: request.session_id,
        session_token: request.session_token,
        expected_incarnation: Some(launched.session_incarnation.clone()),
    };
    let inspection = manager.inspect(&installed, &selector).expect("inspect");
    assert_eq!(
        inspection.process_state,
        diri_proto::remote_pty::RemoteProcessState::Running {
            pid: launched.process_pid
        }
    );
    assert!(
        manager
            .list(&installed)
            .expect("list")
            .iter()
            .any(|session| session.session_id == selector.session_id)
    );
    manager.kill(&installed, &selector).expect("cleanup");
    let gc = manager.gc(&installed).expect("gc");
    assert_eq!(gc.removed_sessions, 1);
}

#[test]
fn bootstrap_refuses_a_symlinked_remote_cache_ancestor() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    let outside = temporary.path().join("outside-cache");
    fs::create_dir(&remote_home).expect("remote home");
    fs::create_dir(&outside).expect("outside cache");
    symlink(&outside, remote_home.join(".cache")).expect("cache symlink");
    let fake_ssh = write_fake_ssh(temporary.path(), &remote_home, &remote_state);
    let manager = RemoteManager::new(
        ProcessExecutor::new(fake_ssh),
        ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
        temporary.path().join("ssh-control"),
    )
    .expect("manager");
    let host = HostEntry {
        id: "fixture-symlink".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: None,
        node: None,
    };
    let error = manager
        .ensure_helper(&host)
        .expect_err("symlinked cache must fail closed");
    assert!(error.to_string().contains("upload"));
    assert_eq!(fs::read_dir(outside).expect("outside").count(), 0);
}

#[test]
fn interrupted_upload_cleans_only_its_nonce_and_is_retryable() {
    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    fs::create_dir(&remote_home).expect("remote home");
    let disconnect_marker = temporary.path().join("upload-interrupted");
    let fake_ssh = write_fake_ssh_with_upload_disconnect(
        temporary.path(),
        &remote_home,
        &remote_state,
        &disconnect_marker,
    );
    let manager = RemoteManager::new(
        ProcessExecutor::new(fake_ssh),
        ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
        temporary.path().join("ssh-control"),
    )
    .expect("manager");
    let host = HostEntry {
        id: "fixture-upload-retry".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: None,
        node: None,
    };
    manager
        .ensure_helper(&host)
        .expect_err("first upload channel is deliberately interrupted");
    assert!(disconnect_marker.is_file());
    assert!(
        !tree_contains_prefix(&remote_home, ".tmp-"),
        "the failed nonce upload must be cleaned without touching other builds"
    );
    let installed = manager
        .ensure_helper(&host)
        .expect("a new nonce retries cleanly");
    assert!(
        remote_home
            .join(format!(
                ".cache/diri/bin/protocol-1/{}/diri-remote",
                installed.build_id
            ))
            .is_file()
    );
}

#[test]
fn attach_ssh_disconnect_reconnects_and_flushes_queued_input() {
    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    fs::create_dir(&remote_home).expect("remote home");
    let disconnect_marker = temporary.path().join("attach-interrupted");
    let fake_ssh = write_fake_ssh_with_attach_disconnect(
        temporary.path(),
        &remote_home,
        &remote_state,
        &disconnect_marker,
    );
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(fake_ssh),
            ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
            temporary.path().join("ssh-control"),
        )
        .expect("manager"),
    );
    let host = HostEntry {
        id: "fixture-attach-retry".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    let request = LaunchRequest {
        session_id: "attach-reconnect".into(),
        session_token: token_for_retry(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'attach-ready>'; IFS= read -r value; printf 'attach-bye:%s\\n' \"$value\""
                .into(),
        ],
        cwd: "/".into(),
        environment: vec![EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let bindings = RemoteBindingStore::new(temporary.path().join("bindings")).expect("bindings");
    let mut session = Session::spawn(
        SessionSpec {
            id: request.session_id.clone(),
            pty: PtySpec::new(request.argv.clone(), "/").size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temporary.path().join("logs"),
            holder: None,
            remote: Some(RemoteSessionSpec {
                manager,
                helper: installed,
                launch: request,
                host_id: host.id,
                binding_store: bindings,
            }),
            defer_launch: false,
        },
        Arc::new(ManifestEngine::new(Vec::new())),
    )
    .expect("spawn remote Session");
    wait_until("first attach interruption", Duration::from_secs(5), || {
        disconnect_marker.is_file()
    });
    // Input during the reconnect window is bounded and delivered after the
    // replacement controller receives ControlGranted.
    session
        .write_input(b"after-ssh-reconnect\n")
        .expect("queued input");
    // Wait for the echo itself, not for the exit. `exited` flips when the
    // remote process is reaped, which can beat the last of its output through
    // the Holder, the frame queue and the terminal parser — so asserting the
    // screen right after it raced the flush and failed on a loaded CI runner
    // while passing locally.
    wait_until(
        "the queued input to echo after reconnect",
        Duration::from_secs(10),
        || {
            session
                .screen_lines()
                .join("\n")
                .contains("attach-bye:after-ssh-reconnect")
        },
    );
    wait_until(
        "remote exit after reconnect",
        Duration::from_secs(10),
        || session.view().exited,
    );
    session
        .terminate(Duration::from_millis(200))
        .expect("cleanup Holder");
}

fn wait_for_grid(session: &Session, needle: &str) {
    wait_until(needle, Duration::from_secs(5), || {
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

fn write_fake_ssh(root: &Path, home: &Path, state: &Path) -> std::path::PathBuf {
    let path = root.join("ssh");
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&path)
        .expect("fake ssh");
    writeln!(
        file,
        "#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\nexec /bin/sh -c \"$last\"",
        home.display(),
        state.display()
    )
    .expect("fake ssh script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
    path
}

fn write_fake_ssh_with_launch_disconnect(
    root: &Path,
    home: &Path,
    state: &Path,
    marker: &Path,
) -> std::path::PathBuf {
    let path = root.join("ssh-launch-disconnect");
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&path)
        .expect("fake ssh");
    writeln!(
        file,
        "#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\ncase \"$last\" in\n  *' launch'*)\n    if [ ! -e '{}' ]; then\n      : > '{}'\n      /bin/sh -c \"$last\" >/dev/null\n      printf 'simulated lost launch response\\n' >&2\n      exit 255\n    fi\n    ;;\nesac\nexec /bin/sh -c \"$last\"",
        home.display(),
        state.display(),
        marker.display(),
        marker.display(),
    )
    .expect("fake ssh script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
    path
}

fn write_fake_ssh_with_upload_disconnect(
    root: &Path,
    home: &Path,
    state: &Path,
    marker: &Path,
) -> std::path::PathBuf {
    let path = root.join("ssh-upload-disconnect");
    write_executable_script(
        &path,
        &format!(
            "#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\ncase \"$last\" in\n  *'cat > '*)\n    if [ ! -e '{}' ]; then\n      : > '{}'\n      head -c 1024 | /bin/sh -c \"$last\"\n      printf 'simulated interrupted upload\\n' >&2\n      exit 255\n    fi\n    ;;\nesac\nexec /bin/sh -c \"$last\"",
            home.display(),
            state.display(),
            marker.display(),
            marker.display(),
        ),
    );
    path
}

fn write_fake_ssh_with_attach_disconnect(
    root: &Path,
    home: &Path,
    state: &Path,
    marker: &Path,
) -> std::path::PathBuf {
    let path = root.join("ssh-attach-disconnect");
    write_executable_script(
        &path,
        &format!(
            "#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\ncase \"$last\" in\n  *' attach'*)\n    if [ ! -e '{}' ]; then\n      : > '{}'\n      /bin/sh -c \"$last\" <&0 & bridge=$!\n      (sleep 0.2; kill \"$bridge\" 2>/dev/null || true) & killer=$!\n      wait \"$bridge\" || true\n      kill \"$killer\" 2>/dev/null || true\n      wait \"$killer\" 2>/dev/null || true\n      printf 'simulated interrupted attach\\n' >&2\n      exit 255\n    fi\n    ;;\nesac\nexec /bin/sh -c \"$last\"",
            home.display(),
            state.display(),
            marker.display(),
            marker.display(),
        ),
    );
    path
}

fn write_executable_script(path: &Path, contents: &str) {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(path)
        .expect("fake ssh");
    file.write_all(contents.as_bytes())
        .expect("fake ssh script");
    drop(file);
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
}

fn tree_contains_prefix(root: &Path, prefix: &str) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            return true;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && tree_contains_prefix(&entry.path(), prefix)
        {
            return true;
        }
    }
    false
}

fn token_for_retry() -> SessionToken {
    SessionToken::new("abcdef0123456789abcdef0123456789").expect("token")
}
