//! One Engine-side controller for one remote Holder.

use std::collections::HashMap;
use std::io::{self, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use diri_proto::frames::Frame;
use diri_proto::remote_pty::{
    Hello, HelloAck, PHASE_ONE_HOLDER_CAPABILITIES, ProtocolVersion, RemoteCodec, RemoteMessage,
    RemoteRole, ScrollbackRequest, ScrollbackResponse, SessionInspection, SessionSelector,
    SessionToken, Signal, validate_terminal_dimensions,
};

use super::binding::RemoteBindingStore;
use super::manager::{InstalledHelper, RemoteManager};

const MAX_QUEUED_INPUT: usize = 1024 * 1024;
const OFFSET_PERSIST_INTERVAL: u64 = 1024 * 1024;
const REQUIRED_CAPABILITIES: &[diri_proto::remote_pty::RemoteCapability] =
    PHASE_ONE_HOLDER_CAPABILITIES;

struct WriterState {
    child: Option<Child>,
    input: Option<ChildStdin>,
    generation: u64,
    controller_epoch: Option<u64>,
    queued_input: Vec<u8>,
    queued_resize: Option<(u16, u16)>,
}

/// The pump owns SSH stdout. Interactive callers share this small writer
/// state; no terminal/parser lock is on the input hot path.
pub struct RemoteSessionClient {
    manager: Arc<RemoteManager>,
    helper: InstalledHelper,
    session_id: String,
    token: SessionToken,
    incarnation: String,
    binding_store: RemoteBindingStore,
    writer: Mutex<WriterState>,
    observed_output_offset: AtomicU64,
    persisted_output_offset: AtomicU64,
    next_request_id: AtomicU64,
    scrollback_requests: Mutex<HashMap<u64, mpsc::Sender<diri_proto::ReadScrollbackCellsResult>>>,
}

impl RemoteSessionClient {
    #[must_use]
    pub fn new(
        manager: Arc<RemoteManager>,
        helper: InstalledHelper,
        session_id: String,
        token: SessionToken,
        incarnation: String,
        binding_store: RemoteBindingStore,
        initial_output_offset: u64,
    ) -> Self {
        Self {
            manager,
            helper,
            session_id,
            token,
            incarnation,
            binding_store,
            writer: Mutex::new(WriterState {
                child: None,
                input: None,
                generation: 0,
                controller_epoch: None,
                queued_input: Vec::new(),
                queued_resize: None,
            }),
            observed_output_offset: AtomicU64::new(initial_output_offset),
            persisted_output_offset: AtomicU64::new(initial_output_offset),
            next_request_id: AtomicU64::new(1),
            scrollback_requests: Mutex::new(HashMap::new()),
        }
    }

    pub fn connect(
        &self,
        output_offset: u64,
        grid_sequence: Option<u64>,
    ) -> io::Result<(u64, ChildStdout)> {
        let mut channel = self.manager.attach(&self.helper)?;
        let hello = RemoteMessage::Hello(Hello {
            protocol: ProtocolVersion::CURRENT,
            local_build_id: format!("engine-{}", env!("CARGO_PKG_VERSION")),
            session_id: self.session_id.clone(),
            session_token: self.token.clone(),
            expected_incarnation: Some(self.incarnation.clone()),
            requested_role: RemoteRole::Controller,
            client_nonce: random_identifier()?,
            required_capabilities: REQUIRED_CAPABILITIES.to_vec(),
            last_acknowledged_output_offset: Some(output_offset),
            last_acknowledged_grid_sequence: grid_sequence,
        });
        let encoded = RemoteCodec::encode(&hello).map_err(io::Error::other)?;
        channel.input.write_all(&encoded)?;
        channel.input.flush()?;

        let mut writer = self.writer.lock().expect("remote writer");
        terminate_current(&mut writer);
        writer.generation = writer.generation.saturating_add(1);
        writer.controller_epoch = None;
        writer.child = Some(channel.child);
        writer.input = Some(channel.input);
        Ok((writer.generation, channel.output))
    }

    pub fn accept_hello(&self, generation: u64, epoch: u64) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        require_generation(&writer, generation)?;
        writer.controller_epoch = Some(epoch);
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
            write_message(&mut writer, &RemoteMessage::Terminal(Frame::input(bytes)))?;
        }
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
        if writer.controller_epoch.is_none() || writer.input.is_none() {
            // Wheel/motion is ephemeral. Replaying it after a reconnect would
            // target a screen that may already have changed.
            return Ok(());
        }
        if write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::scroll(direction, lines, col, row)),
        )
        .is_err()
        {
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
        if writer.controller_epoch.is_none() || writer.input.is_none() {
            return queue_input(&mut writer, bytes);
        }
        let frame = terminal_input_frame(self.helper.protocol, bytes, mouse);
        if let Err(error) = write_message(&mut writer, &RemoteMessage::Terminal(frame)) {
            queue_input(&mut writer, bytes)?;
            terminate_current(&mut writer);
            writer.controller_epoch = None;
            let _ = error;
            return Ok(());
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        validate_terminal_dimensions(cols, rows)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut writer = self.writer.lock().expect("remote writer");
        if writer.controller_epoch.is_none() || writer.input.is_none() {
            writer.queued_resize = Some((cols, rows));
            return Ok(());
        }
        if let Err(error) = write_message(
            &mut writer,
            &RemoteMessage::Terminal(Frame::resize(cols, rows)),
        ) {
            writer.queued_resize = Some((cols, rows));
            terminate_current(&mut writer);
            writer.controller_epoch = None;
            let _ = error;
            return Ok(());
        }
        Ok(())
    }

    pub fn signal(&self, signal: i32) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("remote writer");
        let epoch = writer.controller_epoch.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "remote controller is reconnecting",
            )
        })?;
        write_message(
            &mut writer,
            &RemoteMessage::Signal(Signal {
                controller_epoch: epoch,
                signal,
            }),
        )
    }

    pub fn kill(&self) -> io::Result<()> {
        self.manager.kill(
            &self.helper,
            &SessionSelector {
                session_id: self.session_id.clone(),
                session_token: self.token.clone(),
                expected_incarnation: Some(self.incarnation.clone()),
            },
        )?;
        Ok(())
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
            if writer.controller_epoch.is_none() {
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
        let persisted = self.persisted_output_offset.load(Ordering::Acquire);
        if offset.saturating_sub(persisted) < OFFSET_PERSIST_INTERVAL {
            return;
        }
        if self
            .binding_store
            .update_output_offset(&self.session_id, offset)
            .is_ok()
        {
            self.persisted_output_offset
                .store(offset, Ordering::Release);
        }
    }

    fn persist_observed_output_offset(&self) {
        let offset = self.observed_output_offset.load(Ordering::Acquire);
        if offset <= self.persisted_output_offset.load(Ordering::Acquire) {
            return;
        }
        if self
            .binding_store
            .update_output_offset(&self.session_id, offset)
            .is_ok()
        {
            self.persisted_output_offset
                .store(offset, Ordering::Release);
        }
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
    let encoded = RemoteCodec::encode(message).map_err(io::Error::other)?;
    let input = writer
        .input
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SSH channel is closed"))?;
    input.write_all(&encoded)?;
    input.flush()
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

fn terminate_current(writer: &mut WriterState) {
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
}
