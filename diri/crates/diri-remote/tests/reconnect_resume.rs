#![cfg(unix)]

//! Reconnect cost after a forced transport drop, measured end to end through
//! the real Engine pump, a fake `ssh` and a real Holder/PTY. The fake `ssh`
//! records every byte each attach carries in both directions, so the cost of
//! a reconnect is read from the wire rather than inferred.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::remote::binding::RemoteBindingStore;
use diri_engine::remote::executor::ProcessExecutor;
use diri_engine::remote::manager::{ArtifactCatalog, RemoteManager};
use diri_engine::{Authority, ManifestEngine, PtySpec, RemoteSessionSpec, Session, SessionSpec};
use diri_proto::remote_pty::{
    EnvironmentVariable, GridResume, GridResumeDecline, GridResumeOutcome, Hello, LaunchRequest,
    PROTOCOL_MINOR, PersistenceCapability, ProtocolVersion, RemoteCapability, RemoteCodec,
    RemoteMessage, RemoteRole, SessionToken,
};
use diri_proto::{HostEntry, RemoteConnectionState};
use diri_terminal_state::GridMirror;

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_diri-remote")
}

fn unique_session_id(label: &str) -> String {
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).expect("fixture nonce");
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{label}-{suffix}")
}

/// Each attach waits for `allow.N`, then tees its upstream into `up.N` and its
/// downstream into `down.N`. The Bridge PID is written to `bridge.N` so the
/// test can drop exactly that transport. `ssh.N` names its process group.
fn write_recording_ssh(root: &Path, home: &Path, state: &Path, control: &Path) -> PathBuf {
    let path = root.join("ssh-recording");
    let script = format!(
        r#"#!/bin/sh
export HOME='{home}'
export DIRI_REMOTE_STATE_DIR='{state}'
for last; do :; done
case "$last" in
  *' attach'*)
    dir='{control}'
    n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))
    echo $n > "$dir/count"
    echo $$ > "$dir/ssh.$n"
    while [ ! -e "$dir/allow.$n" ]; do sleep 0.002; done
    mkfifo "$dir/fifo.$n"
    tee "$dir/down.$n" < "$dir/fifo.$n" & tee_pid=$!
    # An asynchronous list's stdin is /dev/null in a non-interactive shell
    # (dash applies that even under an explicit `<&0`), so the Engine's pipe
    # reaches tee through fd 3.
    exec 3<&0
    tee "$dir/up.$n" <&3 | /bin/sh -c "$last" > "$dir/fifo.$n" & bridge=$!
    echo $bridge > "$dir/bridge.$n"
    wait $bridge
    wait $tee_pid
    exit 255
    ;;
esac
exec /bin/sh -c "$last"
"#,
        home = home.display(),
        state = state.display(),
        control = control.display(),
    );
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&path)
        .expect("fake ssh");
    file.write_all(script.as_bytes()).expect("fake ssh script");
    drop(file);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
    path
}

fn wait_until(what: &str, timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("timed out waiting for {what}");
}

fn connection_state(session: &Session) -> Option<RemoteConnectionState> {
    session
        .view()
        .remote_connection
        .map(|connection| connection.state)
}

/// What one attach carried, decoded from the recorded downstream bytes up to
/// and including `ControlGranted`.
#[derive(Debug, Default)]
struct SeedCost {
    upstream_hello_bytes: usize,
    downstream_bytes: usize,
    replay_bytes: usize,
    snapshot_bytes: usize,
    snapshot_rows: usize,
    delta_bytes: usize,
    delta_rows: usize,
    kinds: Vec<&'static str>,
}

fn kind(message: &RemoteMessage) -> &'static str {
    use diri_proto::frames::FrameType;
    match message {
        RemoteMessage::Terminal(frame) => match frame.frame_type {
            FrameType::Output => "Output",
            FrameType::ReplayBegin => "ReplayBegin",
            FrameType::ReplayEnd => "ReplayEnd",
            _ => "Terminal",
        },
        RemoteMessage::HelloAck(_) => "HelloAck",
        RemoteMessage::FullSnapshot(_) => "FullSnapshot",
        RemoteMessage::GridDelta(_) => "GridDelta",
        RemoteMessage::ControlGranted(_) => "ControlGranted",
        RemoteMessage::InputModes(_) => "InputModes",
        RemoteMessage::TerminalResetState(_) => "TerminalResetState",
        RemoteMessage::ForegroundProcess(_) => "ForegroundProcess",
        RemoteMessage::Error(_) => "Error",
        _ => "Other",
    }
}

