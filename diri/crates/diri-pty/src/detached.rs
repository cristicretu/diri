//! macOS: an Agent started as a launchd job of its own, on a PTY this process
//! keeps.
//!
//! macOS puts every process an app spawns in the app's coalition, and
//! children inherit it. LaunchServices force-quits a coalition as one app: on
//! 2026-10-04 force-quitting a Chrome that an Agent's browser MCP server had
//! started SIGTERMed 184 processes, every session's Agent among them, because
//! they all descended from the one Holder manager. A launchd job is the only
//! way into a fresh coalition without private entitlements, so each Agent is
//! started as one, and a force-quit now reaches only that Agent and what it
//! launched.
//!
//! No process is added. The job's program is the app's own main executable
//! run with [`HELPER_FLAG`]: it forks (launchd starts a job as a process-group
//! leader, which may not call `setsid`), the job's process exits at once, and
//! the child takes the PTY as its controlling terminal and execs the Agent.
//! TCC keeps crediting the job's program, diri.app, through the fork and the
//! exec, so the Agents keep diri's privacy grants.
//!
//! The PTY master never leaves this process. The helper connects back to a
//! one-shot socket in the caller's private directory, proves it is the
//! launcher running as this user, and receives argv, environment, cwd and the
//! slave descriptor over it (nothing sensitive touches the disk). It execs
//! only on a "go" byte sent after this process has armed an exit watch on its
//! pid, so no exit can be missed. The Agent is launchd's child, not ours: its
//! wait status comes from `EVFILT_PROC` `NOTE_EXITSTATUS`, which XNU reports to
//! any watcher, and launchd reaps it.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Exit, PtySpec};

/// The argument that runs the app's main executable as the Agent helper:
/// `<app> --exec-agent <socket>`.
pub const HELPER_FLAG: &str = "--exec-agent";
/// Every job label starts with this.
pub const LABEL_PREFIX: &str = "com.dirijor.diri.agent.";

const LAUNCHCTL: &str = "/bin/launchctl";
/// How long the helper has to call back after `launchctl bootstrap`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long each step of the handshake may take once connected.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// argv + environment + cwd: far above any real launch, far below abuse.
const MAX_PAYLOAD: usize = 8 << 20;
/// The handoff's version, its first byte. A manager outlives app updates, so
/// it may meet a newer helper (or, after a downgrade, an older one): a helper
/// that does not know the version exits without running anything, and the
/// manager, seeing nothing started, falls back to a direct spawn. Newer
/// helpers must keep accepting every version a live manager may send.
const HANDOFF_VERSION: u8 = 1;
const READY: u8 = 0;
const SETUP_FAILED: u8 = 1;
const GO: u8 = 1;

/// Where the helper lives and where its one-shot sockets go.
#[derive(Clone, Debug)]
pub struct Launcher {
    /// The app's main executable (TCC credits the job to it).
    pub helper: PathBuf,
    /// A private (0700) directory short enough for a socket path.
    pub rendezvous: PathBuf,
}

/// Why a detached launch did not start the Agent.
#[derive(Debug)]
pub enum DetachedError {
    /// Nothing was executed: starting the Agent another way is safe.
    Unavailable(io::Error),
    /// The helper ran the launch and it failed (an `exec` error, a missing
    /// cwd), exactly as a direct spawn would have.
    Spawn(io::Error),
}

/// The Agent process of a detached launch: launchd's child, watched by pid.
pub struct DetachedLeader {
    pid: u32,
    watch: OwnedFd,
    state: Mutex<WatchState>,
    changed: Condvar,
}

#[derive(Default)]
struct WatchState {
    exit: Option<Exit>,
    /// One thread at a time blocks in `kevent`; a `try_wait` meanwhile must
    /// not consume the event out from under it.
    blocked: bool,
}

impl DetachedLeader {
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// A descriptor that turns readable once the Agent has exited, so a
    /// caller can wait for the exit alongside other descriptors and then
    /// collect it with [`Self::wait`] without blocking.
    #[must_use]
    pub fn exit_descriptor(&self) -> RawFd {
        self.watch.as_raw_fd()
    }

