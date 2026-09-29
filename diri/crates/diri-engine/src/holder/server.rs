//! The per-session holder: owns exactly one PTY and child tree.
//!
//! The holder has no daemon dependencies. Its entire control interface is one
//! request/response NDJSON line per unix-socket connection, and its durable
//! output interface is the [`OutputLog`] file. When the child exits the
//! holder appends an in-band [`HolderExitMarker`] to the log — so a daemon
//! that wasn't running at the time still learns how the child died — then
//! removes its control files and stops serving.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::log::OutputLog;
use crate::pty::{Pty, PtySpec};

use super::client::HolderClient;
use super::guard::GroupGuard;
use super::process_tree;
use super::protocol::{
    HOLDER_INPUT_QUEUE_CAPACITY, HOLDER_OUTPUT_STREAM_VERSION, HOLDER_QUEUED_STREAM_MAX_PAYLOAD,
    HOLDER_QUEUED_STREAM_VERSION, HOLDER_STREAM_ACK, HOLDER_STREAM_FULL, HOLDER_STREAM_INPUT,
    HOLDER_STREAM_MAX_PAYLOAD, HOLDER_STREAM_RESIZE, HOLDER_STREAM_VERSION, HolderExitMarker,
    HolderExitReason, HolderExitStatus, HolderLaunchSpec, HolderOperation, HolderRequest,
    HolderResponse, HolderStat,
};
use super::socket;
use super::{HolderError, HolderResult};

/// One past the highest acceptable signal number, as Swift validated with
/// `NSIG`.
#[cfg(target_os = "macos")]
const MAX_SIGNAL: i32 = 32;
#[cfg(not(target_os = "macos"))]
const MAX_SIGNAL: i32 = 65;

/// How many PTY chunks may be waiting to be written before the reader has to
/// wait for the writer. At 64 KiB a chunk this absorbs a multi-megabyte stall
/// without letting a wedged filesystem grow the queue without bound.
const WRITE_QUEUE_DEPTH: usize = 256;

/// How much queued output one disk write may carry. Larger writes cost the
/// filesystem far less per byte than many small ones.
const WRITE_BATCH_BYTES: usize = 1 << 20;

/// How many chunks may be waiting for one output subscriber. This is what
/// bounds how far output can run ahead of the screen rendering it, so it is
/// deliberately short: a megabyte of slack, not sixteen.
const OUTPUT_QUEUE_DEPTH: usize = 16;

/// How long the pump waits for a full subscriber queue before giving up on
/// that subscriber. Long enough to ride out a scheduling hiccup, short enough
/// that a hung daemon cannot hold the PTY.
const OUTPUT_SEND_PATIENCE: Duration = Duration::from_millis(50);

/// Buffer for one subscriber's socket writes.
const OUTPUT_WRITE_BUFFER: usize = 256 << 10;

/// How long a version 1 writer waits for the PTY to take any byte before it
/// reports failure, as the Swift holder did. Its bytes stay queued.
const LEGACY_WRITE_PATIENCE: Duration = Duration::from_secs(1);

/// A drained input queue keeps at most this much of its allocation, so one
/// large paste does not pin its size in the manager for the Holder's life.
const INPUT_QUEUE_RETAINED: usize = 64 << 10;

/// Live holders, so a test can inspect the state `run` builds for itself.
#[cfg(test)]
static RUNNING: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

pub struct HolderServer;

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
    /// the holder's whole life; closing happens when `run` returns.
    pty: Mutex<Pty>,
    log: Mutex<OutputLog>,
    /// Log tail at the moment this holder started: the boundary between prior
    /// incarnations' bytes and bytes attributable to THIS child.
    epoch_offset: u64,
    finished: AtomicBool,
    /// Self-pipe the exit path writes to once `finished` is set. It is what
    /// lets the pump wait on the PTY without a deadline: a silent session
    /// costs no wakeups, and the exit is still noticed at once. Both ends live
    /// here so the write can never meet a closed pipe.
    pump_wake: (std::io::PipeReader, std::io::PipeWriter),
    /// Every return from the pump's wait, so a test can prove a silent
    /// session leaves it parked.
    #[cfg(test)]
    pump_wakeups: std::sync::atomic::AtomicUsize,
    listen_fd: AtomicI32,
    /// When the child was spawned, for its recorded runtime.
    spawned_at: std::time::Instant,
    /// Weak handles let the exit path interrupt blocking input reads without
    /// making idle Holder streams wake on a timer.
    input_streams: Mutex<Vec<Weak<std::os::unix::net::UnixStream>>>,
    /// Input accepted for the program and not yet taken by the PTY.
    input: InputQueue,
    /// Daemons receiving output as it is read, rather than by tailing the log.
    /// The log is still written, and is still what a subscriber falls back to;
    /// this only removes the filesystem from the path a live screen waits on.
    output: Mutex<OutputFanout>,
}