fn seed_cost(control: &Path, attach: usize) -> SeedCost {
    let mut cost = SeedCost::default();
    let up = fs::read(control.join(format!("up.{attach}"))).unwrap_or_default();
    let mut codec = RemoteCodec::new();
    if let Some(hello) = codec.feed(&up).unwrap_or_default().first() {
        cost.upstream_hello_bytes = RemoteCodec::encode(hello).expect("encode").len();
    }
    let down = fs::read(control.join(format!("down.{attach}"))).expect("downstream");
    let mut codec = RemoteCodec::new();
    let mut replaying = false;
    for message in codec.feed(&down).expect("decode downstream") {
        let size = RemoteCodec::encode(&message).expect("encode").len();
        cost.downstream_bytes += size;
        cost.kinds.push(kind(&message));
        match &message {
            RemoteMessage::Terminal(frame) => match frame.frame_type {
                diri_proto::frames::FrameType::ReplayBegin => replaying = true,
                diri_proto::frames::FrameType::ReplayEnd => replaying = false,
                diri_proto::frames::FrameType::Output if replaying => cost.replay_bytes += size,
                _ => {}
            },
            RemoteMessage::FullSnapshot(snapshot) => {
                cost.snapshot_bytes += size;
                cost.snapshot_rows += snapshot.grid.changed_rows.len();
            }
            RemoteMessage::GridDelta(delta) => {
                cost.delta_bytes += size;
                cost.delta_rows += delta.grid.changed_rows.len();
            }
            _ => {}
        }
        if matches!(message, RemoteMessage::ControlGranted(_)) {
            break;
        }
    }
    cost
}

struct Fixture {
    _temporary: tempfile::TempDir,
    control: PathBuf,
    session: Session,
    attaches: usize,
}

