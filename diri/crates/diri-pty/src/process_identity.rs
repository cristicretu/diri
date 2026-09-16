//! Host-local observation. Never pass a PID obtained from another host here.
use diri_proto::process::{BootId, ProcessBirth, ProcessIdentity};
use std::io;

/// Observe an identity, not ownership. Owners must capture it while their
/// child is still unreaped; later observations only verify that captured value.
pub fn observe(pid: u32) -> io::Result<ProcessIdentity> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process PID",
        ));
    }
    platform_observe(pid)
}

/// Read facts only between matching observations of an already-owned identity.
/// Failure never substitutes the process currently occupying the same PID.
pub fn inspect_verified<T>(
    expected: &ProcessIdentity,
    read: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    inspect_with(expected, observe, read)
}
fn inspect_with<T>(
    expected: &ProcessIdentity,
    mut observation: impl FnMut(u32) -> io::Result<ProcessIdentity>,
    read: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let mismatch = || io::Error::new(io::ErrorKind::NotFound, "process birth identity changed");
    if observation(expected.pid())? != *expected {
        return Err(mismatch());
    }
    let result = read()?;
    if observation(expected.pid())? != *expected {
        return Err(mismatch());
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn platform_observe(pid: u32) -> io::Result<ProcessIdentity> {
    let mut boot = [0u8; 37];
    let mut length = boot.len();
    // SAFETY: fixed read-only sysctl name and an exactly bounded output buffer.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            boot.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length == 0 || length > boot.len() {
        return Err(invalid("invalid boot UUID length"));
    }
    let boot = std::str::from_utf8(&boot[..length])
        .map_err(|_| invalid("invalid boot UUID encoding"))?
        .trim_end_matches('\0');
    let boot_session = BootId::parse(boot).map_err(invalid)?;
    // SAFETY: libc supplies the SDK-matching struct; proc_pidinfo initializes
    // the complete buffer, whose exact returned size and PID are checked.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as libc::c_int;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    if filled != size {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process birth information unavailable",
        ));
    }
    if info.pbi_pid != pid {
        return Err(invalid("process PID changed during observation"));
    }
    ProcessIdentity::new(
        pid,
        ProcessBirth::Macos {
            boot_session,
            start_seconds: info.pbi_start_tvsec,
            start_microseconds: u32::try_from(info.pbi_start_tvusec)
                .map_err(|_| invalid("invalid process birth microseconds"))?,
        },
    )
    .map_err(invalid)
}

#[cfg(target_os = "linux")]
fn platform_observe(pid: u32) -> io::Result<ProcessIdentity> {
    use std::io::Read;
    fn bounded_file(path: &str, max: usize) -> io::Result<String> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(max as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max {
            return Err(invalid("oversized process identity source"));
        }
        String::from_utf8(bytes).map_err(|_| invalid("invalid process identity encoding"))
    }
    let boot_id = BootId::parse(bounded_file("/proc/sys/kernel/random/boot_id", 64)?.trim())
        .map_err(invalid)?;
    let stat = bounded_file(&format!("/proc/{pid}/stat"), 4096)?;
    let start_ticks = linux_start_ticks(pid, &stat)?;
    // SAFETY: sysconf queries one constant, has no pointers or side effects.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let clock_ticks_per_second =
        u32::try_from(ticks).map_err(|_| invalid("invalid process clock tick rate"))?;
    ProcessIdentity::new(
        pid,
        ProcessBirth::Linux {
            boot_id,
            start_ticks,
            clock_ticks_per_second,
        },
    )
    .map_err(invalid)
}

#[cfg(any(target_os = "linux", test))]
fn linux_start_ticks(pid: u32, stat: &str) -> io::Result<u64> {
    // comm is parenthesized and may itself contain spaces or ')'. Fields after
    // its final ')' begin at field 3 (state); starttime is field 22.
    let open = stat
        .find('(')
        .ok_or_else(|| invalid("missing process name"))?;
    let close = stat
        .rfind(')')
        .filter(|&close| close > open)
        .ok_or_else(|| invalid("missing process name end"))?;
    if stat[..open].trim().parse::<u32>().ok() != Some(pid) {
        return Err(invalid("process stat PID mismatch"));
    }
    stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| invalid("missing process start ticks"))?
        .parse()
        .map_err(|_| invalid("invalid process start ticks"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_observe(_: u32) -> io::Result<ProcessIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process birth identity unsupported on this platform",
    ))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn fixture(ticks: u64, boot: &str) -> ProcessIdentity {
        ProcessIdentity::new(
            42,
            ProcessBirth::Linux {
                boot_id: BootId::parse(boot).unwrap(),
                start_ticks: ticks,
                clock_ticks_per_second: 100,
            },
        )
        .unwrap()
    }
    const BOOT: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    #[test]
    fn recycled_pid_or_changed_boot_never_reads_or_returns_facts() {
        let expected = fixture(100, BOOT);
        for replacement in [
            fixture(101, BOOT),
            fixture(100, "bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee"),
        ] {
            let called = Cell::new(false);
            assert!(
                inspect_with(
                    &expected,
                    |_| Ok(replacement),
                    || {
                        called.set(true);
                        Ok(7)
                    }
                )
                .is_err()
            );
            assert!(!called.get());
            let n = Cell::new(0);
            assert!(
                inspect_with(
                    &expected,
                    |_| {
                        n.set(n.get() + 1);
                        Ok(if n.get() == 1 { expected } else { replacement })
                    },
                    || Ok(7)
                )
                .is_err(),
                "changed identity after facts must discard them"
            );
        }
        assert_eq!(
            inspect_with(&expected, |_| Ok(expected), || Ok(7)).unwrap(),
            7
        );
    }
    #[test]
    fn linux_stat_parser_preserves_ticks_and_handles_parentheses() {
        let fields = (3..=21)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            linux_start_ticks(
                42,
                &format!("42 (name ) with spaces) {fields} 987654321 99")
            )
            .unwrap(),
            987654321
        );
        for value in [
            "42 (short) R",
            "41 (wrong) R",
            "42 no-name",
            "42 (name) R nope",
        ] {
            assert!(linux_start_ticks(42, value).is_err());
        }
    }
    #[test]
    fn a_real_owned_child_has_stable_identity_until_reaped() {
        let mut pty = crate::Pty::spawn(&crate::PtySpec::new(
            vec!["/bin/sh".into(), "-c".into(), "read -r done".into()],
            "/tmp",
        ))
        .unwrap();
        let expected = pty.child_identity().expect("supported host birth identity");
        assert_eq!(expected.pid(), pty.pid());
        assert_eq!(observe(pty.pid()).unwrap(), expected);
        assert_eq!(
            inspect_verified(&expected, || Ok("owned")).unwrap(),
            "owned"
        );
        pty.kill_group(libc::SIGKILL).unwrap();
        pty.wait().unwrap();
        assert!(inspect_verified(&expected, || Ok("stale")).is_err());
    }
}
