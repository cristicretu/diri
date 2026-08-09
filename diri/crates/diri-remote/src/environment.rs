//! Login-shell environment capture over a dedicated inherited descriptor.

use std::ffi::{CStr, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, EnvironmentCaptureResult, EnvironmentVariable,
    MAX_ENVIRONMENT_VALUE_BYTES, MAX_ENVIRONMENT_VARIABLES, MAX_LAUNCH_BYTES,
};

const ENVIRONMENT_FD: libc::c_int = 9;
const MARKER: &[u8] = b"DIRIENV1\0";
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const LOGIN_COMMAND: &str = "exec \"$DIRI_REMOTE_SELF\" __dump-environment";
const WORKING_DIRECTORY_COMMAND: &str =
    "cd -- \"$DIRI_REMOTE_CWD\" && exec \"$DIRI_REMOTE_SELF\" __dump-environment";

pub fn capture(
    request: &EnvironmentCaptureRequest,
    executable: &Path,
) -> io::Result<EnvironmentCaptureResult> {
    let shell = account_shell()?;
    capture_with_shell(request, executable, &shell)
}

pub(crate) fn capture_with_shell(
    request: &EnvironmentCaptureRequest,
    executable: &Path,
    shell: &Path,
) -> io::Result<EnvironmentCaptureResult> {
    request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
    let timeout = Duration::from_millis(request.timeout_millis);
    let account = capture_layer(shell, executable, &home, None, None, timeout)?;
    let target = request
        .cwd
        .as_deref()
        .map(expand_home)
        .transpose()?
        .unwrap_or_else(|| PathBuf::from(&account.cwd));
    let working = capture_layer(
        shell,
        executable,
        &home,
        Some(&account.environment),
        Some(&target),
        timeout,
    )?;
    let diagnostics = bounded_diagnostics(
        account.diagnostics.as_bytes(),
        working.diagnostics.as_bytes(),
    );
    Ok(EnvironmentCaptureResult {
        shell: shell.to_string_lossy().into_owned(),
        cwd: working.cwd,
        environment: working.environment,
        diagnostics,
        diagnostics_truncated: account.diagnostics_truncated || working.diagnostics_truncated,
    })
}

struct LayerCapture {
    cwd: String,
    environment: Vec<EnvironmentVariable>,
    diagnostics: String,
    diagnostics_truncated: bool,
}

fn capture_layer(
    shell: &Path,
    executable: &Path,
    initial_cwd: &Path,
    base_environment: Option<&[EnvironmentVariable]>,
    target_cwd: Option<&Path>,
    timeout: Duration,
) -> io::Result<LayerCapture> {
    let (environment_reader, environment_writer) = UnixStream::pair()?;
    let inherited_fd = environment_writer.as_raw_fd();
    let mut command = Command::new(shell);
    command.arg("-l");
    // Full user shells need interactive startup for zshrc/bashrc toolchain
    // setup. Minimal POSIX `sh`/dash closes inherited descriptors in
    // interactive no-TTY mode, so its portable login startup is used instead.
    let shell_name = shell.file_name().and_then(|name| name.to_str());
    if !matches!(shell_name, Some("sh" | "dash")) {
        command.arg("-i");
    }
    command.arg("-c").arg(if target_cwd.is_some() {
        WORKING_DIRECTORY_COMMAND
    } else {
        LOGIN_COMMAND
    });
    command.current_dir(initial_cwd);
    if let Some(environment) = base_environment {
        command.env_clear();
        command.envs(
            environment
                .iter()
                .map(|variable| (&variable.name, &variable.value)),
        );
    }
    if let Some(cwd) = target_cwd {
        command.env("DIRI_REMOTE_CWD", cwd);
    }
    command.env("DIRI_REMOTE_SELF", executable);
    command.env("DIRI_ENV_FD", ENVIRONMENT_FD.to_string());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // SAFETY: the closure only duplicates/updates integer descriptors with
    // async-signal-safe libc calls. `inherited_fd` is held open by the parent
    // until after spawn, and fd 9 is reserved solely for this child protocol.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if inherited_fd != ENVIRONMENT_FD && libc::dup2(inherited_fd, ENVIRONMENT_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(ENVIRONMENT_FD, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(environment_writer);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("login shell stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("login shell stderr is unavailable"))?;
    let environment_thread =
        std::thread::spawn(move || drain_bounded(environment_reader, MAX_LAUNCH_BYTES));
    let stdout_thread = std::thread::spawn(move || drain_bounded(stdout, DIAGNOSTIC_LIMIT));
    let stderr_thread = std::thread::spawn(move || drain_bounded(stderr, DIAGNOSTIC_LIMIT));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: the child created a new session/process group in
                // `pre_exec`; killing that group also closes descriptors held
                // by startup-script descendants.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.wait();
            let _ = join_reader(environment_thread);
            let _ = join_reader(stdout_thread);
            let _ = join_reader(stderr_thread);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "login environment capture timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (environment_bytes, environment_truncated) = join_reader(environment_thread)?;
    let (stdout, stdout_truncated) = join_reader(stdout_thread)?;
    let (stderr, stderr_truncated) = join_reader(stderr_thread)?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "login shell exited with {status}: {}",
            bounded_diagnostics(&stdout, &stderr)
        )));
    }
    if environment_truncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured environment exceeds 1 MiB",
        ));
    }
    let (cwd, environment) = parse_environment(&environment_bytes)?;
    Ok(LayerCapture {
        cwd,
        environment,
        diagnostics: bounded_diagnostics(&stdout, &stderr),
        diagnostics_truncated: stdout_truncated || stderr_truncated,
    })
}

