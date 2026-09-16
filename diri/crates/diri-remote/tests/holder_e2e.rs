#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_proto::frames::Frame;
use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, EnvironmentCaptureResult, Hello, LaunchRequest, LaunchResult,
    PersistenceCapability, PersistenceProbeAction, PersistenceProbeRequest, PersistenceProbeResult,
    ProtocolVersion, RemoteCapability, RemoteCodec, RemoteMessage, RemoteProcessState, RemoteRole,
    SessionInspection, SessionSelector, SessionToken,
};

fn helper() -> std::path::PathBuf {
    std::env::var_os("DIRI_REMOTE_HELPER_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_diri-remote").into())
}

fn log_tail(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::FileExt;
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut tail = 0;
    // Observe committed offsets independently of the asynchronous session
    // checkpoint. Detailed checksum/torn-write recovery is tested in OutputLog.
    for slot in 0..512 {
        for commit in 0..2 {
            let mut header = [0_u8; 64];
            if file
                .read_exact_at(&mut header, 16 + slot * (65536 + 128) + commit * 64)
                .is_err()
            {
                continue;
            }
            let base = u64::from_be_bytes(header[..8].try_into().unwrap());
            let len = u32::from_be_bytes(header[8..12].try_into().unwrap());
            if header[12..16] == 0x4452_434d_u32.to_be_bytes() && len <= 65536 {
                tail = tail.max(base + u64::from(len));
            }
        }
    }
    tail
}

fn token() -> SessionToken {
    SessionToken::new("0123456789abcdef0123456789abcdef").expect("token")
}

fn run_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    command: &str,
    state_dir: &std::path::Path,
    request: Option<&T>,
) -> R {
    let output = run_output(command, state_dir, request);
    assert!(
        output.status.success(),
        "{command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON response")
}

fn run_output<T: serde::Serialize>(
    command: &str,
    state_dir: &std::path::Path,
    request: Option<&T>,
) -> std::process::Output {
    let fixture_home = state_dir
        .parent()
        .expect("state parent")
        .join("fixture-home");
    std::fs::create_dir_all(&fixture_home).expect("fixture home");
    let mut child = Command::new(helper())
        .arg(command)
        .env("DIRI_REMOTE_STATE_DIR", state_dir)
        .env("HOME", fixture_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper command");
    if let Some(request) = request {
        serde_json::to_writer(child.stdin.as_mut().expect("stdin"), request)
            .expect("write request");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("wait helper command")
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestGcResult {
    removed_sessions: usize,
    retained_sessions: usize,
    removed_helper_builds: usize,
    retained_helper_builds: usize,
}

type TestWriter = Box<dyn Write + Send>;
type TestReader = Box<dyn Read + Send>;

struct Attach {
    child: Child,
    input: Box<dyn Write + Send>,
    messages: Receiver<Result<RemoteMessage, String>>,
    diagnostics: std::path::PathBuf,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Attach {
    fn open(state_dir: &std::path::Path, hello: Hello) -> Self {
        Self::open_impl(state_dir, hello, false)
    }

    fn open_impl(state_dir: &std::path::Path, hello: Hello, tcp: bool) -> Self {
        let session_id = hello.session_id.clone();
        let mut child = Command::new(helper())
            .arg("attach")
            .env("DIRI_REMOTE_STATE_DIR", state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn attach");
        let input = child.stdin.take().expect("attach stdin");
        let output = child.stdout.take().expect("attach stdout");
        let (mut input, mut output): (TestWriter, TestReader) = if tcp {
            tcp_bridge(input, output)
        } else {
            (Box::new(input), Box::new(output))
        };
        let mut stderr = child.stderr.take().expect("attach stderr");
        let captured_stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink = Arc::clone(&captured_stderr);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            *stderr_sink.lock().expect("stderr lock") = bytes;
        });
        let hello = RemoteCodec::encode(&RemoteMessage::Hello(hello)).expect("encode Hello");
        input.write_all(&hello).expect("write Hello");
        input.flush().expect("flush Hello");

        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut codec = RemoteCodec::new();
            let mut bytes = [0_u8; 64 * 1024];
            loop {
                match output.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => match codec.feed(&bytes[..count]) {
                        Ok(decoded) => {
                            for message in decoded {
                                if sender.send(Ok(message)).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error.to_string()));
                            return;
                        }
                    },
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        return;
                    }
                }
            }
        });
        Self {
            child,
            input,
            messages,
            diagnostics: state_dir.join(format!("sessions/{session_id}/holder.log")),
            stderr: captured_stderr,
        }
    }

    fn send(&mut self, message: RemoteMessage) {
        self.try_send(message).expect("write message");
    }

    fn try_send(&mut self, message: RemoteMessage) -> std::io::Result<()> {
        let encoded = RemoteCodec::encode(&message).expect("encode message");
        self.input.write_all(&encoded)?;
        self.input.flush()
    }

    fn receive_until(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&RemoteMessage) -> bool,
    ) -> Vec<RemoteMessage> {
        let deadline = Instant::now() + timeout;
        let mut received = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(remaining) {
                Ok(message) => message,
                Err(error) => {
                    let diagnostics = std::fs::read_to_string(&self.diagnostics)
                        .unwrap_or_else(|read_error| format!("<unreadable: {read_error}>"));
                    let state = std::fs::read_to_string(
                        self.diagnostics.parent().expect("session dir").join("session.json"),
                    )
                    .unwrap_or_else(|read_error| format!("<unreadable: {read_error}>"));
                    let status = self.child.try_wait().expect("attach status");
                    let stderr = if status.is_some() {
                        String::from_utf8_lossy(&self.stderr.lock().expect("stderr lock")).into_owned()
                    } else {
                        "<attach still running>".into()
                    };
                    panic!(
                        "attach receive timeout/disconnect: {error}; seen={}; status={status:?}; stderr={}; Holder: {diagnostics}; state={state}",
                        message_kinds(&received), stderr
                    )
                }
            }
            .unwrap_or_else(|error| panic!("attach decode: {error}"));
            let matched = predicate(&message);
            received.push(message);
            if matched {
                return received;
            }
        }
    }
}

