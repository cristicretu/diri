//! The per-session holder: owns exactly one PTY and child tree.
//!
//! The holder has no daemon dependencies. Its entire control interface is one
//! request/response NDJSON line per unix-socket connection, and its durable
//! output interface is the [`OutputLog`] file. When the child exits the
//! holder appends an in-band [`HolderExitMarker`] to the log — so a daemon
//! that wasn't running at the time still learns how the child died — then
//! removes its control files and stops serving.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine as _;

use crate::holder::HolderPaths;
use crate::log::OutputLog;
use crate::pty::{Pty, PtySpec};

use super::client::HolderClient;
use super::process_tree;
use super::protocol::{
    HOLDER_OUTPUT_STREAM_VERSION, HOLDER_STREAM_ACK, HOLDER_STREAM_INPUT,
    HOLDER_STREAM_MAX_PAYLOAD, HOLDER_STREAM_RESIZE, HOLDER_STREAM_VERSION, HolderChildRecord,
    HolderExitMarker, HolderExitReason, HolderExitStatus, HolderLaunchSpec, HolderOperation,
    HolderRequest, HolderResponse, HolderStat,
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

/// Live holders, so a test can inspect the state `run` builds for itself.
#[cfg(test)]
static RUNNING: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

pub struct HolderServer;

struct Shared {
    spec: HolderLaunchSpec,
    child_pid: i32,
    child_identity: Option<diri_proto::process::ProcessIdentity>,
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
    /// Weak handles let the exit path interrupt blocking input reads without
    /// making idle Holder streams wake on a timer.
    input_streams: Mutex<Vec<Weak<std::os::unix::net::UnixStream>>>,
    /// Daemons receiving output as it is read, rather than by tailing the log.
    /// The log is still written, and is still what a subscriber falls back to;
    /// this only removes the filesystem from the path a live screen waits on.
    output: Mutex<OutputFanout>,
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
        let pty = Pty::spawn(&pty_spec).map_err(|error| HolderError::io("PTY spawn", error))?;
        let child_pid = pty.pid() as i32;
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

        let shared = Arc::new(Shared {
            child_pid,
            child_identity: pty.child_identity(),
            pty: Mutex::new(pty),
            log: Mutex::new(log),
            epoch_offset,
            finished: AtomicBool::new(false),
            pump_wake,
            #[cfg(test)]
            pump_wakeups: std::sync::atomic::AtomicUsize::new(0),
            listen_fd: AtomicI32::new(listen_fd),
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

        let pump = {
            let shared = Arc::clone(&shared);
            let mut reader = reader;
            std::thread::Builder::new()
                .name(format!("holder-pty-{}", shared.spec.session_id))
                .spawn(move || pump_pty(&shared, &mut reader))
                .map_err(|error| HolderError::io("spawn pump", error))?
        };

        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("holder-exit-{}", shared.spec.session_id))
                .spawn(move || watch_exit(&shared, pump))
                .map_err(|error| HolderError::io("spawn exit watcher", error))?;
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
                    if request.stream_version != Some(HOLDER_STREAM_VERSION) {
                        HolderResponse::failure("unsupported Holder input stream version")
                    } else {
                        let response = HolderResponse::stream(HOLDER_STREAM_VERSION);
                        if socket::write_json_line(&mut client, &response).is_ok() {
                            let client = Arc::new(client);
                            let mut streams = shared.input_streams.lock().expect("input streams");
                            streams.retain(|stream| stream.strong_count() > 0);
                            streams.push(Arc::downgrade(&client));
                            drop(streams);
                            let shared = Arc::clone(&shared);
                            let _ = std::thread::Builder::new()
                                .name(format!("holder-input-{}", shared.spec.session_id))
                                .spawn(move || serve_input_stream(&shared, client));
                        }
                        continue;
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

/// Reaps the child, then finishes the holder: final drain, exit marker,
/// control-file cleanup, listener shutdown.
fn watch_exit(shared: &Shared, pump: std::thread::JoinHandle<()>) {
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
    // The pump waits with no deadline, so it has to be told.
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
            Ok(HolderResponse::with_tree(process_tree::signal(
                shared.child_pid,
                signal,
            )))
        }

        HolderOperation::KillTree => {
            process_tree::kill_tree(shared.child_pid);
            Ok(HolderResponse::success())
        }

        HolderOperation::Stat => Ok(HolderResponse::with_stat(current_stat(shared))),
    }
}

/// Serves the negotiated high-frequency input lane. Each operation is
/// acknowledged only after it has reached the PTY, preserving the delivery
/// guarantee of the legacy request/response protocol without reconnecting or
/// encoding base64 for every key.
fn serve_input_stream(shared: &Shared, stream: Arc<std::os::unix::net::UnixStream>) {
    prioritize_interactive_io();
    let mut stream = &*stream;
    let mut payload = Vec::with_capacity(256);
    loop {
        let mut header = [0_u8; 5];
        if !read_stream_exact(shared, &mut stream, &mut header) {
            return;
        }
        let length = u32::from_be_bytes(header[1..].try_into().expect("four-byte length")) as usize;
        if length > HOLDER_STREAM_MAX_PAYLOAD {
            let _ = stream.write_all(&[1]);
            return;
        }
        payload.resize(length, 0);
        if !read_stream_exact(shared, &mut stream, &mut payload) {
            return;
        }
        let result = match header[0] {
            HOLDER_STREAM_INPUT => write_pty(shared, &payload),
            HOLDER_STREAM_RESIZE if payload.len() == 4 => {
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                if cols < 2 || rows < 2 {
                    Err(HolderError::InvalidRequest(
                        "resize requires cols/rows >= 2".into(),
                    ))
                } else {
                    let _ = shared.pty.lock().expect("pty").resize(cols, rows);
                    Ok(())
                }
            }
            _ => Err(HolderError::InvalidRequest(
                "unknown Holder input stream frame".into(),
            )),
        };
        if result.is_err() {
            let _ = stream.write_all(&[1]);
            return;
        }
        if stream.write_all(&[HOLDER_STREAM_ACK]).is_err() {
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