    /// The exit, if it has happened. Never blocks.
    pub fn try_wait(&self) -> io::Result<Option<Exit>> {
        let mut state = self.state.lock().expect("detached leader");
        if state.exit.is_some() || state.blocked {
            return Ok(state.exit);
        }
        state.exit = self.collect(Some(Duration::ZERO))?;
        Ok(state.exit)
    }

    /// Blocks until the Agent exits.
    pub fn wait(&self) -> io::Result<Exit> {
        let mut state = self.state.lock().expect("detached leader");
        loop {
            if let Some(exit) = state.exit {
                return Ok(exit);
            }
            if state.blocked {
                state = self.changed.wait(state).expect("detached leader");
                continue;
            }
            state.blocked = true;
            drop(state);
            let collected = self.collect(None);
            state = self.state.lock().expect("detached leader");
            state.blocked = false;
            self.changed.notify_all();
            match collected {
                Ok(Some(exit)) => state.exit = Some(exit),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn collect(&self, timeout: Option<Duration>) -> io::Result<Option<Exit>> {
        let timespec = timeout.map(|timeout| libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: libc::c_long::from(timeout.subsec_nanos() as i32),
        });
        // SAFETY: zeroed kevent is a valid out-param.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: one writable event; the timeout is null or a valid
            // timespec on this stack frame.
            let count = unsafe {
                libc::kevent(
                    self.watch.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    timespec
                        .as_ref()
                        .map_or(std::ptr::null(), |timespec| timespec as *const _),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                return Ok(None);
            }
            if event.fflags & libc::NOTE_EXIT != 0 {
                return Ok(Some(decode_wait_status(event.data as i32)));
            }
        }
    }
}

/// The same decode the Holder applies to `waitpid`, bit for bit.
fn decode_wait_status(status: i32) -> Exit {
    if status & 0x7F != 0 {
        Exit::Signal(status & 0x7F)
    } else {
        Exit::Code((status >> 8) & 0xFF)
    }
}

/// Starts `spec` as its own launchd job on a fresh PTY and returns the PTY.
pub fn spawn(spec: &PtySpec, launcher: &Launcher) -> Result<super::Pty, DetachedError> {
    use DetachedError::{Spawn, Unavailable};
    let (master, slave) = super::unix::open_pair(spec).map_err(Unavailable)?;
    let nonce = nonce();
    let socket_path = launcher.rendezvous.join(format!("a{nonce:x}.sock"));
    let listener = UnixListener::bind(&socket_path).map_err(Unavailable)?;
    // The directory is private already; the socket is too, and gone as soon
    // as the helper is in (or the launch is abandoned).
    let _socket = RemoveOnDrop(&socket_path);
    let label = format!("{LABEL_PREFIX}{nonce:x}");
    bootstrap(&label, &launcher.helper, &socket_path, &launcher.rendezvous).map_err(Unavailable)?;
    let _job = BootoutOnDrop(&label);

    let mut stream = accept_within(&listener, CONNECT_TIMEOUT).map_err(Unavailable)?;
    drop(listener);
    let pid = verify_peer(&stream, &launcher.helper).map_err(Unavailable)?;
    // Armed before the Agent can run: it execs only on the "go" below.
    let watch = watch_exit(pid).map_err(Unavailable)?;
    stream
        .set_read_timeout(Some(STEP_TIMEOUT))
        .map_err(Unavailable)?;
    stream
        .set_write_timeout(Some(STEP_TIMEOUT))
        .map_err(Unavailable)?;

    send_with_fd(&stream, &encode(spec), slave.as_raw_fd()).map_err(Unavailable)?;
    drop(slave);
    let mut reply = [0u8; 1];
    stream.read_exact(&mut reply).map_err(Unavailable)?;
    if reply[0] == SETUP_FAILED {
        return Err(Spawn(read_errno(&mut stream)));
    }
    if reply[0] != READY {
        return Err(Unavailable(io::Error::other(
            "helper sent an unknown reply",
        )));
    }
    stream.write_all(&[GO]).map_err(Unavailable)?;
    // The socket is close-on-exec: EOF means the exec happened, or that the
    // helper explained on the terminal why it could not and exited 127/126.
    // Four bytes are an older helper's errno for an exec that did not.
    let mut errno = Vec::new();
    stream.read_to_end(&mut errno).map_err(Spawn)?;
    if let Ok(bytes) = <[u8; 4]>::try_from(errno.as_slice()) {
        return Err(Spawn(io::Error::from_raw_os_error(i32::from_le_bytes(
            bytes,
        ))));
    }

    let leader = DetachedLeader {
        pid,
        watch,
        state: Mutex::new(WatchState::default()),
        changed: Condvar::new(),
    };
    let identity = crate::process_identity::observe(pid).ok();
    Ok(super::Pty::detached(master, leader, identity))
}

struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// The job's own process exits as soon as it has forked, so the job is
/// finished by the time anything is decided here; booting it out never
/// touches the Agent.
struct BootoutOnDrop<'a>(&'a str);

impl Drop for BootoutOnDrop<'_> {
    fn drop(&mut self) {
        let _ = launchctl(&["bootout", &format!("{}/{}", gui_domain(), self.0)]);
    }
}