fn message_kinds(messages: &[RemoteMessage]) -> String {
    messages
        .iter()
        .map(|message| match message {
            RemoteMessage::Terminal(frame) => match frame.frame_type {
                diri_proto::frames::FrameType::Output => "Output",
                diri_proto::frames::FrameType::ReplayBegin => "ReplayBegin",
                diri_proto::frames::FrameType::ReplayEnd => "ReplayEnd",
                _ => "Terminal",
            },
            RemoteMessage::Hello(_) => "Hello",
            RemoteMessage::HelloAck(_) => "HelloAck",
            RemoteMessage::FullSnapshot(_) => "FullSnapshot",
            RemoteMessage::GridDelta(_) => "GridDelta",
            RemoteMessage::ProcessExit(_) => "ProcessExit",
            RemoteMessage::Signal(_) => "Signal",
            RemoteMessage::StopSession(_) => "StopSession",
            RemoteMessage::AcquireControl(_) => "AcquireControl",
            RemoteMessage::ControlGranted(_) => "ControlGranted",
            RemoteMessage::ControlRevoked(_) => "ControlRevoked",
            RemoteMessage::ReleaseControl(_) => "ReleaseControl",
            RemoteMessage::ScrollbackRequest(_) => "ScrollbackRequest",
            RemoteMessage::ScrollbackResponse(_) => "ScrollbackResponse",
            RemoteMessage::Error(_) => "Error",
            RemoteMessage::ForegroundProcess(_) => "ForegroundProcess",
            RemoteMessage::InputModes(_) => "InputModes",
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl Drop for Attach {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn hello(launch: &LaunchResult, acknowledged: Option<u64>, nonce: &str) -> Hello {
    Hello {
        protocol: ProtocolVersion::CURRENT,
        local_build_id: "e2e-client".into(),
        session_id: launch.session_id.clone(),
        session_token: token(),
        expected_incarnation: Some(launch.session_incarnation.clone()),
        requested_role: RemoteRole::Controller,
        client_nonce: nonce.into(),
        required_capabilities: vec![
            RemoteCapability::FullSnapshot,
            RemoteCapability::IncrementalGrid,
            RemoteCapability::ControllerLease,
        ],
        last_acknowledged_output_offset: acknowledged,
        last_acknowledged_grid_sequence: None,
    }
}

#[test]
fn detach_reconnect_preserves_pid_snapshot_and_input() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-e2e".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'ready>'; IFS= read -r first; printf 'got:%s\\nnext>' \"$first\"; IFS= read -r second; printf 'bye:%s\\n' \"$second\"".into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
    assert_ne!(launch.holder_pid, launch.process_pid);

    let mut old_hello = hello(&launch, None, "old-client");
    old_hello.protocol.minor = 9;
    let mut old = Attach::open(&state_dir, old_hello);
    let old_handshake = old.receive_until(Duration::from_secs(2), |message| {
        matches!(message, RemoteMessage::FullSnapshot(_))
    });
    assert!(old_handshake.iter().any(|message| matches!(message,
        RemoteMessage::HelloAck(ack) if ack.child_identity.is_none())));
    drop(old);
    let mut first = Attach::open(&state_dir, hello(&launch, Some(0), "client-one"));
    let initial = first.receive_until(Duration::from_secs(2), |message| match message {
        RemoteMessage::FullSnapshot(snapshot) => grid_text(&snapshot.grid).contains("ready>"),
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("ready>"),
        _ => false,
    });
    let first_birth = initial
        .iter()
        .find_map(|message| match message {
            RemoteMessage::HelloAck(ack) => ack.child_identity,
            _ => None,
        })
        .expect("owned-child birth from capable Holder");
    assert_eq!(first_birth.pid(), launch.process_pid);
    let first_epoch = initial
        .iter()
        .find_map(|message| match message {
            RemoteMessage::HelloAck(ack) => Some(ack.controller_epoch),
            _ => None,
        })
        .expect("HelloAck");
    first.send(RemoteMessage::Terminal(Frame::input(b"alpha\n".to_vec())));
    let output = first.receive_until(Duration::from_secs(2), |message| match message {
        RemoteMessage::Terminal(frame) => frame
            .output_payload()
            .is_some_and(|(_, bytes)| bytes.windows(9).any(|window| window == b"got:alpha")),
        _ => false,
    });
    let acknowledged = output
        .iter()
        .filter_map(|message| match message {
            RemoteMessage::Terminal(frame) => frame
                .output_payload()
                .map(|(offset, bytes)| offset + bytes.len() as u64),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(100));
    let inspection: SessionInspection = run_json(
        "inspect",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation.clone()),
        }),
    );
    assert_eq!(
        inspection.process_state,
        RemoteProcessState::Running {
            pid: launch.process_pid
        }
    );

    assert_eq!(inspection.verified_child_identity(), Some(first_birth));
    assert_eq!(
        inspection.controller_epoch, first_epoch,
        "inspection must not acquire control"
    );

    let mut second = Attach::open(&state_dir, hello(&launch, Some(acknowledged), "client-two"));
    let reconnected = second.receive_until(Duration::from_secs(2), |message| {
        matches!(message, RemoteMessage::FullSnapshot(_))
    });
    let second_ack = reconnected
        .iter()
        .find_map(|message| match message {
            RemoteMessage::HelloAck(ack) => Some(ack),
            _ => None,
        })
        .expect("second HelloAck");
    assert!(second_ack.controller_epoch > first_epoch);
    assert_eq!(second_ack.child_identity, Some(first_birth));
    assert_eq!(
        second_ack.process_state,
        RemoteProcessState::Running {
            pid: launch.process_pid
        }
    );
    let snapshot = reconnected
        .iter()
        .find_map(|message| match message {
            RemoteMessage::FullSnapshot(snapshot) => Some(snapshot),
            _ => None,
        })
        .expect("reconnect snapshot");
    assert!(grid_text(&snapshot.grid).contains("next>"));

    first.receive_until(Duration::from_secs(2), |message| {
        matches!(
            message,
            RemoteMessage::ControlRevoked(revoked) if revoked.controller_epoch == first_epoch
        )
    });

    // The second authenticated attach increments the epoch and drops the old
    // UDS before any further controller input is considered.
    let _ = first.try_send(RemoteMessage::Terminal(Frame::input(b"stale\n".to_vec())));
    second.send(RemoteMessage::Terminal(Frame::input(b"omega\n".to_vec())));
    let finished = second.receive_until(Duration::from_secs(2), |message| {
        matches!(message, RemoteMessage::ProcessExit(_))
    });
    let final_output = finished
        .iter()
        .filter_map(|message| match message {
            RemoteMessage::Terminal(frame) => frame.output_payload().map(|(_, bytes)| bytes),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    assert!(String::from_utf8_lossy(&final_output).contains("bye:omega"));
    assert!(!String::from_utf8_lossy(&final_output).contains("bye:stale"));
    drop(first);

    let killed: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
    assert!(matches!(
        killed.process_state,
        RemoteProcessState::Exited { .. }
    ));
}

#[test]
fn detached_foreground_job_does_not_spin_the_holder() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "foreground-detach".into(),
        session_token: token(),
        argv: vec![
            "/bin/bash".into(),
            "--norc".into(),
            "--noprofile".into(),
            "-i".into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "PATH".into(),
            value: "/usr/bin:/bin".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
    struct Cleanup<'a>(&'a std::path::Path, SessionSelector);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _: SessionInspection = run_json("kill", self.0, Some(&self.1));
        }
    }
    let _cleanup = Cleanup(
        &state_dir,
        SessionSelector {
            session_id: launch.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation.clone()),
        },
    );
    let mut attach = Attach::open(&state_dir, hello(&launch, None, "foreground-client"));
    attach.receive_until(Duration::from_secs(2), |message| {
        matches!(message, RemoteMessage::FullSnapshot(_))
    });
    attach.send(RemoteMessage::Terminal(Frame::input(
        b"sleep 30\n".to_vec(),
    )));
    attach.receive_until(Duration::from_secs(3), |message| {
        matches!(message, RemoteMessage::ForegroundProcess(foreground)
            if foreground.pid.is_some_and(|pid| pid > 0 && pid != launch.process_pid as i32))
    });
    drop(attach);

    // Let the one-second job probe expire after the Bridge has disconnected.
    // A stale deadline makes poll(0) spin even though sleep produces no output.
    std::thread::sleep(Duration::from_millis(1200));
    let before = diri_engine::governor::cpu_time_of(&[launch.holder_pid as i32]);
    std::thread::sleep(Duration::from_secs(1));
    let cpu =
        diri_engine::governor::cpu_time_of(&[launch.holder_pid as i32]).saturating_sub(before);
    let mut reattached = Attach::open(&state_dir, hello(&launch, None, "foreground-reconnect"));
    let messages = reattached.receive_until(Duration::from_secs(2), |message| {
        matches!(message, RemoteMessage::FullSnapshot(_))
    });
    assert!(messages.iter().any(|message| matches!(message,
        RemoteMessage::HelloAck(ack)
            if ack.process_state == (RemoteProcessState::Running { pid: launch.process_pid })
                && ack.foreground_pid.is_some_and(|pid| pid != launch.process_pid as i32)
    )));
    assert!(
        cpu < 100_000_000,
        "detached Holder used {cpu} ns CPU in one second"
    );
}

