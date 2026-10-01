//! Readiness for byte streams, independent of POSIX fd vs Winsock SOCKET.
use std::io;
#[cfg(unix)]
pub type RawIo = std::os::fd::RawFd;
#[cfg(windows)]
pub type RawIo = isize;
#[cfg(unix)]
pub use std::os::fd::OwnedFd as OwnedIo;
#[cfg(windows)]
pub use std::os::windows::io::OwnedSocket as OwnedIo;

pub trait AsRawIo {
    fn as_raw_io(&self) -> RawIo;
}
#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> AsRawIo for T {
    fn as_raw_io(&self) -> RawIo {
        self.as_raw_fd()
    }
}
#[cfg(windows)]
impl<T: std::os::windows::io::AsRawSocket> AsRawIo for T {
    fn as_raw_io(&self) -> RawIo {
        self.as_raw_socket() as RawIo
    }
}

#[cfg(unix)]
pub use libc::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT, pollfd as PollFd};
#[cfg(windows)]
pub use windows_sys::Win32::Networking::WinSock::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};
#[cfg(windows)]
#[repr(C)]
pub struct PollFd {
    pub fd: RawIo,
    pub events: i16,
    pub revents: i16,
}

/// # Safety
/// `entries` must address `count` initialized writable entries, whose handles
/// stay open until this call returns. Negative handles are ignored.
pub unsafe fn poll(entries: *mut PollFd, count: usize, millis: i32) -> i32 {
    #[cfg(unix)]
    unsafe {
        libc::poll(entries, count as _, millis)
    }
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Networking::WinSock::WSAPoll(entries.cast(), count as u32, millis)
    }
}

#[cfg(unix)]
pub fn duplicate(value: &impl std::os::fd::AsFd) -> io::Result<OwnedIo> {
    value.as_fd().try_clone_to_owned()
}
#[cfg(windows)]
pub fn duplicate(value: &impl std::os::windows::io::AsSocket) -> io::Result<OwnedIo> {
    value.as_socket().try_clone_to_owned()
}

pub fn set_nonblocking(value: &impl AsRawIo, enabled: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let fd = value.as_raw_io();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if enabled {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::*;
        let mut enabled = u32::from(enabled);
        if unsafe { ioctlsocket(value.as_raw_io() as usize, FIONBIO, &mut enabled) } != 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
    }
    Ok(())
}