impl Fixture {
    fn spawn(label: &str, program: &str, cols: u16, rows: u16) -> Self {
        let temporary = tempfile::tempdir().expect("temp");
        let remote_home = temporary.path().join("remote-home");
        let remote_state = temporary.path().join("remote-state");
        let control = temporary.path().join("control");
        fs::create_dir(&remote_home).expect("remote home");
        fs::create_dir(&control).expect("control");
        fs::write(control.join("allow.1"), b"").expect("allow first attach");
        let fake_ssh = write_recording_ssh(temporary.path(), &remote_home, &remote_state, &control);
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
            id: format!("fixture-{label}"),
            name: None,
            ssh: "fixture-host".into(),
            default_cwd: Some("/".into()),
            node: None,
        };
        let installed = manager.ensure_helper(&host).expect("bootstrap");
        let request = LaunchRequest {
            session_id: unique_session_id(label),
            session_token: SessionToken::new("abcdef0123456789abcdef0123456789").expect("token"),
            argv: vec!["/bin/sh".into(), "-c".into(), program.into()],
            cwd: "/".into(),
            environment: vec![EnvironmentVariable {
                name: "TERM".into(),
                value: "xterm-256color".into(),
            }],
            cols,
            rows,
            persistence: PersistenceCapability::NonPersistent,
            keyboard_enhancements: false,
        };
        let bindings =
            RemoteBindingStore::new(temporary.path().join("bindings")).expect("bindings");
        let session = Session::spawn(
            SessionSpec {
                id: request.session_id.clone(),
                pty: PtySpec::new(request.argv.clone(), "/").size(cols, rows),
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
        Self {
            _temporary: temporary,
            control,
            session,
            attaches: 1,
        }
    }

    fn wait_connected(&self) {
        wait_until("connected", Duration::from_secs(10), || {
            connection_state(&self.session) == Some(RemoteConnectionState::Connected)
        });
    }

    /// Kills the current Bridge, keeps the transport down for `outage`, then
    /// lets the Engine's waiting reconnect through and times it until the
    /// Engine has a validated seed again.
    fn drop_and_reconnect(&mut self, outage: Duration) -> (Duration, SeedCost) {
        self.drop_and_reconnect_observing(outage, |_| {})
    }

    /// As [`Self::drop_and_reconnect`], running `while_down` once the Engine
    /// has noticed the drop and before the transport comes back.
    fn drop_and_reconnect_observing(
        &mut self,
        outage: Duration,
        while_down: impl FnOnce(&Session),
    ) -> (Duration, SeedCost) {
        let current = self.attaches;
        // The fake ssh runs in its own session (the Engine setsids it), so
        // its process group is the whole local+remote transport: ssh dying.
        let ssh = fs::read_to_string(self.control.join(format!("ssh.{current}"))).expect("ssh pid");
        let group: libc::pid_t = ssh.trim().parse().expect("ssh pid");
        // Signal the group directly: Ubuntu 24.04's procps `kill -9 -<pgid>`
        // reads the group as a second signal option, kills nothing and still
        // exits 0, so on Linux the transport never dropped.
        // SAFETY: a plain kill(2) on the fake ssh's own process group.
        let killed = unsafe { libc::kill(-group, libc::SIGKILL) };
        assert_eq!(
            killed,
            0,
            "kill transport group: {}",
            std::io::Error::last_os_error()
        );
        let next = current + 1;
        wait_until("Engine notices the drop", Duration::from_secs(10), || {
            connection_state(&self.session) == Some(RemoteConnectionState::Reconnecting)
        });
        wait_until("reconnect ssh is waiting", Duration::from_secs(10), || {
            fs::read_to_string(self.control.join("count"))
                .is_ok_and(|count| count.trim() == next.to_string())
        });
        std::thread::sleep(outage);
        while_down(&self.session);
        let started = Instant::now();
        fs::write(self.control.join(format!("allow.{next}")), b"").expect("allow");
        let down = self.control.join(format!("down.{next}"));
        let mut first_byte = None;
        wait_until("validated reseed", Duration::from_secs(10), || {
            if first_byte.is_none() && fs::metadata(&down).is_ok_and(|m| m.len() > 0) {
                first_byte = Some(started.elapsed());
            }
            connection_state(&self.session) == Some(RemoteConnectionState::Connected)
        });
        if std::env::var_os("DIRI_RECONNECT_DEBUG").is_some() {
            eprintln!(
                "first downstream byte {first_byte:?}, connected {:?}",
                started.elapsed()
            );
        }
        let elapsed = started.elapsed();
        self.attaches = next;
        // Let the seed frames land in the recording before decoding them.
        std::thread::sleep(Duration::from_millis(100));
        (elapsed, seed_cost(&self.control, next))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.session.terminate(Duration::from_millis(200));
    }
}

fn busy_screen(ticker: bool) -> String {
    let fill = "i=0; while [ $i -lt 120 ]; do printf '\\033[3%dmrow %03d\\033[0m %s\\n' \
                $((i % 7 + 1)) $i 'the quick brown fox jumps over the lazy dog 0123456789 \
                the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnop'; \
                i=$((i + 1)); done; printf 'ready>';";
    if ticker {
        format!(
            "{fill} t=0; while :; do printf '\\0337\\033[1;140Htick %05d\\0338' $t; \
             t=$((t + 1)); sleep 0.2; done"
        )
    } else {
        format!("{fill} IFS= read -r line; printf 'bye:%s\\n' \"$line\"")
    }
}

fn percentile(samples: &mut [Duration], percentile: usize) -> Duration {
    samples.sort();
    let index = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
    samples[index.min(samples.len() - 1)]
}

/// Opt-in measurement: `cargo test -p diri-remote --test reconnect_resume
/// -- --ignored --nocapture reconnect_cost`. Prints one table row per
/// scenario; asserts nothing about speed.
#[test]
#[ignore = "measurement; run explicitly"]
fn reconnect_cost_after_forced_transport_drop() {
    let reps: usize = std::env::var("DIRI_RECONNECT_REPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(7);
    for (label, ticker) in [("idle", false), ("ticking", true)] {
        let mut fixture = Fixture::spawn(&format!("cost-{label}"), &busy_screen(ticker), 160, 48);
        fixture.wait_connected();
        wait_until("screen filled", Duration::from_secs(10), || {
            fixture.session.screen_lines().join("\n").contains("ready>")
        });
        std::thread::sleep(Duration::from_millis(300));
        let mut times = Vec::new();
        let mut last = SeedCost::default();
        for _ in 0..reps {
            let before = fixture.session.screen_lines();
            let (elapsed, cost) = fixture.drop_and_reconnect(Duration::from_millis(1000));
            if !ticker {
                assert_eq!(
                    fixture.session.screen_lines(),
                    before,
                    "idle screen changed"
                );
            }
            times.push(elapsed);
            last = cost;
        }
        let p50 = percentile(&mut times, 50);
        let p90 = percentile(&mut times, 90);
        eprintln!(
            "RECONNECT {label}: 160x48, {reps} drops of 1 s | up Hello {} B | down to ControlGranted {} B \
             (snapshot {} B / {} rows, delta {} B / {} rows, replay {} B) | time-to-seed p50 {:.1} ms p90 {:.1} ms | {}",
            last.upstream_hello_bytes,
            last.downstream_bytes,
            last.snapshot_bytes,
            last.snapshot_rows,
            last.delta_bytes,
            last.delta_rows,
            last.replay_bytes,
            p50.as_secs_f64() * 1000.0,
            p90.as_secs_f64() * 1000.0,
            last.kinds.join(","),
        );
    }
}

/// Same scenario through the real Engine: a dropped transport comes back as a
/// delta of the rows that changed, the display keeps its grid, and input is
/// admitted again afterwards.
#[test]
fn engine_reconnect_resumes_without_a_full_snapshot() {
    let mut fixture = Fixture::spawn("engine-resume", &busy_screen(false), 120, 40);
    fixture.wait_connected();
    wait_until("screen filled", Duration::from_secs(10), || {
        fixture.session.screen_lines().join("\n").contains("ready>")
    });
    std::thread::sleep(Duration::from_millis(200));
    let before = fixture.session.screen_lines();
    // A display's view of the grid: drain what it has already been sent.
    let mut display = diri_engine::GridSignature::default();
    while fixture
        .session
        .grid_update_if_changed(&mut display)
        .is_some()
    {}
    let (_, cost) = fixture.drop_and_reconnect_observing(Duration::from_millis(100), |session| {
        assert_eq!(
            session.grid_update_if_changed(&mut display),
            None,
            "a transport drop alone repaints nothing"
        );
    });
    let after = fixture
        .session
        .grid_update_if_changed(&mut display)
        .expect("the resumed seed reaches the display");
    assert!(
        !after.is_full_snapshot && after.changed_rows.is_empty(),
        "the display keeps every row it already shows: {after:?}"
    );
    assert!(
        !cost.kinds.contains(&"FullSnapshot") && cost.kinds.contains(&"GridDelta"),
        "resumed seed: {:?}",
        cost.kinds
    );
    assert_eq!(cost.delta_rows, 0, "nothing changed while away");
    assert_eq!(fixture.session.screen_lines(), before);
    fixture
        .session
        .write_input(b"after-resume\n")
        .expect("input after a resumed seed");
    wait_until("echo after resume", Duration::from_secs(10), || {
        fixture
            .session
            .screen_lines()
            .join("\n")
            .contains("bye:after-resume")
    });
}

/// Clear Screen and Scrollback on a remote session: the Engine asks the
/// Holder (terminal-clear-v1), the Holder clears its own grid and history and
/// the child keeps running with the prompt line it was showing.
#[test]
fn remote_clear_keeps_the_prompt_line_and_drops_history() {
    let fixture = Fixture::spawn("remote-clear", &busy_screen(false), 120, 40);
    fixture.wait_connected();
    wait_until("screen filled", Duration::from_secs(10), || {
        fixture.session.screen_lines().join("\n").contains("ready>")
    });
    let history = fixture.session.read_scrollback_cells(0, 1);
    assert!(
        history.live_start_row > 0,
        "the fixture scrolled rows into history first: {history:?}"
    );
    let mut display = diri_engine::GridSignature::default();
    while fixture
        .session
        .grid_update_if_changed(&mut display)
        .is_some()
    {}

    assert!(fixture.session.clear_scrollback().expect("remote clear"));
    wait_until("cleared grid", Duration::from_secs(10), || {
        fixture.session.screen_lines() == vec!["ready>".to_owned()]
    });
    let update = fixture
        .session
        .grid_update_if_changed(&mut display)
        .expect("the clear reaches the display");
    assert!(
        update.is_full_snapshot,
        "a display never diffs against pre-clear cells"
    );
    wait_until("history dropped", Duration::from_secs(10), || {
        fixture.session.read_scrollback_cells(0, 1).live_start_row == 0
    });

    fixture
        .session
        .write_input(b"after-clear\n")
        .expect("input after a clear");
    wait_until("child still answers", Duration::from_secs(10), || {
        fixture
            .session
            .screen_lines()
            .join("\n")
            .contains("bye:after-clear")
    });
}

// ---------------------------------------------------------------------------
// Holder-level harness: a real Holder and PTY, attached over its Unix socket
// directly so each test controls exactly which frames a controller receives.

fn run_launch(state_dir: &Path, request: &LaunchRequest) -> diri_proto::remote_pty::LaunchResult {
    let home = state_dir
        .parent()
        .expect("state parent")
        .join("fixture-home");
    fs::create_dir_all(&home).expect("fixture home");
    let mut child = std::process::Command::new(helper())
        .arg("launch")
        .env("DIRI_REMOTE_STATE_DIR", state_dir)
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn launch");
    serde_json::to_writer(child.stdin.as_mut().expect("stdin"), request).expect("request");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("launch");
    assert!(
        output.status.success(),
        "launch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("launch result")
}

/// Mirrors `diri-remote`'s socket placement for a session.
fn holder_socket(session_id: &str) -> PathBuf {
    use sha2::Digest as _;
    let fallback = || {
        // SAFETY: getuid has no preconditions.
        std::env::temp_dir().join(format!("diri-remote-{}", unsafe { libc::getuid() }))
    };
    let mut root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("diri"))
        .unwrap_or_else(fallback);
    if root.as_os_str().len() + 38 >= 100 {
        root = fallback();
    }
    let digest = sha2::Sha256::digest(session_id.as_bytes());
    let name: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    root.join(format!("{name}.sock"))
}

struct Holder {
    _temporary: tempfile::TempDir,
    state_dir: PathBuf,
    launch: diri_proto::remote_pty::LaunchResult,
    marker: PathBuf,
}

impl Holder {
    /// `program` may wait for `$MARK` (a path) to change its screen while no
    /// controller is attached.
    fn launch(label: &str, program: &str, cols: u16, rows: u16) -> Self {
        let temporary = tempfile::tempdir().expect("temp");
        let state_dir = temporary.path().join("state");
        let marker = temporary.path().join("mark");
        let request = LaunchRequest {
            session_id: unique_session_id(label),
            session_token: token(),
            argv: vec!["/bin/sh".into(), "-c".into(), program.into()],
            cwd: "/".into(),
            environment: vec![
                EnvironmentVariable {
                    name: "TERM".into(),
                    value: "xterm-256color".into(),
                },
                EnvironmentVariable {
                    name: "MARK".into(),
                    value: marker.display().to_string(),
                },
            ],
            cols,
            rows,
            persistence: PersistenceCapability::NonPersistent,
            keyboard_enhancements: false,
        };
        let launch = run_launch(&state_dir, &request);
        Self {
            _temporary: temporary,
            state_dir,
            launch,
            marker,
        }
    }

    fn hello(&self, minor: u16, grid_resume: Option<GridResume>) -> Hello {
        Hello {
            protocol: ProtocolVersion {
                major: diri_proto::remote_pty::PROTOCOL_MAJOR,
                minor,
            },
            local_build_id: "resume-test".into(),
            session_id: self.launch.session_id.clone(),
            session_token: token(),
            expected_incarnation: Some(self.launch.session_incarnation.clone()),
            requested_role: RemoteRole::Controller,
            client_nonce: "resume-test-nonce".into(),
            required_capabilities: vec![
                RemoteCapability::FullSnapshot,
                RemoteCapability::IncrementalGrid,
                RemoteCapability::ControllerLease,
                RemoteCapability::TerminalReset,
            ],
            last_acknowledged_output_offset: None,
            last_acknowledged_grid_sequence: None,
            grid_resume,
        }
    }

    fn attach(&self, hello: Hello) -> Controller {
        let mut stream = std::os::unix::net::UnixStream::connect(holder_socket(&hello.session_id))
            .expect("connect Holder");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let encoded = RemoteCodec::encode(&RemoteMessage::Hello(hello)).expect("encode Hello");
        let started = Instant::now();
        stream.write_all(&encoded).expect("write Hello");
        Controller {
            stream,
            codec: RemoteCodec::new(),
            queued: std::collections::VecDeque::new(),
            started,
            upstream_bytes: encoded.len(),
        }
    }

    fn change_screen_while_detached(&self) {
        fs::write(&self.marker, b"").expect("marker");
        let done = PathBuf::from(format!("{}.done", self.marker.display()));
        wait_until("program changed its screen", Duration::from_secs(5), || {
            done.exists()
        });
        // The Holder parses PTY output on its own loop.
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let mut child = std::process::Command::new(helper())
            .arg("kill")
            .env("DIRI_REMOTE_STATE_DIR", &self.state_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kill");
        let _ = serde_json::to_writer(
            child.stdin.as_mut().expect("stdin"),
            &diri_proto::remote_pty::SessionSelector {
                session_id: self.launch.session_id.clone(),
                session_token: token(),
                expected_incarnation: Some(self.launch.session_incarnation.clone()),
            },
        );
        drop(child.stdin.take());
        let _ = child.wait();
    }
}

fn token() -> SessionToken {
    SessionToken::new("0123456789abcdef0123456789abcdef").expect("token")
}

struct Controller {
    stream: std::os::unix::net::UnixStream,
    codec: RemoteCodec,
    queued: std::collections::VecDeque<RemoteMessage>,
    started: Instant,
    upstream_bytes: usize,
}

impl Controller {
    fn next(&mut self) -> RemoteMessage {
        use std::io::Read as _;
        loop {
            if let Some(message) = self.queued.pop_front() {
                return message;
            }
            let mut bytes = [0_u8; 64 * 1024];
            let count = self.stream.read(&mut bytes).expect("read Holder");
            assert!(count > 0, "Holder closed the attach");
            self.queued
                .extend(self.codec.feed(&bytes[..count]).expect("decode"));
        }
    }
}

/// A controller's view of the terminal: what the Engine keeps in its mirror.
#[derive(Default)]
struct View {
    mirror: GridMirror,
    epoch: u64,
    reset_generation: u64,
}

/// What one attach delivered up to and including `ControlGranted`.
#[derive(Debug)]
struct Seed {
    outcome: Option<GridResumeOutcome>,
    epoch: u64,
    snapshot_rows: Option<usize>,
    delta_rows: Option<usize>,
    delta_text: String,
    downstream_bytes: usize,
    upstream_bytes: usize,
    elapsed: Duration,
}

impl View {
    fn resume(&self) -> GridResume {
        let mut key = [0_u8; diri_proto::remote_pty::GRID_RESUME_KEY_BYTES];
        getrandom::fill(&mut key).expect("key");
        let (cols, rows) = self.mirror.size();
        GridResume::new(
            self.epoch,
            self.mirror.sequence().expect("seeded"),
            self.reset_generation,
            cols,
            rows,
            &key,
            &self.mirror.row_digests(&key).expect("digests"),
        )
    }

    /// Reads one attach's seed, applying it the way the Engine does. With
    /// `stop_after_ack` the controller vanishes right after `HelloAck`.
    fn seed(&mut self, controller: &mut Controller, stop_after_ack: bool) -> Seed {
        let mut seed = Seed {
            outcome: None,
            epoch: 0,
            snapshot_rows: None,
            delta_rows: None,
            delta_text: String::new(),
            downstream_bytes: 0,
            upstream_bytes: controller.upstream_bytes,
            elapsed: Duration::ZERO,
        };
        let mut accepted = None;
        loop {
            let message = controller.next();
            seed.downstream_bytes += RemoteCodec::encode(&message).expect("encode").len();
            match message {
                RemoteMessage::HelloAck(ack) => {
                    seed.epoch = ack.controller_epoch;
                    seed.outcome = ack.grid_resume.clone();
                    if let Some(GridResumeOutcome::Accepted { base_sequence }) = ack.grid_resume {
                        accepted = Some(base_sequence);
                    }
                    if stop_after_ack {
                        // The epoch was granted on the Holder, but this
                        // controller never learns of it: an interrupted resume.
                        seed.elapsed = controller.started.elapsed();
                        return seed;
                    }
                    self.epoch = ack.controller_epoch;
                }
                RemoteMessage::TerminalResetState(reset) => {
                    self.reset_generation = reset.generation;
                }
                RemoteMessage::FullSnapshot(snapshot) => {
                    seed.snapshot_rows = Some(snapshot.grid.changed_rows.len());
                    self.mirror
                        .apply_snapshot(
                            snapshot.sequence,
                            &snapshot.grid,
                            snapshot.alt_screen,
                            snapshot.bracketed_paste,
                            snapshot.mouse,
                        )
                        .expect("snapshot");
                }
                RemoteMessage::GridDelta(delta) => {
                    if let Some(base) = accepted.take() {
                        seed.delta_rows = Some(delta.grid.changed_rows.len());
                        seed.delta_text = delta
                            .grid
                            .changed_rows
                            .iter()
                            .flat_map(|row| &row.cells)
                            .filter_map(|cell| char::from_u32(cell.scalar))
                            .collect();
                        self.mirror
                            .apply_resumed_delta(
                                base,
                                delta.sequence,
                                &delta.grid,
                                delta.alt_screen,
                                delta.bracketed_paste,
                                delta.mouse,
                            )
                            .expect("resumed delta");
                    } else {
                        self.mirror
                            .apply_delta(
                                delta.sequence,
                                &delta.grid,
                                delta.alt_screen,
                                delta.bracketed_paste,
                                delta.mouse,
                            )
                            .expect("delta");
                    }
                }
                RemoteMessage::ControlGranted(_) => {
                    seed.elapsed = controller.started.elapsed();
                    return seed;
                }
                RemoteMessage::Error(error) => panic!("Holder error: {error:?}"),
                _ => {}
            }
        }
    }

    /// The exact bytes a display would hold: grid, cursor and modes.
    fn image(&self) -> (Vec<u8>, (bool, bool, diri_proto::terminal::MouseModes)) {
        (
            self.mirror
                .full_update()
                .expect("seeded")
                .encode()
                .expect("encode"),
            self.mirror.modes(),
        )
    }
}

/// The Holder's grid right now, as a fresh controller with no resume sees it.
fn authoritative_image(
    holder: &Holder,
) -> (Vec<u8>, (bool, bool, diri_proto::terminal::MouseModes)) {
    let mut fresh = View::default();
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
    let seed = fresh.seed(&mut controller, false);
    assert!(seed.snapshot_rows.is_some());
    fresh.image()
}

fn screen_changing_on_mark() -> String {
    // Twelve rows of colored text, then one more line only once `$MARK`
    // exists, so the change happens while no controller is attached.
    "i=0; while [ $i -lt 12 ]; do printf '\\033[3%dmrow %02d\\033[0m resume me\\n' \
     $((i % 7 + 1)) $i; i=$((i + 1)); done; printf 'ready>'; \
     while [ ! -e \"$MARK\" ]; do sleep 0.01; done; printf 'changed while away'; \
     : > \"$MARK.done\"; while :; do sleep 1; done"
        .into()
}

fn first_seed(holder: &Holder, view: &mut View) {
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
    let seed = view.seed(&mut controller, false);
    assert!(seed.snapshot_rows.is_some(), "first attach is a snapshot");
    wait_until("prompt", Duration::from_secs(5), || {
        let mut fresh = View::default();
        let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
        fresh.seed(&mut controller, false);
        let text: String = fresh
            .mirror
            .cells()
            .iter()
            .filter_map(|cell| char::from_u32(cell.scalar))
            .collect();
        text.contains("ready>")
    });
    // Those probes took the lease; re-seed this view as the last controller.
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
    view.seed(&mut controller, false);
}

#[test]
fn resume_within_the_window_sends_only_changed_rows_and_reproduces_the_grid() {
    let holder = Holder::launch("resume-within", &screen_changing_on_mark(), 80, 24);
    let mut view = View::default();
    first_seed(&holder, &mut view);
    let unchanged = view.image();

    holder.change_screen_while_detached();
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
    let seed = view.seed(&mut controller, false);
    assert!(
        matches!(seed.outcome, Some(GridResumeOutcome::Accepted { .. })),
        "{seed:?}"
    );
    assert_eq!(seed.snapshot_rows, None, "no Full Snapshot on resume");
    assert_eq!(
        seed.delta_rows,
        Some(1),
        "only the row that changed: {seed:?}"
    );
    assert!(seed.delta_text.contains("changed while away"));
    assert_ne!(view.image(), unchanged);
    drop(controller);
    assert_eq!(
        view.image(),
        authoritative_image(&holder),
        "resumed grid must be byte-identical to a fresh snapshot"
    );

    // A resume with nothing changed carries no rows at all.
    let mut view = View::default();
    first_seed(&holder, &mut view);
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
    let seed = view.seed(&mut controller, false);
    assert!(matches!(
        seed.outcome,
        Some(GridResumeOutcome::Accepted { .. })
    ));
    assert_eq!(seed.delta_rows, Some(0));
    drop(controller);
    assert_eq!(view.image(), authoritative_image(&holder));
}

#[test]
fn resume_outside_the_window_falls_back_to_a_full_snapshot() {
    let holder = Holder::launch("resume-outside", &screen_changing_on_mark(), 80, 24);
    let mut view = View::default();
    first_seed(&holder, &mut view);
    holder.change_screen_while_detached();

    // The controller's grid predates a terminal reset it never saw.
    let mut stale_reset = view.resume();
    stale_reset.reset_generation += 1;
    // The controller's grid has other dimensions than the Holder's.
    let mut other_geometry = view.mirror.clone();
    other_geometry
        .apply_snapshot(
            other_geometry.sequence().unwrap(),
            &diri_proto::grid::GridUpdate {
                cols: 80,
                rows: 23,
                cursor_col: 0,
                cursor_row: 0,
                cursor_visible: true,
                is_full_snapshot: true,
                changed_rows: Vec::new(),
                cursor_style: Default::default(),
                history: None,
            },
            false,
            false,
            Default::default(),
        )
        .unwrap();
    let geometry = View {
        mirror: other_geometry,
        epoch: view.epoch,
        reset_generation: view.reset_generation,
    }
    .resume();
    for (mut request, reason) in [
        (stale_reset, GridResumeDecline::ResetGenerationChanged),
        (geometry, GridResumeDecline::GeometryChanged),
    ] {
        request.controller_epoch = view.epoch;
        let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(request)));
        let seed = view.seed(&mut controller, false);
        assert_eq!(seed.outcome, Some(GridResumeOutcome::Declined { reason }));
        assert_eq!(seed.snapshot_rows, Some(24), "authoritative Full Snapshot");
        assert_eq!(seed.delta_rows, None);
        drop(controller);
        assert_eq!(view.image(), authoritative_image(&holder));
        // The fresh probe above took the lease; take it back.
        let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
        view.seed(&mut controller, false);
    }
}

#[test]
fn resume_naming_a_stale_controller_epoch_is_declined() {
    let holder = Holder::launch("resume-stale", &screen_changing_on_mark(), 80, 24);
    let mut view = View::default();
    first_seed(&holder, &mut view);
    let stale = view.resume();
    let stale_epoch = view.epoch;

    // Another controller attaches while this one is away.
    let mut other = View::default();
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
    let taken = other.seed(&mut controller, false);
    assert!(taken.epoch > stale_epoch);
    drop(controller);
    holder.change_screen_while_detached();

    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(stale)));
    let seed = view.seed(&mut controller, false);
    assert_eq!(
        seed.outcome,
        Some(GridResumeOutcome::Declined {
            reason: GridResumeDecline::StaleControllerEpoch
        })
    );
    assert!(seed.snapshot_rows.is_some(), "falls back to a snapshot");
    assert!(
        seed.epoch > taken.epoch,
        "the attach still takes a new lease"
    );
    // The stale epoch still cannot act.
    controller
        .stream
        .write_all(
            &RemoteCodec::encode(&RemoteMessage::Signal(diri_proto::remote_pty::Signal {
                controller_epoch: stale_epoch,
                signal: 2,
            }))
            .unwrap(),
        )
        .unwrap();
    let rejected = loop {
        if let RemoteMessage::Error(error) = controller.next() {
            break error;
        }
    };
    assert!(rejected.fatal);
    assert_eq!(rejected.code, "protocol_error");
    drop(controller);
    assert_eq!(view.image(), authoritative_image(&holder));
}

