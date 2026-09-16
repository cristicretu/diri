//! Account database calls can block in directory services. They run only in a
//! short-lived worker; its caller owns timeout, output bounds and reaping.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock};
mod reap;
use reap::{Budget, Permit};
use std::time::{Duration, Instant};

use diri_proto::process_facts::{
    MAX_ACCOUNT_REPLY_BYTES, ProcessAccount, ProcessValue, UnavailableReason, decode_account_reply,
    encode_account_reply,
};

pub const WORKER_FLAG: &str = "--account-facts";
const TIMEOUT: Duration = Duration::from_millis(250);
const MAX_TIMEOUT: Duration = Duration::from_secs(1);
static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| Arc::new(Budget::default()));

pub fn parse_uid(raw: &str) -> io::Result<u32> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account UID must be an unsigned decimal integer",
        ));
    }
    raw.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "account UID exceeds native range",
        )
    })
}

/// Called by the narrow hidden mode in the existing Rust helper binaries.
/// This function may block; never call it inside an Engine or Holder loop.
pub fn run_worker(uid: u32, output: &mut (impl Write + ?Sized)) -> io::Result<()> {
    let encoded = encode_account_reply(super::value(native_account(uid))).or_else(|_| {
        encode_account_reply(ProcessValue::unavailable(UnavailableReason::InvalidData))
    })?;
    output.write_all(&encoded)
}

pub fn lookup(executable: &Path, uid: u32) -> ProcessValue<ProcessAccount> {
    lookup_until(executable, uid, Instant::now() + TIMEOUT)
}

/// Compose with a caller deadline. Even a longer requested deadline is capped
/// at one second; the interactive default remains 250 ms.
pub fn lookup_until(
    executable: &Path,
    uid: u32,
    deadline: Instant,
) -> ProcessValue<ProcessAccount> {
    let Some(permit) = BUDGET.acquire() else {
        return ProcessValue::unavailable(UnavailableReason::Busy);
    };
    let mut command = Command::new(executable);
    command.arg(WORKER_FLAG).arg(uid.to_string()).env_clear();
    // A timed-out worker keeps this permit through asynchronous reap. New
    // requests cannot accumulate unbounded workers behind stalled cleanup.
    match lookup_command_admitted(
        command,
        uid,
        deadline.min(Instant::now() + MAX_TIMEOUT),
        permit,
    ) {
        Ok(result) => result,
        Err(error) => super::value::<ProcessAccount>(Err(error)),
    }
}

struct OwnedWorker {
    child: Option<Child>,
    permit: Option<Permit>,
}
impl Drop for OwnedWorker {
    fn drop(&mut self) {
        let mut child = self.child.take().expect("owned worker");
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = child.kill();
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        self.permit.take().expect("worker admission").reap(child);
    }
}

#[cfg(test)]
fn lookup_command(
    command: Command,
    uid: u32,
    deadline: Instant,
) -> io::Result<ProcessValue<ProcessAccount>> {
    lookup_command_admitted(
        command,
        uid,
        deadline,
        Arc::new(Budget::default()).acquire().unwrap(),
    )
}

fn lookup_command_admitted(
    mut command: Command,
    uid: u32,
    deadline: Instant,
    permit: Permit,
) -> io::Result<ProcessValue<ProcessAccount>> {
    let check_deadline = || {
        if Instant::now() >= deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "account lookup deadline expired",
            ))
        } else {
            Ok(())
        }
    };
    check_deadline()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = OwnedWorker {
        child: Some(command.spawn()?),
        permit: Some(permit),
    };
    let mut output = child
        .child
        .as_mut()
        .expect("owned worker")
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("account worker stdout missing"))?;
    let fd = output.as_raw_fd();
    // SAFETY: output owns fd, and fcntl reads/modifies integer descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    let mut eof = false;
    loop {
        check_deadline()?;
        while !eof {
            check_deadline()?;
            match output.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(count) => {
                    if bytes.len() + count > MAX_ACCOUNT_REPLY_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "account worker output exceeds bound",
                        ));
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if let Some(status) = child.child.as_mut().expect("owned worker").try_wait()? {
            check_deadline()?;
            if !status.success() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "account worker unavailable",
                ));
            }
            if eof {
                return decode_account_reply(&bytes, uid);
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if eof {
            // The worker has closed stdout but has not exited. Bound this
            // request-only reap wait; no timer survives the lookup.
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
        } else {
            let mut descriptor = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
            // SAFETY: descriptor is initialized storage for one live fd.
            let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

fn native_account(uid: u32) -> io::Result<ProcessAccount> {
    // Fixed upper bound. A larger NSS record is explicitly unavailable rather
    // than allocating according to an untrusted directory-service response.
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: passwd is plain SDK data, initialized before the native call.
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: both output pointers and the exactly bounded scratch buffer live
    // throughout the call. This potentially blocking call runs only in worker.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut record,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if result.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "account unavailable",
        ));
    }
    if result != &raw mut record || record.pw_uid != uid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "account UID mismatch",
        ));
    }
    Ok(ProcessAccount {
        uid,
        name: buffer_string(&buffer, record.pw_name)?,
        home_directory: buffer_string(&buffer, record.pw_dir)?,
    })
}

fn buffer_string(buffer: &[u8], pointer: *const libc::c_char) -> io::Result<String> {
    let start = (pointer as usize)
        .checked_sub(buffer.as_ptr() as usize)
        .filter(|start| *start < buffer.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "account string lies outside reply buffer",
            )
        })?;
    let tail = &buffer[start..];
    let end = tail.iter().position(|byte| *byte == 0).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "account string is unterminated")
    })?;
    String::from_utf8(tail[..end].to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "account string is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn account_worker_rejects_expired_deadline_stall_and_oversized_output() {
        assert_eq!(
            lookup_command(shell("exit 0"), 42, Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let start = Instant::now();
        assert_eq!(
            lookup_command(
                shell("exec /bin/sleep 1"),
                42,
                start + Duration::from_millis(20)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_millis(500));
        let error = lookup_command(
            shell("i=0; while [ $i -lt 1000 ]; do printf 0123456789; i=$((i+1)); done"),
            42,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn account_worker_decodes_only_requested_uid_and_supported_version() {
        let valid = "printf '%s' '{\"version\":1,\"result\":{\"status\":\"available\",\"value\":{\"uid\":42,\"name\":\"fixture\",\"homeDirectory\":\"/fixture\"}}}'";
        assert!(matches!(
            lookup_command(shell(valid), 42, Instant::now() + Duration::from_secs(2)).unwrap(),
            ProcessValue::Available { .. }
        ));
        assert!(lookup_command(shell(valid), 43, Instant::now() + Duration::from_secs(2)).is_err());
        assert!(
            lookup_command(
                shell(&valid.replace("version\":1", "version\":2")),
                42,
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
        );
    }

    #[test]
    fn account_strings_are_checked_inside_the_native_buffer() {
        let buffer = b"fixture\0/fixture\0";
        assert_eq!(
            buffer_string(buffer, buffer.as_ptr().cast()).unwrap(),
            "fixture"
        );
        assert!(buffer_string(buffer, std::ptr::null()).is_err());
        assert!(buffer_string(b"no terminator", b"elsewhere".as_ptr().cast()).is_err());
    }
}