/// Input on its way to the program, in order.
///
/// A program that is busy when a paste lands (an agent mid-turn, `sleep; cat`)
/// leaves the PTY's input buffer full for as long as it likes. The queue holds
/// the rest, bounded by [`HOLDER_INPUT_QUEUE_CAPACITY`], and a drainer thread
/// feeds it to the PTY as the program reads. Nothing here ever waits on the
/// PTY while holding the lock: writes are nonblocking, and the drainer waits
/// for room in `poll` with the lock released. The pump reads output on its own
/// thread throughout, so a program blocked writing output while its input is
/// pending is always drained.
struct InputQueue {
    state: Mutex<InputState>,
    /// Signalled whenever bytes reach the PTY or the queue fails.
    progress: Condvar,
}

struct InputState {
    pending: VecDeque<u8>,
    /// Bytes ever accepted, and ever taken by the PTY. A writer that needs
    /// its bytes delivered waits for `written` to pass its own end.
    accepted: u64,
    written: u64,
    /// Whether a drainer thread owns delivering `pending`.
    draining: bool,
    /// Why nothing more can be delivered: the program's terminal is gone.
    failed: Option<String>,
    /// A handle on the PTY master of its own, so writing never contends with
    /// resize or stat for the PTY lock. Nonblocking, like every handle.
    writer: crate::pty::PtyStream,
}

/// Why input was not accepted.
enum InputRefusal {
    /// There is no room for the whole write; none of it was queued.
    Full,
    /// The program can no longer receive input.
    Failed(String),
}

