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
        transport: Default::default(),
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
        keyboard_enhancements: false,
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
    // The Holder stays stopped until requests 2-4 are answered (or a cap
    // passes). Answers that waited behind the stopped Holder's scrollback
    // could only arrive after the resume, so the check is causal rather
    // than a latency budget a loaded runner can exceed.
    let (unblocked, answered) = std::sync::mpsc::channel::<()>();
    let resume = std::thread::spawn(move || {
        let _ = answered.recv_timeout(Duration::from_secs(3));
        let resumed = Instant::now();
        // SAFETY: Cleanup keeps this fixture process alive until join.
        unsafe {
            libc::kill(pid, libc::SIGCONT);
        }
        resumed
    });
    let mut attach_input_elapsed = Duration::ZERO;
    let mut sent = std::collections::HashMap::new();
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
        sent.insert(id, Instant::now());
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
        timings.insert(id, Instant::now());
        if [2, 3, 4].iter().all(|id| timings.contains_key(id)) {
            let _ = unblocked.send(());
        }
    }
    let resumed = resume.join().unwrap();
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
    let since = |id: u64| timings[&id].duration_since(sent[&id]);
    eprintln!(
        "scrollback: {:?}; hello: {:?}; input: {:?}; screen: {:?}; resumed after {:?}",
        since(1),
        since(2),
        since(3),
        since(4),
        resumed.duration_since(started)
    );
    assert!(
        timings[&1] > resumed,
        "scrollback must wait for the stopped Holder"
    );
    for id in [2, 3, 4] {
        assert!(
            timings[&id] < resumed,
            "request {id} waited behind remote scrollback: {:?}",
            since(id)
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
        transport: Default::default(),
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
    let commands = || fs::read_to_string(&argv).unwrap().lines().count();
    let before_warm = commands();
    let second = manager.transcript_usage(&host, &request).unwrap();
    // A warm host answers a poll with ONE SSH command: probe + scan fused.
    assert_eq!(commands() - before_warm, 1);
    let warm_call = fs::read_to_string(&argv).unwrap();
    let warm_call = warm_call.lines().last().unwrap();
    assert!(
        warm_call.contains(" probe --format=json </dev/null; exec ")
            && warm_call.ends_with(" usage'>"),
        "{warm_call}"
    );
    // A vanished Helper fails the fused probe closed: the full verified
    // bootstrap reinstalls it, and the result is unchanged.
    fs::rename(home.join(".cache/diri/bin"), home.join("moved-helper-bin")).unwrap();
    let before_reinstall = commands();
    let reinstalled = manager.transcript_usage(&host, &request).unwrap();
    assert!(commands() - before_reinstall > 2);
    assert_eq!(reinstalled.buckets, first.buckets);
    assert_eq!(reinstalled.source_id, first.source_id);
    let before_warm = commands();
    manager.transcript_usage(&host, &request).unwrap();
    assert_eq!(commands() - before_warm, 1);
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
        transport: Default::default(),
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
        transport: Default::default(),
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
        transport: Default::default(),
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
        keyboard_enhancements: false,
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
        transport: Default::default(),
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
        keyboard_enhancements: false,
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
        transport: Default::default(),
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
        keyboard_enhancements: false,
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
        transport: Default::default(),
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
        transport: Default::default(),
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
        transport: Default::default(),
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
        keyboard_enhancements: false,
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
        transport: Default::default(),
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
        keyboard_enhancements: false,
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

/// A remote program prints an inline image; the Holder keeps its pixels and
/// anchors the cells, and the Engine reads the pixels back in bounded chunks
/// over the same SSH protocol channel.
#[test]
fn engine_reads_remote_inline_images_from_a_capable_holder() {
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
        id: "fixture-images".into(),
        transport: Default::default(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    // 300×300 RGBA spans two 256 KiB chunks; a position-dependent pattern
    // proves the chunks are joined in order.
    let pixels: Vec<u8> = (0..300 * 300 * 4)
        .map(|index: usize| (index % 251) as u8)
        .collect();
    let image = temporary.path().join("pixels");
    fs::write(&image, &pixels).expect("pixels");
    let script = format!(
        "printf '\\033_Ga=T,f=32,s=300,v=300,i=4,q=2;'; base64 < '{}' | tr -d '\\n'; \
         printf '\\033\\\\ready>'; IFS= read -r line",
        image.display()
    );
    let session_id = unique_stop_session_id("engine-remote-images");
    let request = LaunchRequest {
        session_id: session_id.clone(),
        session_token: SessionToken::new("0123abcd0123abcd0123abcd0123abcd").expect("token"),
        argv: vec!["/bin/sh".into(), "-c".into(), script],
        cwd: "/".into(),
        environment: vec![
            EnvironmentVariable {
                name: "TERM".into(),
                value: "xterm-256color".into(),
            },
            EnvironmentVariable {
                name: "PATH".into(),
                value: "/usr/bin:/bin".into(),
            },
        ],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NativeDetach,
        keyboard_enhancements: false,
    };
    let bindings = RemoteBindingStore::new(temporary.path().join("bindings")).expect("bindings");
    let session = Session::spawn(
        SessionSpec {
            id: session_id.clone(),
            pty: PtySpec::new(request.argv.clone(), "/").size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temporary.path().join("logs"),
            holder: None,
            remote: Some(RemoteSessionSpec {
                manager: Arc::clone(&manager),
                helper: installed,
                launch: request,
                host_id: host.id.clone(),
                binding_store: bindings,
            }),
            defer_launch: false,
        },
        Arc::new(ManifestEngine::new(Vec::new())),
    )
    .expect("spawn remote Session");
    wait_until("the image and prompt", Duration::from_secs(10), || {
        session.screen_lines().join("\n").contains("ready>")
    });

    // The Holder's grid carries the anchors; a fresh signature reads a full
    // grid once any pending diff has been taken.
    let mut key = None;
    for _ in 0..2 {
        let mut signature = diri_engine::session::GridSignature::default();
        let grid = session
            .grid_update_if_changed(&mut signature)
            .expect("grid");
        key = key.or_else(|| {
            grid.changed_rows
                .iter()
                .flat_map(|row| &row.metadata.links)
                .find_map(|link| diri_proto::inline_image::ImageAnchor::parse(&link.uri))
                .map(|anchor| anchor.key)
        });
    }
    let key = key.expect("an image anchor in the remote grid");

    let client = session
        .remote_inline_images()
        .expect("the bootstrapped Holder negotiated inline-images-v1");
    let mut read = Vec::new();
    while read.len() < pixels.len() {
        let chunk = client
            .read_image(&key, read.len() as u64)
            .expect("read")
            .expect("stored on the Holder");
        assert_eq!(
            chunk.format,
            diri_proto::inline_image::InlineImageFormat::Rgba
        );
        assert_eq!((chunk.width, chunk.height), (300, 300));
        assert_eq!(chunk.total, pixels.len() as u64);
        assert!(chunk.data.len() <= diri_proto::remote_pty::MAX_IMAGE_CHUNK_BYTES);
        assert!(!chunk.data.is_empty());
        read.extend_from_slice(&chunk.data);
    }
    assert_eq!(read, pixels);
    assert_eq!(client.read_image("00000000.1", 0).expect("miss"), None);

    // Input still flows on the same channel after the reads.
    session.write_input(b"done\n").expect("input");
    wait_until("remote exit", Duration::from_secs(5), || {
        session.view().exited
    });
    let mut session = session;
    let _ = session.terminate(Duration::from_millis(100));
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

/// Launches `argv` as a remote session through the fake `ssh` and the real
/// bootstrapped Helper, attached by a real Engine `Session`.
fn spawn_remote_program(
    temporary: &Path,
    label: &str,
    argv: Vec<String>,
    one_way_delay: Option<Duration>,
) -> Session {
    let remote_home = temporary.join("remote-home");
    let remote_state = temporary.join("remote-state");
    fs::create_dir(&remote_home).expect("remote home");
    let fake_ssh = match one_way_delay {
        Some(delay) => write_delayed_fake_ssh(temporary, &remote_home, &remote_state, delay),
        None => write_fake_ssh(temporary, &remote_home, &remote_state),
    };
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(fake_ssh),
            ArtifactCatalog::from_native_helper(Path::new(helper())).expect("catalog"),
            temporary.join("ssh-control"),
        )
        .expect("manager"),
    );
    let host = HostEntry {
        id: format!("fixture-{label}"),
        transport: Default::default(),
        name: None,
        ssh: "fixture-host".into(),
        default_cwd: Some("/".into()),
        node: None,
    };
    let installed = manager.ensure_helper(&host).expect("bootstrap");
    let session_id = unique_stop_session_id(label);
    let request = LaunchRequest {
        session_id: session_id.clone(),
        session_token: SessionToken::new("0123abcd0123abcd0123abcd0123abcd").expect("token"),
        argv,
        cwd: "/".into(),
        environment: vec![
            EnvironmentVariable {
                name: "TERM".into(),
                value: "xterm-256color".into(),
            },
            EnvironmentVariable {
                name: "PATH".into(),
                value: "/usr/bin:/bin".into(),
            },
        ],
        cols: 100,
        rows: 30,
        persistence: PersistenceCapability::NativeDetach,
        keyboard_enhancements: false,
    };
    let bindings = RemoteBindingStore::new(temporary.join("bindings")).expect("bindings");
    Session::spawn(
        SessionSpec {
            id: session_id,
            pty: PtySpec::new(request.argv.clone(), "/").size(100, 30),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temporary.join("logs"),
            holder: None,
            remote: Some(RemoteSessionSpec {
                manager: Arc::clone(&manager),
                helper: installed,
                launch: request,
                host_id: host.id.clone(),
                binding_store: bindings,
            }),
            defer_launch: false,
        },
        Arc::new(ManifestEngine::new(Vec::new())),
    )
    .expect("spawn remote Session")
}

/// A remote program's queries are answered by its Holder, from the Holder's
/// own terminal, in the order they were asked: the `OSC 7501` support query
/// ahead of the primary device attributes that follow it. The Engine, which
/// used to answer `OSC 7501` a round trip later, answers nothing, so no reply
/// arrives twice.
#[test]
fn remote_program_queries_are_answered_once_in_stream_order() {
    // The support probe programs use (`OSC 7501`, then DA1) around queries
    // they block on, plus the color queries answered from the palette the
    // Engine pushed.
    let probe: &[u8] = b"\x1b]7501;?\x1b\\\x1b[6n\x1b[5n\x1b[?2004$p\x1b[>c\
        \x1b]11;?\x1b\\\x1b]4;1;?\x1b\\\x1b[?996n\x1b[>0q\x1b[c";
    let expected = [
        diri_terminal_state::PROGRAM_STATUS_QUERY_REPLY,
        b"\x1b[1;1R\x1b[0n\x1b[?2004;2$y\x1b[>0;2600;1c",
        b"\x1b]11;rgb:0a0a/0b0b/0c0c\x1b\\\x1b]4;1;rgb:c0c0/2020/3030\x1b\\\x1b[?997;1n",
        format!(
            "\x1bP>|diri {}\x1b\\",
            &diri_remote::BUILD_ID[..diri_remote::BUILD_ID.len().min(12)]
        )
        .as_bytes(),
        b"\x1b[?62;22c",
    ]
    .concat();
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
    let temporary = tempfile::tempdir().expect("temp");
    let probe_path = temporary.path().join("probe.bin");
    fs::write(&probe_path, probe).expect("probe");
    // Reads exactly the expected byte count (each read waits up to 2 s, so a
    // missing answer ends it short), then anything else that arrives within
    // a second, and prints both hex encoded.
    let script = format!(
        "printf 'ready>'; IFS= read -r go; clear; stty raw -echo min 0 time 20; cat '{}'; \
         dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'; printf '|extra:'; \
         stty min 0 time 10; dd bs=256 count=1 2>/dev/null | od -An -tx1 | tr -d ' \\n'; \
         printf '|done'; IFS= read -r line",
        probe_path.display(),
        expected.len()
    );
    let session = spawn_remote_program(
        temporary.path(),
        "query-replies",
        vec!["/bin/sh".into(), "-c".into(), script],
        None,
    );
    // Asked only once the Engine is attached and the output is live, so the
    // controller-attached path is the one under test.
    wait_for_grid(&session, "ready>");
    session.apply_terminal_palette(Some(diri_proto::TerminalPalette {
        appearance: diri_proto::TerminalAppearance::Dark,
        foreground: [0xee, 0xee, 0xee],
        background: [0x0a, 0x0b, 0x0c],
        cursor: [0xff, 0xff, 0xff],
        ansi: [[0xc0, 0x20, 0x30]; 16],
    }));
    session.write_input(b"go\n").expect("input");
    let mut screen = String::new();
    wait_until("the probe", Duration::from_secs(20), || {
        screen = session.screen_lines().concat();
        screen.retain(|character| !character.is_whitespace());
        screen.contains("|done")
    });
    assert_eq!(
        screen,
        format!("{}|extra:|done", hex(&expected)),
        "answers (hex) as the remote program read them"
    );
    session.write_input(b"\n").expect("input");
    let mut session = session;
    let _ = session.terminate(Duration::from_millis(100));
}

/// A remote program negotiates kitty keyboard flags exactly as a local one:
/// its Holder answers `CSI ? u` from its own emulator, ahead of DA1, and once
/// the program pushes flags the Engine encodes keys with them (Shift+Enter
/// becomes `CSI 13;2u`), from the flags the Holder published.
#[test]
fn a_remote_program_negotiates_kitty_keyboard_flags() {
    use diri_proto::terminal_input::{KeyAction, KeyEvent, Modifiers, NamedKey, encode_action};

    let query_replies: &[u8] = b"\x1b[?0u\x1b[?62;22c";
    let shift_enter: &[u8] = b"\x1b[13;2u";
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
    let temporary = tempfile::tempdir().expect("temp");
    let script = format!(
        "printf 'ready>'; IFS= read -r go; clear; stty raw -echo min 0 time 20; \
         printf '\\033[?u\\033[c'; dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'; \
         printf '\\033[>1u|pushed>'; dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'; \
         printf '|done'; stty min 1 time 0; IFS= read -r line",
        query_replies.len(),
        shift_enter.len()
    );
    let session = spawn_remote_program(
        temporary.path(),
        "kitty-keyboard",
        vec!["/bin/sh".into(), "-c".into(), script],
        None,
    );
    wait_for_grid(&session, "ready>");
    assert_eq!(
        session
            .keyboard_state()
            .and_then(|state| state.enhancements)
            .map(|flags| flags.bits()),
        Some(0),
        "a known zero before the program asks"
    );
    session.write_input(b"go\n").expect("input");
    wait_for_grid(&session, "pushed>");
    wait_until("the pushed flags", Duration::from_secs(10), || {
        session
            .keyboard_state()
            .and_then(|state| state.enhancements)
            .is_some_and(|flags| flags.bits() == 1)
    });
    let shift = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    let bytes = encode_action(
        &KeyEvent::named(NamedKey::Enter),
        shift,
        session.keyboard_state(),
        KeyAction::Press,
    )
    .expect("encode");
    assert_eq!(bytes, shift_enter);
    session.write_input(&bytes).expect("input");
    let mut screen = String::new();
    wait_until("the key", Duration::from_secs(20), || {
        screen = session.screen_lines().concat();
        screen.retain(|character| !character.is_whitespace());
        screen.contains("|done")
    });
    assert_eq!(
        screen,
        format!("{}|pushed>{}|done", hex(query_replies), hex(shift_enter)),
        "what the remote program read (hex)"
    );
    session.write_input(b"\r").expect("input");
    let mut session = session;
    let _ = session.terminate(Duration::from_millis(100));
}

/// Round trip of each terminal query a remote program makes, measured inside
/// the remote PTY: the time from writing the query to reading its whole
/// answer, or `timeout` after 1 s. Then the order in which an `OSC 7501`
/// support query and primary device attributes are answered when sent
/// together. A measurement, not a gate:
/// `cargo test -p diri-remote --test engine_remote_e2e measure_remote_query_round_trips -- --ignored --nocapture`
/// `DIRI_QUERY_RTT_DELAY_MS` adds that one-way delay to every fake `ssh`
/// channel, standing in for a WAN link.
#[test]
#[ignore = "measurement; prints per-query round trips"]
fn measure_remote_query_round_trips() {
    let delay = std::env::var("DIRI_QUERY_RTT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis);
    let temporary = tempfile::tempdir().expect("temp");
    let probe = temporary.path().join("probe.pl");
    fs::write(&probe, QUERY_PROBE).expect("probe");
    let session = spawn_remote_program(
        temporary.path(),
        "query-rtt",
        vec!["/usr/bin/perl".into(), probe.display().to_string()],
        delay,
    );
    println!("one-way ssh delay: {delay:?}");
    // The probe waits for the Engine to attach and push the App's palette.
    wait_for_grid(&session, "ready>");
    session.apply_terminal_palette(Some(diri_proto::TerminalPalette {
        appearance: diri_proto::TerminalAppearance::Dark,
        foreground: [0xee, 0xee, 0xee],
        background: [0x0a, 0x0b, 0x0c],
        cursor: [0xff, 0xff, 0xff],
        ansi: [[0xc0, 0x20, 0x30]; 16],
    }));
    session.write_input(b"go\n").expect("input");
    wait_until("the probe", Duration::from_secs(60), || {
        session.screen_lines().join("\n").contains("PROBE-DONE")
    });
    for line in session.screen_lines() {
        let line = line.trim_end();
        if line.starts_with("RTT") || line.starts_with("ORDER") {
            println!("{line}");
        }
    }
    session.write_input(b"\n").expect("input");
    let mut session = session;
    let _ = session.terminate(Duration::from_millis(100));
}

/// Writes each query, waits up to 1 s for an answer ending in its final
/// byte, and prints `RTT <name> <ms|timeout> <median of 20>`.
const QUERY_PROBE: &str = r#"
use strict; use Time::HiRes qw(time sleep);
print "ready>"; my $go = <STDIN>;
system("stty raw -echo");
$| = 1;
sub collect {
    my ($until, $deadline) = @_;
    my $buffer = "";
    while (1) {
        my $left = $deadline - time;
        last if $left <= 0;
        my $bits = ""; vec($bits, fileno(STDIN), 1) = 1;
        if (select($bits, undef, undef, $left) > 0) {
            my $chunk; last unless sysread(STDIN, $chunk, 4096);
            $buffer .= $chunk;
            last if $until->($buffer);
        }
    }
    return $buffer;
}
my @queries = (
    ["CPR", "\e[6n", qr/\e\[\d+;\d+R$/],
    ["DSR", "\e[5n", qr/\e\[0n$/],
    ["DA1", "\e[c", qr/\e\[\?[\d;]*c$/],
    ["DA2", "\e[>c", qr/\e\[>[\d;]*c$/],
    ["DECRQM-2004", "\e[?2004\$p", qr/\e\[\?2004;\d\$y$/],
    ["XTWINOPS-18t", "\e[18t", qr/\e\[8;\d+;\d+t$/],
    ["OSC-11", "\e]11;?\e\\", qr/\e\]11;rgb:[0-9a-f\/]+\e\\$/],
    ["OSC-4", "\e]4;1;?\e\\", qr/\e\]4;1;rgb:[0-9a-f\/]+\e\\$/],
    ["CSI-?996n", "\e[?996n", qr/\e\[\?997;\dn$/],
    ["XTVERSION", "\e[>0q", qr/\eP>\|[^\e]*\e\\$/],
    ["OSC-7501", "\e]7501;?\e\\", qr/\e\]7501;\?\e\\$/],
    ["kitty-?u", "\e[?u", qr/\e\[\?\d+u$/],
);
for my $query (@queries) {
    my ($name, $bytes, $pattern) = @$query;
    my @samples;
    for my $round (1 .. 20) {
        my $start = time;
        syswrite(STDOUT, $bytes);
        my $answer = collect(sub { $_[0] =~ $pattern }, $start + 1.0);
        if ($answer =~ $pattern) { push @samples, (time - $start) * 1000 }
        else { @samples = (); last }
        # A late answer must not land in the next measurement.
        collect(sub { 0 }, time + 0.02);
    }
    if (@samples) {
        my @sorted = sort { $a <=> $b } @samples;
        printf "RTT %-13s median %.2f ms  p90 %.2f ms\r\n", $name,
            $sorted[int(@sorted / 2)], $sorted[int(@sorted * 0.9)];
    } else {
        printf "RTT %-13s timeout (no answer within 1000 ms)\r\n", $name;
    }
}
my $start = time;
syswrite(STDOUT, "\e]7501;?\e\\\e[c");
my $both = collect(sub { $_[0] =~ /7501/ && $_[0] =~ /\e\[\?[\d;]*c/ }, $start + 1.0);
my $status = index($both, "7501");
my $attributes = index($both, "\e[?");
my $order = $status < 0 ? "no 7501 answer within 1000 ms"
    : $status < $attributes ? "7501 answer before DA1 (support detected)"
    : "DA1 answer before 7501 (support missed)";
printf "ORDER %s\r\nPROBE-DONE\r\n", $order;
my $line = <STDIN>;
"#;

/// The fake `ssh`, with every channel relayed through a process that holds
/// each chunk for `delay` in both directions.
fn write_delayed_fake_ssh(
    root: &Path,
    home: &Path,
    state: &Path,
    delay: Duration,
) -> std::path::PathBuf {
    let relay = root.join("relay.pl");
    fs::write(&relay, DELAY_RELAY).expect("relay");
    let path = root.join("ssh");
    write_executable_script(
        &path,
        &format!(
            "#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\nexec /usr/bin/perl '{}' {} \"$last\"\n",
            home.display(),
            state.display(),
            relay.display(),
            delay.as_secs_f64(),
        ),
    );
    path
}

/// Runs `sh -c <command>` and forwards stdin to it and its stdout back, each
/// chunk released `delay` seconds after it was read, in order.
const DELAY_RELAY: &str = r#"
use strict; use IPC::Open2; use Time::HiRes qw(time sleep);
my ($delay, $command) = @ARGV;
my $pid = open2(my $from_child, my $to_child, "/bin/sh", "-c", $command);
binmode $_ for (\*STDIN, \*STDOUT, $from_child, $to_child);
my @queue; # [release time, handle, bytes]
my %open = (in => 1, out => 1);
while ($open{out} || @queue) {
    my $bits = "";
    vec($bits, fileno(STDIN), 1) = 1 if $open{in};
    vec($bits, fileno($from_child), 1) = 1 if $open{out};
    my $wait = @queue ? $queue[0][0] - time : undef;
    $wait = 0 if defined $wait && $wait < 0;
    if (select(my $ready = $bits, undef, undef, $wait) > 0) {
        for ([\*STDIN, $to_child, "in"], [$from_child, \*STDOUT, "out"]) {
            my ($source, $sink, $name) = @$_;
            next unless $open{$name} && vec($ready, fileno($source), 1);
            my $bytes;
            if (sysread($source, $bytes, 65536)) { push @queue, [time + $delay, $sink, $bytes] }
            else { $open{$name} = 0; push @queue, [time + $delay, $sink, undef] }
        }
    }
    while (@queue && $queue[0][0] <= time) {
        my (undef, $sink, $bytes) = @{shift @queue};
        if (defined $bytes) { syswrite($sink, $bytes) } else { close $sink }
    }
}
waitpid($pid, 0);
exit($? >> 8);
"#;

/// A destructive repaint (an erase, then its redraw a few milliseconds later
/// in a separate write) never reaches a local Engine attach as an erased
/// frame, whether it answers a keystroke or nobody typed for it.
#[test]
fn a_remote_repaint_is_published_with_its_redraw() {
    let flashes = remote_repaint_flashes(Duration::ZERO, 5, &[(true, 5), (false, 5)]);
    assert_eq!(
        flashes,
        [0, 0],
        "erased frames per (keystroke, background) repaint"
    );
}

/// Whether a remote destructive repaint (an erase, then its redraw a few
/// milliseconds later in a separate write) ever reaches a local Engine
/// attach as an erased frame, over a fake `ssh` that holds every chunk for
/// `DIRI_REPAINT_DELAY_MS` (default 25) in each direction.
///
/// Two triggers: a keystroke sent through the Engine (the program repaints
/// in answer to it, so the Holder's interactive budget applies), and output
/// nobody typed for, written to the child's TTY. For each gap between the
/// erase and the redraw, it prints how many repaints showed a frame with
/// fewer rows than the screen held before and after.
///
/// ```sh
/// cargo test --release -p diri-remote --test engine_remote_e2e \
///     measure_remote_destructive_repaint -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement: run with --release -- --ignored --nocapture"]
fn measure_remote_destructive_repaint() {
    let delay = Duration::from_millis(
        std::env::var("DIRI_REPAINT_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(25),
    );
    let trials: usize = std::env::var("DIRI_REPAINT_TRIALS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    let cases: Vec<(bool, u64)> = [true, false]
        .into_iter()
        .flat_map(|typed| [0, 2, 5, 10, 14, 20].map(|gap| (typed, gap)))
        .collect();
    eprintln!("one-way ssh delay {delay:?}, {trials} repaints per gap");
    let flashes = remote_repaint_flashes(delay, trials, &cases);
    for ((typed, gap), flashed) in cases.iter().zip(flashes) {
        let trigger = if *typed { "keystroke " } else { "background" };
        eprintln!("REPAINT {trigger} gap {gap:>2} ms: {flashed}/{trials} showed an erased frame");
    }
}

/// Runs `trials` repaints per `(typed, gap ms)` case in a remote session
/// behind a fake `ssh` with `delay` each way and counts, per case, the
/// repaints whose erase reached an Engine attach before its redraw.
fn remote_repaint_flashes(delay: Duration, trials: usize, cases: &[(bool, u64)]) -> Vec<usize> {
    use diri_engine::control::ControlServer;
    use diri_engine::registry::Registry;
    use diri_proto::frames::{FrameCodec, FrameType};
    use serde_json::json;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;

    const ROWS: usize = 20;
    let temporary = tempfile::tempdir_in("/tmp").expect("temp");
    let home = temporary.path().join("home");
    fs::create_dir(&home).unwrap();
    let script = temporary.path().join("repaint.pl");
    let tty_file = temporary.path().join("tty");
    // Each input line is "<letter> <gap ms>": erase the screen, wait the
    // gap, redraw every row with the letter.
    fs::write(
        &script,
        format!(
            r#"use strict; use Time::HiRes qw(sleep); $| = 1;
system("stty -echo -icanon min 1; tty > '{tty}'");
print "ready\r\n";
while (my $line = <STDIN>) {{
    my ($letter, $gap) = split ' ', $line;
    syswrite(STDOUT, "\e[H\e[2J");
    sleep($gap / 1000) if $gap > 0;
    syswrite(STDOUT, "\e[H" . join("\r\n", map {{ $letter x 60 }} 1 .. {ROWS}));
}}
"#,
            tty = tty_file.display()
        ),
    )
    .unwrap();
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(write_delayed_fake_ssh(
                temporary.path(),
                &home,
                &temporary.path().join("remote-state"),
                delay,
            )),
            ArtifactCatalog::from_native_helper(Path::new(helper())).unwrap(),
            temporary.path().join("control"),
        )
        .unwrap(),
    );
    let host = HostEntry {
        transport: Default::default(),
        id: "repaint".into(),
        name: None,
        ssh: "fixture".into(),
        default_cwd: None,
        node: None,
    };
    let installed = manager.ensure_helper(&host).unwrap();
    let token = token_for_retry();
    let id = unique_stop_session_id("repaint");
    let request = LaunchRequest {
        session_id: id.clone(),
        session_token: token.clone(),
        argv: vec!["/usr/bin/perl".into(), script.display().to_string()],
        cwd: "/".into(),
        environment: vec![],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NativeDetach,
        keyboard_enhancements: false,
    };
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(ManifestEngine::new(Vec::new())),
        temporary.path().join("state.json"),
    )));
    let record = serde_json::from_value(json!({
        "id":id, "kind":{"shell":{}}, "cwd":"/", "projectID":"p",
        "title":"repaint", "titleSource":0, "status":{"starting":{}},
        "resumability":"notResumable", "createdAt":0, "updatedAt":0, "pinned":false,
        "host":host.id,
    }))
    .unwrap();
    registry
        .lock()
        .unwrap()
        .spawn(
            SessionSpec {
                id: id.clone(),
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
    wait_for_grid(registry.lock().unwrap().get(&id).unwrap(), "ready");
    struct Cleanup {
        manager: Arc<RemoteManager>,
        helper: diri_engine::remote::manager::InstalledHelper,
        selector: SessionSelector,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.manager.kill(&self.helper, &self.selector);
        }
    }
    let _cleanup = Cleanup {
        manager: Arc::clone(&manager),
        helper: installed,
        selector: SessionSelector {
            session_id: id.clone(),
            session_token: token,
            expected_incarnation: None,
        },
    };

    let server = Arc::new(ControlServer::new(
        Arc::clone(&registry),
        temporary.path().join("daemon.sock"),
    ));
    let (mut attach, attach_server) = UnixStream::pair().unwrap();
    let attach_worker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let _ = server.serve(attach_server);
        })
    };
    attach
        .write_all(format!("{{\"attach\":\"{id}\"}}\n").as_bytes())
        .unwrap();
    // Folds every Grid frame the attach receives into a mirror of the first
    // ROWS rows and reports each frame's count of filled rows and its letter.
    let (sender, frames) = std::sync::mpsc::channel::<(usize, char)>();
    {
        let mut attach = attach.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut codec = FrameCodec::new();
            let mut bytes = vec![0; 1 << 16];
            let mut mirror = vec![String::new(); ROWS];
            while let Ok(count) = attach.read(&mut bytes) {
                if count == 0 {
                    return;
                }
                let Ok(decoded) = codec.feed(&bytes[..count]) else {
                    return;
                };
                for frame in decoded {
                    if frame.frame_type != FrameType::Grid {
                        continue;
                    }
                    let Ok(Some(update)) = frame.grid_payload() else {
                        continue;
                    };
                    if update.is_full_snapshot {
                        mirror.iter_mut().for_each(String::clear);
                    }
                    for row in &update.changed_rows {
                        if let Some(slot) = mirror.get_mut(usize::from(row.y)) {
                            *slot = row
                                .cells
                                .iter()
                                .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                                .collect::<String>()
                                .trim()
                                .to_string();
                        }
                    }
                    let filled = mirror.iter().filter(|row| row.len() >= 60).count();
                    let letter = mirror[ROWS - 1].chars().next().unwrap_or(' ');
                    if sender.send((filled, letter)).is_err() {
                        return;
                    }
                }
            }
        });
    }
    let session_input = |bytes: &[u8]| {
        registry
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .write_input(bytes)
            .unwrap();
    };
    // Waits for the redraw with `letter`, returning whether any frame before
    // it showed fewer than every row.
    let repaint = |letter: char| -> bool {
        let mut erased = false;
        loop {
            let (filled, shown) = frames
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("timed out waiting for the {letter} redraw"));
            if filled == ROWS && shown == letter {
                return erased;
            }
            erased |= filled < ROWS;
        }
    };
    session_input(b"a 0\n");
    repaint('a');
    std::thread::sleep(Duration::from_millis(200));
    while frames.try_recv().is_ok() {}
    let tty = fs::read_to_string(&tty_file).unwrap().trim().to_string();
    let mut terminal = fs::OpenOptions::new().write(true).open(&tty).unwrap();
    let mut flashes = Vec::with_capacity(cases.len());
    for &(typed, gap) in cases {
        let mut flashed = 0;
        for trial in 0..trials {
            let letter = char::from(b'b' + (trial % 24) as u8);
            if typed {
                session_input(format!("{letter} {gap}\n").as_bytes());
            } else {
                terminal.write_all(b"\x1b[H\x1b[2J").unwrap();
                std::thread::sleep(Duration::from_millis(gap));
                let rows = vec![letter.to_string().repeat(60); ROWS].join("\r\n");
                write!(terminal, "\x1b[H{rows}").unwrap();
            }
            if repaint(letter) {
                flashed += 1;
            }
            std::thread::sleep(Duration::from_millis(60));
            while frames.try_recv().is_ok() {}
        }
        flashes.push(flashed);
    }
    let _ = attach.shutdown(std::net::Shutdown::Both);
    drop(attach_worker);
    flashes
}