fn nonce() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 40)
}

fn bootstrap(label: &str, helper: &Path, socket: &Path, directory: &Path) -> io::Result<()> {
    bootstrap_job(
        label,
        &[
            helper.as_os_str(),
            OsStr::new(HELPER_FLAG),
            socket.as_os_str(),
        ],
        &[],
        directory,
    )
}

/// Starts `program` once as a transient launchd job labelled `label`, in a
/// process coalition of its own. The plist goes through `directory` (private)
/// and is deleted at once; the job never restarts and is gone at logout. A
/// job inherits nothing from its caller's environment but `environment`.
pub fn bootstrap_job(
    label: &str,
    program: &[&OsStr],
    environment: &[(&str, String)],
    directory: &Path,
) -> io::Result<()> {
    let plist_path = directory.join(format!("{label}.plist"));
    let plist = job_plist(label, program, environment);
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&plist_path)?;
        file.write_all(plist.as_bytes())?;
    }
    let result = launchctl(&["bootstrap", &gui_domain(), &plist_path.to_string_lossy()]);
    let _ = std::fs::remove_file(&plist_path);
    result
}

/// Removes the job `label`, stopping its process if it has one.
pub fn bootout_job(label: &str) -> io::Result<()> {
    launchctl(&["bootout", &format!("{}/{label}", gui_domain())])
}

/// What launchd says about a job, for diagnosing one that never ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobStatus {
    /// `launchctl print` knew the job at all.
    pub known: bool,
    /// The job's `state = …`, reduced to a fixed vocabulary.
    pub state: &'static str,
    /// `last exit code = N`, once its process has run and exited.
    pub last_exit: Option<i64>,
    /// `runs = N`: how many times launchd started its process.
    pub runs: Option<i64>,
}

/// Reads [`JobStatus`] from `launchctl print gui/<uid>/<label>`.
#[must_use]
pub fn job_status(label: &str) -> JobStatus {
    let Ok(output) = Command::new(LAUNCHCTL)
        .args(["print", &format!("{}/{label}", gui_domain())])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return JobStatus::default();
    };
    if !output.status.success() {
        return JobStatus::default();
    }
    parse_job_status(&String::from_utf8_lossy(&output.stdout))
}

fn parse_job_status(print: &str) -> JobStatus {
    let mut status = JobStatus {
        known: true,
        state: "unknown",
        ..JobStatus::default()
    };
    // Only the job's own keys: nested blocks indent further and reuse names.
    for line in print
        .lines()
        .filter(|line| line.starts_with('\t') && !line.starts_with("\t\t"))
    {
        let Some((key, value)) = line.trim().split_once(" = ") else {
            continue;
        };
        let value = value.trim();
        match key {
            "state" => {
                status.state = match value {
                    "running" => "running",
                    "not running" => "not_running",
                    "spawn scheduled" => "spawn_scheduled",
                    "spawning" => "spawning",
                    "exited" => "exited",
                    "waiting" => "waiting",
                    _ => "other",
                };
            }
            // "78: EX_CONFIG": the number leads.
            "last exit code" => {
                status.last_exit = value
                    .split(':')
                    .next()
                    .and_then(|code| code.trim().parse().ok());
            }
            "runs" => status.runs = value.parse().ok(),
            _ => {}
        }
    }
    status
}

