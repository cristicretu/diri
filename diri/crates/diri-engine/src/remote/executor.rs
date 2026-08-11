//! Bounded, timeout-aware process execution for system OpenSSH.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ssh::CommandSpec;

const DIAGNOSTIC_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ProcessExecutor {
    ssh_executable: OsString,
    askpass_executable: Option<OsString>,
}

impl Default for ProcessExecutor {
    fn default() -> Self {
        Self {
            ssh_executable: OsString::from("ssh"),
            askpass_executable: None,
        }
    }
}

impl ProcessExecutor {
    #[must_use]
    pub fn new(ssh_executable: impl Into<OsString>) -> Self {
        Self {
            ssh_executable: ssh_executable.into(),
            askpass_executable: None,
        }
    }

    /// Configure Diri's UI broker for OpenSSH password, key-passphrase and
    /// host-key prompts. `SSH_ASKPASS_REQUIRE=force` is essential because the
    /// SSH channel's stdin is reserved for the binary Helper protocol.
    #[must_use]
    pub fn with_askpass(mut self, executable: impl Into<OsString>) -> Self {
        self.askpass_executable = Some(executable.into());
        self
    }

    #[must_use]
    pub fn ssh_executable(&self) -> &std::ffi::OsStr {
        &self.ssh_executable
    }

    pub fn run(
        &self,
        mut spec: CommandSpec,
        input: Vec<u8>,
        timeout: Duration,
        stdout_limit: usize,
    ) -> io::Result<CommandOutput> {
        spec.program.clone_from(&self.ssh_executable);
        let mut command = self.command(&spec);
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("SSH stdin is unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("SSH stdout is unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("SSH stderr is unavailable"))?;
        let input_thread = std::thread::spawn(move || -> io::Result<()> {
            stdin.write_all(&input)?;
            stdin.flush()
        });
        let stdout_thread = std::thread::spawn(move || drain_bounded(stdout, stdout_limit));
        let stderr_thread = std::thread::spawn(move || drain_bounded(stderr, DIAGNOSTIC_LIMIT));

        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                terminate_process_group(&mut child);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSH command timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let input_result = input_thread
            .join()
            .map_err(|_| io::Error::other("SSH input thread panicked"))?;
        if status.success() {
            input_result?;
        } else if let Err(error) = input_result {
            // A remote failure commonly closes stdin early. The exit status
            // and bounded diagnostics below are the useful error, not EPIPE.
            let _ = error;
        }
        let (stdout, stdout_truncated) = join_reader(stdout_thread)?;
        let (stderr, stderr_truncated) = join_reader(stderr_thread)?;
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    pub fn open(&self, mut spec: CommandSpec) -> io::Result<SshChannel> {
        spec.program.clone_from(&self.ssh_executable);
        let mut child = self.command(&spec).spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("SSH stdin is unavailable"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("SSH stdout is unavailable"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("SSH stderr is unavailable"))?;
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&diagnostics);
        std::thread::spawn(move || {
            let (bytes, _) = drain_bounded(&mut stderr, DIAGNOSTIC_LIMIT).unwrap_or_default();
            *sink.lock().expect("SSH diagnostics") = bytes;
        });
        Ok(SshChannel {
            child,
            input,
            output,
            diagnostics,
        })
    }

    fn command(&self, spec: &CommandSpec) -> Command {
        let mut command = command(spec);
        if let Some(askpass) = &self.askpass_executable {
            command
                .env("SSH_ASKPASS", askpass)
                .env("SSH_ASKPASS_REQUIRE", "force");
            // OpenSSH implementations commonly retain the historical DISPLAY
            // gate even when askpass is explicitly forced. It is only a
            // presence check; the native macOS broker does not contact X11.
            if std::env::var_os("DISPLAY").is_none() {
                command.env("DISPLAY", "diri-native-askpass");
            }
        }
        command
    }
}

fn command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.arguments);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: this post-fork closure invokes only the async-signal-safe
    // `setsid` syscall. A dedicated process group lets cancellation reap ssh
    // plus any local ProxyCommand/askpass descendants without touching the
    // Engine's own group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

pub(crate) fn terminate_process_group(child: &mut Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `pid` is the group leader created by `command`; a negative
        // pid targets precisely that process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl CommandOutput {
    pub fn require_success(self, phase: &str) -> io::Result<Self> {
        if self.status.success() {
            Ok(self)
        } else {
            Err(io::Error::other(format!(
                "{phase} failed with {}: {}",
                self.status,
                String::from_utf8_lossy(&self.stderr)
            )))
        }
    }
}

pub struct SshChannel {
    pub child: Child,
    pub input: ChildStdin,
    pub output: ChildStdout,
    diagnostics: Arc<Mutex<Vec<u8>>>,
}

impl SshChannel {
    #[must_use]
    pub fn diagnostics(&self) -> Vec<u8> {
        self.diagnostics.lock().expect("SSH diagnostics").clone()
    }

    pub fn terminate(&mut self) {
        terminate_process_group(&mut self.child);
    }
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
        .map_err(|_| io::Error::other("SSH output thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_bounded_while_the_child_is_fully_drained() {
        let executor = ProcessExecutor::new("/bin/sh");
        let spec = CommandSpec {
            program: "ignored".into(),
            arguments: vec!["-c".into(), "head -c 10000 /dev/zero".into()],
        };
        let output = executor
            .run(spec, Vec::new(), Duration::from_secs(2), 128)
            .expect("run")
            .require_success("fixture")
            .expect("success");
        assert_eq!(output.stdout.len(), 128);
        assert!(output.stdout_truncated);
    }

    #[test]
    fn timeout_terminates_a_stuck_command() {
        let executor = ProcessExecutor::new("/bin/sh");
        let spec = CommandSpec {
            program: "ignored".into(),
            arguments: vec!["-c".into(), "sleep 10".into()],
        };
        let error = executor
            .run(spec, Vec::new(), Duration::from_millis(20), 128)
            .expect_err("timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn authentication_diagnostics_are_bounded_and_propagated() {
        let executor = ProcessExecutor::new("/bin/sh");
        let spec = CommandSpec {
            program: "ignored".into(),
            arguments: vec![
                "-c".into(),
                "printf 'authentication-required' >&2; exit 255".into(),
            ],
        };
        let error = executor
            .run(spec, Vec::new(), Duration::from_secs(2), 128)
            .expect("command result")
            .require_success("remote authentication")
            .expect_err("authentication failure");
        assert!(error.to_string().contains("authentication-required"));
    }

    #[test]
    fn askpass_is_forced_without_consuming_protocol_stdin() {
        let executor = ProcessExecutor::new("/bin/sh").with_askpass("/tmp/diri-askpass");
        let spec = CommandSpec {
            program: "ignored".into(),
            arguments: vec![
                "-c".into(),
                "printf '%s|%s|%s' \"$SSH_ASKPASS\" \"$SSH_ASKPASS_REQUIRE\" \"$DISPLAY\"".into(),
            ],
        };
        let output = executor
            .run(spec, Vec::new(), Duration::from_secs(2), 256)
            .expect("askpass environment");
        let fields = String::from_utf8(output.stdout).expect("utf8");
        assert!(fields.starts_with("/tmp/diri-askpass|force|"));
        assert!(!fields.ends_with('|'));
    }
}
