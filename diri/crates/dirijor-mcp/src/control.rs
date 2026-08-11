use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use diri_proto::control::MAX_CONTROL_LINE_BYTES;
use diri_proto::{ControlError, ControlMessage};
use serde_json::Value;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum ControlFailure {
    Io(io::Error),
    Protocol(String),
    Daemon(ControlError),
    Timeout,
}

impl std::fmt::Display for ControlFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Daemon(error) => error.fmt(formatter),
            Self::Timeout => formatter.write_str("daemon request timed out"),
        }
    }
}

impl std::error::Error for ControlFailure {}

impl From<io::Error> for ControlFailure {
    fn from(error: io::Error) -> Self {
        if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            Self::Timeout
        } else {
            Self::Io(error)
        }
    }
}

pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DIRIJOR_SOCKET") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Library/Application Support/Dirijor/daemon.sock")
}

#[cfg(unix)]
pub struct ControlClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

#[cfg(unix)]
impl ControlClient {
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self, ControlFailure> {
        let stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_read_timeout(Some(timeout))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    pub fn connect_default(timeout: Duration) -> Result<Self, ControlFailure> {
        Self::connect(&default_socket_path(), timeout)
    }

    pub fn set_read_timeout(&self, timeout: Duration) -> Result<(), ControlFailure> {
        self.stream.set_read_timeout(Some(timeout))?;
        Ok(())
    }

    pub fn request(
        &mut self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Value, ControlFailure> {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let request = ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        };
        serde_json::to_writer(&mut self.stream, &request)
            .map_err(|error| ControlFailure::Protocol(error.to_string()))?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;

        loop {
            match self.read_message()? {
                ControlMessage::Response {
                    id: response_id,
                    result,
                } if response_id == id => return result.map_err(ControlFailure::Daemon),
                ControlMessage::Event { .. } => continue,
                other => {
                    return Err(ControlFailure::Protocol(format!(
                        "unexpected daemon message while waiting for request {id}: {other:?}"
                    )));
                }
            }
        }
    }

    pub fn subscribe(
        &mut self,
        params: Value,
        deadline: Instant,
        mut on_event: impl FnMut(&str, u64, &Value) -> Result<bool, ControlFailure>,
    ) -> Result<(), ControlFailure> {
        let now = Instant::now();
        if now >= deadline {
            return Err(ControlFailure::Timeout);
        }
        self.set_read_timeout(deadline.saturating_duration_since(now))?;
        let subscribed = self.request(diri_proto::Method::EVENTS_SUBSCRIBE, params)?;
        if subscribed.get("subscribed").and_then(Value::as_bool) != Some(true) {
            return Err(ControlFailure::Protocol(
                "daemon did not acknowledge the event subscription".into(),
            ));
        }

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(ControlFailure::Timeout);
            }
            self.set_read_timeout(deadline.saturating_duration_since(now))?;
            match self.read_message()? {
                ControlMessage::Event { name, seq, params } => {
                    if !on_event(&name, seq, &params)? {
                        return Ok(());
                    }
                }
                ControlMessage::Response { .. } => continue,
                ControlMessage::Request { .. } => {
                    return Err(ControlFailure::Protocol(
                        "daemon sent a request on an event subscription".into(),
                    ));
                }
            }
        }
    }

    fn read_message(&mut self) -> Result<ControlMessage, ControlFailure> {
        let mut line = Vec::new();
        let read = self
            .reader
            .by_ref()
            .take((MAX_CONTROL_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err(ControlFailure::Protocol(
                "daemon closed the control connection".into(),
            ));
        }
        if line.len() > MAX_CONTROL_LINE_BYTES {
            return Err(ControlFailure::Protocol(format!(
                "daemon message exceeds {MAX_CONTROL_LINE_BYTES} bytes"
            )));
        }
        serde_json::from_slice(&line)
            .map_err(|error| ControlFailure::Protocol(format!("invalid daemon response: {error}")))
    }
}

#[cfg(not(unix))]
pub struct ControlClient;

#[cfg(not(unix))]
impl ControlClient {
    pub fn connect(_: &Path, _: Duration) -> Result<Self, ControlFailure> {
        Err(ControlFailure::Protocol(
            "the local Diri control socket requires a unix platform".into(),
        ))
    }

    pub fn connect_default(timeout: Duration) -> Result<Self, ControlFailure> {
        Self::connect(Path::new(""), timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_socket_override_wins() {
        // SAFETY: this unit test is single-threaded with respect to this key.
        unsafe { std::env::set_var("DIRIJOR_SOCKET", "/tmp/diri-test.sock") };
        assert_eq!(default_socket_path(), PathBuf::from("/tmp/diri-test.sock"));
        // SAFETY: restore process state immediately.
        unsafe { std::env::remove_var("DIRIJOR_SOCKET") };
    }
}