/// Boots out every job labelled `<prefix><hex millis>` that has finished and
/// is older than `age`. A running job is never touched, nor one young
/// enough to be between `bootstrap` and its spawn.
pub fn sweep_finished_jobs(prefix: &str, age: Duration) {
    let Ok(output) = Command::new(LAUNCHCTL)
        .arg("list")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    for label in finished_jobs(
        &String::from_utf8_lossy(&output.stdout),
        prefix,
        now_millis(),
        age,
    ) {
        let _ = launchctl(&["bootout", &format!("{}/{label}", gui_domain())]);
    }
}

/// Whether `pid` is the running process of a job labelled `<prefix>…` in
/// this process's launchd domain; `None` when `launchctl list` fails.
#[must_use]
pub fn is_job_process(prefix: &str, pid: u32) -> Option<bool> {
    let output = Command::new(LAUNCHCTL)
        .arg("list")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| job_runs_pid(&String::from_utf8_lossy(&output.stdout), prefix, pid))
}

fn job_runs_pid(list: &str, prefix: &str, pid: u32) -> bool {
    let pid = pid.to_string();
    list.lines().any(|line| {
        let mut columns = line.split('\t');
        columns.next().map(str::trim) == Some(pid.as_str())
            && columns
                .nth(1)
                .is_some_and(|label| label.trim().starts_with(prefix))
    })
}

/// The launchd session this process runs in (`launchctl managername`):
/// `aqua` for the logged-in GUI, where `open` and the pasteboard work;
/// `none` once that session is gone.
#[must_use]
pub fn session_type() -> &'static str {
    let Ok(output) = Command::new(LAUNCHCTL)
        .arg("managername")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return "unknown";
    };
    if !output.status.success() {
        return "none";
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "Aqua" => "aqua",
        "Background" => "background",
        "LoginWindow" => "loginwindow",
        "StandardIO" => "standardio",
        _ => "other",
    }
}

/// A label suffix that orders by creation: the current time in hex millis.
#[must_use]
pub fn label_suffix() -> String {
    format!("{:x}", now_millis())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The labels in `launchctl list` rows (`PID\tStatus\tLabel`) with `prefix`
/// and a hex-millis suffix, not running, and older than `age`.
fn finished_jobs<'a>(list: &'a str, prefix: &str, now: u64, age: Duration) -> Vec<&'a str> {
    list.lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let pid = columns.next()?.trim();
            let _status = columns.next()?;
            let label = columns.next()?.trim();
            let created = u64::from_str_radix(label.strip_prefix(prefix)?, 16).ok()?;
            let old = now.saturating_sub(created) > age.as_millis() as u64;
            (pid == "-" && old).then_some(label)
        })
        .collect()
}

/// A job that runs once and is never restarted. `Interactive`: an Agent is
/// foreground work and must not be throttled as a background daemon.
fn job_plist(label: &str, program: &[&OsStr], environment: &[(&str, String)]) -> String {
    let environment = if environment.is_empty() {
        String::new()
    } else {
        let pairs: String = environment
            .iter()
            .map(|(key, value)| {
                format!(
                    "    <key>{}</key>\n    <string>{}</string>\n",
                    xml_escape(key),
                    xml_escape(value)
                )
            })
            .collect();
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{pairs}  </dict>\n")
    };
    let arguments: String = program
        .iter()
        .map(|argument| {
            format!(
                "    <string>{}</string>\n",
                xml_escape(&argument.to_string_lossy())
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <false/>
  <key>AbandonProcessGroup</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
{environment}</dict>
</plist>
"#,
        label = xml_escape(label),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn gui_domain() -> String {
    // SAFETY: getuid has no failure mode.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(arguments: &[&str]) -> io::Result<()> {
    let output = Command::new(LAUNCHCTL)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "launchctl {}: {} {}",
        arguments.first().copied().unwrap_or_default(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn accept_within(listener: &UnixListener, timeout: Duration) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the Agent helper did not call back",
            ));
        }
        let mut descriptor = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().clamp(1, 1_000) as libc::c_int;
        // SAFETY: one valid pollfd for the call.
        if unsafe { libc::poll(&mut descriptor, 1, millis) } > 0 {
            let (stream, _) = listener.accept()?;
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
    }
}

/// The caller is this user running the launcher, not anyone who found the
/// socket. Returns its pid, which is the Agent's once it execs.
fn verify_peer(stream: &UnixStream, helper: &Path) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: two writable integers for the call.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getuid has no failure mode.
    if uid != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "helper runs as another user",
        ));
    }
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: a writable pid_t and its length.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is the documented maximum.
    let written =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(written as usize);
    let running = std::fs::canonicalize(OsStr::from_bytes(&buffer))?;
    if running != std::fs::canonicalize(helper)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the caller is not the Agent helper",
        ));
    }
    u32::try_from(pid).map_err(|_| io::Error::other("negative peer pid"))
}

