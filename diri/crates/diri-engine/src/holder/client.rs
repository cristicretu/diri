//! Daemon-side clients: one for a session holder, one for the shared manager.
//!
//! Control requests remain connectionless so a Holder can outlive any number
//! of daemons. High-frequency input and resize operations negotiate one
//! acknowledged binary stream; clients fall back to the legacy request shape
//! when adopting an older live Holder.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;

use super::protocol::{
    HOLDER_OUTPUT_MAX_FRAME, HOLDER_OUTPUT_STREAM_VERSION, HOLDER_STREAM_ACK, HOLDER_STREAM_INPUT,
    HOLDER_STREAM_MAX_PAYLOAD, HOLDER_STREAM_RESIZE, HOLDER_STREAM_VERSION, HolderLaunchSpec,
    HolderManagerRequest, HolderManagerResponse, HolderOperation, HolderProcessSample,
    HolderRequest, HolderResponse, HolderStat,
};
use super::socket;
use super::{HolderError, HolderResult};

/// Control client for one session holder.
#[derive(Clone, Debug)]
pub struct HolderClient {
    pub socket_path: PathBuf,
    input: Arc<Mutex<InputTransport>>,
}

#[derive(Debug)]
enum InputTransport {
    Unknown,
    Stream(HolderInputStream),
    Legacy,
}

#[derive(Debug)]
struct HolderInputStream {
    stream: UnixStream,
    encoded: Vec<u8>,
}