impl InputRefusal {
    fn into_error(self) -> HolderError {
        match self {
            Self::Full => HolderError::Transport(format!(
                "the Holder's input queue is full ({} MiB waiting for the program)",
                HOLDER_INPUT_QUEUE_CAPACITY >> 20
            )),
            Self::Failed(reason) => HolderError::Transport(reason),
        }
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
struct OutputSubscriber {
    frames: Arc<super::fanout::FrameQueue>,
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
            std::fs::create_dir_all(parent)
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
        let pty = Pty::spawn(&pty_spec).map_err(|error| {
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

        // Nonblocking master: the reader drains in bursts, and writes bound
        // their patience with poll rather than blocking the control loop.
        // O_NONBLOCK lives on the shared file description, so setting it on
        // this dup covers every handle.
        let reader = pty
            .reader()
            .map_err(|error| HolderError::io("PTY reader", error))?;
        set_nonblocking(reader.as_raw_fd());

        let listener = socket::listen(&socket_path)?;
        // The accept loop owns this fd from here; the exit watcher closes it
        // to end the loop (see `socket::accept_raw` for why close, not just
        // shutdown).
        let listen_fd = {
            use std::os::fd::IntoRawFd;
            listener.into_raw_fd()
        };

        let pump_wake =
            std::io::pipe().map_err(|error| HolderError::io("create pump wake pipe", error))?;
        let input_writer = pty
            .writer()
            .map_err(|error| HolderError::io("PTY writer", error))?;

        let shared = Arc::new(Shared {
            child_pid,
            child_identity: pty.child_identity(),
            frozen: Mutex::new(Vec::new()),
            guard: guard.clone(),
            pty: Mutex::new(pty),
            log: Mutex::new(log),
            epoch_offset,
            finished: AtomicBool::new(false),
            pump_wake,
            #[cfg(test)]
            pump_wakeups: std::sync::atomic::AtomicUsize::new(0),
            listen_fd: AtomicI32::new(listen_fd),
            spawned_at,
            input_streams: Mutex::new(Vec::new()),
            input: InputQueue {
                state: Mutex::new(InputState {
                    pending: VecDeque::new(),
                    accepted: 0,
                    written: 0,
                    draining: false,
                    failed: None,
                    writer: input_writer,
                }),
                progress: Condvar::new(),
            },
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

        let pump = {
            let shared = Arc::clone(&shared);
            let mut reader = reader;
            std::thread::Builder::new()
                .name(format!("holder-pty-{}", shared.spec.session_id))
                .spawn(move || pump_pty(&shared, &mut reader))
                .map_err(|error| HolderError::io("spawn pump", error))?
        };

        // Registered only once the exit watcher, which alone releases it, is
        // certain to run.
        if let Some(guard) = &guard {
            guard.register(child_pid);
        }
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("holder-exit-{}", shared.spec.session_id))
                .spawn(move || watch_exit(&shared, pump, exit_watcher))
                .map_err(|error| {
                    if let Some(guard) = &guard {
                        guard.release(child_pid);
                    }
                    HolderError::io("spawn exit watcher", error)
                })?;
        }

        while let Some(mut client) =
            socket::accept_raw(listen_fd, || shared.finished.load(Ordering::SeqCst))?
        {
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
                        // Registered under the same lock that hands out frames,
                        // so the offset quoted here is exactly where this
                        // subscriber's first frame will begin.
                        let mut fanout = shared.output.lock().expect("output");
                        let start_offset = fanout.next_offset;
                        let response = HolderResponse::output_stream(
                            HOLDER_OUTPUT_STREAM_VERSION,
                            start_offset,
                        );
                        if socket::write_json_line(&mut client, &response).is_ok() {
                            let frames = super::fanout::FrameQueue::new(OUTPUT_QUEUE_DEPTH);
                            fanout.subscribers.push(OutputSubscriber {
                                frames: Arc::clone(&frames),
                            });
                            drop(fanout);
                            // Weak: a writer wedged on a daemon that stopped
                            // reading must not keep the PTY master open after
                            // the holder has finished.
                            let holder = Arc::downgrade(&shared);
                            let _ = std::thread::Builder::new()
                                .name(format!("holder-output-{}", shared.spec.session_id))
                                .spawn(move || serve_output_stream(&holder, client, &frames));
                        }
                        continue;
                    }
                }
                Ok(request) if request.op == HolderOperation::Stream => {
                    let version = request.stream_version.filter(|version| {
                        [HOLDER_STREAM_VERSION, HOLDER_QUEUED_STREAM_VERSION].contains(version)
                    });
                    if let Some(version) = version {
                        let response = HolderResponse::stream(version);
                        if socket::write_json_line(&mut client, &response).is_ok() {
                            let client = Arc::new(client);
                            let mut streams = shared.input_streams.lock().expect("input streams");
                            streams.retain(|stream| stream.strong_count() > 0);
                            streams.push(Arc::downgrade(&client));
                            drop(streams);
                            let shared = Arc::clone(&shared);
                            let _ = std::thread::Builder::new()
                                .name(format!("holder-input-{}", shared.spec.session_id))
                                .spawn(move || serve_input_stream(&shared, client, version));
                        }
                        continue;
                    } else {
                        HolderResponse::failure("unsupported Holder input stream version")
                    }
                }
                Ok(request) => handle(&shared, &request)
                    .unwrap_or_else(|error| HolderResponse::failure(error.to_string())),
                Err(error) => HolderResponse::failure(error.to_string()),
            };
            let _ = socket::write_json_line(&mut client, &response);
        }
        Ok(())
        // `shared` unwinds here: the PTY master closes, EOFing any straggler
        // that still holds the slave.
    }
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

/// Drains PTY output into the log until the child is gone.
///
/// The loop has to notice `finished` as well as output — after the exit marker
/// is written, nothing more may be appended, or straggling grandchild output
/// would land beyond the marker — so it waits on the PTY and the exit path's
/// wake pipe together, with no deadline.
fn pump_pty(shared: &Arc<Shared>, reader: &mut crate::pty::PtyStream) {
    // The log is also the transport: the daemon reads this session's output by
    // tailing the spill file. Appending on this thread therefore made draining
    // the PTY wait on the filesystem — under a burst of output the pump sat in
    // `write` for most of its wall time while the child blocked on a full PTY
    // buffer. Handing chunks to a writer decouples the two: the queue absorbs a
    // filesystem stall (a truncation rewrite, most of all) without ever
    // stalling the reader.
    //
    // The channel is bounded, so a writer that genuinely cannot keep up applies
    // backpressure rather than growing without limit. Ordering is preserved
    // because exactly one thread writes.
    let (send, receive) = std::sync::mpsc::sync_channel::<Arc<[u8]>>(WRITE_QUEUE_DEPTH);
    let writer = {
        let shared = Arc::clone(shared);
        std::thread::Builder::new()
            .name(format!("holder-log-{}", shared.spec.session_id))
            .spawn(move || {
                let mut batch: Vec<u8> = Vec::with_capacity(WRITE_BATCH_BYTES);
                while let Ok(chunk) = receive.recv() {
                    batch.clear();
                    batch.extend_from_slice(&chunk);
                    // Whatever else is already queued joins this write. One
                    // large append costs far less than many small ones — a
                    // syscall and a filesystem extent per chunk otherwise —
                    // and nothing waits, so this adds no latency of its own.
                    while batch.len() < WRITE_BATCH_BYTES {
                        match receive.try_recv() {
                            Ok(next) => batch.extend_from_slice(&next),
                            Err(_) => break,
                        }
                    }
                    // A failed disk write must not stop the session: the child
                    // is still running and its status still matters.
                    let _ = shared.log.lock().expect("log").append(&batch);
                }
            })
            .ok()
    };
    // Without a writer thread the append happens inline, which is slower but
    // always correct.
    let mut queue = writer.is_some().then_some(send);

    let mut buffer = [0u8; 64 << 10];
    loop {
        if shared.finished.load(Ordering::SeqCst) {
            break;
        }
        let readable = wait_for_output(reader.as_raw_fd(), &shared.pump_wake.0);
        #[cfg(test)]
        shared.pump_wakeups.fetch_add(1, Ordering::SeqCst);
        match readable {
            Ok(false) => continue,
            Ok(true) => {}
            Err(_) => break,
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
            let chunk: Arc<[u8]> = Arc::from(&buffer[..filled]);
            broadcast_output(shared, &chunk);
            // Bytes read are handed on even if `finished` was just set: the
            // exit watcher joins this thread before writing the marker, so
            // nothing can land beyond it — but a byte consumed from the kernel
            // and then dropped would be lost.
            match queue.as_ref() {
                Some(queue) if queue.send(Arc::clone(&chunk)).is_ok() => {}
                _ => {
                    let _ = shared.log.lock().expect("log").append(&chunk);
                }
            }
        }
        if eof || shared.finished.load(Ordering::SeqCst) {
            break;
        }
    }

    // Every queued byte must reach the log before this thread is joined: the
    // exit watcher writes the exit marker straight after the join, and a byte
    // still in flight would land after it.
    drop(queue.take());
    if let Some(writer) = writer {
        let _ = writer.join();
    }
}

/// Parks the pump until the PTY has something for it, or the exit path says
/// the child is gone. Returns whether the PTY is what woke it.
///
/// The wake pipe is never drained: it is written once, when `finished` is
/// set, and the pump stops on seeing that.
fn wait_for_output(pty_fd: i32, wake: &std::io::PipeReader) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut descriptors = [
        libc::pollfd {
            fd: pty_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: two initialized poll descriptors stay writable throughout
        // the call; both fds outlive it. A negative timeout waits forever.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if ready >= 0 {
            // Any event at all, errors included: the read that follows turns
            // a broken PTY into EOF, where ignoring it would spin here.
            return Ok(descriptors[0].revents != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

/// Hands one chunk of PTY output to every attached subscriber.
///
/// A subscriber that cannot accept within [`OUTPUT_SEND_PATIENCE`] is dropped.
/// That bound is the whole point: waiting a little applies the backpressure a
/// single-process terminal gets for free, so output cannot race arbitrarily far
/// ahead of the screen that renders it — and giving up keeps a wedged or dead
/// daemon from stalling the PTY. Anything a dropped subscriber misses is in the
/// log, which is where it resumes.
fn broadcast_output(shared: &Shared, frame: &Arc<[u8]>) {
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
        if offer_frame(subscriber, offset, frame) {
            return true;
        }
        diri_telemetry::warn_event!(
            "holder.subscriber_dropped",
            session = diri_telemetry::id(&shared.spec.session_id),
            offset = offset,
        );
        // Closing is what makes dropping visible. Letting the subscriber go
        // only releases this end of the queue; its writer would keep waiting
        // on the other, holding the socket open, and the daemon would wait for
        // frames that are never coming instead of falling back to the log.
        subscriber.frames.close();
        false
    });
}

/// Offers one frame to a subscriber, waiting only as long as
/// [`OUTPUT_SEND_PATIENCE`] for room.
///
/// Returns false when the subscriber should be dropped: either it is gone, or
/// it is far enough behind that continuing to wait would stall the PTY.
fn offer_frame(subscriber: &OutputSubscriber, offset: u64, frame: &Arc<[u8]>) -> bool {
    subscriber
        .frames
        .push((offset, Arc::clone(frame)), OUTPUT_SEND_PATIENCE)
}

/// Serves one subscriber until it disappears or the channel closes, then
/// releases everything held for it.
fn serve_output_stream(
    shared: &Weak<Shared>,
    stream: std::os::unix::net::UnixStream,
    frames: &Arc<super::fanout::FrameQueue>,
) {
    // The writer parks on the queue, and a write is the only way it would
    // learn the daemon has gone — so through a silent session a dead
    // subscriber would keep its thread, socket and buffer. Watching the
    // socket for the hangup closes the queue instead. If the watcher cannot
    // start, the subscriber is released by its next failed write, as before.
    let _watcher = stream.try_clone().ok().and_then(|peer| {
        let frames = Arc::clone(frames);
        let name = std::thread::current().name().map_or_else(
            || "holder-output-peer".to_string(),
            |name| format!("{name}-peer"),
        );
        std::thread::Builder::new()
            .name(name)
            .spawn(move || watch_output_peer(peer, &frames))
            .ok()
    });

    let mut stream = std::io::BufWriter::with_capacity(OUTPUT_WRITE_BUFFER, stream);
    write_output_frames(&mut stream, frames);
    frames.close();

    // Shutdown is what interrupts the watcher's blocking read; dropping this
    // handle alone would not, because the watcher holds a clone of it.
    let _ = stream.get_ref().shutdown(Shutdown::Both);
    // The pump only prunes a closed subscriber when it next has output to
    // offer, which a silent session never does.
    if let Some(shared) = shared.upgrade() {
        let mut fanout = shared.output.lock().expect("output");
        fanout
            .subscribers
            .retain(|subscriber| !Arc::ptr_eq(&subscriber.frames, frames));
    }
}

/// Closes a subscriber's queue when its daemon hangs up.
///
/// A subscriber sends nothing after its request, so this read blocks for the
/// subscription's whole life and costs no wakeups. It returns when the peer
/// closes or the writer shuts the socket down on its way out.
fn watch_output_peer(mut peer: std::os::unix::net::UnixStream, frames: &super::fanout::FrameQueue) {
    let mut scratch = [0u8; 64];
    loop {
        match peer.read(&mut scratch) {
            Ok(0) => break,
            // Not part of the protocol; discarded rather than trusted.
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    frames.close();
}

/// Writes frames to one subscriber until a write fails or the queue closes.
fn write_output_frames(
    stream: &mut std::io::BufWriter<std::os::unix::net::UnixStream>,
    frames: &super::fanout::FrameQueue,
) {
    let mut queued: Option<super::fanout::Frame> = None;
    loop {
        let (offset, frame) = match queued.take() {
            Some(frame) => frame,
            // Everything written so far was flushed before parking here, so
            // waiting without a deadline strands nothing in the buffer.
            None => match frames.pop() {
                Some(frame) => frame,
                None => break,
            },
        };
        let length = frame.len() as u32;
        if stream.write_all(&offset.to_be_bytes()).is_err()
            || stream.write_all(&length.to_be_bytes()).is_err()
            || stream.write_all(&frame).is_err()
        {
            frames.close();
            return;
        }
        // Flushed only once nothing else is waiting, so a burst coalesces into
        // few writes while a lone chunk still leaves immediately. The frame
        // taken here is carried to the next turn, never dropped.
        match frames.try_pop() {
            Some(next) => queued = Some(next),
            None => {
                if stream.flush().is_err() {
                    frames.close();
                    return;
                }
            }
        }
    }
    let _ = stream.flush();
}

/// Waits for the child to exit, kills whatever of its tree outlived it, reaps
/// it, then finishes the holder: final drain, exit marker, control-file
/// cleanup, listener shutdown.
fn watch_exit(
    shared: &Shared,
    pump: std::thread::JoinHandle<()>,
    exit_watcher: Option<diri_pty::ExitWatcher>,
) {
    // The leader stays an unreaped zombie until the sweep is done: that is
    // what keeps its pid, and so the group id the sweep and the guard both
    // name, from being handed to anyone else.
    wait_for_exit_unreaped(shared.child_pid, exit_watcher);
    let mut frozen = shared.frozen.lock().expect("frozen");
    process_tree::kill_stragglers(shared.child_pid, &frozen);
    if let Some(guard) = &shared.guard {
        guard.thaw(&frozen);
        guard.release(shared.child_pid);
    }
    frozen.clear();
    drop(frozen);

    let mut status: libc::c_int = 0;
    // SAFETY: waitpid on our own child; EINTR retried.
    while unsafe { libc::waitpid(shared.child_pid, &mut status, 0) } < 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
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
        return;
    }
    diri_telemetry::event!(
        "holder.exit",
        session = diri_telemetry::id(&shared.spec.session_id),
        code = exit.code,
        signal = exit.signal,
        runtime_s = shared.spawned_at.elapsed().as_secs(),
    );
    // The pump waits with no deadline, so it has to be told. So does an
    // input drainer waiting for room, which polls the same pipe.
    let _ = (&shared.pump_wake.1).write_all(&[1]);
    discard_input(shared, "the program has exited");
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
    // land after the exit marker.
    let _ = pump.join();

    if let Ok(mut drain) = shared.pty.lock().expect("pty").reader() {
        let mut buffer = [0u8; 64 << 10];
        loop {
            match drain.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let _ = shared.log.lock().expect("log").append(&buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break, // WouldBlock: nothing buffered
            }
        }
    }

    {
        let mut log = shared.log.lock().expect("log");
        let _ = log.append(&HolderExitMarker::encode(&exit));
        let _ = log.flush();
    }

    // The marker goes to the log, not the stream — it is not PTY output — so a
    // subscriber would otherwise keep waiting for frames from a child that has
    // already gone, and learn of the exit only when this process finally
    // closed its socket. Ending the subscription sends it back to the log,
    // where the marker is already durable.
    for subscriber in shared.output.lock().expect("output").subscribers.drain(..) {
        subscriber.frames.close();
    }

    let _ = std::fs::remove_file(&shared.spec.socket_path);
    let _ = std::fs::remove_file(&shared.spec.pid_file_path);

    let listen_fd = shared.listen_fd.swap(-1, Ordering::SeqCst);
    if listen_fd >= 0 {
        // Shutdown then close: on macOS only the close wakes a blocked
        // accept(2) on an AF_UNIX listener. `finished` is already set, so the
        // woken loop exits rather than reporting an error.
        // SAFETY: this fd was surrendered to raw ownership in `run`; nothing
        // else closes it.
        unsafe {
            libc::shutdown(listen_fd, libc::SHUT_RDWR);
            libc::close(listen_fd);
        }
    }
}

fn handle(shared: &Arc<Shared>, request: &HolderRequest) -> HolderResult<HolderResponse> {
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
            write_and_wait(shared, &data)?;
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
            let tree = process_tree::signal(shared.child_pid, signal);
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

        HolderOperation::Stat => Ok(HolderResponse::with_stat(current_stat(shared))),
    }
}

/// Serves the negotiated high-frequency input lane.
///
/// Version 1 acknowledges an input frame after it has reached the PTY, and
/// fails if the PTY takes nothing for a second. Version 2 acknowledges it once
/// queued, which the Holder then delivers for as long as the program lives,
/// and answers [`HOLDER_STREAM_FULL`] without closing when there is no room.
fn serve_input_stream(
    shared: &Arc<Shared>,
    stream: Arc<std::os::unix::net::UnixStream>,
    version: u16,
) {
    prioritize_interactive_io();
    let queued = version == HOLDER_QUEUED_STREAM_VERSION;
    let max_payload = if queued {
        HOLDER_QUEUED_STREAM_MAX_PAYLOAD
    } else {
        HOLDER_STREAM_MAX_PAYLOAD
    };
    let mut stream = &*stream;
    let mut payload = Vec::with_capacity(256);
    loop {
        let mut header = [0_u8; 5];
        if !read_stream_exact(shared, &mut stream, &mut header) {
            return;
        }
        let length = u32::from_be_bytes(header[1..].try_into().expect("four-byte length")) as usize;
        if length > max_payload {
            let _ = stream.write_all(&[1]);
            return;
        }
        payload.resize(length, 0);
        if !read_stream_exact(shared, &mut stream, &mut payload) {
            return;
        }
        let result = match header[0] {
            HOLDER_STREAM_INPUT if queued => match queue_input(shared, &payload) {
                Ok(_) => Ok(HOLDER_STREAM_ACK),
                Err(InputRefusal::Full) => Ok(HOLDER_STREAM_FULL),
                Err(refusal) => Err(refusal.into_error()),
            },
            HOLDER_STREAM_INPUT => write_and_wait(shared, &payload).map(|()| HOLDER_STREAM_ACK),
            HOLDER_STREAM_RESIZE if payload.len() == 4 => {
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                if cols < 2 || rows < 2 {
                    Err(HolderError::InvalidRequest(
                        "resize requires cols/rows >= 2".into(),
                    ))
                } else {
                    let _ = shared.pty.lock().expect("pty").resize(cols, rows);
                    Ok(HOLDER_STREAM_ACK)
                }
            }
            _ => Err(HolderError::InvalidRequest(
                "unknown Holder input stream frame".into(),
            )),
        };
        // A paste-sized buffer is not kept for a stream that goes back to
        // single keys.
        if payload.capacity() > INPUT_QUEUE_RETAINED {
            payload = Vec::with_capacity(256);
        }
        let Ok(answer) = result else {
            let _ = stream.write_all(&[1]);
            return;
        };
        if stream.write_all(&[answer]).is_err() {
            return;
        }
    }
}

/// A persistent socket moves keystrokes off the Holder's accept thread. Keep
/// that dedicated, mostly-sleeping lane at interactive QoS on Apple platforms
/// so waking it does not add latency ahead of the PTY and renderer pipeline.
#[cfg(target_vendor = "apple")]
fn prioritize_interactive_io() {
    // SAFETY: this changes only the calling thread's QoS class. Priority zero
    // is the documented relative priority for the selected class.
    let _ = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
}

#[cfg(not(target_vendor = "apple"))]
fn prioritize_interactive_io() {}

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

/// Accepts `data` for the program, whole or not at all, and returns the
/// stream position its last byte will occupy.
///
/// With nothing already waiting, as many bytes as the PTY takes right now are
/// written here, so a keystroke reaches the program without a thread handoff.
/// Whatever the PTY has no room for is queued behind a drainer.
fn queue_input(shared: &Arc<Shared>, data: &[u8]) -> Result<u64, InputRefusal> {
    let mut state = shared.input.state.lock().expect("input");
    if let Some(reason) = &state.failed {
        return Err(InputRefusal::Failed(reason.clone()));
    }
    if shared.finished.load(Ordering::SeqCst) {
        return Err(InputRefusal::Failed("the program has exited".into()));
    }
    if state.pending.len().saturating_add(data.len()) > HOLDER_INPUT_QUEUE_CAPACITY {
        return Err(InputRefusal::Full);
    }
    let mut rest = data;
    if state.pending.is_empty() {
        match write_available(state.writer.as_raw_fd(), rest) {
            Ok(count) => {
                state.written += count as u64;
                rest = &rest[count..];
            }
            Err(error) => {
                let reason = format!("PTY write failed: {error}");
                state.failed = Some(reason.clone());
                return Err(InputRefusal::Failed(reason));
            }
        }
    }
    state.accepted += data.len() as u64;
    let end = state.accepted;
    if rest.is_empty() {
        shared.input.progress.notify_all();
        return Ok(end);
    }
    state.pending.extend(rest);
    if !state.draining {
        state.draining = true;
        let drainer = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name(format!("holder-input-drain-{}", shared.spec.session_id))
            .spawn(move || drain_input(&drainer));
        if spawned.is_err() {
            // Delivered on this thread instead: slower for this stream, but
            // never lost.
            drop(state);
            drain_input(shared);
        }
    }
    Ok(end)
}

/// Delivers the queue to the PTY, waiting for room with the lock released,
/// until it is empty or the program is gone.
fn drain_input(shared: &Shared) {
    loop {
        let mut state = shared.input.state.lock().expect("input");
        let fd = state.writer.as_raw_fd();
        loop {
            if shared.finished.load(Ordering::SeqCst) || state.failed.is_some() {
                state.draining = false;
                return;
            }
            if state.pending.is_empty() {
                state.draining = false;
                state.pending.shrink_to(INPUT_QUEUE_RETAINED);
                return;
            }
            let (front, _) = state.pending.as_slices();
            match write_available(fd, front) {
                Ok(0) => break,
                Ok(count) => {
                    state.pending.drain(..count);
                    state.written += count as u64;
                    shared.input.progress.notify_all();
                }
                Err(error) => {
                    fail_input(shared, &mut state, format!("PTY write failed: {error}"));
                    state.draining = false;
                    return;
                }
            }
        }
        drop(state);
        match wait_for_input_room(fd, &shared.pump_wake.0) {
            Ok(true) => {}
            // The wake pipe: the program has exited. The next turn discards.
            Ok(false) => {}
            Err(reason) => {
                let mut state = shared.input.state.lock().expect("input");
                fail_input(shared, &mut state, reason);
                state.draining = false;
                return;
            }
        }
    }
}

/// Parks until the PTY has room for input, or the exit path says the program
/// is gone. Returns whether the PTY is what woke it.
fn wait_for_input_room(pty_fd: i32, wake: &std::io::PipeReader) -> Result<bool, String> {
    use std::os::fd::AsRawFd;
    let mut descriptors = [
        libc::pollfd {
            fd: pty_fd,
            events: libc::POLLOUT,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: two initialized poll descriptors stay writable throughout
        // the call; both fds outlive it. A negative timeout waits forever.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if ready >= 0 {
            let pty = descriptors[0].revents;
            if pty & libc::POLLOUT != 0 {
                return Ok(true);
            }
            if pty & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err("the program's terminal has closed".into());
            }
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("waiting for the PTY failed: {error}"));
        }
    }
}

