//! One Engine-side controller for one remote Holder.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use diri_proto::frames::Frame;
use diri_proto::remote_pty::{
    Hello, HelloAck, PHASE_ONE_HOLDER_CAPABILITIES, ProtocolVersion, RemoteCodec, RemoteMessage,
    RemoteProcessState, RemoteRole, ScrollbackRequest, ScrollbackResponse, SessionInspection,
    SessionSelector, SessionToken, Signal, validate_terminal_dimensions,
};

use super::binding::RemoteBindingStore;
use super::bootstrap::RemoteTarget;

use super::manager::{InstalledHelper, RemoteManager};

const MAX_QUEUED_INPUT: usize = 1024 * 1024;
const OFFSET_PERSIST_INTERVAL: u64 = 1024 * 1024;
const REQUIRED_CAPABILITIES: &[diri_proto::remote_pty::RemoteCapability] =
    PHASE_ONE_HOLDER_CAPABILITIES;

struct PendingFrame {
    bytes: Vec<u8>,
    written: usize,
    replay_input: Option<Vec<u8>>,
    resize: Option<(u16, u16)>,
    effect: bool,
}

#[derive(Default)]
struct WriterState {
    failed: bool,
    child: Option<Child>,
    input: Option<ChildStdin>,
    generation: u64,
    controller_epoch: Option<u64>,
    control_granted: bool,
    queued_input: Vec<u8>,
    queued_resize: Option<(u16, u16)>,
    pending: VecDeque<PendingFrame>,
    pending_bytes: usize,
    uncertain_effect: bool,
    wake: Option<UnixStream>,
    wake_reader: Option<UnixStream>,
}

/// The pump owns SSH stdout. Interactive callers share this small writer
/// state; no terminal/parser lock is on the input hot path.
pub struct RemoteSessionClient {
    manager: Arc<RemoteManager>,
    helper: InstalledHelper,
    session_id: String,
    token: SessionToken,
    incarnation: String,
    checkpoint: diri_pty::checkpoint::CheckpointWriter<u64>,
    writer: Mutex<WriterState>,
    reconnect_pid: AtomicU64,
    accepted_controller_epoch: AtomicU64,
    reconnect_epoch_floor: AtomicU64,
    observed_output_offset: AtomicU64,
    scheduled_output_offset: AtomicU64,
    next_request_id: AtomicU64,
    scrollback_requests: Mutex<HashMap<u64, mpsc::Sender<diri_proto::ReadScrollbackCellsResult>>>,
}

impl RemoteSessionClient {
    pub fn new(
        manager: Arc<RemoteManager>,
        helper: InstalledHelper,
        session_id: String,
        token: SessionToken,
        incarnation: String,
        binding_store: RemoteBindingStore,
        initial_output_offset: u64,
    ) -> io::Result<Self> {
        let binding_id = session_id.clone();
        let binding_incarnation = incarnation.clone();
        let checkpoint =
            diri_pty::checkpoint::CheckpointWriter::new("remote-binding", move |offset| {
                // Recovery offsets are best effort. Keep the worker alive so
                // a transient filesystem failure is retried at the next
                // scheduled offset (or the final close), without IO retries
                // on the terminal-processing thread.
                let _ = binding_store.update_output_offset_for_incarnation(
                    &binding_id,
                    &binding_incarnation,
                    offset,
                );
                Ok(())
            })?;
        Ok(Self {
            manager,
            helper,
            session_id,
            token,
            incarnation,
            checkpoint,
            writer: Mutex::new(WriterState::default()),
            reconnect_pid: AtomicU64::new(0),
            accepted_controller_epoch: AtomicU64::new(0),
            reconnect_epoch_floor: AtomicU64::new(0),
            observed_output_offset: AtomicU64::new(initial_output_offset),
            scheduled_output_offset: AtomicU64::new(initial_output_offset),
            next_request_id: AtomicU64::new(1),
            scrollback_requests: Mutex::new(HashMap::new()),
        })
    }

    /// Rejects further transport writes until explicit identity-checked recovery.
    /// Clearing queues ensures an uncertain operation can never be replayed.
    pub(crate) fn fail_closed(&self) {
        fail_writer(&mut self.writer.lock().expect("remote writer"));
        self.scrollback_requests
            .lock()
            .expect("scrollback requests")
            .clear();
    }

