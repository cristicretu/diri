//! Receive-only local preview multiplexing. Session publishers remain the sole
//! diff owners; this module owns only membership and one bounded socket writer.
use crate::attach::AttachHub;
use crate::registry::Registry;
use diri_proto::frames::MAX_FRAME_BYTES;
use diri_proto::preview_set::*;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// Experimental until the same capacity matrix validates the shared path.
const BACKLOG_BYTES: usize = 8 * 1024 * 1024;
const BACKLOG_REFS: usize = 512;
const CONTROL_BYTES: usize = MAX_PREVIEW_SET_MEMBERS * MAX_PREVIEW_SET_HEADER_BYTES;
const CONTROL_REFS: usize = MAX_PREVIEW_SET_MEMBERS + 1;
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);
const STALL_TIMEOUT: Duration = Duration::from_secs(2);

struct Packet {
    header: Arc<[u8]>,
    frame: Option<Arc<[u8]>>,
}
impl Packet {
    fn bytes(&self) -> usize {
        self.header.len() + self.frame.as_ref().map_or(0, |frame| frame.len())
    }
    fn refs(&self) -> usize {
        1 + usize::from(self.frame.is_some())
    }
    fn control(&self) -> bool {
        self.frame.is_none()
    }
}
#[derive(Default)]
struct QueueState {
    packets: VecDeque<Packet>,
    normal_bytes: usize,
    normal_refs: usize,
    control_bytes: usize,
    control_refs: usize,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Needed {
    bytes: usize,
    refs: usize,
}
#[derive(Debug)]
pub(crate) enum QueueError {
    Closed,
    Full(Needed),
    Invalid,
}

pub(crate) struct MuxQueue {
    state: Mutex<QueueState>,
    changed: Condvar,
    closed: AtomicBool,
    cancel: UnixStream,
}
impl MuxQueue {
    fn new(stream: &UnixStream) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            state: Mutex::new(QueueState::default()),
            changed: Condvar::new(),
            closed: AtomicBool::new(false),
            cancel: stream.try_clone()?,
        }))
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    fn fits(state: &QueueState, needed: Needed) -> bool {
        let normal = state.normal_bytes.saturating_add(needed.bytes) <= BACKLOG_BYTES;
        // One valid full-grid/modes seed may exceed the ordinary budget. Its
        // allocation stays counted while partially written, exactly like the
        // existing single-session channel; no second oversized batch fits.
        let large_seed = state.normal_bytes == 0
            && needed.refs <= 4
            && needed.bytes <= MAX_FRAME_BYTES + 2 * MAX_PREVIEW_SET_HEADER_BYTES + 128;
        (normal || large_seed) && state.normal_refs.saturating_add(needed.refs) <= BACKLOG_REFS
    }
    fn enqueue(&self, packets: Vec<Packet>) -> Result<(), QueueError> {
        let needed = Needed {
            bytes: packets.iter().map(Packet::bytes).sum(),
            refs: packets.iter().map(Packet::refs).sum(),
        };
        let mut state = self.state.lock().map_err(|_| QueueError::Closed)?;
        if self.is_closed() {
            return Err(QueueError::Closed);
        }
        if !Self::fits(&state, needed) {
            return Err(QueueError::Full(needed));
        }
        state.normal_bytes += needed.bytes;
        state.normal_refs += needed.refs;
        state.packets.extend(packets);
        self.changed.notify_one();
        Ok(())
    }
    fn control(&self, bytes: Vec<u8>) -> bool {
        let accepted = {
            let Ok(mut state) = self.state.lock() else {
                return false;
            };
            if self.is_closed()
                || state.control_refs >= CONTROL_REFS
                || state.control_bytes.saturating_add(bytes.len()) > CONTROL_BYTES
            {
                false
            } else {
                state.control_bytes += bytes.len();
                state.control_refs += 1;
                state.packets.push_back(Packet {
                    header: Arc::from(bytes),
                    frame: None,
                });
                self.changed.notify_one();
                true
            }
        };
        if !accepted {
            self.close();
        }
        accepted
    }
    pub(crate) fn wait_capacity(&self, needed: Needed, deadline: Instant) -> bool {
        let mut state = self.state.lock().unwrap();
        while !self.is_closed() && !Self::fits(&state, needed) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
        !self.is_closed()
    }
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            {
                let mut state = self.state.lock().unwrap();
                *state = QueueState::default();
            }
            self.changed.notify_all();
            let _ = self.cancel.shutdown(std::net::Shutdown::Both);
        }
    }
    fn next(&self) -> Option<Packet> {
        let mut state = self.state.lock().unwrap();
        while state.packets.is_empty() && !self.is_closed() {
            state = self.changed.wait(state).unwrap();
        }
        if self.is_closed() {
            None
        } else {
            state.packets.pop_front()
        }
    }
    fn released(&self, packet: &Packet) {
        let mut state = self.state.lock().unwrap();
        if packet.control() {
            state.control_bytes = state.control_bytes.saturating_sub(packet.bytes());
            state.control_refs = state.control_refs.saturating_sub(packet.refs());
        } else {
            state.normal_bytes = state.normal_bytes.saturating_sub(packet.bytes());
            state.normal_refs = state.normal_refs.saturating_sub(packet.refs());
        }
        self.changed.notify_all();
    }
    fn write_loop(&self, mut stream: UnixStream) {
        while let Some(packet) = self.next() {
            let mut last_progress = Instant::now();
            for bytes in std::iter::once(packet.header.as_ref()).chain(packet.frame.as_deref()) {
                let mut offset = 0;
                while offset < bytes.len() && !self.is_closed() {
                    match stream.write(&bytes[offset..]) {
                        Ok(0) => {
                            self.close();
                            break;
                        }
                        Ok(count) => {
                            offset += count;
                            last_progress = Instant::now();
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if last_progress.elapsed() >= STALL_TIMEOUT {
                                self.close();
                                break;
                            }
                            poll_socket(&stream, libc::POLLOUT, 1);
                        }
                        Err(_) => {
                            self.close();
                            break;
                        }
                    }
                }
                if self.is_closed() {
                    break;
                }
            }
            self.released(&packet);
        }
    }
}