/// Writes as much of `data` as the PTY takes without waiting.
fn write_available(fd: i32, data: &[u8]) -> std::io::Result<usize> {
    let mut written = 0;
    while written < data.len() {
        // SAFETY: plain write(2) on an owned nonblocking fd with an
        // in-bounds slice.
        let count =
            unsafe { libc::write(fd, data[written..].as_ptr().cast(), data.len() - written) };
        if count > 0 {
            written += count as usize;
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => break,
            _ if count == 0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "PTY write returned zero",
                ));
            }
            _ => return Err(error),
        }
    }
    Ok(written)
}

fn fail_input(shared: &Shared, state: &mut InputState, reason: String) {
    if !state.pending.is_empty() {
        diri_telemetry::warn_event!(
            "holder.input_undeliverable",
            session = diri_telemetry::id(&shared.spec.session_id),
            bytes = state.pending.len(),
        );
    }
    state.pending.clear();
    state.pending.shrink_to(INPUT_QUEUE_RETAINED);
    state.failed = Some(reason);
    shared.input.progress.notify_all();
}

/// The program is gone: input still waiting has nowhere to go.
fn discard_input(shared: &Shared, reason: &str) {
    let mut state = shared.input.state.lock().expect("input");
    fail_input(shared, &mut state, reason.to_owned());
}

