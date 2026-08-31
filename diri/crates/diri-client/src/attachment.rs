//! One binary terminal data channel to a daemon session.
//!
//! This mirrors `Sources/DirijorClient/SessionAttachment.swift`: a fresh Unix
//! socket receives one JSON attach line and then carries binary frames until it
//! fails. Reattachment deliberately belongs to the caller.

use std::fmt;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::grid::GridUpdate;
use diri_proto::methods::{AttachRequest, ClientRole};
use diri_proto::model::SessionId;
use diri_proto::terminal::MouseModes;
use futures_core::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

const READ_BUFFER_BYTES: usize = 64 * 1024;
const KEEPALIVE_CHECK_EVERY: Duration = Duration::from_secs(5);
const PING_AFTER: Duration = Duration::from_secs(20);
const DEAD_AFTER: Duration = Duration::from_secs(30);
const CHUNK_QUEUE_CAPACITY: usize = 256;

/// A decoded event from the daemon's authoritative terminal data channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalChunk {
    Grid(GridUpdate),
    Modes {
        alt_screen: bool,
        bracketed_paste: bool,
        mouse: MouseModes,
    },
    Pong,
}

/// The receiving half of an attachment.
///
/// It implements [`Stream`] and also offers [`Self::recv`] so callers do not
/// need a stream extension trait for the common one-at-a-time use case.
#[derive(Debug)]
pub struct AttachmentChunks {
    receiver: mpsc::Receiver<TerminalChunk>,
}

impl AttachmentChunks {
    pub async fn recv(&mut self) -> Option<TerminalChunk> {
        self.receiver.recv().await
    }
}

impl Stream for AttachmentChunks {
    type Item = TerminalChunk;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

#[derive(Debug)]
pub enum AttachmentError {
    Io(io::Error),
    EncodeHandshake(serde_json::Error),
}

impl fmt::Display for AttachmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "attachment I/O failed: {error}"),
            Self::EncodeHandshake(error) => {
                write!(formatter, "failed to encode attach handshake: {error}")
            }
        }
    }
}

impl std::error::Error for AttachmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::EncodeHandshake(error) => Some(error),
        }
    }
}

impl From<io::Error> for AttachmentError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AttachmentError {
    fn from(error: serde_json::Error) -> Self {
        Self::EncodeHandshake(error)
    }
}

/// Returned when an outgoing frame is queued after the data channel has ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachmentClosed;

impl fmt::Display for AttachmentClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session attachment is closed")
    }
}

impl std::error::Error for AttachmentClosed {}

/// A separate binary data connection attached to one daemon session.
pub struct SessionAttachment {
    commands: mpsc::UnboundedSender<Command>,
    task: Option<JoinHandle<()>>,
    pub chunks: AttachmentChunks,
}

/// Cloneable write half for a live, resident attachment.
///
/// The app keeps this handle beside its resident terminal model while the
/// attachment task independently drains [`AttachmentChunks`]. It deliberately
/// exposes only per-session data-channel operations; reconnect policy remains
/// with the caller, as in `SessionAttachment.swift`.
#[derive(Clone)]
pub struct SessionAttachmentHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl SessionAttachmentHandle {
    pub fn send_input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), AttachmentClosed> {
        self.send(Frame::input(bytes))
    }

    pub fn send_mouse(&self, bytes: impl Into<Vec<u8>>) -> Result<(), AttachmentClosed> {
        self.send(Frame::mouse(bytes))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), AttachmentClosed> {
        self.send(Frame::resize(cols, rows))
    }

    pub fn scroll(
        &self,
        direction: u8,
        lines: u16,
        col: u16,
        row: u16,
    ) -> Result<(), AttachmentClosed> {
        self.send(Frame::scroll(direction, lines, col, row))
    }

    pub fn close(&self) -> Result<(), AttachmentClosed> {
        self.commands
            .send(Command::Close)
            .map_err(|_| AttachmentClosed)
    }

    fn send(&self, frame: Frame) -> Result<(), AttachmentClosed> {
        self.commands
            .send(Command::Frame(frame))
            .map_err(|_| AttachmentClosed)
    }
}

