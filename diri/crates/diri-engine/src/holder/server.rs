//! The per-session holder: owns exactly one PTY and child tree.
//!
//! The holder has no daemon dependencies. Its entire control interface is one
//! request/response NDJSON line per unix-socket connection, and its durable
//! output interface is the [`OutputLog`] file. When the child exits the
//! holder appends an in-band [`HolderExitMarker`] to the log — so a daemon
//! that wasn't running at the time still learns how the child died — then
//! removes its control files and stops serving.
//!
//! A holder has no thread of its own. Each of its lanes (the PTY reader, the
//! control socket, the input stream, each output subscriber, the exit watch)
//! runs on a thread only while it has work, and parks its descriptor on the
//! process's [`reactor`] once it has been idle for [`reactor::LANE_IDLE`]. A
//! parked lane costs no thread and no wakeup; its next event starts a thread
//! again. At any moment exactly one party owns each lane's descriptor.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine as _;

use crate::holder::HolderPaths;
use crate::log::OutputLog;
use crate::pty::{Pty, PtySpec};

use super::client::HolderClient;
use super::guard::GroupGuard;
use super::process_tree;
use super::protocol::{
    HOLDER_OUTPUT_STREAM_VERSION, HOLDER_STREAM_ACK, HOLDER_STREAM_INPUT,
    HOLDER_STREAM_MAX_PAYLOAD, HOLDER_STREAM_RESIZE, HOLDER_STREAM_RESIZE_PIXELS,
    HOLDER_STREAM_VERSION, HOLDER_STREAM_VERSION_PIXELS, HolderChildRecord, HolderExitMarker,
    HolderExitReason, HolderExitStatus, HolderLaunchSpec, HolderOperation, HolderRequest,
    HolderResponse, HolderStat,
};
use super::reactor;
use super::socket;
use super::{HolderError, HolderResult};

/// One past the highest acceptable signal number, as Swift validated with
/// `NSIG`.
#[cfg(target_os = "macos")]
const MAX_SIGNAL: i32 = 32;
#[cfg(not(target_os = "macos"))]
const MAX_SIGNAL: i32 = 65;

/// How many bytes may be waiting for one output subscriber. This is what
/// bounds how far output can run ahead of the screen rendering it, so it is
/// deliberately short: a megabyte of slack, not sixteen. It is counted in
/// bytes because a PTY read is a kilobyte: sixteen of those was all the slack
/// a count of frames allowed.
const OUTPUT_QUEUE_BYTES: usize = 1 << 20;

/// How long the pump waits for a full subscriber queue while the subscriber
/// takes nothing, before giving up on it.
///
/// A subscriber that is still reading — the queue, or its log backlog — is
/// waited for: the daemon is where the session's terminal state is parsed, so
/// output must not outrun it, exactly as a terminal emulator's own PTY reads
/// pace a program. A subscriber dropped here lets the program run free until
/// it resubscribes, and if the log rotates past it meanwhile, those bytes are
/// never parsed.
///
/// So this has to outlast one parse pass of a busy daemon (half a megabyte:
/// a few milliseconds in a release build, a hundred in a debug one, more on a
/// loaded machine), while still bounding how long a hung daemon can hold a
/// program's output: once, for this long, after which it runs free.
const OUTPUT_SEND_PATIENCE: Duration = Duration::from_millis(500);

/// Buffer for one subscriber's socket writes.
const OUTPUT_WRITE_BUFFER: usize = 256 << 10;

/// Live holders, so a test can inspect the state `run` builds for itself.
#[cfg(test)]
static RUNNING: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

pub struct HolderServer;

/// How a hosted Agent is started on macOS; nothing elsewhere.
#[cfg(target_os = "macos")]
pub use diri_pty::detached::{DetachedLeader, Launcher as AgentLauncher};
/// Placeholders off macOS, where every Agent is the Holder's child.
#[cfg(not(target_os = "macos"))]
pub enum AgentLauncher {}
#[cfg(not(target_os = "macos"))]
pub enum DetachedLeader {}
#[cfg(not(target_os = "macos"))]
impl DetachedLeader {
    fn wait(&self) -> std::io::Result<diri_pty::Exit> {
        match *self {}
    }
}

struct Shared {
    spec: HolderLaunchSpec,
    child_pid: i32,
    child_identity: Option<diri_proto::process::ProcessIdentity>,
    /// What the last SIGSTOP stopped, until a SIGCONT resumes it. If the
    /// leader exits while these are still frozen they would stay stopped
    /// forever, so the exit path kills them (see
    /// [`process_tree::kill_stragglers`]).
    frozen: Mutex<Vec<super::protocol::HolderProcessSample>>,
    /// The manager's guard, told about the session's group and frozen
    /// processes so they die with the manager too.
    guard: Option<Arc<GroupGuard>>,
    /// The PTY, kept for write/resize/stat access. The master stays open for
    /// the holder's whole life; it closes once the last handle on this state
    /// is gone after the exit path has finished.
    pty: Mutex<Pty>,
    log: Mutex<OutputLog>,
    /// Signalled after every append, for a resumed subscriber whose backlog
    /// reaches bytes the log writer has not appended yet.
    log_grew: std::sync::Condvar,
    /// The log's tail after its last completed append, so a stat need not
    /// wait for the log lock. The writer holds that lock across a disk write,
    /// and a stat queued behind it held the PTY lock meanwhile, which made
    /// keystrokes wait on the filesystem.
    log_tail: AtomicU64,
    /// Log tail at the moment this holder started: the boundary between prior
    /// incarnations' bytes and bytes attributable to THIS child.
    epoch_offset: u64,
    finished: AtomicBool,
    /// Self-pipe the exit path writes to once `finished` is set. It is what
    /// lets a running pump wait on the PTY without a deadline short of its
    /// idle park, and still notice the exit at once. Both ends live here so
    /// the write can never meet a closed pipe.
    pump_wake: (std::io::PipeReader, std::io::PipeWriter),
    /// Every return from the pump's wait, so a test can prove a silent
    /// session leaves it parked.
    #[cfg(test)]
    pump_wakeups: std::sync::atomic::AtomicUsize,
    /// Who reads the PTY: a pump thread while output flows, the reactor once
    /// it has been quiet for [`reactor::LANE_IDLE`].
    pump: Mutex<PumpLane>,
    /// The control socket. Shared so a request thread can keep it open
    /// while the exit path withdraws it.
    listener: Mutex<Option<Arc<OwnedFd>>>,
    /// Whether the control socket's lane is parked on the reactor.
    accept: Mutex<Lane>,
    /// How the child's exit is watched; taken by the exit path.
    exit_watch: Mutex<ExitWatch>,
    /// Told once the exit path has finished, after the marker is durable.
    on_finish: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// When the child was spawned, for its recorded runtime.
    spawned_at: std::time::Instant,
    /// Weak handles let the exit path interrupt blocking input reads without
    /// making idle Holder streams wake on a timer.
    input_streams: Mutex<Vec<Weak<diri_platform::ipc::UnixStream>>>,
    /// Daemons receiving output as it is read, rather than by tailing the log.
    /// The log is still written, and is still what a subscriber falls back to;
    /// this only removes the filesystem from the path a live screen waits on.
    output: Mutex<OutputFanout>,
}

/// A lane that can hand its descriptor to the reactor.
#[derive(Default)]
struct Lane {
    /// Set by the exit path: the lane must not park again.
    closed: bool,
    /// The reactor registration while parked.
    parked: Option<reactor::Token>,
}

/// The PTY reader's lane. Exactly one party owns the read side at any time:
/// a pump thread (which took `reader`), or the reactor (while `parked`).
#[derive(Default)]
struct PumpLane {
    reader: Option<crate::pty::PtyStream>,
    /// The latest pump thread, for the exit path to join.
    thread: Option<std::thread::JoinHandle<()>>,
    parked: Option<reactor::Token>,
    finished: bool,
}

#[derive(Default)]
struct ExitWatch {
    watcher: Option<diri_pty::ExitWatcher>,
    detached: Option<Arc<DetachedLeader>>,
}

impl ExitWatch {
    /// The descriptor that turns readable when the child exits.
    fn descriptor(&self) -> Option<i32> {
        #[cfg(target_os = "macos")]
        if let Some(leader) = &self.detached {
            return Some(leader.exit_descriptor());
        }
        self.watcher.as_ref().map(diri_pty::ExitWatcher::as_raw_fd)
    }
}

/// Subscribers, and where in the stream the next byte handed to them sits.
///
/// The offset is tracked here rather than read from the log because the log is
/// written asynchronously: bytes already read from the PTY can still be in
/// flight to it. A subscriber told to start at the log tail would receive its
/// first frame from further along and take it for an earlier one, which is a
/// gap the emulator has no way to notice.
struct OutputFanout {
    next_offset: u64,
    subscribers: Vec<OutputSubscriber>,
}

/// One attached daemon's view of the output stream.
///
/// Delivery is bounded and off the pump's thread: a subscriber that keeps up
/// costs the pump a channel send, and one that does not is dropped rather than
/// allowed to stall the PTY. A dropped subscriber loses nothing, because every
/// byte is in the log it will fall back to.
///
/// Its writer thread exists only while frames flow: it ends after
/// [`reactor::LANE_IDLE`] with nothing to send, and the next frame starts
/// another. A hangup is noticed by the reactor, not by a thread per socket.
struct OutputSubscriber {
    frames: Arc<super::fanout::FrameQueue>,
    link: Arc<SubscriberLink>,
}

/// A subscriber's socket. It closes once the fan-out entry, any writer and
/// any hangup watch have all let go of it, so a registration on it can never
/// outlive the descriptor.
struct SubscriberLink {
    stream: diri_platform::ipc::UnixStream,
    /// The reactor's hangup watch while armed.
    peer: Mutex<Option<reactor::Token>>,
}