/// A publisher receives only enqueue/deactivate capability, never socket I/O.
#[derive(Clone)]
pub(crate) struct MuxSink {
    pub(crate) member: PreviewMember,
    queue: Arc<MuxQueue>,
    active: Arc<AtomicBool>,
}
impl MuxSink {
    fn new(member: PreviewMember, queue: Arc<MuxQueue>) -> Self {
        Self {
            member,
            queue,
            active: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(crate) fn seed(&self, frames: &[Arc<[u8]>]) -> Result<(), QueueError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(QueueError::Closed);
        }
        let packets = frames
            .iter()
            .map(|frame| {
                let header = PreviewSetHeader::Chunk {
                    member: self.member.clone(),
                    frame_bytes: frame.len(),
                }
                .encode()
                .map_err(|_| QueueError::Invalid)?;
                Ok(Packet {
                    header: Arc::from(header),
                    frame: Some(Arc::clone(frame)),
                })
            })
            .collect::<Result<Vec<_>, QueueError>>()?;
        self.queue.enqueue(packets)
    }
    pub(crate) fn publish(&self, frames: &[Arc<[u8]>]) -> bool {
        match self.seed(frames) {
            Ok(()) => true,
            Err(QueueError::Closed) => false,
            Err(_) => {
                self.queue.close();
                false
            }
        }
    }
    pub(crate) fn unavailable(&self, reason: PreviewUnavailable) {
        if self.active.swap(false, Ordering::AcqRel)
            && let Ok(header) = (PreviewSetHeader::Unavailable {
                member: self.member.clone(),
                reason,
            })
            .encode()
        {
            self.queue.control(header);
        }
    }
    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
    pub(crate) fn is_closed(&self) -> bool {
        !self.active.load(Ordering::Acquire) || self.queue.is_closed()
    }
}

pub(crate) enum Admission {
    Added(u64),
    Missing,
    Limit,
    Retry(Needed),
    Unavailable,
}

