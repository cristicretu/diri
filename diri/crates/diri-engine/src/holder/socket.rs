//! Blocking NDJSON-over-UDS plumbing shared by holder clients and servers.

#[cfg(unix)]
use diri_platform::ipc::UnixListener;
use diri_platform::ipc::UnixStream;
use std::io::{Read, Write};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{HolderError, HolderResult};

/// A request or response line must fit in this. Matches the Swift limit.
const LINE_LIMIT: usize = 16 << 20;

pub fn connect(path: &Path) -> HolderResult<UnixStream> {
    UnixStream::connect(path).map_err(|error| HolderError::io("connect", error))
}

/// Kernel buffering for a socket carrying a session's output stream, each way.
///
/// macOS gives an AF_UNIX stream 8 KiB, so a stream of output moved 8 KiB per
/// wakeup of each end: the sender blocked, the receiver drained, and both went
/// round again. A quarter megabyte lets a burst cross in one pass, and costs
/// nothing while idle, since the kernel holds only bytes in flight.
pub const OUTPUT_SOCKET_BUFFER: usize = 256 << 10;

/// Raises `SO_SNDBUF` or `SO_RCVBUF` on `stream`. Best effort: a kernel that
/// refuses keeps its default, which is slower but correct.
#[cfg(unix)]
pub fn set_buffer(stream: &UnixStream, option: libc::c_int, bytes: usize) {
    use std::os::fd::AsRawFd;
    let size = libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
    // SAFETY: a live socket fd and a correctly sized option value.
    let _ = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        )
    };
}

/// Binds an owner-only listening socket, replacing any stale file at `path`.
#[cfg(unix)]
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
#[cfg(unix)]
pub fn accept_raw(
    listen_fd: i32,
    finished: impl Fn() -> bool,
) -> HolderResult<Option<diri_platform::ipc::UnixStream>> {
    loop {
        // SAFETY: accept(2) on a listening fd; the addr out-params are unused.
        let client = unsafe { libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if client >= 0 {
            // SAFETY: a fresh fd accept just handed us; the stream owns it.
            return Ok(Some(unsafe {
                use std::os::fd::FromRawFd;
                diri_platform::ipc::UnixStream::from_raw_fd(client)
            }));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if finished() {
            return Ok(None);
        }
        // Never leave the accept loop while the listener is live: a manager
        // that returns exits and its guard SIGKILLs every agent it hosts, and
        // a holder that returns strands its still-running agent. The usual
        // cause, descriptor exhaustion, clears once something closes.
        let delay = crate::limits::accept_retry_delay(&error);
        eprintln!("diri-holder: accept: {error}; retrying in {delay:?}");
        std::thread::sleep(delay);
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

#[cfg(windows)]
pub fn set_buffer(stream: &UnixStream, option: i32, bytes: usize) {
    use diri_platform::windows_sys::Win32::Networking::WinSock::*;
    use std::os::windows::io::AsRawSocket;
    let value = bytes.min(i32::MAX as usize) as i32;
    // SAFETY: a live socket and exactly sized integer option.
    unsafe {
        setsockopt(
            stream.as_raw_socket() as usize,
            SOL_SOCKET,
            option,
            (&raw const value).cast(),
            size_of_val(&value) as i32,
        );
    }
}
#[cfg(unix)]
pub const RECEIVE_BUFFER: i32 = libc::SO_RCVBUF;
#[cfg(windows)]
pub const RECEIVE_BUFFER: i32 = diri_platform::windows_sys::Win32::Networking::WinSock::SO_RCVBUF;