    /// Explicit recovery only, after the previous pump has joined. Old input,
    /// resize, controller epochs and partial writes never cross this boundary.
    pub(crate) fn restart_failed(&self, pid: u32) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        let next_epoch = self
            .accepted_controller_epoch
            .load(Ordering::SeqCst)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("remote controller epoch exhausted"))?;
        restart_failed_writer(&mut writer)?;
        self.reconnect_epoch_floor
            .store(next_epoch, Ordering::SeqCst);
        self.reconnect_pid.store(u64::from(pid), Ordering::SeqCst);
        Ok(())
    }

    pub(crate) fn inspect_for_reconnect(&self, expected_pid: i32) -> io::Result<SessionInspection> {
        let inspection = self.manager.inspect_for_reconnect(
            &self.helper,
            &SessionSelector {
                session_id: self.session_id.clone(),
                session_token: self.token.clone(),
                expected_incarnation: Some(self.incarnation.clone()),
            },
        )?;
        if inspection.session_id != self.session_id
            || inspection.session_incarnation != self.incarnation
            || inspection.holder_build_id != self.helper.build_id
            || matches!(inspection.process_state, RemoteProcessState::Running { pid } if expected_pid > 0 && pid != expected_pid as u32)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote reconnect identity does not match the preserved session",
            ));
        }
        Ok(inspection)
    }

    pub(crate) fn enhanced_keyboard_protocol(&self) -> bool {
        self.helper.protocol.minor >= diri_proto::remote_pty::ENHANCED_KEYBOARD_PROTOCOL_MINOR
    }

    pub fn connect(
        &self,
        output_offset: u64,
        grid_sequence: Option<u64>,
    ) -> io::Result<(u64, ChildStdout)> {
        ensure_available(&self.writer.lock().expect("remote writer"))?;
        let mut channel = self.manager.attach(&self.helper)?;
        let setup = (|| {
            let mut required_capabilities = REQUIRED_CAPABILITIES.to_vec();
            if self.helper.protocol.minor
                >= diri_proto::remote_pty::ENHANCED_KEYBOARD_PROTOCOL_MINOR
            {
                required_capabilities
                    .push(diri_proto::remote_pty::RemoteCapability::EnhancedKeyboard);
            }
            let hello = RemoteMessage::Hello(Hello {
                protocol: ProtocolVersion::CURRENT,
                local_build_id: format!("engine-{}", env!("CARGO_PKG_VERSION")),
                session_id: self.session_id.clone(),
                session_token: self.token.clone(),
                expected_incarnation: Some(self.incarnation.clone()),
                requested_role: RemoteRole::Controller,
                client_nonce: random_identifier()?,
                required_capabilities,
                last_acknowledged_output_offset: Some(output_offset),
                last_acknowledged_grid_sequence: grid_sequence,
            });
            let encoded = RemoteCodec::encode(&hello).map_err(io::Error::other)?;
            channel.input.write_all(&encoded)?;
            channel.input.flush()?;

            // Only this Engine writes this pipe. Never let SSH backpressure block
            // an interactive caller or the Registry that called it.
            let fd = channel.input.as_raw_fd();
            // SAFETY: fd is the live owned SSH stdin descriptor; preserve its flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let (wake, wake_reader) = UnixStream::pair()?;
            wake.set_nonblocking(true)?;
            wake_reader.set_nonblocking(true)?;
            Ok((wake, wake_reader))
        })();
        let (wake, wake_reader) = match setup {
            Ok(pair) => pair,
            Err(error) => {
                super::executor::terminate_process_group(&mut channel.child);
                return Err(error);
            }
        };
        let mut writer = self.writer.lock().expect("remote writer");
        if let Err(error) = ensure_available(&writer) {
            super::executor::terminate_process_group(&mut channel.child);
            return Err(error);
        }
        terminate_current(&mut writer);
        writer.wake = Some(wake);
        writer.wake_reader = Some(wake_reader);
        writer.generation = writer.generation.saturating_add(1);
        writer.controller_epoch = None;
        writer.child = Some(channel.child);
        writer.input = Some(channel.input);
        Ok((writer.generation, channel.output))
    }

    pub(crate) fn take_write_wakeup(&self, generation: u64) -> io::Result<UnixStream> {
        let mut writer = self.writer.lock().expect("remote writer");
        require_generation(&writer, generation)?;
        writer
            .wake_reader
            .take()
            .ok_or_else(|| io::Error::other("SSH writer wakeup already taken"))
    }

    pub(crate) fn pending_write_fd(&self, generation: u64) -> io::Result<Option<OwnedFd>> {
        let writer = self.writer.lock().expect("remote writer");
        require_generation(&writer, generation)?;
        if writer.pending.is_empty() {
            return Ok(None);
        }
        writer
            .input
            .as_ref()
            .map(|input| input.as_fd().try_clone_to_owned())
            .transpose()
    }

    pub(crate) fn flush_pending(&self, generation: u64) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        require_generation(&writer, generation)?;
        flush_pending(&mut writer)?;
        if let Some((cols, rows)) = writer.queued_resize.take()
            && let Err(error) = write_message(
                &mut writer,
                &RemoteMessage::Terminal(Frame::resize(cols, rows)),
            )
        {
            writer.queued_resize = Some((cols, rows));
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn uncertain_effect(&self) -> bool {
        self.writer.lock().expect("remote writer").uncertain_effect
    }

    pub fn accept_hello(&self, generation: u64, epoch: u64) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        require_generation(&writer, generation)?;
        writer.controller_epoch = Some(epoch);
        self.accepted_controller_epoch
            .fetch_max(epoch, Ordering::SeqCst);
        writer.control_granted = false;
        Ok(())
    }

    pub fn validate_hello(&self, acknowledgement: &HelloAck) -> io::Result<()> {
        acknowledgement.validate().map_err(io::Error::other)?;
        if acknowledgement.protocol.major != ProtocolVersion::CURRENT.major
            || acknowledgement.holder_build_id != self.helper.build_id
            || acknowledgement.session_incarnation != self.incarnation
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote HelloAck identity, build, or protocol does not match",
            ));
        }
        if acknowledgement.controller_epoch < self.reconnect_epoch_floor.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote reconnect controller epoch did not advance",
            ));
        }
        let expected_pid = self.reconnect_pid.load(Ordering::SeqCst);
        if matches!(acknowledgement.process_state, RemoteProcessState::Running { pid } if expected_pid != 0 && u64::from(pid) != expected_pid)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote process identity changed after reconnect inspection",
            ));
        }
        if self.helper.protocol.minor >= diri_proto::remote_pty::ENHANCED_KEYBOARD_PROTOCOL_MINOR
            && (acknowledgement.protocol.minor
                < diri_proto::remote_pty::ENHANCED_KEYBOARD_PROTOCOL_MINOR
                || !acknowledgement
                    .capabilities
                    .contains(&diri_proto::remote_pty::RemoteCapability::EnhancedKeyboard)
                || !acknowledgement
                    .capabilities
                    .contains(&diri_proto::remote_pty::RemoteCapability::InputModes))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote Holder did not confirm enhanced-keyboard-v1",
            ));
        }
        if REQUIRED_CAPABILITIES
            .iter()
            .any(|required| !acknowledgement.capabilities.contains(required))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote Holder is missing a required capability",
            ));
        }
        Ok(())
    }

    pub fn grant_control(&self, generation: u64, epoch: u64) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        require_generation(&writer, generation)?;
        if writer.controller_epoch != Some(epoch) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote controller epoch does not match HelloAck",
            ));
        }
        if let Some((cols, rows)) = writer.queued_resize.take() {
            write_message(
                &mut writer,
                &RemoteMessage::Terminal(Frame::resize(cols, rows)),
            )?;
        }
        if !writer.queued_input.is_empty() {
            let bytes = std::mem::take(&mut writer.queued_input);
            // Reconnect input was accepted before this lease. If the queue
            // cannot accept it, retain it without duplicating a sent prefix.
            if let Err(error) = write_message(
                &mut writer,
                &RemoteMessage::Terminal(Frame::input(bytes.clone())),
            ) {
                if error.kind() == io::ErrorKind::WouldBlock {
                    writer.queued_input = bytes;
                }
                return Err(error);
            }
        }
        // The same writer lock keeps new input behind the reconnect batch.
        writer.control_granted = true;
        Ok(())
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_terminal(bytes, false)
    }

    pub fn write_mouse(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_terminal(bytes, true)
    }

    /// Routes wheel intent to the Holder that owns the authoritative parser.
    /// This remains safe for protocol 1.3 sessions, whose mode byte did not
    /// expose enough detail for the Engine to encode the report itself.
    pub fn scroll(&self, direction: u8, lines: u16, col: u16, row: u16) -> io::Result<()> {
        if lines == 0 {
            return Ok(());
        }
        let mut writer = self.writer.lock().expect("remote writer");
        ensure_available(&writer)?;
        if !writer.control_granted || writer.input.is_none() {
            // Wheel/motion is ephemeral. Replaying it after a reconnect would
            // target a screen that may already have changed.
            return Ok(());
        }
        if let Err(error) = write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::scroll(direction, lines, col, row)),
        ) {
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            terminate_current(&mut writer);
            writer.controller_epoch = None;
        }
        Ok(())
    }

    fn write_terminal(&self, bytes: &[u8], mouse: bool) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock().expect("remote writer");
        ensure_available(&writer)?;
        if !writer.control_granted || writer.input.is_none() {
            return queue_input(&mut writer, bytes);
        }
        if writer.uncertain_effect {
            return Err(io::Error::other("remote input delivery is uncertain"));
        }
        let frame = terminal_input_frame(self.helper.protocol, bytes, mouse);
        if let Err(error) = write_message(&mut writer, &RemoteMessage::Terminal(frame)) {
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::InvalidInput
            ) {
                return Err(error);
            }
            terminate_current(&mut writer);
            writer.controller_epoch = None;
            if writer.uncertain_effect {
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        validate_terminal_dimensions(cols, rows)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut writer = self.writer.lock().expect("remote writer");
        ensure_available(&writer)?;
        if !writer.control_granted || writer.input.is_none() {
            writer.queued_resize = Some((cols, rows));
            return Ok(());
        }
        if let Err(error) = write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::resize(cols, rows)),
        ) {
            writer.queued_resize = Some((cols, rows));
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            terminate_current(&mut writer);
            writer.controller_epoch = None;
            let _ = error;
            return Ok(());
        }
        Ok(())
    }

    pub fn signal(&self, signal: i32) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        let epoch = writer
            .controller_epoch
            .filter(|_| writer.control_granted)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "remote controller is reconnecting",
                )
            })?;
        write_message(
            &mut writer,
            &RemoteMessage::Signal(Signal {
                controller_epoch: epoch,
                signal: remote_signal(self.helper.target, signal)?,
            }),
        )
    }

    pub fn kill(&self) -> io::Result<diri_proto::remote_pty::ProcessExit> {
        let inspection = self.manager.kill(
            &self.helper,
            &SessionSelector {
                session_id: self.session_id.clone(),
                session_token: self.token.clone(),
                expected_incarnation: Some(self.incarnation.clone()),
            },
        )?;
        let RemoteProcessState::Exited { code, signal } = inspection.process_state else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stop did not observe an Agent exit",
            ));
        };
        Ok(diri_proto::remote_pty::ProcessExit { code, signal })
    }

    pub fn inspect(&self) -> io::Result<SessionInspection> {
        self.manager.inspect(
            &self.helper,
            &SessionSelector {
                session_id: self.session_id.clone(),
                session_token: self.token.clone(),
                expected_incarnation: Some(self.incarnation.clone()),
            },
        )
    }

    pub(crate) fn process_facts(
        &self,
        deadline: std::time::Instant,
    ) -> io::Result<diri_proto::process_facts::ProcessFacts> {
        self.manager.inspect_process_facts(
            &self.helper,
            &SessionSelector {
                session_id: self.session_id.clone(),
                session_token: self.token.clone(),
                expected_incarnation: Some(self.incarnation.clone()),
            },
            deadline,
        )
    }

    pub fn read_scrollback_cells(
        &self,
        first_row: i64,
        max_rows: i64,
    ) -> io::Result<diri_proto::ReadScrollbackCellsResult> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.scrollback_requests
            .lock()
            .expect("scrollback requests")
            .insert(request_id, sender);
        let request = RemoteMessage::ScrollbackRequest(ScrollbackRequest {
            request_id,
            first_row,
            max_rows,
        });
        let sent = {
            let mut writer = self.writer.lock().expect("remote writer");
            if !writer.control_granted {
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "remote controller is reconnecting",
                ))
            } else {
                write_message(&mut writer, &request)
            }
        };
        if let Err(error) = sent {
            self.scrollback_requests
                .lock()
                .expect("scrollback requests")
                .remove(&request_id);
            return Err(error);
        }
        match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => Ok(result),
            Err(_) => {
                self.scrollback_requests
                    .lock()
                    .expect("scrollback requests")
                    .remove(&request_id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote scrollback timed out",
                ))
            }
        }
    }

    pub fn complete_scrollback(&self, response: ScrollbackResponse) {
        if let Some(sender) = self
            .scrollback_requests
            .lock()
            .expect("scrollback requests")
            .remove(&response.request_id)
        {
            let _ = sender.send(response.result);
        }
    }

    pub fn observe_output_offset(&self, offset: u64) {
        let previous = self
            .observed_output_offset
            .fetch_max(offset, Ordering::AcqRel);
        let offset = offset.max(previous);
        let persisted = self.scheduled_output_offset.load(Ordering::Acquire);
        if offset.saturating_sub(persisted) < OFFSET_PERSIST_INTERVAL {
            return;
        }
        if self.checkpoint.submit(offset).is_ok() {
            self.scheduled_output_offset
                .store(offset, Ordering::Release);
        }
    }

    fn persist_observed_output_offset(&self) {
        let offset = self.observed_output_offset.load(Ordering::Acquire);
        let _ = self.checkpoint.submit(offset);
        let _ = self.checkpoint.finish();
    }

    pub fn disconnect(&self, generation: u64) {
        let mut writer = self.writer.lock().expect("remote writer");
        if writer.generation == generation {
            terminate_current(&mut writer);
            writer.controller_epoch = None;
        }
    }

    pub fn close(&self) {
        let mut writer = self.writer.lock().expect("remote writer");
        terminate_current(&mut writer);
        writer.controller_epoch = None;
        drop(writer);
        self.scrollback_requests
            .lock()
            .expect("scrollback requests")
            .clear();
        self.persist_observed_output_offset();
    }

    #[must_use]
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
}