#[test]
fn holder_agent_has_a_real_editable_resizable_terminal() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-terminal-capabilities".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "stty erase '^?'; trap 'set -- $(stty size); printf \"winch=%sx%s\\n\" \"$1\" \"$2\"' WINCH; if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then printf 'isatty=yes\\n'; else printf 'isatty=no\\n'; fi; printf 'ready>'; IFS= read -r line; printf 'line=<%s>\\n' \"$line\"; while :; do sleep 1; done"
                .into(),
        ],
        cwd: "/".into(),
        environment: vec![
            diri_proto::remote_pty::EnvironmentVariable {
                name: "PATH".into(),
                value: "/usr/bin:/bin".into(),
            },
            diri_proto::remote_pty::EnvironmentVariable {
                name: "TERM".into(),
                value: "xterm-256color".into(),
            },
        ],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
    let mut attach = Attach::open(&state_dir, hello(&launch, Some(0), "terminal-capabilities"));

    attach.receive_until(Duration::from_secs(3), |message| {
        message_contains(message, "isatty=yes") && message_contains(message, "ready>")
    });
    attach.send(RemoteMessage::Terminal(Frame::input(
        b"abc\x7fd\n".to_vec(),
    )));
    attach.receive_until(Duration::from_secs(3), |message| {
        message_contains(message, "line=<abd>")
    });

    attach.send(RemoteMessage::Terminal(Frame::resize(100, 30)));
    attach.receive_until(Duration::from_secs(4), |message| {
        message_contains(message, "winch=30x100")
    });

    let killed: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
    assert!(matches!(
        killed.process_state,
        RemoteProcessState::Exited { .. }
    ));
}

#[test]
fn list_kill_and_gc_complete_the_session_lifecycle() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-gc".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'gc-ready>'; IFS= read -r _".into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launched: LaunchResult = run_json("launch", &state_dir, Some(&request));
    let session_root = state_dir.join("sessions/holder-gc");
    assert_eq!(
        std::fs::metadata(&session_root)
            .expect("session metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for name in ["session.json", "auth.sha256", "output.log", "holder.log"] {
        assert_eq!(
            std::fs::metadata(session_root.join(name))
                .expect("private file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{name} must be owner-only"
        );
    }
    let sessions: Vec<SessionInspection> = run_json::<(), _>("list", &state_dir, None);
    assert!(
        sessions
            .iter()
            .any(|session| session.session_id == launched.session_id)
    );
    let selector = SessionSelector {
        session_id: launched.session_id,
        session_token: token(),
        expected_incarnation: Some(launched.session_incarnation),
    };
    let _: SessionInspection = run_json("kill", &state_dir, Some(&selector));

    let fixture_home = state_dir
        .parent()
        .expect("state parent")
        .join("fixture-home");
    let stale_build = fixture_home.join(".cache/diri/bin/protocol-1/stale-build");
    std::fs::create_dir_all(&stale_build).expect("stale build dir");
    std::fs::write(stale_build.join("diri-remote"), b"stale").expect("stale helper");
    let gc: TestGcResult = run_json::<(), _>("gc", &state_dir, None);
    assert_eq!(gc.removed_sessions, 1);
    assert_eq!(gc.retained_sessions, 0);
    assert_eq!(gc.removed_helper_builds, 1);
    assert_eq!(gc.retained_helper_builds, 0);
    let sessions: Vec<SessionInspection> = run_json::<(), _>("list", &state_dir, None);
    assert!(sessions.is_empty());
    assert!(!stale_build.exists());
}

#[test]
fn killed_session_id_can_launch_a_new_authenticated_incarnation() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let first_request = LaunchRequest {
        session_id: "holder-relaunch".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'old>'; IFS= read -r _".into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let first: LaunchResult = run_json("launch", &state_dir, Some(&first_request));
    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: first.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(first.session_incarnation.clone()),
        }),
    );

    let second_token = SessionToken::new("abcdef0123456789abcdef0123456789").expect("token");
    let mut second_request = first_request;
    second_request.session_token = second_token.clone();
    second_request.argv[2] = "printf 'new>'; IFS= read -r _".into();
    let second: LaunchResult = run_json("launch", &state_dir, Some(&second_request));
    assert_ne!(second.session_incarnation, first.session_incarnation);
    let mut second_hello = hello(&second, None, "new-incarnation");
    second_hello.session_token = second_token.clone();
    let mut attach = Attach::open(&state_dir, second_hello);
    attach.receive_until(Duration::from_secs(2), |message| match message {
        RemoteMessage::FullSnapshot(snapshot) => grid_text(&snapshot.grid).contains("new>"),
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("new>"),
        _ => false,
    });
    let auth = std::fs::read_to_string(state_dir.join("sessions/holder-relaunch/auth.sha256"))
        .expect("auth hash");
    assert!(!auth.contains(second_token.expose_secret()));
    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: second.session_id,
            session_token: second_token,
            expected_incarnation: Some(second.session_incarnation),
        }),
    );
}

