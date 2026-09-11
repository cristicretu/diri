//! Unix PTY implementation.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use super::{Exit, PtySpec};

#[cfg(target_os = "macos")]
const MAX_SIGNAL: libc::c_int = 32;
#[cfg(not(target_os = "macos"))]
const MAX_SIGNAL: libc::c_int = 65;

/// A child process whose controlling terminal is a private pseudo-terminal.
pub struct Pty {
    master: OwnedFd,
    child: Child,
}

impl Pty {
    /// Spawn an exact structured command on a new controlling PTY.
    pub fn spawn(spec: &PtySpec) -> io::Result<Self> {
        let program = spec
            .argv
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "argv is empty"))?;
        if spec.cols == 0 || spec.rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY dimensions must be non-zero",
            ));
        }

        #[cfg(target_os = "linux")]
        let winsize = libc::winsize {
            ws_row: spec.rows,
            ws_col: spec.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Apple's libc declares `openpty` with a mutable winsize pointer even
        // though the structure is an input. Keep that ABI detail at this seam;
        // Linux correctly accepts a shared pointer and clippy enforces it.
        #[cfg(not(target_os = "linux"))]
        let mut winsize = libc::winsize {
            ws_row: spec.rows,
            ws_col: spec.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        #[cfg(target_os = "linux")]
        let winsize_ptr = &winsize;
        #[cfg(not(target_os = "linux"))]
        let winsize_ptr = &mut winsize;
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        // SAFETY: both output pointers refer to initialized local storage and
        // `winsize` is fully initialized. On success both returned fds are new
        // owned descriptors, transferred immediately into `OwnedFd`.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                winsize_ptr,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `openpty` succeeded and returned two fresh descriptors.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        // SAFETY: same ownership argument as `master`; each fd is wrapped once.
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };

        let mut command = Command::new(OsStr::new(program));
        command.args(&spec.argv[1..]);
        command.env_clear();
        command.envs(spec.env.iter().map(|(key, value)| (key, value)));
        command.current_dir(&spec.cwd);
        command.stdin(Stdio::from(slave.try_clone()?));
        command.stdout(Stdio::from(slave.try_clone()?));
        command.stderr(Stdio::from(slave.try_clone()?));

        let slave_fd = slave.as_raw_fd();
        // SAFETY: `pre_exec` runs after fork and before exec. The closure only
        // invokes async-signal-safe libc calls, touches stack values captured
        // by copy, and reports errors without heap allocation in the child.
        unsafe {
            command.pre_exec(move || {
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                for signal in 1..MAX_SIGNAL {
                    libc::signal(signal, libc::SIG_DFL);
                }

                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // stdin is already the slave; pin this session as the
                // foreground group so a parent `TIOCGPGRP` is defined
                // before exec. Ignore failure: TIOCSCTTY already set pgrp.
                let _ = libc::tcsetpgrp(0, libc::getpid());
                close_extra_fds();
                Ok(())
            });
        }

        let child = command.spawn()?;
        drop(slave);
        Ok(Self { master, child })
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The process group currently in the foreground on this PTY, if any.
    ///
    /// Call this on the owner, not on the live read stream: `tcgetpgrp` on
    /// the same fd the pump is reading can lose canonical-mode input. Do not
    /// extract a raw fd from a temporary clone either; closing that clone
    /// before the call yields EBADF and looks like no job.
    #[must_use]
    pub fn foreground_pgid(&self) -> Option<i32> {
        foreground_pgid(self.master.as_raw_fd()).or_else(|| proc_tpgid(self.child.id()))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY dimensions must be non-zero",
            ));
        }
        let winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `master` remains owned for the call and `winsize` is a valid
        // initialized input buffer for `TIOCSWINSZ`.
        let result =
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &winsize) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        // SAFETY: zero is a valid initialization for `winsize`; the kernel
        // fills it through the valid, owned master fd.
        let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `winsize` is writable for the duration of the ioctl.
        let result =
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ as _, &mut winsize) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((winsize.ws_col, winsize.ws_row))
    }

    pub fn reader(&self) -> io::Result<PtyStream> {
        Ok(PtyStream(File::from(self.master.try_clone()?)))
    }

    pub fn writer(&self) -> io::Result<PtyStream> {
        Ok(PtyStream(File::from(self.master.try_clone()?)))
    }

    pub fn wait(&mut self) -> io::Result<Exit> {
        self.child.wait().map(exit_from)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        Ok(self.child.try_wait()?.map(exit_from))
    }

    pub fn kill_group(&self, signal: i32) -> io::Result<()> {
        let pid = i32::try_from(self.child.id()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "child pid does not fit in i32")
        })?;
        // SAFETY: the child called `setsid`, therefore `-pid` names the
        // process group created by this object. No pointer memory is involved.
        let result = unsafe { libc::kill(-pid, signal) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn terminate(&mut self, grace: std::time::Duration) -> io::Result<Exit> {
        self.kill_group(libc::SIGTERM)?;
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.kill_group(libc::SIGKILL)?;
        self.wait()
    }
}

