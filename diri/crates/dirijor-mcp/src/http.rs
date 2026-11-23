//! Streamable HTTP transport for the `dirijor` MCP server, served by the
//! Engine itself so an Agent session costs no extra `dirijor-mcp` process.
//!
//! The transport is deliberately small and stateless:
//!
//! - one `POST /mcp` per JSON-RPC message, answered with `application/json`,
//!   or for `tools/call` with a `text/event-stream` that carries progress
//!   heartbeats for long waits before the final response;
//! - no `Mcp-Session-Id`: every POST names its Diri session through its
//!   bearer token, so a restarted Engine answers the next call without the
//!   client re-initializing;
//! - `Connection: close` after each response, so an idle Agent holds no
//!   socket or thread in the Engine.
//!
//! Every message goes through [`crate::protocol::handle_message`] and a
//! [`Bridge`] bound to the token's session, exactly like the stdio frontend,
//! so tools, schemas, errors, and write policy are shared. The stdio
//! frontend's per-process dispatch rules (overlapping reads capped at 8,
//! mutations in arrival order with at most 8 queued, cancellation of reads
//! and of not-yet-started mutations) are kept per Diri session here.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::Bridge;
use crate::cancellation::Cancellation;
use crate::protocol::{
    DirectBackend, error, expand_action, handle_profile, read_only, success, tool_content,
};

/// The only path the endpoint serves.
pub const MCP_PATH: &str = "/mcp";
const MAX_READS: usize = 8;
const MAX_QUEUED_MUTATIONS: u64 = 8;
const MAX_HEAD_BYTES: u64 = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY_BYTES: usize = diri_proto::control::MAX_CONTROL_LINE_BYTES;
/// Concurrent HTTP connections; past this the endpoint answers 503 at once.
const MAX_CONNECTIONS: usize = 256;
/// A client that stalls while sending its request is dropped.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Maps a bearer token to the live Diri session it was minted for.
pub trait Authenticator: Send + Sync + 'static {
    fn session(&self, token: &str) -> Option<String>;
}

impl<F> Authenticator for F
where
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    fn session(&self, token: &str) -> Option<String> {
        self(token)
    }
}

pub struct HttpConfig {
    /// The Engine control socket every tool call is bridged to.
    pub socket_path: PathBuf,
    /// Where Diri Notes live; `None` resolves it per call like the stdio
    /// frontend does.
    pub notes_dir: Option<PathBuf>,
    pub auth: Box<dyn Authenticator>,
}

pub struct HttpServer {
    config: HttpConfig,
    lanes: Mutex<HashMap<String, Lane>>,
    turn: Condvar,
    connections: AtomicUsize,
}

/// One Diri session's in-flight requests. Removed once idle, so a quiet
/// Engine keeps no per-session state for this transport.
#[derive(Default)]
struct Lane {
    active: HashMap<String, ActiveRequest>,
    reads: usize,
    next_ticket: u64,
    serving: u64,
}

impl Lane {
    fn idle(&self) -> bool {
        self.active.is_empty() && self.reads == 0 && self.next_ticket == self.serving
    }
}

struct ActiveRequest {
    cancellation: Cancellation,
    read_only: bool,
    started: bool,
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: &'static str,
}