fn write_message(writer: &mut WriterState, message: &RemoteMessage) -> io::Result<()> {
    ensure_available(writer)?;
    if writer.input.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "SSH channel is closed",
        ));
    }
    let bytes = RemoteCodec::encode(message)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if writer
        .pending_bytes
        .saturating_add(writer.queued_input.len())
        .saturating_add(bytes.len())
        > MAX_QUEUED_INPUT + 4096
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote input queue is full",
        ));
    }
    let (replay_input, resize, effect) = match message {
        RemoteMessage::Terminal(frame)
            if frame.frame_type == diri_proto::frames::FrameType::Input =>
        {
            (Some(frame.payload.clone()), None, true)
        }
        RemoteMessage::Terminal(frame)
            if frame.frame_type == diri_proto::frames::FrameType::Resize =>
        {
            (None, frame.resize_payload(), false)
        }
        RemoteMessage::Signal(_) => (None, None, true),
        _ => (None, None, false),
    };
    writer.pending_bytes += bytes.len();
    writer.pending.push_back(PendingFrame {
        bytes,
        written: 0,
        replay_input,
        resize,
        effect,
    });
    let result = flush_pending(writer);
    if !writer.pending.is_empty()
        && let Some(wake) = &mut writer.wake
    {
        // A full wake pipe already means the pump will observe this queue.
        let _ = wake.write(&[1]);
    }
    result
}