#[test]
fn a_controller_without_grid_resume_v1_keeps_the_full_snapshot() {
    let holder = Holder::launch("resume-old", &screen_changing_on_mark(), 80, 24);
    let mut view = View::default();
    first_seed(&holder, &mut view);
    holder.change_screen_while_detached();
    // A protocol 1.16 controller cannot negotiate the capability, even if
    // its Hello carried the field: the Holder answers exactly as before.
    let mut controller = holder.attach(holder.hello(
        diri_proto::remote_pty::GRID_RESUME_PROTOCOL_MINOR - 1,
        Some(view.resume()),
    ));
    let mut ack = None;
    let mut kinds = Vec::new();
    loop {
        let message = controller.next();
        kinds.push(kind(&message));
        if let RemoteMessage::HelloAck(value) = &message {
            ack = Some(value.clone());
        }
        if matches!(message, RemoteMessage::ControlGranted(_)) {
            break;
        }
    }
    let ack = ack.expect("HelloAck");
    assert_eq!(ack.grid_resume, None);
    assert!(!ack.capabilities.contains(&RemoteCapability::GridResume));
    assert_eq!(
        kinds,
        [
            "HelloAck",
            "TerminalResetState",
            "InputModes",
            "FullSnapshot",
            "ControlGranted"
        ]
    );
}