fn exit_from(status: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    status
        .signal()
        .map_or_else(|| Exit::Code(status.code().unwrap_or(-1)), Exit::Signal)
}

fn close_extra_fds() {
    // GitHub runners set NOFILE to ~1M. Closing that range one fd at a
    // time delays exec by seconds and the foreground-job tests time out.
    #[cfg(target_os = "linux")]
    unsafe {
        // SAFETY: after fork in the child; fds 0-2 stay the slave. musl has
        // no close_range wrapper, so the syscall is used on gnu and musl.
        if libc::syscall(libc::SYS_close_range, 3, libc::c_uint::MAX, 0) == 0 {
            return;
        }
    }
    unsafe {
        // SAFETY: same child-side ownership. Linux before close_range (or a
        // seccomp policy denying it) still needs every inherited fd closed.
        // macOS uses this path directly.
        let maximum = libc::getdtablesize();
        for fd in 3..maximum {
            libc::close(fd);
        }
    }
}

fn foreground_pgid(fd: RawFd) -> Option<i32> {
    // SAFETY: `tcgetpgrp` on an owned PTY master; a bad fd returns -1.
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    (pgid > 0).then_some(pgid)
}

/// Linux parents often see `tcgetpgrp(master) == 0` even after the child
/// claimed the slave. `/proc/<pid>/stat` tpgid is the child's view of the
/// same tty and does not require the caller to own it.
fn proc_tpgid(pid: u32) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        tpgid_from_stat(&stat)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn tpgid_from_stat(stat: &str) -> Option<i32> {
    let after = stat.get(stat.rfind(')')? + 2..)?;
    let tpgid: i32 = after.split_whitespace().nth(5)?.parse().ok()?;
    (tpgid > 0).then_some(tpgid)
}

/// Independently clonable handle on the PTY master.
pub struct PtyStream(File);

impl PtyStream {
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
        // SAFETY: `F_GETFL` and `F_SETFL` operate on an owned fd and do not
        // access caller memory.
        let flags = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let updated = if enabled {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        // SAFETY: `updated` contains the existing status flags with only
        // `O_NONBLOCK` changed.
        if unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETFL, updated) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn wait_readable(&self, timeout: std::time::Duration) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: one initialized poll descriptor remains writable throughout
        // the call; its fd is owned by `self`.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
    }

    #[must_use]
    pub fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl Read for PtyStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buffer) {
            Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
            other => other,
        }
    }
}

impl Write for PtyStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Pollable notification that becomes readable when a child exits. This lets
/// a Holder sleep indefinitely without a timer while still reaping promptly.
pub struct ExitWatcher(OwnedFd);

