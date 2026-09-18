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

fn unique_stop_session_id(label: &str) -> String {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).expect("fixture nonce");
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{label}-{suffix}")
}

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_diri-remote")
}

/// A slow WAN history reply must not stall the control connection or the
/// Registry used by attach input and grid publication. Pause only our fixture
/// Holder to make the remote reply deterministically slower than local work.
#[test]
fn remote_scrollback_does_not_block_input_or_screen_reads() {
    use diri_engine::control::ControlServer;
    use diri_engine::registry::Registry;
    use diri_proto::frames::{Frame, FrameCodec};
    use diri_proto::{ControlMessage, Method, WIRE_VERSION};
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;

    let temporary = tempfile::tempdir().expect("temp");
    let home = temporary.path().join("home");
    fs::create_dir(&home).unwrap();
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(write_fake_ssh(
                temporary.path(),
                &home,
                &temporary.path().join("remote-state"),
            )),
            ArtifactCatalog::from_native_helper(Path::new(helper())).unwrap(),
            temporary.path().join("control"),
        )
        .unwrap(),
    );
    let host = HostEntry {
        id: "latency".into(),
        name: None,
        ssh: "fixture".into(),
        default_cwd: None,
        node: None,
    };
    let installed = manager.ensure_helper(&host).unwrap();
    let token = token_for_retry();
    let id = "scroll-latency";
    let request = LaunchRequest {
        session_id: id.into(),
        session_token: token.clone(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "i=0; while [ \"$i\" -lt 80 ]; do printf 'history:%s\\n' \"$i\"; i=$((i + 1)); done; printf 'ready>'; IFS= read -r line; printf 'received:%s\\n' \"$line\""
                .into(),
        ],
        cwd: "/".into(),
        environment: vec![],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NativeDetach,
    };
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(ManifestEngine::new(Vec::new())),
        temporary.path().join("state.json"),
    )));
    let record = serde_json::from_value(json!({
        "id":id, "kind":{"shell":{}}, "cwd":"/", "projectID":"p",
        "title":"latency", "titleSource":0, "status":{"starting":{}},
        "resumability":"notResumable", "createdAt":0, "updatedAt":0, "pinned":false,
        "host":host.id,
    }))
    .unwrap();
    registry
        .lock()
        .unwrap()
        .spawn(
            SessionSpec {
                id: id.into(),
                pty: PtySpec::new(request.argv.clone(), "/").size(80, 24),
                manifest_id: "shell".into(),
                authority: Authority::ProcessOnly,
                logs_dir: temporary.path().join("logs"),
                holder: None,
                defer_launch: false,
                remote: Some(RemoteSessionSpec {
                    manager: Arc::clone(&manager),
                    helper: installed.clone(),
                    launch: request,
                    host_id: host.id.clone(),
                    binding_store: RemoteBindingStore::new(temporary.path().join("bindings"))
                        .unwrap(),
                }),
            },
            record,
        )
        .unwrap();
    wait_for_grid(registry.lock().unwrap().get(id).unwrap(), "ready>");
    let selector = SessionSelector {
        session_id: id.into(),
        session_token: token,
        expected_incarnation: None,
    };
    let inspection = manager.inspect(&installed, &selector).unwrap();
    // Always resume/clean up the fixture, including assertion failures.
    struct Cleanup {
        manager: Arc<RemoteManager>,
        helper: diri_engine::remote::manager::InstalledHelper,
        selector: SessionSelector,
        pid: libc::pid_t,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            // SAFETY: this is the live Holder created by this test.
            unsafe {
                libc::kill(self.pid, libc::SIGCONT);
            }
            let _ = self.manager.kill(&self.helper, &self.selector);
        }
    }
    let cleanup = Cleanup {
        manager,
        helper: installed,
        selector,
        pid: inspection.holder_pid as libc::pid_t,
    };
    let socket = temporary.path().join("daemon.sock");
    let server = Arc::new(ControlServer::new(Arc::clone(&registry), &socket));
    let (mut attach, attach_server) = UnixStream::pair().unwrap();
    attach
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let attach_worker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || server.serve(attach_server).unwrap())
    };
    attach
        .write_all(b"{\"attach\":\"scroll-latency\"}\n")
        .unwrap();
    let mut codec = FrameCodec::new();
    let mut frames = std::collections::VecDeque::new();
    let mut wait_for_pong = |stream: &mut UnixStream| {
        loop {
            while let Some(frame) = frames.pop_front() {
                if frame == Frame::pong() {
                    return;
                }
            }
            let mut bytes = [0; 65536];
            let count = stream.read(&mut bytes).unwrap();
            assert_ne!(count, 0, "attach disconnected before Pong");
            frames.extend(codec.feed(&bytes[..count]).unwrap());
        }
    };
    attach
        .write_all(&FrameCodec::encode(&Frame::ping()).unwrap())
        .unwrap();
    wait_for_pong(&mut attach);
    let listener = server.bind().unwrap();
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        server.serve(stream).unwrap();
    });
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // SAFETY: stop only the disposable Holder, never a developer's session.
    assert_eq!(unsafe { libc::kill(cleanup.pid, libc::SIGSTOP) }, 0);
    let started = Instant::now();
    let pid = cleanup.pid;
    let resume = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        // SAFETY: Cleanup keeps this fixture process alive until join.
        unsafe {
            libc::kill(pid, libc::SIGCONT);
        }
    });
    let mut attach_input_elapsed = Duration::ZERO;
    for (id, method, params) in [
        (
            1,
            Method::SESSION_READ_SCROLLBACK_CELLS,
            json!({"sessionID":"scroll-latency", "firstRow":0, "maxRows":24}),
        ),
        (
            2,
            Method::HELLO,
            json!({"proto":WIRE_VERSION, "build":"test"}),
        ),
        (
            3,
            Method::SESSION_SEND_TEXT,
            json!({"sessionID":"scroll-latency", "text":"geons", "submit":true}),
        ),
        (
            4,
            Method::SESSION_READ_SCREEN,
            json!({"sessionID":"scroll-latency"}),
        ),
    ] {
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).unwrap();
        if id == 1 {
            // Give the history worker time to send its request to the stopped
            // Holder before testing contention from a second connection.
            std::thread::sleep(Duration::from_millis(50));
            let input_started = Instant::now();
            attach
                .write_all(&FrameCodec::encode(&Frame::input(b"pi".to_vec())).unwrap())
                .unwrap();
            attach
                .write_all(&FrameCodec::encode(&Frame::ping()).unwrap())
                .unwrap();
            wait_for_pong(&mut attach);
            attach_input_elapsed = input_started.elapsed();
        }
    }
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut timings = std::collections::HashMap::new();
    for _ in 0..4 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ControlMessage::Response { id, result } = serde_json::from_str(&line).unwrap() else {
            panic!("unexpected response")
        };
        assert!(result.is_ok(), "request {id}: {result:?}");
        if id == 1 {
            let history: diri_proto::ReadScrollbackCellsResult =
                serde_json::from_value(result.unwrap()).unwrap();
            assert_eq!(history.row_count, 24);
            assert!(
                history.live_start_row >= 24,
                "fixture must have real scrollback"
            );
        }
        timings.insert(id, started.elapsed());
    }
    resume.join().unwrap();
    wait_for_grid(
        registry.lock().unwrap().get("scroll-latency").unwrap(),
        "received:pigeons",
    );
    drop(reader);
    drop(stream);
    worker.join().unwrap();
    drop(attach);
    attach_worker.join().unwrap();
    eprintln!("attach input: {attach_input_elapsed:?}");
    assert!(
        attach_input_elapsed < Duration::from_millis(400),
        "terminal input waited behind history: {attach_input_elapsed:?}"
    );
    assert!(timings[&1] >= Duration::from_millis(750));
    eprintln!(
        "delayed scrollback: {:?}; hello: {:?}; input: {:?}; screen: {:?}",
        timings[&1], timings[&2], timings[&3], timings[&4]
    );
    for id in [2, 3, 4] {
        assert!(
            timings[&id] < Duration::from_millis(400),
            "request {id} waited behind remote scrollback: {:?}",
            timings[&id]
        );
    }
}