#[test]
fn an_interrupted_resume_is_retried_or_reseeded_without_a_wrong_grid() {
    let holder = Holder::launch("resume-interrupted", &screen_changing_on_mark(), 80, 24);
    let mut view = View::default();
    first_seed(&holder, &mut view);
    holder.change_screen_while_detached();

    // The transport drops after the Holder granted the lease and accepted the
    // resume, before its delta arrived. Nothing was applied to the view.
    let before = view.image();
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
    let interrupted = view.seed(&mut controller, true);
    assert!(matches!(
        interrupted.outcome,
        Some(GridResumeOutcome::Accepted { .. })
    ));
    drop(controller);
    assert_eq!(view.image(), before);

    // This controller never saw that HelloAck, so it still names the epoch
    // before it: the Holder cannot tell it from another controller's, and
    // reseeds with a snapshot.
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
    let seed = view.seed(&mut controller, false);
    assert_eq!(
        seed.outcome,
        Some(GridResumeOutcome::Declined {
            reason: GridResumeDecline::StaleControllerEpoch
        })
    );
    assert!(seed.snapshot_rows.is_some());
    drop(controller);
    assert_eq!(view.image(), authoritative_image(&holder));

    // Had the HelloAck arrived (the Engine records the epoch on HelloAck),
    // the retry resumes from the unchanged mirror.
    let mut view = View::default();
    first_seed(&holder, &mut view);
    let before = view.image();
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
    let interrupted = view.seed(&mut controller, true);
    drop(controller);
    view.epoch = interrupted.epoch;
    assert_eq!(view.image(), before);
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
    let seed = view.seed(&mut controller, false);
    assert!(matches!(
        seed.outcome,
        Some(GridResumeOutcome::Accepted { .. })
    ));
    assert_eq!(seed.snapshot_rows, None);
    drop(controller);
    assert_eq!(view.image(), authoritative_image(&holder));
}