pub(crate) fn serve(
    hub: &AttachHub,
    registry: &Arc<Mutex<Registry>>,
    mut reader: UnixStream,
    buffered: Vec<u8>,
) -> io::Result<()> {
    reader.set_nonblocking(true)?;
    let queue = MuxQueue::new(&reader)?;
    let writer = reader.try_clone()?;
    let writing = Arc::clone(&queue);
    let worker = std::thread::Builder::new()
        .name("diri-preview-writer".into())
        .spawn(move || writing.write_loop(writer))?;
    let mut ready = serde_json::to_vec(&PreviewSetReady {
        version: PREVIEW_SET_VERSION,
    })
    .map_err(io::Error::other)?;
    ready.push(b'\n');
    queue.control(ready);
    let mut subscriptions: HashMap<String, (MuxSink, u64)> = HashMap::new();
    let mut pending = buffered;
    let mut bytes = [0; 64 * 1024];
    let result = (|| -> io::Result<()> {
        while !queue.is_closed() {
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                if newline > MAX_PREVIEW_SET_REQUEST_BYTES {
                    return Err(invalid("oversized preview membership"));
                }
                let desired: PreviewSetMembership =
                    serde_json::from_slice(&pending[..newline]).map_err(invalid)?;
                pending.drain(..=newline);
                desired.validate()?;
                let removed: Vec<_> = subscriptions
                    .iter()
                    .filter(|(_, (sink, _))| !desired.members.contains(&sink.member))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in removed {
                    if let Some((sink, sink_id)) = subscriptions.remove(&id) {
                        sink.deactivate();
                        hub.remove_mux(&id, sink_id);
                    }
                }
                let deadline = Instant::now() + ADMISSION_TIMEOUT;
                for member in desired.members {
                    if subscriptions.contains_key(&member.session_id.0) {
                        continue;
                    }
                    let sink = MuxSink::new(member.clone(), Arc::clone(&queue));
                    let failure = loop {
                        if queue.is_closed() {
                            return Ok(());
                        }
                        if Instant::now() >= deadline {
                            break Some(PreviewUnavailable::AdmissionTimeout);
                        }
                        match hub.add_mux(registry, sink.clone()) {
                            Admission::Added(id) => {
                                subscriptions
                                    .insert(member.session_id.0.clone(), (sink.clone(), id));
                                break None;
                            }
                            Admission::Missing => break Some(PreviewUnavailable::Missing),
                            Admission::Limit => break Some(PreviewUnavailable::AdmissionLimit),
                            Admission::Unavailable => {
                                break Some(PreviewUnavailable::PublisherUnavailable);
                            }
                            Admission::Retry(needed) => {
                                // No Registry lock or captured seed survives this
                                // wait. Retrying captures a fresh seed atomically
                                // with recipient registration.
                                if !queue.wait_capacity(needed, deadline) {
                                    break Some(PreviewUnavailable::AdmissionTimeout);
                                }
                            }
                        }
                    };
                    if let Some(reason) = failure {
                        let header = PreviewSetHeader::Unavailable { member, reason }.encode()?;
                        if !queue.control(header) {
                            return Ok(());
                        }
                    }
                }
            }
            if pending.len() > MAX_PREVIEW_SET_REQUEST_BYTES {
                return Err(invalid("oversized preview membership"));
            }
            match reader.read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(count) => pending.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    poll_socket(&reader, libc::POLLIN, -1);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })();
    for (id, (sink, sink_id)) in subscriptions {
        sink.deactivate();
        hub.remove_mux(&id, sink_id);
    }
    queue.close();
    let _ = worker.join();
    result
}

