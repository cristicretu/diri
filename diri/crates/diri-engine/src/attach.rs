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

use diri_proto::frames::{Frame, FrameCodec, FrameType};

use crate::registry::Registry;
use crate::session::{AttachmentSeed, GridSignature};

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
const STALLED_SINK_TIMEOUT: Duration = Duration::from_secs(2);

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
        if self.closed
            || self.frames.len() >= SINK_BACKLOG_FRAMES
            || self.queued_bytes.saturating_add(bytes.len()) > limit
        {
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
            self.close();
        }
        !self.closed
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
}

/// Routes attach connections to per-session pumps.
#[derive(Clone, Default)]
pub struct AttachHub {
    sessions: Arc<Mutex<HashMap<String, SessionSinks>>>,
    next_sink: Arc<AtomicU64>,
    #[cfg(test)]
    registration_hook: Arc<Mutex<Option<RegistrationHook>>>,
}

#[cfg(test)]
type RegistrationHook = Arc<dyn Fn() + Send + Sync>;

impl AttachHub {
    pub fn new() -> Self {
        Self::default()
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
            let Ok(guard) = registry.lock() else {
                return;
            };
            if guard.get(session_id).is_some() {
                None
            } else {
                guard.completed_run(session_id)
            }
        };
        if let Some(handle) = completed {
            self.serve_completed(handle, output, reader, buffered, session_id, preview);
            return;
        }
        // Snapshot, seed queueing and registration share the publisher's
        // Registry sequencing boundary. No update can slip between a new
        // sink's snapshot and admission, and no socket write holds this lock.
        let (sink_id, output, wake) = {
            let Ok(guard) = registry.lock() else {
                return;
            };
            let Some(session) = guard.get(session_id) else {
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

        // The read loop is this connection's thread. A feed error means a
        // corrupt stream; a false from handle_frame means the peer's write
        // half died — both end the whole serve.
        let mut codec = FrameCodec::new();
        let mut chunk = [0u8; 64 << 10];
        let mut pending = buffered;
        'serve: while let Ok(frames) = codec.feed(&pending) {
            pending.clear();
            for frame in frames {
                if preview && !matches!(frame.frame_type, FrameType::Ping | FrameType::Pong) {
                    break 'serve;
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
    }

    /// Seeds one connection from a retained terminal and then holds it open:
    /// pings are answered, every other frame is swallowed because there is no
    /// child to receive it, and nothing is ever published again. The pane
    /// sees the same thing a live exited session shows, its last screen.
    fn serve_completed(
        &self,
        handle: crate::registry::CompletedRunHandle,
        mut output: SinkOutput,
        mut reader: UnixStream,
        buffered: Vec<u8>,
        session_id: &str,
        preview: bool,
    ) {
        let Ok(Some(terminal)) = handle.load() else {
            return;
        };
        let Some(mut screen) = terminal.screen() else {
            return;
        };
        let Ok(grid) = Frame::grid(&screen.grid_update(true))
            .ok()
            .and_then(|frame| FrameCodec::encode(&frame).ok())
            .ok_or(())
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
        if !output.enqueue(Arc::from(grid)) {
            return;
        }
        let Ok(modes) = encoded(&Frame::modes_with_keyboard_capability(
            screen.is_alt_screen(),
            screen.bracketed_paste(),
            screen.mouse_modes(),
            terminal.checkpoint.keyboard,
            output.enhanced_keyboard,
        )) else {
            return;
        };
        if !output.enqueue(modes) || !drain_output(&mut output) {
            return;
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
        match frame.frame_type {
            FrameType::Input => {
                if session.write_input(&frame.payload).is_err() {
                    return false;
                }
            }
            FrameType::Mouse => {
                if session.write_mouse(&frame.payload).is_err() {
                    return false;
                }
            }
            FrameType::Resize => {
                if let Some((cols, rows)) = frame.resize_payload() {
                    let _ = session.resize(cols.max(2), rows.max(2));
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

    fn flush_sinks(&self, session_id: &str) -> bool {
        let mut pending = false;
        for (id, output) in self.sink_outputs(session_id) {
            let keep = match output {
                PublicationOutput::Socket(output) => output.lock().is_ok_and(|mut output| {
                    let keep = output.flush();
                    pending |= !output.frames.is_empty();
                    keep
                }),
                PublicationOutput::Mux(output) => !output.is_closed(),
            };
            if !keep {
                self.deregister(session_id, id);
            }
        }
        pending
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
        loop {
            let pending = self.flush_sinks(session_id);
            // A publication deadline must not suspend partially sent frames.
            // Remember dirty state while the loop services bounded write retries.
            let mut timeout = if publication_pending {
                GRID_FLUSH_INTERVAL.saturating_sub(last_emission.elapsed())
            } else {
                Duration::from_secs(1)
            };
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
            let replacement_wake =
                if changed || last_owner_check.elapsed() >= Duration::from_secs(1) {
                    last_owner_check = Instant::now();
                    let Ok(guard) = registry.lock() else { break };
                    guard.get(session_id).map(|session| session.grid_wake())
                } else {
                    None
                };
            if let Some(replacement) = replacement_wake
                && !wake.same_source(&replacement)
            {
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
                // Two publications per input may bypass coalescing: one can
                // be a trailing change already in flight, and the next is the
                // actual terminal response. The bounded budget prevents a
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

#[cfg(test)]
mod tests {
    use super::*;

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