/// Opt-in measurement of the Holder side alone, without process spawns:
/// alternating snapshot and resume reattaches to the same busy Holder.
#[test]
#[ignore = "measurement; run explicitly"]
fn holder_reattach_cost_snapshot_versus_resume() {
    let reps: usize = std::env::var("DIRI_RECONNECT_REPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(25);
    let holder = Holder::launch("resume-cost", &busy_screen(true), 160, 48);
    let mut view = View::default();
    std::thread::sleep(Duration::from_millis(500));
    let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
    view.seed(&mut controller, false);
    drop(controller);
    let mut snapshot = (Vec::new(), 0, 0);
    let mut resume = (Vec::new(), 0, 0);
    for _ in 0..reps {
        std::thread::sleep(Duration::from_millis(250));
        let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, None));
        let seed = view.seed(&mut controller, false);
        drop(controller);
        snapshot.0.push(seed.elapsed);
        (snapshot.1, snapshot.2) = (seed.upstream_bytes, seed.downstream_bytes);
        std::thread::sleep(Duration::from_millis(250));
        let mut controller = holder.attach(holder.hello(PROTOCOL_MINOR, Some(view.resume())));
        let seed = view.seed(&mut controller, false);
        drop(controller);
        assert!(seed.snapshot_rows.is_none());
        resume.0.push(seed.elapsed);
        (resume.1, resume.2) = (seed.upstream_bytes, seed.downstream_bytes);
    }
    for (label, (mut times, up, down)) in [("snapshot", snapshot), ("resume", resume)] {
        eprintln!(
            "HOLDER {label}: 160x48 busy, {reps} reattaches | up {up} B | down {down} B | \
             Hello-to-ControlGranted p50 {:.2} ms p90 {:.2} ms",
            percentile(&mut times, 50).as_secs_f64() * 1000.0,
            percentile(&mut times, 90).as_secs_f64() * 1000.0,
        );
    }
}