impl ExitWatcher {
    pub fn new(pid: u32) -> io::Result<Self> {
        platform_exit_watcher(pid).map(Self)
    }

    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
fn platform_exit_watcher(pid: u32) -> io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid does not fit pid_t"))?;
    // SAFETY: `pidfd_open` takes integer values only and returns a fresh fd on
    // success. Flags zero is the only currently supported value.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful syscall returned a fresh owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "macos")]
fn platform_exit_watcher(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: `kqueue` takes no inputs and returns a fresh fd on success.
    let descriptor = unsafe { libc::kqueue() };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call returned a fresh owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let event = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: both descriptor and event are valid; the timeout is null for a
    // non-blocking registration operation with no output events requested.
    let result = unsafe {
        libc::kevent(
            descriptor.as_raw_fd(),
            &event,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_child_does_not_inherit_extra_descriptors() {
        #[cfg(target_os = "linux")]
        if let Ok(error) = std::env::var("DIRI_TEST_CLOSE_RANGE_ERRNO") {
            block_close_range(error.parse().expect("errno"));
        }

        let file = File::open("/dev/null").expect("open sentinel");
        // Deliberately inherit an fd without CLOEXEC, well above the stdio
        // and shell startup descriptors, without replacing an existing fd.
        // SAFETY: file is live; F_DUPFD returns a fresh descriptor on success.
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(fd >= 200);
        // SAFETY: fcntl returned a new descriptor owned by this test.
        let sentinel = unsafe { OwnedFd::from_raw_fd(fd) };
        let spec = PtySpec::new(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "if [ -e \"/dev/fd/$1\" ]; then exit 42; fi; printf clean".into(),
                "fd-test".into(),
                sentinel.as_raw_fd().to_string(),
            ],
            "/",
        );
        let mut pty = Pty::spawn(&spec).expect("spawn");
        let mut output = Vec::new();
        pty.reader()
            .expect("reader")
            .read_to_end(&mut output)
            .expect("output");
        assert_eq!(
            pty.wait().expect("wait"),
            Exit::Code(0),
            "inherited sentinel fd"
        );
        assert_eq!(output, b"clean");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_closes_descriptors_when_close_range_is_unavailable_or_denied() {
        for error in [libc::ENOSYS, libc::EPERM] {
            let output = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "unix::tests::pty_child_does_not_inherit_extra_descriptors",
                    "--nocapture",
                ])
                .env("DIRI_TEST_CLOSE_RANGE_ERRNO", error.to_string())
                .output()
                .expect("run isolated seccomp test");
            assert!(
                output.status.success(),
                "close_range errno {error}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn block_close_range(error: u32) {
        // Only this disposable test process and its children receive the
        // filter. No privileges or host configuration are required.
        let mut instructions = [
            libc::sock_filter {
                code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: libc::SYS_close_range as u32,
            },
            libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | error,
            },
            libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ALLOW,
            },
        ];
        let filter = libc::sock_fprog {
            len: instructions.len() as u16,
            filter: instructions.as_mut_ptr(),
        };
        // SAFETY: no_new_privs only restricts this process; the filter points
        // to initialized BPF instructions for the duration of prctl.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(
                libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &filter),
                0
            );
        }
    }

    #[test]
    fn structured_argv_environment_and_size_reach_the_child() {
        let spec = PtySpec::new(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s:%s' \"$DIRI_VALUE\" \"$#\"".into(),
                "holder-test".into(),
                "one argument".into(),
            ],
            "/",
        )
        .env("DIRI_VALUE", "exact value")
        .size(91, 37);
        let mut pty = Pty::spawn(&spec).expect("spawn PTY");
        assert_eq!(pty.size().expect("size"), (91, 37));
        let mut reader = pty.reader().expect("reader");
        let mut output = Vec::new();
        reader.read_to_end(&mut output).expect("read output");
        assert_eq!(pty.wait().expect("wait"), Exit::Code(0));
        assert!(String::from_utf8_lossy(&output).contains("exact value:1"));
    }

    #[test]
    fn tpgid_is_the_eighth_stat_field_after_a_spaced_comm() {
        let stat = "42 (sleep 8) R 1 10 10 34816 99 0";
        assert_eq!(tpgid_from_stat(stat), Some(99));
        assert_eq!(tpgid_from_stat("42 (sleep 8) R 1"), None);
        assert_eq!(tpgid_from_stat("no-paren 1 2 3 4 5 6"), None);
    }

    #[test]
    fn foreground_pgid_tracks_a_job_other_than_the_shell() {
        use std::time::{Duration, Instant};

        // bash is on every CI image; zsh is not. Interactive + job control
        // puts `sleep` in a process group other than the shell.
        let spec = PtySpec::new(
            vec![
                "/bin/bash".into(),
                "--norc".into(),
                "--noprofile".into(),
                "-i".into(),
            ],
            "/tmp",
        )
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .env("PS1", "$ ");
        let pty = Pty::spawn(&spec).expect("spawn shell");
        let child = pty.pid() as i32;
        let mut reader = pty.reader().expect("reader");
        reader.set_nonblocking(true).ok();
        let mut writer = pty.writer().expect("writer");
        let mut drain = [0u8; 4096];
        let claimed = Instant::now() + Duration::from_secs(2);
        let mut last = None;
        while Instant::now() < claimed {
            let _ = reader.read(&mut drain);
            last = pty.foreground_pgid();
            if last == Some(child) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(last, Some(child), "shell never claimed the tty");
        writer.write_all(b"sleep 8\n").expect("write sleep");
        writer.flush().expect("flush");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = reader.read(&mut drain);
            last = pty.foreground_pgid();
            if last.is_some_and(|pgid| pgid > 0 && pgid != child) {
                break;
            }
            if Instant::now() >= deadline {
                panic!("sleep never became the foreground group; last={last:?} child={child}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn empty_argv_and_zero_dimensions_are_rejected() {
        assert!(Pty::spawn(&PtySpec::new(Vec::new(), "/")).is_err());
        assert!(Pty::spawn(&PtySpec::new(vec!["/bin/true".into()], "/").size(0, 24)).is_err());
    }
}