impl HttpError {
    fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl HttpServer {
    pub fn new(config: HttpConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            lanes: Mutex::new(HashMap::new()),
            turn: Condvar::new(),
            connections: AtomicUsize::new(0),
        })
    }

    /// Accepts connections until the listener fails permanently. Blocks in
    /// `accept`, so an idle endpoint costs one parked thread and no wakeups.
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    // Descriptor exhaustion clears when a connection closes;
                    // back off instead of spinning, never leave the loop.
                    eprintln!("dirijor-mcp http: accept: {error}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if self.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                self.connections.fetch_sub(1, Ordering::SeqCst);
                let mut stream = stream;
                let _ = write_response(&mut stream, 503, "application/json", b"");
                continue;
            }
            let server = Arc::clone(&self);
            let spawned = std::thread::Builder::new()
                .name("diri-mcp-http".into())
                .spawn(move || {
                    server.handle_connection(stream);
                    server.connections.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.connections.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    fn handle_connection(&self, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(stream);
        let request = match read_request(&mut reader, &mut writer) {
            Ok(request) => request,
            Err(failure) => {
                let _ = write_response(
                    &mut writer,
                    failure.status,
                    "text/plain",
                    failure.message.as_bytes(),
                );
                return;
            }
        };
        self.handle_request(request, &mut writer);
        let _ = writer.shutdown(Shutdown::Both);
    }

    fn handle_request(&self, request: Request, out: &mut TcpStream) {
        let path = request.path.split('?').next().unwrap_or_default();
        let compact = path == "/mcp/skills-v1";
        if path != MCP_PATH && !compact {
            let _ = write_response(out, 404, "text/plain", b"not found");
            return;
        }
        // DNS-rebinding guard: a browser page may reach 127.0.0.1, but it
        // always says where it came from.
        if let Some(origin) = request.header("origin")
            && !is_loopback_origin(origin)
        {
            let _ = write_response(out, 403, "text/plain", b"forbidden origin");
            return;
        }
        let Some(session) = request
            .header("authorization")
            .and_then(bearer_token)
            .and_then(|token| self.config.auth.session(token))
        else {
            let _ = write_head(
                out,
                401,
                &[
                    ("Content-Type", "text/plain"),
                    ("WWW-Authenticate", "Bearer realm=\"diri\""),
                ],
                Some(0),
            );
            return;
        };
        if request.method != "POST" {
            // No server-initiated stream and no session to delete.
            let _ = write_head(
                out,
                405,
                &[("Allow", "POST"), ("Content-Type", "text/plain")],
                Some(0),
            );
            return;
        }
        let mut message: Value = match serde_json::from_slice(&request.body) {
            Ok(message) => message,
            Err(_) => {
                let _ = write_json(out, 400, &error(Value::Null, -32700, "Parse error"));
                return;
            }
        };
        if message.is_array() {
            let _ = write_json(
                out,
                400,
                &error(Value::Null, -32600, "JSON-RPC batches are not supported"),
            );
            return;
        }
        let accepts_stream = request
            .header("accept")
            .is_some_and(|accept| accept.contains("text/event-stream"));
        if compact {
            expand_action(&mut message);
        }
        self.handle_message(&session, message, accepts_stream, out, compact);
    }

    fn handle_message(
        &self,
        session: &str,
        message: Value,
        accepts_stream: bool,
        out: &mut TcpStream,
        compact: bool,
    ) {
        let has_id = message.get("id").is_some();
        let method = message.get("method").and_then(Value::as_str);
        let valid = message["jsonrpc"] == "2.0"
            && method.is_some()
            && message
                .get("id")
                .is_none_or(|id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
            && message.get("params").is_none_or(Value::is_object);
        // A client's response to a server request: acknowledged, no body.
        if method.is_none()
            && message.is_object()
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            let _ = write_response(out, 202, "application/json", b"");
            return;
        }
        if valid && !has_id {
            if method == Some("notifications/cancelled") {
                self.cancel(session, &message["params"]["requestId"]);
            }
            let _ = write_response(out, 202, "application/json", b"");
            return;
        }
        if valid && method == Some("initialize") {
            let params = &message["params"];
            if !params["protocolVersion"].is_string()
                || !params["capabilities"].is_object()
                || !params["clientInfo"]["name"].is_string()
                || !params["clientInfo"]["version"].is_string()
            {
                let _ = write_json(
                    out,
                    200,
                    &error(
                        message["id"].clone(),
                        -32602,
                        "initialize requires protocolVersion, capabilities, and clientInfo",
                    ),
                );
                return;
            }
        }
        if valid && matches!(method, Some("tools/list" | "tools/call")) {
            self.dispatch(session, message, accepts_stream, out, compact);
            return;
        }
        let mut immediate =
            DirectBackend::with_bridge(self.bridge(session, Cancellation::default()));
        match handle_profile(message, &mut immediate, compact) {
            Some(response) => {
                let _ = write_json(out, 200, &response);
            }
            None => {
                let _ = write_response(out, 202, "application/json", b"");
            }
        }
    }

    fn bridge(&self, session: &str, cancellation: Cancellation) -> Bridge {
        let bridge = Bridge::new(self.config.socket_path.clone(), Some(session.to_owned()))
            .with_cancellation(cancellation);
        match &self.config.notes_dir {
            Some(dir) => bridge.with_notes_dir(dir.clone()),
            None => bridge,
        }
    }

    fn cancel(&self, session: &str, request_id: &Value) {
        let lanes = self.lanes.lock().unwrap();
        if let Some(request) = lanes
            .get(session)
            .and_then(|lane| lane.active.get(&request_id.to_string()))
            && (request.read_only || !request.started)
        {
            request.cancellation.cancel();
            // Wake a queued mutation so it gives up its turn promptly.
            self.turn.notify_all();
        }
    }

    fn dispatch(
        &self,
        session: &str,
        message: Value,
        accepts_stream: bool,
        out: &mut TcpStream,
        compact: bool,
    ) {
        let id = message["id"].clone();
        let key = id.to_string();
        let reads_only = read_only(&message);
        let cancellation = Cancellation::default();
        let ticket = {
            let mut lanes = self.lanes.lock().unwrap();
            let lane = lanes.entry(session.to_owned()).or_default();
            if lane.active.contains_key(&key) {
                if lane.idle() {
                    lanes.remove(session);
                }
                drop(lanes);
                let _ = write_json(
                    out,
                    200,
                    &error(
                        id,
                        -32600,
                        "request ID is already in progress; duplicate was not dispatched",
                    ),
                );
                return;
            }
            let busy = if reads_only {
                lane.reads >= MAX_READS
            } else {
                lane.next_ticket - lane.serving > MAX_QUEUED_MUTATIONS
            };
            if busy {
                if lane.idle() {
                    lanes.remove(session);
                }
                drop(lanes);
                let _ = write_json(
                    out,
                    200,
                    &success(
                        id,
                        tool_content(Err("MCP is busy; this request was not dispatched".into())),
                    ),
                );
                return;
            }
            lane.active.insert(
                key.clone(),
                ActiveRequest {
                    cancellation: cancellation.clone(),
                    read_only: reads_only,
                    started: false,
                },
            );
            if reads_only {
                lane.reads += 1;
                None
            } else {
                lane.next_ticket += 1;
                Some(lane.next_ticket - 1)
            }
        };
        let _finish = Finish {
            server: self,
            session,
            key: &key,
            ticket,
        };

        let stream = if accepts_stream && message["method"] == "tools/call" {
            match EventStream::open(out, &message, cancellation.clone(), reads_only) {
                Ok(stream) => Some(stream),
                Err(_) => {
                    cancellation.cancel();
                    None
                }
            }
        } else {
            None
        };
        if let Some(ticket) = ticket {
            // Mutations keep arrival order: wait for this ticket's turn.
            let mut lanes = self.lanes.lock().unwrap();
            loop {
                let lane = lanes.get_mut(session).expect("lane held by this request");
                if lane.serving == ticket {
                    if let Some(request) = lane.active.get_mut(&key) {
                        request.started = !request.cancellation.is_cancelled();
                    }
                    break;
                }
                lanes = self.turn.wait(lanes).unwrap();
            }
        } else if let Some(request) = self
            .lanes
            .lock()
            .unwrap()
            .get_mut(session)
            .and_then(|lane| lane.active.get_mut(&key))
        {
            request.started = true;
        }

        let response = if cancellation.is_cancelled() {
            None
        } else {
            let mut backend =
                DirectBackend::with_bridge(self.bridge(session, cancellation.clone()));
            let response_id = id.clone();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_profile(message, &mut backend, compact)
            }))
            .unwrap_or_else(|_| Some(success(response_id, tool_content(Err(
                "Tool failed unexpectedly. An action may already have reached the Engine; inspect its state before repeating it.".into()
            )))))
        };
        match stream {
            Some(stream) => {
                if let Some(response) = response.filter(|_| !cancellation.is_cancelled()) {
                    stream.finish(&response);
                }
            }
            None => {
                let response = response.unwrap_or_else(|| error(id, -32800, "Request cancelled"));
                let _ = write_json(out, 200, &response);
            }
        }
    }
}

