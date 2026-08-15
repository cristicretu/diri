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
use crate::session::{AttachmentModes, AttachmentSeed, GridSignature, GridWake};

/// Background-output ceiling for grid emission. The first frame after quiet
/// and the bounded response frames after interactive input go immediately;
/// a continuous producer is capped at the display cadence of a 120 Hz panel.
/// This transport budget never delays the interactive leading edge.
const GRID_FLUSH_INTERVAL: Duration = Duration::from_millis(8);

/// One attached client's write half.
struct Sink {
    id: u64,
    writer: Arc<Mutex<UnixStream>>,
    /// Session/modes snapshot the client most recently confirmed applying.
    acknowledged_wake: GridWake,
    acknowledged_modes: AttachmentModes,
    pending_modes: Option<PendingModes>,
    /// Historical clients never acknowledge Modes and retain their previous
    /// best-effort behavior. Once a client proves support, every replacement
    /// is held behind the ordered acknowledgement barrier.
    modes_ack_capable: bool,
}

#[derive(Clone)]
struct PendingModes {
    token: u64,
    wake: GridWake,
    modes: AttachmentModes,
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
    next_modes_token: Arc<AtomicU64>,
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
        let modes_token = self.next_modes_token();
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
            if write_frame(
                &writer,
                &terminal_modes_frame(seed.modes).with_modes_token(modes_token),
            )
            .is_err()
            {
                return;
            }
            seed
        };

        let sink_id = self.next_sink.fetch_add(1, Ordering::SeqCst);
        self.register(
            registry,
            session_id,
            sink_id,
            Arc::clone(&writer),
            seed,
            modes_token,
        );

        // The read loop is this connection's thread. A feed error means a
        // corrupt stream; a false from handle_frame means the peer's write
        // half died — both end the whole serve.
        let mut codec = FrameCodec::new();
        let mut chunk = [0u8; 64 << 10];
        let mut pending = buffered;
        'serve: while let Ok(frames) = codec.feed(&pending) {
            pending.clear();
            for frame in frames {
                if !self.handle_frame(registry, session_id, sink_id, &writer, &frame) {
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
        sink_id: u64,
        writer: &Arc<Mutex<UnixStream>>,
        frame: &Frame,
    ) -> bool {
        let Ok(mut guard) = registry.lock() else {
            return false;
        };
        if frame.frame_type == FrameType::Modes {
            if let Some(token) = frame.modes_token_payload() {
                self.acknowledge_modes(session_id, sink_id, token);
            }
            return true;
        }
        if matches!(frame.frame_type, FrameType::Input | FrameType::Mouse) {
            let Some(session) = guard.get(session_id) else {
                return true; // session ended; swallow input quietly, as Swift does
            };
            if !self.input_modes_are_acknowledged(
                session_id,
                sink_id,
                &session.grid_wake(),
                session.modes(),
            ) {
                return true;
            }
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
            FrameType::Mouse => {
                let _ = session.write_mouse(&frame.payload);
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
        modes_token: u64,
    ) {
        let mut sessions = self.sessions.lock().expect("attach hub");
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.sinks.push(Sink {
            id: sink_id,
            writer,
            acknowledged_wake: seed.wake.clone(),
            acknowledged_modes: seed.modes,
            pending_modes: Some(PendingModes {
                token: modes_token,
                wake: seed.wake.clone(),
                modes: seed.modes,
            }),
            modes_ack_capable: false,
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

    fn next_modes_token(&self) -> u64 {
        // Zero remains a useful sentinel in traces and malformed-frame tests.
        self.next_modes_token
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    fn acknowledge_modes(&self, session_id: &str, sink_id: u64, token: u64) {
        let mut sessions = self.sessions.lock().expect("attach hub");
        let Some(sink) = sessions
            .get_mut(session_id)
            .and_then(|entry| entry.sinks.iter_mut().find(|sink| sink.id == sink_id))
        else {
            return;
        };
        // Even a superseded token proves that this peer understands the
        // acknowledgement extension. Keep the newest pending barrier armed;
        // otherwise an initial ack racing a live mode update would leave a
        // current client on the legacy best-effort path.
        sink.modes_ack_capable = true;
        let Some(pending) = sink.pending_modes.take_if(|pending| pending.token == token) else {
            return;
        };
        sink.acknowledged_wake = pending.wake;
        sink.acknowledged_modes = pending.modes;
    }

    fn input_modes_are_acknowledged(
        &self,
        session_id: &str,
        sink_id: u64,
        current_wake: &GridWake,
        current_modes: AttachmentModes,
    ) -> bool {
        let sessions = self.sessions.lock().expect("attach hub");
        let Some(sink) = sessions
            .get(session_id)
            .and_then(|entry| entry.sinks.iter().find(|sink| sink.id == sink_id))
        else {
            return false;
        };
        !sink.modes_ack_capable
            || (sink.pending_modes.is_none()
                && sink.acknowledged_wake.same_source(current_wake)
                && sink.acknowledged_modes == current_modes)
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
            let mut modes_publication = None;
            if let Some((grid, modes)) = observed {
                if let Some(update) = grid
                    && let Ok(frame) = Frame::grid(&update)
                {
                    frames.push(frame);
                }
                // A replacement deliberately clears `last_modes`: its first
                // sample must reseed this same socket even when it is unknown,
                // so clients cannot retain the previous child's DECCKM state.
                if last_modes != Some(modes) {
                    let token = self.next_modes_token();
                    frames.push(terminal_modes_frame(modes).with_modes_token(token));
                    modes_publication = Some((token, modes));
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
                    let mut sessions = self.sessions.lock().expect("attach hub");
                    let Some(entry) = sessions.get_mut(session_id) else {
                        continue;
                    };
                    if let Some((token, modes)) = modes_publication {
                        // Arm and snapshot recipients under one lock. A sink
                        // registered after this point receives its own newer
                        // seed, never this publication after that seed.
                        for sink in &mut entry.sinks {
                            sink.pending_modes = Some(PendingModes {
                                token,
                                wake: wake.clone(),
                                modes,
                            });
                        }
                    }
                    entry
                        .sinks
                        .iter()
                        .map(|sink| (sink.id, Arc::clone(&sink.writer)))
                        .collect()
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

fn terminal_modes_frame(modes: crate::session::AttachmentModes) -> Frame {
    let frame =
        Frame::modes_with_bracketed_paste(modes.alt_screen, modes.bracketed_paste, modes.mouse);
    match modes.application_cursor_keys {
        Some(value) => frame.with_application_cursor_keys(value),
        None => frame,
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

#[cfg(test)]
mod mode_tests {
    use std::collections::VecDeque;
    use std::net::Shutdown;
    use std::path::Path;

    use super::*;
    use crate::detect::ManifestEngine;
    use crate::pty::PtySpec;
    use crate::session::{AttachmentModes, SessionSpec};
    use crate::status::Authority;
    use diri_proto::terminal::MouseModes;
    use diri_proto::{
        AgentKind, DateMillis, ProjectId, Resumability, SessionId, SessionRecord, SessionStatus,
        TitleSource,
    };

    fn modes(application_cursor_keys: Option<bool>) -> AttachmentModes {
        AttachmentModes {
            alt_screen: true,
            bracketed_paste: true,
            application_cursor_keys,
            mouse: MouseModes::OFF,
        }
    }

    #[test]
    fn same_socket_replacement_reseeds_known_and_unknown_cursor_state() {
        let known = terminal_modes_frame(modes(Some(false)));
        assert_eq!(
            known.application_cursor_keys_state_payload(),
            Some(Some(false))
        );
        assert_eq!(
            known.terminal_modes_payload(),
            Some((true, true, MouseModes::OFF))
        );

        let unknown = terminal_modes_frame(modes(None));
        assert_eq!(unknown.application_cursor_keys_state_payload(), Some(None));
        assert_eq!(
            unknown.terminal_modes_payload(),
            Some((true, true, MouseModes::OFF)),
            "unknown DECCKM does not discard the replacement's paste state"
        );
    }

    struct FrameReader {
        stream: UnixStream,
        codec: FrameCodec,
        queued: VecDeque<Frame>,
    }

    impl FrameReader {
        fn new(stream: UnixStream) -> Self {
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("read timeout");
            Self {
                stream,
                codec: FrameCodec::new(),
                queued: VecDeque::new(),
            }
        }

        fn until(&mut self, what: &str, mut predicate: impl FnMut(&Frame) -> bool) -> Frame {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut bytes = [0_u8; 64 << 10];
            loop {
                if let Some(frame) = self.queued.pop_front() {
                    if predicate(&frame) {
                        return frame;
                    }
                    continue;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                match self.stream.read(&mut bytes) {
                    Ok(0) => panic!("attachment closed while waiting for {what}"),
                    Ok(count) => self
                        .queued
                        .extend(self.codec.feed(&bytes[..count]).expect("valid frame")),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => panic!("attachment read failed: {error}"),
                }
            }
        }
    }

    fn engine() -> Arc<ManifestEngine> {
        let dir = crate::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load manifests");
        Arc::new(engine)
    }

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            id: SessionId(id.into()),
            kind: AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "replacement barrier".into(),
            title_source: TitleSource::Placeholder,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Starting,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::NotResumable,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    fn spec(id: &str, logs: &Path) -> SessionSpec {
        SessionSpec {
            id: id.into(),
            pty: PtySpec::new(
                vec!["/bin/sh".into(), "-c".into(), "stty -echo; exec cat".into()],
                "/tmp",
            )
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: logs.to_path_buf(),
            holder: None,
            remote: None,
            defer_launch: false,
        }
    }

    fn write_client_frame(stream: &mut UnixStream, frame: &Frame) {
        stream
            .write_all(&FrameCodec::encode(frame).expect("encode client frame"))
            .expect("write client frame");
    }

    fn acknowledge_modes(stream: &mut UnixStream, frames: &mut FrameReader, modes: &Frame) {
        let token = modes.modes_token_payload().expect("Modes token");
        write_client_frame(stream, &Frame::modes_ack(token));
        write_client_frame(stream, &Frame::ping());
        frames.until("acknowledgement fence", |frame| {
            frame.frame_type == FrameType::Pong
        });
    }

    #[test]
    fn connected_sink_blocks_input_until_known_and_unknown_replacements_are_applied() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "replacement-input-barrier";
        let registry = Arc::new(Mutex::new(Registry::new(
            engine(),
            temp.path().join("state.json"),
        )));
        {
            let mut registry = registry.lock().expect("registry");
            registry
                .spawn(spec(id, temp.path()), record(id))
                .expect("initial session");
            registry
                .get(id)
                .expect("initial session")
                .set_application_cursor_mode_for_test(Some(true));
        }

        let hub = AttachHub::new();
        let (mut client, server) = UnixStream::pair().expect("socket pair");
        let reader = server.try_clone().expect("clone server reader");
        let writer = Arc::new(Mutex::new(server));
        let serve = {
            let hub = hub.clone();
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || hub.serve(&registry, id, reader, Vec::new(), writer))
        };
        let mut frames = FrameReader::new(client.try_clone().expect("clone client reader"));
        frames.until("initial grid", |frame| frame.frame_type == FrameType::Grid);
        let initial_modes = frames.until("initial modes", |frame| {
            frame.frame_type == FrameType::Modes
        });
        assert_eq!(
            initial_modes.application_cursor_keys_state_payload(),
            Some(Some(true))
        );
        acknowledge_modes(&mut client, &mut frames, &initial_modes);

        // Replacement one changes known true to known false. Input is sent
        // after Registry exposure but before the app acknowledges the reseed.
        {
            let mut registry = registry.lock().expect("registry");
            registry
                .respawn(spec(id, temp.path()))
                .expect("known replacement");
            registry
                .get(id)
                .expect("known replacement")
                .set_application_cursor_mode_for_test(Some(false));
        }
        write_client_frame(&mut client, &Frame::input(b"blocked-known\n".to_vec()));
        let known_modes = frames.until("known replacement modes", |frame| {
            frame.frame_type == FrameType::Modes
                && frame.application_cursor_keys_state_payload() == Some(Some(false))
        });
        acknowledge_modes(&mut client, &mut frames, &known_modes);
        write_client_frame(&mut client, &Frame::input(b"allowed-known\n".to_vec()));
        let mut blocked_seen = false;
        frames.until("known replacement input", |frame| {
            let text = frame
                .grid_payload()
                .ok()
                .flatten()
                .map(|update| {
                    update
                        .changed_rows
                        .iter()
                        .flat_map(|row| &row.cells)
                        .filter_map(|cell| char::from_u32(cell.scalar))
                        .collect::<String>()
                })
                .unwrap_or_default();
            blocked_seen |= text.contains("blocked-known");
            text.contains("allowed-known")
        });
        assert!(!blocked_seen, "pre-ack input reached the known replacement");

        // Replacement two is deliberately provenance-unknown. The same live
        // socket stays usable after the unknown snapshot is applied, but no
        // input encoded against the previous known state crosses the barrier.
        {
            let mut registry = registry.lock().expect("registry");
            registry
                .respawn(spec(id, temp.path()))
                .expect("unknown replacement");
            registry
                .get(id)
                .expect("unknown replacement")
                .set_application_cursor_mode_for_test(None);
        }
        write_client_frame(&mut client, &Frame::input(b"blocked-unknown\n".to_vec()));
        let unknown_modes = frames.until("unknown replacement modes", |frame| {
            frame.frame_type == FrameType::Modes
                && frame.application_cursor_keys_state_payload() == Some(None)
        });
        acknowledge_modes(&mut client, &mut frames, &unknown_modes);
        write_client_frame(&mut client, &Frame::input(b"allowed-unknown\n".to_vec()));
        let mut blocked_seen = false;
        frames.until("unknown replacement input", |frame| {
            let text = frame
                .grid_payload()
                .ok()
                .flatten()
                .map(|update| {
                    update
                        .changed_rows
                        .iter()
                        .flat_map(|row| &row.cells)
                        .filter_map(|cell| char::from_u32(cell.scalar))
                        .collect::<String>()
                })
                .unwrap_or_default();
            blocked_seen |= text.contains("blocked-unknown");
            text.contains("allowed-unknown")
        });
        assert!(
            !blocked_seen,
            "pre-ack input reached the unknown replacement"
        );

        client.shutdown(Shutdown::Both).expect("close client");
        serve.join().expect("serve thread");
    }
}
