//! Blocking NDJSON-over-UDS plumbing shared by holder clients and servers.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{HolderError, HolderResult};

/// A request or response line must fit in this. Matches the Swift limit.
const LINE_LIMIT: usize = 16 << 20;

pub fn connect(path: &Path) -> HolderResult<UnixStream> {
    UnixStream::connect(path).map_err(|error| HolderError::io("connect", error))
}

/// Binds an owner-only listening socket, replacing any stale file at `path`.
pub fn listen(path: &Path) -> HolderResult<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|error| HolderError::io("bind", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

/// Reads until a newline or EOF; the newline is not included. EOF before any
/// newline returns what arrived, as the Swift `readLine` does.
pub fn read_line(stream: &mut impl Read) -> HolderResult<Vec<u8>> {
    let mut result = Vec::new();
    let mut chunk = [0u8; 4096];
    while result.len() < LINE_LIMIT {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(result),
            Ok(count) => {
                if let Some(newline) = chunk[..count].iter().position(|&byte| byte == b'\n') {
                    result.extend_from_slice(&chunk[..newline]);
                    return Ok(result);
                }
                result.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(HolderError::io("read", error)),
        }
    }
    Err(HolderError::Transport(format!(
        "NDJSON line exceeds {LINE_LIMIT} bytes"
    )))
}

pub fn write_json_line<T: Serialize>(stream: &mut impl Write, value: &T) -> HolderResult<()> {
    let mut encoded = serde_json::to_vec(value)
        .map_err(|error| HolderError::Transport(format!("encode: {error}")))?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .map_err(|error| HolderError::io("write", error))
}

/// Accepts one client on a raw listening fd, or returns `None` when the fd
/// has been shut down/closed by the owner's finish path.
///
/// Raw rather than `UnixListener::incoming` because of teardown: macOS never
/// wakes an `accept(2)` blocked on an AF_UNIX listener via `shutdown` alone —
/// the fd must also be closed, which means the accept loop cannot hold a safe
/// owner of it. The Swift holder shipped this exact shape.
pub fn accept_raw(
    listen_fd: i32,
    finished: impl Fn() -> bool,
) -> HolderResult<Option<std::os::unix::net::UnixStream>> {
    loop {
        // SAFETY: accept(2) on a listening fd; the addr out-params are unused.
        let client = unsafe { libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if client >= 0 {
            // SAFETY: a fresh fd accept just handed us; the stream owns it.
            return Ok(Some(unsafe {
                use std::os::fd::FromRawFd;
                std::os::unix::net::UnixStream::from_raw_fd(client)
            }));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if finished() {
            return Ok(None);
        }
        return Err(HolderError::io("accept", error));
    }
}

pub fn read_json_line<T: DeserializeOwned>(stream: &mut impl Read) -> HolderResult<T> {
    let line = read_line(stream)?;
    serde_json::from_slice(&line).map_err(|error| {
        HolderError::InvalidRequest(format!(
            "decode: {error} in {}",
            String::from_utf8_lossy(&line[..line.len().min(200)])
        ))
    })
}