fn expand_home(cwd: &str) -> io::Result<PathBuf> {
    if cwd == "~" || cwd.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unavailable"))?;
        return Ok(if cwd == "~" {
            home
        } else {
            home.join(&cwd[2..])
        });
    }
    Ok(PathBuf::from(cwd))
}

/// Hidden child operation invoked only by a login shell. It serializes the
/// already-initialized shell environment onto fd 9, never stdout.
pub fn dump() -> io::Result<()> {
    if std::env::var("DIRI_ENV_FD").ok().as_deref() != Some("9") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "environment descriptor was not provisioned",
        ));
    }
    // SAFETY: the capture parent exclusively provisions fd 9 for this exec;
    // this child takes ownership exactly once and closes it on drop.
    let mut output = unsafe { File::from_raw_fd(ENVIRONMENT_FD) };
    output.write_all(MARKER)?;
    output.write_all(std::env::current_dir()?.as_os_str().as_bytes())?;
    output.write_all(&[0])?;
    for (name, value) in std::env::vars_os() {
        let name = name.as_os_str().as_bytes();
        let value = value.as_os_str().as_bytes();
        if name.contains(&0) || name.contains(&b'=') || value.contains(&0) {
            continue;
        }
        output.write_all(name)?;
        output.write_all(b"=")?;
        output.write_all(value)?;
        output.write_all(&[0])?;
    }
    output.flush()
}

fn account_shell() -> io::Result<PathBuf> {
    const INITIAL_BUFFER_BYTES: usize = 16 * 1024;
    const MAX_BUFFER_BYTES: usize = 1024 * 1024;

    // SAFETY: `geteuid` has no preconditions and does not access memory.
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0_u8; INITIAL_BUFFER_BYTES];
    loop {
        let mut passwd = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: `passwd` and `result` are valid output locations and
        // `buffer` is writable for its full advertised length. On success,
        // all pointers in `passwd` refer into `buffer`, which remains alive
        // until the shell bytes are copied into the returned PathBuf.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < MAX_BUFFER_BYTES {
            buffer.resize((buffer.len() * 2).min(MAX_BUFFER_BYTES), 0);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "remote account record is unavailable",
            ));
        }
        // SAFETY: successful `getpwuid_r` initialized `passwd`; its pointer
        // fields remain backed by `buffer` in this scope.
        let passwd = unsafe { passwd.assume_init() };
        let shell = if passwd.pw_shell.is_null() {
            PathBuf::from("/bin/sh")
        } else {
            // SAFETY: POSIX account records expose `pw_shell` as a NUL-
            // terminated string within the caller-owned buffer.
            let bytes = unsafe { CStr::from_ptr(passwd.pw_shell) }.to_bytes();
            if bytes.is_empty() {
                PathBuf::from("/bin/sh")
            } else {
                PathBuf::from(OsStr::from_bytes(bytes))
            }
        };
        if !shell.is_absolute() || !shell.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote account login shell is not an absolute executable file",
            ));
        }
        return Ok(shell);
    }
}