impl SubscriberLink {
    /// Ends the subscription: the daemon reads EOF and falls back to the log.
    fn finish(&self) {
        if let (Some(token), Some(reactor)) = (
            self.peer.lock().expect("peer watch").take(),
            reactor::shared(),
        ) {
            reactor.cancel(token);
        }
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl HolderServer {
    /// Runs the holder to completion: spawns the child, serves control
    /// requests, and returns after the child has exited and the exit marker
    /// is durably in the log.
    pub fn run(spec: HolderLaunchSpec) -> HolderResult<()> {
        Self::run_guarded(spec, None)
    }

    /// [`Self::run`], registering the session's process group with the
    /// manager's [`GroupGuard`] for as long as its leader is unreaped.
    pub fn run_guarded(spec: HolderLaunchSpec, guard: Option<Arc<GroupGuard>>) -> HolderResult<()> {
        Self::run_hosted(spec, guard, None)
    }

    /// [`Self::run_guarded`], starting the Agent through `launcher` when one
    /// is given: on macOS that makes it a launchd job of its own, so no other
    /// session shares its process coalition (see [`diri_pty::detached`]).
    pub fn run_hosted(
        spec: HolderLaunchSpec,
        guard: Option<Arc<GroupGuard>>,
        launcher: Option<&AgentLauncher>,
    ) -> HolderResult<()> {
        let (finished, done) = std::sync::mpsc::channel();
        Self::start_hosted(
            spec,
            guard,
            launcher,
            Box::new(move || {
                let _ = finished.send(());
            }),
        )?;
        let _ = done.recv();
        Ok(())
    }

    /// Starts the holder and returns once it is serving. It then runs on
    /// threads of its own that exist only while it has work, and on the
    /// process's reactor while it has none; `on_finish` runs once the child
    /// has exited and the exit marker is durably in the log.
    pub fn start_hosted(
        spec: HolderLaunchSpec,
        guard: Option<Arc<GroupGuard>>,
        launcher: Option<&AgentLauncher>,
        on_finish: Box<dyn FnOnce() + Send>,
    ) -> HolderResult<()> {
        // Started here, on a thread at the priority sessions start with, so
        // lane threads it later starts can be given that priority too.
        let _ = reactor::shared();
        // Never double-run: a second holder for the same session would
        // interleave two writers into one output log and stack a second child.
        // If a live holder already serves this socket, defer to it — bail
        // before touching the log or spawning anything.
        if HolderClient::new(&spec.socket_path).is_alive() {
            return Err(HolderError::Launch(format!(
                "a live holder already serves {}; refusing to double-run",
                spec.socket_path
            )));
        }

        let socket_path = Path::new(&spec.socket_path).to_path_buf();
        if let Some(parent) = socket_path.parent() {
            diri_platform::private::create_dir_all(parent)
                .map_err(|error| HolderError::io("create socket directory", error))?;
        }

        let log = open_log(&spec)?;
        // Captured before the child can produce a single byte: everything
        // below this offset predates this incarnation.
        let epoch_offset = log.tail_offset();

        let mut env: Vec<(String, String)> = spec
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.sort(); // deterministic environ order, matching no one but ourselves
        let pty_spec = PtySpec {
            argv: spec.argv.clone(),
            env,
            cwd: spec.cwd.clone().into(),
            cols: spec.cols.max(2),
            rows: spec.rows.max(2),
        };
        let spawned_at = std::time::Instant::now();
        let pty = spawn_agent(&pty_spec, launcher, &spec.session_id).map_err(|error| {
            diri_telemetry::incident!(
                "holder.spawn_failed",
                session = diri_telemetry::id(&spec.session_id),
                io = diri_telemetry::io_error(&error),
            );
            HolderError::io("PTY spawn", error)
        })?;
        let child_pid = pty.pid() as i32;
        diri_telemetry::event!(
            "holder.spawn",
            session = diri_telemetry::id(&spec.session_id),
            cols = pty_spec.cols,
            rows = pty_spec.rows,
            ms = spawned_at.elapsed(),
        );
        // Armed at once: registering after the child has exited fails on
        // macOS, which the exit path treats as "already exited".
        let exit_watcher = diri_pty::ExitWatcher::new(child_pid as u32).ok();
        #[cfg(target_os = "macos")]
        let detached = pty.detached_leader();
        #[cfg(not(target_os = "macos"))]
        let detached: Option<Arc<DetachedLeader>> = None;
        // Recorded before the socket exists, so anyone who can reach this
        // Holder, or finds only its log, can also learn which child it forked.
        // Failure is not fatal: the run is then bindable only by a live stat.
        let _ = HolderChildRecord {
            child_pid,
            child_identity: pty.child_identity(),
            epoch_offset,
        }
        .write(&HolderPaths::child_record_beside(Path::new(
            &spec.pid_file_path,
        )));

        // Nonblocking master: the reader drains in bursts, and writes bound
        // their patience with poll rather than blocking the control loop.
        // O_NONBLOCK lives on the shared file description, so setting it on
        // this dup covers every handle.
        let reader = pty
            .reader()
            .map_err(|error| HolderError::io("PTY reader", error))?;
        set_nonblocking(reader.as_raw_fd());

        let listener: OwnedFd = socket::listen(&socket_path)?.into();
        // Nonblocking: a request thread drains every pending connection and
        // then waits with a deadline, so a racing connect never strands it.
        set_nonblocking(listener.as_raw_fd());

        let pump_wake =
            std::io::pipe().map_err(|error| HolderError::io("create pump wake pipe", error))?;

        let shared = Arc::new(Shared {
            child_pid,
            child_identity: pty.child_identity(),
            frozen: Mutex::new(Vec::new()),
            guard: guard.clone(),
            pty: Mutex::new(pty),
            log: Mutex::new(log),
            log_grew: std::sync::Condvar::new(),
            log_tail: AtomicU64::new(epoch_offset),
            epoch_offset,
            finished: AtomicBool::new(false),
            pump_wake,
            #[cfg(test)]
            pump_wakeups: std::sync::atomic::AtomicUsize::new(0),
            pump: Mutex::new(PumpLane {
                reader: Some(reader),
                ..PumpLane::default()
            }),
            listener: Mutex::new(Some(Arc::new(listener))),
            accept: Mutex::new(Lane::default()),
            exit_watch: Mutex::new(ExitWatch {
                watcher: exit_watcher,
                detached,
            }),
            on_finish: Mutex::new(Some(on_finish)),
            spawned_at,
            input_streams: Mutex::new(Vec::new()),
            output: Mutex::new(OutputFanout {
                // Everything below this belongs to earlier incarnations.
                next_offset: epoch_offset,
                subscribers: Vec::new(),
            }),
            spec,
        });

        #[cfg(test)]
        RUNNING
            .lock()
            .expect("running")
            .push(Arc::downgrade(&shared));

        write_pid_file(&shared.spec.pid_file_path)?;

        {
            let mut lane = shared.pump.lock().expect("pump");
            spawn_pump(&shared, &mut lane).map_err(|error| HolderError::io("spawn pump", error))?;
        }

        // Registered only once the exit watch, which alone releases it, is
        // certain to run.
        if let Some(guard) = &guard {
            guard.register(child_pid);
        }
        if let Err(error) = watch_exit(&shared) {
            if let Some(guard) = &guard {
                guard.release(child_pid);
            }
            return Err(HolderError::io("spawn exit watcher", error));
        }

        // A new session is about to be asked for its streams and a stat.
        spawn_control(&shared).map_err(|error| HolderError::io("spawn control", error))?;
        Ok(())
    }
}

/// Starts one of a session's lane threads, named for what it serves.
fn spawn_lane(
    kind: &str,
    session: &str,
    work: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    #[cfg(test)]
    let live = live_lanes::Live::enter(session);
    std::thread::Builder::new()
        .name(format!("holder-{kind}-{session}"))
        .spawn(move || {
            #[cfg(test)]
            let _live = live;
            reactor::lane_qos::adopt();
            work();
        })
}

/// Lane threads alive per session, so a test can prove an idle session holds
/// none.
#[cfg(test)]
mod live_lanes {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static LIVE: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

    pub(super) struct Live(String);

    impl Live {
        pub(super) fn enter(session: &str) -> Self {
            *LIVE
                .lock()
                .expect("live lanes")
                .get_or_insert_with(HashMap::new)
                .entry(session.to_string())
                .or_default() += 1;
            Self(session.to_string())
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            if let Some(count) = LIVE
                .lock()
                .expect("live lanes")
                .as_mut()
                .and_then(|live| live.get_mut(&self.0))
            {
                *count -= 1;
            }
        }
    }

    pub(super) fn count(session: &str) -> usize {
        LIVE.lock()
            .expect("live lanes")
            .as_ref()
            .and_then(|live| live.get(session).copied())
            .unwrap_or(0)
    }
}

/// Starts a pump thread to take the PTY's read side from `lane`.
fn spawn_pump(shared: &Arc<Shared>, lane: &mut PumpLane) -> std::io::Result<()> {
    let pump = Arc::clone(shared);
    let thread = spawn_lane("pty", &shared.spec.session_id, move || pump_pty(&pump))?;
    // The previous pump has parked or is on its way out; nothing to join.
    lane.thread = Some(thread);
    Ok(())
}

/// Runs on the reactor when a parked PTY turns readable.
fn wake_pump(holder: &Weak<Shared>) {
    let Some(shared) = holder.upgrade() else {
        return;
    };
    let mut lane = shared.pump.lock().expect("pump");
    lane.parked = None;
    if lane.finished {
        return;
    }
    if spawn_pump(&shared, &mut lane).is_err() {
        // No thread to be had right now. The PTY is still readable, so the
        // re-armed watch fires again at once: pause so that is a retry, not
        // a spin.
        std::thread::sleep(Duration::from_millis(10));
        if !park_pump(&shared, &mut lane) {
            // Nothing can carry the PTY: leave it to the exit path rather
            // than lose the reader.
            diri_telemetry::warn_event!(
                "holder.pump_unparkable",
                session = diri_telemetry::id(&shared.spec.session_id),
            );
        }
    }
}

/// Hands the PTY in `lane` to the reactor. False if it would not take it.
fn park_pump(shared: &Arc<Shared>, lane: &mut PumpLane) -> bool {
    let (Some(reactor), Some(reader)) = (reactor::shared(), lane.reader.as_ref()) else {
        return false;
    };
    let holder = Arc::downgrade(shared);
    match reactor.when_readable(reader.as_raw_fd(), move || wake_pump(&holder)) {
        Ok(token) => {
            lane.parked = Some(token);
            true
        }
        Err(_) => false,
    }
}

/// Starts a thread to serve control requests.
fn spawn_control(shared: &Arc<Shared>) -> std::io::Result<()> {
    let holder = Arc::downgrade(shared);
    spawn_lane("control", &shared.spec.session_id, move || {
        serve_control(&holder);
    })
    .map(drop)
}

/// Runs on the reactor when a parked control socket has a connection.
fn wake_control(holder: &Weak<Shared>) {
    let Some(shared) = holder.upgrade() else {
        return;
    };
    let mut lane = shared.accept.lock().expect("accept");
    lane.parked = None;
    if lane.closed {
        return;
    }
    if spawn_control(&shared).is_err() {
        std::thread::sleep(Duration::from_millis(10));
        let _ = park_control(&shared, &mut lane);
    }
}

/// Hands the control socket to the reactor. False if it would not take it.
fn park_control(shared: &Arc<Shared>, lane: &mut Lane) -> bool {
    let Some(reactor) = reactor::shared() else {
        return false;
    };
    let Some(listener) = shared.listener.lock().expect("listener").clone() else {
        return true; // withdrawn: nothing left to serve
    };
    let holder = Arc::downgrade(shared);
    let fd = listener.as_raw_fd();
    match reactor.when_readable(fd, move || {
        // Held open for as long as the registration exists.
        let _listener = listener;
        wake_control(&holder);
    }) {
        Ok(token) => {
            lane.parked = Some(token);
            true
        }
        Err(_) => false,
    }
}

/// Answers control connections until the socket has been quiet for
/// [`reactor::CONTROL_IDLE`], then parks it on the reactor.
///
/// It holds the holder only while answering, so a request thread waiting
/// out its idle period never keeps a finished session's PTY open.
fn serve_control(holder: &Weak<Shared>) {
    loop {
        let Some(shared) = holder.upgrade() else {
            return;
        };
        if shared.finished.load(Ordering::SeqCst) {
            return;
        }
        let Some(listener) = shared.listener.lock().expect("listener").clone() else {
            return;
        };
        // SAFETY: accept(2) on a listening fd kept open by `listener`; the
        // addr out-params are unused.
        let client = unsafe {
            libc::accept(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if client >= 0 {
            // SAFETY: a fresh fd accept just handed us; the stream owns it.
            let client = unsafe {
                use std::os::fd::FromRawFd;
                diri_platform::ipc::UnixStream::from_raw_fd(client)
            };
            // BSD hands the listener's O_NONBLOCK on to accepted sockets;
            // requests are served with blocking reads.
            let _ = client.set_nonblocking(false);
            serve_client(&shared, client);
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {}
            _ => {
                // Never give up on a live listener: the usual cause,
                // descriptor exhaustion, clears once something closes.
                let delay = crate::limits::accept_retry_delay(&error);
                eprintln!("diri-holder: accept: {error}; retrying in {delay:?}");
                drop(shared);
                std::thread::sleep(delay);
                continue;
            }
        }
        drop(shared);
        match reactor::wait_readable(listener.as_raw_fd(), None, Some(reactor::CONTROL_IDLE)) {
            Ok(reactor::Readiness::Idle) => {}
            _ => continue,
        }
        let Some(shared) = holder.upgrade() else {
            return;
        };
        let mut lane = shared.accept.lock().expect("accept");
        if lane.closed || park_control(&shared, &mut lane) {
            return;
        }
        // No reactor: keep the thread, looking again each idle period so a
        // withdrawn listener is still noticed.
    }
}

/// Answers one control connection.
fn serve_client(shared: &Arc<Shared>, mut client: diri_platform::ipc::UnixStream) {
    let response = match socket::read_json_line::<HolderRequest>(&mut client) {
        Ok(request) if request.op == HolderOperation::OutputStream => {
            if request.stream_version != Some(HOLDER_OUTPUT_STREAM_VERSION) {
                HolderResponse::failure("unsupported Holder output stream version")
            } else if shared.finished.load(Ordering::SeqCst) {
                // Nothing will ever be streamed again, and the log is
                // complete including the exit marker. Accepting here
                // would leave the subscriber waiting on a stream that
                // is over while the marker sat unread in the file.
                HolderResponse::failure("holder has finished")
            } else {
                subscribe_output(shared, client, request.resume_from);
                return;
            }
        }
        Ok(request) if request.op == HolderOperation::Stream => {
            let version = request.stream_version.filter(|version| {
                *version == HOLDER_STREAM_VERSION || *version == HOLDER_STREAM_VERSION_PIXELS
            });
            if let Some(version) = version {
                let response = HolderResponse::stream(version);
                if socket::write_json_line(&mut client, &response).is_ok() {
                    let client = Arc::new(client);
                    let mut streams = shared.input_streams.lock().expect("input streams");
                    streams.retain(|stream| stream.strong_count() > 0);
                    streams.push(Arc::downgrade(&client));
                    drop(streams);
                    let _ = spawn_input(shared, client);
                }
                return;
            }
            HolderResponse::failure("unsupported Holder input stream version")
        }
        Ok(request) => handle(shared, &request)
            .unwrap_or_else(|error| HolderResponse::failure(error.to_string())),
        Err(error) => HolderResponse::failure(error.to_string()),
    };
    let _ = socket::write_json_line(&mut client, &response);
}

/// Registers a daemon for live output, first replaying what it missed from
/// the log when it asks to resume from an earlier offset.
fn subscribe_output(
    shared: &Arc<Shared>,
    mut client: diri_platform::ipc::UnixStream,
    resume_from: Option<u64>,
) {
    // Registered under the same lock that hands out frames, so the offset
    // quoted here is exactly where this subscriber's first frame will begin.
    let mut fanout = shared.output.lock().expect("output");
    let live_offset = fanout.next_offset;
    // A resumed subscriber first gets what it missed, read from the log, then
    // the live frames. The pump waits for it the whole time it is reading, so
    // output can never run further ahead of it than the queue.
    let backlog_start = resume_from.map(|from| {
        let oldest = shared.log.lock().expect("log").oldest_readable_offset();
        from.max(oldest).min(live_offset)
    });
    let start_offset = backlog_start.unwrap_or(live_offset);
    let response = match backlog_start {
        Some(_) => {
            HolderResponse::resumed_output_stream(HOLDER_OUTPUT_STREAM_VERSION, start_offset)
        }
        None => HolderResponse::output_stream(HOLDER_OUTPUT_STREAM_VERSION, start_offset),
    };
    socket::set_buffer(&client, libc::SO_SNDBUF, socket::OUTPUT_SOCKET_BUFFER);
    if socket::write_json_line(&mut client, &response).is_err() {
        return;
    }
    let frames = super::fanout::FrameQueue::new(OUTPUT_QUEUE_BYTES);
    let link = Arc::new(SubscriberLink {
        stream: client,
        peer: Mutex::new(None),
    });
    let backlog = start_offset..live_offset;
    // A backlog has to be written now, not when the next frame arrives, so
    // its writer starts here. Claimed before the pump can see the queue, so
    // the pump never starts a second one.
    let replay = !backlog.is_empty();
    if replay {
        frames.claim_writer();
    }
    let subscriber = OutputSubscriber {
        frames: Arc::clone(&frames),
        link: Arc::clone(&link),
    };
    if replay && spawn_writer(shared, &subscriber, backlog).is_err() {
        // Nobody serves it, so the release is ours; the daemon falls back to
        // the log.
        frames.close();
        link.finish();
        return;
    }
    fanout.subscribers.push(subscriber);
    drop(fanout);
    // Without a backlog there is no writer yet: the first frame starts one.
    watch_peer(Arc::downgrade(shared), link, frames);
}

/// Closes a subscriber's queue when its daemon hangs up.
///
/// A subscriber sends nothing after its request, so its socket turns
/// readable only when the daemon goes. Through a silent session a dead
/// subscriber would otherwise keep its socket and buffer until the next
/// frame failed to write. Without a reactor nothing watches, and that next
/// failed write is what releases it.
fn watch_peer(
    holder: Weak<Shared>,
    link: Arc<SubscriberLink>,
    frames: Arc<super::fanout::FrameQueue>,
) {
    let Some(reactor) = reactor::shared() else {
        return;
    };
    let mut peer = link.peer.lock().expect("peer watch");
    let fd = link.stream.as_raw_fd();
    let watched = Arc::clone(&link);
    // The registration holds the link, so the socket stays open while it can
    // fire.
    if let Ok(token) = reactor.when_readable(fd, move || peer_readable(holder, watched, frames)) {
        *peer = Some(token);
    }
}

/// Runs on the reactor when a subscriber's socket turns readable.
fn peer_readable(
    holder: Weak<Shared>,
    link: Arc<SubscriberLink>,
    frames: Arc<super::fanout::FrameQueue>,
) {
    *link.peer.lock().expect("peer watch") = None;
    let mut scratch = [0u8; 64];
    loop {
        // SAFETY: a writable scratch buffer of the length passed, on a
        // socket the link keeps open. Never blocks.
        let count = unsafe {
            libc::recv(
                link.stream.as_raw_fd(),
                scratch.as_mut_ptr().cast(),
                scratch.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if count > 0 {
            continue; // not part of the protocol; discarded rather than trusted
        }
        if count < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                    // Still connected: watch again.
                    if !frames.is_closed() {
                        watch_peer(holder, link, frames);
                    }
                    return;
                }
                _ => {}
            }
        }
        break; // EOF or a broken socket: the daemon is gone
    }
    if frames.close() {
        // No writer is running to see the close; release it here. The pump
        // prunes the closed entry itself should it hold the fan-out now.
        link.finish();
        let Some(shared) = holder.upgrade() else {
            return;
        };
        let prune = move |fanout: &mut OutputFanout| {
            fanout
                .subscribers
                .retain(|subscriber| !Arc::ptr_eq(&subscriber.frames, &frames));
        };
        // The pump can hold the fan-out for a while offering a frame; the
        // reactor must not wait on it.
        if let Ok(mut fanout) = shared.output.try_lock() {
            prune(&mut fanout);
        } else {
            let holder = Arc::clone(&shared);
            let _ = spawn_lane("output", &shared.spec.session_id, move || {
                prune(&mut holder.output.lock().expect("output"));
            });
        }
    }
}

/// Starts a writer thread for a subscriber whose queue asked for one, which
/// first writes `backlog` from the log.
fn spawn_writer(
    shared: &Arc<Shared>,
    subscriber: &OutputSubscriber,
    backlog: std::ops::Range<u64>,
) -> std::io::Result<()> {
    let holder = Arc::downgrade(shared);
    let link = Arc::clone(&subscriber.link);
    let frames = Arc::clone(&subscriber.frames);
    spawn_lane("output", &shared.spec.session_id, move || {
        write_output(&holder, &link, &frames, backlog);
    })
    .map(drop)
}

fn open_log(spec: &HolderLaunchSpec) -> HolderResult<OutputLog> {
    let path = Path::new(&spec.log_file_path);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let session = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(&spec.session_id);
    let capacity = if spec.disk_capacity > 0 {
        spec.disk_capacity as usize
    } else {
        super::protocol::DEFAULT_DISK_CAPACITY as usize
    };
    // The Holder only appends and reports offsets. Its consumers either read
    // the durable file themselves or receive the separate bounded live queue;
    // retaining a second raw-output ring here has no reader.
    OutputLog::open(directory, session, 0, capacity, false)
        .map_err(|error| HolderError::io("open output log", error))
}

/// Drains PTY output into the log while it flows, then parks the PTY on the
/// reactor once it has been quiet for [`reactor::LANE_IDLE`].
///
/// The loop has to notice `finished` as well as output — after the exit marker
/// is written, nothing more may be appended, or straggling grandchild output
/// would land beyond the marker — so it waits on the PTY and the exit path's
/// wake pipe together.
fn pump_pty(shared: &Arc<Shared>) {
    let Some(mut reader) = shared.pump.lock().expect("pump").reader.take() else {
        return;
    };
    let idle = reactor::lane_idle();
    let mut log = LogLane::default();
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        let quiet = pump_until_quiet(shared, &mut reader, &mut log, &mut buffer, idle);
        // Every queued byte must reach the log before the pump lets go: the
        // exit path writes the marker straight after joining this thread,
        // and a byte still in flight would land after it. Parked, the writer
        // has nothing to do either.
        log.finish();
        if !quiet {
            return; // EOF or finished: the exit path takes it from here
        }
        let mut lane = shared.pump.lock().expect("pump");
        lane.reader = Some(reader);
        if lane.finished || park_pump(shared, &mut lane) {
            return;
        }
        // The reactor would not take it: keep pumping on this thread.
        reader = lane.reader.take().expect("reader just returned");
    }
}

/// Pumps until the PTY has been quiet for `idle` (returns true), or it hits
/// EOF or the holder finishes (returns false).
fn pump_until_quiet(
    shared: &Arc<Shared>,
    reader: &mut crate::pty::PtyStream,
    log: &mut LogLane,
    buffer: &mut [u8],
    idle: Option<Duration>,
) -> bool {
    use std::os::fd::AsRawFd as _;
    loop {
        if shared.finished.load(Ordering::SeqCst) {
            return false;
        }
        let readable = reactor::wait_readable(
            reader.as_raw_fd(),
            Some(shared.pump_wake.0.as_raw_fd()),
            idle,
        );
        #[cfg(test)]
        shared.pump_wakeups.fetch_add(1, Ordering::SeqCst);
        match readable {
            Ok(reactor::Readiness::Ready) => {}
            Ok(reactor::Readiness::Woken) => continue,
            Ok(reactor::Readiness::Idle) => return true,
            Err(_) => return false,
        }
        // A PTY hands over about a kilobyte per read, so appending per read
        // pays the log's per-chunk costs a thousand times for every megabyte.
        // Bytes already queued in the kernel are folded into one append
        // instead: the reads happen either way, and nothing waits on bytes
        // that have not arrived, so this batches without adding latency.
        let mut filled = 0;
        let mut eof = false;
        loop {
            match reader.read(&mut buffer[filled..]) {
                Ok(0) => {
                    eof = true; // every slave handle is gone
                    break;
                }
                Ok(count) => {
                    filled += count;
                    if filled == buffer.len() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    eof = true;
                    break;
                }
            }
        }
        if filled > 0 {
            // Subscribers first, then durability. A daemon rendering this
            // session should not wait for a disk write to see the bytes, and
            // this ordering is what decouples display latency from filesystem
            // throughput.
            let chunk = &buffer[..filled];
            broadcast_output(shared, chunk);
            // Bytes read are handed on even if `finished` was just set: the
            // exit path joins this thread before writing the marker, so
            // nothing can land beyond it — but a byte consumed from the kernel
            // and then dropped would be lost.
            log.append(shared, chunk);
        }
        if eof || shared.finished.load(Ordering::SeqCst) {
            return false;
        }
    }
}

/// The log writer of one pump turn.
///
/// The log is also the transport: the daemon reads this session's output by
/// tailing the spill file. Appending on the pump's thread therefore made
/// draining the PTY wait on the filesystem — under a burst of output the pump
/// sat in `write` for most of its wall time while the child blocked on a full
/// PTY buffer. Handing bytes to a writer decouples the two: the feed absorbs a
/// filesystem stall (a truncation rewrite, most of all) without ever stalling
/// the reader, and gathers kilobyte reads into large appends.
///
/// The feed is bounded, so a writer that genuinely cannot keep up applies
/// backpressure rather than growing without limit. Ordering is preserved
/// because exactly one thread writes. The writer starts with the turn's first
/// output, after that output has gone to subscribers, so starting it never
/// delays the screen.
#[derive(Default)]
struct LogLane {
    feed: Option<Arc<super::log_feed::LogFeed>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl LogLane {
    fn append(&mut self, shared: &Arc<Shared>, chunk: &[u8]) {
        if self.feed.is_none() {
            let feed = Arc::new(super::log_feed::LogFeed::new());
            let writer = {
                let shared = Arc::clone(shared);
                let feed = Arc::clone(&feed);
                let session = shared.spec.session_id.clone();
                spawn_lane("log", &session, move || {
                    let _exit = super::log_feed::WriterExit(&feed);
                    let mut batch: Vec<u8> = Vec::new();
                    while feed.take(&mut batch) {
                        // A failed disk write must not stop the session:
                        // the child is still running and its status still
                        // matters.
                        append_log(&shared, &batch);
                        batch.clear();
                        // A burst can leave a large buffer behind; an
                        // idle session should not keep it.
                        batch.shrink_to(super::log_feed::LOG_BATCH_BYTES);
                    }
                })
                .ok()
            };
            // Without a writer thread the append happens inline, which is
            // slower but always correct.
            if writer.is_some() {
                self.feed = Some(feed);
                self.writer = writer;
            }
        }
        if !self.feed.as_ref().is_some_and(|feed| feed.push(chunk)) {
            append_log(shared, chunk);
        }
    }

    /// Flushes everything queued to the log and ends the writer.
    fn finish(&mut self) {
        if let Some(feed) = self.feed.take() {
            feed.close();
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Appends to the log and publishes its new tail for [`current_stat`].
fn append_log(shared: &Shared, bytes: &[u8]) {
    let mut log = shared.log.lock().expect("log");
    let _ = log.append(bytes);
    shared.log_tail.store(log.tail_offset(), Ordering::Release);
    drop(log);
    // One batch per append, and nobody waits unless a resumed subscriber is
    // reading its backlog, so this costs nothing measurable.
    shared.log_grew.notify_all();
}

/// Hands one chunk of PTY output to every attached subscriber.
///
/// A subscriber that takes nothing for [`OUTPUT_SEND_PATIENCE`] is dropped.
/// That bound is the whole point: waiting a little applies the backpressure a
/// single-process terminal gets for free, so output cannot race arbitrarily far
/// ahead of the screen that renders it — and giving up keeps a wedged or dead
/// daemon from stalling the PTY. Anything a dropped subscriber misses is in the
/// log, which is where it resumes.
fn broadcast_output(shared: &Arc<Shared>, frame: &[u8]) {
    use super::fanout::Pushed;
    let mut fanout = shared.output.lock().expect("output");
    let offset = fanout.next_offset;
    // Advanced whether or not anyone is listening: a subscriber that arrives
    // later is told to start here, and must not be told an offset that skipped
    // bytes already handed out.
    fanout.next_offset += frame.len() as u64;
    if fanout.subscribers.is_empty() {
        return;
    }
    fanout.subscribers.retain(|subscriber| {
        let release = match subscriber.frames.offer(offset, frame, OUTPUT_SEND_PATIENCE) {
            Pushed::Queued => return true,
            // Its writer ended while the session was quiet; start the next.
            Pushed::WakeWriter => match spawn_writer(shared, subscriber, 0..0) {
                Ok(()) => return true,
                // Nobody serves it now, so the release is ours.
                Err(_) => {
                    subscriber.frames.close();
                    true
                }
            },
            Pushed::Refused => {
                if !subscriber.frames.is_closed() {
                    diri_telemetry::warn_event!(
                        "holder.subscriber_dropped",
                        session = diri_telemetry::id(&shared.spec.session_id),
                        offset = offset,
                    );
                }
                // Closing is what makes dropping visible. Letting the
                // subscriber go only releases this end of the queue; a writer
                // would keep the socket open, and the daemon would wait for
                // frames that are never coming instead of falling back to the
                // log. A running writer sees the close and releases it; with
                // none running, the release is ours.
                subscriber.frames.close()
            }
        };
        if release {
            subscriber.link.finish();
        }
        false
    });
}

/// Writes a resumed subscriber's backlog, `range`, from the log, as frames
/// like any other. Returns false when it cannot, and the subscriber is then
/// closed and falls back to the log itself.
///
/// The pump is waiting on this subscriber's full queue meanwhile; every frame
/// written here tells it the subscriber is still reading, so the PTY is held
/// at the subscriber's pace rather than lapping it.
fn write_log_backlog(
    stream: &mut impl Write,
    shared: &Weak<Shared>,
    frames: &super::fanout::FrameQueue,
    range: std::ops::Range<u64>,
) -> bool {
    let mut from = range.start;
    while from < range.end {
        // Upgraded per frame, so a writer stuck on a daemon that stopped
        // reading does not keep the Holder alive.
        let Some(holder) = shared.upgrade() else {
            return false;
        };
        let (start, bytes) = {
            let mut log = holder.log.lock().expect("log");
            // The log is written behind the pump, so the end of the backlog
            // can still be on its way there. The wait is bounded only so a
            // closed subscriber is noticed: the pump has already handed those
            // bytes to the log writer, which is what will wake it.
            while log.tail_offset() <= from {
                if frames.is_closed() || holder.finished.load(Ordering::SeqCst) {
                    return false;
                }
                log = holder
                    .log_grew
                    .wait_timeout(log, OUTPUT_SEND_PATIENCE)
                    .expect("log")
                    .0;
            }
            let wanted = usize::try_from(range.end - from)
                .unwrap_or(usize::MAX)
                .min(super::fanout::JOINED_FRAME_BYTES);
            log.read(from, wanted)
        };
        drop(holder);
        // Rotated past it, or a failed disk write left a hole: either way
        // these bytes are not here, and the subscriber's own log read is what
        // records the loss.
        if start != from || bytes.is_empty() {
            return false;
        }
        let length = bytes.len() as u32;
        if stream.write_all(&from.to_be_bytes()).is_err()
            || stream.write_all(&length.to_be_bytes()).is_err()
            || stream.write_all(&bytes).is_err()
            || stream.flush().is_err()
        {
            return false;
        }
        frames.note_progress();
        from += bytes.len() as u64;
    }
    true
}

/// Writes frames to one subscriber until it goes, the queue closes, or it has
/// had nothing to send for [`reactor::LANE_IDLE`] (the next frame then starts
/// a new writer). The buffer is always flushed before the writer waits, so
/// ending idle strands nothing.
fn write_output(
    holder: &Weak<Shared>,
    link: &Arc<SubscriberLink>,
    frames: &Arc<super::fanout::FrameQueue>,
    backlog: std::ops::Range<u64>,
) {
    use super::fanout::Popped;
    let idle = reactor::lane_idle();
    let mut stream = std::io::BufWriter::with_capacity(OUTPUT_WRITE_BUFFER, &link.stream);
    let mut queued: Option<super::fanout::Frame> = None;
    let replayed = backlog.is_empty() || write_log_backlog(&mut stream, holder, frames, backlog);
    if replayed {
        loop {
            let (offset, frame) = match queued.take() {
                Some(frame) => frame,
                None => match idle {
                    Some(idle) => match frames.pop_within(idle) {
                        Popped::Frame(frame) => frame,
                        Popped::Closed => break,
                        Popped::Idle => {
                            if frames.park_writer() {
                                return;
                            }
                            continue;
                        }
                    },
                    None => match frames.pop() {
                        Some(frame) => frame,
                        None => break,
                    },
                },
            };
            let length = frame.len() as u32;
            if stream.write_all(&offset.to_be_bytes()).is_err()
                || stream.write_all(&length.to_be_bytes()).is_err()
                || stream.write_all(&frame).is_err()
            {
                frames.close();
                break;
            }
            // Flushed only once nothing else is waiting, so a burst coalesces into
            // few writes while a lone chunk still leaves immediately. The frame
            // taken here is carried to the next turn, never dropped.
            match frames.try_pop() {
                Some(next) => queued = Some(next),
                None => {
                    if stream.flush().is_err() {
                        frames.close();
                        break;
                    }
                }
            }
        }
    }
    let _ = stream.flush();
    drop(stream);
    frames.close();
    // A running writer owns the release.
    link.finish();
    // The pump only prunes a closed subscriber when it next has output to
    // offer, which a silent session never does.
    if let Some(shared) = holder.upgrade() {
        let mut fanout = shared.output.lock().expect("output");
        fanout
            .subscribers
            .retain(|subscriber| !Arc::ptr_eq(&subscriber.frames, frames));
    }
}

/// Arranges for the child's exit to start the exit path: on the reactor when
/// the exit is pollable, else on a thread that waits for it.
fn watch_exit(shared: &Arc<Shared>) -> std::io::Result<()> {
    if let Some(reactor) = reactor::shared() {
        let descriptor = shared.exit_watch.lock().expect("exit watch").descriptor();
        if let Some(fd) = descriptor {
            // The registration holds the holder: while its child lives, this
            // is what keeps an idle session's state (and PTY) alive. The
            // watch it fires on lives in that state, so it stays open.
            let holder = Arc::clone(shared);
            if reactor.when_readable(fd, move || exited(holder)).is_ok() {
                return Ok(());
            }
        }
    }
    let holder = Arc::clone(shared);
    spawn_lane("exit", &shared.spec.session_id, move || finish(holder)).map(drop)
}

/// Runs on the reactor when the child's exit watch fires.
fn exited(shared: Arc<Shared>) {
    let holder = Arc::clone(&shared);
    if spawn_lane("exit", &shared.spec.session_id, move || finish(holder)).is_err() {
        // The watch is still readable, so it fires again at once.
        std::thread::sleep(Duration::from_millis(10));
        if watch_exit(&shared).is_err() {
            diri_telemetry::warn_event!(
                "holder.exit_unwatchable",
                session = diri_telemetry::id(&shared.spec.session_id),
            );
        }
    }
}

/// The exit path, on its own thread, then `on_finish` once nothing here holds
/// the holder any more.
fn finish(shared: Arc<Shared>) {
    let on_finish = finish_holder(&shared);
    drop(shared);
    if let Some(on_finish) = on_finish {
        on_finish();
    }
}

/// Waits for the child to exit, kills whatever of its tree outlived it, reaps
/// it, then finishes the holder: final drain, exit marker, control-file
/// cleanup, listener shutdown.
fn finish_holder(shared: &Shared) -> Option<Box<dyn FnOnce() + Send>> {
    let ExitWatch { watcher, detached } =
        std::mem::take(&mut *shared.exit_watch.lock().expect("exit watch"));
    // The leader stays an unreaped zombie until the sweep is done: that is
    // what keeps its pid, and so the group id the sweep and the guard both
    // name, from being handed to anyone else. A detached leader is launchd's
    // child and reaped by it, but a group id is never reissued while any
    // member is alive, and the sweep only signals live members.
    let detached_exit = match &detached {
        Some(leader) => Some(wait_for_detached_exit(leader, shared.child_pid)),
        None => {
            wait_for_exit_unreaped(shared.child_pid, watcher);
            None
        }
    };
    let mut frozen = shared.frozen.lock().expect("frozen");
    process_tree::kill_stragglers(shared.child_pid, &frozen);
    if let Some(guard) = &shared.guard {
        guard.thaw(&frozen);
        guard.release(shared.child_pid);
    }
    frozen.clear();
    drop(frozen);

    let mut status: libc::c_int = 0;
    match detached_exit {
        // Re-encoded so the one decode below feeds the marker either way.
        Some(diri_pty::Exit::Code(code)) => status = (code & 0xFF) << 8,
        Some(diri_pty::Exit::Signal(signal)) => status = signal & 0x7F,
        // SAFETY: waitpid on our own child; EINTR retried.
        None => {
            while unsafe { libc::waitpid(shared.child_pid, &mut status, 0) } < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
        }
    }
    // The same decode Swift used, bit for bit, because it feeds the marker.
    let exit = if status & 0x7F != 0 {
        HolderExitStatus {
            reason: HolderExitReason::Signaled,
            code: None,
            signal: Some(status & 0x7F),
        }
    } else {
        HolderExitStatus {
            reason: HolderExitReason::Exited,
            code: Some((status >> 8) & 0xFF),
            signal: None,
        }
    };

    if shared.finished.swap(true, Ordering::SeqCst) {
        return None;
    }
    diri_telemetry::event!(
        "holder.exit",
        session = diri_telemetry::id(&shared.spec.session_id),
        code = exit.code,
        signal = exit.signal,
        runtime_s = shared.spawned_at.elapsed().as_secs(),
    );
    // A running pump waits on the PTY, so it has to be told.
    let _ = (&shared.pump_wake.1).write_all(&[1]);
    for stream in shared
        .input_streams
        .lock()
        .expect("input streams")
        .drain(..)
        .filter_map(|stream| stream.upgrade())
    {
        let _ = stream.shutdown(Shutdown::Both);
    }
    // The pump must stop before the final drain, or a straggler byte could
    // land after the exit marker. A parked PTY is withdrawn from the reactor
    // under the same lock a wake would take, so no new pump can start.
    let pump = {
        let mut lane = shared.pump.lock().expect("pump");
        lane.finished = true;
        if let (Some(token), Some(reactor)) = (lane.parked.take(), reactor::shared()) {
            reactor.cancel(token);
        }
        lane.thread.take()
    };
    if let Some(pump) = pump {
        let _ = pump.join();
    }

    if let Ok(mut drain) = shared.pty.lock().expect("pty").reader() {
        let mut buffer = [0u8; 64 << 10];
        loop {
            match drain.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => append_log(shared, &buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break, // WouldBlock: nothing buffered
            }
        }
    }

    {
        let mut log = shared.log.lock().expect("log");
        let _ = log.append(&HolderExitMarker::encode(&exit));
        let _ = log.flush();
        shared.log_tail.store(log.tail_offset(), Ordering::Release);
    }

    // The marker goes to the log, not the stream — it is not PTY output — so a
    // subscriber would otherwise keep waiting for frames from a child that has
    // already gone, and learn of the exit only when this process finally
    // closed its socket. Ending the subscription sends it back to the log,
    // where the marker is already durable.
    for subscriber in shared.output.lock().expect("output").subscribers.drain(..) {
        if subscriber.frames.close() {
            subscriber.link.finish();
        }
    }

    let _ = std::fs::remove_file(&shared.spec.socket_path);
    let _ = std::fs::remove_file(&shared.spec.pid_file_path);

    {
        let mut lane = shared.accept.lock().expect("accept");
        lane.closed = true;
        if let (Some(token), Some(reactor)) = (lane.parked.take(), reactor::shared()) {
            reactor.cancel(token);
        }
    }
    if let Some(listener) = shared.listener.lock().expect("listener").take() {
        // A request thread still waiting holds its own handle; it sees
        // `finished` within its idle period and lets go.
        // SAFETY: shutdown on a descriptor `listener` keeps open.
        unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
    }
    shared.on_finish.lock().expect("on finish").take()
}

/// Starts a thread serving one input stream.
fn spawn_input(
    shared: &Arc<Shared>,
    stream: Arc<diri_platform::ipc::UnixStream>,
) -> std::io::Result<()> {
    let holder = Arc::clone(shared);
    spawn_lane("input", &shared.spec.session_id, move || {
        serve_input_stream(&holder, stream);
    })
    .map(drop)
}

/// Hands an idle input stream to the reactor. False if it would not take it.
///
/// The registration holds the stream, so it stays on the exit path's list
/// and is shut down with the others, which fires the wake and lets it go.
fn park_input(shared: &Arc<Shared>, stream: &Arc<diri_platform::ipc::UnixStream>) -> bool {
    let Some(reactor) = reactor::shared() else {
        return false;
    };
    let holder = Arc::downgrade(shared);
    let held = Arc::clone(stream);
    reactor
        .when_readable(stream.as_raw_fd(), move || wake_input(&holder, held))
        .is_ok()
}

/// Runs on the reactor when a parked input stream has a frame (or hung up).
fn wake_input(holder: &Weak<Shared>, stream: Arc<diri_platform::ipc::UnixStream>) {
    let Some(shared) = holder.upgrade() else {
        return;
    };
    if shared.finished.load(Ordering::SeqCst) {
        return; // shut down by the exit path
    }
    if spawn_input(&shared, Arc::clone(&stream)).is_err() {
        // Still readable, so a re-armed watch fires again at once.
        std::thread::sleep(Duration::from_millis(10));
        let _ = park_input(&shared, &stream);
    }
}

/// Serves the negotiated high-frequency input lane. Each operation is
/// acknowledged only after it has reached the PTY, preserving the delivery
/// guarantee of the legacy request/response protocol without reconnecting or
/// encoding base64 for every key.
///
/// The thread lives while keys arrive. After [`reactor::LANE_IDLE`] without a
/// frame the stream is handed to the reactor and the thread ends; the next
/// key starts a new one, at the same interactive priority.
fn serve_input_stream(shared: &Arc<Shared>, stream: Arc<diri_platform::ipc::UnixStream>) {
    prioritize_interactive_io();
    let idle = reactor::lane_idle();
    let mut payload = Vec::with_capacity(256);
    loop {
        if let Some(idle) = idle
            && matches!(
                reactor::wait_readable(stream.as_raw_fd(), None, Some(idle)),
                Ok(reactor::Readiness::Idle)
            )
            && park_input(shared, &stream)
        {
            return;
        }
        let mut reader = &*stream;
        let mut header = [0_u8; 5];
        if !read_stream_exact(shared, &mut reader, &mut header) {
            return;
        }
        let length = u32::from_be_bytes(header[1..].try_into().expect("four-byte length")) as usize;
        if length > HOLDER_STREAM_MAX_PAYLOAD {
            let _ = reader.write_all(&[1]);
            return;
        }
        payload.resize(length, 0);
        if !read_stream_exact(shared, &mut reader, &mut payload) {
            return;
        }
        let result = match header[0] {
            HOLDER_STREAM_INPUT => write_pty(shared, &payload),
            HOLDER_STREAM_RESIZE | HOLDER_STREAM_RESIZE_PIXELS
                if payload.len()
                    == if header[0] == HOLDER_STREAM_RESIZE {
                        4
                    } else {
                        8
                    } =>
            {
                let word = |at: usize| u16::from_be_bytes([payload[at], payload[at + 1]]);
                let (cols, rows) = (word(0), word(2));
                let pixels = if payload.len() == 8 {
                    (word(4), word(6))
                } else {
                    (0, 0)
                };
                if cols < 2 || rows < 2 {
                    Err(HolderError::InvalidRequest(
                        "resize requires cols/rows >= 2".into(),
                    ))
                } else {
                    let _ = shared
                        .pty
                        .lock()
                        .expect("pty")
                        .resize_with_pixels(cols, rows, pixels);
                    Ok(())
                }
            }
            _ => Err(HolderError::InvalidRequest(
                "unknown Holder input stream frame".into(),
            )),
        };
        if result.is_err() {
            let _ = reader.write_all(&[1]);
            return;
        }
        if reader.write_all(&[HOLDER_STREAM_ACK]).is_err() {
            return;
        }
    }
}

fn handle(shared: &Shared, request: &HolderRequest) -> HolderResult<HolderResponse> {
    match request.op {
        HolderOperation::Stream | HolderOperation::OutputStream => Err(
            HolderError::InvalidRequest("stream negotiation must be the first operation".into()),
        ),
        HolderOperation::Write => {
            let data = request
                .data
                .as_deref()
                .and_then(|encoded| {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()
                })
                .ok_or_else(|| HolderError::InvalidRequest("write requires base64 data".into()))?;
            write_pty(shared, &data)?;
            Ok(HolderResponse::success())
        }

        HolderOperation::Resize => {
            let (Some(cols), Some(rows)) = (request.cols, request.rows) else {
                return Err(HolderError::InvalidRequest(
                    "resize requires cols/rows >= 2".into(),
                ));
            };
            if cols < 2 || rows < 2 {
                return Err(HolderError::InvalidRequest(
                    "resize requires cols/rows >= 2".into(),
                ));
            }
            let _ = shared.pty.lock().expect("pty").resize(cols, rows);
            Ok(HolderResponse::success())
        }

        HolderOperation::Signal => {
            let signal = request
                .sig
                .ok_or_else(|| HolderError::InvalidRequest("signal requires a valid sig".into()))?;
            if signal <= 0 || signal >= MAX_SIGNAL {
                return Err(HolderError::InvalidRequest(
                    "signal requires a valid sig".into(),
                ));
            }
            let tree = if signal == libc::SIGCONT {
                let frozen = shared.frozen.lock().expect("frozen").clone();
                process_tree::resume(shared.child_pid, &frozen)
            } else {
                process_tree::signal(shared.child_pid, signal)
            };
            if matches!(signal, libc::SIGSTOP | libc::SIGCONT) {
                let mut frozen = shared.frozen.lock().expect("frozen");
                if let Some(guard) = &shared.guard {
                    guard.thaw(&frozen);
                }
                frozen.clear();
                if signal == libc::SIGSTOP {
                    frozen.clone_from(&tree);
                    if let Some(guard) = &shared.guard {
                        guard.freeze(&frozen);
                    }
                }
            }
            Ok(HolderResponse::with_tree(tree))
        }

        HolderOperation::KillTree => {
            process_tree::kill_tree(shared.child_pid);
            Ok(HolderResponse::success())
        }

        HolderOperation::Stat => {
            let mut stat = current_stat(shared);
            if request.line_probe == Some(true) {
                stat.awaiting_line = Some(shared.pty.lock().expect("pty").job_awaits_line());
            }
            Ok(HolderResponse::with_stat(stat))
        }
    }
}

/// A persistent socket moves keystrokes off the Holder's accept thread. Keep
/// that dedicated, mostly-sleeping lane at interactive QoS on Apple platforms
/// so waking it does not add latency ahead of the PTY and renderer pipeline.
#[cfg(target_vendor = "apple")]
pub(super) fn prioritize_interactive_io() {
    // SAFETY: this changes only the calling thread's QoS class. Priority zero
    // is the documented relative priority for the selected class.
    let _ = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
}

#[cfg(not(target_vendor = "apple"))]
pub(super) fn prioritize_interactive_io() {}

fn read_stream_exact(shared: &Shared, stream: &mut impl Read, mut bytes: &mut [u8]) -> bool {
    while !bytes.is_empty() && !shared.finished.load(Ordering::SeqCst) {
        match stream.read(bytes) {
            Ok(0) => return false,
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return false,
        }
    }
    bytes.is_empty()
}

/// Writes with the same bounded semantics the Swift holder used: retry
/// `EINTR`/`EAGAIN`, waiting for the child to drain, but give up if the PTY
/// stays unwritable for a full second.
fn write_pty(shared: &Shared, data: &[u8]) -> HolderResult<()> {
    let pty = shared.pty.lock().expect("pty");
    let writer = pty
        .writer()
        .map_err(|error| HolderError::io("PTY writer", error))?;
    let fd = writer.as_raw_fd();
    let mut written = 0;
    while written < data.len() {
        // SAFETY: plain write(2) on an owned fd with an in-bounds slice.
        let count =
            unsafe { libc::write(fd, data[written..].as_ptr().cast(), data.len() - written) };
        if count > 0 {
            written += count as usize;
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one initialized pollfd, millisecond timeout.
                let ready = unsafe { libc::poll(&mut poll_fd, 1, 1000) };
                if ready <= 0 || poll_fd.revents & libc::POLLOUT == 0 {
                    return Err(HolderError::Transport(
                        "PTY remained unwritable for 1 second".into(),
                    ));
                }
            }
            _ => return Err(HolderError::io("PTY write", error)),
        }
    }
    Ok(())
}

fn current_stat(shared: &Shared) -> HolderStat {
    let Some(expected) = shared.child_identity else {
        return current_stat_without_identity(shared);
    };
    if let Ok(mut stat) = diri_pty::process_identity::inspect_verified(&expected, || {
        Ok(current_stat_without_identity(shared))
    }) {
        stat.child_identity = Some(expected);
        return stat;
    }
    // The child can no longer be inspected: it exited, or a stranger now
    // occupies its PID. Neither changes which child this Holder spawned, and
    // the identity it recorded at birth is the Engine's only way to bind the
    // run's retained terminal. A child that exits within a millisecond, as
    // `sh -c 'exit 3'` does on Linux, is gone before the Engine's first stat.
    // Keep vouching for the birth identity while `alive` says the process
    // itself cannot be read; a stranger on the PID never reads as alive here
    // because `finished` is already set once the child was reaped.
    let mut stat = current_stat_without_identity(shared);
    if !stat.alive {
        stat.child_identity = Some(expected);
    }
    stat
}

fn current_stat_without_identity(shared: &Shared) -> HolderStat {
    let finished = shared.finished.load(Ordering::SeqCst);
    let log_offset = shared.log_tail.load(Ordering::Acquire);
    let pty = shared.pty.lock().expect("pty");
    // SAFETY: kill with signal 0 only checks existence.
    let child_alive = unsafe { libc::kill(shared.child_pid, 0) } == 0;
    let size = pty.size().ok();
    HolderStat {
        child_identity: None,
        child_pid: shared.child_pid,
        alive: !finished && child_alive,
        log_offset,
        // Sample the live owner. A cloned writer dropped before `tcgetpgrp`
        // leaves a closed fd, so every job looks like the idle shell.
        foreground_pid: pty.foreground_pgid(),
        cols: size.map(|(cols, _)| cols),
        rows: size.map(|(_, rows)| rows),
        epoch_offset: Some(shared.epoch_offset),
        // Sampled on request, from the owner: the holder itself never polls,
        // and an idle one still costs no wakeups.
        secret_input: Some(pty.secret_input()),
        awaiting_line: None,
    }
}

fn write_pid_file(path: &str) -> HolderResult<()> {
    let contents = format!("{}\n", std::process::id());
    diri_platform::private::write(Path::new(path), contents)
        .map_err(|error| HolderError::io("write pid file", error))
}

/// Blocks until the watched child has exited, leaving it unreaped.
///
/// A readiness fd rather than `waitid(WEXITED | WNOWAIT)`: macOS's `waitid`
/// also returns for a child that merely stopped, so a hibernation would read
/// as an exit. Returning here is what licenses the SIGKILL sweep of the
/// child's group, so only proof of exit may end the wait. A watcher that could
/// not be armed (a full descriptor table, a kernel without pidfd) is re-armed
/// instead of being read as "already exited".
fn wait_for_exit_unreaped(pid: i32, watcher: Option<diri_pty::ExitWatcher>) {
    let mut watcher = watcher;
    loop {
        if let Some(watcher) = &watcher {
            let mut descriptor = libc::pollfd {
                fd: watcher.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; no timeout.
            let ready = loop {
                let result = unsafe { libc::poll(&mut descriptor, 1, -1) };
                if result >= 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    break result;
                }
            };
            if ready > 0 || child_has_exited(pid) {
                return;
            }
        } else if child_has_exited(pid) {
            return;
        }
        match diri_pty::ExitWatcher::new(pid as u32) {
            Ok(armed) => watcher = Some(armed),
            // Registration fails with ESRCH only once the child has exited.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return,
            Err(_) => {
                watcher = None;
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// The exit of a launchd-started leader. Its watch was armed before it could
/// run, so it reports the real status; should the watch itself fail, the pid
/// is polled until it is gone and the status is unknown (a code of 255).
fn wait_for_detached_exit(leader: &DetachedLeader, pid: i32) -> diri_pty::Exit {
    if let Ok(exit) = leader.wait() {
        return exit;
    }
    // SAFETY: signal 0 only probes.
    while unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        std::thread::sleep(Duration::from_millis(250));
    }
    diri_pty::Exit::Code(255)
}

/// How long Agents start directly after a launchd handoff timed out, so a
/// launchd that stopped running jobs costs one tab the wait, not every tab.
#[cfg(target_os = "macos")]
const DETACHED_RETRY_AFTER: Duration = Duration::from_secs(600);

#[cfg(target_os = "macos")]
static DETACHED_TIMED_OUT_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn detached_allowed(timed_out_at: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    timed_out_at.is_none_or(|at| now.saturating_duration_since(at) >= DETACHED_RETRY_AFTER)
}

/// Starts the Agent through `launcher` when there is one, else (and whenever
/// the launcher provably started nothing) as this process's child.
fn spawn_agent(
    spec: &diri_pty::PtySpec,
    launcher: Option<&AgentLauncher>,
    session_id: &str,
) -> std::io::Result<Pty> {
    #[cfg(target_os = "macos")]
    if let Some(launcher) = launcher.filter(|launcher| {
        !super::launcher::launchd_unavailable(&launcher.rendezvous, &holder_executable())
    }) {
        let started = std::time::Instant::now();
        let timed_out_at = *DETACHED_TIMED_OUT_AT.lock().expect("detached timeout");
        if !detached_allowed(timed_out_at, started) {
            diri_telemetry::debug_event!(
                "holder.detached_skipped",
                session = diri_telemetry::id(session_id),
            );
            return Pty::spawn(spec);
        }
        match diri_pty::detached::spawn(spec, launcher) {
            Ok(pty) => {
                diri_telemetry::event!(
                    "holder.detached_spawn",
                    session = diri_telemetry::id(session_id),
                    ms = started.elapsed(),
                );
                return Ok(pty);
            }
            Err(diri_pty::detached::DetachedError::Spawn(error)) => return Err(error),
            Err(diri_pty::detached::DetachedError::Unavailable(error)) => {
                eprintln!("diri-holder: detached launch unavailable, spawning directly: {error}");
                // One job launchd never ran is enough: later sessions, in
                // this manager and the next, skip the wait.
                if error.kind() == std::io::ErrorKind::TimedOut {
                    *DETACHED_TIMED_OUT_AT.lock().expect("detached timeout") =
                        Some(std::time::Instant::now());
                    super::launcher::mark_launchd_unavailable(
                        &launcher.rendezvous,
                        &holder_executable(),
                    );
                }
                diri_telemetry::incident!(
                    "holder.detached_unavailable",
                    session = diri_telemetry::id(session_id),
                    io = diri_telemetry::io_error(&error),
                    ms = started.elapsed(),
                );
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (launcher, session_id);
    Pty::spawn(spec)
}

/// This Holder's own executable, the one the Engine launched managers from.
#[cfg(target_os = "macos")]
fn holder_executable() -> std::path::PathBuf {
    std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .unwrap_or_default()
}

/// Whether `pid`, our child, has exited (it stays unreaped). Stops and
/// continues are reported by macOS even without `WSTOPPED`, so the reason is
/// checked rather than trusted.
fn child_has_exited(pid: i32) -> bool {
    // SAFETY: zeroed siginfo is a valid out-param for waitid.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid on our own child with a valid out-param.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        // ECHILD: nothing left to wait for, which is also an exit.
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: waitid filled the SIGCHLD member.
    let reported = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let reported = info.si_pid;
    reported == pid
        && matches!(
            info.si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        )
}

fn set_nonblocking(fd: i32) {
    // SAFETY: fcntl on an owned fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn a_timed_out_launchd_handoff_starts_agents_directly_until_the_retry() {
        let at = std::time::Instant::now();
        assert!(detached_allowed(None, at));
        assert!(!detached_allowed(Some(at), at));
        assert!(!detached_allowed(Some(at), at + DETACHED_RETRY_AFTER / 2));
        assert!(detached_allowed(Some(at), at + DETACHED_RETRY_AFTER));
    }

    #[test]
    fn a_missing_exit_watcher_never_reads_a_live_agent_as_exited() {
        // #524 armed the sweep on `ExitWatcher::new(..).ok()`: any arming
        // failure (EMFILE, a kernel without pidfd) returned at once and the
        // straggler sweep SIGKILLed a freshly spawned, healthy agent.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        let (done, finished) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            wait_for_exit_unreaped(pid, None);
            let _ = done.send(());
        });
        assert!(
            finished.recv_timeout(Duration::from_millis(600)).is_err(),
            "a live child is not an exit"
        );
        // SAFETY: signalling our own child.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        assert!(
            finished.recv_timeout(Duration::from_millis(600)).is_err(),
            "a hibernated (stopped) child is not an exit"
        );
        // SAFETY: signalling our own child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("a real exit ends the wait");
        waiter.join().expect("waiter");
        assert!(!child.try_wait().expect("reap").unwrap().success());
    }

    fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn running(session_id: &str) -> Arc<Shared> {
        RUNNING
            .lock()
            .expect("running")
            .iter()
            .filter_map(Weak::upgrade)
            .find(|shared| shared.spec.session_id == session_id)
            .expect("holder is running")
    }

    #[test]
    fn a_resumed_subscriber_gets_its_backlog_and_paces_a_flood_past_the_log() {
        let root = tempfile::tempdir().unwrap();
        // A log of a quarter megabyte, which keeps 32 KiB when it rotates,
        // and a program that prints ten times that as fast as it can.
        let spec = HolderLaunchSpec {
            session_id: "s_resume".into(),
            socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .path()
                .join("s_resume.bin")
                .to_string_lossy()
                .into_owned(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "seq 1 5000; sleep 0.5; seq 5001 400000; exec sleep 30".into(),
            ],
            cwd: "/tmp".into(),
            environment: vec![("PATH".into(), "/usr/bin:/bin".into())]
                .into_iter()
                .collect(),
            cols: 80,
            rows: 24,
            disk_capacity: 256 << 10,
        };
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());
        let first_part = (1..=5000)
            .map(|n| format!("{n}\r\n").len() as u64)
            .sum::<u64>();
        wait_until("the first part in the log", || {
            client
                .stat()
                .is_ok_and(|stat| stat.log_offset >= first_part)
        });

        // Resumed from the very start: the first part comes from the log.
        let mut stream = client
            .open_output_stream_from(0)
            .expect("subscribe")
            .expect("output stream supported");
        assert!(stream.resumed());
        assert_eq!(stream.start_offset(), 0);

        let expected: Vec<u8> = (1..=400_000)
            .flat_map(|n| format!("{n}\r\n").into_bytes())
            .collect();
        let mut received = Vec::with_capacity(expected.len());
        let mut run = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while received.len() < expected.len() {
            assert!(std::time::Instant::now() < deadline, "stream stalled");
            // A reader far slower than `seq`, in small bites.
            match stream.next_run_into(Duration::from_millis(500), 16 << 10, &mut run) {
                Ok(Some(at)) => {
                    assert_eq!(at, received.len() as u64, "frames are contiguous");
                    received.extend_from_slice(&run);
                }
                Ok(None) => {}
                Err(error) => panic!(
                    "dropped after {} of {} bytes: {error}",
                    received.len(),
                    expected.len()
                ),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(received == expected, "every byte, in order");

        client.kill_tree().expect("kill-tree");
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
    }

    #[test]
    fn a_silent_session_wakes_nothing_and_still_releases_a_closed_subscriber() {
        let root = tempfile::tempdir().unwrap();
        let spec = HolderLaunchSpec {
            session_id: "s_idle".into(),
            socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .path()
                .join("s_idle.bin")
                .to_string_lossy()
                .into_owned(),
            // Echo is the only output cat ever produces, and nothing is typed.
            argv: vec!["/bin/cat".into()],
            cwd: "/tmp".into(),
            environment: Default::default(),
            cols: 80,
            rows: 24,
            disk_capacity: 4096,
        };
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());

        let stream = client
            .open_output_stream()
            .expect("subscribe")
            .expect("output stream supported");
        let shared = running("s_idle");
        let frames = {
            let fanout = shared.output.lock().expect("output");
            assert_eq!(fanout.subscribers.len(), 1);
            Arc::clone(&fanout.subscribers[0].frames)
        };

        // Several of the old 100 ms ticks, in both loops.
        let pump_before = shared.pump_wakeups.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(shared.pump_wakeups.load(Ordering::SeqCst), pump_before);
        assert_eq!(frames.wakeups.load(Ordering::SeqCst), 0);

        // The daemon goes away while the session stays silent. Only this
        // test's handle on the queue may remain: the fan-out entry, the writer
        // and the peer watcher must all have let go.
        drop(stream);
        wait_until("subscriber released", || {
            shared.output.lock().expect("output").subscribers.is_empty()
                && Arc::strong_count(&frames) == 1
        });
        assert_eq!(
            shared.pump_wakeups.load(Ordering::SeqCst),
            pump_before,
            "the release must not depend on PTY output"
        );

        // A pump parked without a deadline still has to see the child exit.
        drop(shared);
        client.kill_tree().expect("kill-tree");
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
    }

    /// Reads output until `text` has arrived, or panics after `within`.
    fn read_until(stream: &mut crate::holder::HolderOutputStream, text: &[u8], within: Duration) {
        let deadline = std::time::Instant::now() + within;
        let mut seen = Vec::new();
        let mut run = Vec::new();
        while !seen.windows(text.len()).any(|window| window == text) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !left.is_zero(),
                "no {:?} within {within:?}",
                String::from_utf8_lossy(text)
            );
            if let Ok(Some(_)) =
                stream.next_run_into(left.min(Duration::from_millis(100)), 1 << 16, &mut run)
            {
                seen.extend_from_slice(&run);
            }
        }
    }

    #[test]
    fn an_idle_session_holds_no_threads_and_a_keystroke_wakes_it() {
        let root = tempfile::tempdir().unwrap();
        let spec = HolderLaunchSpec {
            session_id: "s_lanes".into(),
            socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .path()
                .join("s_lanes.bin")
                .to_string_lossy()
                .into_owned(),
            argv: vec!["/bin/cat".into()],
            cwd: "/tmp".into(),
            environment: Default::default(),
            cols: 80,
            rows: 24,
            disk_capacity: 1 << 20,
        };
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());
        let mut stream = client
            .open_output_stream()
            .expect("subscribe")
            .expect("output stream supported");

        // Busy: the PTY, its log, the subscriber and the keystroke each have
        // a thread.
        client.write(b"one\n").expect("type");
        read_until(&mut stream, b"one", Duration::from_secs(5));
        assert!(live_lanes::count("s_lanes") > 0);

        // Idle: every lane parks on the reactor and its thread ends. The
        // threads used to stay for the session's whole life, seven of them.
        let deadline = std::time::Instant::now() + reactor::LANE_IDLE * 3;
        while live_lanes::count("s_lanes") > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "{} lane threads still running after {:?} idle",
                live_lanes::count("s_lanes"),
                reactor::LANE_IDLE * 3
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // A keystroke wakes the input lane, then the PTY and the subscriber,
        // and its echo arrives as it did before the session went idle.
        let typed = std::time::Instant::now();
        client.write(b"two\n").expect("type after idle");
        read_until(&mut stream, b"two", Duration::from_secs(2));
        assert!(typed.elapsed() < Duration::from_secs(1));

        // The parked control socket still answers.
        assert!(client.stat().is_ok());
        client.kill_tree().expect("kill-tree");
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
        wait_until("lanes gone", || live_lanes::count("s_lanes") == 0);
    }

    /// Closing a session must not wait out the SIGKILL grace just because
    /// its leader is an interactive shell: `$SHELL -l`, and the `-i -l -c`
    /// wrapper that returns an agent to a prompt. Interactive zsh and bash
    /// ignore SIGTERM, which made every close take the full half second
    /// under the Registry lock. A tree that ignores the polite signals still
    /// dies, just not early.
    #[test]
    fn kill_tree_does_not_wait_out_the_grace_for_an_interactive_shell() {
        let wrapper = |shell: &str| {
            vec![
                shell.to_string(),
                "-i".into(),
                "-l".into(),
                "-c".into(),
                format!("sleep 30; printf x; exec {shell} -i -l"),
            ]
        };
        let cases = [
            (
                "zsh",
                vec!["/bin/zsh".to_string(), "-f".into(), "-i".into()],
                true,
            ),
            ("zsh_agent", wrapper("/bin/zsh"), true),
            ("bash_agent", wrapper("/bin/bash"), true),
            (
                "deaf",
                vec![
                    "/bin/sh".to_string(),
                    "-c".into(),
                    "trap '' TERM HUP; sleep 30 & wait".into(),
                ],
                false,
            ),
        ];
        for (name, argv, prompt) in cases {
            if !Path::new(&argv[0]).exists() {
                eprintln!("{name}: {} is not installed here, skipped", argv[0]);
                continue;
            }
            let root = tempfile::tempdir().unwrap();
            let id = format!("s_kill_{name}");
            let spec = HolderLaunchSpec {
                session_id: id.clone(),
                socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
                pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
                log_file_path: root
                    .path()
                    .join(format!("{id}.bin"))
                    .to_string_lossy()
                    .into_owned(),
                argv,
                cwd: "/tmp".into(),
                environment: Default::default(),
                cols: 80,
                rows: 24,
                disk_capacity: 4096,
            };
            let client = HolderClient::new(&spec.socket_path);
            let server = std::thread::spawn(move || HolderServer::run(spec));
            wait_until("holder ready", || client.is_alive());
            // Let the shell finish starting up and install its dispositions.
            std::thread::sleep(Duration::from_millis(300));
            let child_pid = running(&id).child_pid;
            let tree = process_tree::enumerate(child_pid);
            assert!(!tree.is_empty(), "{name}: the tree is running");

            let started = std::time::Instant::now();
            client.kill_tree().expect("kill-tree");
            let took = started.elapsed();
            eprintln!("{name}: kill_tree took {took:?}");
            if prompt {
                assert!(
                    took < Duration::from_millis(250),
                    "{name}: kill_tree waited {took:?}, the SIGKILL grace"
                );
            }
            wait_until("holder finished", || server.is_finished());
            server.join().expect("join").expect("clean holder exit");
            for sample in &tree {
                assert!(
                    !process_tree::is_alive(sample),
                    "{name}: pid {} survived kill_tree",
                    sample.pid
                );
            }
        }
    }

    #[test]
    fn a_log_write_in_progress_holds_up_neither_stat_nor_input() {
        let root = tempfile::tempdir().unwrap();
        let spec = HolderLaunchSpec {
            session_id: "s_slow_disk".into(),
            socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .path()
                .join("s_slow_disk.bin")
                .to_string_lossy()
                .into_owned(),
            argv: vec!["/bin/cat".into()],
            cwd: "/tmp".into(),
            environment: Default::default(),
            cols: 80,
            rows: 24,
            disk_capacity: 4096,
        };
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());
        let shared = running("s_slow_disk");

        // The log writer holds this lock across its disk write. A stat used to
        // wait for it while holding the PTY lock, so a slow filesystem stalled
        // the Engine's fact sampling and every keystroke behind it.
        let log = shared.log.lock().expect("log");
        let (done, finished) = std::sync::mpsc::channel();
        {
            let client = client.clone();
            std::thread::spawn(move || {
                let stat = client.stat().map(|stat| stat.alive);
                let input = client.write(b"typed while the disk is busy\n");
                let _ = done.send((stat.ok(), input.is_ok()));
            });
        }
        let outcome = finished.recv_timeout(Duration::from_secs(3));
        drop(log);
        assert_eq!(
            outcome.expect("stat and input must not wait for the log"),
            (Some(true), true)
        );

        drop(shared);
        client.kill_tree().expect("kill-tree");
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
    }

    #[test]
    fn stat_reports_secret_input_only_for_a_silenced_line_prompt() {
        let root = tempfile::tempdir().unwrap();
        // Each marker follows its `stty`, so reading it from the log means
        // the mode it names is already in force.
        let script = "stty -echo; printf hidden; read secret; \
            stty echo; printf shown; read a; \
            stty raw -echo; printf rawmode; read b";
        let spec = HolderLaunchSpec {
            session_id: "s_secret".into(),
            socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .path()
                .join("s_secret.bin")
                .to_string_lossy()
                .into_owned(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: "/tmp".into(),
            environment: [("PATH".to_string(), "/usr/bin:/bin".to_string())].into(),
            cols: 80,
            rows: 24,
            disk_capacity: 4096,
        };
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());

        let logged = |marker: &[u8]| {
            OutputLog::reader(root.path(), "s_secret").is_ok_and(|mut log| {
                log.refresh_from_disk();
                let (_, bytes) = log.read(0, 4096);
                bytes.windows(marker.len()).any(|window| window == marker)
            })
        };
        let secret = || client.stat().expect("stat").secret_input;

        wait_until("password prompt", || logged(b"hidden"));
        assert_eq!(secret(), Some(true));
        let pump_before = running("s_secret").pump_wakeups.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            running("s_secret").pump_wakeups.load(Ordering::SeqCst),
            pump_before,
            "a holder parked at a password prompt must not poll its termios"
        );

        client.write(b"hunter2\n").expect("answer the prompt");
        wait_until("echo restored", || logged(b"shown"));
        assert_eq!(secret(), Some(false));
        assert!(!logged(b"hunter2"), "a silenced line never reaches the log");

        client.write(b"\n").expect("continue");
        wait_until("raw mode", || logged(b"rawmode"));
        assert_eq!(secret(), Some(false), "raw mode without echo is a TUI");

        client.kill_tree().expect("kill-tree");
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
    }

    /// The state letter `ps` reports, or `None` once the pid is gone.
    fn process_state(pid: i32) -> Option<String> {
        let output = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!state.is_empty()).then_some(state)
    }

    /// Gone, or a zombie waiting for init: either way nothing left running.
    fn dead(pid: i32) -> bool {
        process_state(pid).is_none_or(|state| state.starts_with('Z'))
    }

    fn read_pids(path: &Path) -> Option<Vec<i32>> {
        let text = std::fs::read_to_string(path).ok()?;
        let pids: Vec<i32> = text
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect();
        (text.ends_with('\n') && !pids.is_empty()).then_some(pids)
    }

    fn held(root: &Path, session_id: &str, script: &str) -> HolderLaunchSpec {
        HolderLaunchSpec {
            session_id: session_id.into(),
            socket_path: root.join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .join(format!("{session_id}.bin"))
                .to_string_lossy()
                .into_owned(),
            // Like `fish -c codex`: the leader forks the agent rather than
            // exec'ing it, so the agent is a separate member of its group.
            argv: vec!["/bin/sh".into(), "-c".into(), format!("{script}; true")],
            cwd: "/tmp".into(),
            environment: [(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )]
            .into(),
            cols: 80,
            rows: 24,
            disk_capacity: 4096,
        }
    }

    /// A terminal tab: interactive zsh running an agent as its foreground
    /// job, and the agent's own children (MCP servers, tool shells) started
    /// after it. Every wake must leave the agent in the foreground; a zsh
    /// that resumes first reaps it as suspended and takes the tab, and the
    /// agent's mouse tracking then types into the prompt. macOS-only because
    /// `/bin/zsh` is the default shell there.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_woken_job_control_shell_keeps_its_job_in_the_foreground() {
        let root = tempfile::tempdir().unwrap();
        let pids = root.path().join("pids");
        let mut spec = held(root.path(), "s_job", "");
        spec.argv = vec!["/bin/zsh".into(), "-f".into(), "-i".into()];
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        // A timed-out wait must not leave zsh and 150 stopped sleeps behind.
        let _cleanup = KillTreeOnDrop(&client);
        wait_until("holder ready", || client.is_alive());
        client
            .write(
                format!(
                    "/bin/sh -c 'i=0; while [ $i -lt 150 ]; do sleep 1000 & i=$((i+1)); done; echo $$ > {pids}.tmp && mv {pids}.tmp {pids} && wait'\n",
                    pids = pids.display()
                )
                .as_bytes(),
            )
            .expect("start the job");
        let mut job = 0;
        wait_until("job running", || {
            job = read_pids(&pids).map_or(0, |pids| pids[0]);
            job > 0 && process_state(job).is_some_and(|state| state.contains('+'))
        });

        let taken = (0..20).find_map(|round| {
            client.signal(libc::SIGSTOP).expect("hibernate");
            wait_until("job stopped", || {
                process_state(job).is_some_and(|state| state.starts_with('T'))
            });
            client.signal(libc::SIGCONT).expect("wake");
            // There is no event for "zsh left it alone", so watch long
            // enough for a loaded zsh to have reaped the job and taken the
            // terminal if it was going to.
            let deadline = std::time::Instant::now() + Duration::from_millis(250);
            loop {
                let state = process_state(job).unwrap_or_default();
                if !state.contains('+') || state.starts_with('T') {
                    break Some((round, state));
                }
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        client.kill_tree().expect("kill-tree");
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
        assert_eq!(taken, None, "the shell took the terminal from its job");
    }

    /// A hibernated agent's helper that left the group (an MCP watchdog in
    /// its own session) loses its parent while stopped, so the wake's walk
    /// from the leader no longer reaches it. The wake must still continue
    /// it: it was frozen, and after the wake nothing remembers it.
    #[test]
    fn a_wake_continues_a_frozen_process_the_tree_lost() {
        let root = tempfile::tempdir().unwrap();
        let pids = root.path().join("pids");
        let agent = root.path().join("agent.sh");
        std::fs::write(
            &agent,
            format!(
                "perl -e 'use POSIX; POSIX::setsid(); sleep 1000' &\n\
                 echo $$ $! > {pids}.tmp && mv {pids}.tmp {pids}\n\
                 wait\n",
                pids = pids.display()
            ),
        )
        .unwrap();
        let spec = held(
            root.path(),
            "s_lost",
            &format!("/bin/sh {}", agent.display()),
        );
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        let _cleanup = KillTreeOnDrop(&client);
        wait_until("holder ready", || client.is_alive());
        let mut tree = Vec::new();
        wait_until("agent tree", || {
            tree = read_pids(&pids).unwrap_or_default();
            tree.len() == 2
        });
        let (agent, helper) = (tree[0], tree[1]);
        let _helper = KillOnDrop(helper);
        wait_until("helper in its own session", || {
            // SAFETY: getpgid on a pid we started; read-only.
            unsafe { libc::getpgid(helper) == helper }
        });

        client.signal(libc::SIGSTOP).expect("hibernate");
        wait_until("helper stopped", || {
            process_state(helper).is_some_and(|state| state.starts_with('T'))
        });
        // SAFETY: plain kill(2) on this test's own process.
        unsafe { libc::kill(agent, libc::SIGKILL) };
        wait_until("helper orphaned", || {
            // SAFETY: plain kill(2) probe of this test's own process.
            dead(agent) && unsafe { libc::kill(helper, 0) } == 0
        });

        client.signal(libc::SIGCONT).expect("wake");
        wait_until("helper continued", || {
            process_state(helper).is_some_and(|state| !state.starts_with('T'))
        });

        // The leader's death may already have ended the session (Linux
        // reports it at once), taking the socket with it; then there is
        // nothing left to kill.
        if let Err(error) = client.kill_tree() {
            assert!(
                server.is_finished() || !client.is_alive(),
                "kill-tree on a live holder: {error:?}"
            );
        }
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
    }

    /// Kills a test's held tree if the test panics before its own cleanup.
    struct KillTreeOnDrop<'a>(&'a HolderClient);

    impl Drop for KillTreeOnDrop<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill_tree();
        }
    }