fn flush_pending(writer: &mut WriterState) -> io::Result<()> {
    let input = writer
        .input
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SSH channel is closed"))?;
    let mut budget = 64 << 10;
    while budget > 0
        && let Some(frame) = writer.pending.front_mut()
    {
        let end = frame.bytes.len().min(frame.written + budget);
        let before = frame.written;
        super::effect::write_at_most_once(input, &frame.bytes, &mut frame.written, &mut budget)?;
        if frame.written == frame.bytes.len() {
            writer.pending_bytes -= frame.bytes.len();
            writer.pending.pop_front();
        } else if frame.written == before || frame.written < end {
            break;
        }
    }
    Ok(())
}

fn terminal_input_frame(protocol: ProtocolVersion, bytes: &[u8], mouse: bool) -> Frame {
    if mouse && protocol.minor >= diri_proto::remote_pty::MOUSE_INPUT_PROTOCOL_MINOR {
        Frame::mouse(bytes.to_vec())
    } else {
        // Protocol 1.3 Holders predate the distinct frame but accept the exact
        // same bytes as ordinary raw input. Live sessions retain their
        // creation Build ID, so this compatibility path matters across an
        // Engine upgrade.
        Frame::input(bytes.to_vec())
    }
}

fn queue_input(writer: &mut WriterState, bytes: &[u8]) -> io::Result<()> {
    ensure_available(writer)?;
    if writer.uncertain_effect {
        return Err(io::Error::other("remote input delivery is uncertain"));
    }
    if writer.queued_input.len().saturating_add(bytes.len()) > MAX_QUEUED_INPUT {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote reconnect input queue is full",
        ));
    }
    writer.queued_input.extend_from_slice(bytes);
    Ok(())
}