impl HolderClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            input: Arc::new(Mutex::new(InputTransport::Unknown)),
        }
    }

    /// Sends bytes to the held child, as if typed.
    pub fn write(&self, data: &[u8]) -> HolderResult<()> {
        if self.send_stream(HOLDER_STREAM_INPUT, data)? {
            return Ok(());
        }
        self.write_legacy(data)
    }

    fn write_legacy(&self, data: &[u8]) -> HolderResult<()> {
        let mut request = HolderRequest::op(HolderOperation::Write);
        request.data = Some(base64::engine::general_purpose::STANDARD.encode(data));
        self.request(&request).map(drop)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> HolderResult<()> {
        let mut payload = [0_u8; 4];
        payload[..2].copy_from_slice(&cols.to_be_bytes());
        payload[2..].copy_from_slice(&rows.to_be_bytes());
        if self.send_stream(HOLDER_STREAM_RESIZE, &payload)? {
            return Ok(());
        }
        let mut request = HolderRequest::op(HolderOperation::Resize);
        request.cols = Some(cols);
        request.rows = Some(rows);
        self.request(&request).map(drop)
    }

    /// Signals the whole child tree; returns the processes that were signalled.
    pub fn signal(&self, sig: i32) -> HolderResult<Vec<HolderProcessSample>> {
        let mut request = HolderRequest::op(HolderOperation::Signal);
        request.sig = Some(sig);
        Ok(self.request(&request)?.tree.unwrap_or_default())
    }

    pub fn kill_tree(&self) -> HolderResult<()> {
        self.request(&HolderRequest::op(HolderOperation::KillTree))
            .map(drop)
    }

    pub fn stat(&self) -> HolderResult<HolderStat> {
        self.request(&HolderRequest::op(HolderOperation::Stat))?
            .stat
            .ok_or_else(|| HolderError::Transport("stat response omitted stat".into()))
    }

    /// Whether a live holder with a live child serves this socket.
    /// Subscribes to PTY output as the holder reads it.
    ///
    /// Returns the stream and the offset its first frame will carry: bytes
    /// below that belong to the log, and the caller must finish reading them
    /// before consuming a frame, or the emulator would see a gap. `None` means
    /// the holder predates the output stream, and tailing the log is the only
    /// route.
    pub fn open_output_stream(&self) -> HolderResult<Option<HolderOutputStream>> {
        let mut stream = socket::connect(&self.socket_path)?;
        let mut request = HolderRequest::op(HolderOperation::OutputStream);
        request.stream_version = Some(HOLDER_OUTPUT_STREAM_VERSION);
        socket::write_json_line(&mut stream, &request)?;
        let response: HolderResponse = socket::read_json_line(&mut stream)?;
        if !response.ok || response.stream_version != Some(HOLDER_OUTPUT_STREAM_VERSION) {
            return Ok(None);
        }
        let Some(start_offset) = response.start_offset else {
            return Ok(None);
        };
        Ok(Some(HolderOutputStream {
            stream: std::io::BufReader::with_capacity(OUTPUT_READ_BUFFER, stream),
            start_offset,
            timeout: None,
        }))
    }

    pub fn is_alive(&self) -> bool {
        self.stat().map(|stat| stat.alive).unwrap_or(false)
    }

    fn request(&self, request: &HolderRequest) -> HolderResult<HolderResponse> {
        let mut stream = socket::connect(&self.socket_path)?;
        socket::write_json_line(&mut stream, request)?;
        let response: HolderResponse = socket::read_json_line(&mut stream)?;
        if !response.ok {
            return Err(HolderError::Rejected(
                response.error.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        Ok(response)
    }

    /// Sends one acknowledged operation over the negotiated binary stream.
    /// `Ok(false)` means the live Holder predates this additive protocol and
    /// the caller must issue the equivalent legacy request. Once a stream has
    /// accepted a frame, a missing acknowledgement is returned as an error and
    /// the frame is never retried, avoiding duplicate keystrokes.
    fn send_stream(&self, kind: u8, payload: &[u8]) -> HolderResult<bool> {
        let mut transport = self
            .input
            .lock()
            .map_err(|_| HolderError::Transport("input stream lock poisoned".into()))?;
        if matches!(*transport, InputTransport::Unknown) {
            *transport = match HolderInputStream::connect(&self.socket_path)? {
                Some(stream) => InputTransport::Stream(stream),
                None => InputTransport::Legacy,
            };
        }
        match &mut *transport {
            InputTransport::Stream(stream) => match stream.send(kind, payload) {
                Ok(()) => Ok(true),
                Err(error) => {
                    *transport = InputTransport::Unknown;
                    Err(error)
                }
            },
            InputTransport::Legacy => Ok(false),
            InputTransport::Unknown => unreachable!("negotiation resolved the transport"),
        }
    }
}

/// Read buffer for one output subscription.
const OUTPUT_READ_BUFFER: usize = 256 << 10;

/// A subscription to one holder's PTY output.
pub struct HolderOutputStream {
    /// Buffered, because a frame costs two reads and frames are small: at PTY
    /// chunk sizes the syscalls cost more than the parsing they feed.
    stream: std::io::BufReader<std::os::unix::net::UnixStream>,
    start_offset: u64,
    /// The timeout currently set on the socket. Setting it is a syscall, and
    /// the coalescing loop would otherwise pay one per frame.
    timeout: Option<Duration>,
}

impl HolderOutputStream {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> HolderResult<()> {
        if self.timeout == timeout {
            return Ok(());
        }
        self.stream
            .get_ref()
            .set_read_timeout(timeout)
            .map_err(|error| HolderError::io("set output stream timeout", error))?;
        self.timeout = timeout;
        Ok(())
    }

    /// Offset of the first byte this stream will deliver.
    #[must_use]
    pub fn start_offset(&self) -> u64 {
        self.start_offset
    }

    /// Blocks up to `timeout` for the next frame, then folds in every frame
    /// already waiting behind it, into `run`, up to `budget` bytes.
    ///
    /// Frames are PTY-sized while the caller's per-pass work — locking the
    /// screen, checking timers, publishing — is charged per call, so handing
    /// back one large contiguous run instead of a dozen small ones is worth
    /// several times the throughput. `run` is the caller's buffer and is
    /// reused, because allocating one per frame and copying into it charged
    /// every byte of output an allocation and a copy it did not need.
    ///
    /// Returns the offset the run begins at.
    pub fn next_run_into(
        &mut self,
        timeout: Duration,
        budget: usize,
        run: &mut Vec<u8>,
    ) -> HolderResult<Option<u64>> {
        run.clear();
        let Some(offset) = self.read_frame_into(timeout, run)? else {
            return Ok(None);
        };
        while run.len() < budget {
            match self.read_frame_into(Duration::ZERO, run) {
                Ok(Some(_)) => {}
                Ok(None) => break,
                // A frame that fails mid-run leaves the stream unusable, but
                // what was already read is contiguous and safe to return; the
                // next call surfaces the error.
                Err(_) => break,
            }
        }
        Ok(Some(offset))
    }

    /// Reads one frame onto the end of `run`, returning where it began.
    fn read_frame_into(
        &mut self,
        timeout: Duration,
        run: &mut Vec<u8>,
    ) -> HolderResult<Option<u64>> {
        // A zero `SO_RCVTIMEO` means "no timeout", which would block forever;
        // the shortest expressible wait is what "poll" has to mean here.
        let timeout = timeout.max(Duration::from_nanos(1));
        let mut header = [0_u8; 12];
        let mut filled = 0;
        while filled < header.len() {
            // The timeout applies only while waiting for a frame to begin.
            // Once any byte of one has arrived the rest is awaited without a
            // deadline: abandoning a frame halfway leaves those bytes consumed
            // and the stream desynchronized, and the next read would take
            // whatever followed for a header — a length that never arrives,
            // and a read that blocks until the holder exits.
            self.set_timeout(if filled == 0 { Some(timeout) } else { None })?;
            match self.stream.read(&mut header[filled..]) {
                Ok(0) => {
                    return Err(HolderError::io(
                        "read output stream",
                        std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                    ));
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if filled == 0
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(HolderError::io("read output stream", error)),
            }
        }
        let offset = u64::from_be_bytes(header[..8].try_into().expect("eight-byte offset"));
        let length = u32::from_be_bytes(header[8..].try_into().expect("four-byte length")) as usize;
        if length > HOLDER_OUTPUT_MAX_FRAME {
            return Err(HolderError::Rejected(format!(
                "output frame is {length} bytes; maximum is {HOLDER_OUTPUT_MAX_FRAME}"
            )));
        }
        // The rest of a frame that has started must arrive: a timeout here
        // would strand half of it and desynchronize the stream.
        self.set_timeout(None)?;
        let from = run.len();
        run.resize(from + length, 0);
        self.stream
            .read_exact(&mut run[from..])
            .map_err(|error| HolderError::io("read output frame", error))?;
        Ok(Some(offset))
    }
}

impl HolderInputStream {
    fn connect(path: &Path) -> HolderResult<Option<Self>> {
        let mut stream = socket::connect(path)?;
        let mut request = HolderRequest::op(HolderOperation::Stream);
        request.stream_version = Some(HOLDER_STREAM_VERSION);
        socket::write_json_line(&mut stream, &request)?;
        let response: HolderResponse = socket::read_json_line(&mut stream)?;
        if !response.ok || response.stream_version != Some(HOLDER_STREAM_VERSION) {
            return Ok(None);
        }
        Ok(Some(Self {
            stream,
            encoded: Vec::with_capacity(256),
        }))
    }

    fn send(&mut self, kind: u8, payload: &[u8]) -> HolderResult<()> {
        if payload.len() > HOLDER_STREAM_MAX_PAYLOAD {
            return Err(HolderError::InvalidRequest(format!(
                "input stream payload is {} bytes; maximum is {HOLDER_STREAM_MAX_PAYLOAD}",
                payload.len()
            )));
        }
        let length = u32::try_from(payload.len())
            .map_err(|_| HolderError::InvalidRequest("input stream payload is too large".into()))?;
        self.encoded.clear();
        self.encoded.reserve(5 + payload.len());
        self.encoded.push(kind);
        self.encoded.extend_from_slice(&length.to_be_bytes());
        self.encoded.extend_from_slice(payload);
        self.stream
            .write_all(&self.encoded)
            .map_err(|error| HolderError::io("write input stream", error))?;
        let mut acknowledgement = [0_u8; 1];
        self.stream
            .read_exact(&mut acknowledgement)
            .map_err(|error| HolderError::io("read input acknowledgement", error))?;
        if acknowledgement[0] != HOLDER_STREAM_ACK {
            return Err(HolderError::Rejected(format!(
                "input stream returned acknowledgement {}",
                acknowledgement[0]
            )));
        }
        Ok(())
    }
}

/// Control client for the one lightweight holder manager in a registry.
///
/// Session traffic never flows through this socket. It is used only to ask
/// the manager to create a session-local holder; per-session sockets, logs,
/// and restart adoption semantics are unchanged by its existence.
#[derive(Clone, Debug)]
pub struct HolderManagerClient {
    pub socket_path: PathBuf,
}

impl HolderManagerClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn ping(&self) -> HolderResult<i32> {
        self.request(&HolderManagerRequest::ping())
    }

    /// Asks the manager to host a holder for `spec`; returns the manager pid.
    pub fn launch(&self, spec: &HolderLaunchSpec) -> HolderResult<i32> {
        self.request(&HolderManagerRequest::launch(spec.clone()))
    }

    /// Stops the manager only after it has confirmed that no Holder thread is
    /// active. A refusal is safe and leaves its normal 30-second grace intact.
    pub fn shutdown_if_idle(&self) -> HolderResult<i32> {
        self.request(&HolderManagerRequest::shutdown_if_idle())
    }

    /// Subscribes to PTY output as the holder reads it.
    ///
    /// Returns the stream and the offset its first frame will carry: bytes
    /// below that belong to the log, and the caller must finish reading them
    /// before consuming a frame, or the emulator would see a gap. `None` means
    /// the holder predates the output stream, and tailing the log is the only
    /// route.
    pub fn open_output_stream(&self) -> HolderResult<Option<HolderOutputStream>> {
        let mut stream = socket::connect(&self.socket_path)?;
        let mut request = HolderRequest::op(HolderOperation::OutputStream);
        request.stream_version = Some(HOLDER_OUTPUT_STREAM_VERSION);
        socket::write_json_line(&mut stream, &request)?;
        let response: HolderResponse = socket::read_json_line(&mut stream)?;
        if !response.ok || response.stream_version != Some(HOLDER_OUTPUT_STREAM_VERSION) {
            return Ok(None);
        }
        let Some(start_offset) = response.start_offset else {
            return Ok(None);
        };
        Ok(Some(HolderOutputStream {
            stream: std::io::BufReader::with_capacity(OUTPUT_READ_BUFFER, stream),
            start_offset,
            timeout: None,
        }))
    }

    pub fn is_alive(&self) -> bool {
        self.ping().is_ok()
    }

    fn request(&self, request: &HolderManagerRequest) -> HolderResult<i32> {
        let mut stream = socket::connect(&self.socket_path)?;
        socket::write_json_line(&mut stream, request)?;
        let response: HolderManagerResponse = socket::read_json_line(&mut stream)?;
        if !response.ok {
            return Err(HolderError::Rejected(
                response
                    .error
                    .unwrap_or_else(|| "unknown manager error".into()),
            ));
        }
        match response.manager_pid {
            Some(pid) if pid > 1 => Ok(pid),
            _ => Err(HolderError::Transport(
                "manager response omitted pid".into(),
            )),
        }
    }
}

impl HolderClient {
    /// Convenience over `Path` without an allocation at every call site.
    pub fn at(path: &Path) -> Self {
        Self::new(path.to_path_buf())
    }
}