fn watch_exit(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: kqueue takes no inputs.
    let descriptor = unsafe { libc::kqueue() };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor, owned once.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let event = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: valid descriptor and one input event; no output requested.
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

/// `[u32 n][n × (u32 len, bytes)]` for argv, then env as alternating key and
/// value, then cwd as a one-item list. Little-endian lengths.
fn encode(spec: &PtySpec) -> Vec<u8> {
    let mut out = Vec::new();
    let list = |items: &mut dyn Iterator<Item = &[u8]>, out: &mut Vec<u8>| {
        let items: Vec<&[u8]> = items.collect();
        out.extend_from_slice(&(items.len() as u32).to_le_bytes());
        for item in items {
            out.extend_from_slice(&(item.len() as u32).to_le_bytes());
            out.extend_from_slice(item);
        }
    };
    list(
        &mut spec.argv.iter().map(|value| value.as_bytes()),
        &mut out,
    );
    list(
        &mut spec
            .env
            .iter()
            .flat_map(|(key, value)| [key.as_bytes(), value.as_bytes()]),
        &mut out,
    );
    list(
        &mut std::iter::once(spec.cwd.as_os_str().as_bytes()),
        &mut out,
    );
    let mut framed = vec![HANDOFF_VERSION];
    framed.extend_from_slice(&(out.len() as u32).to_le_bytes());
    framed.extend_from_slice(&out);
    framed
}

struct Decoded {
    argv: Vec<CString>,
    env: Vec<CString>,
    path: Option<Vec<u8>>,
    cwd: CString,
}

fn decode(payload: &[u8]) -> Option<Decoded> {
    let mut rest = payload;
    let mut take = |count: usize| -> Option<&[u8]> {
        let (head, tail) = rest.split_at_checked(count)?;
        rest = tail;
        Some(head)
    };
    let mut list = || -> Option<Vec<Vec<u8>>> {
        let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        (0..count)
            .map(|_| {
                let length = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
                Some(take(length)?.to_vec())
            })
            .collect()
    };
    let argv = list()?;
    let env = list()?;
    let cwd = list()?.pop()?;
    if argv.is_empty() || env.len() % 2 != 0 {
        return None;
    }
    let path = env
        .chunks(2)
        .find(|pair| pair[0] == b"PATH")
        .map(|pair| pair[1].clone());
    let env = env
        .chunks(2)
        .map(|pair| CString::new([pair[0].as_slice(), b"=", pair[1].as_slice()].concat()).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(Decoded {
        argv: argv
            .into_iter()
            .map(|value| CString::new(value).ok())
            .collect::<Option<_>>()?,
        env,
        path,
        cwd: CString::new(cwd).ok()?,
    })
}

fn send_with_fd(stream: &UnixStream, payload: &[u8], fd: RawFd) -> io::Result<()> {
    // The descriptor rides on the first byte; the rest is a plain write.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    // SAFETY: zeroed msghdr, then every pointer set to live local buffers.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as libc::socklen_t;
    // SAFETY: the control buffer holds exactly one SCM_RIGHTS header + fd.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
    }
    // SAFETY: the message references the buffers above for the call.
    if unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) } != 1 {
        return Err(io::Error::last_os_error());
    }
    (&*stream).write_all(&payload[1..])
}

fn read_errno(stream: &mut UnixStream) -> io::Error {
    let mut bytes = [0u8; 4];
    match stream.read_exact(&mut bytes) {
        Ok(()) => io::Error::from_raw_os_error(i32::from_le_bytes(bytes)),
        Err(error) => error,
    }
}