fn require_generation(writer: &WriterState, generation: u64) -> io::Result<()> {
    if writer.generation != generation || writer.input.is_none() {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "SSH channel was superseded",
        ))
    } else {
        Ok(())
    }
}

fn remote_signal(target: RemoteTarget, signal: i32) -> io::Result<i32> {
    // Existing holders interpret signal numbers using their own operating system.
    match signal {
        libc::SIGCONT => Ok(match target {
            RemoteTarget::LinuxX86_64 | RemoteTarget::LinuxAarch64 => 18,
            RemoteTarget::MacosAarch64 => 19,
        }),
        libc::SIGSTOP => Ok(match target {
            RemoteTarget::LinuxX86_64 | RemoteTarget::LinuxAarch64 => 19,
            RemoteTarget::MacosAarch64 => 17,
        }),
        libc::SIGHUP => Ok(1),
        libc::SIGINT => Ok(2),
        libc::SIGQUIT => Ok(3),
        libc::SIGKILL => Ok(9),
        libc::SIGTERM => Ok(15),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported remote signal",
        )),
    }
}

#[derive(Debug)]
pub(crate) struct RemoteTransportFailed;
impl std::fmt::Display for RemoteTransportFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("remote_transport_failed")
    }
}
impl std::error::Error for RemoteTransportFailed {}

