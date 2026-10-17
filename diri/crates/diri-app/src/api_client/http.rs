//! Sends one request through the system `curl`, off the main thread.
//!
//! Why `curl` and not an in-process client: the workspace links no HTTP or
//! TLS stack, and adding one (hyper + rustls, or reqwest) to a GPUI app brings
//! a large dependency tree and its own certificate story. diri already sends
//! its HTTPS through the system `curl` (`diri-updater::net`, usage limits), which
//! uses the system trust store, speaks HTTP/1.1 and HTTP/2, follows
//! redirects, and decompresses. This module drives it the same hardened way:
//!
//! - `-q` first, so no `~/.curlrc` changes what is sent;
//! - every option, the URL and every header travel over stdin (`-K -`),
//!   never the argument list, where any local user could read them;
//! - a body goes through an owner-only temp file that is deleted after;
//! - only `http` and `https` may be requested or redirected to;
//! - the response body is read with a hard cap and the transfer killed past
//!   it; cancelling (or dropping) the send kills `curl`.
//!
//! Nothing here logs: headers and bodies routinely carry tokens.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use super::model::Method;

/// Response bodies past this are cut off and flagged as truncated.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Headers past this are not read (a header flood, not an API).
const MAX_HEADER_BYTES: u64 = 256 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT_SECONDS: u32 = 10;
const MAX_REDIRECTS: u32 = 10;
/// curl prints this line on stderr after the transfer (`write-out`).
const STATS_MARKER: &str = "diri-api-stats:";

/// A request with every `{{variable}}` already filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    pub follow_redirects: bool,
    pub max_body: usize,
}

impl Outgoing {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: DEFAULT_TIMEOUT,
            follow_redirects: true,
            max_body: MAX_RESPONSE_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    /// `HTTP/1.1`, `HTTP/2`, …
    pub version: String,
    pub reason: String,
    /// The final response's headers, in the order they came.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The body was longer than the cap and was cut off.
    pub truncated: bool,
    pub took_ms: u64,
    /// Bytes received for the body, before any cut.
    pub size: u64,
    pub redirects: u32,
    pub final_url: String,
}

impl ApiResponse {
    #[cfg(test)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The request could not be written as asked (bad URL, header, …).
    Invalid(String),
    /// `curl` is not installed or could not start.
    Unavailable,
    Resolve,
    Connect,
    Timeout,
    Tls,
    TooManyRedirects,
    Cancelled,
    /// Any other transfer failure, by curl's exit code.
    Transfer(i32),
}

impl HttpError {
    /// What the response pane says. Never echoes the URL or headers back.
    pub fn message(&self) -> String {
        match self {
            HttpError::Invalid(reason) => reason.clone(),
            HttpError::Unavailable => "curl isn’t installed".to_owned(),
            HttpError::Resolve => "Couldn’t find that host".to_owned(),
            HttpError::Connect => "Couldn’t connect — is the server running?".to_owned(),
            HttpError::Timeout => "The request timed out".to_owned(),
            HttpError::Tls => "The secure connection failed (certificate or TLS)".to_owned(),
            HttpError::TooManyRedirects => "Too many redirects".to_owned(),
            HttpError::Cancelled => "Cancelled".to_owned(),
            HttpError::Transfer(code) => format!("The request failed (curl error {code})"),
        }
    }
}

/// A value inside a double-quoted curl config string. Callers have already
/// refused control characters, so only quotes and backslashes need escaping.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn refuse_controls(what: &str, value: &str) -> Result<(), HttpError> {
    if value.chars().any(|c| c.is_control() && c != '\t') {
        return Err(HttpError::Invalid(format!(
            "{what} contains a line break or control character"
        )));
    }
    Ok(())
}

