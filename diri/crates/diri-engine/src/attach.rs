//! The per-session binary data channel: the app's terminal rendering path.
//!
//! A client connects to the daemon socket and sends one JSON
//! [`AttachRequest`] line instead of a control handshake; from then on the
//! connection carries binary [`Frame`]s both ways. The server side owns the
//! authoritative emulator: it seeds a fresh sink with a full grid snapshot
//! plus current modes (no byte replay, no reattach-mangle — the mosh model),
//! then streams paced grid diffs while output flows. The client sends input,
//! resize, scroll, and ping frames back on the same socket.
//!
//! One pump thread per session broadcasts to every sink attached to it, so
//! the grid walk and diff are done once regardless of sink count — the same
//! shape as the Swift daemon's coalesced `flushGrid`.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_proto::frames::{AttachRejection, Frame, FrameCodec, FrameType};

use crate::registry::Registry;
use crate::session::{AttachmentSeed, GridSignature, GridWake};

/// Background-output ceiling for grid emission. The first frame after quiet
/// and the bounded response frames after interactive input go immediately;
/// a continuous producer is capped at the display cadence of a 120 Hz panel.
/// This transport budget never delays the interactive leading edge.
const GRID_FLUSH_INTERVAL: Duration = Duration::from_millis(8);

/// One attached client's write half.
struct Sink {
    id: u64,
    preview: bool,
    output: PublicationOutput,
}

#[derive(Clone)]
enum PublicationOutput {
    Socket(Arc<Mutex<SinkOutput>>),
    Mux(crate::preview_mux::MuxSink),
}
impl From<Arc<Mutex<SinkOutput>>> for PublicationOutput {
    fn from(output: Arc<Mutex<SinkOutput>>) -> Self {
        Self::Socket(output)
    }
}
impl PublicationOutput {
    fn close(&self) {
        match self {
            Self::Socket(output) => {
                if let Ok(mut output) = output.lock() {
                    output.close();
                }
            }
            Self::Mux(output) => output
                .unavailable(diri_proto::preview_set::PreviewUnavailable::PublisherUnavailable),
        }
    }
}

// Queues own only Arc references: a publication is encoded once for all sinks.
// One oversized-but-valid seed is allowed; ordinary backlog remains <= 1 MiB.
const SINK_BACKLOG_BYTES: usize = 1024 * 1024;
const SINK_BACKLOG_FRAMES: usize = 64;
const WRITE_BUDGET_BYTES: usize = 256 * 1024;
const WRITE_RETRY: Duration = Duration::from_millis(1);
/// A safety ceiling for an idle pump, not its wake mechanism; see `pump`.
const IDLE_PUMP_CEILING: Duration = Duration::from_secs(30);
/// How often a pump looks for its Session while a restart has removed it.
const ABSENT_SESSION_RETRY: Duration = Duration::from_secs(1);
const STALLED_SINK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a reseedable sink may stay behind before it is dropped after all.
/// A throttled client (App Nap, memory pressure) catches up well inside it;
/// past it the peer is wedged and only holds a thread and a descriptor.
const LAGGING_SINK_LIMIT: Duration = Duration::from_secs(30);
/// How often a pump looks at a lagging sink's socket for room to reseed it.
/// Bounds the pump's wakeups while a client is descheduled, and the delay
/// between the client reading again and its fresh screen.
const LAGGING_RECHECK: Duration = Duration::from_millis(100);

