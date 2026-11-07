#![cfg(unix)]

//! sshd does not give every SSH session the same `XDG_RUNTIME_DIR`, and the
//! Helper derives its Holder socket root from it. A Session launched by one
//! SSH session must stay reachable, and must never be reset, from a later
//! launch or kill that runs under a different runtime root.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use diri_engine::remote::ssh::{HelperCommand, SshTransport};
use diri_proto::HostEntry;
use diri_proto::remote_pty::{
    EnvironmentVariable, LaunchRequest, LaunchResult, PersistenceCapability, RemoteProcessState,
    SessionInspection, SessionSelector, SessionToken,
};

const BUILD_ID: &str = "socket-root-test";

struct Fixture {
    _root: tempfile::TempDir,
    state: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    xdg: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Short root: Holder socket paths must fit sockaddr_un.
        let root = tempfile::Builder::new()
            .prefix("dsr")
            .tempdir_in("/tmp")
            .expect("temp");
        let path = root.path();
        let home = path.join("h");
        let bin = home.join(format!(
            ".cache/diri/bin/protocol-{}/{BUILD_ID}",
            diri_proto::remote_pty::PROTOCOL_MAJOR
        ));
        std::fs::create_dir_all(&bin).expect("helper dir");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_diri-remote"), bin.join("diri-remote"))
            .expect("helper link");
        let tmp = path.join("t");
        let xdg = path.join("x");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&xdg).expect("xdg");
        Self {
            state: path.join("s"),
            home,
            tmp,
            xdg,
            _root: root,
        }
    }

    /// A shell standing in for one SSH session's environment.
    fn shell(&self, xdg: Option<&Path>) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .env("DIRI_REMOTE_STATE_DIR", &self.state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(xdg) = xdg {
            command.env("XDG_RUNTIME_DIR", xdg);
        }
        command
    }

    /// The Helper invoked without any socket-root prelude, as every
    /// non-attach command was before.
    fn bare(&self, xdg: Option<&Path>, command: &str, request: &impl serde::Serialize) -> Output {
        let mut shell = self.shell(xdg);
        shell.arg("-c").arg(format!(
            "exec \"$HOME/.cache/diri/bin/protocol-{}/{BUILD_ID}/diri-remote\" {command}",
            diri_proto::remote_pty::PROTOCOL_MAJOR
        ));
        run(shell, request)
    }

    /// The exact remote command the Engine sends for a Session command.
    fn engine(
        &self,
        xdg: Option<&Path>,
        command: HelperCommand,
        session_id: &str,
        request: &impl serde::Serialize,
    ) -> Output {
        let host = HostEntry {
            transport: Default::default(),
            id: "forge".into(),
            name: None,
            ssh: "developer@forge".into(),
            default_cwd: None,
            node: None,
        };
        let spec = SshTransport::new(&host, "/tmp/control")
            .helper_session_command(BUILD_ID, command, session_id)
            .expect("session command");
        let remote = spec.arguments.last().expect("remote command").clone();
        let mut shell = self.shell(xdg);
        shell.arg("-c").arg(remote);
        run(shell, request)
    }
}

fn run(mut command: Command, request: &impl serde::Serialize) -> Output {
    let mut child = command.spawn().expect("spawn");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), request).expect("request");
    drop(child.stdin.take());
    child.wait_with_output().expect("wait")
}

fn success<R: serde::de::DeserializeOwned>(output: Output) -> R {
    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON response")
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal zero only checks whether the numeric PID exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn a_session_survives_and_stops_across_differing_runtime_roots() {
    let fixture = Fixture::new();
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).expect("nonce");
    let session_id = format!(
        "socket-root-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let token = SessionToken::new("0123456789abcdef0123456789abcdef").expect("token");
    let request = LaunchRequest {
        session_id: session_id.clone(),
        session_token: token.clone(),
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'root-ready>'; IFS= read -r _".into(),
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

    // The first SSH session has no XDG_RUNTIME_DIR: the socket lands under
    // TMPDIR.
    let launched: LaunchResult =
        success(fixture.engine(None, HelperCommand::Launch, &session_id, &request));
    let session_json = fixture
        .state
        .join("sessions")
        .join(&session_id)
        .join("session.json");
    let original = std::fs::read(&session_json).expect("session state");

    // A later SSH session has XDG_RUNTIME_DIR. Without the prelude its launch
    // does not see the socket, but the Holder still owns the Session: the
    // Helper must refuse instead of resetting the live Holder.
    let refused = fixture.bare(Some(&fixture.xdg), "launch", &request);
    assert!(!refused.status.success());
    assert_eq!(
        refused.stdout.trim_ascii(),
        br#"{"error":"holder_unavailable"}"#
    );
    assert_eq!(std::fs::read(&session_json).expect("state kept"), original);
    assert!(alive(launched.process_pid), "Agent must survive");

    // The Engine's Session commands resolve the Holder's root regardless.
    let relaunched: LaunchResult = success(fixture.engine(
        Some(&fixture.xdg),
        HelperCommand::Launch,
        &session_id,
        &request,
    ));
    assert_eq!(relaunched.session_incarnation, launched.session_incarnation);
    assert_eq!(relaunched.process_pid, launched.process_pid);

    let selector = SessionSelector {
        session_id: session_id.clone(),
        session_token: token,
        expected_incarnation: Some(launched.session_incarnation.clone()),
    };
    let stopped: SessionInspection = success(fixture.engine(
        Some(&fixture.xdg),
        HelperCommand::Kill,
        &session_id,
        &selector,
    ));
    assert!(matches!(
        stopped.process_state,
        RemoteProcessState::Exited { .. }
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(launched.process_pid) {
        assert!(Instant::now() < deadline, "Agent outlived its stop");
        std::thread::sleep(Duration::from_millis(20));
    }
}