fn parse_environment(bytes: &[u8]) -> io::Result<(String, Vec<EnvironmentVariable>)> {
    let payload = bytes.strip_prefix(MARKER).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "environment marker is missing")
    })?;
    let mut fields = payload.split(|byte| *byte == 0);
    let cwd = fields
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "captured cwd is missing"))?;
    let cwd = std::str::from_utf8(cwd)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .to_string();
    if !Path::new(&cwd).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured cwd is not absolute",
        ));
    }
    let mut environment = Vec::new();
    for field in fields {
        if field.is_empty() {
            continue;
        }
        let Some(separator) = field.iter().position(|byte| *byte == b'=') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "captured environment entry has no '='",
            ));
        };
        let name = std::str::from_utf8(&field[..separator])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let value = std::str::from_utf8(&field[separator + 1..])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "captured environment value exceeds 64 KiB",
            ));
        }
        if should_scrub(name) {
            continue;
        }
        environment.push(EnvironmentVariable {
            name: name.to_string(),
            value: value.to_string(),
        });
        if environment.len() > MAX_ENVIRONMENT_VARIABLES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "captured environment exceeds 4096 variables",
            ));
        }
    }
    environment.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((cwd, environment))
}

fn should_scrub(name: &str) -> bool {
    name.starts_with("DIRI_")
        || name.starts_with("SSH_")
        || matches!(name, "_" | "PWD" | "OLDPWD" | "SHLVL")
}

fn drain_bounded(mut reader: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        let stored = remaining.min(count);
        captured.extend_from_slice(&buffer[..stored]);
        truncated |= stored != count;
    }
    Ok((captured, truncated))
}

fn join_reader(
    thread: std::thread::JoinHandle<io::Result<(Vec<u8>, bool)>>,
) -> io::Result<(Vec<u8>, bool)> {
    thread
        .join()
        .map_err(|_| io::Error::other("environment reader thread panicked"))?
}

fn bounded_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let mut combined = Vec::with_capacity((stdout.len() + stderr.len()).min(DIAGNOSTIC_LIMIT));
    combined.extend_from_slice(&stdout[..stdout.len().min(DIAGNOSTIC_LIMIT)]);
    let remaining = DIAGNOSTIC_LIMIT.saturating_sub(combined.len());
    combined.extend_from_slice(&stderr[..stderr.len().min(remaining)]);
    String::from_utf8_lossy(&combined).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_nul_values_and_scrubs_ssh_session_state() {
        let bytes =
            b"DIRIENV1\0/tmp/project\0PATH=/bin:/usr/bin\0SSH_CONNECTION=secret\0VALUE=a b\0";
        let (cwd, environment) = parse_environment(bytes).expect("parse");
        assert_eq!(cwd, "/tmp/project");
        assert_eq!(
            environment,
            vec![
                EnvironmentVariable {
                    name: "PATH".into(),
                    value: "/bin:/usr/bin".into()
                },
                EnvironmentVariable {
                    name: "VALUE".into(),
                    value: "a b".into()
                }
            ]
        );
    }

    #[test]
    fn bounded_reader_drains_but_does_not_retain_excess() {
        let input = vec![b'x'; 1024];
        let (captured, truncated) = drain_bounded(input.as_slice(), 10).expect("drain");
        assert_eq!(captured, vec![b'x'; 10]);
        assert!(truncated);
    }

    #[test]
    fn os_string_bytes_are_not_interpreted_as_shell_syntax() {
        assert_eq!(
            std::ffi::OsStr::new("$(untouched)").as_bytes(),
            b"$(untouched)"
        );
    }
}