#[test]
fn engine_collects_remote_usage_without_a_node_or_holder() {
    use diri_proto::remote_pty::{TranscriptUsageDirectory, TranscriptUsageRequest};
    let temporary = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    let home = root.join("remote-home");
    let state = root.join("remote-state");
    let argv = root.join("argv.log");
    let project = home.join("account with 'quotes' $(no-shell)/projects/project");
    fs::create_dir_all(&project).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let date = diri_usage::transcripts::dashboard::date_label(now / 86_400);
    let event = serde_json::json!({"type":"assistant","timestamp":format!("{date}T00:00:00Z"),
        "requestId":"r1","message":{"id":"m1","model":"claude-sonnet","content":"PROMPT_MUST_STAY_REMOTE",
        "usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}});
    fs::write(project.join("one.jsonl"), format!("{event}\n{event}\n")).unwrap();
    let ssh = write_fake_ssh_with_argv_log(&root, &home, &state, &argv);
    let script = fs::read_to_string(&ssh).unwrap().replace(
        "#!/bin/sh\n",
        "#!/bin/sh\nunset CODEX_HOME CLAUDE_CONFIG_DIR ZDOTDIR\n",
    );
    fs::write(&ssh, script).unwrap();
    let manager = RemoteManager::new(
        ProcessExecutor::new(ssh),
        ArtifactCatalog::from_native_helper(Path::new(helper())).unwrap(),
        root.join("control"),
    )
    .unwrap();
    let host = HostEntry {
        id: "usage".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: None,
        node: None,
    };
    let request = TranscriptUsageRequest {
        profiles: vec![TranscriptUsageDirectory {
            provider: "claude".into(),
            config_home: "~/account with 'quotes' $(no-shell)".into(),
        }],
    };
    let first = manager.transcript_usage(&host, &request).unwrap();
    let second = manager.transcript_usage(&host, &request).unwrap();
    assert_eq!(first.buckets, second.buckets);
    assert_eq!(first.source_id, second.source_id);
    assert_eq!(first.buckets.len(), 1);
    assert_eq!(first.buckets[0].input, 100);
    assert_eq!(first.buckets[0].cache_read, 40);
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("PROMPT_MUST_STAY_REMOTE")
    );
    assert_eq!(fs::read_dir(state.join("sessions")).unwrap().count(), 0);
    let calls = fs::read_to_string(&argv).unwrap();
    assert!(calls.lines().all(|line| line.contains("<BatchMode=yes>")));
    assert!(
        calls
            .lines()
            .filter(|line| line.contains(" usage"))
            .all(|line| line.contains("<-T>"))
    );
    fs::write(&argv, "").unwrap();
    manager.ensure_helper(&host).unwrap();
    assert!(
        !fs::read_to_string(&argv).unwrap().contains("BatchMode=yes"),
        "background mode must not affect interactive connections"
    );
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
        .list_directories(
            &host,
            &DirectoryListRequest {
                path: "~".into(),
                mode: Default::default(),
            },
        )
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
fn persistence_probe_closes_the_shared_master_before_independent_checks() {
    let temporary = tempfile::tempdir().expect("temp");
    let remote_home = temporary.path().join("remote-home");
    let remote_state = temporary.path().join("remote-state");
    let argv_log = temporary.path().join("ssh-argv.log");
    fs::create_dir(&remote_home).expect("remote home");
    let fake_ssh =
        write_fake_ssh_with_argv_log(temporary.path(), &remote_home, &remote_state, &argv_log);
    let manager = RemoteManager::new(
        ProcessExecutor::new(fake_ssh),
        ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
        temporary.path().join("ssh-control"),
    )
    .expect("manager");
    let host = HostEntry {
        id: "persistence-fixture".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    fs::write(&argv_log, b"").expect("clear bootstrap calls");

    assert_eq!(
        manager
            .probe_persistence(&host, &installed)
            .expect("persistence"),
        PersistenceCapability::NativeDetach
    );

    let calls = fs::read_to_string(&argv_log).expect("SSH argv log");
    let mut lines = calls.lines();
    let teardown = lines.next().expect("control-master teardown");
    assert!(
        teardown.contains("<-O><exit>"),
        "first persistence action did not close the shared master: {calls}"
    );
    let probes = lines
        .filter(|line| line.contains(" persistence"))
        .collect::<Vec<_>>();
    assert_eq!(probes.len(), 3, "begin, check and cleanup: {calls}");
    assert!(probes.iter().all(|line| {
        line.contains("<ControlMaster=no>")
            && line.contains("<ControlPersist=no>")
            && line.contains("<ControlPath=none>")
    }));
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
            "printf '\\033[?1h\\033=ready>'; IFS= read -r first; printf '\\033[?1lfirst:%s\\nnext>' \"$first\"; IFS= read -r second; printf 'second:%s\\n' \"$second\"".into(),
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
    assert_eq!(
        session.keyboard_state(),
        Some(diri_proto::terminal_input::KeyboardState {
            enhancements: Some(0.try_into().unwrap()),
            application_cursor_keys: true,
            application_keypad: true
        })
    );
    session.write_input(b"alpha\n").expect("first input");
    wait_for_grid(&session, "next>");
    assert_eq!(
        session.keyboard_state(),
        Some(diri_proto::terminal_input::KeyboardState {
            enhancements: Some(0.try_into().unwrap()),
            application_cursor_keys: false,
            application_keypad: true
        })
    );

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
    assert_eq!(
        session.keyboard_state(),
        Some(diri_proto::terminal_input::KeyboardState {
            enhancements: Some(0.try_into().unwrap()),
            application_cursor_keys: false,
            application_keypad: true
        })
    );
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
    for key in "omega"
        .chars()
        .map(|ch| diri_proto::terminal_input::KeyEvent::character(ch.to_string()))
        .chain([diri_proto::terminal_input::KeyEvent::named(
            diri_proto::terminal_input::NamedKey::Enter,
        )])
    {
        let bytes = diri_proto::terminal_input::encode_action(
            &key,
            Default::default(),
            session.keyboard_state(),
            diri_proto::terminal_input::KeyAction::Press,
        )
        .unwrap();
        session
            .write_input(&bytes)
            .expect("mode-aware input after reconnect");
    }
    wait_until("remote exit", Duration::from_secs(5), || {
        session.view().exited
    });
    assert!(session.screen_lines().join("\n").contains("second:omega"));
    let _ = session.terminate(Duration::from_millis(100));
}

/// A remote emulator reset clears the Holder's terminal state without
/// replacing its PTY or process, and the reset survives detach and adoption
/// because the Holder, not the Engine, owns the emulator.
#[test]
fn engine_resets_a_remote_terminal_without_replacing_the_process() {
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
        id: "fixture-reset".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    let session_id = "engine-remote-reset".to_string();
    let token = SessionToken::new("fedcba9876543210fedcba9876543210").expect("token");
    let request = LaunchRequest {
        session_id: session_id.clone(),
        session_token: token.clone(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '\\033]0;remote-title\\007ready>'; IFS= read -r first; printf 'first:%s\\nnext>' \"$first\"; IFS= read -r second; printf 'second:%s\\n' \"$second\"".into(),
        ],
        cwd: "/".into(),
        environment: vec![EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
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
    let binding = bindings
        .load_all()
        .expect("load binding")
        .into_iter()
        .next()
        .expect("saved binding");
    let selector = || SessionSelector {
        session_id: session_id.clone(),
        session_token: token.clone(),
        expected_incarnation: Some(binding.session_incarnation.clone()),
    };
    let pid_before = match manager
        .inspect(&installed, &selector())
        .expect("inspect")
        .process_state
    {
        diri_proto::remote_pty::RemoteProcessState::Running { pid } => pid,
        state => panic!("unexpected process state: {state:?}"),
    };

    session
        .reset_terminal()
        .expect("the live Holder accepts a reset");
    wait_until(
        "the reset snapshot to arrive",
        Duration::from_secs(5),
        || !session.screen_lines().join("\n").contains("ready>"),
    );
    assert!(
        session
            .screen_lines()
            .iter()
            .all(|line| line.trim().is_empty()),
        "the visible grid is blank after a reset: {:?}",
        session.screen_lines()
    );
    assert!(
        matches!(
            manager.inspect(&installed, &selector()).expect("inspect").process_state,
            diri_proto::remote_pty::RemoteProcessState::Running { pid } if pid == pid_before
        ),
        "a reset never replaces the Agent process"
    );

    // The same child keeps reading from the same PTY after the reset.
    session.write_input(b"alpha\n").expect("input after reset");
    wait_for_grid(&session, "next>");
    let screen = session.screen_lines().join("\n");
    assert!(screen.contains("first:alpha"), "{screen}");
    assert!(
        !screen.contains("ready>"),
        "pre-reset content must not reappear: {screen}"
    );

    // Detach and adopt: the reconnect seed comes from the reset Holder.
    drop(session);
    let binding = bindings
        .load_all()
        .expect("reload binding after detach")
        .into_iter()
        .next()
        .expect("persisted binding");
    let helper = manager
        .existing_helper(&host, &binding.helper_build_id, binding.protocol)
        .expect("same build");
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
    wait_for_grid(&session, "next>");
    let screen = session.screen_lines().join("\n");
    assert!(
        !screen.contains("ready>"),
        "a reconnect must not resurrect pre-reset content: {screen}"
    );
    assert!(matches!(
        manager.inspect(&installed, &selector()).expect("inspect after adopt").process_state,
        diri_proto::remote_pty::RemoteProcessState::Running { pid } if pid == pid_before
    ));
    session.write_input(b"omega\n").expect("input after adopt");
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
    let birth = manager
        .inspect_process_identity(&installed, &selector)
        .expect("host-verified birth");
    assert_eq!(birth.pid(), launched.process_pid);
    assert_eq!(inspection.verified_child_identity(), Some(birth));
    assert_eq!(
        manager
            .inspect(&installed, &selector)
            .unwrap()
            .controller_epoch,
        inspection.controller_epoch,
        "lease-free facts cannot change control ownership"
    );
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
fn attach_ssh_disconnect_reconnects_without_replaying_unavailable_input() {
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
        temporary
            .path()
            .join("attach-interrupted.disconnected")
            .is_file()
            && session.view().remote_connection.is_some_and(|connection| {
                connection.state == diri_proto::RemoteConnectionState::Reconnecting
            })
    });
    // The child may have changed modes while disconnected. Reject without
    // queueing until a matching authoritative seed restores mode knowledge.
    assert_eq!(
        session
            .write_input(b"must-not-replay\n")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    fs::write(temporary.path().join("attach-interrupted.resume"), b"").unwrap();
    wait_until("validated reconnect seed", Duration::from_secs(5), || {
        session.view().remote_connection.is_some_and(|connection| {
            connection.state == diri_proto::RemoteConnectionState::Connected
        })
    });
    session
        .write_input(b"after-ssh-reconnect\n")
        .expect("input after validated seed");
    // Wait for the echo itself, not for the exit. `exited` flips when the
    // remote process is reaped, which can beat the last of its output through
    // the Holder, the frame queue and the terminal parser — so asserting the
    // screen right after it raced the flush and failed on a loaded CI runner
    // while passing locally.
    wait_until(
        "the newly admitted input to echo after reconnect",
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

#[test]
fn engine_terminate_uses_stop_result_after_controller_revocation() {
    let temporary = tempfile::tempdir().unwrap();
    let remote_home = temporary.path().join("remote-home");
    fs::create_dir(&remote_home).unwrap();
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(write_fake_ssh(
                temporary.path(),
                &remote_home,
                &temporary.path().join("remote-state"),
            )),
            ArtifactCatalog::from_native_helper(Path::new(helper())).unwrap(),
            temporary.path().join("ssh-control"),
        )
        .unwrap(),
    );
    let host = HostEntry {
        id: "stop-fixture".into(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).unwrap();
    let request = LaunchRequest {
        session_id: unique_stop_session_id("stop-facts"),
        session_token: token_for_retry(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "trap 'sleep 0.1; exit 42' TERM; printf ready; while :; do sleep 1; done".into(),
        ],
        cwd: "/".into(),
        environment: vec![],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let mut session = Session::spawn(
        SessionSpec {
            id: request.session_id.clone(),
            pty: PtySpec::new(request.argv.clone(), "/").size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temporary.path().join("logs"),
            holder: None,
            defer_launch: false,
            remote: Some(RemoteSessionSpec {
                manager,
                helper: installed,
                launch: request,
                host_id: host.id,
                binding_store: RemoteBindingStore::new(temporary.path().join("bindings")).unwrap(),
            }),
        },
        Arc::new(ManifestEngine::new(Vec::new())),
    )
    .unwrap();
    wait_for_grid(&session, "ready");
    let exit = session.terminate(Duration::ZERO).unwrap();
    assert_eq!(
        exit,
        diri_engine::Exit::Code(42),
        "the old controller cannot supply the stop channel's exit"
    );
    assert!(
        session.view().exited,
        "actual stop fact reaches the shared session projection"
    );
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

fn write_fake_ssh_with_argv_log(
    root: &Path,
    home: &Path,
    state: &Path,
    argv_log: &Path,
) -> std::path::PathBuf {
    let path = root.join("ssh-argv-log");
    write_executable_script(
        &path,
        &format!(
            "#!/bin/sh\nprintf '<%s>' \"$@\" >> '{}'\nprintf '\\n' >> '{}'\ncase \" $* \" in\n  *' -O exit '*) exit 0;;\nesac\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\nexec /bin/sh -c \"$last\"",
            argv_log.display(),
            argv_log.display(),
            home.display(),
            state.display(),
        ),
    );
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
            "#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\ncase \"$last\" in\n  *' attach'*)\n    if [ ! -e '{}' ]; then\n      : > '{}'\n      /bin/sh -c \"$last\" <&0 & bridge=$!\n      (sleep 0.2; kill \"$bridge\" 2>/dev/null || true) & killer=$!\n      wait \"$bridge\" || true\n      kill \"$killer\" 2>/dev/null || true\n      wait \"$killer\" 2>/dev/null || true\n      printf 'simulated interrupted attach\\n' >&2\n      : > '{}.disconnected'\n      exit 255\n    fi\n    while [ ! -e '{}.resume' ]; do sleep 0.01; done\n    ;;\nesac\nexec /bin/sh -c \"$last\"",
            home.display(),
            state.display(),
            marker.display(),
            marker.display(),
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
