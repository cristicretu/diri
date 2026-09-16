//! Bounded, on-demand native process observations on the owning host.
use std::io;

use diri_proto::process::ProcessIdentity;
use diri_proto::process_facts::{
    ProcessAccount, ProcessFacts, ProcessUserIds, ProcessValue, UnavailableReason,
};

pub mod account;

/// The caller supplies a bounded account lookup. Never inspect a remote PID
/// through this local function; invoke it inside the remote Helper instead.
pub fn inspect(
    identity: &ProcessIdentity,
    lookup: impl FnOnce(u32) -> ProcessValue<ProcessAccount>,
) -> io::Result<ProcessFacts> {
    crate::process_identity::inspect_verified(identity, || {
        let user_ids = value(user_ids(identity.pid()));
        let account = match &user_ids {
            ProcessValue::Available { value: ids } => match lookup(ids.effective) {
                ProcessValue::Available { value } if value.uid != ids.effective => {
                    ProcessValue::unavailable(UnavailableReason::InvalidData)
                }
                result => result,
            },
            ProcessValue::Unavailable { reason } => ProcessValue::unavailable(*reason),
        };
        Ok(ProcessFacts {
            identity: *identity,
            executable: value(executable(identity.pid())),
            working_directory: value(working_directory(identity.pid())),
            user_ids,
            account,
        })
    })
}

pub(crate) fn value<T>(result: io::Result<T>) -> ProcessValue<T> {
    match result {
        Ok(value) => ProcessValue::available(value),
        Err(error) => ProcessValue::unavailable(match error.kind() {
            io::ErrorKind::PermissionDenied => UnavailableReason::PermissionDenied,
            io::ErrorKind::NotFound => UnavailableReason::NotFound,
            io::ErrorKind::Unsupported => UnavailableReason::Unsupported,
            io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
                UnavailableReason::InvalidData
            }
            io::ErrorKind::TimedOut => UnavailableReason::TimedOut,
            _ => UnavailableReason::Io,
        }),
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn path_text(bytes: &[u8]) -> io::Result<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    if bytes.is_empty() || bytes.first() != Some(&b'/') {
        return Err(invalid("native process path is not absolute"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| invalid("native process path is not UTF-8"))
}

#[cfg(target_os = "linux")]
fn proc_link(pid: u32, name: &str) -> io::Result<String> {
    let path = std::ffi::CString::new(format!("/proc/{pid}/{name}")).unwrap();
    let mut buffer = [0u8; 16 * 1024];
    // SAFETY: path is terminated, buffer is live and exactly bounded.
    let length = unsafe { libc::readlink(path.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    let length = length as usize;
    if length == buffer.len() {
        return Err(invalid("native process path exceeds bound"));
    }
    path_text(&buffer[..length])
}

#[cfg(target_os = "linux")]
fn executable(pid: u32) -> io::Result<String> {
    proc_link(pid, "exe")
}

#[cfg(target_os = "linux")]
fn working_directory(pid: u32) -> io::Result<String> {
    proc_link(pid, "cwd")
}

#[cfg(target_os = "linux")]
fn user_ids(pid: u32) -> io::Result<ProcessUserIds> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(format!("/proc/{pid}/status"))?
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(invalid("oversized process status"));
    }
    parse_linux_user_ids(&bytes)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_user_ids(bytes: &[u8]) -> io::Result<ProcessUserIds> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| invalid("invalid process status encoding"))?;
    let mut lines = text.lines().filter_map(|line| line.strip_prefix("Uid:"));
    let fields: Vec<_> = lines
        .next()
        .ok_or_else(|| invalid("process UID missing"))?
        .split_whitespace()
        .collect();
    if fields.len() != 4 || lines.next().is_some() {
        return Err(invalid("invalid process UID fields"));
    }
    let ids: Vec<u32> = fields
        .iter()
        .map(|field| field.parse().map_err(|_| invalid("invalid process UID")))
        .collect::<io::Result<_>>()?;
    Ok(ProcessUserIds {
        real: ids[0],
        effective: ids[1],
    })
}

#[cfg(target_os = "macos")]
fn executable(pid: u32) -> io::Result<String> {
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the native API writes at most the supplied buffer size.
    let length =
        unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize >= buffer.len() {
        return Err(invalid("native executable path exceeds bound"));
    }
    path_text(&buffer[..length as usize])
}

#[cfg(target_os = "macos")]
fn working_directory(pid: u32) -> io::Result<String> {
    // SAFETY: native SDK-matching struct, initialized before the native call.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    // SAFETY: info is writable storage of exactly size bytes.
    let filled = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if filled != size {
        return Err(io::Error::last_os_error());
    }
    let bytes: Vec<_> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|byte| *byte as u8)
        .collect();
    if !bytes.contains(&0) {
        return Err(invalid("unterminated native working directory"));
    }
    path_text(&bytes)
}

#[cfg(target_os = "macos")]
fn user_ids(pid: u32) -> io::Result<ProcessUserIds> {
    // SAFETY: native SDK-matching struct, initialized before the native call.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    // SAFETY: info is writable storage of exactly size bytes.
    let filled = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if filled != size {
        return Err(io::Error::last_os_error());
    }
    if info.pbi_pid != pid {
        return Err(invalid("native UID PID mismatch"));
    }
    Ok(ProcessUserIds {
        real: info.pbi_ruid,
        effective: info.pbi_uid,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn executable(_: u32) -> io::Result<String> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn working_directory(_: u32) -> io::Result<String> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn user_ids(_: u32) -> io::Result<ProcessUserIds> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_ids_keep_real_and_effective_distinct() {
        assert_eq!(
            parse_linux_user_ids(b"Name:\tx\nUid:\t100 200 300 400\n").unwrap(),
            ProcessUserIds {
                real: 100,
                effective: 200
            }
        );
        for input in [
            "Uid: 1 2 3",
            "Uid: -1 2 3 4",
            "Uid: 1 2 3 4\nUid: 1 2 3 4",
            "Uid: 1 2 invalid 4",
        ] {
            assert!(parse_linux_user_ids(input.as_bytes()).is_err());
        }
    }

    #[test]
    fn paths_preserve_unicode_and_reject_lossy_or_relative_results() {
        assert_eq!(
            path_text("/tmp/界/é\0padding".as_bytes()).unwrap(),
            "/tmp/界/é"
        );
        assert!(path_text(b"relative").is_err());
        assert!(path_text(b"/tmp/\xff").is_err());
    }

    #[test]
    fn own_native_facts_are_identity_bound_and_account_uid_is_checked() {
        let identity = crate::process_identity::observe(std::process::id()).unwrap();
        let facts = inspect(&identity, |uid| {
            ProcessValue::available(ProcessAccount {
                uid: uid.wrapping_add(1),
                name: "synthetic".into(),
                home_directory: "/synthetic".into(),
            })
        })
        .unwrap();
        assert_eq!(facts.identity, identity);
        assert!(matches!(facts.executable, ProcessValue::Available { .. }));
        assert!(matches!(
            facts.working_directory,
            ProcessValue::Available { .. }
        ));
        assert_eq!(
            facts.account,
            ProcessValue::unavailable(UnavailableReason::InvalidData)
        );
    }

    #[test]
    fn child_exit_during_lookup_discards_all_process_facts() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let identity = crate::process_identity::observe(child.id()).unwrap();
        let facts = inspect(&identity, |_| {
            child.kill().unwrap();
            child.wait().unwrap();
            ProcessValue::unavailable(UnavailableReason::TimedOut)
        });
        assert!(
            facts.is_err(),
            "a partial account result cannot outlive its birth bracket"
        );
    }
}