#[test]
fn incompatible_protocol_and_wrong_incarnation_fail_with_structured_errors() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-reject".into(),
        session_token: token(),
        argv: vec!["/bin/sh".into(), "-c".into(), "IFS= read -r _".into()],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));

    let mut incompatible = hello(&launch, None, "wrong-protocol");
    incompatible.protocol.major = incompatible.protocol.major.saturating_add(1);
    let mut attach = Attach::open(&state_dir, incompatible);
    let messages = attach.receive_until(
        Duration::from_secs(2),
        |message| matches!(message, RemoteMessage::Error(error) if error.fatal),
    );
    assert!(messages.iter().any(|message| {
        matches!(message, RemoteMessage::Error(error) if error.code == "protocol_error")
    }));

    let mut wrong_incarnation = hello(&launch, None, "wrong-incarnation");
    wrong_incarnation.expected_incarnation = Some("different-incarnation".into());
    let mut attach = Attach::open(&state_dir, wrong_incarnation);
    attach.receive_until(
        Duration::from_secs(2),
        |message| matches!(message, RemoteMessage::Error(error) if error.fatal),
    );

    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
}

#[test]
fn explicit_stop_records_real_term_trap_and_forced_exit_facts() {
    for (name, script, expected) in [
        (
            "stop-trap",
            "trap 'printf final-tail; exit 42' TERM; printf stop-ready; while :; do read -r line; done",
            RemoteProcessState::Exited {
                code: Some(42),
                signal: None,
            },
        ),
        (
            "stop-force",
            "trap '' TERM; printf stop-ready; while :; do read -r line; done",
            RemoteProcessState::Exited {
                code: None,
                signal: Some(libc::SIGKILL),
            },
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let state_dir = temporary.path().join("state");
        let request = LaunchRequest {
            session_id: name.into(),
            session_token: token(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: "/".into(),
            environment: vec![],
            cols: 80,
            rows: 24,
            persistence: PersistenceCapability::NonPersistent,
        };
        let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
        let selector = SessionSelector {
            session_id: launch.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation.clone()),
        };
        let mut attach = Attach::open(&state_dir, hello(&launch, None, "ready-client"));
        attach.receive_until(Duration::from_secs(2), |message| match message {
            RemoteMessage::FullSnapshot(snapshot) => {
                grid_text(&snapshot.grid).contains("stop-ready")
            }
            RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("stop-ready"),
            _ => false,
        });
        let started = Instant::now();
        let stopped: SessionInspection = run_json("kill", &state_dir, Some(&selector));
        assert_eq!(
            stopped.process_state, expected,
            "requesting TERM must not invent TERM as the exit"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let again: SessionInspection = run_json("kill", &state_dir, Some(&selector));
        assert_eq!(
            again.process_state, expected,
            "completed stop is idempotent"
        );
        if name == "stop-trap" {
            assert!(
                log_tail(&state_dir.join("sessions").join(name).join("output.log"))
                    >= b"stop-readyfinal-tail".len() as u64
            );
        }
        let gc: TestGcResult = run_json::<SessionSelector, _>("gc", &state_dir, None);
        assert_eq!(
            gc.removed_sessions, 1,
            "completed stop released Holder ownership"
        );
    }
}

#[test]
fn concurrent_explicit_stops_complete_one_owned_lifecycle() {
    let temporary = tempfile::tempdir().unwrap();
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "stop-concurrent".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "trap 'sleep 0.1; exit 42' TERM; printf ready; while :; do read -r line; done".into(),
        ],
        cwd: "/".into(),
        environment: vec![],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
    let selector = SessionSelector {
        session_id: launch.session_id.clone(),
        session_token: token(),
        expected_incarnation: Some(launch.session_incarnation.clone()),
    };
    let mut attach = Attach::open(&state_dir, hello(&launch, None, "ready-client"));
    attach.receive_until(Duration::from_secs(2), |message| match message {
        RemoteMessage::FullSnapshot(snapshot) => grid_text(&snapshot.grid).contains("ready"),
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("ready"),
        _ => false,
    });
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let jobs = (0..2)
        .map(|_| {
            let state_dir = state_dir.clone();
            let selector = selector.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                run_json::<_, SessionInspection>("kill", &state_dir, Some(&selector))
            })
        })
        .collect::<Vec<_>>();
    for job in jobs {
        assert_eq!(
            job.join().unwrap().process_state,
            RemoteProcessState::Exited {
                code: Some(42),
                signal: None
            }
        );
    }
}