/// Version 1 and legacy request semantics: accept `data`, then wait until the
/// PTY has taken it, reporting failure once the PTY has taken nothing for
/// [`LEGACY_WRITE_PATIENCE`]. Unlike the Swift holder, a write that gives up
/// leaves its bytes queued, so the program still receives them in order.
fn write_and_wait(shared: &Arc<Shared>, data: &[u8]) -> HolderResult<()> {
    let end = queue_input(shared, data).map_err(InputRefusal::into_error)?;
    let mut state = shared.input.state.lock().expect("input");
    let mut progress = state.written;
    let mut since = Instant::now();
    while state.written < end {
        if let Some(reason) = &state.failed {
            return Err(HolderError::Transport(reason.clone()));
        }
        if state.written > progress {
            progress = state.written;
            since = Instant::now();
        }
        let Some(patience) = LEGACY_WRITE_PATIENCE.checked_sub(since.elapsed()) else {
            return Err(HolderError::Transport(
                "PTY remained unwritable for 1 second".into(),
            ));
        };
        state = shared
            .input
            .progress
            .wait_timeout(state, patience)
            .expect("input")
            .0;
    }
    Ok(())
}

fn current_stat(shared: &Shared) -> HolderStat {
    if let Some(expected) = shared.child_identity
        && let Ok(mut stat) = diri_pty::process_identity::inspect_verified(&expected, || {
            Ok(current_stat_without_identity(shared))
        })
    {
        stat.child_identity = Some(expected);
        return stat;
    }
    current_stat_without_identity(shared)
}