/// Runs the Agent helper when this process was started as one, and never
/// returns then. Call first thing in the app's `main`.
pub fn run_helper_if_requested() {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().is_none_or(|flag| flag != HELPER_FLAG) {
        return;
    }
    let socket: Option<OsString> = arguments.next();
    // SAFETY: fork in a process that has started nothing yet. The job's own
    // process (a process-group leader, which may not `setsid`) leaves at once.
    match unsafe { libc::fork() } {
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }
    let code = socket.map_or(0, |socket| helper(Path::new(&socket)));
    // SAFETY: nothing to flush or unwind in the helper.
    unsafe { libc::_exit(code) };
}

/// The forked helper: everything up to the Agent's exec. Returns an exit
/// code only when no exec happened.
fn helper(socket: &Path) -> i32 {
    // SAFETY: the forked child is single-threaded and owns its session.
    if unsafe { libc::setsid() } < 0 {
        return 0;
    }
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return 0;
    };
    let _ = stream.set_read_timeout(Some(STEP_TIMEOUT));
    let Some((slave, payload)) = receive(&stream) else {
        return 0;
    };
    let Some(launch) = decode(&payload) else {
        return 0;
    };
    let fail = |stream: &mut UnixStream, error: io::Error| {
        let errno = error.raw_os_error().unwrap_or(libc::EIO);
        let _ = stream.write_all(&[SETUP_FAILED]);
        let _ = stream.write_all(&errno.to_le_bytes());
        0
    };
    // SAFETY: plain syscalls on descriptors this process owns.
    unsafe {
        if libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) < 0 {
            return fail(&mut stream, io::Error::last_os_error());
        }
        for target in 0..3 {
            if libc::dup2(slave.as_raw_fd(), target) < 0 {
                return fail(&mut stream, io::Error::last_os_error());
            }
        }
        let _ = libc::tcsetpgrp(0, libc::getpid());
        if libc::chdir(launch.cwd.as_ptr()) < 0 {
            return fail(&mut stream, io::Error::last_os_error());
        }
    }
    drop(slave);
    let plan = crate::exec::ExecPlan::new(
        launch.argv.clone(),
        launch.env.clone(),
        launch.path.as_deref(),
    );
    if stream.write_all(&[READY]).is_err() {
        return 0;
    }
    let mut go = [0u8; 1];
    if stream.read_exact(&mut go).is_err() || go[0] != GO {
        return 0;
    }
    // SAFETY: plain process-state setup before exec, as a direct spawn does.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        for signal in 1..32 {
            libc::signal(signal, libc::SIG_DFL);
        }
        let keep = stream.as_raw_fd();
        libc::fcntl(keep, libc::F_SETFD, libc::FD_CLOEXEC);
        for fd in 3..libc::getdtablesize() {
            if fd != keep {
                libc::close(fd);
            }
        }
    }
    // A program that cannot start is explained on the terminal and ends with
    // the shell's status (127, or 126), like a direct spawn: the launch
    // itself succeeded, so nothing goes back on the stream and its EOF reads
    // as an exec. An older parent handles that the same way.
    // SAFETY: plain execve and write in this single-threaded helper.
    unsafe {
        let errno = plan.exec();
        plan.report(errno)
    }
}