/// Releases a request's slot and passes the mutation turn on, even if the
/// tool panicked or the client vanished.
struct Finish<'a> {
    server: &'a HttpServer,
    session: &'a str,
    key: &'a str,
    ticket: Option<u64>,
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        let mut lanes = self.server.lanes.lock().unwrap();
        if let Some(lane) = lanes.get_mut(self.session) {
            lane.active.remove(self.key);
            match self.ticket {
                Some(_) => lane.serving += 1,
                None => lane.reads -= 1,
            }
            if lane.idle() {
                lanes.remove(self.session);
            }
        }
        drop(lanes);
        if self.ticket.is_some() {
            self.server.turn.notify_all();
        }
    }
}

/// A `text/event-stream` answer to one `tools/call`. Long waits get a
/// heartbeat every 10 s: an MCP progress notification when the client asked
/// for one (as the stdio frontend sends), otherwise an SSE comment. The
/// heartbeat keeps HTTP idle timeouts from cutting a legitimate wait, and a
/// failed heartbeat means the client is gone, which cancels a read.
struct EventStream {
    out: Arc<Mutex<TcpStream>>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl EventStream {
    fn open(
        out: &TcpStream,
        message: &Value,
        cancellation: Cancellation,
        reads_only: bool,
    ) -> io::Result<Self> {
        let mut stream = out.try_clone()?;
        write_head(
            &mut stream,
            200,
            &[("Content-Type", "text/event-stream")],
            None,
        )?;
        stream.flush()?;
        let out = Arc::new(Mutex::new(stream));
        let tool = message
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !tool.starts_with("wait_") && !matches!(tool.as_str(), "spawn_agent" | "spawn_agents") {
            return Ok(Self {
                out,
                stop: None,
                heartbeat: None,
            });
        }
        let token = message.pointer("/params/_meta/progressToken").cloned();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let writer = Arc::clone(&out);
        let heartbeat = std::thread::Builder::new()
            .name("diri-mcp-http-progress".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(PROGRESS_INTERVAL)
                {
                    let elapsed = started.elapsed().as_secs();
                    let frame = match &token {
                        Some(token) => event(&json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": elapsed,
                                "message": format!("{tool}: still waiting after {elapsed}s"),
                            },
                        })),
                        None => b": waiting\n\n".to_vec(),
                    };
                    let mut out = writer.lock().unwrap();
                    if out.write_all(&frame).and_then(|()| out.flush()).is_err() {
                        // Only reads are abandoned with their caller; a
                        // dispatched mutation always runs to completion.
                        if reads_only {
                            cancellation.cancel();
                        }
                        return;
                    }
                }
            })
            .ok();
        Ok(Self {
            out,
            stop: Some(stop),
            heartbeat,
        })
    }

    fn finish(mut self, response: &Value) {
        self.stop_heartbeat();
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(&event(response)).and_then(|()| out.flush());
    }

    fn stop_heartbeat(&mut self) {
        self.stop.take();
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

fn event(message: &Value) -> Vec<u8> {
    let mut frame = b"event: message\ndata: ".to_vec();
    frame.extend(serde_json::to_vec(message).unwrap_or_default());
    frame.extend(b"\n\n");
    frame
}

fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

fn is_loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| &h[1..]).unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(host, _)| host)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn read_request(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
) -> Result<Request, HttpError> {
    let mut head = reader.by_ref().take(MAX_HEAD_BYTES);
    let mut line = String::new();
    let read = head
        .read_line(&mut line)
        .map_err(|_| HttpError::new(400, "bad request"))?;
    if read == 0 || !line.ends_with('\n') {
        return Err(HttpError::new(400, "bad request"));
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::new(400, "bad request"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::new(505, "HTTP/1.1 only"));
    }
    let (method, path) = (method.to_owned(), path.to_owned());
    let mut headers = Vec::new();
    loop {
        line.clear();
        head.read_line(&mut line)
            .map_err(|_| HttpError::new(400, "bad request"))?;
        if !line.ends_with('\n') {
            return Err(HttpError::new(431, "request head too large"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(HttpError::new(431, "too many headers"));
        }
        let (name, value) = trimmed
            .split_once(':')
            .ok_or(HttpError::new(400, "bad header"))?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    if request
        .header("expect")
        .is_some_and(|expect| expect.eq_ignore_ascii_case("100-continue"))
    {
        let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let chunked = request
        .header("transfer-encoding")
        .is_some_and(|encoding| encoding.to_ascii_lowercase().contains("chunked"));
    if chunked {
        request.body = read_chunked(reader)?;
    } else if let Some(length) = request.header("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| HttpError::new(400, "bad content-length"))?;
        if length > MAX_BODY_BYTES {
            return Err(HttpError::new(413, "MCP message exceeds the frame limit"));
        }
        let mut body = vec![0; length];
        reader
            .read_exact(&mut body)
            .map_err(|_| HttpError::new(400, "truncated body"))?;
        request.body = body;
    }
    Ok(request)
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader
            .by_ref()
            .take(1024)
            .read_line(&mut line)
            .map_err(|_| HttpError::new(400, "bad chunk"))?;
        let size = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size, 16).map_err(|_| HttpError::new(400, "bad chunk"))?;
        if size == 0 {
            // Trailers are not used by MCP clients; read through the end.
            loop {
                line.clear();
                reader
                    .by_ref()
                    .take(1024)
                    .read_line(&mut line)
                    .map_err(|_| HttpError::new(400, "bad chunk"))?;
                if line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err(HttpError::new(413, "MCP message exceeds the frame limit"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(|_| HttpError::new(400, "truncated chunk"))?;
        let mut crlf = [0; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|_| HttpError::new(400, "bad chunk"))?;
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

fn write_head(
    out: &mut impl Write,
    status: u16,
    headers: &[(&str, &str)],
    content_length: Option<usize>,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(length) = content_length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("Cache-Control: no-store\r\nConnection: close\r\n\r\n");
    out.write_all(head.as_bytes())
}

fn write_response(
    out: &mut impl Write,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write_head(
        out,
        status,
        &[("Content-Type", content_type)],
        Some(body.len()),
    )?;
    out.write_all(body)?;
    out.flush()
}

fn write_json(out: &mut impl Write, status: u16, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).unwrap_or_default();
    write_response(out, status, "application/json", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn start() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = HttpServer::new(HttpConfig {
            // No Engine: only transport behaviour that never reaches it.
            socket_path: PathBuf::from("/nonexistent/diri-test.sock"),
            notes_dir: None,
            auth: Box::new(|token: &str| (token == "good-token").then(|| "s-1".to_owned())),
        });
        std::thread::spawn(move || server.serve(listener));
        address
    }

    fn post(address: SocketAddr, headers: &[(&str, &str)], body: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head[9..12].parse().unwrap();
        (status, head.to_owned(), body.to_owned())
    }

    const AUTH: (&str, &str) = ("Authorization", "Bearer good-token");

    #[test]
    fn rejects_missing_and_unknown_tokens_with_401() {
        let address = start();
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let (status, head, _) = post(address, &[], ping);
        assert_eq!(status, 401);
        assert!(head.contains("WWW-Authenticate: Bearer"));
        let (status, _, _) = post(address, &[("Authorization", "Bearer bad-token")], ping);
        assert_eq!(status, 401);
        let (status, _, _) = post(address, &[("Authorization", "Basic good-token")], ping);
        assert_eq!(status, 401);
        let (status, _, body) = post(address, &[AUTH], ping);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["result"],
            json!({})
        );
    }

    #[test]
    fn initialize_matches_the_stdio_server() {
        let address = start();
        let (status, _, body) = post(
            address,
            &[AUTH, ("Accept", "application/json, text/event-stream")],
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        );
        assert_eq!(status, 200);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            body["result"],
            crate::protocol::initialize(&json!({"protocolVersion":"2025-06-18"}))
        );
        let (_, _, body) = post(
            address,
            &[AUTH],
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32602
        );
    }

    #[test]
    fn notifications_are_accepted_and_unknown_methods_are_errors() {
        let address = start();
        let (status, _, body) = post(
            address,
            &[AUTH],
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert_eq!((status, body.as_str()), (202, ""));
        let (status, _, body) = post(
            address,
            &[AUTH],
            r#"{"jsonrpc":"2.0","id":"probe","method":"server/discover","params":{}}"#,
        );
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32601
        );
        let (status, _, _) = post(address, &[AUTH], "not json");
        assert_eq!(status, 400);
        let (status, _, _) = post(address, &[AUTH], "[]");
        assert_eq!(status, 400);
    }

    #[test]
    fn tool_errors_reach_the_client_through_the_event_stream() {
        // With no Engine behind it the call fails, and that failure is the
        // ordinary tool error the stdio frontend would return.
        let address = start();
        let (status, head, body) = post(
            address,
            &[AUTH, ("Accept", "application/json, text/event-stream")],
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_agents","arguments":{}}}"#,
        );
        assert_eq!(status, 200);
        assert!(head.contains("text/event-stream"), "{head}");
        let data = body
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("one event");
        let message: Value = serde_json::from_str(data).unwrap();
        assert_eq!(message["id"], 7);
        assert_eq!(message["result"]["isError"], true);
    }

    #[test]
    fn rejects_other_paths_methods_and_foreign_origins() {
        let address = start();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(b"GET /mcp HTTP/1.1\r\nAuthorization: Bearer good-token\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");

        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(b"POST /other HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        let (status, _, _) = post(
            address,
            &[AUTH, ("Origin", "https://evil.example")],
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert_eq!(status, 403);
        let (status, _, _) = post(
            address,
            &[AUTH, ("Origin", "http://localhost:3000")],
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn oversized_bodies_are_refused_before_they_are_read() {
        let address = start();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(
                format!(
                    "POST /mcp HTTP/1.1\r\nAuthorization: Bearer good-token\r\nContent-Length: {}\r\n\r\n",
                    MAX_BODY_BYTES + 1
                )
                .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    }

    #[test]
    fn reads_chunked_bodies() {
        let address = start();
        let mut stream = TcpStream::connect(address).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let (a, b) = body.split_at(10);
        stream
            .write_all(
                format!(
                    "POST /mcp HTTP/1.1\r\nAuthorization: Bearer good-token\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                    a.len(),
                    b.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        // Key order depends on serde_json features unified across the
        // workspace, so compare values, not text.
        let body = response.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"jsonrpc":"2.0","id":1,"result":{}})
        );
    }

    #[test]
    fn origin_parsing() {
        assert!(is_loopback_origin("http://127.0.0.1:8080"));
        assert!(is_loopback_origin("http://[::1]:1"));
        assert!(is_loopback_origin("https://localhost"));
        assert!(!is_loopback_origin("http://localhost.evil.com"));
        assert!(!is_loopback_origin("null"));
    }
}