#[test]
fn signal_exit_and_holder_failure_are_reported_without_orphaning_the_agent() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-signal-exit".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do sleep 1; done".into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
    let mut attach = Attach::open(&state_dir, hello(&launch, None, "signal-client"));
    let handshake = attach.receive_until(Duration::from_secs(2), |message| {
        matches!(message, RemoteMessage::ControlGranted(_))
    });
    let epoch = handshake
        .iter()
        .find_map(|message| match message {
            RemoteMessage::HelloAck(ack) => Some(ack.controller_epoch),
            _ => None,
        })
        .expect("epoch");
    attach.send(RemoteMessage::Signal(diri_proto::remote_pty::Signal {
        controller_epoch: epoch,
        signal: libc::SIGTERM,
    }));
    let exited = attach.receive_until(Duration::from_secs(3), |message| {
        matches!(message, RemoteMessage::ProcessExit(_))
    });
    assert!(exited.iter().any(|message| {
        matches!(message, RemoteMessage::ProcessExit(exit) if exit.signal == Some(libc::SIGTERM))
    }));
    attach.send(RemoteMessage::Signal(diri_proto::remote_pty::Signal {
        controller_epoch: epoch,
        signal: libc::SIGTERM,
    }));
    let deadline = Instant::now() + Duration::from_secs(2);
    let after_reap = loop {
        match attach
            .messages
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(Ok(RemoteMessage::Error(error))) => break Some(error),
            Ok(Ok(_)) if Instant::now() < deadline => {}
            _ => break None,
        }
    };
    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );

    assert!(
        after_reap
            .is_some_and(|error| error.fatal && error.message.contains("already been reaped")),
        "signals after reap must be explicitly rejected, never sent to a reusable PGID"
    );

    let mut failure_request = request;
    failure_request.session_id = "holder-failure".into();
    failure_request.argv[2] =
        "trap '' HUP TERM; printf 'failure-ready>'; while :; do sleep 1; done".into();
    let failed: LaunchResult = run_json("launch", &state_dir, Some(&failure_request));
    let mut attach = Attach::open(&state_dir, hello(&failed, None, "failure-client"));
    attach.receive_until(Duration::from_secs(2), |message| match message {
        RemoteMessage::FullSnapshot(snapshot) => {
            grid_text(&snapshot.grid).contains("failure-ready>")
        }
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("failure-ready>"),
        _ => false,
    });
    // SAFETY: this test launched and uniquely owns the recorded Holder PID.
    assert_eq!(
        unsafe { libc::kill(failed.holder_pid as i32, libc::SIGKILL) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_exists(failed.process_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if process_exists(failed.process_pid) {
        // Prevent a failed assertion from leaking the fixture process group.
        // SAFETY: the PTY child created a process group with its own PID.
        unsafe {
            libc::kill(-(failed.process_pid as i32), libc::SIGKILL);
        }
        panic!("Agent process survived an abnormal Holder exit");
    }
    let inspection = run_output(
        "inspect",
        &state_dir,
        Some(&SessionSelector {
            session_id: failed.session_id,
            session_token: token(),
            expected_incarnation: Some(failed.session_incarnation),
        }),
    );
    assert_eq!(inspection.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<diri_proto::remote_pty::RemoteManagementFailure>(
            &inspection.stdout
        )
        .unwrap(),
        diri_proto::remote_pty::RemoteManagementFailure::HolderUnavailable,
    );
}

fn process_exists(pid: u32) -> bool {
    // SAFETY: signal zero only checks whether the numeric PID still exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn grid_text(grid: &diri_proto::grid::GridUpdate) -> String {
    let mut lines = Vec::new();
    for row in &grid.changed_rows {
        let mut line = String::new();
        for cell in &row.cells {
            line.push(char::from_u32(cell.scalar).unwrap_or(' '));
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

fn message_contains(message: &RemoteMessage, needle: &str) -> bool {
    match message {
        RemoteMessage::Terminal(frame) => frame
            .output_payload()
            .is_some_and(|(_, bytes)| String::from_utf8_lossy(bytes).contains(needle)),
        RemoteMessage::FullSnapshot(snapshot) => grid_text(&snapshot.grid).contains(needle),
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains(needle),
        _ => false,
    }
}

#[test]
fn environment_capture_survives_an_interactive_login_bash() {
    // A login shell is started interactively so rc-file toolchain setup runs,
    // and GNU bash marks every descriptor from 3 through 19 close-on-exec in
    // exactly that mode (shell.c, "some systems have the bad habit of starting
    // login shells with lots of open file descriptors"). Any return to an
    // inherited descriptor for the payload fails here with EBADF, which is how
    // the Linux/bash remote soak broke while macOS/zsh stayed green.
    let bash = std::path::Path::new("/bin/bash");
    if !bash.is_file() {
        eprintln!("skipping: /bin/bash is unavailable on this host");
        return;
    }
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = EnvironmentCaptureRequest {
        cwd: Some("/".into()),
        timeout_millis: 10_000,
    };
    let mut child = Command::new(helper())
        .args(["__environment-test-shell", "/bin/bash"])
        .env("DIRI_REMOTE_STATE_DIR", &state_dir)
        .env("SSH_CONNECTION", "must-not-propagate")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn login bash capture");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request).expect("request");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("capture output");
    assert!(
        output.status.success(),
        "login bash capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured: EnvironmentCaptureResult =
        serde_json::from_slice(&output.stdout).expect("capture JSON");
    assert_eq!(captured.shell, "/bin/bash");
    assert_eq!(captured.cwd, "/");
    assert!(
        captured
            .environment
            .iter()
            .any(|variable| variable.name == "PATH")
    );
    assert!(captured.environment.iter().all(|variable| {
        !variable.name.starts_with("SSH_") && !variable.name.starts_with("DIRI_")
    }));
}

#[test]
fn environment_capture_frames_its_payload_and_scrubs_ssh_state() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = EnvironmentCaptureRequest {
        cwd: Some("/".into()),
        timeout_millis: 10_000,
    };
    let mut child = Command::new(helper())
        .arg("environment")
        .env("DIRI_REMOTE_STATE_DIR", &state_dir)
        .env("SHELL", "/must/not/be/trusted")
        .env("SSH_CONNECTION", "must-not-propagate")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn environment capture");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request).expect("request");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("capture output");
    assert!(
        output.status.success(),
        "capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured: EnvironmentCaptureResult =
        serde_json::from_slice(&output.stdout).expect("capture JSON");
    assert!(std::path::Path::new(&captured.shell).is_absolute());
    assert_ne!(captured.shell, "/must/not/be/trusted");
    assert_eq!(captured.cwd, "/");
    assert!(
        captured
            .environment
            .iter()
            .any(|variable| variable.name == "PATH")
    );
    assert!(captured.environment.iter().all(|variable| {
        !variable.name.starts_with("SSH_") && !variable.name.starts_with("DIRI_")
    }));
}

#[test]
fn environment_capture_tolerates_startup_noise_and_bounds_timeout_failures() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let noisy_shell = temporary.path().join("sh");
    std::fs::write(
        &noisy_shell,
        b"#!/bin/sh\nprintf 'rc-stdout\\n'\nprintf 'rc-stderr\\n' >&2\nexec /bin/sh \"$@\"\n",
    )
    .expect("noisy shell");
    std::fs::set_permissions(&noisy_shell, std::fs::Permissions::from_mode(0o700))
        .expect("shell mode");
    let request = EnvironmentCaptureRequest {
        cwd: Some("/".into()),
        timeout_millis: 2_000,
    };
    let mut child = Command::new(helper())
        .args([
            "__environment-test-shell",
            noisy_shell.to_str().expect("shell path"),
        ])
        .env("DIRI_REMOTE_STATE_DIR", &state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("noisy environment capture");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request).expect("request");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("noisy output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured: EnvironmentCaptureResult =
        serde_json::from_slice(&output.stdout).expect("capture JSON");
    assert!(captured.diagnostics.contains("rc-stdout"));
    assert!(captured.diagnostics.contains("rc-stderr"));

    let stuck_shell = temporary.path().join("dash");
    std::fs::write(&stuck_shell, b"#!/bin/sh\nsleep 10\n").expect("stuck shell");
    std::fs::set_permissions(&stuck_shell, std::fs::Permissions::from_mode(0o700))
        .expect("shell mode");
    let request = EnvironmentCaptureRequest {
        cwd: Some("/".into()),
        timeout_millis: 100,
    };
    let started = Instant::now();
    let mut child = Command::new(helper())
        .args([
            "__environment-test-shell",
            stuck_shell.to_str().expect("shell path"),
        ])
        .env("DIRI_REMOTE_STATE_DIR", &state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("stuck environment capture");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), &request).expect("request");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("timeout output");
    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
}

#[test]
fn persistence_witness_is_checked_and_cleaned_over_separate_commands() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let nonce = "0123456789abcdef0123456789abcdef".to_string();
    let started: PersistenceProbeResult = run_json(
        "persistence",
        &state_dir,
        Some(&PersistenceProbeRequest {
            nonce: nonce.clone(),
            action: PersistenceProbeAction::BeginNative,
        }),
    );
    assert!(started.alive);
    let checked: PersistenceProbeResult = run_json(
        "persistence",
        &state_dir,
        Some(&PersistenceProbeRequest {
            nonce: nonce.clone(),
            action: PersistenceProbeAction::Check,
        }),
    );
    assert!(checked.alive);
    let cleaned: PersistenceProbeResult = run_json(
        "persistence",
        &state_dir,
        Some(&PersistenceProbeRequest {
            nonce,
            action: PersistenceProbeAction::Cleanup,
        }),
    );
    assert!(!cleaned.alive);
}

#[test]
#[ignore = "requires an already-running transient user supervisor"]
fn existing_user_supervisor_can_own_one_holder_without_persistent_configuration() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-supervised".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'supervised>'; IFS= read -r line; printf 'done:%s\\n' \"$line\"".into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 80,
        rows: 24,
        persistence: PersistenceCapability::UserSupervisor,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));
    assert_eq!(launch.persistence, PersistenceCapability::UserSupervisor);
    let mut attach = Attach::open(&state_dir, hello(&launch, None, "supervised-client"));
    attach.receive_until(Duration::from_secs(5), |message| match message {
        RemoteMessage::FullSnapshot(snapshot) => grid_text(&snapshot.grid).contains("supervised>"),
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("supervised>"),
        _ => false,
    });
    attach.send(RemoteMessage::Terminal(Frame::input(b"ok\n".to_vec())));
    attach.receive_until(Duration::from_secs(5), |message| {
        matches!(message, RemoteMessage::ProcessExit(_))
    });
    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
}

#[test]
#[ignore = "release-only local UDS latency gate; run scripts/remote-perf-gate.sh"]
fn performance_gate_meets_remote_holder_latency_budget() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-performance".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'ready\\n'; while IFS= read -r line; do printf 'ack:%s\\n' \"$line\"; done"
                .into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 100,
        rows: 32,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));

    let mut snapshot_latencies = Vec::new();
    let mut active = None;
    for index in 0..12 {
        let attach = Attach::open(
            &state_dir,
            hello(&launch, None, &format!("snapshot-{index}")),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut acknowledged_at = None;
        loop {
            let message = attach
                .messages
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("snapshot handshake timeout")
                .expect("snapshot decode");
            match message {
                RemoteMessage::HelloAck(_) => acknowledged_at = Some(Instant::now()),
                RemoteMessage::FullSnapshot(_) => {
                    snapshot_latencies.push(
                        acknowledged_at
                            .expect("snapshot followed HelloAck")
                            .elapsed(),
                    );
                    break;
                }
                _ => {}
            }
        }
        active = Some(attach);
    }
    let mut attach = active.expect("active controller");
    let mut input_to_pty = Vec::new();
    let mut output_to_diff = Vec::new();
    let mut loopback = Vec::new();
    for index in 0..32 {
        let marker = format!("latency-{index:02}");
        let acknowledgement = format!("ack:{marker}");
        let sent_at = Instant::now();
        attach.send(RemoteMessage::Terminal(Frame::input(
            format!("{marker}\n").into_bytes(),
        )));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output_at = None;
        let mut acknowledged_at = None;
        let mut diff_at = None;
        while acknowledged_at.is_none() || diff_at.is_none() {
            let message = attach
                .messages
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("latency sample timeout")
                .expect("latency sample decode");
            match message {
                RemoteMessage::Terminal(frame) => {
                    if let Some((_, bytes)) = frame.output_payload() {
                        let text = String::from_utf8_lossy(bytes);
                        if output_at.is_none() && text.contains(&marker) {
                            output_at = Some(Instant::now());
                        }
                        if acknowledged_at.is_none() && text.contains(&acknowledgement) {
                            acknowledged_at = Some(Instant::now());
                        }
                    }
                }
                RemoteMessage::GridDelta(delta) if grid_text(&delta.grid).contains(&marker) => {
                    diff_at = Some(Instant::now());
                }
                RemoteMessage::FullSnapshot(snapshot)
                    if grid_text(&snapshot.grid).contains(&marker) =>
                {
                    diff_at = Some(Instant::now());
                }
                _ => {}
            }
        }
        let output_at = output_at.expect("PTY echo output");
        input_to_pty.push(output_at.duration_since(sent_at));
        output_to_diff.push(diff_at.expect("grid diff").duration_since(output_at));
        loopback.push(
            acknowledged_at
                .expect("agent acknowledgement")
                .duration_since(sent_at),
        );
    }

    assert_percentile("snapshot p90", &mut snapshot_latencies, 90, 100);
    assert_percentile("input-to-PTY p95", &mut input_to_pty, 95, 10);
    assert_percentile("output-to-diff p90", &mut output_to_diff, 90, 8);
    assert_percentile("loopback median", &mut loopback.clone(), 50, 75);
    assert_percentile("loopback p90", &mut loopback, 90, 150);

    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
}

