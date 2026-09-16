//! Deadline-bound local Unix socket operations for on-demand management.
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

pub fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|value| !value.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "local request deadline expired"))
}
fn pause_until(deadline: Instant) {
    std::thread::sleep(
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(10)),
    );
}

/// The management deadline includes socket admission. A blocking connect can
/// otherwise wait indefinitely behind a full local listen backlog.
pub fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    remaining(deadline)?;
    // SAFETY: all-zero sockaddr_un is valid storage before initializing fields.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Holder socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: socket takes only integer constants and returns an owned fd.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful socket call returned a new owned descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: owned holds a live fd; these commands take integer flag values.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(owned);
    stream.set_nonblocking(true)?;
    loop {
        remaining(deadline)?;
        // SAFETY: address is initialized, with a valid bounded length, and
        // stream owns raw throughout this synchronous connect attempt.
        if unsafe { libc::connect(raw, (&raw const address).cast(), length) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                pause_until(deadline);
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                let mut descriptor = libc::pollfd {
                    fd: raw,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let timeout = remaining(deadline)?
                    .as_millis()
                    .max(1)
                    .min(i32::MAX as u128) as i32;
                // SAFETY: descriptor is live initialized storage for one entry.
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                remaining(deadline)?;
                if ready == 0 {
                    continue;
                }
                if let Some(error) = stream.take_error()? {
                    return Err(error);
                }
                if descriptor.revents & libc::POLLOUT != 0 {
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Holder socket closed during connect",
                ));
            }
            _ => return Err(error),
        }
    }
    remaining(deadline)?;
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Reads one bounded response, leaving the stream nonblocking. Queued bytes
/// remain readable after peer close; no socket timeout mutation is needed.
pub fn read_line_until(
    stream: &mut UnixStream,
    deadline: Instant,
    limit: usize,
) -> io::Result<Vec<u8>> {
    stream.set_nonblocking(true)?;
    let mut output = Vec::with_capacity(limit.min(1024));
    let mut buffer = [0u8; 1024];
    loop {
        remaining(deadline)?;
        match stream.read(&mut buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "local reply ended before its newline",
                ));
            }
            Ok(count) => {
                let end = buffer[..count].iter().position(|byte| *byte == b'\n');
                let bytes = &buffer[..end.unwrap_or(count)];
                if output.len() + bytes.len() > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "local reply exceeds bound",
                    ));
                }
                output.extend_from_slice(bytes);
                remaining(deadline)?;
                if end.is_some() {
                    return Ok(output);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                let timeout = remaining(deadline)?.as_millis().clamp(1, i32::MAX as u128) as i32;
                let mut descriptor = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one initialized descriptor, live for this bounded wait.
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                if descriptor.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "local reply socket invalid",
                    ));
                }
                // HUP/ERR still require draining bytes already queued before EOF.
                remaining(deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn queued_reply_is_readable_after_peer_has_closed() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"complete\n").unwrap();
        drop(writer);
        // macOS rejects SO_RCVTIMEO after peer close despite queued bytes.
        assert_eq!(
            read_line_until(&mut reader, Instant::now() + Duration::from_secs(1), 100).unwrap(),
            b"complete"
        );
    }

    #[test]
    fn reply_deadline_covers_partial_reads_and_bound() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_millis(25);
        let producer = std::thread::spawn(move || {
            for _ in 0..20 {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        assert_eq!(
            read_line_until(&mut reader, deadline, 100)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        drop(reader);
        producer.join().unwrap();
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"too long\n").unwrap();
        assert_eq!(
            read_line_until(&mut reader, Instant::now() + Duration::from_secs(1), 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"ready\n").unwrap();
        assert_eq!(
            read_line_until(&mut reader, Instant::now(), 100)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