fn current_stat_without_identity(shared: &Shared) -> HolderStat {
    let finished = shared.finished.load(Ordering::SeqCst);
    let pty = shared.pty.lock().expect("pty");
    // SAFETY: kill with signal 0 only checks existence.
    let child_alive = unsafe { libc::kill(shared.child_pid, 0) } == 0;
    let size = pty.size().ok();
    HolderStat {
        child_identity: None,
        child_pid: shared.child_pid,
        alive: !finished && child_alive,
        log_offset: shared.log.lock().expect("log").tail_offset(),
        // Sample the live owner. A cloned writer dropped before `tcgetpgrp`
        // leaves a closed fd, so every job looks like the idle shell.
        foreground_pid: pty.foreground_pgid(),
        cols: size.map(|(cols, _)| cols),
        rows: size.map(|(_, rows)| rows),
        epoch_offset: Some(shared.epoch_offset),
        // Sampled on request, from the owner: the holder itself never polls,
        // and an idle one still costs no wakeups.
        secret_input: Some(pty.secret_input()),
    }
}

fn write_pid_file(path: &str) -> HolderResult<()> {
    let contents = format!("{}\n", std::process::id());
    std::fs::write(path, contents).map_err(|error| HolderError::io("write pid file", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
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
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        wait_until("holder ready", || client.is_alive());
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
