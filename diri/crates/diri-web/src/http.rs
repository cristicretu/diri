//! A small HTTP/1.1 server, sized to exactly what the phone frontend needs.
//!
//! This is deliberately not a framework. The surface is one user, on a
//! tailnet, driving a handful of JSON endpoints and one event stream, and a
//! framework would add a dependency tree (and a licence inventory) far larger
//! than the code it replaced. What is here is the subset that is easy to get
//! right: bounded reads, `Content-Length` bodies only, and keep-alive.
//!
//! Deliberate omissions, because nothing in this frontend uses them: chunked
//! request bodies, `Expect: 100-continue`, pipelining, and compression.

use std::collections::HashMap;
use std::fmt::Write as _;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// A request line plus headers may not exceed this. Generous for a browser,
/// small enough that a hostile peer cannot grow the buffer without bound.
const MAX_HEAD_BYTES: usize = 32 * 1024;

/// Bodies are prompts and spawn parameters. A megabyte is already luxurious;
/// pasted diffs and images do not travel this way.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The most headers we will accept before deciding the peer is not a browser.
const MAX_HEADERS: usize = 64;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// Path with the query string stripped, percent-decoded.
    pub path: String,
    /// Parsed `?a=b&c=d`, percent-decoded.
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    keep_alive: bool,
}

impl Request {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// The value of `name` in the `Cookie` header, if the browser sent one.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<String> {
        let raw = self.header("cookie")?;
        raw.split(';').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
    }

    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// The path split on `/`, with empty segments dropped.
    #[must_use]
    pub fn segments(&self) -> Vec<&str> {
        self.path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect()
    }
}

#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    /// The peer is not speaking HTTP we can serve; the caller closes the
    /// connection rather than guessing.
    Malformed(&'static str),
}

impl From<std::io::Error> for ReadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads one request. `Ok(None)` means the peer closed cleanly between
/// requests, which is the normal end of a keep-alive connection.
pub async fn read_request<R>(reader: &mut BufReader<R>) -> Result<Option<Request>, ReadError>
where
    R: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = read_line_bounded(reader, &mut line, MAX_HEAD_BYTES - head.len()).await?;
        if read == 0 {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err(ReadError::Malformed("connection closed mid-head"))
            };
        }
        let blank = line.iter().all(|byte| matches!(byte, b'\r' | b'\n'));
        head.extend_from_slice(&line);
        if blank {
            break;
        }
        if head.len() >= MAX_HEAD_BYTES {
            return Err(ReadError::Malformed("request head too large"));
        }
    }

    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.lines();
    let request_line = lines
        .next()
        .ok_or(ReadError::Malformed("no request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ReadError::Malformed("no method"))?
        .to_string();
    let target = parts.next().ok_or(ReadError::Malformed("no target"))?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(raw_path);
    let query = parse_query(raw_query);

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(ReadError::Malformed("too many headers"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadError::Malformed("header without a colon"));
        };
        // Repeated headers keep the first value. Nothing this server reads is
        // legitimately repeatable, and last-wins is the shape that lets a
        // smuggled duplicate override the real one.
        headers
            .entry(name.trim().to_ascii_lowercase())
            .or_insert_with(|| value.trim().to_string());
    }

    if headers.contains_key("transfer-encoding") {
        return Err(ReadError::Malformed("chunked requests are not served"));
    }

    let length: usize = match headers.get("content-length") {
        Some(value) => value
            .parse()
            .map_err(|_| ReadError::Malformed("bad content-length"))?,
        None => 0,
    };
    if length > MAX_BODY_BYTES {
        return Err(ReadError::Malformed("request body too large"));
    }
    let mut body = vec![0_u8; length];
    if length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let keep_alive = match headers.get("connection").map(String::as_str) {
        Some(value) if value.eq_ignore_ascii_case("close") => false,
        Some(value) if value.eq_ignore_ascii_case("keep-alive") => true,
        _ => version != "HTTP/1.0",
    };

    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
        keep_alive,
    }))
}