/// The framed payload and the descriptor sent with its first byte.
fn receive(stream: &UnixStream) -> Option<(OwnedFd, Vec<u8>)> {
    let mut first = [0u8; 1];
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: first.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: zeroed msghdr pointing at live local buffers.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as libc::socklen_t;
    // SAFETY: the message references the buffers above.
    if unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) } != 1 {
        return None;
    }
    // SAFETY: the kernel filled the control buffer; read one SCM_RIGHTS fd.
    let fd = unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return None;
        }
        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>())
    };
    // SAFETY: SCM_RIGHTS installed a fresh descriptor in this process.
    let slave = unsafe { OwnedFd::from_raw_fd(fd) };
    if first[0] != HANDOFF_VERSION {
        return None; // a handoff this helper does not speak: start nothing
    }
    let mut length = [0u8; 4];
    (&*stream).read_exact(&mut length).ok()?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_PAYLOAD {
        return None;
    }
    let mut payload = vec![0u8; length];
    (&*stream).read_exact(&mut payload).ok()?;
    Some((slave, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_round_trips_and_finds_its_own_path() {
        let spec = PtySpec::new(vec!["claude".into(), "--a b".into()], "/tmp/x y")
            .env("PATH", "/opt/bin:/usr/bin")
            .env("EMPTY", "");
        let framed = encode(&spec);
        assert_eq!(framed[0], HANDOFF_VERSION, "the version leads");
        let length = u32::from_le_bytes(framed[1..5].try_into().unwrap()) as usize;
        assert_eq!(length, framed.len() - 5);
        let decoded = decode(&framed[5..]).unwrap();
        assert_eq!(decoded.argv[1].as_bytes(), b"--a b");
        assert_eq!(decoded.cwd.as_bytes(), b"/tmp/x y");
        assert_eq!(
            decoded.path.as_deref(),
            Some(b"/opt/bin:/usr/bin".as_slice())
        );
        assert_eq!(decoded.env[1].as_bytes(), b"EMPTY=");
        assert!(decode(&framed[5..framed.len() - 1]).is_none(), "truncated");
    }

    #[test]
    fn job_status_reads_only_the_jobs_own_keys() {
        let print = "gui/501/com.dirijor.diri.holders.1 = {\n\
                     \tactive count = 0\n\
                     \tstate = spawn scheduled\n\
                     \truns = 3\n\
                     \tlast exit code = 78: EX_CONFIG\n\
                     \tenvironment = {\n\
                     \t\tstate = running\n\
                     \t}\n\
                     }\n";
        let status = parse_job_status(print);
        assert!(status.known);
        assert_eq!(status.state, "spawn_scheduled");
        assert_eq!(status.runs, Some(3));
        assert_eq!(status.last_exit, Some(78));
        let status = parse_job_status("x = {\n\tstate = not running\n\tlast exit code = 127\n}\n");
        assert_eq!((status.state, status.last_exit), ("not_running", Some(127)));
    }

    #[test]
    fn wait_statuses_decode_like_waitpid() {
        assert_eq!(decode_wait_status(42 << 8), Exit::Code(42));
        assert_eq!(decode_wait_status(0), Exit::Code(0));
        assert_eq!(
            decode_wait_status(libc::SIGTERM),
            Exit::Signal(libc::SIGTERM)
        );
        assert_eq!(
            decode_wait_status(libc::SIGKILL | 0x80),
            Exit::Signal(libc::SIGKILL)
        );
    }

    #[test]
    fn only_finished_old_jobs_are_swept() {
        let now = 0x1_0000_0000u64;
        let old = format!("{:x}", now - 600_000);
        let young = format!("{:x}", now - 1_000);
        let list = format!(
            "PID\tStatus\tLabel\n-\t0\tp.{old}\n42\t0\tp.{old}\n-\t0\tp.{young}\n-\t-15\tq.{old}\n-\t0\tp.junk\n"
        );
        assert_eq!(
            finished_jobs(&list, "p.", now, Duration::from_secs(120)),
            vec![format!("p.{old}")]
        );
    }

    #[test]
    fn a_pid_counts_only_as_its_own_prefixed_job() {
        let list = "PID\tStatus\tLabel\n-\t0\tp.1\n42\t0\tp.2\n7\t0\tq.1\n420\t0\tp.3\n";
        assert!(job_runs_pid(list, "p.", 42));
        assert!(!job_runs_pid(list, "p.", 7));
        assert!(!job_runs_pid(list, "p.", 4));
        assert!(!job_runs_pid(list, "p.", 1));
    }

    #[test]
    fn the_job_runs_once_and_escapes_its_arguments() {
        let plist = job_plist(
            "l",
            &[OsStr::new("/A & B/diri"), OsStr::new("<x>")],
            &[("K", "a&b".to_owned())],
        );
        assert!(plist.contains("<key>K</key>\n    <string>a&amp;b</string>"));
        assert!(!job_plist("l", &[], &[]).contains("EnvironmentVariables"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <false/>"));
        assert!(plist.contains("<string>/A &amp; B/diri</string>"));
        assert!(plist.contains("<string>&lt;x&gt;</string>"));
    }
}