#[test]
#[ignore = "release acceptance test emits 23 MiB to force slow-attach recovery"]
fn slow_attach_never_blocks_pty_and_reconnects_from_full_snapshot() {
    let temporary = tempfile::tempdir().expect("temp");
    let state_dir = temporary.path().join("state");
    let request = LaunchRequest {
        session_id: "holder-slow-attach".into(),
        session_token: token(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "IFS= read -r _; /usr/bin/yes 0123456789abcdef | /usr/bin/head -c 23000000; printf 'slow-tail>'; IFS= read -r _"
                .into(),
        ],
        cwd: "/".into(),
        environment: vec![diri_proto::remote_pty::EnvironmentVariable {
            name: "TERM".into(),
            value: "xterm-256color".into(),
        }],
        cols: 100,
        rows: 32,
        persistence: PersistenceCapability::NonPersistent,
    };
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&request));

    // Intentionally retain but never read the Bridge stdout. Its pipe and UDS
    // fill quickly; the Holder must continue draining and parsing the PTY.
    let mut slow = Command::new(helper())
        .arg("attach")
        .env("DIRI_REMOTE_STATE_DIR", &state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("slow attach");
    let mut slow_input = slow.stdin.take().expect("slow attach input");
    slow_input
        .write_all(
            &RemoteCodec::encode(&RemoteMessage::Hello(hello(
                &launch,
                None,
                "slow-controller",
            )))
            .expect("Hello"),
        )
        .expect("write Hello");
    slow_input
        .write_all(
            &RemoteCodec::encode(&RemoteMessage::Terminal(Frame::input(b"go\n".to_vec())))
                .expect("input"),
        )
        .expect("write trigger");
    slow_input.flush().expect("flush trigger");

    let selector = SessionSelector {
        session_id: launch.session_id.clone(),
        session_token: token(),
        expected_incarnation: Some(launch.session_incarnation.clone()),
    };
    let output_log = state_dir.join("sessions/holder-slow-attach/output.log");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if std::fs::read(&output_log).is_ok_and(|bytes| {
            bytes
                .windows(b"slow-tail>".len())
                .any(|part| part == b"slow-tail>")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Holder stopped draining a slow attach"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut recovered = Attach::open(&state_dir, hello(&launch, None, "recovered-controller"));
    recovered.receive_until(Duration::from_secs(5), |message| match message {
        RemoteMessage::FullSnapshot(snapshot) => grid_text(&snapshot.grid).contains("slow-tail>"),
        _ => false,
    });

    let _ = slow.kill();
    let _ = slow.wait();
    let _: SessionInspection = run_json("kill", &state_dir, Some(&selector));
    let gc: TestGcResult = run_json::<(), _>("gc", &state_dir, None);
    assert_eq!(
        gc.removed_sessions, 1,
        "GC must remove the circular output log"
    );
    assert_eq!(gc.retained_sessions, 0);
}

fn assert_percentile(name: &str, samples: &mut [Duration], percentile: usize, max_ms: u128) {
    samples.sort_unstable();
    let rank = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
    let actual = samples[rank].as_micros();
    eprintln!(
        "{name}: {actual}us (budget {}us, samples={})",
        max_ms * 1_000,
        samples.len()
    );
    assert!(
        actual <= max_ms * 1_000,
        "{name} was {actual}us, budget is {}us; samples={samples:?}",
        max_ms * 1_000
    );
}

#[test]
fn bounded_drain_preserves_output_and_grid_before_process_exit() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let state_dir = temporary.path().join("state");
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&LaunchRequest {
        session_id: "exit-tail".into(), session_token: token(),
        argv: vec!["/bin/sh".into(), "-c".into(), "stty -echo; printf 'ready\\n'; read start; head -c 2097152 /dev/zero | tr '\\000' x; printf 'tail-complete'".into()],
        cwd: "/".into(), environment: vec![], cols: 80, rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    }));
    let mut attach = Attach::open(&state_dir, hello(&launch, Some(0), "exit-tail-client"));
    attach.receive_until(Duration::from_secs(5), |message| {
        matches!(message, RemoteMessage::ControlGranted(_))
    });
    attach.send(RemoteMessage::Terminal(Frame::input(b"start\n".to_vec())));
    let started = Instant::now();
    let messages = attach.receive_until(Duration::from_secs(15), |message| {
        matches!(message, RemoteMessage::ProcessExit(_))
    });
    let elapsed = started.elapsed();
    let tail_in_grid = messages.iter().any(|message| match message {
        RemoteMessage::FullSnapshot(snapshot) => {
            grid_text(&snapshot.grid).contains("tail-complete")
        }
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("tail-complete"),
        _ => false,
    });
    let inspection: SessionInspection = run_json(
        "inspect",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation.clone()),
        }),
    );
    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
    assert!(tail_in_grid, "final grid must arrive before ProcessExit");
    assert_eq!(
        inspection.output_offset,
        b"ready\r\n".len() as u64 + 2097152 + b"tail-complete".len() as u64
    );
    eprintln!("2 MiB output and final grid drained in {elapsed:?}");
}