impl SessionAttachment {
    /// Opens a fresh local Unix socket and adopts it as a desktop data channel.
    pub async fn connect(
        socket_path: impl AsRef<Path>,
        session_id: SessionId,
    ) -> Result<Self, AttachmentError> {
        let stream = UnixStream::connect(socket_path).await?;
        Self::adopt(stream, session_id).await
    }

    async fn adopt(mut stream: UnixStream, session_id: SessionId) -> Result<Self, AttachmentError> {
        let request = AttachRequest {
            attach: session_id,
            from_offset: None,
            token: None,
            role: ClientRole::Desktop,
        };
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        stream.write_all(&line).await?;

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        // Bound decoded terminal work between the socket and UI. The daemon's
        // PTY drain is independent of this connection, so brief UI stalls
        // backpressure only the attach writer instead of growing client memory
        // without limit.
        let (chunk_tx, chunk_rx) = mpsc::channel(CHUNK_QUEUE_CAPACITY);
        let task = tokio::spawn(run_connection(stream, command_rx, chunk_tx));

        Ok(Self {
            commands: command_tx,
            task: Some(task),
            chunks: AttachmentChunks { receiver: chunk_rx },
        })
    }

    /// Queues raw keystroke bytes for the session PTY.
    pub fn send_input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), AttachmentClosed> {
        self.handle().send_input(bytes)
    }

    /// Queues a pre-encoded mouse report on the raw interactive path.
    pub fn send_mouse(&self, bytes: impl Into<Vec<u8>>) -> Result<(), AttachmentClosed> {
        self.handle().send_mouse(bytes)
    }

    /// Queues a PTY resize. Debouncing and first-resize semantics belong to the caller.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), AttachmentClosed> {
        self.handle().resize(cols, rows)
    }

    /// Queues a wheel event. `direction` is 0 for up and 1 for down.
    pub fn scroll(
        &self,
        direction: u8,
        lines: u16,
        col: u16,
        row: u16,
    ) -> Result<(), AttachmentClosed> {
        self.handle().scroll(direction, lines, col, row)
    }

    /// Returns a cloneable command handle so reads and writes can be driven by
    /// different tasks without wrapping the attachment in a mutex. The returned
    /// handle is tied to this connection and becomes closed when it is reattached.
    pub fn handle(&self) -> SessionAttachmentHandle {
        SessionAttachmentHandle {
            commands: self.commands.clone(),
        }
    }

    /// Detaches and cleanly finishes the chunk stream. Idempotent.
    pub async fn close(&mut self) {
        let _ = self.commands.send(Command::Close);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SessionAttachment {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

enum Command {
    Frame(Frame),
    Close,
}

async fn run_connection(
    mut stream: UnixStream,
    mut commands: mpsc::UnboundedReceiver<Command>,
    chunks: mpsc::Sender<TerminalChunk>,
) {
    let mut codec = FrameCodec::new();
    let mut read_buffer = vec![0_u8; READ_BUFFER_BYTES];
    let mut last_received = Instant::now();
    let start = Instant::now() + KEEPALIVE_CHECK_EVERY;
    let mut keepalive = tokio::time::interval_at(start, KEEPALIVE_CHECK_EVERY);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            read = stream.read(&mut read_buffer) => {
                let Ok(read) = read else { return };
                if read == 0 {
                    return;
                }
                last_received = Instant::now();
                let Ok(frames) = codec.feed(&read_buffer[..read]) else { return };
                for frame in frames {
                    if process_incoming(frame, &mut stream, &chunks).await.is_err() {
                        return;
                    }
                }
            }
            command = commands.recv() => {
                match command {
                    Some(Command::Frame(frame)) => {
                        if write_frame(&mut stream, &frame).await.is_err() {
                            return;
                        }
                    }
                    Some(Command::Close) | None => return,
                }
            }
            _ = keepalive.tick() => {
                let idle = Instant::now().duration_since(last_received);
                if idle >= DEAD_AFTER {
                    return;
                }
                if idle >= PING_AFTER && write_frame(&mut stream, &Frame::ping()).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn process_incoming(
    frame: Frame,
    stream: &mut UnixStream,
    chunks: &mpsc::Sender<TerminalChunk>,
) -> Result<(), ()> {
    match frame.frame_type {
        FrameType::Grid => {
            let update = frame.grid_payload().map_err(|_| ())?.ok_or(())?;
            chunks
                .send(TerminalChunk::Grid(update))
                .await
                .map_err(|_| ())?;
        }
        FrameType::Modes => {
            let (alt_screen, bracketed_paste, mouse) = frame.terminal_modes_payload().ok_or(())?;
            chunks
                .send(TerminalChunk::Modes {
                    alt_screen,
                    bracketed_paste,
                    mouse,
                })
                .await
                .map_err(|_| ())?;
        }
        FrameType::Ping => write_frame(stream, &Frame::pong()).await.map_err(|_| ())?,
        FrameType::Pong => chunks.send(TerminalChunk::Pong).await.map_err(|_| ())?,
        // These byte-replay frames belong to the retired VT-parsing client.
        FrameType::Output | FrameType::ReplayBegin | FrameType::ReplayEnd => {}
        // The daemon does not send client-to-daemon frame types.
        FrameType::Input | FrameType::Resize | FrameType::Scroll | FrameType::Mouse => {}
    }
    Ok(())
}

async fn write_frame(stream: &mut UnixStream, frame: &Frame) -> io::Result<()> {
    let encoded = FrameCodec::encode(frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stream.write_all(&encoded).await
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use diri_proto::control::{ControlMessage, decode_line, encode_line};
    use diri_proto::frames::Frame;
    use diri_proto::grid::GridCell;
    use diri_proto::methods::{
        HelloParams, HelloResult, Method, SessionIdParams, SessionSpawnParams,
    };
    use diri_proto::model::{AgentKind, SessionId, SessionRecord};
    use diri_proto::paths::{DirijorEnv, DirijorPaths};
    use diri_proto::terminal::{MouseEncoding, MouseModes, MouseTrackingMode};
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::time::timeout;

    use super::{SessionAttachment, TerminalChunk, process_incoming};

    #[tokio::test]
    async fn mode_frames_keep_bracketed_paste_state() {
        let (mut stream, _peer) = UnixStream::pair().expect("unix stream pair");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mouse = MouseModes::new(MouseTrackingMode::ButtonMotion, MouseEncoding::Sgr);

        process_incoming(
            Frame::modes_with_bracketed_paste(true, true, mouse),
            &mut stream,
            &tx,
        )
        .await
        .expect("valid modes frame");

        assert_eq!(
            rx.recv().await,
            Some(TerminalChunk::Modes {
                alt_screen: true,
                bracketed_paste: true,
                mouse,
            })
        );
    }

    struct TestControl {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
        next_id: u64,
    }

    impl TestControl {
        async fn connect(socket: &Path) -> Result<Self, Box<dyn Error>> {
            let stream = UnixStream::connect(socket).await?;
            let (reader, writer) = stream.into_split();
            let mut control = Self {
                reader: BufReader::new(reader),
                writer,
                next_id: 1,
            };
            let _: HelloResult = control
                .request(Method::HELLO, &HelloParams::new("diri-t4-integration"))
                .await?;
            Ok(control)
        }

        async fn request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Box<dyn Error>>
        where
            P: Serialize,
            R: DeserializeOwned,
        {
            let id = self.next_id;
            self.next_id += 1;
            let message = ControlMessage::Request {
                id,
                method: method.to_owned(),
                params: Some(serde_json::to_value(params)?),
            };
            self.writer.write_all(&encode_line(&message)?).await?;

            loop {
                let mut line = Vec::new();
                if self.reader.read_until(b'\n', &mut line).await? == 0 {
                    return Err("control channel closed".into());
                }
                match decode_line(&line)? {
                    ControlMessage::Response {
                        id: response_id,
                        result,
                    } if response_id == id => {
                        return match result {
                            Ok(value) => Ok(serde_json::from_value(value)?),
                            Err(error) => Err(Box::new(error)),
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    fn daemon_socket() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(DirijorEnv::SOCKET) {
            return Some(PathBuf::from(path));
        }
        std::env::var_os("HOME").map(DirijorPaths::socket)
    }

    fn composed_text(cells: &[GridCell]) -> String {
        cells
            .iter()
            .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
            .collect()
    }

    fn scratch_title() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        format!("diri-t4-test-{}-{nanos}", std::process::id())
    }

    async fn cleanup_session(control: &mut TestControl, session_id: &SessionId) {
        let params = SessionIdParams {
            session_id: session_id.clone(),
        };
        let _: Result<serde_json::Value, _> = control.request(Method::SESSION_KILL, &params).await;
        let _: Result<serde_json::Value, _> =
            control.request(Method::SESSION_REMOVE, &params).await;
    }

    #[tokio::test]
    async fn live_shell_attachment_renders_input_and_resize() -> Result<(), Box<dyn Error>> {
        if std::env::var_os("DIRI_RUN_MUTATING_DAEMON_TESTS").is_none() {
            eprintln!(
                "skipping mutating live daemon test; set DIRI_RUN_MUTATING_DAEMON_TESTS=1 to opt in"
            );
            return Ok(());
        }
        let Some(socket) = daemon_socket() else {
            eprintln!("skipping live attachment test: daemon socket path is unavailable");
            return Ok(());
        };
        if !socket.exists() {
            eprintln!(
                "skipping live attachment test: daemon socket does not exist at {}",
                socket.display()
            );
            return Ok(());
        }

        let scratch = tempfile::Builder::new().prefix("diri-t4-").tempdir()?;
        let mut control = TestControl::connect(&socket).await?;
        let spawn = SessionSpawnParams {
            kind: AgentKind::SHELL,
            cwd: scratch.path().to_string_lossy().into_owned(),
            new_worktree: None,
            worktree_branch: None,
            title: Some(scratch_title()),
            initial_prompt: None,
            parent: None,
            initial_cols: None,
            initial_rows: None,
            host: None,
            same_repo_as: None,
        };
        let session: SessionRecord = control.request(Method::SESSION_SPAWN, &spawn).await?;
        let session_id = session.id;

        let result: Result<(), Box<dyn Error>> = async {
            let mut attachment = SessionAttachment::connect(&socket, session_id.clone()).await?;
            attachment.resize(80, 24)?;

            let mut cells = Vec::new();
            timeout(Duration::from_secs(15), async {
                while let Some(chunk) = attachment.chunks.recv().await {
                    if let TerminalChunk::Grid(update) = chunk {
                        update.apply(&mut cells);
                        if !composed_text(&cells).trim().is_empty() {
                            return Ok::<(), Box<dyn Error>>(());
                        }
                    }
                }
                Err("attachment ended before the shell painted".into())
            })
            .await
            .map_err(|_| "timed out waiting for the shell to paint")??;

            attachment.send_input(b"echo diri_test_marker\n".to_vec())?;
            timeout(Duration::from_secs(15), async {
                while let Some(chunk) = attachment.chunks.recv().await {
                    if let TerminalChunk::Grid(update) = chunk {
                        update.apply(&mut cells);
                        if composed_text(&cells).contains("diri_test_marker") {
                            return Ok::<(), Box<dyn Error>>(());
                        }
                    }
                }
                Err("attachment ended before marker appeared".into())
            })
            .await
            .map_err(|_| "timed out waiting for marker")??;

            attachment.resize(100, 30)?;
            timeout(Duration::from_secs(15), async {
                while let Some(chunk) = attachment.chunks.recv().await {
                    if let TerminalChunk::Grid(update) = chunk
                        && update.is_full_snapshot
                        && update.cols == 100
                        && update.rows == 30
                    {
                        return Ok::<(), Box<dyn Error>>(());
                    }
                }
                Err("attachment ended before resized snapshot arrived".into())
            })
            .await
            .map_err(|_| "timed out waiting for resized full snapshot")??;
            attachment.close().await;
            Ok(())
        }
        .await;

        cleanup_session(&mut control, &session_id).await;
        result
    }
}
