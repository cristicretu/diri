//! The child side of a PTY launch: `execve` with the launch's own `PATH`, and,
//! when no exec happens, the shell's answer instead of a failed spawn.
//!
//! A program that cannot be started is reported the way a shell reports it:
//! `diri: name: command not found` on the terminal, then exit status 127
//! (126 when the file exists but cannot be executed). The message lands in the
//! session's own output, so whoever opens the tab can read why it ended.
//! Before this, a direct spawn aborted inside the Rust runtime (its exec error
//! pipe is among the descriptors the child closes) and showed `fatal runtime
//! error` with SIGABRT, and a launchd-started one failed the launch with an
//! empty screen.

use std::ffi::CString;
use std::io;

/// Exit status of a program that was not found, as in POSIX shells.
pub const NOT_FOUND: i32 = 127;
/// Exit status of a program that exists but could not be executed.
pub const NOT_EXECUTABLE: i32 = 126;

/// Searched when the launch's environment has no `PATH`, as `execvp` does.
const DEFAULT_PATH: &[u8] = b"/usr/bin:/bin:/usr/sbin:/sbin";

/// Everything the child needs to exec, built before the fork: the child only
/// reads it, so it never allocates.
pub(crate) struct ExecPlan {
    program: CString,
    bare: bool,
    candidates: Vec<CString>,
    _argv: Vec<CString>,
    _env: Vec<CString>,
    argv_pointers: Vec<*const libc::c_char>,
    env_pointers: Vec<*const libc::c_char>,
}

// SAFETY: the pointers address heap buffers owned by the CStrings in the same
// value, which are never mutated or dropped while it lives; moving the plan
// moves only the owning handles, not the bytes.
unsafe impl Send for ExecPlan {}
// SAFETY: as above; every method takes `&self` and only reads.
unsafe impl Sync for ExecPlan {}

impl ExecPlan {
    /// `argv[0]` is searched for in `path` (the launch's `PATH`) when it has
    /// no slash, like `execvp` with the child's own environment.
    pub(crate) fn new(argv: Vec<CString>, env: Vec<CString>, path: Option<&[u8]>) -> Self {
        let program = argv[0].clone();
        let name = program.as_bytes();
        let bare = !name.contains(&b'/');
        let candidates = if bare {
            path.unwrap_or(DEFAULT_PATH)
                .split(|byte| *byte == b':')
                .filter(|directory| !directory.is_empty())
                .filter_map(|directory| CString::new([directory, b"/", name].concat()).ok())
                .collect()
        } else {
            vec![program.clone()]
        };
        let mut argv_pointers: Vec<_> = argv.iter().map(|arg| arg.as_ptr()).collect();
        argv_pointers.push(std::ptr::null());
        let mut env_pointers: Vec<_> = env.iter().map(|var| var.as_ptr()).collect();
        env_pointers.push(std::ptr::null());
        Self {
            program,
            bare,
            candidates,
            _argv: argv,
            _env: env,
            argv_pointers,
            env_pointers,
        }
    }

    /// From a structured spec. Fails on an interior NUL, before any fork.
    pub(crate) fn from_spec(spec: &crate::PtySpec) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "launch contains a NUL byte");
        let argv = spec
            .argv
            .iter()
            .map(|arg| CString::new(arg.as_bytes()).map_err(|_| invalid()))
            .collect::<io::Result<Vec<_>>>()?;
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
        }
        let env = spec
            .env
            .iter()
            .map(|(key, value)| CString::new(format!("{key}={value}")).map_err(|_| invalid()))
            .collect::<io::Result<Vec<_>>>()?;
        // The last assignment wins, as it does in the environment itself.
        let path = spec
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_bytes());
        Ok(Self::new(argv, env, path))
    }

    /// Execs the program, searching like `execvp`: entries that are missing
    /// or not directories are skipped, a permission failure is remembered,
    /// any other error stops. Returns the errno only when nothing was exec'd.
    ///
    /// # Safety
    /// For a forked child: only async-signal-safe calls, no allocation.
    pub(crate) unsafe fn exec(&self) -> i32 {
        let mut denied = false;
        for candidate in &self.candidates {
            // SAFETY: NUL-terminated argv/env arrays of live CStrings.
            unsafe {
                libc::execve(
                    candidate.as_ptr(),
                    self.argv_pointers.as_ptr(),
                    self.env_pointers.as_ptr(),
                )
            };
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOENT | libc::ENOTDIR) => {}
                Some(libc::EACCES) => denied = true,
                Some(errno) => return errno,
                None => return libc::ENOEXEC,
            }
        }
        if denied { libc::EACCES } else { libc::ENOENT }
    }

    /// Writes the shell's one-line explanation of `errno` to standard error
    /// (the terminal) and returns the shell's exit status for it.
    ///
    /// # Safety
    /// For a forked child: only `write`, no allocation.
    pub(crate) unsafe fn report(&self, errno: i32) -> i32 {
        let not_found = matches!(errno, libc::ENOENT | libc::ENOTDIR);
        let reason: &[u8] = if not_found && self.bare {
            b"command not found"
        } else {
            reason(errno)
        };
        for part in [
            b"diri: ".as_slice(),
            self.program.as_bytes(),
            b": ",
            reason,
            b"\n",
        ] {
            // SAFETY: a plain write of a live buffer to fd 2.
            unsafe { libc::write(2, part.as_ptr().cast(), part.len()) };
        }
        if not_found { NOT_FOUND } else { NOT_EXECUTABLE }
    }

    /// Execs, or reports why not and exits the child with the shell's status.
    ///
    /// # Safety
    /// For a forked child: never returns, never allocates.
    pub(crate) unsafe fn exec_or_exit(&self) -> ! {
        // SAFETY: forwarded; the caller is a forked child.
        unsafe {
            let errno = self.exec();
            libc::_exit(self.report(errno))
        }
    }
}

