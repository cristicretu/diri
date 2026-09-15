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
    output: Arc<Mutex<SinkOutput>>,
}

// Queues own only Arc references: a publication is encoded once for all sinks.
// One oversized-but-valid seed is allowed; ordinary backlog remains <= 1 MiB.
const SINK_BACKLOG_BYTES: usize = 1024 * 1024;
const SINK_BACKLOG_FRAMES: usize = 64;
const WRITE_BUDGET_BYTES: usize = 256 * 1024;
const WRITE_RETRY: Duration = Duration::from_millis(1);
const STALLED_SINK_TIMEOUT: Duration = Duration::from_secs(2);

struct SinkOutput {
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
        self.serve_kind(registry, session_id, reader, buffered, writer, false);
    }

    pub fn serve_preview(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        reader: UnixStream,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
    ) {
        self.serve_kind(registry, session_id, reader, buffered, writer, true);
    }

    fn serve_kind(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        mut reader: UnixStream,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
        preview: bool,
    ) {
        // Selecting a hibernated session revives it: the seed below paints
        // instantly from the emulator, and the live program resumes
        // underneath — the Swift attach() behavior.
        if !preview {
            let Ok(mut guard) = registry.lock() else {
                return;
            };
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
            let Ok(modes) = encoded(&Frame::modes_with_bracketed_paste(
                seed.modes.0,
                seed.modes.1,
                seed.modes.2,
            )) else {
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
                Arc::clone(&output),
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
                if !self.handle_frame(registry, session_id, &output, &wake, &frame) {
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

    fn handle_frame(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        output: &Arc<Mutex<SinkOutput>>,
        wake: &crate::session::GridWake,
        frame: &Frame,
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
        output: Arc<Mutex<SinkOutput>>,
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
                    if let Ok(mut output) = sink.output.lock() {
                        output.close();
                    }
                    false
                } else {
                    true
                }
            });
        }
    }

    fn sink_outputs(&self, session_id: &str) -> Vec<(u64, Arc<Mutex<SinkOutput>>)> {
        self.sessions
            .lock()
            .expect("attach hub")
            .get(session_id)
            .map(|entry| {
                entry
                    .sinks
                    .iter()
                    .map(|sink| (sink.id, Arc::clone(&sink.output)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Recipients were captured with the grid under Registry. Looking them
    /// up after encoding could send an older diff behind a newer client's seed.
    fn enqueue_publication(
        &self,
        session_id: &str,
        sinks: Vec<(u64, Arc<Mutex<SinkOutput>>)>,
        frames: &[Arc<[u8]>],
    ) {
        for (sink_id, output) in sinks {
            let accepted = output.lock().is_ok_and(|mut output| {
                frames.iter().all(|frame| output.enqueue(Arc::clone(frame)))
            });
            if !accepted {
                self.deregister(session_id, sink_id);
            }
        }
    }

    fn flush_sinks(&self, session_id: &str) -> bool {
        let mut pending = false;
        for (id, output) in self.sink_outputs(session_id) {
            let keep = output.lock().is_ok_and(|mut output| {
                let keep = output.flush();
                pending |= !output.frames.is_empty();
                keep
            });
            if !keep {
                self.deregister(session_id, id);
            }
        }
        pending
    }

    /// The per-session broadcast loop. Grid writers wake it after a complete
    /// PTY output batch. The leading edge and interactive responses publish
    /// immediately; continuous background output coalesces to 8 ms. A quiet
    /// attached terminal performs no Registry or Screen polling. Ends within
    /// one bounded wait after the last sink.
    fn pump(&self, registry: &Arc<Mutex<Registry>>, session_id: &str, seed: AttachmentSeed) {
        let mut signature = seed.signature;
        let mut last_modes = Some(seed.modes);
        let mut wake = seed.wake;
        let mut wake_generation = seed.wake_generation;
        let mut last_emission = Instant::now()
            .checked_sub(GRID_FLUSH_INTERVAL)
            .unwrap_or_else(Instant::now);
        let stop = AtomicBool::new(false);
        let mut last_owner_check = Instant::now();
        loop {
            let pending = self.flush_sinks(session_id);
            let observed_generation = wake_generation;
            // Retry only outstanding data. Once every queue is empty, retain
            // the existing quiet GridWake wait rather than polling sockets.
            let event = wake.wait_for_change(
                wake_generation,
                if pending {
                    WRITE_RETRY
                } else {
                    Duration::from_secs(1)
                },
            );
            let mut changed = event.generation != wake_generation;
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

            if changed && !interactive {
                let elapsed = last_emission.elapsed();
                if elapsed < GRID_FLUSH_INTERVAL {
                    let event = wake.wait_for_priority_or_timeout(
                        observed_generation,
                        GRID_FLUSH_INTERVAL - elapsed,
                    );
                    wake_generation = event.generation;
                }
            }
            // The session may be briefly absent mid-restart adoption: keep
            // the sinks, send nothing until it is back.
            let observed = if changed {
                let Ok(guard) = registry.lock() else { break };
                guard.get(session_id).map(|session| {
                    (
                        session.grid_update_if_changed(&mut signature),
                        session.modes(),
                        self.sink_outputs(session_id),
                    )
                })
            } else {
                None
            };

            let mut frames: Vec<Frame> = Vec::with_capacity(2);
            let mut eligible_sinks = Vec::new();
            if let Some((grid, modes, sinks)) = observed {
                eligible_sinks = sinks;
                if let Some(update) = grid
                    && let Ok(frame) = Frame::grid(&update)
                {
                    frames.push(frame);
                }
                // Fresh sinks get their initial modes at seed time; the pump
                // only broadcasts changes.
                if let Some(previous) = last_modes
                    && previous != modes
                {
                    frames.push(Frame::modes_with_bracketed_paste(modes.0, modes.1, modes.2));
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
                            if let Ok(mut output) = sink.output.lock() {
                                output.close();
                            }
                        }
                    }
                    return;
                };
                self.enqueue_publication(session_id, eligible_sinks, &encoded_frames);
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
                    output: Arc::clone(&old),
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
                output: Arc::clone(&new),
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