/// The `-K -` config for `outgoing`, writing headers to `header_file` and the
/// body from `body_file`.
fn config(
    outgoing: &Outgoing,
    header_file: &Path,
    body_file: Option<&Path>,
) -> Result<String, HttpError> {
    let url = outgoing.url.trim();
    refuse_controls("The URL", url)?;
    if url.contains(char::is_whitespace) {
        return Err(HttpError::Invalid("The URL contains spaces".to_owned()));
    }
    let scheme = url.split_once("://").map(|(scheme, _)| scheme);
    if !matches!(
        scheme.map(str::to_ascii_lowercase).as_deref(),
        Some("http" | "https")
    ) {
        return Err(HttpError::Invalid(
            "Only http:// and https:// addresses can be sent".to_owned(),
        ));
    }
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", quoted(url)));
    match outgoing.method {
        // `-X HEAD` waits for a body that never comes; `--head` does not.
        Method::Head => config.push_str("head\n"),
        method => config.push_str(&format!("request = \"{}\"\n", method.name())),
    }
    for (name, value) in &outgoing.headers {
        diri_proto::api_request::valid_header_name(name).map_err(HttpError::Invalid)?;
        refuse_controls("A header", value)?;
        // `Name:` would remove curl's own header; `Name;` sends it empty.
        let line = if value.is_empty() {
            format!("{name};")
        } else {
            format!("{name}: {value}")
        };
        config.push_str(&format!("header = {}\n", quoted(&line)));
    }
    if let Some(path) = body_file {
        config.push_str(&format!(
            "data-binary = {}\n",
            quoted(&format!("@{}", path.display()))
        ));
    }
    config.push_str("silent\nshow-error\ncompressed\n");
    config.push_str("proto = \"=http,https\"\nproto-redir = \"=http,https\"\n");
    if outgoing.follow_redirects {
        config.push_str(&format!("location\nmax-redirs = {MAX_REDIRECTS}\n"));
    }
    config.push_str(&format!(
        "max-time = {}\n",
        outgoing.timeout.as_secs().max(1)
    ));
    config.push_str(&format!("connect-timeout = {CONNECT_TIMEOUT_SECONDS}\n"));
    config.push_str(&format!(
        "user-agent = \"diri/{}\"\n",
        env!("CARGO_PKG_VERSION")
    ));
    config.push_str(&format!(
        "dump-header = {}\n",
        quoted(&header_file.display().to_string())
    ));
    config.push_str(&format!(
        "write-out = \"%{{stderr}}{STATS_MARKER}%{{http_code}} %{{time_total}} %{{size_download}} %{{num_redirects}} %{{url_effective}}\\n\"\n"
    ));
    Ok(config)
}

/// The last response's status line and headers from a `--dump-header` file,
/// which holds one block per hop (redirects, `100 Continue`).
pub fn parse_headers(dump: &str) -> (String, u16, String, Vec<(String, String)>) {
    let mut version = String::new();
    let mut status = 0;
    let mut reason = String::new();
    let mut headers = Vec::new();
    for line in dump.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("HTTP/") {
            let mut parts = line.splitn(3, ' ');
            version = parts.next().unwrap_or_default().to_owned();
            status = parts.next().and_then(|code| code.parse().ok()).unwrap_or(0);
            reason = parts.next().unwrap_or_default().trim().to_owned();
            headers.clear();
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    (version, status, reason, headers)
}

fn failure(code: Option<i32>) -> HttpError {
    match code {
        Some(6 | 5) => HttpError::Resolve,
        Some(7) => HttpError::Connect,
        Some(28) => HttpError::Timeout,
        Some(35 | 51 | 53 | 54 | 58 | 59 | 60 | 64 | 66 | 77 | 80 | 82 | 83 | 90 | 91) => {
            HttpError::Tls
        }
        Some(47) => HttpError::TooManyRedirects,
        Some(1 | 3) => HttpError::Invalid("curl could not use that address".to_owned()),
        Some(code) => HttpError::Transfer(code),
        None => HttpError::Cancelled,
    }
}

/// An owner-only temp file holding `bytes`, removed on drop.
fn private_file(bytes: &[u8]) -> Result<tempfile::NamedTempFile, HttpError> {
    // tempfile creates files 0600 on Unix.
    let mut file = tempfile::Builder::new()
        .prefix("diri-api-")
        .tempfile()
        .map_err(|_| HttpError::Invalid("Couldn’t stage the request body".to_owned()))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|_| HttpError::Invalid("Couldn’t stage the request body".to_owned()))?;
    Ok(file)
}