struct SinkOutput {
    enhanced_keyboard: bool,
    preview: bool,
    stream: UnixStream,
    frames: VecDeque<Arc<[u8]>>,
    offset: usize,
    // Retained allocation bytes, including already-written prefixes until the
    // complete frame is released. Counting only unsent bytes could hide memory.
    queued_bytes: usize,
    last_progress: Instant,
    closed: bool,
    /// Why this sink was closed from the publishing side, for telemetry.
    close_reason: Option<&'static str>,
    /// A pump-registered sink that falls behind discards its stale diffs and
    /// is reseeded with a Full Snapshot instead of being closed. Off for
    /// connections no pump serves (a completed session's read-only seed).
    reseedable: bool,
    /// Since when, and why, this sink has been behind; cleared by the reseed.
    lagging: Option<(Instant, &'static str)>,
}

impl SinkOutput {
    fn new(stream: UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            enhanced_keyboard: false,
            preview: false,
            stream,
            frames: VecDeque::new(),
            offset: 0,
            queued_bytes: 0,
            last_progress: Instant::now(),
            closed: false,
            close_reason: None,
            reseedable: false,
            lagging: None,
        })
    }

    fn enqueue(&mut self, bytes: Arc<[u8]>) -> bool {
        // A full valid frame may exceed the ordinary backlog cap. Permit that
        // frame and small mode/control frames, but never a second large frame.
        let limit = self
            .frames
            .front()
            .map_or(SINK_BACKLOG_BYTES.max(bytes.len() + 64), |front| {
                SINK_BACKLOG_BYTES.max(front.len() + 64)
            });
        if self.lagging.is_some() {
            // Stale: the reseed will carry this frame's effect.
            return !self.closed;
        }
        if self.closed
            || self.frames.len() >= SINK_BACKLOG_FRAMES
            || self.queued_bytes.saturating_add(bytes.len()) > limit
        {
            if !self.closed && self.reseedable {
                self.fall_behind("backlog");
                return true;
            }
            if !self.closed {
                self.close_reason = Some("backlog");
            }
            self.close();
            return false;
        }
        if self.frames.is_empty() {
            self.last_progress = Instant::now();
        }
        self.queued_bytes += bytes.len();
        self.frames.push_back(bytes);
        true
    }

    fn flush(&mut self) -> bool {
        let start = Instant::now();
        let mut budget = WRITE_BUDGET_BYTES;
        while let Some(bytes) = self.frames.front() {
            if budget == 0 || start.elapsed() >= WRITE_RETRY {
                break;
            }
            let remaining = &bytes[self.offset..];
            match self.stream.write(&remaining[..remaining.len().min(budget)]) {
                Ok(0) => {
                    self.close();
                    return false;
                }
                Ok(count) => {
                    self.offset += count;
                    budget -= count;
                    self.last_progress = Instant::now();
                    if self.offset == bytes.len() {
                        self.queued_bytes -= bytes.len();
                        self.frames.pop_front();
                        self.offset = 0;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.close();
                    return false;
                }
            }
        }
        if !self.frames.is_empty() && self.last_progress.elapsed() >= STALLED_SINK_TIMEOUT {
            if self.reseedable {
                self.fall_behind("stalled");
            } else {
                self.close_reason.get_or_insert("stalled");
                self.close();
            }
        }
        if let Some((since, reason)) = self.lagging
            && since.elapsed() >= LAGGING_SINK_LIMIT
        {
            self.close_reason.get_or_insert(reason);
            self.close();
        }
        !self.closed
    }

    /// The client stopped keeping up (it is descheduled or throttled, not
    /// gone): drop every queued diff and wait for room to send one Full
    /// Snapshot. A partially written frame stays, because the peer's codec is
    /// mid-frame and anything spliced in would corrupt the stream.
    fn fall_behind(&mut self, reason: &'static str) {
        let partial = (self.offset > 0).then(|| self.frames.pop_front()).flatten();
        self.frames.clear();
        self.queued_bytes = partial.as_ref().map_or(0, |frame| frame.len());
        self.frames.extend(partial);
        self.lagging.get_or_insert((Instant::now(), reason));
    }

    /// Whether this sink is behind with nothing left to finish and its socket
    /// has room again, so a reseed would be read rather than stall in turn.
    fn ready_for_reseed(&self) -> bool {
        if self.closed || self.lagging.is_none() || !self.frames.is_empty() {
            return false;
        }
        let mut descriptor = libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: one valid pollfd for a live socket; a zero timeout never blocks.
        unsafe { libc::poll(&mut descriptor, 1, 0) > 0 && descriptor.revents & libc::POLLOUT != 0 }
    }

    /// Replaces the dropped diffs with the current screen. Returns since
    /// when, and why, the sink had been behind.
    fn reseed(&mut self, grid: Arc<[u8]>, modes: Arc<[u8]>) -> Option<(Instant, &'static str)> {
        let lagging = self.lagging.take()?;
        let _ = self.enqueue(grid) && self.enqueue(modes);
        Some(lagging)
    }

    fn close(&mut self) {
        self.closed = true;
        self.frames.clear();
        self.queued_bytes = 0;
        self.offset = 0;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn encoded(frame: &Frame) -> std::io::Result<Arc<[u8]>> {
    FrameCodec::encode(frame)
        .map(Arc::from)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

/// All live sinks for one session, plus whether a pump is serving them.
#[derive(Default)]
struct SessionSinks {
    sinks: Vec<Sink>,
    pump_running: bool,
    /// The wake source the pump is currently blocked on, so a departing sink
    /// can wake it to notice it may be the last one.
    wake: Option<GridWake>,
}

/// Routes attach connections to per-session pumps.
#[derive(Clone, Default)]
pub struct AttachHub {
    sessions: Arc<Mutex<HashMap<String, SessionSinks>>>,
    next_sink: Arc<AtomicU64>,
    rejections: Arc<Mutex<RejectionLog>>,
    #[cfg(test)]
    registration_hook: Arc<Mutex<Option<RegistrationHook>>>,
}

#[cfg(test)]
type RegistrationHook = Arc<dyn Fn() + Send + Sync>;

/// One `attach.rejected` per session per window: a client that keeps
/// retrying a refused attach must not turn into tens of thousands of events.
const REJECTION_EVENT_EVERY: Duration = Duration::from_secs(60);
/// Sessions remembered by [`RejectionLog`] before stale ones are forgotten.
const REJECTION_LOG_CAP: usize = 256;

#[derive(Default)]
struct RejectionLog(HashMap<String, (Instant, u64)>);

impl RejectionLog {
    /// `Some(suppressed since the last event)` when an event is due now.
    fn due(&mut self, session_id: &str, now: Instant) -> Option<u64> {
        if let Some((last, suppressed)) = self.0.get_mut(session_id) {
            if now.duration_since(*last) < REJECTION_EVENT_EVERY {
                *suppressed += 1;
                return None;
            }
            let count = std::mem::take(suppressed);
            *last = now;
            return Some(count);
        }
        if self.0.len() >= REJECTION_LOG_CAP {
            self.0
                .retain(|_, (last, _)| now.duration_since(*last) < REJECTION_EVENT_EVERY);
        }
        self.0.insert(session_id.to_owned(), (now, 0));
        Some(0)
    }
}

impl AttachHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses an attach for good: one [`FrameType::AttachRejected`] frame,
    /// then the caller closes. A client that knows the frame stops retrying;
    /// an older one fails to decode it and sees the same close as before.
    pub(crate) fn reject(
        &self,
        writer: &Arc<Mutex<UnixStream>>,
        session_id: &str,
        reason: AttachRejection,
    ) {
        if let Some(suppressed) = self
            .rejections
            .lock()
            .ok()
            .and_then(|mut log| log.due(session_id, Instant::now()))
        {
            diri_telemetry::event!(
                "attach.rejected",
                session = diri_telemetry::id(session_id),
                reason = reason.as_str(),
                suppressed = suppressed,
            );
        }
        let Ok(encoded) = FrameCodec::encode(&Frame::attach_rejected(reason)) else {
            return;
        };
        if let Ok(mut stream) = writer.lock() {
            // A fresh socket's buffer takes five bytes; the timeout only
            // guards a peer that is somehow not reading at all.
            let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
            let _ = stream.write_all(&encoded);
        }
    }

    pub fn serve_preview_set(
        &self,
        registry: &Arc<Mutex<Registry>>,
        reader: UnixStream,
        buffered: Vec<u8>,
    ) -> std::io::Result<()> {
        crate::preview_mux::serve(self, registry, reader, buffered)
    }

    pub(crate) fn add_mux(
        &self,
        registry: &Arc<Mutex<Registry>>,
        sink: crate::preview_mux::MuxSink,
    ) -> crate::preview_mux::Admission {
        use crate::preview_mux::{Admission, QueueError};
        let Ok(guard) = registry.lock() else {
            return Admission::Unavailable;
        };
        let id = &sink.member.session_id.0;
        let Some(session) = guard.get(id) else {
            return Admission::Missing;
        };
        let count = self
            .sessions
            .lock()
            .expect("attach hub")
            .values()
            .flat_map(|entry| &entry.sinks)
            .filter(|sink| sink.preview)
            .count();
        if count >= diri_proto::preview::MAX_PREVIEWS {
            return Admission::Limit;
        }
        let seed = session.preview_seed();
        let Ok(grid) = Frame::grid(&seed.grid)
            .map_err(std::io::Error::other)
            .and_then(|frame| encoded(&frame))
        else {
            return Admission::Unavailable;
        };
        let Ok(modes) = encoded(
            &Frame::modes_with_keyboard(
                seed.modes.0,
                seed.modes.1,
                seed.modes.2,
                seed.signature.keyboard,
            )
            .with_secret_input(seed.secret_input),
        ) else {
            return Admission::Unavailable;
        };
        match sink.seed(&[grid, modes]) {
            Ok(()) => {}
            Err(QueueError::Full(needed)) => return Admission::Retry(needed),
            Err(_) => return Admission::Unavailable,
        }
        let sink_id = self.next_sink.fetch_add(1, Ordering::SeqCst);
        let wake = seed.wake.clone();
        self.register(
            registry,
            id,
            sink_id,
            PublicationOutput::Mux(sink.clone()),
            seed,
            true,
        );
        // Seed and registration share the same Registry sequencing boundary as
        // normal publications. Neither queue admission nor registration writes.
        wake.notify();
        Admission::Added(sink_id)
    }

    pub(crate) fn remove_mux(&self, session_id: &str, sink_id: u64) {
        self.deregister(session_id, sink_id);
    }

    /// Runs one attach connection to completion: seeds the sink, registers it
    /// with the session's pump, then loops on incoming frames until the peer
    /// leaves. `reader` may hold bytes buffered past the attach line; they are
    /// fed to the frame codec first.
    pub fn serve(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        reader: UnixStream,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
    ) {
        self.serve_with_keyboard(registry, session_id, false, reader, buffered, writer);
    }

    pub fn serve_with_keyboard(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        enhanced_keyboard: bool,
        reader: UnixStream,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
    ) {
        self.serve_kind(
            registry,
            session_id,
            reader,
            buffered,
            writer,
            (false, enhanced_keyboard),
        );
    }

    pub fn serve_preview(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        reader: UnixStream,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
    ) {
        self.serve_kind(
            registry,
            session_id,
            reader,
            buffered,
            writer,
            (true, false),
        );
    }

    fn serve_kind(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        mut reader: UnixStream,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
        (preview, enhanced_keyboard): (bool, bool),
    ) {
        // Selecting a hibernated session revives it: the seed below paints
        // instantly from the emulator, and the live program resumes
        // underneath — the Swift attach() behavior.
        if !preview {
            let Ok(mut guard) = registry.lock() else {
                return;
            };
            if guard
                .get(session_id)
                .is_some_and(|session| !session.allows_keyboard_controller(enhanced_keyboard))
            {
                drop(guard);
                self.reject(&writer, session_id, AttachRejection::KeyboardUnsupported);
                return;
            }
            let _ = guard.wake_session(session_id);
        }
        let mut output = {
            let Ok(writer) = writer.lock() else {
                return;
            };
            let Ok(stream) = writer.try_clone() else {
                return;
            };
            let Ok(output) = SinkOutput::new(stream) else {
                return;
            };
            output
        };
        output.enhanced_keyboard = enhanced_keyboard;
        output.preview = preview;
        // A completed local session whose Engine has since been replaced has
        // no live Session to attach to, but its final terminal may have been
        // retained. Serve that as a read-only seed on this connection.
        let completed = {
            let Ok(mut guard) = registry.lock() else {
                return;
            };
            if guard.get(session_id).is_some() {
                None
            } else {
                guard.completed_run(session_id).and_then(|handle| {
                    // Watched from the same lock as the lookup: a resume can
                    // only install its Session after this, and closes it.
                    let view = self.next_sink.fetch_add(1, Ordering::SeqCst);
                    let stream = reader.try_clone().ok()?;
                    guard.watch_completed_view(session_id, view, stream);
                    Some((handle, view))
                })
            }
        };
        if let Some((handle, view)) = completed {
            // A retained run whose terminal never made it to disk (the Mac
            // restarted before the checkpoint, or it no longer loads) has
            // nothing to show. Refuse it like a missing session: a bare close
            // reads as a dropped connection, and the pane would retry it every
            // 30 s for as long as it stays open.
            let served =
                self.serve_completed(handle, output, reader, buffered, session_id, preview);
            if let Ok(mut guard) = registry.lock() {
                guard.forget_completed_view(session_id, view);
            }
            if !served && !preview {
                self.reject(&writer, session_id, AttachRejection::SessionNotFound);
            }
            return;
        }
        // The pump serving this sink can reseed it when it falls behind.
        output.reseedable = true;
        let attach_started = Instant::now();
        let seed_bytes;
        // Snapshot, seed queueing and registration share the publisher's
        // Registry sequencing boundary. No update can slip between a new
        // sink's snapshot and admission, and no socket write holds this lock.
        let (sink_id, output, wake) = {
            let Ok(guard) = registry.lock() else {
                return;
            };
            let Some(session) = guard.get(session_id) else {
                drop(guard);
                if !preview {
                    self.reject(&writer, session_id, AttachRejection::SessionNotFound);
                }
                return;
            };
            if preview
                && self
                    .sessions
                    .lock()
                    .expect("attach hub")
                    .values()
                    .flat_map(|entry| &entry.sinks)
                    .filter(|sink| sink.preview)
                    .count()
                    >= diri_proto::preview::MAX_PREVIEWS
            {
                return;
            }
            if !preview && !session.allows_keyboard_controller(enhanced_keyboard) {
                drop(guard);
                self.reject(&writer, session_id, AttachRejection::KeyboardUnsupported);
                return;
            }
            let seed = if preview {
                session.preview_seed()
            } else {
                session.attachment_seed()
            };
            #[cfg(test)]
            if let Some(hook) = self.registration_hook.lock().unwrap().clone() {
                hook();
            }
            let Some(grid) = Frame::grid(&seed.grid)
                .ok()
                .and_then(|frame| FrameCodec::encode(&frame).ok())
            else {
                return;
            };
            let grid = if preview {
                let ready = diri_proto::preview::PreviewReady {
                    preview: diri_proto::SessionId(session_id.to_owned()),
                    version: diri_proto::preview::PREVIEW_VERSION,
                };
                let Ok(mut bytes) = serde_json::to_vec(&ready) else {
                    return;
                };
                bytes.push(b'\n');
                bytes.extend_from_slice(&grid);
                bytes
            } else {
                grid
            };
            seed_bytes = grid.len();
            output.enqueue(Arc::from(grid));
            let Ok(modes) = encoded(
                &Frame::modes_with_keyboard_capability(
                    seed.modes.0,
                    seed.modes.1,
                    seed.modes.2,
                    seed.signature.keyboard,
                    enhanced_keyboard,
                )
                .with_secret_input(seed.secret_input),
            ) else {
                return;
            };
            output.enqueue(modes);
            let output = Arc::new(Mutex::new(output));
            let sink_id = self.next_sink.fetch_add(1, Ordering::SeqCst);
            let wake = seed.wake.clone();
            self.register(
                registry,
                session_id,
                sink_id,
                Arc::clone(&output).into(),
                seed,
                preview,
            );
            (sink_id, output, wake)
        };
        wake.notify();
        let seeded = attach_started.elapsed();
        diri_telemetry::observe("attach.seed", seeded);
        diri_telemetry::debug_event!(
            "attach.open",
            session = diri_telemetry::id(session_id),
            preview = preview,
            seed_bytes = seed_bytes,
            ms = seeded,
        );

        // The read loop is this connection's thread. A feed error means a
        // corrupt stream; a false from handle_frame means the peer's write
        // half died — both end the whole serve.
        let mut codec = FrameCodec::new();
        let mut chunk = [0u8; 64 << 10];
        let mut pending = buffered;
        'serve: while let Ok(frames) = codec.feed(&pending) {
            pending.clear();
            let mut frames = frames.into_iter().peekable();
            while let Some(frame) = frames.next() {
                if preview && !matches!(frame.frame_type, FrameType::Ping | FrameType::Pong) {
                    break 'serve;
                }
                if resize_superseded(&frame, frames.peek()) {
                    continue;
                }
                if !self.handle_frame(
                    registry,
                    session_id,
                    &output,
                    &wake,
                    &frame,
                    enhanced_keyboard,
                ) {
                    break 'serve;
                }
            }
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => pending.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // The FD is nonblocking for the pump's writes. The existing
                    // input thread sleeps in poll, retaining the FrameCodec's
                    // partial header/body across readiness notifications.
                    if !wait_readable(&reader) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        self.deregister(session_id, sink_id);
        let reason = output.lock().ok().and_then(|output| output.close_reason);
        if let Some(reason) = reason {
            // The client reattaches and is reseeded with a full grid.
            diri_telemetry::count("attach.reseeds", 1);
            diri_telemetry::warn_event!(
                "attach.sink_dropped",
                session = diri_telemetry::id(session_id),
                reason = reason,
                preview = preview,
                attached_s = attach_started.elapsed().as_secs(),
            );
        } else {
            diri_telemetry::debug_event!(
                "attach.close",
                session = diri_telemetry::id(session_id),
                preview = preview,
                attached_s = attach_started.elapsed().as_secs(),
            );
        }
    }

    /// Seeds one connection from a retained terminal and then holds it open:
    /// pings are answered, every other frame is swallowed because there is no
    /// child to receive it, and nothing is ever published again. The pane
    /// sees the same thing a live exited session shows, its last screen.
    /// A Session installed under the id (a resume) shuts the connection
    /// down so the client reattaches to it.
    /// False when there was no screen to seed, before anything was written.
    fn serve_completed(
        &self,
        handle: crate::registry::CompletedRunHandle,
        mut output: SinkOutput,
        mut reader: UnixStream,
        buffered: Vec<u8>,
        session_id: &str,
        preview: bool,
    ) -> bool {
        let Ok(Some(terminal)) = handle.load() else {
            return false;
        };
        let Some(mut screen) = terminal.screen() else {
            return false;
        };
        let Ok(grid) = Frame::grid(&screen.grid_update(true))
            .ok()
            .and_then(|frame| FrameCodec::encode(&frame).ok())
            .ok_or(())
        else {
            return false;
        };
        let grid = if preview {
            let ready = diri_proto::preview::PreviewReady {
                preview: diri_proto::SessionId(session_id.to_owned()),
                version: diri_proto::preview::PREVIEW_VERSION,
            };
            let Ok(mut bytes) = serde_json::to_vec(&ready) else {
                return false;
            };
            bytes.push(b'\n');
            bytes.extend_from_slice(&grid);
            bytes
        } else {
            grid
        };
        if !output.enqueue(Arc::from(grid)) {
            return false;
        }
        let Ok(modes) = encoded(&Frame::modes_with_keyboard_capability(
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
            terminal.checkpoint.keyboard,
            output.enhanced_keyboard,
        )) else {
            return false;
        };
        if !output.enqueue(modes) {
            return false;
        }
        // Part of the seed may be on the wire: never splice a rejection after it.
        if !drain_output(&mut output) {
            return true;
        }
        let mut codec = FrameCodec::new();
        let mut chunk = [0u8; 4096];
        let mut pending = buffered;
        'serve: while let Ok(frames) = codec.feed(&pending) {
            pending.clear();
            for frame in frames {
                match frame.frame_type {
                    FrameType::Ping => {
                        let Ok(pong) = encoded(&Frame::pong()) else {
                            break 'serve;
                        };
                        if !output.enqueue(pong) || !drain_output(&mut output) {
                            break 'serve;
                        }
                    }
                    FrameType::Pong => {}
                    _ if preview => break 'serve,
                    _ => {} // no child: input, resize and scroll go nowhere
                }
            }
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => pending.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if !wait_readable(&reader) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        output.close();
        true
    }

    fn handle_frame(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        output: &Arc<Mutex<SinkOutput>>,
        wake: &crate::session::GridWake,
        frame: &Frame,
        enhanced_keyboard: bool,
    ) -> bool {
        if frame.frame_type == FrameType::Ping {
            let Ok(pong) = encoded(&Frame::pong()) else {
                return false;
            };
            let queued = output.lock().is_ok_and(|mut output| output.enqueue(pong));
            wake.notify();
            return queued;
        }
        if frame.frame_type == FrameType::Pong {
            return true;
        }
        if frame.frame_type == FrameType::Resize {
            let Some((cols, rows)) = frame.resize_payload() else {
                return true;
            };
            // Reflow outside the Registry lock: it can take tens of
            // milliseconds over long history, and every session's input
            // and publication needs that lock.
            let reflow = {
                let Ok(guard) = registry.lock() else {
                    return false;
                };
                let Some(session) = guard.get(session_id) else {
                    return true;
                };
                session.resize_pty(cols.max(2), rows.max(2))
            };
            if let Ok(Some(reflow)) = reflow {
                reflow.apply();
            }
            return true;
        }
        let Ok(mut guard) = registry.lock() else {
            return false;
        };
        if frame.frame_type == FrameType::Input
            && guard
                .get(session_id)
                .is_some_and(|session| !session.accepts_keyboard_input(enhanced_keyboard))
        {
            return false;
        }
        if matches!(frame.frame_type, FrameType::Input | FrameType::Mouse) {
            // Input to a frozen session wakes it; write_input's queue covers
            // the race where the governor froze it mid-keystroke.
            let _ = guard.wake_session(session_id);
        }
        let Some(session) = guard.get(session_id) else {
            return true; // session ended; swallow input quietly, as Swift does
        };
        // Guards would move the failed-write check into the patterns; the
        // explicit form keeps every input arm reading the same way.
        #[allow(clippy::collapsible_match)]
        match frame.frame_type {
            FrameType::Input => {
                trace_hop!(InputDecoded);
                if session.write_input(&frame.payload).is_err() {
                    return false;
                }
                trace_hop!(InputHandled);
            }
            FrameType::Mouse => {
                if session.write_mouse(&frame.payload).is_err() {
                    return false;
                }
            }
            FrameType::Scroll => {
                if let Some((direction, lines, col, row)) = frame.scroll_payload() {
                    let _ =
                        session.scroll(direction == 0, lines as usize, col as usize, row as usize);
                }
            }
            _ => {}
        }
        true
    }

    fn register(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        sink_id: u64,
        output: PublicationOutput,
        seed: AttachmentSeed,
        preview: bool,
    ) {
        let mut sessions = self.sessions.lock().expect("attach hub");
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.sinks.push(Sink {
            id: sink_id,
            preview,
            output,
        });
        if !entry.pump_running {
            entry.pump_running = true;
            entry.wake = Some(seed.wake.clone());
            let hub = self.clone();
            let registry = Arc::clone(registry);
            let session_id = session_id.to_string();
            let _ = std::thread::Builder::new()
                .name(format!("diri-attach-{session_id}"))
                .spawn(move || hub.pump(&registry, &session_id, seed));
        }
    }

    /// Whether any client is currently attached to `session_id` — the
    /// governor's "someone is looking at this" signal.
    pub fn has_sinks(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .expect("attach hub")
            .get(session_id)
            .is_some_and(|entry| entry.sinks.iter().any(|sink| !sink.preview))
    }

    /// Attached terminal (non-preview) connections across every session.
    pub fn sink_count(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .flat_map(|entry| &entry.sinks)
                    .filter(|sink| !sink.preview)
                    .count()
            })
            .unwrap_or(0)
    }

    fn deregister(&self, session_id: &str, sink_id: u64) {
        let mut sessions = self.sessions.lock().expect("attach hub");
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.sinks.retain(|sink| {
                if sink.id == sink_id {
                    sink.output.close();
                    false
                } else {
                    true
                }
            });
            // An idle pump waits on the grid alone; without this it would
            // learn it has no sinks left only at its next output.
            if let Some(wake) = &entry.wake {
                wake.notify();
            }
        }
    }

    fn sink_outputs(&self, session_id: &str) -> Vec<(u64, PublicationOutput)> {
        self.sessions
            .lock()
            .expect("attach hub")
            .get(session_id)
            .map(|entry| {
                entry
                    .sinks
                    .iter()
                    .map(|sink| (sink.id, sink.output.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Recipients were captured with the grid under Registry. Looking them
    /// up after encoding could send an older diff behind a newer client's seed.
    #[cfg(test)]
    fn enqueue_publication(
        &self,
        session_id: &str,
        sinks: Vec<(u64, PublicationOutput)>,
        frames: &[Arc<[u8]>],
    ) {
        self.enqueue_with_keyboard(session_id, sinks, frames, None, false);
    }

    fn enqueue_with_keyboard(
        &self,
        session_id: &str,
        sinks: Vec<(u64, PublicationOutput)>,
        frames: &[Arc<[u8]>],
        enhanced_modes: Option<&Arc<[u8]>>,
        requires_enhanced: bool,
    ) {
        for (sink_id, output) in sinks {
            let accepted = match output {
                PublicationOutput::Socket(output) => output.lock().is_ok_and(|mut output| {
                    if !output.preview && !output.enhanced_keyboard && requires_enhanced {
                        output.close();
                        return false;
                    }
                    let enhanced = output.enhanced_keyboard;
                    frames.iter().enumerate().all(|(index, frame)| {
                        let frame = if enhanced && index + 1 == frames.len() {
                            enhanced_modes.unwrap_or(frame)
                        } else {
                            frame
                        };
                        output.enqueue(Arc::clone(frame))
                    })
                }),
                PublicationOutput::Mux(output) => output.publish(frames),
            };
            if !accepted {
                self.deregister(session_id, sink_id);
            }
        }
    }

    /// Writes what each sink can take. Returns whether frames remain queued,
    /// whether any sink is behind, and the sinks that are ready for a reseed.
    fn flush_sinks(&self, session_id: &str) -> (bool, bool, Vec<Arc<Mutex<SinkOutput>>>) {
        let mut pending = false;
        let mut lagging = false;
        let mut reseeds = Vec::new();
        for (id, output) in self.sink_outputs(session_id) {
            let keep = match output {
                PublicationOutput::Socket(output) => {
                    let ready = output
                        .lock()
                        .map(|mut guard| {
                            let keep = guard.flush();
                            // A lagging sink's unfinished frame is retried on the
                            // slower lagging cadence, not the write-retry spin.
                            let behind = keep && guard.lagging.is_some();
                            pending |= !guard.frames.is_empty() && !behind;
                            lagging |= behind;
                            (keep, keep && guard.ready_for_reseed())
                        })
                        .ok();
                    match ready {
                        Some((keep, ready)) => {
                            if ready {
                                reseeds.push(output);
                            }
                            keep
                        }
                        None => false,
                    }
                }
                PublicationOutput::Mux(output) => !output.is_closed(),
            };
            if !keep {
                self.deregister(session_id, id);
            }
        }
        (pending, lagging, reseeds)
    }

    /// Sends each ready lagging sink the current screen and modes. The
    /// snapshot is taken under the Registry, the publication sequencing
    /// boundary, by the pump that publishes every later diff — the same
    /// ordering a fresh attach's seed gets.
    fn reseed_sinks(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        sinks: Vec<Arc<Mutex<SinkOutput>>>,
    ) {
        let Ok(guard) = registry.lock() else { return };
        // Mid-restart the Session is briefly absent; the sinks stay behind
        // and the next pass reseeds them from its replacement.
        let Some(session) = guard.get(session_id) else {
            return;
        };
        let seed = session.preview_seed();
        let Ok(grid) = Frame::grid(&seed.grid)
            .map_err(std::io::Error::other)
            .and_then(|frame| encoded(&frame))
        else {
            return;
        };
        for output in sinks {
            let Ok(mut output) = output.lock() else {
                continue;
            };
            let Ok(modes) = encoded(
                &Frame::modes_with_keyboard_capability(
                    seed.modes.0,
                    seed.modes.1,
                    seed.modes.2,
                    seed.signature.keyboard,
                    output.enhanced_keyboard,
                )
                .with_secret_input(seed.secret_input),
            ) else {
                continue;
            };
            if let Some((since, reason)) = output.reseed(Arc::clone(&grid), modes) {
                diri_telemetry::count("attach.lag_reseeds", 1);
                diri_telemetry::debug_event!(
                    "attach.sink_reseeded",
                    session = diri_telemetry::id(session_id),
                    reason = reason,
                    preview = output.preview,
                    lagged_ms = since.elapsed(),
                );
            }
        }
    }

    /// Wait only on queued output, so a reader freeing socket capacity wakes
    /// the owner immediately. Retaining the outputs keeps every polled fd live;
    /// no Registry, session, or output lock is held across the bounded wait.
    fn wait_for_writable(&self, session_id: &str, timeout: Duration) {
        let outputs = self.sink_outputs(session_id);
        let mut descriptors: Vec<_> = outputs
            .iter()
            .filter_map(|(_, output)| {
                let PublicationOutput::Socket(output) = output else {
                    return None;
                };
                let output = output.lock().ok()?;
                (!output.closed && !output.frames.is_empty()).then(|| libc::pollfd {
                    fd: output.stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                })
            })
            .collect();
        if descriptors.is_empty() {
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
            // SAFETY: retained output Arcs keep all sockets live, and the poll
            // array is exclusively owned for its exact initialized length.
            let result =
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, millis) };
            if result >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                || Instant::now() >= deadline
            {
                return;
            }
        }
    }

    /// The per-session broadcast loop. Grid writers wake it after a complete
    /// PTY output batch. The leading edge and interactive responses publish
    /// immediately; continuous background output coalesces to 8 ms. A quiet
    /// attached terminal performs no Registry or Screen polling. Ends within
    /// one bounded wait after the last sink.
    fn pump(&self, registry: &Arc<Mutex<Registry>>, session_id: &str, seed: AttachmentSeed) {
        let mut signature = seed.signature;
        let mut last_modes = Some((seed.modes, seed.signature.keyboard, seed.secret_input));
        let mut wake = seed.wake;
        let mut wake_generation = seed.wake_generation;
        let mut last_emission = Instant::now()
            .checked_sub(GRID_FLUSH_INTERVAL)
            .unwrap_or_else(Instant::now);
        let stop = AtomicBool::new(false);
        let mut last_owner_check = Instant::now();
        let mut publication_pending = false;
        let mut session_present = true;
        loop {
            let (mut pending, lagging, reseeds) = self.flush_sinks(session_id);
            if !reseeds.is_empty() {
                self.reseed_sinks(registry, session_id, reseeds);
                pending = self.flush_sinks(session_id).0;
            }
            // A publication deadline must not suspend partially sent frames.
            // Remember dirty state while the loop services bounded write retries.
            // Idle, the pump sleeps on the grid: output, a departing sink and
            // the Session's own drop (a restart replacing it) all wake it. Only
            // while the Session is absent mid-restart is there nothing that
            // will, so that state retries on a short ceiling.
            let mut timeout = if publication_pending {
                GRID_FLUSH_INTERVAL.saturating_sub(last_emission.elapsed())
            } else if session_present {
                IDLE_PUMP_CEILING
            } else {
                ABSENT_SESSION_RETRY
            };
            if lagging {
                timeout = timeout.min(LAGGING_RECHECK);
            }
            if pending {
                self.wait_for_writable(session_id, timeout.min(WRITE_RETRY));
                timeout = Duration::ZERO;
            }
            let event = wake.wait_for_change(wake_generation, timeout);
            let mut changed = publication_pending || event.generation != wake_generation;
            let mut interactive = event.interactive;
            wake_generation = event.generation;

            // A restart can replace the Session (and therefore its wake
            // source) while sinks remain connected. The bounded wait above is
            // the recovery ceiling; re-seed from the replacement immediately.
            let replacement_wake = if changed || last_owner_check.elapsed() >= ABSENT_SESSION_RETRY
            {
                last_owner_check = Instant::now();
                let Ok(guard) = registry.lock() else { break };
                let current = guard.get(session_id).map(|session| session.grid_wake());
                session_present = current.is_some();
                current
            } else {
                None
            };
            if let Some(replacement) = replacement_wake
                && !wake.same_source(&replacement)
            {
                if let Some(entry) = self
                    .sessions
                    .lock()
                    .expect("attach hub")
                    .get_mut(session_id)
                {
                    entry.wake = Some(replacement.clone());
                }
                wake = replacement;
                wake_generation = wake.generation();
                signature = GridSignature::default();
                last_modes = None;
                changed = true;
                interactive = true;
            }

            if changed && !interactive && last_emission.elapsed() < GRID_FLUSH_INTERVAL {
                publication_pending = true;
                continue;
            }
            publication_pending = false;
            // The session may be briefly absent mid-restart adoption: keep
            // the sinks, send nothing until it is back.
            let observed = if changed {
                let Ok(guard) = registry.lock() else { break };
                guard.get(session_id).map(|session| {
                    (
                        session.terminal_publication(&mut signature),
                        self.sink_outputs(session_id),
                        !session.allows_keyboard_controller(false),
                    )
                })
            } else {
                None
            };

            let mut frames: Vec<Frame> = Vec::with_capacity(2);
            let mut enhanced_modes = None;
            let mut requires_enhanced = false;
            let mut eligible_sinks = Vec::new();
            if let Some((publication, sinks, requires_capability)) = observed {
                eligible_sinks = sinks;
                let modes = (
                    publication.modes,
                    publication.keyboard,
                    publication.secret_input,
                );
                requires_enhanced = requires_capability;
                if let Some(update) = publication.grid
                    && let Ok(frame) = Frame::grid(&update)
                {
                    frames.push(frame);
                }
                // Fresh sinks get their initial modes at seed time; the pump
                // only broadcasts changes.
                if last_modes != Some(modes) {
                    enhanced_modes = encoded(
                        &Frame::modes_with_keyboard_capability(
                            modes.0.0, modes.0.1, modes.0.2, modes.1, true,
                        )
                        .with_secret_input(modes.2),
                    )
                    .ok();
                    frames.push(
                        Frame::modes_with_keyboard(modes.0.0, modes.0.1, modes.0.2, modes.1)
                            .with_secret_input(modes.2),
                    );
                }
                last_modes = Some(modes);
            }

            if !frames.is_empty() {
                // A few publications per input may bypass coalescing: one can
                // be a trailing change already in flight, the rest are the
                // terminal's response, which a TUI often writes in parts
                // (`INTERACTIVE_GRID_BUDGET`). The bounded budget prevents a
                // keystroke from unthrottling sustained output indefinitely.
                wake.consume_interactive_priority();
                last_emission = Instant::now();
                let encoded_frames = frames
                    .iter()
                    .map(encoded)
                    .collect::<std::io::Result<Vec<_>>>();
                let Ok(encoded_frames) = encoded_frames else {
                    // A stream cannot continue after an unrepresentable grid:
                    // close every affected sink so clients can recover rather
                    // than leaving a registered publisher that will never run.
                    let entry = self.sessions.lock().expect("attach hub").remove(session_id);
                    if let Some(entry) = entry {
                        for sink in entry.sinks {
                            sink.output.close();
                        }
                    }
                    return;
                };
                self.enqueue_with_keyboard(
                    session_id,
                    eligible_sinks,
                    &encoded_frames,
                    enhanced_modes.as_ref(),
                    requires_enhanced,
                );
                trace_hop!(FrameEnqueued);
                wake.note_published_for_telemetry();
            }

            {
                let mut sessions = self.sessions.lock().expect("attach hub");
                if let Some(entry) = sessions.get_mut(session_id)
                    && entry.sinks.is_empty()
                {
                    entry.pump_running = false;
                    sessions.remove(session_id);
                    stop.store(true, Ordering::SeqCst);
                }
            }
            if stop.load(Ordering::SeqCst) {
                break;
            }
        }
    }
}

/// Writes everything queued on a sink whose only writer is this thread,
/// waiting for the socket between bounded write budgets.
fn drain_output(output: &mut SinkOutput) -> bool {
    loop {
        if !output.flush() {
            return false;
        }
        if output.frames.is_empty() {
            return true;
        }
        if !wait_writable(&output.stream) {
            return false;
        }
    }
}

fn wait_writable(stream: &UnixStream) -> bool {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd for a live socket. A hangup or error wakes
        // the poll so the caller's next write fails instead of spinning.
        let result = unsafe { libc::poll(&mut descriptor, 1, WRITE_RETRY.as_millis() as i32) };
        if result > 0 {
            return descriptor.revents & (libc::POLLNVAL | libc::POLLERR) == 0;
        }
        if result == 0 {
            return true; // timed out; let flush judge progress and stalls
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Blocking readiness wait used only by the existing input reader thread.
fn wait_readable(stream: &UnixStream) -> bool {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one live socket and one initialized pollfd. EINTR is retried;
        // hangup/error wakes the following read so shutdown always unwinds.
        let result = unsafe { libc::poll(&mut descriptor, 1, -1) };
        if result > 0 {
            return descriptor.revents & libc::POLLNVAL == 0;
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// A drag sends a resize per display frame. Those that queued up behind a
/// slow reflow are superseded by the next one in the same read, and skipping
/// them spares a history reflow and a SIGWINCH repaint each. Only an adjacent
/// resize supersedes: input or mouse between two resizes keeps its order.
fn resize_superseded(frame: &Frame, next: Option<&Frame>) -> bool {
    frame.frame_type == FrameType::Resize
        && next.is_some_and(|next| next.frame_type == FrameType::Resize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_of_adjacent_resizes_is_applied() {
        let batch = [
            Frame::resize(100, 30),
            Frame::resize(101, 30),
            Frame::input(b"x".to_vec()),
            Frame::resize(102, 30),
            Frame::mouse(b"m".to_vec()),
            Frame::resize(103, 30),
            Frame::resize(104, 30),
        ];
        let applied: Vec<_> = batch
            .iter()
            .enumerate()
            .filter(|(index, frame)| !resize_superseded(frame, batch.get(index + 1)))
            .map(|(_, frame)| (frame.frame_type, frame.resize_payload()))
            .collect();
        assert_eq!(
            applied,
            [
                (FrameType::Resize, Some((101, 30))),
                (FrameType::Input, None),
                (FrameType::Resize, Some((102, 30))),
                (FrameType::Mouse, None),
                (FrameType::Resize, Some((104, 30))),
            ]
        );
    }

    fn constrained_output() -> (SinkOutput, UnixStream) {
        let (writer, reader) = UnixStream::pair().unwrap();
        let size: libc::c_int = 1024;
        // SAFETY: live socket and correctly sized socket option value.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    writer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        reader.set_nonblocking(true).unwrap();
        (SinkOutput::new(writer).unwrap(), reader)
    }

    fn retained_registry(temp: &std::path::Path) -> Arc<Mutex<Registry>> {
        use diri_proto::process::{BootId, ProcessBirth, ProcessIdentity};
        use std::os::unix::fs::PermissionsExt;
        let exit = diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
            interrupted: false,
        };
        let record = diri_proto::SessionRecord {
            attention_state: None,
            id: diri_proto::SessionId("finished".into()),
            kind: diri_proto::AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: diri_proto::ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: diri_proto::TitleSource::Placeholder,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: diri_proto::SessionStatus::Exited(exit.clone()),
            status_evidence: None,
            needs_input: None,
            resumability: diri_proto::Resumability::NotResumable,
            capabilities: None,
            parent: None,
            created_at: diri_proto::DateMillis(1_700_000_000_000.0),
            updated_at: diri_proto::DateMillis(1_700_000_000_000.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            terminal_cwd: None,
            agent_workspace: None,
            note_id: None,
            foreground_ports: None,
            terminal_progress: None,
            scheduled_run: None,
        };
        let child = ProcessIdentity::new(
            4321,
            ProcessBirth::Macos {
                boot_session: BootId::parse("0f0e0d0c-0b0a-0908-0706-050403020100").unwrap(),
                start_seconds: 1_700_000_000,
                start_microseconds: 1,
            },
        )
        .unwrap();
        let key = crate::completed_terminal::CompletedRunKey::bind(&record, child, 10).unwrap();
        let mut screen = diri_terminal_state::HeadlessScreen::new(40, 4);
        screen.feed(b"old line\r\nretained screen\x1b[?2004h");
        let checkpoint = crate::checkpoint::ScreenCheckpoint {
            keyboard_snapshot: screen.keyboard_snapshot(),
            keyboard: Some(screen.keyboard_state()),
            log_offset: 64,
            history: screen.history_snapshot(),
            history_metadata: screen.history_metadata(),
            grid: screen.grid_update(true),
            marker_buffer: Vec::new(),
            alt_screen: false,
            bracketed_paste: true,
            mouse: Default::default(),
        };
        let dir = temp.join(crate::registry::COMPLETED_TERMINALS_DIR_NAME);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::completed_terminal::CompletedTerminalStore::open(&dir)
            .unwrap()
            .publish(&record, &key, &checkpoint, &exit)
            .unwrap();
        let mut registry = Registry::new(
            Arc::new(crate::ManifestEngine::new(Vec::new())),
            temp.join("state.json"),
        );
        registry.insert_record(record);
        let recovery = registry.recovery_directory("finished");
        std::fs::create_dir_all(&recovery).unwrap();
        diri_proto::recovery::SessionRecoveryStore::new(recovery)
            .write_completed_run(&key)
            .unwrap();
        Arc::new(Mutex::new(registry))
    }

    fn read_frames(codec: &mut FrameCodec, stream: &mut UnixStream, want: usize) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut chunk = [0u8; 64 << 10];
        let deadline = Instant::now() + Duration::from_secs(5);
        while frames.len() < want {
            assert!(Instant::now() < deadline, "timed out with {frames:?}");
            match stream.read(&mut chunk) {
                Ok(0) => panic!("stream closed with {frames:?}"),
                Ok(count) => frames.extend(codec.feed(&chunk[..count]).unwrap()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
        frames
    }

    #[test]
    fn attaching_to_a_completed_session_seeds_its_retained_terminal_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let registry = retained_registry(temp.path());
        let hub = AttachHub::new();
        let (mut client, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server.try_clone().unwrap()));
        let serve = {
            let registry = Arc::clone(&registry);
            let hub = hub.clone();
            std::thread::spawn(move || hub.serve(&registry, "finished", server, Vec::new(), writer))
        };
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut codec = FrameCodec::new();
        let frames = read_frames(&mut codec, &mut client, 2);
        let grid = frames[0]
            .grid_payload()
            .unwrap()
            .expect("a grid seed first");
        assert!(grid.is_full_snapshot);
        assert_eq!((grid.cols, grid.rows), (40, 4));
        let text: String = grid.changed_rows[1]
            .cells
            .iter()
            .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
            .collect();
        assert!(text.starts_with("retained screen"), "{text:?}");
        let (alt_screen, bracketed_paste, _) = frames[1].terminal_modes_payload().expect("modes");
        assert!(!alt_screen);
        assert!(bracketed_paste, "the retained modes travel with the grid");

        // The connection stays open: pings are answered, input goes nowhere,
        // and nothing else is ever published.
        for _ in 0..2 {
            client
                .write_all(&FrameCodec::encode(&Frame::input(b"typed".to_vec())).unwrap())
                .unwrap();
            client
                .write_all(&FrameCodec::encode(&Frame::ping()).unwrap())
                .unwrap();
            let frames = read_frames(&mut codec, &mut client, 1);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].frame_type, FrameType::Pong);
        }
        assert!(
            !hub.has_sinks("finished"),
            "a retained terminal registers no publisher"
        );
        drop(client);
        serve.join().unwrap();
    }

    #[test]
    fn resuming_a_completed_run_closes_its_read_only_attach() {
        // Selecting an ended tab after an Engine restart attaches to its
        // retained terminal; the auto-resume lands right after. That
        // connection must end, or the pane keeps it and every keystroke
        // is swallowed while the resumed Agent waits at its prompt.
        let temp = tempfile::tempdir().unwrap();
        let registry = retained_registry(temp.path());
        let hub = AttachHub::new();
        let (mut client, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server.try_clone().unwrap()));
        let serve = {
            let registry = Arc::clone(&registry);
            let hub = hub.clone();
            std::thread::spawn(move || hub.serve(&registry, "finished", server, Vec::new(), writer))
        };
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut codec = FrameCodec::new();
        read_frames(&mut codec, &mut client, 2);

        let engine = Arc::new(crate::ManifestEngine::new(Vec::new()));
        registry
            .lock()
            .unwrap()
            .respawn(crate::session::SessionSpec {
                id: "finished".into(),
                pty: crate::pty::PtySpec::new(
                    vec!["/bin/sh".into(), "-c".into(), "read line".into()],
                    temp.path(),
                )
                .size(40, 4),
                manifest_id: "shell".into(),
                authority: crate::session::authority_for("shell", &engine),
                logs_dir: temp.path().join("logs"),
                holder: None,
                remote: None,
                defer_launch: false,
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut chunk = [0u8; 4096];
        loop {
            assert!(
                Instant::now() < deadline,
                "the read-only attach stayed open"
            );
            match client.read(&mut chunk) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        serve.join().unwrap();

        // A fresh attach now reaches the live run.
        let (mut client, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server.try_clone().unwrap()));
        let serve = {
            let registry = Arc::clone(&registry);
            let hub = hub.clone();
            std::thread::spawn(move || hub.serve(&registry, "finished", server, Vec::new(), writer))
        };
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        read_frames(&mut FrameCodec::new(), &mut client, 2);
        assert!(hub.has_sinks("finished"), "attached to the live Session");
        drop(client);
        serve.join().unwrap();
        let _ = registry
            .lock()
            .unwrap()
            .terminate("finished", Duration::from_millis(500));
    }

    #[test]
    fn a_completed_run_whose_terminal_was_never_retained_is_refused() {
        // A reboot ends the Holder before the final checkpoint is published:
        // the run is still recorded, but there is no screen to seed. A bare
        // close read as a dropped connection and the pane retried it forever.
        let temp = tempfile::tempdir().unwrap();
        let registry = retained_registry(temp.path());
        let dir = temp
            .path()
            .join(crate::registry::COMPLETED_TERMINALS_DIR_NAME);
        for entry in std::fs::read_dir(&dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let hub = AttachHub::new();
        let (mut client, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server.try_clone().unwrap()));
        let serve = {
            let registry = Arc::clone(&registry);
            let hub = hub.clone();
            std::thread::spawn(move || hub.serve(&registry, "finished", server, Vec::new(), writer))
        };
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut codec = FrameCodec::new();
        let frames = read_frames(&mut codec, &mut client, 1);
        assert_eq!(
            frames[0].attach_rejected_payload(),
            Some(AttachRejection::SessionNotFound)
        );
        serve.join().unwrap();
    }

    #[test]
    fn keyboard_publication_preserves_legacy_bytes_and_shares_grid() {
        let hub = AttachHub::new();
        let (legacy, _legacy_reader) = constrained_output();
        let (mut capable, _capable_reader) = constrained_output();
        let (mut preview, _preview_reader) = constrained_output();
        capable.enhanced_keyboard = true;
        preview.preview = true;
        let legacy = Arc::new(Mutex::new(legacy));
        let capable = Arc::new(Mutex::new(capable));
        let preview = Arc::new(Mutex::new(preview));
        let sinks = || {
            vec![
                (1, Arc::clone(&legacy).into()),
                (2, Arc::clone(&capable).into()),
                (3, Arc::clone(&preview).into()),
            ]
        };
        let keyboard = Some(diri_proto::terminal_input::KeyboardState {
            enhancements: Some(0.try_into().unwrap()),
            ..Default::default()
        });
        let grid = encoded(
            &Frame::grid(&diri_terminal_state::HeadlessScreen::new(4, 2).full_snapshot()).unwrap(),
        )
        .unwrap();
        let v1 = encoded(&Frame::modes_with_keyboard(
            false,
            false,
            Default::default(),
            keyboard,
        ))
        .unwrap();
        let v2 = encoded(&Frame::modes_with_keyboard_capability(
            false,
            false,
            Default::default(),
            keyboard,
            true,
        ))
        .unwrap();
        hub.enqueue_with_keyboard(
            "fixture",
            sinks(),
            &[Arc::clone(&grid), Arc::clone(&v1)],
            Some(&v2),
            false,
        );
        assert!(Arc::ptr_eq(
            legacy.lock().unwrap().frames.front().unwrap(),
            &grid
        ));
        assert!(Arc::ptr_eq(
            capable.lock().unwrap().frames.front().unwrap(),
            &grid
        ));
        assert!(Arc::ptr_eq(
            legacy.lock().unwrap().frames.back().unwrap(),
            &v1
        ));
        assert!(Arc::ptr_eq(
            capable.lock().unwrap().frames.back().unwrap(),
            &v2
        ));
        assert!(Arc::ptr_eq(
            preview.lock().unwrap().frames.back().unwrap(),
            &v1
        ));
        hub.enqueue_with_keyboard("fixture", sinks(), &[v1], Some(&v2), true);
        assert!(legacy.lock().unwrap().closed);
        assert!(!capable.lock().unwrap().closed);
        assert!(!preview.lock().unwrap().closed);
    }

    #[test]
    fn late_registration_cannot_receive_a_pre_seed_publication() {
        let hub = AttachHub::new();
        let (old, _old_reader) = constrained_output();
        let old = Arc::new(Mutex::new(old));
        hub.sessions.lock().unwrap().insert(
            "s".into(),
            SessionSinks {
                sinks: vec![Sink {
                    id: 1,
                    preview: false,
                    output: Arc::clone(&old).into(),
                }],
                pump_running: true,
                wake: None,
            },
        );
        // A publisher captures its recipients along with an older grid. Delay
        // its encoding/queueing until after a newer sink has registered.
        let recipients = hub.sink_outputs("s");
        let (mut new, _new_reader) = constrained_output();
        let new_seed = encoded(&Frame::input(b"new seed".to_vec())).unwrap();
        assert!(new.enqueue(Arc::clone(&new_seed)));
        let new = Arc::new(Mutex::new(new));
        hub.sessions
            .lock()
            .unwrap()
            .get_mut("s")
            .unwrap()
            .sinks
            .push(Sink {
                id: 2,
                preview: false,
                output: Arc::clone(&new).into(),
            });
        hub.enqueue_publication(
            "s",
            recipients,
            &[encoded(&Frame::input(b"old diff".to_vec())).unwrap()],
        );
        let output = new.lock().unwrap();
        assert_eq!(
            output.frames.len(),
            1,
            "older publication must not follow a newer seed"
        );
        assert_eq!(&**output.frames.front().unwrap(), &*new_seed);
        drop(output);
        hub.enqueue_publication(
            "s",
            hub.sink_outputs("s"),
            &[encoded(&Frame::input(b"subsequent diff".to_vec())).unwrap()],
        );
        assert_eq!(new.lock().unwrap().frames.len(), 2);
        assert_eq!(old.lock().unwrap().frames.len(), 2);
    }

    #[test]
    fn an_idle_pump_stops_as_soon_as_its_last_sink_leaves() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) =
            crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
                .unwrap();
        let engine = Arc::new(engine);
        let record: diri_proto::SessionRecord = serde_json::from_value(serde_json::json!({
            "id":"s", "kind":diri_proto::AgentKind::new("generic"), "cwd":temp.path(),
            "projectID":"p", "title":"fixture", "titleSource":diri_proto::TitleSource::Placeholder,
            "status":diri_proto::SessionStatus::Idle, "resumability":diri_proto::Resumability::Live,
            "createdAt":0.0,"updatedAt":0.0,"pinned":false
        }))
        .unwrap();
        let mut registry = Registry::new(Arc::clone(&engine), temp.path().join("state.json"));
        registry
            .spawn(
                crate::session::SessionSpec {
                    id: "s".into(),
                    pty: crate::pty::PtySpec::new(
                        vec!["/bin/sh".into(), "-c".into(), "read line".into()],
                        temp.path(),
                    )
                    .size(80, 24),
                    manifest_id: "generic".into(),
                    authority: crate::session::authority_for("generic", &engine),
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                record,
            )
            .unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let hub = AttachHub::new();
        let (writer, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = {
            let hub = hub.clone();
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                hub.serve(
                    &registry,
                    "s",
                    writer.try_clone().unwrap(),
                    Vec::new(),
                    Arc::new(Mutex::new(writer)),
                );
            })
        };
        let mut bytes = [0; 65536];
        assert!(reader.read(&mut bytes).unwrap() > 0, "seeded");
        // Let the pump settle into its idle wait on an unchanging grid.
        std::thread::sleep(Duration::from_millis(300));
        assert!(hub.sessions.lock().unwrap().contains_key("s"));

        drop(reader);
        worker.join().unwrap();
        let left = Instant::now();
        while hub.sessions.lock().unwrap().contains_key("s") {
            assert!(
                left.elapsed() < Duration::from_millis(300),
                "an idle pump must be woken by its last sink leaving, not find out later"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn output_between_seed_and_registration_is_not_lost() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) =
            crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
                .unwrap();
        let engine = Arc::new(engine);
        let record: diri_proto::SessionRecord = serde_json::from_value(serde_json::json!({
            "id":"s", "kind":diri_proto::AgentKind::new("generic"), "cwd":temp.path(),
            "projectID":"p", "title":"fixture", "titleSource":diri_proto::TitleSource::Placeholder,
            "status":diri_proto::SessionStatus::Idle, "resumability":diri_proto::Resumability::Live,
            "createdAt":0.0,"updatedAt":0.0,"pinned":false
        }))
        .unwrap();
        let mut registry = Registry::new(Arc::clone(&engine), temp.path().join("state.json"));
        registry
            .spawn(
                crate::session::SessionSpec {
                    id: "s".into(),
                    pty: crate::pty::PtySpec::new(
                        vec!["/bin/sh".into(), "-c".into(),
                "while [ ! -f ready ]; do sleep 0.01; done; printf 'new-output'; read line".into()],
                        temp.path(),
                    )
                    .size(80, 24),
                    manifest_id: "generic".into(),
                    authority: crate::session::authority_for("generic", &engine),
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                record,
            )
            .unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let hub = AttachHub::new();
        let open = || {
            let (writer, reader) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let hub = hub.clone();
            let registry = Arc::clone(&registry);
            let worker = std::thread::spawn(move || {
                hub.serve(
                    &registry,
                    "s",
                    writer.try_clone().unwrap(),
                    Vec::new(),
                    Arc::new(Mutex::new(writer)),
                );
            });
            (reader, worker)
        };
        let receive = |reader: &mut UnixStream, kind: FrameType| {
            let mut codec = FrameCodec::new();
            let mut bytes = [0; 65536];
            loop {
                let count = reader.read(&mut bytes).unwrap();
                assert!(count > 0);
                if let Some(frame) = codec
                    .feed(&bytes[..count])
                    .unwrap()
                    .into_iter()
                    .find(|frame| frame.frame_type == kind)
                {
                    break frame;
                }
            }
        };
        let (mut existing, first_worker) = open();
        receive(&mut existing, FrameType::Modes);
        existing
            .write_all(&FrameCodec::encode(&Frame::ping()).unwrap())
            .unwrap();
        receive(&mut existing, FrameType::Pong);
        let wake = registry.lock().unwrap().get("s").unwrap().grid_wake();
        let before = wake.generation();
        let (seeded_tx, seeded_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let resume_rx = Mutex::new(resume_rx);
        *hub.registration_hook.lock().unwrap() = Some(Arc::new(move || {
            seeded_tx.send(()).unwrap();
            resume_rx.lock().unwrap().recv().unwrap();
        }));
        let (mut late, late_worker) = open();
        seeded_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        std::fs::write(temp.path().join("ready"), b"").unwrap();
        let event = wake.wait_for_change(before, Duration::from_secs(2));
        assert!(
            event.generation > before,
            "PTY output must land while registration is paused"
        );
        assert!(
            registry.try_lock().is_err(),
            "seed and admission must exclude a publication between them"
        );
        resume_tx.send(()).unwrap();
        let mut codec = FrameCodec::new();
        let mut bytes = [0; 65536];
        let mut saw_seed = false;
        loop {
            let count = late.read(&mut bytes).unwrap();
            assert!(count > 0);
            let mut saw_output = false;
            for frame in codec.feed(&bytes[..count]).unwrap() {
                if let Some(grid) = frame.grid_payload().unwrap() {
                    if !saw_seed {
                        assert!(grid.is_full_snapshot);
                        saw_seed = true;
                    }
                    saw_output |= grid.changed_rows.iter().any(|row| {
                        row.cells
                            .iter()
                            .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                            .collect::<String>()
                            .contains("new-output")
                    });
                }
            }
            if saw_output {
                break;
            }
        }
        assert!(saw_seed);
        let _ = existing.shutdown(std::net::Shutdown::Both);
        let _ = late.shutdown(std::net::Shutdown::Both);
        first_worker.join().unwrap();
        late_worker.join().unwrap();
        registry
            .lock()
            .unwrap()
            .remove("s", &temp.path().join("logs"))
            .unwrap();
    }

    #[test]
    fn partial_writes_keep_exact_frame_boundaries_and_release_retained_bytes() {
        let (mut output, mut reader) = constrained_output();
        let payload = (0..2 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let first = Frame::input(payload);
        let second =
            Frame::modes_with_bracketed_paste(false, true, diri_proto::terminal::MouseModes::OFF);
        let first_bytes = encoded(&first).unwrap();
        let second_bytes = encoded(&second).unwrap();
        let retained = first_bytes.len() + second_bytes.len();
        assert!(output.enqueue(Arc::clone(&first_bytes)));
        assert!(output.enqueue(second_bytes));
        assert!(output.flush());
        assert!(output.offset > 0 && output.offset < first_bytes.len());
        assert_eq!(
            output.queued_bytes, retained,
            "partial prefixes still retain their allocation"
        );
        let mut received = Vec::new();
        let mut bytes = [0; 997];
        let deadline = Instant::now() + Duration::from_secs(5);
        while received.len() < retained {
            assert!(Instant::now() < deadline);
            assert!(output.flush());
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => panic!("closed before complete frame"),
                    Ok(count) => received.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                }
            }
        }
        assert_eq!(
            FrameCodec::new().feed(&received).unwrap(),
            vec![first, second]
        );
        assert_eq!(output.queued_bytes, 0);
        assert_eq!(output.offset, 0);
        assert!(output.frames.is_empty());
    }

    #[test]
    fn overflow_after_partial_frame_closes_instead_of_splicing_a_new_frame() {
        let (mut output, mut reader) = constrained_output();
        let first = encoded(&Frame::input(vec![1; 2 * 1024 * 1024])).unwrap();
        assert!(output.enqueue(first));
        assert!(output.flush());
        let written = output.offset;
        assert!(written > 0);
        assert!(!output.enqueue(encoded(&Frame::input(vec![2; 1024 * 1024])).unwrap()));
        assert!(output.closed);
        assert_eq!(output.queued_bytes, 0);
        assert!(output.frames.is_empty());
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received.len(), written);
        assert!(
            FrameCodec::new().feed(&received).unwrap().is_empty(),
            "only the unfinished first frame may be received"
        );
    }

    #[test]
    fn a_stalled_reseedable_sink_drops_stale_diffs_and_waits_for_room() {
        let (mut output, mut reader) = constrained_output();
        output.reseedable = true;
        let diff = encoded(&Frame::input(vec![1; 65536])).unwrap();
        assert!(output.enqueue(Arc::clone(&diff)));
        assert!(output.flush());
        let written = output.offset;
        assert!(written > 0, "the socket takes a prefix of the first frame");
        assert!(output.enqueue(encoded(&Frame::pong()).unwrap()));
        output.last_progress = Instant::now() - STALLED_SINK_TIMEOUT;
        assert!(output.flush(), "a slow client is not dropped");
        assert_eq!(output.lagging.map(|(_, reason)| reason), Some("stalled"));
        assert_eq!(output.frames.len(), 1, "only the unfinished frame stays");
        assert_eq!(output.queued_bytes, diff.len());
        assert!(output.enqueue(encoded(&Frame::pong()).unwrap()));
        assert_eq!(output.frames.len(), 1, "diffs while behind are stale");
        assert!(
            !output.ready_for_reseed(),
            "the unfinished frame comes first"
        );

        // The client reads again: the unfinished frame completes intact.
        let mut received = Vec::new();
        let mut bytes = [0; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.frames.is_empty() {
            assert!(Instant::now() < deadline);
            assert!(output.flush());
            while let Ok(count) = reader.read(&mut bytes) {
                received.extend_from_slice(&bytes[..count]);
            }
        }
        while let Ok(count) = reader.read(&mut bytes) {
            received.extend_from_slice(&bytes[..count]);
        }
        assert!(output.ready_for_reseed());
        let grid = encoded(&Frame::pong()).unwrap();
        let modes = encoded(&Frame::ping()).unwrap();
        assert_eq!(
            output.reseed(grid, modes).map(|(_, reason)| reason),
            Some("stalled")
        );
        assert!(output.lagging.is_none());
        assert!(output.flush());
        assert!(output.frames.is_empty());
        while let Ok(count) = reader.read(&mut bytes) {
            received.extend_from_slice(&bytes[..count]);
        }
        let frames = FrameCodec::new().feed(&received).unwrap();
        let kinds: Vec<_> = frames.iter().map(|frame| frame.frame_type).collect();
        assert_eq!(kinds, [FrameType::Input, FrameType::Pong, FrameType::Ping]);
    }

    #[test]
    fn a_reseedable_sink_survives_backlog_and_is_dropped_only_when_wedged() {
        let (mut output, _reader) = constrained_output();
        output.reseedable = true;
        let pong = encoded(&Frame::pong()).unwrap();
        for _ in 0..=SINK_BACKLOG_FRAMES {
            assert!(output.enqueue(Arc::clone(&pong)));
        }
        assert!(!output.closed);
        assert_eq!(output.lagging.map(|(_, reason)| reason), Some("backlog"));
        assert!(output.frames.is_empty() && output.queued_bytes == 0);
        assert!(output.flush());
        output.lagging = Some((Instant::now() - LAGGING_SINK_LIMIT, "backlog"));
        assert!(!output.flush(), "a peer behind for too long is wedged");
        assert_eq!(output.close_reason, Some("backlog"));
    }

    /// A client whose process is descheduled for longer than the stall
    /// timeout (App Nap, memory pressure) keeps its connection: the Engine
    /// discards what it missed and reseeds it with the current screen.
    #[test]
    fn a_consumer_paused_past_the_stall_timeout_is_reseeded_not_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) =
            crate::detect::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir())
                .unwrap();
        let engine = Arc::new(engine);
        let record: diri_proto::SessionRecord = serde_json::from_value(serde_json::json!({
            "id":"s", "kind":diri_proto::AgentKind::new("generic"), "cwd":temp.path(),
            "projectID":"p", "title":"fixture", "titleSource":diri_proto::TitleSource::Placeholder,
            "status":diri_proto::SessionStatus::Idle, "resumability":diri_proto::Resumability::Live,
            "createdAt":0.0,"updatedAt":0.0,"pinned":false
        }))
        .unwrap();
        let mut registry = Registry::new(Arc::clone(&engine), temp.path().join("state.json"));
        registry
            .spawn(
                crate::session::SessionSpec {
                    id: "s".into(),
                    pty: crate::pty::PtySpec::new(
                        vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            "i=0; while :; do i=$((i+1)); printf 'output line %d %0200d\\n' $i 0; sleep 0.002; done".into(),
                        ],
                        temp.path(),
                    )
                    .size(80, 24),
                    manifest_id: "generic".into(),
                    authority: crate::session::authority_for("generic", &engine),
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                record,
            )
            .unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let hub = AttachHub::new();
        let (writer, mut reader) = UnixStream::pair().unwrap();
        let size: libc::c_int = 4096;
        // SAFETY: live socket and correctly sized socket option value.
        unsafe {
            libc::setsockopt(
                writer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            );
        }
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = {
            let hub = hub.clone();
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                hub.serve(
                    &registry,
                    "s",
                    writer.try_clone().unwrap(),
                    Vec::new(),
                    Arc::new(Mutex::new(writer)),
                );
            })
        };
        let mut codec = FrameCodec::new();
        let mut bytes = [0; 65536];
        let mut next_grids = |reader: &mut UnixStream, want: usize| {
            let mut grids = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while grids.len() < want {
                assert!(Instant::now() < deadline, "no output after {grids:?}");
                let count = match reader.read(&mut bytes) {
                    Ok(count) => count,
                    // Signals from sibling tests' children, or a loaded runner.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => panic!("the connection stays open: {error}"),
                };
                assert!(count > 0, "the Engine dropped a merely slow client");
                for frame in codec.feed(&bytes[..count]).expect("intact frames") {
                    if let Some(grid) = frame.grid_payload().unwrap() {
                        grids.push(grid.is_full_snapshot);
                    }
                }
            }
            grids
        };
        // One read can carry more than one frame on a loaded machine (CI has
        // delivered the seed and a second snapshot together), so only the
        // first frame is the seed's to answer for.
        assert_eq!(
            next_grids(&mut reader, 1).first(),
            Some(&true),
            "the attach seed"
        );
        next_grids(&mut reader, 3);
        // The consumer is descheduled while the session keeps printing.
        std::thread::sleep(STALLED_SINK_TIMEOUT + Duration::from_millis(500));
        let resumed = Instant::now();
        let mut reseeded = false;
        while !reseeded {
            reseeded = next_grids(&mut reader, 1)[0];
        }
        let caught_up = resumed.elapsed();
        assert!(
            caught_up < Duration::from_secs(1),
            "reseed took {caught_up:?} after the consumer resumed"
        );
        // Live diffs follow the reseed (a loaded runner may reseed again first).
        let mut live = Vec::new();
        while !live.iter().any(|full| !full) {
            live.extend(next_grids(&mut reader, 1));
            assert!(live.len() < 64, "no live diff after the reseed: {live:?}");
        }
        assert!(hub.has_sinks("s"), "the sink was never dropped");
        let _ = reader.shutdown(std::net::Shutdown::Both);
        worker.join().unwrap();
        registry
            .lock()
            .unwrap()
            .remove("s", &temp.path().join("logs"))
            .unwrap();
    }

    #[test]
    fn small_frames_are_count_bounded_and_stalled_sinks_close() {
        let (mut output, _) = constrained_output();
        let pong = encoded(&Frame::pong()).unwrap();
        for _ in 0..SINK_BACKLOG_FRAMES {
            assert!(output.enqueue(Arc::clone(&pong)));
        }
        assert!(!output.enqueue(pong));
        assert_eq!(output.queued_bytes, 0);
        let (mut output, _reader) = constrained_output();
        assert!(output.enqueue(encoded(&Frame::input(vec![1; 65536])).unwrap()));
        assert!(output.flush());
        output.last_progress = Instant::now() - STALLED_SINK_TIMEOUT;
        assert!(!output.flush());
        assert!(output.closed);
    }
}