async fn read_line_bounded<R>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
    budget: usize,
) -> Result<usize, ReadError>
where
    R: AsyncRead + Unpin,
{
    let read = reader
        .take(budget as u64 + 1)
        .read_until(b'\n', line)
        .await?;
    if read > budget {
        return Err(ReadError::Malformed("request head too large"));
    }
    Ok(read)
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub extra_headers: Vec<(String, String)>,
}

impl Response {
    #[must_use]
    pub fn json(value: &serde_json::Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&serde_json::json!({ "error": message }))
                .unwrap_or_else(|_| b"{}".to_vec()),
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn text(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn redirect(location: &str) -> Self {
        Self {
            status: 303,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            extra_headers: vec![("Location".into(), location.into())],
        }
    }

    #[must_use]
    pub fn with_header(mut self, name: &str, value: String) -> Self {
        self.extra_headers.push((name.to_string(), value));
        self
    }
}

/// Writes a complete response. Returns whether the connection may be reused.
pub async fn write_response<W>(
    writer: &mut W,
    request: Option<&Request>,
    response: Response,
) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let keep_alive = request.is_some_and(|request| request.keep_alive) && response.status != 500;
    let mut head = String::new();
    let _ = write!(
        head,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len(),
        if keep_alive { "keep-alive" } else { "close" },
    );
    for (name, value) in &response.extra_headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    // The frontend is a control surface for live agents. A cached session list
    // is worse than a slow one.
    head.push_str("Cache-Control: no-store\r\n");
    head.push_str(SECURITY_HEADERS);
    head.push_str("\r\n");

    writer.write_all(head.as_bytes()).await?;
    let head_only = request.is_some_and(|request| request.method == "HEAD");
    if !head_only {
        writer.write_all(&response.body).await?;
    }
    writer.flush().await?;
    Ok(keep_alive)
}

/// The page is same-origin, self-contained, and must never be framed by
/// anything: it can kill sessions.
const SECURITY_HEADERS: &str = concat!(
    "X-Content-Type-Options: nosniff\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "X-Frame-Options: DENY\r\n",
    "Content-Security-Policy: default-src 'none'; ",
    "script-src 'unsafe-inline'; style-src 'unsafe-inline'; ",
    // `manifest-src` has no fallback to anything but `default-src`, so
    // omitting it blocks the web-app manifest and silently costs the page its
    // add-to-home-screen identity.
    "connect-src 'self'; img-src 'self' data:; manifest-src 'self'; ",
    "base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n",
);

/// Opens a `text/event-stream`. The caller then writes events with
/// [`write_event`] until the peer disappears.
pub async fn begin_event_stream<W>(writer: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\
         X-Accel-Buffering: no\r\n",
    );
    head.push_str(SECURITY_HEADERS);
    head.push_str("\r\n");
    writer.write_all(head.as_bytes()).await?;
    writer.flush().await
}