fn ensure_available(writer: &WriterState) -> io::Result<()> {
    if writer.failed {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            RemoteTransportFailed,
        ))
    } else {
        Ok(())
    }
}

fn restart_failed_writer(writer: &mut WriterState) -> io::Result<()> {
    if !writer.failed {
        return Err(io::Error::other("remote transport is not failed"));
    }
    let generation = writer.generation;
    *writer = WriterState {
        generation,
        ..WriterState::default()
    };
    Ok(())
}

fn fail_writer(writer: &mut WriterState) {
    writer.failed = true;
    terminate_current(writer);
    writer.queued_input.clear();
    writer.queued_resize = None;
    writer.controller_epoch = None;
}

fn terminate_current(writer: &mut WriterState) {
    writer.control_granted = false;
    let latest_resize = writer.queued_resize.take();
    for frame in writer.pending.drain(..) {
        if frame.effect && frame.written > 0 {
            writer.uncertain_effect = true;
        } else if let Some(bytes) = frame.replay_input {
            writer.queued_input.extend_from_slice(&bytes);
        } else if frame.effect {
            // Signals are never replayed, even if the local queue had not
            // written them. Surface the lost operation rather than hide it.
            writer.uncertain_effect = true;
        }
        if let Some(size) = frame.resize {
            writer.queued_resize = Some(size);
        }
    }
    writer.queued_resize = latest_resize.or(writer.queued_resize);
    writer.pending_bytes = 0;
    writer.wake.take();
    writer.wake_reader.take();
    writer.input.take();
    if let Some(mut child) = writer.child.take() {
        super::executor::terminate_process_group(&mut child);
    }
}