/// `strerror`'s text for the errors `execve` can return, as static bytes:
/// `strerror` itself is not async-signal-safe.
fn reason(errno: i32) -> &'static [u8] {
    match errno {
        libc::ENOENT => b"No such file or directory",
        libc::ENOTDIR => b"Not a directory",
        libc::EACCES => b"Permission denied",
        libc::ENOEXEC => b"Exec format error",
        libc::E2BIG => b"Argument list too long",
        libc::ENOMEM => b"Cannot allocate memory",
        libc::ELOOP => b"Too many levels of symbolic links",
        libc::ENAMETOOLONG => b"File name too long",
        libc::ETXTBSY => b"Text file busy",
        libc::EISDIR => b"Is a directory",
        libc::EPERM => b"Operation not permitted",
        _ => b"cannot execute",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(argv: &[&str], path: Option<&str>) -> ExecPlan {
        ExecPlan::new(
            argv.iter().map(|arg| CString::new(*arg).unwrap()).collect(),
            Vec::new(),
            path.map(str::as_bytes),
        )
    }

    #[test]
    fn a_bare_name_is_searched_in_the_launch_path_only() {
        let bare = plan(&["tool"], Some("/a::/b"));
        let candidates: Vec<_> = bare
            .candidates
            .iter()
            .map(|candidate| candidate.to_str().unwrap())
            .collect();
        assert_eq!(candidates, ["/a/tool", "/b/tool"]);
        assert!(bare.bare);
        let path = plan(&["./tool"], Some("/a"));
        assert!(!path.bare);
        assert_eq!(path.candidates.len(), 1);
    }

    #[test]
    fn exit_statuses_match_posix_shells() {
        // Probed in a forked child so the report's writes go to a pipe.
        for (argv, status, text) in [
            (
                "diri-no-such-program-xyz",
                NOT_FOUND,
                "diri: diri-no-such-program-xyz: command not found\n",
            ),
            (
                "/nonexistent/diri-tool",
                NOT_FOUND,
                "diri: /nonexistent/diri-tool: No such file or directory\n",
            ),
            ("/", NOT_EXECUTABLE, "diri: /: Permission denied\n"),
        ] {
            let launch = plan(&[argv], Some("/nonexistent-dir"));
            let (mut reader, writer) = std::io::pipe().unwrap();
            use std::os::fd::AsRawFd;
            // SAFETY: the child only dup2s, execs or writes, then _exits.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                unsafe {
                    libc::dup2(writer.as_raw_fd(), 2);
                    launch.exec_or_exit();
                }
            }
            drop(writer);
            let mut output = String::new();
            std::io::Read::read_to_string(&mut reader, &mut output).unwrap();
            let mut wait = 0;
            // SAFETY: reaping our own child.
            unsafe { libc::waitpid(pid, &mut wait, 0) };
            assert!(libc::WIFEXITED(wait), "{argv}");
            assert_eq!(libc::WEXITSTATUS(wait), status, "{argv}");
            assert_eq!(output, text);
        }
    }
}