/// Sends `outgoing`, stopping early (and killing `curl`) when `cancel`
/// resolves. Runs on the tokio runtime.
pub async fn send(
    outgoing: Outgoing,
    cancel: tokio::sync::oneshot::Receiver<()>,
) -> Result<ApiResponse, HttpError> {
    send_with(diri_platform::curl_executable(), outgoing, cancel).await
}

pub(crate) async fn send_with(
    curl: &Path,
    outgoing: Outgoing,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
) -> Result<ApiResponse, HttpError> {
    let header_file = private_file(b"")?;
    let body_file = outgoing.body.as_deref().map(private_file).transpose()?;
    let config = config(
        &outgoing,
        header_file.path(),
        body_file.as_ref().map(|file| file.path()),
    )?;
    let started = Instant::now();
    let mut child = Command::new(curl)
        .args(["-q", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| HttpError::Unavailable)?;
    {
        let mut stdin = child.stdin.take().ok_or(HttpError::Unavailable)?;
        stdin
            .write_all(config.as_bytes())
            .await
            .map_err(|_| HttpError::Unavailable)?;
    }
    let mut stdout = child.stdout.take().ok_or(HttpError::Unavailable)?;
    let mut stderr = child.stderr.take().ok_or(HttpError::Unavailable)?;
    let max_body = outgoing.max_body;

    let transfer = async {
        let mut body = Vec::new();
        let mut size = 0u64;
        let mut truncated = false;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let read = stdout.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            size += read as u64;
            let room = max_body.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..read.min(room)]);
            if read > room {
                truncated = true;
                break;
            }
        }
        (body, size, truncated)
    };
    let (body, size, truncated) = tokio::select! {
        result = transfer => result,
        _ = &mut cancel => {
            let _ = child.kill().await;
            return Err(HttpError::Cancelled);
        }
    };
    if truncated {
        let _ = child.kill().await;
    }
    let mut diagnostics = Vec::new();
    let status = tokio::select! {
        status = async {
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut diagnostics).await;
            child.wait().await
        } => status.map_err(|_| HttpError::Unavailable)?,
        _ = &mut cancel => {
            let _ = child.kill().await;
            return Err(HttpError::Cancelled);
        }
    };
    let took = started.elapsed();
    if !status.success() && !truncated {
        return Err(failure(status.code()));
    }
    // `--head` prints the headers where the body would go; they are already
    // in the dump file, and a HEAD answer has no body.
    let (body, truncated) = if outgoing.method == Method::Head {
        (Vec::new(), false)
    } else {
        (body, truncated)
    };
    let stats = String::from_utf8_lossy(&diagnostics);
    let stats = stats
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(STATS_MARKER))
        .unwrap_or_default()
        .to_owned();
    let mut fields = stats.splitn(5, ' ');
    let _code = fields.next();
    let took_ms = fields
        .next()
        .and_then(|seconds| seconds.parse::<f64>().ok())
        .map_or(took.as_millis() as u64, |seconds| {
            (seconds * 1000.0).round() as u64
        });
    let downloaded = fields.next().and_then(|bytes| bytes.parse::<u64>().ok());
    let redirects = fields
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    let final_url = fields.next().unwrap_or(&outgoing.url).trim().to_owned();

    let mut dump = String::new();
    if let Ok(file) = tokio::fs::File::open(header_file.path()).await {
        let _ = file.take(MAX_HEADER_BYTES).read_to_string(&mut dump).await;
    }
    let (version, status_code, reason, headers) = parse_headers(&dump);
    Ok(ApiResponse {
        status: status_code,
        version,
        reason,
        headers,
        body,
        truncated,
        took_ms,
        size: if truncated {
            size
        } else {
            downloaded.unwrap_or(size).max(size)
        },
        redirects,
        final_url,
    })
}

/// Standard words for a status, for when the server sent none (HTTP/2).
pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// `1.2 KB` style sizes.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `85 ms`, `1.24 s`.
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}