    /// Kills a process the held tree no longer reaches.
    struct KillOnDrop(i32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: cleanup of this test's own process.
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                libc::kill(self.0, libc::SIGCONT);
            }
        }
    }

    /// The Codex shape that leaked on a real machine: `fish -c codex` leads the
    /// session, codex's node wrapper forwards TERM/HUP to the native binary and
    /// waits for it, and the agent has a helper that left the group (Codex's
    /// code-mode host, Chrome DevTools MCP's watchdog). Hibernated, then the
    /// leader dies: on macOS the unhandled hangup kills the native child even
    /// though it is stopped, but the wrapper, which handles SIGHUP, stays
    /// stopped with it pending. Nothing ever continued it; it sat under
    /// launchd, holding a revoked terminal, for weeks.
    #[test]
    fn a_leader_dying_while_hibernated_takes_its_frozen_tree_with_it() {
        let root = tempfile::tempdir().unwrap();
        let pids = root.path().join("pids");
        let wrapper = root.path().join("codex.sh");
        std::fs::write(
            &wrapper,
            format!(
                "trap 'kill -TERM $child 2>/dev/null' TERM HUP INT\n\
                 perl -e 'use POSIX; POSIX::setsid(); sleep 1000' &\n\
                 helper=$!\n\
                 sleep 1000 & child=$!\n\
                 echo $$ $child $helper > {pids}.tmp && mv {pids}.tmp {pids}\n\
                 wait $child; wait $child\n",
                pids = pids.display()
            ),
        )
        .unwrap();
        let spec = held(
            root.path(),
            "s_frozen",
            &format!("/bin/sh {}", wrapper.display()),
        );
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());
        let mut agent = Vec::new();
        wait_until("agent tree", || {
            agent = read_pids(&pids).unwrap_or_default();
            agent.len() == 3
        });
        // The setsid'd helper must have left the group before the freeze.
        wait_until("helper in its own session", || {
            // SAFETY: getpgid on a pid we started; read-only.
            unsafe { libc::getpgid(agent[2]) == agent[2] }
        });

        let frozen = client.signal(libc::SIGSTOP).expect("hibernate");
        for pid in &agent {
            assert!(
                frozen.iter().any(|sample| sample.pid == *pid),
                "{pid} is part of the hibernated tree: {frozen:?}"
            );
        }
        wait_until("tree stopped", || {
            agent
                .iter()
                .all(|&pid| process_state(pid).is_some_and(|state| state.starts_with('T')))
        });

        // Whatever kills the leader — a hangup, memory pressure, the user —
        // it goes without the holder's say-so.
        let leader = running("s_frozen").child_pid;
        // SAFETY: plain kill(2) on the holder's own child.
        unsafe { libc::kill(leader, libc::SIGKILL) };
        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !agent.iter().all(|&pid| dead(pid)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivors: Vec<(i32, String)> = agent
            .iter()
            .filter_map(|&pid| process_state(pid).map(|state| (pid, state)))
            .filter(|(_, state)| !state.starts_with('Z'))
            .collect();
        for (pid, _) in &survivors {
            // SAFETY: cleanup of this test's own leaked processes.
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
                libc::kill(*pid, libc::SIGCONT);
            }
        }
        assert!(
            survivors.is_empty(),
            "the agent tree outlived its session: {survivors:?} of {agent:?}"
        );
    }

    /// A member of the session's own group that ignores the hangup outlives a
    /// leader that exits normally unless the holder sweeps the group, as the
    /// remote Helper's guard does.
    #[test]
    fn a_leader_exiting_normally_leaves_nothing_in_its_group() {
        let root = tempfile::tempdir().unwrap();
        let pids = root.path().join("pids");
        let script = format!(
            "(trap '' HUP TERM; exec sleep 1000) & echo $! > {pids}.tmp && mv {pids}.tmp {pids}; sleep 0.3",
            pids = pids.display()
        );
        let spec = held(root.path(), "s_group", &script);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        // No "holder ready" wait: the leader lives 0.3 s, so a loaded runner
        // can miss the whole window. The pid file proves the session ran.
        let mut straggler = Vec::new();
        wait_until("background job", || {
            straggler = read_pids(&pids).unwrap_or_default();
            !straggler.is_empty()
        });
        let straggler = straggler[0];

        wait_until("holder finished", || server.is_finished());
        server.join().expect("join").expect("clean holder exit");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !dead(straggler) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survived = !dead(straggler);
        // SAFETY: cleanup of this test's own leaked process.
        unsafe { libc::kill(straggler, libc::SIGKILL) };
        assert!(!survived, "a group member outlived the session leader");
    }

    #[test]
    fn holder_log_retains_no_duplicate_output_and_replays_from_disk() {
        let root = tempfile::tempdir().unwrap();
        let spec = HolderLaunchSpec {
            session_id: "s_log".into(),
            socket_path: String::new(),
            pid_file_path: String::new(),
            log_file_path: root.path().join("s_log.bin").to_string_lossy().into_owned(),
            argv: Vec::new(),
            cwd: String::new(),
            environment: Default::default(),
            cols: 80,
            rows: 24,
            disk_capacity: 4096,
        };
        let mut writer = open_log(&spec).unwrap();
        let bytes = b"\x1b[2Joutput retained in the durable log\r\n";
        for _ in 0..200 {
            writer.append(bytes).unwrap();
        }
        writer.flush().unwrap();
        let tail = writer.tail_offset();
        assert_eq!(
            writer.ring_start_offset(),
            tail,
            "the Holder never reads its ring"
        );
        let mut reader = OutputLog::reader(root.path(), "s_log").unwrap();
        let (start, replay) = reader.read(tail - bytes.len() as u64, bytes.len());
        assert_eq!(start, tail - bytes.len() as u64);
        assert_eq!(replay, bytes, "log rotation must preserve the final output");
    }
}
