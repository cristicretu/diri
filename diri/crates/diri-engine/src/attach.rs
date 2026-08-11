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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_proto::frames::{Frame, FrameCodec, FrameType};

use crate::registry::Registry;
use crate::session::{AttachmentSeed, GridSignature};

/// Background-output ceiling for grid emission, matching the client pacer and
/// the Swift daemon's flush interval. The first frame after quiet and the
/// bounded response frames after interactive input go immediately.
const GRID_FLUSH_INTERVAL: Duration = Duration::from_millis(16);

/// One attached client's write half.
struct Sink {
    id: u64,
    writer: Arc<Mutex<UnixStream>>,
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
}

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
        mut reader: impl Read,
        buffered: Vec<u8>,
        writer: Arc<Mutex<UnixStream>>,
    ) {
        // Selecting a hibernated session revives it: the seed below paints
        // instantly from the emulator, and the live program resumes
        // underneath — the Swift attach() behavior.
        {
            let Ok(mut guard) = registry.lock() else {
                return;
            };
            let _ = guard.wake_session(session_id);
        }
        // Seed before registering: the full snapshot must be the sink's first
        // frame, ahead of any diff the pump broadcasts.
        let seed = {
            let Ok(guard) = registry.lock() else { return };
            let Some(session) = guard.get(session_id) else {
                return; // unknown session: close, as the Swift daemon does
            };
            let seed = session.attachment_seed();
            let Ok(grid_frame) = Frame::grid(&seed.grid) else {
                return;
            };
            if write_frame(&writer, &grid_frame).is_err() {
                return;
            }
            let _ = write_frame(&writer, &Frame::modes(seed.modes.0, seed.modes.1));
            seed
        };

        let sink_id = self.next_sink.fetch_add(1, Ordering::SeqCst);
        self.register(registry, session_id, sink_id, Arc::clone(&writer), seed);

        // The read loop is this connection's thread. A feed error means a
        // corrupt stream; a false from handle_frame means the peer's write
        // half died — both end the whole serve.
        let mut codec = FrameCodec::new();
        let mut chunk = [0u8; 64 << 10];
        let mut pending = buffered;
        'serve: while let Ok(frames) = codec.feed(&pending) {
            pending.clear();
            for frame in frames {
                if !self.handle_frame(registry, session_id, &writer, &frame) {
                    break 'serve;
                }
            }
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => pending.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        self.deregister(session_id, sink_id);
    }

    fn handle_frame(
        &self,
        registry: &Arc<Mutex<Registry>>,
        session_id: &str,
        writer: &Arc<Mutex<UnixStream>>,
        frame: &Frame,
    ) -> bool {
        let Ok(mut guard) = registry.lock() else {
            return false;
        };
        if matches!(frame.frame_type, FrameType::Input) {
            // Input to a frozen session wakes it; write_input's queue covers
            // the race where the governor froze it mid-keystroke.
            let _ = guard.wake_session(session_id);
        }
        let Some(session) = guard.get(session_id) else {
            return true; // session ended; swallow input quietly, as Swift does
        };
        match frame.frame_type {
            FrameType::Input => {
                let _ = session.write_input(&frame.payload);
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
            FrameType::Ping => {
                drop(guard);
                return write_frame(writer, &Frame::pong()).is_ok();
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
        writer: Arc<Mutex<UnixStream>>,
        seed: AttachmentSeed,
    ) {
        let mut sessions = self.sessions.lock().expect("attach hub");
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.sinks.push(Sink {
            id: sink_id,
            writer,
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
            .is_some_and(|entry| !entry.sinks.is_empty())
    }

    fn deregister(&self, session_id: &str, sink_id: u64) {
        let mut sessions = self.sessions.lock().expect("attach hub");
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.sinks.retain(|sink| sink.id != sink_id);
        }
    }

    /// The per-session broadcast loop. Grid writers wake it on change;
    /// background bursts coalesce to 16 ms while interactive responses bypass
    /// that wait. A quiet attached terminal performs no Registry or Screen
    /// polling. Ends within one bounded wait after the last sink.
    fn pump(&self, registry: &Arc<Mutex<Registry>>, session_id: &str, seed: AttachmentSeed) {
        let mut signature = seed.signature;
        let mut last_modes = Some(seed.modes);
        let mut wake = seed.wake;
        let mut wake_generation = seed.wake_generation;
        let mut last_emission = Instant::now()
            .checked_sub(GRID_FLUSH_INTERVAL)
            .unwrap_or_else(Instant::now);
        let stop = AtomicBool::new(false);
        loop {
            let observed_generation = wake_generation;
            let event = wake.wait_for_change(wake_generation, Duration::from_secs(1));
            let mut changed = event.generation != wake_generation;
            let mut interactive = event.interactive;
            wake_generation = event.generation;

            // A restart can replace the Session (and therefore its wake
            // source) while sinks remain connected. The bounded wait above is
            // the recovery ceiling; re-seed from the replacement immediately.
            let replacement_wake = {
                let Ok(guard) = registry.lock() else { break };
                guard.get(session_id).map(|session| session.grid_wake())
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
                    )
                })
            } else {
                None
            };

            let mut frames: Vec<Frame> = Vec::with_capacity(2);
            if let Some((grid, modes)) = observed {
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
                    frames.push(Frame::modes(modes.0, modes.1));
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
                let sinks: Vec<(u64, Arc<Mutex<UnixStream>>)> = {
                    let sessions = self.sessions.lock().expect("attach hub");
                    match sessions.get(session_id) {
                        Some(entry) => entry
                            .sinks
                            .iter()
                            .map(|sink| (sink.id, Arc::clone(&sink.writer)))
                            .collect(),
                        None => Vec::new(),
                    }
                };
                for (sink_id, writer) in sinks {
                    for frame in &frames {
                        if write_frame(&writer, frame).is_err() {
                            // The peer is gone; its serve loop will also
                            // notice, but don't keep writing meanwhile.
                            self.deregister(session_id, sink_id);
                            break;
                        }
                    }
                }
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

fn write_frame(writer: &Arc<Mutex<UnixStream>>, frame: &Frame) -> std::io::Result<()> {
    let bytes =
        FrameCodec::encode(frame).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut stream = writer
        .lock()
        .map_err(|_| std::io::Error::other("writer poisoned"))?;
    stream.write_all(&bytes)?;
    stream.flush()
}
