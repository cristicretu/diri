//! Local byte streams. Windows AF_UNIX endpoints live in a private, short namespace.
#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{UnixListener, UnixStream};

#[cfg(all(feature = "async", unix))]
pub mod asynchronous {
    pub use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    pub use tokio::net::{UnixListener, UnixStream};
}
#[cfg(all(feature = "async", windows))]
pub mod asynchronous;

use std::io;
use std::time::Duration;

/// Wait for socket readiness without changing nonblocking or timeout settings.
pub fn wait(stream: &UnixStream, write: bool, timeout: Option<Duration>) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut entry = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: if write { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        };
        let millis = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        // SAFETY: a live socket and one initialized poll entry.
        let result = unsafe { libc::poll(&mut entry, 1, millis) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result != 0)
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::*;
        let mut entry = WSAPOLLFD {
            fd: stream.as_raw_socket() as usize,
            events: if write { POLLWRNORM } else { POLLRDNORM },
            revents: 0,
        };
        let millis = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        // SAFETY: a live Winsock stream and one initialized poll entry.
        let result = unsafe { WSAPoll(&mut entry, 1, millis) };
        if result < 0 {
            Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
        } else {
            Ok(result != 0)
        }
    }
}
