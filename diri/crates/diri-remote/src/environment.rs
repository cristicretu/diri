//! Login-shell environment capture over a marker-framed stdout payload.
//!
//! An inherited descriptor cannot carry this payload: GNU bash closes every
//! descriptor from 3 through 19 across `exec` whenever it starts as an
//! interactive login shell (`shell.c`, "some systems have the bad habit of
//! starting login shells with lots of open file descriptors"). stdout is the
//! only stream a login shell is guaranteed to hand to the command it execs, so
//! the payload travels there behind a marker and an explicit length that
//! separate it from rc-file chatter.

use std::ffi::{CStr, OsStr};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, EnvironmentCaptureResult, EnvironmentVariable,
    MAX_ENVIRONMENT_VALUE_BYTES, MAX_ENVIRONMENT_VARIABLES, MAX_LAUNCH_BYTES,
};

const MARKER: &[u8] = b"DIRIENV1\0";
const LENGTH_BYTES: usize = 8;
const PAYLOAD_LIMIT: usize = MARKER.len() + LENGTH_BYTES + MAX_LAUNCH_BYTES;
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
    let mut command = Command::new(shell);
    command.arg("-l");
    // Full user shells need interactive startup for zshrc/bashrc toolchain
    // setup. Minimal POSIX `sh`/dash misbehaves in interactive no-TTY mode, so
    // its portable login startup is used instead.
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
    command.env("DIRI_ENV_CAPTURE", "1");
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // SAFETY: the closure only calls `setsid`, which is async-signal-safe and
    // touches no memory shared with the forking parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("login shell stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("login shell stderr is unavailable"))?;
    let stdout_thread = std::thread::spawn(move || drain_framed(stdout));
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
            let _ = join_framed(stdout_thread);
            let _ = join_reader(stderr_thread);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "login environment capture timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let framed = join_framed(stdout_thread)?;
    let (stderr, stderr_truncated) = join_reader(stderr_thread)?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "login shell exited with {status}: {}",
            bounded_diagnostics(&framed.noise, &stderr)
        )));
    }
    if framed.payload_truncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured environment exceeds 1 MiB",
        ));
    }
    let (cwd, environment) = parse_environment(&framed.payload)?;
    Ok(LayerCapture {
        cwd,
        environment,
        diagnostics: bounded_diagnostics(&framed.noise, &stderr),
        diagnostics_truncated: framed.noise_truncated || stderr_truncated,
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
/// already-initialized shell environment onto stdout, framed by a marker and
/// an explicit length so that rc-file output before it and any background
/// startup job writing after it stay outside the payload.
pub fn dump(stdout: &mut dyn Write) -> io::Result<()> {
    if std::env::var("DIRI_ENV_CAPTURE").ok().as_deref() != Some("1") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "environment capture was not requested",
        ));
    }
    let mut body = Vec::new();
    body.extend_from_slice(std::env::current_dir()?.as_os_str().as_bytes());
    body.push(0);
    for (name, value) in std::env::vars_os() {
        let name = name.as_os_str().as_bytes();
        let value = value.as_os_str().as_bytes();
        if name.contains(&0) || name.contains(&b'=') || value.contains(&0) {
            continue;
        }
        body.extend_from_slice(name);
        body.push(b'=');
        body.extend_from_slice(value);
        body.push(0);
    }
    let length = u64::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "environment is too large"))?;
    stdout.write_all(MARKER)?;
    stdout.write_all(&length.to_be_bytes())?;
    stdout.write_all(&body)?;
    stdout.flush()
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

/// Unwraps the marker/length frame the dump child wrote. `bytes` begins at the
/// marker; the length header decides where the payload ends, so trailing shell
/// output can never be read back as environment entries.
fn frame_body(bytes: &[u8]) -> io::Result<&[u8]> {
    let framed = bytes.strip_prefix(MARKER).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "environment marker is missing")
    })?;
    let (header, body) = framed.split_at_checked(LENGTH_BYTES).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "environment length header is incomplete",
        )
    })?;
    let length = usize::try_from(u64::from_be_bytes(
        header.try_into().expect("length header is eight bytes"),
    ))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "captured environment exceeds 1 MiB",
        )
    })?;
    body.get(..length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "captured environment is incomplete",
        )
    })
}