#[test]
#[ignore = "release-only responsiveness under continuous PTY output"]
fn continuous_output_keeps_controller_responsive() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let state_dir = temporary.path().join("state");
    let launch: LaunchResult = run_json("launch", &state_dir, Some(&LaunchRequest {
        session_id: "loaded-input".into(), session_token: token(),
        argv: vec!["/bin/sh".into(), "-c".into(), "stty -echo; printf 'ready\\n'; read start; yes noisy-output & producer=$!; read line; kill $producer; wait $producer 2>/dev/null; printf 'ack:%s\\n' \"$line\"; read end".into()],
        cwd: "/".into(), environment: vec![], cols: 80, rows: 24,
        persistence: PersistenceCapability::NonPersistent,
    }));
    let mut attach = Attach::open(&state_dir, hello(&launch, Some(0), "loaded-client"));
    attach.receive_until(Duration::from_secs(5), |message| {
        matches!(message, RemoteMessage::ControlGranted(_))
    });
    attach.send(RemoteMessage::Terminal(Frame::input(b"start\n".to_vec())));
    attach.receive_until(Duration::from_secs(5), |message| matches!(message, RemoteMessage::Terminal(frame) if frame.output_payload().is_some_and(|(_, bytes)| bytes.windows(12).any(|part| part == b"noisy-output"))));
    let started = Instant::now();
    attach.send(RemoteMessage::Terminal(Frame::input(
        b"interactive\n".to_vec(),
    )));
    attach.receive_until(Duration::from_secs(5), |message| match message {
        RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("ack:interactive"),
        RemoteMessage::FullSnapshot(snapshot) => {
            grid_text(&snapshot.grid).contains("ack:interactive")
        }
        _ => false,
    });
    let elapsed = started.elapsed();
    let _: SessionInspection = run_json(
        "kill",
        &state_dir,
        Some(&SessionSelector {
            session_id: launch.session_id,
            session_token: token(),
            expected_incarnation: Some(launch.session_incarnation),
        }),
    );
    eprintln!("loaded input-to-grid: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(150));
}

#[test]
fn stopping_during_output_checkpoints_leaves_no_temporary_state() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let state_dir = temporary.path().join("state");
    for iteration in 0..4 {
        let launch: LaunchResult = run_json(
            "launch",
            &state_dir,
            Some(&LaunchRequest {
                session_id: format!("checkpoint-stop-{iteration}"),
                session_token: token(),
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec yes checkpoint-output".into(),
                ],
                cwd: "/".into(),
                environment: vec![],
                cols: 80,
                rows: 24,
                persistence: PersistenceCapability::NonPersistent,
            }),
        );
        let session_root = state_dir.join("sessions").join(&launch.session_id);
        let deadline = Instant::now() + Duration::from_secs(10);
        while log_tail(&session_root.join("output.log")) < 1 << 20 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let inspection: SessionInspection = run_json(
            "kill",
            &state_dir,
            Some(&SessionSelector {
                session_id: launch.session_id,
                session_token: token(),
                expected_incarnation: Some(launch.session_incarnation),
            }),
        );
        let log_bytes = log_tail(&session_root.join("output.log"));
        assert_eq!(
            inspection.output_offset, log_bytes,
            "management exit must include the final log offset"
        );
        let gc: TestGcResult = run_json::<(), _>("gc", &state_dir, None);
        assert_eq!(
            gc.removed_sessions, 1,
            "interrupted checkpoint left temporary state on iteration {iteration}"
        );
        assert_eq!(gc.retained_sessions, 0);
    }
}