fn random_identifier() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("secure random source failed: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Drop for RemoteSessionClient {
    fn drop(&mut self) {
        if let Ok(writer) = self.writer.get_mut() {
            terminate_current(writer);
        }
        self.persist_observed_output_offset();
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::frames::FrameType;

    use super::*;

    #[test]
    fn enhanced_keyboard_ack_must_match_the_installed_protocol_contract() {
        use crate::remote::{
            executor::ProcessExecutor, manager::ArtifactCatalog, ssh::SshTransport,
        };
        use diri_proto::remote_pty::RemoteCapability;
        let temp = tempfile::tempdir().unwrap();
        let host = diri_proto::HostEntry {
            id: "fixture".into(),
            name: None,
            ssh: "fixture".into(),
            default_cwd: None,
            node: None,
        };
        let manager = Arc::new(
            RemoteManager::new(
                ProcessExecutor::new("/bin/false"),
                ArtifactCatalog::without_artifacts_for_test(),
                temp.path().join("control"),
            )
            .unwrap(),
        );
        for minor in [8, 13, 14] {
            let helper = InstalledHelper {
                target: RemoteTarget::MacosAarch64,
                build_id: "fixture".into(),
                protocol: ProtocolVersion { major: 1, minor },
                transport: SshTransport::new(&host, temp.path().join("control/socket")),
            };
            let client = RemoteSessionClient::new(
                Arc::clone(&manager),
                helper,
                "fixture".into(),
                SessionToken::new("fixture-token-long-enough").unwrap(),
                "incarnation".into(),
                RemoteBindingStore::new(temp.path().join(format!("binding-{minor}"))).unwrap(),
                0,
            )
            .unwrap();
            let mut ack = HelloAck {
                protocol: ProtocolVersion { major: 1, minor },
                holder_build_id: "fixture".into(),
                session_incarnation: "incarnation".into(),
                capabilities: REQUIRED_CAPABILITIES.to_vec(),
                controller_epoch: 1,
                process_state: RemoteProcessState::Running { pid: 123 },
                child_identity: None,
                output_offset: 0,
                snapshot_sequence: 1,
                foreground_pid: None,
            };
            assert_eq!(client.validate_hello(&ack).is_ok(), minor < 14);
            if minor == 14 {
                ack.capabilities.push(RemoteCapability::EnhancedKeyboard);
                assert!(client.validate_hello(&ack).is_err());
                ack.capabilities.push(RemoteCapability::InputModes);
                assert!(client.validate_hello(&ack).is_ok());
                ack.protocol.minor = 13;
                assert!(client.validate_hello(&ack).is_err());
                ack.protocol.minor = 14;
                ack.holder_build_id = "wrong-build".into();
                assert!(client.validate_hello(&ack).is_err());
            }
        }
    }

    #[test]
    fn unsupported_signals_are_rejected_before_delivery() {
        for target in RemoteTarget::ALL {
            for signal in [0, -1, libc::SIGUSR1, libc::SIGUSR2, libc::SIGCHLD] {
                assert_eq!(
                    remote_signal(target, signal).unwrap_err().kind(),
                    io::ErrorKind::InvalidInput
                );
            }
        }
    }

    #[test]
    fn signals_use_the_receiving_operating_system_numbers() {
        for (target, stop, resume) in [
            (RemoteTarget::LinuxX86_64, 19, 18),
            (RemoteTarget::LinuxAarch64, 19, 18),
            (RemoteTarget::MacosAarch64, 17, 19),
        ] {
            for (native, expected) in [
                (libc::SIGCONT, resume),
                (libc::SIGSTOP, stop),
                (libc::SIGINT, 2),
                (libc::SIGTERM, 15),
                (libc::SIGKILL, 9),
                (libc::SIGHUP, 1),
                (libc::SIGQUIT, 3),
            ] {
                assert_eq!(
                    remote_signal(target, native).unwrap(),
                    expected,
                    "native signal {native} for {target:?}"
                );
            }
        }
    }

    #[test]
    fn mouse_input_falls_back_for_live_protocol_1_3_holders() {
        let old = ProtocolVersion { major: 1, minor: 3 };
        assert_eq!(
            terminal_input_frame(old, b"mouse", true).frame_type,
            FrameType::Input
        );
        assert_eq!(
            terminal_input_frame(ProtocolVersion::CURRENT, b"mouse", true).frame_type,
            FrameType::Mouse
        );
        assert_eq!(
            terminal_input_frame(ProtocolVersion::CURRENT, b"key", false).frame_type,
            FrameType::Input
        );
    }
    fn pipe_writer() -> (WriterState, UnixStream) {
        let (input, output) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        output.set_nonblocking(true).unwrap();
        let size: libc::c_int = 4096;
        // SAFETY: input owns a live socket and size points to a valid integer.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    input.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as _,
                )
            },
            0
        );
        (
            WriterState {
                input: Some(ChildStdin::from(OwnedFd::from(input))),
                generation: 7,
                controller_epoch: Some(3),
                control_granted: true,
                ..WriterState::default()
            },
            output,
        )
    }

    #[test]
    fn fatal_transport_discards_queued_effects_and_rejects_future_writes() {
        let (mut writer, _peer) = pipe_writer();
        writer.queued_input.extend_from_slice(b"uncertain input");
        writer.queued_resize = Some((132, 42));
        writer.uncertain_effect = true;
        fail_writer(&mut writer);
        assert!(writer.failed);
        assert!(writer.uncertain_effect);
        assert!(writer.queued_input.is_empty());
        assert!(writer.pending.is_empty());
        assert!(writer.queued_resize.is_none());
        assert!(writer.input.is_none());
        assert!(queue_input(&mut writer, b"new input").is_err());
        assert!(
            write_message(&mut writer, &RemoteMessage::Terminal(Frame::resize(80, 24))).is_err()
        );
        assert_eq!(
            ensure_available(&writer).unwrap_err().to_string(),
            "remote_transport_failed"
        );
    }

    #[test]
    fn nonblocking_frames_resume_in_order_and_reject_overflow_atomically() {
        use std::io::Read;
        let (mut writer, mut output) = pipe_writer();
        let messages = [
            RemoteMessage::Terminal(Frame::input(vec![b'x'; 128 * 1024])),
            RemoteMessage::Terminal(Frame::input(b"tail".to_vec())),
        ];
        for message in &messages {
            write_message(&mut writer, message).unwrap();
        }
        assert!(!writer.pending.is_empty());
        let before = writer.pending_bytes;
        let full = RemoteMessage::Terminal(Frame::input(vec![b'y'; MAX_QUEUED_INPUT]));
        assert_eq!(
            write_message(&mut writer, &full).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(writer.pending_bytes, before);
        assert!(require_generation(&writer, 6).is_err());
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 64 * 1024];
            while let Ok(count) = output.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            if writer.pending.is_empty() {
                break;
            }
            flush_pending(&mut writer).unwrap();
        }
        assert_eq!(RemoteCodec::new().feed(&bytes).unwrap(), messages);
        assert_eq!(writer.pending_bytes, 0);
    }

    #[test]
    fn disconnect_never_replays_a_partially_written_effect() {
        let (mut writer, _output) = pipe_writer();
        write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::input(vec![b'x'; 128 * 1024])),
        )
        .unwrap();
        assert!(writer.pending.front().unwrap().written > 0);
        write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::input(b"later".to_vec())),
        )
        .unwrap();
        terminate_current(&mut writer);
        assert!(writer.uncertain_effect);
        assert_eq!(writer.queued_input, b"later");
        assert!(queue_input(&mut writer, b"retry").is_err());
    }

    #[test]
    fn disconnect_retains_only_wholly_unwritten_input() {
        let (mut writer, _output) = pipe_writer();
        // A large ephemeral mouse report occupies the transport first.
        write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::mouse(vec![b'm'; 128 * 1024])),
        )
        .unwrap();
        write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::input(b"safe".to_vec())),
        )
        .unwrap();
        assert_eq!(writer.pending.back().unwrap().written, 0);
        terminate_current(&mut writer);
        assert!(!writer.uncertain_effect);
        assert_eq!(writer.queued_input, b"safe");
    }
    #[test]
    fn explicit_restart_discards_uncertain_effects_and_revokes_the_old_writer() {
        let (mut writer, _output) = pipe_writer();
        let generation = writer.generation;
        write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::input(vec![b'x'; 128 * 1024])),
        )
        .unwrap();
        assert!(writer.pending.front().unwrap().written > 0);
        writer
            .queued_input
            .extend_from_slice(b"never replay later input");
        writer.queued_resize = Some((132, 42));
        fail_writer(&mut writer);
        assert!(writer.uncertain_effect);
        assert!(queue_input(&mut writer, b"rejected until recovery").is_err());
        restart_failed_writer(&mut writer).unwrap();
        assert!(writer.pending.is_empty());
        assert!(writer.queued_input.is_empty());
        assert!(writer.queued_resize.is_none());
        assert_eq!(writer.pending_bytes, 0);
        assert!(!writer.uncertain_effect);
        assert!(!writer.control_granted);
        assert!(require_generation(&writer, generation).is_err());
        assert!(
            restart_failed_writer(&mut writer).is_err(),
            "live transport cannot be reset"
        );
    }
}
