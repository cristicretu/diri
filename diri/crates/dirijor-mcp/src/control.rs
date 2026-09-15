use std::io::{self, BufRead, BufReader, Write};
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
    Cancelled,
}

impl std::fmt::Display for ControlFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Daemon(error) => error.fmt(formatter),
            Self::Timeout => formatter.write_str("daemon request timed out"),
            Self::Cancelled => formatter.write_str("request cancelled"),
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
    diri_proto::paths::DirijorPaths::socket(home)
}

#[cfg(unix)]
pub struct ControlClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    timeout: Duration,
    cancellation: crate::cancellation::Cancellation,
    _registration: Option<crate::cancellation::Registration>,
}

#[cfg(unix)]
impl ControlClient {
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self, ControlFailure> {
        let stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_read_timeout(Some(timeout))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream,
            reader,
            timeout,
            cancellation: Default::default(),
            _registration: None,
        })
    }

    pub fn connect_default(timeout: Duration) -> Result<Self, ControlFailure> {
        Self::connect(&default_socket_path(), timeout)
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), ControlFailure> {
        self.timeout = timeout;
        self.stream.set_read_timeout(Some(timeout))?;
        Ok(())
    }

    pub fn with_cancellation(
        mut self,
        cancellation: crate::cancellation::Cancellation,
    ) -> Result<Self, ControlFailure> {
        self._registration = Some(cancellation.register(&self.stream)?);
        self.cancellation = cancellation;
        Ok(self)
    }

    pub fn request(
        &mut self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Value, ControlFailure> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(|| ControlFailure::Protocol("invalid request timeout".into()))?;
        self.request_until(method.into(), params, deadline)
    }

    pub(crate) fn request_until(
        &mut self,
        method: String,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, ControlFailure> {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let request = ControlMessage::Request {
            id,
            method,
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&request)
            .map_err(|_| ControlFailure::Protocol("could not encode daemon request".into()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_CONTROL_LINE_BYTES {
            return Err(ControlFailure::Protocol(
                "daemon request exceeds the frame limit; nothing was sent".into(),
            ));
        }
        let mut written = 0;
        while written < bytes.len() {
            self.stream
                .set_write_timeout(Some(self.remaining(deadline)?))?;
            match self.stream.write(&bytes[written..]) {
                Ok(0) => {
                    return Err(ControlFailure::Protocol(
                        "daemon closed the control connection".into(),
                    ));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(self.io_failure(error)),
            }
        }
        loop {
            match self.read_message_until(deadline)? {
                ControlMessage::Response {
                    id: response_id,
                    result,
                } if response_id == id => return result.map_err(ControlFailure::Daemon),
                ControlMessage::Event { .. } => continue,
                _ => {
                    return Err(ControlFailure::Protocol(
                        "unexpected daemon message while waiting for a response".into(),
                    ));
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
        self.subscribe_observing(params, deadline, |event| match event {
            None => Ok(true),
            Some((name, seq, params)) => on_event(name, seq, params),
        })
    }

    /// The callback runs once after subscription is installed, then on events.
    /// Read authoritative state on that first call to close the snapshot/event gap.
    pub fn subscribe_observing(
        &mut self,
        params: Value,
        deadline: Instant,
        mut observe: impl FnMut(Option<(&str, u64, &Value)>) -> Result<bool, ControlFailure>,
    ) -> Result<(), ControlFailure> {
        let subscribed = self.request_until(
            diri_proto::Method::EVENTS_SUBSCRIBE.into(),
            params,
            deadline,
        )?;
        if subscribed.get("subscribed").and_then(Value::as_bool) != Some(true) {
            return Err(ControlFailure::Protocol(
                "daemon did not acknowledge the event subscription".into(),
            ));
        }
        if !observe(None)? {
            return Ok(());
        }
        loop {
            match self.read_message_until(deadline)? {
                ControlMessage::Event { name, seq, params } => {
                    if !observe(Some((&name, seq, &params)))? {
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

    fn remaining(&self, deadline: Instant) -> Result<Duration, ControlFailure> {
        if self.cancellation.is_cancelled() {
            return Err(ControlFailure::Cancelled);
        }
        deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(ControlFailure::Timeout)
    }

    fn io_failure(&self, error: io::Error) -> ControlFailure {
        if self.cancellation.is_cancelled() {
            ControlFailure::Cancelled
        } else {
            error.into()
        }
    }

    fn read_message_until(&mut self, deadline: Instant) -> Result<ControlMessage, ControlFailure> {
        let mut line = Vec::new();
        loop {
            // Recompute for every socket read, including partial JSON frames.
            // A drip of bytes/events must not keep resetting the request timeout.
            self.stream
                .set_read_timeout(Some(self.remaining(deadline)?))?;
            let buffered = match self.reader.fill_buf() {
                Ok(bytes) if !bytes.is_empty() => bytes,
                Ok(_) => {
                    return Err(ControlFailure::Protocol(
                        "daemon closed the control connection".into(),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(self.io_failure(error)),
            };
            let newline = buffered.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(buffered.len(), |position| position + 1);
            if line.len() + take > MAX_CONTROL_LINE_BYTES {
                return Err(ControlFailure::Protocol(
                    "daemon message exceeds the frame limit".into(),
                ));
            }
            line.extend_from_slice(&buffered[..take]);
            self.reader.consume(take);
            if newline.is_some() {
                return serde_json::from_slice(&line)
                    .map_err(|_| ControlFailure::Protocol("invalid daemon response".into()));
            }
        }
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
    fn request_deadline_survives_event_floods_and_partial_frames() {
        for partial in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("engine.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let ControlMessage::Request { id, .. } = serde_json::from_str(&line).unwrap()
                else {
                    panic!("request");
                };
                let response = serde_json::to_vec(&ControlMessage::Response {
                    id,
                    result: Ok(serde_json::json!({})),
                })
                .unwrap();
                let event = serde_json::to_vec(&ControlMessage::Event {
                    name: "noise".into(),
                    seq: 1,
                    params: Value::Null,
                })
                .unwrap();
                for byte in &response {
                    std::thread::sleep(Duration::from_millis(10));
                    let bytes = if partial {
                        vec![*byte]
                    } else {
                        [event.as_slice(), b"\n"].concat()
                    };
                    if stream.write_all(&bytes).is_err() {
                        return;
                    }
                }
                if !partial {
                    let _ = stream.write_all(&response);
                }
                let _ = stream.write_all(b"\n");
            });
            let mut client = ControlClient::connect(&path, Duration::from_millis(50)).unwrap();
            let result = client.request("test", Value::Null);
            drop(client);
            worker.join().unwrap();
            assert!(
                matches!(result, Err(ControlFailure::Timeout)),
                "incoming bytes extended the absolute deadline: {result:?}"
            );
        }
    }

    #[test]
    fn protocol_errors_do_not_echo_daemon_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("engine.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            serde_json::to_writer(
                &mut stream,
                &ControlMessage::Request {
                    id: 999,
                    method: "unexpected".into(),
                    params: Some(serde_json::json!({"secret":"private-test-payload"})),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let mut client = ControlClient::connect(&path, Duration::from_secs(1)).unwrap();
        let error = client.request("test", Value::Null).unwrap_err();
        worker.join().unwrap();
        assert!(!error.to_string().contains("private-test-payload"));
    }

    #[test]
    fn explicit_socket_override_wins() {
        // SAFETY: this unit test is single-threaded with respect to this key.
        unsafe { std::env::set_var("DIRIJOR_SOCKET", "/tmp/diri-test.sock") };
        assert_eq!(default_socket_path(), PathBuf::from("/tmp/diri-test.sock"));
        // SAFETY: restore process state immediately.
        unsafe { std::env::remove_var("DIRIJOR_SOCKET") };
    }
}