// Keep fixture forwarding in userspace. Linux splice can hold a pipe lock
// while waiting for TCP input, deadlocking the Helper's reader of that pipe.
fn copy_test_stream(reader: &mut impl Read, writer: &mut impl Write) -> std::io::Result<()> {
    let mut bytes = [0_u8; 8192];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..count])?;
    }
}

// The test bridge is a reliable byte stream. netem drops actual TCP packets;
// it never discards application bytes or changes the Helper transport.
fn tcp_bridge(
    mut input: impl Write + Send + 'static,
    mut output: impl Read + Send + 'static,
) -> (TestWriter, TestReader) {
    use std::net::{Shutdown, TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client =
        TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(10))
            .unwrap();
    let (mut server, _) = listener.accept().unwrap();
    client.set_nodelay(true).unwrap();
    server.set_nodelay(true).unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut server_read = server.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = copy_test_stream(&mut server_read, &mut input);
    });
    std::thread::spawn(move || {
        let _ = copy_test_stream(&mut output, &mut server);
        let _ = server.shutdown(Shutdown::Write);
    });
    (Box::new(client.try_clone().unwrap()), Box::new(client))
}

#[cfg(target_os = "linux")]
#[path = "support/netem.rs"]
mod netem;

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires Linux unprivileged user/network namespaces; no host network changes"]
fn impaired_tcp_preserves_input_history_and_reconnect() {
    if !netem::enter_fixture() {
        return;
    }
    for (delay, jitter, loss) in [(0, 0, 0), (70, 15, 2)] {
        netem::configure(delay, jitter, loss);
        let temporary = tempfile::tempdir().unwrap();
        let state_dir = temporary.path().join("state");
        let launch: LaunchResult = run_json("launch", &state_dir, Some(&LaunchRequest {
            session_id: "netem-fixture".into(), session_token: token(),
            argv: vec!["/bin/sh".into(), "-c".into(),
                "i=0; while [ $i -lt 8192 ]; do printf '%080d\\n' $i; i=$((i+1)); done; printf 'ready>'; while IFS= read -r line; do printf 'ack:%s\\n' \"$line\"; done".into()],
            cwd: "/".into(), environment: vec![], cols: 100, rows: 24,
            persistence: PersistenceCapability::NonPersistent,
        }));
        // Always reap the fixture's Agent tree even if a protocol assertion fails.
        struct Cleanup(std::path::PathBuf, SessionSelector);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _: SessionInspection = run_json("kill", &self.0, Some(&self.1));
            }
        }
        let cleanup = Cleanup(
            state_dir.clone(),
            SessionSelector {
                session_id: launch.session_id.clone(),
                session_token: token(),
                expected_incarnation: Some(launch.session_incarnation.clone()),
            },
        );
        let mut attach =
            Attach::open_impl(&state_dir, hello(&launch, Some(0), "netem-first"), true);
        let mut raw = Vec::new();
        attach.receive_until(Duration::from_secs(30), |message| {
            if let RemoteMessage::Terminal(frame) = message
                && let Some((_, bytes)) = frame.output_payload()
            {
                raw.extend_from_slice(bytes);
            }
            raw.ends_with(b"ready>")
        });
        let mut expected: Vec<u8> = Vec::new();
        for index in 0..8192 {
            expected.extend(format!("{index:080}\r\n").as_bytes());
        }
        expected.extend(b"ready>");
        assert!(
            raw == expected,
            "loss must never corrupt or duplicate replay bytes"
        );
        let mut timings = Vec::new();
        for index in 0..32 {
            let ack = format!("ack:{index:03}");
            let start = Instant::now();
            attach.send(RemoteMessage::Terminal(Frame::input(
                format!("{index:03}\n").into_bytes(),
            )));
            let mut output: Vec<u8> = Vec::new();
            attach.receive_until(Duration::from_secs(10), |message| {
                if let RemoteMessage::Terminal(frame) = message
                    && let Some((_, bytes)) = frame.output_payload()
                {
                    output.extend(bytes);
                }
                output.windows(ack.len()).any(|part| part == ack.as_bytes())
            });
            timings.push(start.elapsed());
        }
        timings.sort_unstable();
        eprintln!(
            "TCP delay={delay}ms jitter={jitter}ms loss={loss}%: input/output median={:?} p90={:?} max={:?}",
            timings[16], timings[28], timings[31]
        );
        assert!(
            timings[28] < Duration::from_secs(2),
            "interactive latency exceeds impaired-link budget"
        );
        let scroll = Instant::now();
        attach.send(RemoteMessage::ScrollbackRequest(
            diri_proto::remote_pty::ScrollbackRequest {
                request_id: 1,
                first_row: 0,
                max_rows: 100,
            },
        ));
        let history = attach.receive_until(Duration::from_secs(10), |message| {
            matches!(message, RemoteMessage::ScrollbackResponse(response) if response.request_id == 1)
        });
        assert!(history.iter().any(|message| matches!(message,
            RemoteMessage::ScrollbackResponse(response) if response.result.row_count == 100 && !response.result.payload.is_empty())));
        eprintln!("on-demand history: {:?}", scroll.elapsed());
        drop(attach);
        let inspection: SessionInspection = run_json("inspect", &state_dir, Some(&cleanup.1));
        assert_eq!(
            inspection.process_state,
            RemoteProcessState::Running {
                pid: launch.process_pid
            }
        );
        let mut attach =
            Attach::open_impl(&state_dir, hello(&launch, None, "netem-reconnect"), true);
        attach.receive_until(Duration::from_secs(10), |message| {
            matches!(message, RemoteMessage::FullSnapshot(_))
        });
        attach.send(RemoteMessage::Terminal(Frame::input(
            b"reconnected\n".to_vec(),
        )));
        attach.receive_until(Duration::from_secs(10), |message| match message {
            RemoteMessage::GridDelta(delta) => grid_text(&delta.grid).contains("ack:reconnected"),
            RemoteMessage::FullSnapshot(snapshot) => {
                grid_text(&snapshot.grid).contains("ack:reconnected")
            }
            _ => false,
        });
        let drops = netem::drops();
        eprintln!("actual kernel packet drops: {drops}");
        if loss > 0 {
            assert!(drops > 0, "fixture did not exercise packet loss");
        }
        drop(attach);
        drop(cleanup);
    }
}