pub async fn write_event<W>(
    writer: &mut W,
    event: &str,
    data: &serde_json::Value,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".into());
    // SSE frames are newline-delimited, so a payload containing a newline
    // would forge a frame boundary. `serde_json` escapes them, but the guard
    // is cheap and this is the one place it would matter.
    let payload = payload.replace('\n', " ");
    writer
        .write_all(format!("event: {event}\ndata: {payload}\n\n").as_bytes())
        .await?;
    writer.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    }
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &str) -> Result<Option<Request>, ReadError> {
        let mut reader = BufReader::new(raw.as_bytes());
        read_request(&mut reader).await
    }

    #[tokio::test]
    async fn parses_a_get_with_query_and_cookie() {
        let request = parse(
            "GET /api/session/s_1/screen?lines=40 HTTP/1.1\r\n\
             Host: forge\r\nCookie: diri_token=abc; other=1\r\n\r\n",
        )
        .await
        .expect("read")
        .expect("request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/session/s_1/screen");
        assert_eq!(request.query.get("lines").map(String::as_str), Some("40"));
        assert_eq!(request.cookie("diri_token").as_deref(), Some("abc"));
        assert_eq!(request.segments(), vec!["api", "session", "s_1", "screen"]);
    }

    #[tokio::test]
    async fn reads_a_body_of_the_declared_length() {
        let request = parse("POST /api/spawn HTTP/1.1\r\nContent-Length: 9\r\n\r\n{\"a\":1234}")
            .await
            .expect("read")
            .expect("request");
        assert_eq!(request.body, b"{\"a\":1234".to_vec());
    }

    #[tokio::test]
    async fn percent_escapes_survive_the_path() {
        let request = parse("GET /api/x?cwd=%2Fhome%2Fcristi%2Fcode+two HTTP/1.1\r\n\r\n")
            .await
            .expect("read")
            .expect("request");
        assert_eq!(
            request.query.get("cwd").map(String::as_str),
            Some("/home/cristi/code two")
        );
    }

    #[tokio::test]
    async fn an_empty_connection_is_not_an_error() {
        assert!(parse("").await.expect("read").is_none());
    }

    #[tokio::test]
    async fn chunked_bodies_are_refused_rather_than_misread() {
        let error = parse("POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .expect_err("must refuse");
        assert!(matches!(error, ReadError::Malformed(_)));
    }

    #[tokio::test]
    async fn an_oversized_head_is_refused_before_it_is_buffered() {
        let padding = "x".repeat(MAX_HEAD_BYTES + 16);
        let error = parse(&format!("GET /?a={padding} HTTP/1.1\r\n\r\n"))
            .await
            .expect_err("must refuse");
        assert!(matches!(error, ReadError::Malformed(_)));
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_before_it_is_allocated() {
        let error = parse(&format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        ))
        .await
        .expect_err("must refuse");
        assert!(matches!(error, ReadError::Malformed(_)));
    }

    #[tokio::test]
    async fn a_duplicated_header_keeps_the_first_value() {
        let request =
            parse("GET /x HTTP/1.1\r\nAuthorization: real\r\nAuthorization: smuggled\r\n\r\n")
                .await
                .expect("read")
                .expect("request");
        assert_eq!(request.header("authorization"), Some("real"));
    }

    #[tokio::test]
    async fn responses_carry_the_frame_and_sniffing_guards() {
        let mut out = Vec::new();
        write_response(
            &mut out,
            None,
            Response::json(&serde_json::json!({"ok": true})),
        )
        .await
        .expect("write");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("X-Frame-Options: DENY"));
        assert!(text.contains("X-Content-Type-Options: nosniff"));
        assert!(text.contains("frame-ancestors 'none'"));
        assert!(text.ends_with("{\"ok\":true}"));
    }

    #[tokio::test]
    async fn a_head_request_gets_the_headers_without_the_body() {
        let request = parse("HEAD /api/health HTTP/1.1\r\n\r\n")
            .await
            .expect("read")
            .expect("request");
        let mut out = Vec::new();
        write_response(
            &mut out,
            Some(&request),
            Response::json(&serde_json::json!({"ok": true})),
        )
        .await
        .expect("write");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("Content-Length: 11"));
        assert!(!text.contains("{\"ok\":true}"));
    }

    #[tokio::test]
    async fn a_newline_in_an_event_payload_cannot_forge_a_frame() {
        let mut out = Vec::new();
        write_event(
            &mut out,
            "session.updated",
            &serde_json::json!({ "title": "line\nevent: spoofed" }),
        )
        .await
        .expect("write");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<&str> = text.split('\n').collect();
        // "event: …", "data: …", then the blank line that ends the frame. The
        // injected text must be *inside* the data line, never at the start of
        // one, which is the only place a reader looks for a field name.
        assert_eq!(lines.len(), 4, "payload newline forged a line: {text:?}");
        assert_eq!(lines[0], "event: session.updated");
        assert!(lines[1].starts_with("data: "));
        assert!(lines[1].contains("event: spoofed"));
        assert_eq!(lines[2], "");
    }
}