fn parse_environment(bytes: &[u8]) -> io::Result<(String, Vec<EnvironmentVariable>)> {
    let payload = frame_body(bytes)?;
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

#[derive(Default)]
struct FramedCapture {
    noise: Vec<u8>,
    noise_truncated: bool,
    payload: Vec<u8>,
    payload_truncated: bool,
    framed: bool,
}

/// Splits the login shell's stdout into the chatter that precedes the payload
/// and the payload itself. The split happens while draining, so a talkative rc
/// file is bounded as a diagnostic without ever pushing the payload out of the
/// capture.
fn drain_framed(mut reader: impl Read) -> io::Result<FramedCapture> {
    let mut capture = FramedCapture::default();
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        if capture.framed {
            // Once the frame is whole, anything still arriving is a startup
            // job that outlived the shell; it is diagnostic, not payload.
            if frame_body(&capture.payload).is_ok() {
                push_bounded(
                    &mut capture.noise,
                    &mut capture.noise_truncated,
                    chunk,
                    DIAGNOSTIC_LIMIT,
                );
            } else {
                push_bounded(
                    &mut capture.payload,
                    &mut capture.payload_truncated,
                    chunk,
                    PAYLOAD_LIMIT,
                );
            }
            continue;
        }
        pending.extend_from_slice(chunk);
        match pending
            .windows(MARKER.len())
            .position(|window| window == MARKER)
        {
            Some(position) => {
                let payload = pending.split_off(position);
                push_bounded(
                    &mut capture.noise,
                    &mut capture.noise_truncated,
                    &pending,
                    DIAGNOSTIC_LIMIT,
                );
                push_bounded(
                    &mut capture.payload,
                    &mut capture.payload_truncated,
                    &payload,
                    PAYLOAD_LIMIT,
                );
                pending.clear();
                capture.framed = true;
            }
            None => {
                // Retain the bytes a marker could still straddle across reads.
                let settled = pending.len().saturating_sub(MARKER.len() - 1);
                push_bounded(
                    &mut capture.noise,
                    &mut capture.noise_truncated,
                    &pending[..settled],
                    DIAGNOSTIC_LIMIT,
                );
                pending.drain(..settled);
            }
        }
    }
    push_bounded(
        &mut capture.noise,
        &mut capture.noise_truncated,
        &pending,
        DIAGNOSTIC_LIMIT,
    );
    Ok(capture)
}

fn push_bounded(target: &mut Vec<u8>, truncated: &mut bool, bytes: &[u8], limit: usize) {
    let stored = limit.saturating_sub(target.len()).min(bytes.len());
    target.extend_from_slice(&bytes[..stored]);
    *truncated |= stored != bytes.len();
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

fn join_framed(
    thread: std::thread::JoinHandle<io::Result<FramedCapture>>,
) -> io::Result<FramedCapture> {
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

    fn framed(body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::from(MARKER);
        bytes.extend_from_slice(&(body.len() as u64).to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn parser_accepts_nul_values_and_scrubs_ssh_session_state() {
        let bytes = framed(b"/tmp/project\0PATH=/bin:/usr/bin\0SSH_CONNECTION=secret\0VALUE=a b\0");
        let (cwd, environment) = parse_environment(&bytes).expect("parse");
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

    struct ChunkedReader(std::collections::VecDeque<Vec<u8>>);

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.0.pop_front() else {
                return Ok(0);
            };
            let count = chunk.len().min(buffer.len());
            buffer[..count].copy_from_slice(&chunk[..count]);
            if count < chunk.len() {
                self.0.push_front(chunk[count..].to_vec());
            }
            Ok(count)
        }
    }

    #[test]
    fn framed_drain_separates_chatter_from_a_payload_split_across_reads() {
        let payload = framed(b"/tmp/project\0PATH=/bin\0");
        let mut chunks = vec![b"motd line\n".to_vec(), payload[..3].to_vec()];
        chunks.push(payload[3..].to_vec());
        chunks.push(b"background job output".to_vec());
        let capture = drain_framed(ChunkedReader(chunks.into())).expect("drain");
        // Chatter on either side of the frame stays a diagnostic.
        assert_eq!(capture.noise, b"motd line\nbackground job output");
        assert!(!capture.noise_truncated);
        assert!(!capture.payload_truncated);
        let (cwd, environment) = parse_environment(&capture.payload).expect("parse");
        assert_eq!(cwd, "/tmp/project");
        assert_eq!(environment.len(), 1);
    }

    #[test]
    fn framed_drain_keeps_the_payload_behind_chatter_that_exceeds_the_diagnostic_bound() {
        let payload = framed(b"/tmp/project\0PATH=/bin\0");
        let chunks = vec![vec![b'x'; DIAGNOSTIC_LIMIT * 2], payload];
        let capture = drain_framed(ChunkedReader(chunks.into())).expect("drain");
        assert_eq!(capture.noise.len(), DIAGNOSTIC_LIMIT);
        assert!(capture.noise_truncated);
        let (cwd, _) = parse_environment(&capture.payload).expect("parse");
        assert_eq!(cwd, "/tmp/project");
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