fn poll_socket(stream: &UnixStream, events: libc::c_short, timeout: i32) {
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: one live socket and one exclusively owned initialized pollfd.
    unsafe {
        libc::poll(&mut descriptor, 1, timeout);
    }
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::frames::{Frame, FrameCodec};
    use diri_proto::{SessionId, terminal::MouseModes};

    fn fixture() -> (Arc<MuxQueue>, UnixStream, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        (MuxQueue::new(&server).unwrap(), server, client)
    }
    fn packet(bytes: usize) -> Packet {
        Packet {
            header: Arc::from([]),
            frame: Some(vec![0; bytes].into()),
        }
    }

    #[test]
    fn an_inflight_allocation_counts_until_the_complete_packet_is_released() {
        let (queue, _server, _client) = fixture();
        queue.enqueue(vec![packet(BACKLOG_BYTES)]).unwrap();
        let active = queue.next().unwrap();
        assert!(queue.state.lock().unwrap().packets.is_empty());
        assert!(matches!(
            queue.enqueue(vec![packet(1)]),
            Err(QueueError::Full(_))
        ));
        queue.released(&active);
        queue.enqueue(vec![packet(1)]).unwrap();
        queue.close();
    }

    #[test]
    fn closing_cancels_admission_without_waiting_for_its_deadline() {
        let (queue, _server, _client) = fixture();
        queue.enqueue(vec![packet(BACKLOG_BYTES)]).unwrap();
        let waiting = Arc::clone(&queue);
        let (started, ready) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            waiting.wait_capacity(
                Needed { bytes: 1, refs: 1 },
                Instant::now() + Duration::from_secs(60),
            )
        });
        ready.recv().unwrap();
        queue.close();
        assert!(!worker.join().unwrap());
    }

    #[test]
    fn writer_preserves_envelopes_and_partial_frames_under_backpressure() {
        let (queue, server, mut client) = fixture();
        // Larger than the default Unix socket send buffer on supported hosts.
        let member = PreviewMember {
            session_id: SessionId::new("fixture"),
            generation: 3,
        };
        let mut expected = Vec::new();
        let mut frames = Vec::new();
        for index in 0..200 {
            let frame: Arc<[u8]> =
                FrameCodec::encode(&Frame::modes(index % 2 == 0, MouseModes::OFF))
                    .unwrap()
                    .into();
            expected.extend(
                PreviewSetHeader::Chunk {
                    member: member.clone(),
                    frame_bytes: frame.len(),
                }
                .encode()
                .unwrap(),
            );
            expected.extend_from_slice(&frame);
            frames.push(frame);
        }
        MuxSink::new(member, Arc::clone(&queue))
            .seed(&frames)
            .unwrap();
        let writing = Arc::clone(&queue);
        let worker = std::thread::spawn(move || writing.write_loop(server));
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut received = vec![0; expected.len()];
        // Deliberately small reads exercise retained offsets across packets.
        for chunk in received.chunks_mut(37) {
            client.read_exact(chunk).unwrap();
        }
        assert_eq!(received, expected);
        let packets = PreviewSetDecoder::default().feed(&received).unwrap();
        assert_eq!(packets.len(), 200);
        queue.close();
        worker.join().unwrap();
    }

    #[test]
    fn overflow_closes_the_stream_instead_of_continuing_after_a_partial_frame() {
        let (queue, server, mut client) = fixture();
        queue.enqueue(vec![packet(BACKLOG_BYTES)]).unwrap();
        let writing = Arc::clone(&queue);
        let worker = std::thread::spawn(move || writing.write_loop(server));
        let member = PreviewMember {
            session_id: SessionId::new("fixture"),
            generation: 1,
        };
        let sink = MuxSink::new(member, Arc::clone(&queue));
        let frame = FrameCodec::encode(&Frame::modes(false, MouseModes::OFF))
            .unwrap()
            .into();
        assert!(!sink.publish(&[frame]));
        assert!(queue.is_closed());
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        assert!(received.len() < BACKLOG_BYTES);
        assert!(received.iter().all(|byte| *byte == 0));
        worker.join().unwrap();
    }
    #[test]
    fn capacity_retry_recaptures_the_seed_without_holding_registry() {
        let temp = tempfile::tempdir().unwrap();
        let (engine, _) =
            crate::ManifestEngine::load_dir(&crate::detect::bundled_manifest_dir()).unwrap();
        let engine = Arc::new(engine);
        let record = serde_json::from_value(serde_json::json!({
            "id":"fixture", "kind":diri_proto::AgentKind::SHELL,"cwd":temp.path(),
            "projectID":"fixture","title":"fixture","titleSource":diri_proto::TitleSource::Placeholder,
            "status":diri_proto::SessionStatus::Idle,"resumability":diri_proto::Resumability::Live,
            "createdAt":0,"updatedAt":0,"pinned":false
        })).unwrap();
        let mut registry = Registry::new(engine, temp.path().join("state.json"));
        registry.spawn(crate::session::SessionSpec {
            id: "fixture".into(),
            pty: crate::PtySpec::new(vec!["/bin/sh".into(), "-c".into(),
                "printf old; while [ ! -f ready ]; do sleep 0.01; done; printf '\\rnew-output'; read line".into()], temp.path()).size(80,24),
            manifest_id: "shell".into(), authority: crate::Authority::ProcessOnly,
            logs_dir: temp.path().join("logs"), holder: None, remote: None, defer_launch: false,
        }, record).unwrap();
        let registry = Arc::new(Mutex::new(registry));
        struct Cleanup(Arc<Mutex<Registry>>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.lock().unwrap().terminate("fixture", Duration::ZERO);
            }
        }
        let _cleanup = Cleanup(Arc::clone(&registry));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !registry
            .lock()
            .unwrap()
            .get("fixture")
            .unwrap()
            .screen_lines()
            .join("")
            .contains("old")
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let hub = AttachHub::new();
        let (queue, _server, _client) = fixture();
        queue.enqueue(vec![packet(BACKLOG_BYTES)]).unwrap();
        let sink = MuxSink::new(
            PreviewMember {
                session_id: SessionId::new("fixture"),
                generation: 1,
            },
            Arc::clone(&queue),
        );
        assert!(matches!(
            hub.add_mux(&registry, sink.clone()),
            Admission::Retry(_)
        ));
        assert!(
            registry.try_lock().is_ok(),
            "capacity wait must release Registry"
        );
        std::fs::write(temp.path().join("ready"), "").unwrap();
        while !registry
            .lock()
            .unwrap()
            .get("fixture")
            .unwrap()
            .screen_lines()
            .join("")
            .contains("new-output")
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let filler = queue.next().unwrap();
        queue.released(&filler);
        let Admission::Added(id) = hub.add_mux(&registry, sink.clone()) else {
            panic!("fresh admission");
        };
        let seed = queue.next().unwrap();
        let mut decoder = FrameCodec::new();
        let frame = decoder
            .feed(seed.frame.as_deref().unwrap())
            .unwrap()
            .pop()
            .unwrap();
        let grid = frame.grid_payload().unwrap().unwrap();
        assert!(grid.is_full_snapshot);
        let text: String = grid
            .changed_rows
            .iter()
            .flat_map(|row| &row.cells)
            .filter_map(|cell| char::from_u32(cell.scalar))
            .collect();
        assert!(
            text.contains("new-output"),
            "retry sent an obsolete captured seed"
        );
        sink.deactivate();
        hub.remove_mux("fixture", id);
        queue.close();
    }
    #[test]
    fn a_failed_source_reports_unavailable_without_closing_other_members() {
        let (queue, _server, _client) = fixture();
        let failed = MuxSink::new(
            PreviewMember {
                session_id: SessionId::new("failed"),
                generation: 1,
            },
            Arc::clone(&queue),
        );
        let healthy = MuxSink::new(
            PreviewMember {
                session_id: SessionId::new("healthy"),
                generation: 2,
            },
            Arc::clone(&queue),
        );
        failed.unavailable(PreviewUnavailable::PublisherUnavailable);
        let packet = queue.next().unwrap();
        let decoded = PreviewSetDecoder::default().feed(&packet.header).unwrap();
        assert_eq!(
            decoded,
            vec![PreviewSetPacket::Unavailable {
                member: failed.member.clone(),
                reason: PreviewUnavailable::PublisherUnavailable
            }]
        );
        assert!(!queue.is_closed());
        let frame = FrameCodec::encode(&Frame::modes(false, MouseModes::OFF))
            .unwrap()
            .into();
        assert!(healthy.publish(&[frame]));
        queue.close();
    }
    #[test]
    fn a_maximum_frame_seed_counts_envelopes_and_excludes_a_second_allocation() {
        let (queue, _server, _client) = fixture();
        let member = PreviewMember {
            session_id: SessionId::new("x".repeat(MAX_PREVIEW_SESSION_ID_BYTES)),
            generation: u64::MAX,
        };
        let sink = MuxSink::new(member, Arc::clone(&queue));
        // Framing boundary only: terminal payload validation belongs to GridCodec.
        let grid = Frame::new(
            diri_proto::frames::FrameType::Grid,
            vec![0; MAX_FRAME_BYTES],
        );
        let grid: Arc<[u8]> = FrameCodec::encode(&grid).unwrap().into();
        let modes: Arc<[u8]> = FrameCodec::encode(&Frame::modes(false, MouseModes::OFF))
            .unwrap()
            .into();
        sink.seed(&[Arc::clone(&grid), Arc::clone(&modes)]).unwrap();
        let state = queue.state.lock().unwrap();
        assert_eq!(
            state.normal_bytes,
            state.packets.iter().map(Packet::bytes).sum::<usize>()
        );
        assert!(
            state.normal_bytes > grid.len() + modes.len(),
            "envelope bytes must count"
        );
        drop(state);
        assert!(matches!(sink.seed(&[modes]), Err(QueueError::Full(_))));
        queue.close();
    }
}
