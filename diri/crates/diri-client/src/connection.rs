use std::path::Path;
use std::sync::Arc;

use diri_proto::control::{MAX_CONTROL_LINE_BYTES, decode_line};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::client::{ClientCore, ClientError};

const WRITE_QUEUE_CAPACITY: usize = 256;

/// The live read/write halves of one control socket.
///
/// Dropping this value aborts both halves, which is how heartbeat failures and
/// reconnects force the old socket closed without leaving detached IO tasks.
pub(crate) struct ActiveConnection {
    sender: mpsc::Sender<Vec<u8>>,
    failures: mpsc::UnboundedReceiver<ClientError>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl ActiveConnection {
    pub(crate) async fn open(
        path: &Path,
        core: Arc<ClientCore>,
        generation: u64,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).await.map_err(ClientError::io)?;
        let (read_half, mut write_half) = stream.into_split();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE_CAPACITY);
        let (failure_tx, failure_rx) = mpsc::unbounded_channel();

        let reader_failure_tx = failure_tx.clone();
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            let mut line = Vec::new();
            loop {
                line.clear();
                let read_result = loop {
                    let available = match reader.fill_buf().await {
                        Ok(available) => available,
                        Err(error) => break Err(ClientError::io(error)),
                    };
                    if available.is_empty() {
                        break Err(ClientError::disconnected(
                            "daemon closed the control connection",
                        ));
                    }

                    let newline = available.iter().position(|byte| *byte == b'\n');
                    let consumed = newline.map_or(available.len(), |index| index + 1);
                    let payload_len = newline.unwrap_or(available.len());
                    if line.len().saturating_add(payload_len) > MAX_CONTROL_LINE_BYTES {
                        break Err(ClientError::protocol(format!(
                            "control line exceeds {MAX_CONTROL_LINE_BYTES} bytes"
                        )));
                    }
                    line.extend_from_slice(&available[..payload_len]);
                    reader.consume(consumed);
                    if newline.is_some() {
                        break Ok(());
                    }
                };

                match read_result {
                    Err(error) => {
                        let _ = reader_failure_tx.send(error);
                        break;
                    }
                    Ok(()) if line.iter().all(u8::is_ascii_whitespace) => continue,
                    Ok(()) => match decode_line(&line) {
                        Ok(message) => core.route_message(generation, message).await,
                        Err(error) => {
                            let _ = reader_failure_tx.send(ClientError::json(error));
                            break;
                        }
                    },
                }
            }
        });

        let writer = tokio::spawn(async move {
            while let Some(line) = write_rx.recv().await {
                if let Err(error) = write_half.write_all(&line).await {
                    let _ = failure_tx.send(ClientError::io(error));
                    break;
                }
            }
        });

        Ok(Self {
            sender: write_tx,
            failures: failure_rx,
            reader,
            writer,
        })
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.sender.clone()
    }

    pub(crate) async fn closed(&mut self) -> ClientError {
        self.failures.recv().await.unwrap_or_else(|| {
            ClientError::disconnected("control connection tasks stopped unexpectedly")
        })
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}
