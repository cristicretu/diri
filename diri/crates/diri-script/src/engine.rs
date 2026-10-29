//! A small blocking client for the Engine's control socket, owned by the
//! script thread. One connection for requests (kept open and reopened after
//! an Engine restart) and, only when a script listens to Engine events, one
//! more for the event stream on its own reader thread.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diri_platform::ipc::UnixStream;
use diri_proto::control::MAX_CONTROL_LINE_BYTES;
use diri_proto::{ControlMessage, Method};
use serde_json::{Value, json};

use crate::HostError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// The Engine socket this process would use: `$DIRIJOR_SOCKET`, else the
/// default under the runtime directory.
pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DIRIJOR_SOCKET") {
        return PathBuf::from(path);
    }
    let home = diri_platform::home_dir().unwrap_or_else(|| PathBuf::from("."));
    diri_proto::paths::DirijorPaths::socket(home)
}

struct Connection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Connection {
    /// Connects and checks the Engine's identity: a wrong protocol or a
    /// non-Rust Engine fails closed.
    fn open(path: &Path, build: &str) -> Result<Self, HostError> {
        let stream = UnixStream::connect(path).map_err(|error| {
            HostError::new(
                "engine_unreachable",
                format!("could not reach the Diri Engine: {error}"),
            )
        })?;
        let reader = BufReader::new(stream.try_clone().map_err(io_error)?);
        let mut connection = Self {
            stream,
            reader,
            next_id: 1,
        };
        let hello = connection.request(
            Method::HELLO,
            json!({"proto": diri_proto::WIRE_VERSION, "build": build}),
            CONNECT_TIMEOUT,
        )?;
        if hello["proto"].as_u64() != Some(u64::from(diri_proto::WIRE_VERSION))
            || hello["engineKind"].as_str() != Some(diri_proto::RUST_ENGINE_KIND)
        {
            return Err(HostError::new(
                "engine_mismatch",
                "the running Diri Engine speaks another protocol; restart Diri",
            ));
        }
        Ok(connection)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .map_err(|error| HostError::new("invalid_params", error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_CONTROL_LINE_BYTES {
            return Err(HostError::new(
                "frame_too_large",
                "the request is larger than the Engine accepts",
            ));
        }
        self.stream
            .set_write_timeout(Some(timeout))
            .map_err(io_error)?;
        self.stream.write_all(&bytes).map_err(io_error)?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.read(Some(deadline))? {
                ControlMessage::Response {
                    id: answered,
                    result,
                } if answered == id => {
                    return result.map_err(|error| HostError::new(error.code, error.message));
                }
                // Events (none are subscribed here) and stale answers.
                _ => continue,
            }
        }
    }

    fn read(&mut self, deadline: Option<Instant>) -> Result<ControlMessage, HostError> {
        let mut line = Vec::new();
        loop {
            let timeout = match deadline {
                Some(deadline) => Some(
                    deadline
                        .checked_duration_since(Instant::now())
                        .filter(|left| !left.is_zero())
                        .ok_or_else(timed_out)?,
                ),
                None => None,
            };
            self.stream.set_read_timeout(timeout).map_err(io_error)?;
            let before = line.len();
            let read = Read::take(
                &mut self.reader,
                (MAX_CONTROL_LINE_BYTES + 1 - before) as u64,
            )
            .read_until(b'\n', &mut line)
            .map_err(io_error)?;
            if read == 0 {
                return Err(HostError::new(
                    "engine_unreachable",
                    "the Diri Engine closed the connection",
                ));
            }
            if line.len() > MAX_CONTROL_LINE_BYTES {
                return Err(HostError::new(
                    "frame_too_large",
                    "the Engine sent a reply larger than the frame limit",
                ));
            }
            if line.ends_with(b"\n") {
                return serde_json::from_slice(&line).map_err(|error| {
                    HostError::new("protocol", format!("unreadable Engine reply: {error}"))
                });
            }
        }
    }
}

fn io_error(error: io::Error) -> HostError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        timed_out()
    } else {
        HostError::new("engine_unreachable", format!("Diri Engine: {error}"))
    }
}

fn timed_out() -> HostError {
    HostError::new("timeout", "the Diri Engine did not answer in time")
}

/// Requests over one kept-open connection.
pub struct EngineClient {
    path: PathBuf,
    build: String,
    connection: Option<Connection>,
}

impl EngineClient {
    pub fn new(path: PathBuf, build: impl Into<String>) -> Self {
        Self {
            path,
            build: build.into(),
            connection: None,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HostError> {
        // A connection the Engine closed (a restart) is reopened once, before
        // anything was sent on the new one.
        for attempt in 0..2 {
            let connection = match self.connection.as_mut() {
                Some(connection) => connection,
                None => self
                    .connection
                    .insert(Connection::open(&self.path, &self.build)?),
            };
            match connection.request(method, params.clone(), timeout) {
                Err(error) if error.code == "engine_unreachable" && attempt == 0 => {
                    self.connection = None;
                }
                Err(error) => {
                    if matches!(error.code.as_str(), "engine_unreachable" | "timeout") {
                        // Its stream may hold a late answer; never reuse it.
                        self.connection = None;
                    }
                    return Err(error);
                }
                Ok(value) => return Ok(value),
            }
        }
        Err(HostError::new(
            "engine_unreachable",
            "could not reach the Diri Engine",
        ))
    }
}

/// Subscribes to `kinds` and calls `on_event(name, params)` for each event
/// until it returns false or the connection ends. Blocks; run it on its own
/// thread. `on_subscribed` runs once the subscription is in place, so the
/// caller can read a snapshot without missing anything in between.
pub fn subscribe(
    path: &Path,
    build: &str,
    kinds: &[&str],
    on_subscribed: impl FnOnce(),
    mut on_event: impl FnMut(&str, Value) -> bool,
) -> Result<(), HostError> {
    let mut connection = Connection::open(path, build)?;
    let subscribed = connection.request(
        Method::EVENTS_SUBSCRIBE,
        json!({"kinds": kinds}),
        CONNECT_TIMEOUT,
    )?;
    if subscribed.get("subscribed").and_then(Value::as_bool) != Some(true) {
        return Err(HostError::new(
            "protocol",
            "the Engine did not accept the event subscription",
        ));
    }
    on_subscribed();
    loop {
        match connection.read(None)? {
            ControlMessage::Event { name, params, .. } => {
                if !on_event(&name, params) {
                    return Ok(());
                }
            }
            ControlMessage::Response { .. } => {}
            ControlMessage::Request { .. } => {
                return Err(HostError::new(
                    "protocol",
                    "the Engine sent a request on an event stream",
                ));
            }
        }
    }
}
