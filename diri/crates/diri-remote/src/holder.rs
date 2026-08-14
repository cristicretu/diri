//! One PTY, terminal parser, process tree and controller per Holder process.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use diri_proto::frames::{Frame, FrameType, MAX_FRAME_BYTES};
use diri_proto::remote_pty::{
    ControlGranted, ControlRevoked, DeliveryAccepted, DeliveryUncertain, FullSnapshot, GridDelta,
    Hello, HelloAck, LaunchRequest, LaunchResult, ProcessExit, RemoteCapability, RemoteCodec,
    RemoteError, RemoteMessage, RemoteProcessState, ScrollbackResponse,
    validate_terminal_dimensions,
};
use diri_pty::{Exit, ExitWatcher, Pty, PtySpec, PtyStream};
use diri_terminal_state::HeadlessScreen;
use serde::{Deserialize, Serialize};

use crate::BUILD_ID;
use crate::output_log::OutputLog;
use crate::paths::{SessionPaths, StatePaths, open_private_truncate};
use crate::state::{
    SessionState, acquire_lock, authenticate, initialize_auth, random_hex, read_state,
    remove_stale_socket, write_state,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// One 120 Hz display interval. Remote links still coalesce sustained output,
/// but never force a ProMotion client down to 60 fps.
const DIFF_COALESCE: Duration = Duration::from_millis(8);
const INTERACTIVE_GRID_BUDGET: u8 = 2;
const MAX_OUTBOUND_BYTES: usize = 20 << 20;
const MAX_PENDING_INPUT_BYTES: usize = 1 << 20;
const REPLAY_BUDGET_BYTES: usize = 4 << 20;
const PERSIST_OFFSET_INTERVAL: u64 = 1 << 20;

const DELIVERY_RECEIPT_LIMIT: usize = 64;

pub const PHASE_ONE_CAPABILITIES: &[RemoteCapability] = &[
    RemoteCapability::FullSnapshot,
    RemoteCapability::IncrementalGrid,
    RemoteCapability::ProcessExit,
    RemoteCapability::Signal,
    RemoteCapability::ControllerLease,
    RemoteCapability::Scrollback,
    RemoteCapability::DeliveryReceipts,
];

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HolderStart {
    request: LaunchRequest,
    incarnation: String,
}

pub fn launch(request: LaunchRequest, executable: &std::path::Path) -> io::Result<LaunchResult> {
    request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let roots = StatePaths::resolve()?;
    let paths = roots.session(&request.session_id)?;
    paths.ensure()?;
    let _launch_lock = acquire_lock(&paths.launch_lock)?;

    if let Ok(state) = read_state(&paths.state)
        && crate::state::holder_lock_held(&paths.lock)?
        && paths.socket.exists()
    {
        if !authenticate(&paths, &request.session_token)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session token does not match the live Holder",
            ));
        }
        validate_live_build_id(&state.holder_build_id)?;
        let RemoteProcessState::Running { pid } = state.process_state else {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the requested session already exists and has exited",
            ));
        };
        return Ok(LaunchResult {
            session_id: state.session_id,
            session_incarnation: state.session_incarnation,
            holder_pid: state.holder_pid,
            process_pid: pid,
            persistence: state.persistence,
        });
    }
    reset_dead_session(&paths)?;
    initialize_auth(&paths, &request.session_token)?;

    let incarnation = random_hex(16)?;
    let start = HolderStart {
        request,
        incarnation: incarnation.clone(),
    };
    let diagnostics = open_private_truncate(&paths.diagnostics)?;
    let encoded = serde_json::to_vec(&start)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut child = if start.request.persistence
        == diri_proto::remote_pty::PersistenceCapability::UserSupervisor
    {
        let start_written = (|| {
            let mut start_file = crate::paths::create_private_file(&paths.holder_start)?;
            start_file.write_all(&encoded)?;
            start_file.sync_all()
        })();
        if let Err(error) = start_written {
            let _ = fs::remove_file(&paths.holder_start);
            return Err(error);
        }
        drop(diagnostics);
        if !crate::persistence::launch_holder(executable, &start.request.session_id, &roots.root)? {
            let _ = fs::remove_file(&paths.holder_start);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the detected transient user supervisor is no longer available",
            ));
        }
        None
    } else {
        let mut command = Command::new(executable);
        command
            .arg("__holder")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics));
        // SAFETY: the closure runs after fork and uses only async-signal-safe
        // syscalls. `setsid` detaches the Holder from the SSH controlling
        // session; capability probing determines whether the host preserves it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Holder stdin is unavailable")
        })?;
        stdin.write_all(&encoded)?;
        drop(stdin);
        Some(child)
    };

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(state) = read_state(&paths.state)
            && state.session_incarnation == incarnation
            && paths.socket.exists()
        {
            let RemoteProcessState::Running { pid } = state.process_state else {
                return Err(io::Error::other("Holder child exited during launch"));
            };
            return Ok(LaunchResult {
                session_id: state.session_id,
                session_incarnation: state.session_incarnation,
                holder_pid: state.holder_pid,
                process_pid: pid,
                persistence: state.persistence,
            });
        }
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            return Err(io::Error::other(format!(
                "Holder exited during launch with {status}: {}",
                holder_diagnostic(&paths.diagnostics)
            )));
        }
        if Instant::now() >= deadline {
            if let Some(mut child) = child {
                terminate_process_group(child.id());
                let _ = child.wait();
            } else {
                crate::persistence::cleanup_holder(&start.request.session_id);
                let _ = fs::remove_file(&paths.holder_start);
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Holder did not become ready within five seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn validate_live_build_id(holder_build_id: &str) -> io::Result<()> {
    if holder_build_id == BUILD_ID {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the live Holder uses Helper build {holder_build_id}; this Helper is {BUILD_ID}"
            ),
        ))
    }
}

fn reset_dead_session(paths: &SessionPaths) -> io::Result<()> {
    for path in [
        &paths.socket,
        &paths.auth,
        &paths.output,
        &paths.diagnostics,
        &paths.holder_start,
    ] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to reset a symlinked session path",
                ));
            }
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_socket() => {
                fs::remove_file(path)?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to reset an unexpected session path",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    // Keep the bounded two-phase delivery ledger in the old state file. A
    // replacement Holder adopts it before overwriting the rest of the stale
    // process metadata, so an unresolved Enter can never become new input.
    match fs::symlink_metadata(&paths.state) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to retain an unexpected session state path",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn holder_diagnostic(path: &std::path::Path) -> String {
    let bytes = fs::read(path).unwrap_or_default();
    let bytes = &bytes[..bytes.len().min(4096)];
    String::from_utf8_lossy(bytes).trim().to_string()
}

pub fn run_from_stdin() -> io::Result<()> {
    let start: HolderStart = read_limited_json(io::stdin().lock(), MAX_FRAME_BYTES)?;
    start
        .request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Holder::new(start)?.run()
}

pub fn run_from_file(session_id: &str, state_root: &std::path::Path) -> io::Result<()> {
    crate::paths::validate_identifier(session_id)?;
    let roots = StatePaths::from_root(state_root.to_path_buf())?;
    let paths = roots.session(session_id)?;
    crate::paths::reject_symlink(&paths.holder_start)?;
    let result = (|| {
        let file = File::open(&paths.holder_start)?;
        let decoded: io::Result<HolderStart> = read_limited_json(file, MAX_FRAME_BYTES);
        // The file contains the bearer token. Unlink it as soon as the bytes
        // have been consumed, including when decoding or identity validation
        // fails.
        fs::remove_file(&paths.holder_start)?;
        let start = decoded?;
        if start.request.session_id != session_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "supervised Holder start identity does not match",
            ));
        }
        Holder::new_with_roots(start, roots)?.run()
    })();
    if let Err(error) = &result
        && let Ok(mut diagnostics) = open_private_truncate(&paths.diagnostics)
    {
        let _ = writeln!(diagnostics, "supervised Holder failed: {error}");
    }
    result
}

/// A tiny per-session reaper waits on a pipe owned only by the Holder. Kernel
/// closure of that pipe is reliable even when the Holder is killed with
/// SIGKILL; the reaper then kills the Agent's independent process group. It
/// owns no socket, PTY, state, or orchestration and is not a supervisor.
pub fn run_process_guard(input: &mut dyn Read, process_pid: u32) -> io::Result<()> {
    if process_pid <= 1 || libc::pid_t::try_from(process_pid).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guard process pid is invalid",
        ));
    }
    let mut byte = [0_u8; 1];
    let read_result = loop {
        match input.read(&mut byte) {
            Ok(0) => break Ok(()),
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(error),
        }
    };
    terminate_process_group(process_pid);
    read_result
}

struct ProcessGuard {
    lifetime: Option<ChildStdin>,
    child: Option<Child>,
    watcher: Option<ExitWatcher>,
}

impl ProcessGuard {
    fn spawn(executable: &std::path::Path, process_pid: u32) -> io::Result<Self> {
        let mut command = Command::new(executable);
        command
            .arg("__process-guard")
            .arg(process_pid.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the post-fork closure invokes only the async-signal-safe
        // `setsid` syscall. The guard must not share the Holder's process group,
        // otherwise a Holder group kill could remove the only Agent reaper.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let lifetime = child.stdin.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process guard stdin is unavailable",
            )
        })?;
        let watcher = ExitWatcher::new(child.id())?;
        Ok(Self {
            lifetime: Some(lifetime),
            child: Some(child),
            watcher: Some(watcher),
        })
    }

    fn watcher_fd(&self) -> Option<i32> {
        self.watcher.as_ref().map(ExitWatcher::as_raw_fd)
    }

    fn finish(&mut self) -> io::Result<()> {
        drop(self.lifetime.take());
        self.watcher = None;
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Agent process guard exited with {status}"
            )))
        }
    }

    fn take_unexpected_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        self.child = None;
        self.watcher = None;
        self.lifetime = None;
        Ok(Some(status))
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        drop(self.lifetime.take());
        self.watcher = None;
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct Holder {
    _lock: File,
    paths: SessionPaths,
    listener: UnixListener,
    // Declared before the PTY so ordinary unwinding closes the guard's pipe
    // and reaps the Agent group before the PTY handle itself is dropped.
    process_guard: ProcessGuard,
    pty: Pty,
    pty_reader: Option<PtyStream>,
    pty_writer: PtyStream,
    exit_watcher: Option<ExitWatcher>,
    screen: HeadlessScreen,
    log: OutputLog,
    state: SessionState,
    connection: Option<Connection>,
    pending_connection: Option<Connection>,
    pending_input: PendingBytes,
    /// Submission ids are committed only after all earlier queued input and
    /// their Enter bytes have crossed into the PTY, preserving stream order.
    pending_delivery_receipts: Vec<String>,
    dirty_since: Option<Instant>,
    interactive_grid_budget: u8,
    last_persisted_offset: u64,
}

impl Holder {
    fn new(start: HolderStart) -> io::Result<Self> {
        Self::new_with_roots(start, StatePaths::resolve()?)
    }

    fn new_with_roots(start: HolderStart, roots: StatePaths) -> io::Result<Self> {
        let paths = roots.session(&start.request.session_id)?;
        paths.ensure()?;
        initialize_auth(&paths, &start.request.session_token)?;
        let lock = acquire_lock(&paths.lock)?;
        remove_stale_socket(&paths.socket)?;
        let listener = UnixListener::bind(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let spec = PtySpec {
            argv: resolve_remote_executable(&start.request.argv, &start.request.environment)?,
            env: start
                .request
                .environment
                .iter()
                .map(|variable| (variable.name.clone(), variable.value.clone()))
                .collect(),
            cwd: start.request.cwd.clone().into(),
            cols: start.request.cols,
            rows: start.request.rows,
        };
        let executable = std::env::current_exe()?;
        let mut pty = Pty::spawn(&spec)?;
        let process_pid = pty.pid();
        let process_guard = match ProcessGuard::spawn(&executable, process_pid) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = pty.terminate(Duration::ZERO);
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot start Agent process guard: {error}"),
                ));
            }
        };
        let pty_reader = pty.reader()?;
        pty_reader.set_nonblocking(true)?;
        let pty_writer = pty.writer()?;
        let exit_watcher = ExitWatcher::new(process_pid)?;
        let screen = HeadlessScreen::new(
            usize::from(start.request.cols),
            usize::from(start.request.rows),
        );
        let log = OutputLog::open(&paths.output)?;
        let previous_delivery_ledger = read_state(&paths.state).ok().and_then(|state| {
            (state.session_id == start.request.session_id)
                .then_some((state.accepted_delivery_ids, state.pending_delivery_ids))
        });
        let mut state = SessionState::new(&start.request, start.incarnation, process_pid);
        if let Some((accepted, pending)) = previous_delivery_ledger {
            state.accepted_delivery_ids = accepted;
            state.pending_delivery_ids = pending;
        }
        state.output_offset = log.tail_offset();
        write_state(&paths.state, &state)?;

        Ok(Self {
            _lock: lock,
            paths,
            listener,
            process_guard,
            pty,
            pty_reader: Some(pty_reader),
            pty_writer,
            exit_watcher: Some(exit_watcher),
            screen,
            log,
            state,
            connection: None,
            pending_connection: None,
            pending_input: PendingBytes::default(),
            pending_delivery_receipts: Vec::new(),
            dirty_since: None,
            interactive_grid_budget: 0,
            last_persisted_offset: 0,
        })
    }

    fn run(mut self) -> io::Result<()> {
        loop {
            let mut descriptors = Vec::with_capacity(6);
            descriptors.push(libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            let pty_index = self.pty_reader.as_ref().map(|reader| {
                let index = descriptors.len();
                descriptors.push(libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN
                        | if self.pending_input.is_empty() {
                            0
                        } else {
                            libc::POLLOUT
                        },
                    revents: 0,
                });
                index
            });
            let exit_index = self.exit_watcher.as_ref().map(|watcher| {
                let index = descriptors.len();
                descriptors.push(libc::pollfd {
                    fd: watcher.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                index
            });
            let guard_index = self.process_guard.watcher_fd().map(|fd| {
                let index = descriptors.len();
                descriptors.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                index
            });
            let connection_index = self.connection.as_ref().map(|connection| {
                let index = descriptors.len();
                descriptors.push(libc::pollfd {
                    fd: connection.stream.as_raw_fd(),
                    events: libc::POLLIN
                        | if connection.outbound.is_empty() {
                            0
                        } else {
                            libc::POLLOUT
                        },
                    revents: 0,
                });
                index
            });
            let pending_index = self.pending_connection.as_ref().map(|connection| {
                let index = descriptors.len();
                descriptors.push(libc::pollfd {
                    fd: connection.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                index
            });
            let timeout = self
                .dirty_since
                .map_or(-1, |since| poll_timeout(since + DIFF_COALESCE));
            // SAFETY: `descriptors` owns initialized pollfd entries for the
            // duration of the call. A negative timeout sleeps indefinitely.
            let ready = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    timeout,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }

            if descriptors[0].revents & libc::POLLIN != 0 {
                self.accept_connection()?;
            }
            if let Some(index) = pty_index {
                let events = descriptors[index].revents;
                if events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.drain_pty()?;
                }
                if events & libc::POLLOUT != 0 {
                    self.flush_input()?;
                }
            }
            if let Some(index) = exit_index
                && descriptors[index].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
            {
                self.record_exit()?;
            }
            if let Some(index) = guard_index
                && descriptors[index].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
                && let Some(status) = self.process_guard.take_unexpected_exit()?
            {
                let _ = self.pty.kill_group(libc::SIGKILL);
                let _ = self.pty.wait();
                return Err(io::Error::other(format!(
                    "Agent process guard exited unexpectedly with {status}"
                )));
            }
            if let Some(index) = connection_index {
                let events = descriptors[index].revents;
                if events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read_connection()?;
                }
                if events & libc::POLLOUT != 0 {
                    self.flush_connection()?;
                }
            }
            if let Some(index) = pending_index
                && descriptors[index].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
            {
                self.read_pending_connection()?;
            }
            if self.dirty_since.is_some_and(|since| {
                self.interactive_grid_budget > 0 || since.elapsed() >= DIFF_COALESCE
            }) {
                self.emit_grid_delta()?;
            }
        }
    }

    fn accept_connection(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    self.pending_connection = Some(Connection::new(stream));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn drain_pty(&mut self) -> io::Result<()> {
        let Some(_) = self.pty_reader.as_mut() else {
            return Ok(());
        };
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = self
                .pty_reader
                .as_mut()
                .expect("reader checked")
                .read(&mut buffer);
            match read {
                Ok(0) => {
                    self.pty_reader = None;
                    return Ok(());
                }
                Ok(count) => {
                    let bytes = &buffer[..count];
                    let offset = self.log.append(bytes)?;
                    self.state.output_offset = self.log.tail_offset();
                    self.screen.feed(bytes);
                    if self.dirty_since.is_none() {
                        self.dirty_since = Some(Instant::now());
                    }
                    if self
                        .connection
                        .as_ref()
                        .is_some_and(|connection| connection.epoch.is_some())
                    {
                        self.queue(RemoteMessage::Terminal(Frame::output(offset, bytes)))?;
                    }
                    if self
                        .state
                        .output_offset
                        .saturating_sub(self.last_persisted_offset)
                        >= PERSIST_OFFSET_INTERVAL
                    {
                        write_state(&self.paths.state, &self.state)?;
                        self.last_persisted_offset = self.state.output_offset;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn read_connection(&mut self) -> io::Result<()> {
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        let closed = self.read_messages(&mut connection)?;
        if !closed {
            self.connection = Some(connection);
        }
        Ok(())
    }

    fn read_pending_connection(&mut self) -> io::Result<()> {
        let Some(mut connection) = self.pending_connection.take() else {
            return Ok(());
        };
        let closed = self.read_messages(&mut connection)?;
        if closed {
            return Ok(());
        }
        if connection.epoch.is_some() {
            // Authentication, incarnation, protocol and capability checks all
            // completed before this atomic replacement. An unauthenticated
            // local connector can never revoke a live controller.
            if let Some(mut previous) = self.connection.take() {
                let previous_epoch = previous.epoch.unwrap_or(0);
                let _ = previous.queue(RemoteMessage::ControlRevoked(ControlRevoked {
                    controller_epoch: previous_epoch,
                    reason: "superseded by a newer authenticated attach".into(),
                }));
                // One nonblocking attempt gives a healthy client the explicit
                // revocation without allowing a stale/slow client to delay
                // the atomic controller handoff.
                let _ = previous.flush();
            }
            self.connection = Some(connection);
        } else {
            self.pending_connection = Some(connection);
        }
        Ok(())
    }

    fn read_messages(&mut self, connection: &mut Connection) -> io::Result<bool> {
        let mut buffer = [0_u8; 64 * 1024];
        let mut closed = false;
        loop {
            match connection.stream.read(&mut buffer) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(count) => {
                    let messages = match connection.codec.feed(&buffer[..count]) {
                        Ok(messages) => messages,
                        Err(error) => {
                            connection.send_fatal("invalid_frame", &error.to_string());
                            closed = true;
                            break;
                        }
                    };
                    for message in messages {
                        if let Err(error) = self.handle_message(connection, message) {
                            connection.send_fatal("protocol_error", &error.to_string());
                            closed = true;
                            break;
                        }
                    }
                    if closed {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        Ok(closed)
    }

    fn handle_message(
        &mut self,
        connection: &mut Connection,
        message: RemoteMessage,
    ) -> io::Result<()> {
        if connection.epoch.is_none() {
            let RemoteMessage::Hello(hello) = message else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Hello must be the first attach message",
                ));
            };
            return self.handshake(connection, hello);
        }
        let epoch = connection.epoch.expect("checked");
        if epoch != self.state.controller_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "controller epoch was revoked",
            ));
        }
        match message {
            RemoteMessage::Terminal(frame) => match frame.frame_type {
                FrameType::Input | FrameType::Mouse => {
                    self.write_input(&frame.payload)?;
                    self.commit_delivery_receipts(connection)
                }
                FrameType::Resize => {
                    let Some((cols, rows)) = frame.resize_payload() else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Resize payload must contain exactly two u16 values",
                        ));
                    };
                    if frame.payload.len() != 4 || validate_terminal_dimensions(cols, rows).is_err()
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Resize dimensions are invalid",
                        ));
                    }
                    self.pty.resize(cols, rows)?;
                    self.screen.resize(usize::from(cols), usize::from(rows));
                    self.state.cols = cols;
                    self.state.rows = rows;
                    self.state.snapshot_sequence = self.state.snapshot_sequence.saturating_add(1);
                    write_state(&self.paths.state, &self.state)?;
                    self.queue_snapshot(connection)
                }
                FrameType::Ping => connection.queue(RemoteMessage::Terminal(Frame::pong())),
                FrameType::Scroll => {
                    let Some((direction, lines, col, row)) = frame.scroll_payload() else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Scroll payload is invalid",
                        ));
                    };
                    let bytes = self.screen.mouse_wheel(
                        direction == 0,
                        usize::from(lines),
                        usize::from(col),
                        usize::from(row),
                    );
                    self.write_input(&bytes)?;
                    self.commit_delivery_receipts(connection)
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "terminal frame is not valid from a controller",
                )),
            },
            RemoteMessage::Signal(signal) => {
                if signal.controller_epoch != epoch || !allowed_signal(signal.signal) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "signal or controller epoch is invalid",
                    ));
                }
                self.pty.kill_group(signal.signal)
            }
            RemoteMessage::AcquireControl(_) => {
                connection.queue(RemoteMessage::ControlGranted(ControlGranted {
                    controller_epoch: epoch,
                }))
            }
            RemoteMessage::ScrollbackRequest(request) => {
                let result = self
                    .screen
                    .scrollback_cells(request.first_row, request.max_rows);
                connection.queue(RemoteMessage::ScrollbackResponse(ScrollbackResponse {
                    request_id: request.request_id,
                    result,
                }))
            }
            RemoteMessage::DeliveryInput(delivery) => {
                if self
                    .state
                    .accepted_delivery_ids
                    .iter()
                    .any(|accepted| accepted == &delivery.delivery_id)
                {
                    return connection.queue(RemoteMessage::DeliveryAccepted(DeliveryAccepted {
                        delivery_id: delivery.delivery_id,
                    }));
                }
                if self
                    .state
                    .pending_delivery_ids
                    .iter()
                    .any(|pending| pending == &delivery.delivery_id)
                    && !self
                        .pending_delivery_receipts
                        .iter()
                        .any(|pending| pending == &delivery.delivery_id)
                {
                    return connection.queue(RemoteMessage::DeliveryUncertain(DeliveryUncertain {
                        delivery_id: delivery.delivery_id,
                    }));
                }
                if self
                    .pending_delivery_receipts
                    .iter()
                    .any(|pending| pending == &delivery.delivery_id)
                {
                    // The original bytes are still ahead of this receipt in
                    // the PTY queue. A retry must wait for that same landing,
                    // never append a second copy.
                    return self.commit_delivery_receipts(connection);
                }
                if self.pending_input.len().saturating_add(delivery.data.len())
                    > MAX_PENDING_INPUT_BYTES
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "pending input queue is full",
                    ));
                }
                prepare_delivery_receipt(
                    &self.paths.state,
                    &mut self.state,
                    &delivery.delivery_id,
                )?;
                // Register the id before the nonblocking write. If the PTY
                // accepts only a prefix and then errors, the remaining bytes
                // and this id stay paired for a later flush/retry instead of
                // allowing the controller to append a duplicate operation.
                self.pending_delivery_receipts.push(delivery.delivery_id);
                self.write_input(&delivery.data)?;
                self.commit_delivery_receipts(connection)
            }
            RemoteMessage::ReleaseControl(release) => {
                if release.controller_epoch != epoch {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "release uses a stale controller epoch",
                    ));
                }
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "controller released",
                ))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message is not valid from a controller",
            )),
        }
    }

    fn handshake(&mut self, connection: &mut Connection, hello: Hello) -> io::Result<()> {
        if hello.protocol.major != diri_proto::remote_pty::PROTOCOL_MAJOR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol major does not match",
            ));
        }
        if hello.session_id != self.state.session_id
            || hello
                .expected_incarnation
                .as_ref()
                .is_some_and(|expected| expected != &self.state.session_incarnation)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session identity or incarnation does not match",
            ));
        }
        if !authenticate(&self.paths, &hello.session_token)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session authentication failed",
            ));
        }
        if let Some(missing) = hello
            .required_capabilities
            .iter()
            .find(|capability| !PHASE_ONE_CAPABILITIES.contains(capability))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("required capability {missing:?} is unavailable"),
            ));
        }
        self.state.controller_epoch = self.state.controller_epoch.saturating_add(1);
        let epoch = self.state.controller_epoch;
        connection.epoch = Some(epoch);
        connection.queue(RemoteMessage::HelloAck(HelloAck {
            protocol: diri_proto::remote_pty::ProtocolVersion::CURRENT,
            holder_build_id: BUILD_ID.to_string(),
            session_incarnation: self.state.session_incarnation.clone(),
            capabilities: PHASE_ONE_CAPABILITIES.to_vec(),
            controller_epoch: epoch,
            process_state: self.state.process_state.clone(),
            output_offset: self.state.output_offset,
            snapshot_sequence: self.state.snapshot_sequence,
            accepted_delivery_ids: self.state.accepted_delivery_ids.clone(),
            uncertain_delivery_ids: self.state.pending_delivery_ids.clone(),
        }))?;
        self.queue_replay(connection, hello.last_acknowledged_output_offset)?;
        self.state.snapshot_sequence = self.state.snapshot_sequence.saturating_add(1);
        self.queue_snapshot(connection)?;
        connection.queue(RemoteMessage::ControlGranted(ControlGranted {
            controller_epoch: epoch,
        }))?;
        write_state(&self.paths.state, &self.state)
    }

    fn queue_replay(
        &self,
        connection: &mut Connection,
        acknowledged: Option<u64>,
    ) -> io::Result<()> {
        let tail = self.log.tail_offset();
        let requested = acknowledged.unwrap_or(tail);
        if requested >= tail {
            return Ok(());
        }
        let start = requested.max(tail.saturating_sub(REPLAY_BUDGET_BYTES as u64));
        connection.queue(RemoteMessage::Terminal(Frame::replay_begin(start)))?;
        let mut offset = start;
        while offset < tail {
            let (actual, bytes) = self.log.read(offset, 64 * 1024)?;
            if bytes.is_empty() {
                break;
            }
            connection.queue(RemoteMessage::Terminal(Frame::output(actual, &bytes)))?;
            offset = actual + bytes.len() as u64;
        }
        connection.queue(RemoteMessage::Terminal(Frame::replay_end(tail)))
    }

    fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.pending_input.len().saturating_add(bytes.len()) > MAX_PENDING_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pending input queue is full",
            ));
        }
        self.pending_input.push(bytes);
        // One publication can be trailing output already in flight and the
        // next the actual echo/TUI response. Keep the fast path bounded to
        // those two frames so a keystroke cannot unthrottle a bulk stream.
        self.interactive_grid_budget = INTERACTIVE_GRID_BUDGET;
        self.flush_input()
    }

    fn flush_input(&mut self) -> io::Result<()> {
        while !self.pending_input.is_empty() {
            match self.pty_writer.write(self.pending_input.remaining()) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "PTY input write returned zero",
                    ));
                }
                Ok(count) => self.pending_input.consume(count),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if self.pending_delivery_receipts.is_empty() {
            return Ok(());
        }
        let Some(mut connection) = self.connection.take() else {
            // `read_connection` temporarily lends the live connection to the
            // message handler. Leave receipts pending so that handler can
            // persist them and queue acknowledgements on the same connection.
            return Ok(());
        };
        let result = self.commit_delivery_receipts(&mut connection);
        self.connection = Some(connection);
        result
    }

    fn commit_delivery_receipts(&mut self, connection: &mut Connection) -> io::Result<()> {
        if !self.pending_input.is_empty() || self.pending_delivery_receipts.is_empty() {
            return Ok(());
        }
        let receipts = self.persist_pending_delivery_receipts()?;
        for delivery_id in receipts {
            connection.queue(RemoteMessage::DeliveryAccepted(DeliveryAccepted {
                delivery_id,
            }))?;
        }
        Ok(())
    }

    fn persist_pending_delivery_receipts(&mut self) -> io::Result<Vec<String>> {
        if !self.pending_input.is_empty() || self.pending_delivery_receipts.is_empty() {
            return Ok(Vec::new());
        }
        persist_delivery_receipts(
            &self.paths.state,
            &mut self.state,
            &mut self.pending_delivery_receipts,
        )
    }

    fn flush_connection(&mut self) -> io::Result<()> {
        let Some(connection) = self.connection.as_mut() else {
            return Ok(());
        };
        match connection.flush() {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                self.connection = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn emit_grid_delta(&mut self) -> io::Result<()> {
        self.interactive_grid_budget = self.interactive_grid_budget.saturating_sub(1);
        self.dirty_since = None;
        if self
            .connection
            .as_ref()
            .is_none_or(|connection| connection.epoch.is_none())
        {
            return Ok(());
        }
        let grid = self.screen.grid_update(false);
        self.state.snapshot_sequence = self.state.snapshot_sequence.saturating_add(1);
        if grid.is_full_snapshot {
            self.queue(RemoteMessage::FullSnapshot(FullSnapshot {
                sequence: self.state.snapshot_sequence,
                alt_screen: self.screen.is_alt_screen(),
                bracketed_paste: self.screen.bracketed_paste(),
                mouse: self.screen.mouse_modes(),
                grid,
            }))?;
        } else {
            self.queue(RemoteMessage::GridDelta(GridDelta {
                sequence: self.state.snapshot_sequence,
                alt_screen: self.screen.is_alt_screen(),
                bracketed_paste: self.screen.bracketed_paste(),
                mouse: self.screen.mouse_modes(),
                grid,
            }))?;
        }
        Ok(())
    }

    fn queue(&mut self, message: RemoteMessage) -> io::Result<()> {
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        connection.queue(message)?;
        if connection.outbound.len() > MAX_OUTBOUND_BYTES {
            if connection.sent != 0 {
                // Bytes from the head frame already reached the client; a
                // same-stream reseed would begin in the middle of that frame.
                // Drop only this Bridge. Engine reconnects by sequence and
                // receives an authoritative FullSnapshot.
                return Ok(());
            }
            connection.outbound.clear();
            connection.sent = 0;
            connection.queue_error(
                "slow_client_reseed",
                "incremental output was discarded; applying a fresh terminal snapshot",
                false,
            )?;
            self.state.snapshot_sequence = self.state.snapshot_sequence.saturating_add(1);
            self.queue_snapshot(&mut connection)?;
        }
        self.connection = Some(connection);
        Ok(())
    }

    fn queue_snapshot(&self, connection: &mut Connection) -> io::Result<()> {
        connection.queue(RemoteMessage::FullSnapshot(FullSnapshot {
            sequence: self.state.snapshot_sequence,
            alt_screen: self.screen.is_alt_screen(),
            bracketed_paste: self.screen.bracketed_paste(),
            mouse: self.screen.mouse_modes(),
            grid: self.screen.full_snapshot(),
        }))
    }

    fn record_exit(&mut self) -> io::Result<()> {
        // Close and join the independent guard before reaping the session
        // leader. This lets it kill any surviving grandchildren while the
        // process-group id is still protected from PID reuse by the zombie.
        self.process_guard.finish()?;
        let exit = self.pty.wait()?;
        self.exit_watcher = None;
        self.pty_reader = None;
        let (state, message) = match exit {
            Exit::Code(code) => (
                RemoteProcessState::Exited {
                    code: Some(code),
                    signal: None,
                },
                ProcessExit {
                    code: Some(code),
                    signal: None,
                },
            ),
            Exit::Signal(signal) => (
                RemoteProcessState::Exited {
                    code: None,
                    signal: Some(signal),
                },
                ProcessExit {
                    code: None,
                    signal: Some(signal),
                },
            ),
        };
        self.state.process_state = state;
        self.state.output_offset = self.log.tail_offset();
        self.log.flush()?;
        write_state(&self.paths.state, &self.state)?;
        self.queue(RemoteMessage::ProcessExit(message))
    }
}

fn persist_delivery_receipts(
    path: &std::path::Path,
    state: &mut SessionState,
    pending: &mut Vec<String>,
) -> io::Result<Vec<String>> {
    let receipts = pending.clone();
    let mut next_state = state.clone();
    for delivery_id in &receipts {
        if !next_state
            .accepted_delivery_ids
            .iter()
            .any(|accepted| accepted == delivery_id)
        {
            next_state.accepted_delivery_ids.push(delivery_id.clone());
        }
        next_state
            .pending_delivery_ids
            .retain(|pending| pending != delivery_id);
    }
    while next_state.accepted_delivery_ids.len() > DELIVERY_RECEIPT_LIMIT {
        next_state.accepted_delivery_ids.remove(0);
    }
    // State reaches durable storage before any acknowledgement can leave
    // this Holder. Preserve the pending ids and old in-memory state if
    // persistence fails so a later flush retries the exact transaction.
    write_state(path, &next_state)?;
    *state = next_state;
    pending.clear();
    Ok(receipts)
}

fn prepare_delivery_receipt(
    path: &std::path::Path,
    state: &mut SessionState,
    delivery_id: &str,
) -> io::Result<()> {
    if state
        .pending_delivery_ids
        .iter()
        .any(|pending| pending == delivery_id)
    {
        return Ok(());
    }
    if state.pending_delivery_ids.len() >= DELIVERY_RECEIPT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "unresolved delivery journal is full",
        ));
    }
    let mut next_state = state.clone();
    while next_state
        .accepted_delivery_ids
        .len()
        .saturating_add(next_state.pending_delivery_ids.len())
        >= DELIVERY_RECEIPT_LIMIT
    {
        if next_state.accepted_delivery_ids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "delivery outcome journal is full",
            ));
        }
        next_state.accepted_delivery_ids.remove(0);
    }
    next_state.pending_delivery_ids.push(delivery_id.to_owned());
    // This is the write-ahead half of the Holder receipt protocol. It lands
    // before any byte can reach the PTY; a crash after this point is exposed
    // as outcome-uncertain instead of silently replayed.
    write_state(path, &next_state)?;
    *state = next_state;
    Ok(())
}

fn resolve_remote_executable(
    argv: &[String],
    environment: &[diri_proto::remote_pty::EnvironmentVariable],
) -> io::Result<Vec<String>> {
    let mut resolved = argv.to_vec();
    let executable = resolved
        .first_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "argv is empty"))?;
    if executable.contains('/') {
        return Ok(resolved);
    }
    let path = environment
        .iter()
        .rev()
        .find(|variable| variable.name == "PATH")
        .map(|variable| variable.value.as_str())
        .unwrap_or("/usr/local/bin:/usr/bin:/bin");
    for directory in path.split(':').filter(|directory| !directory.is_empty()) {
        let candidate = std::path::Path::new(directory).join(&*executable);
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            *executable = candidate.to_string_lossy().into_owned();
            return Ok(resolved);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Agent executable was not found on the captured remote PATH",
    ))
}

struct Connection {
    stream: UnixStream,
    codec: RemoteCodec,
    epoch: Option<u64>,
    outbound: Vec<u8>,
    sent: usize,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            codec: RemoteCodec::new(),
            epoch: None,
            outbound: Vec::with_capacity(64 * 1024),
            sent: 0,
        }
    }

    fn queue(&mut self, message: RemoteMessage) -> io::Result<()> {
        RemoteCodec::encode_into(&message, &mut self.outbound).map_err(io::Error::other)
    }

    fn queue_error(&mut self, code: &str, message: &str, fatal: bool) -> io::Result<()> {
        self.queue(RemoteMessage::Error(RemoteError {
            code: code.to_string(),
            message: message.to_string(),
            fatal,
        }))
    }

    fn send_fatal(&mut self, code: &str, message: &str) {
        // Handshake failures must reach the Bridge as structured errors. A
        // bounded blocking write is safe here because this connection is
        // immediately discarded and the PTY event loop cannot wait forever.
        self.outbound.clear();
        self.sent = 0;
        if self.queue_error(code, message, true).is_err() {
            return;
        }
        let _ = self.stream.set_nonblocking(false);
        let _ = self
            .stream
            .set_write_timeout(Some(Duration::from_millis(100)));
        let _ = self.stream.write_all(&self.outbound);
        let _ = self.stream.flush();
    }

    fn flush(&mut self) -> io::Result<()> {
        while self.sent < self.outbound.len() {
            match self.stream.write(&self.outbound[self.sent..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "attach write returned zero",
                    ));
                }
                Ok(count) => self.sent += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.outbound.clear();
        self.sent = 0;
        Ok(())
    }
}

#[derive(Default)]
struct PendingBytes {
    bytes: Vec<u8>,
    consumed: usize,
}

impl PendingBytes {
    fn push(&mut self, bytes: &[u8]) {
        if self.consumed == self.bytes.len() {
            self.bytes.clear();
            self.consumed = 0;
        }
        self.bytes.extend_from_slice(bytes);
    }

    fn consume(&mut self, count: usize) {
        self.consumed += count;
        if self.consumed == self.bytes.len() {
            self.bytes.clear();
            self.consumed = 0;
        }
    }

    fn remaining(&self) -> &[u8] {
        &self.bytes[self.consumed..]
    }

    fn len(&self) -> usize {
        self.bytes.len().saturating_sub(self.consumed)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn read_limited_json<R: Read, T: serde::de::DeserializeOwned>(
    reader: R,
    maximum: usize,
) -> io::Result<T> {
    let mut bytes = Vec::new();
    reader.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON request exceeds its size limit",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn allowed_signal(signal: i32) -> bool {
    matches!(
        signal,
        libc::SIGINT
            | libc::SIGTERM
            | libc::SIGKILL
            | libc::SIGSTOP
            | libc::SIGCONT
            | libc::SIGHUP
            | libc::SIGQUIT
    )
}

fn poll_timeout(deadline: Instant) -> libc::c_int {
    let now = Instant::now();
    if deadline <= now {
        return 0;
    }
    deadline
        .duration_since(now)
        .as_millis()
        .min(libc::c_int::MAX as u128) as libc::c_int
}

fn terminate_process_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: the hidden Holder called `setsid`, so its pid is also its
        // process-group id. Errors are deliberately ignored on cleanup.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::remote_pty::PersistenceCapability;

    use super::*;

    fn receipt_state() -> SessionState {
        SessionState {
            schema: 1,
            session_id: "session".into(),
            session_incarnation: "incarnation".into(),
            holder_build_id: "holder".into(),
            holder_pid: 1,
            process_state: RemoteProcessState::Running { pid: 2 },
            cols: 80,
            rows: 24,
            output_offset: 0,
            snapshot_sequence: 0,
            controller_epoch: 1,
            persistence: PersistenceCapability::NonPersistent,
            created_at_unix_ms: 1,
            accepted_delivery_ids: vec!["older".into()],
            pending_delivery_ids: Vec::new(),
        }
    }

    #[test]
    fn pending_bytes_compact_after_a_complete_write() {
        let mut pending = PendingBytes::default();
        pending.push(b"one");
        pending.consume(2);
        assert_eq!(pending.remaining(), b"e");
        pending.consume(1);
        assert!(pending.is_empty());
        pending.push(b"two");
        assert_eq!(pending.remaining(), b"two");
    }

    #[test]
    fn failed_receipt_persistence_keeps_fifo_ids_and_state_for_retry() {
        let root = tempfile::tempdir().expect("temp");
        let blocked_parent = root.path().join("not-a-directory");
        fs::write(&blocked_parent, b"file").expect("blocking file");
        let mut state = receipt_state();
        let before = state.clone();
        let mut pending = vec!["first".into(), "second".into()];

        assert!(
            persist_delivery_receipts(&blocked_parent.join("state.json"), &mut state, &mut pending)
                .is_err()
        );
        assert_eq!(state, before);
        assert_eq!(pending, ["first", "second"]);

        let landed =
            persist_delivery_receipts(&root.path().join("state.json"), &mut state, &mut pending)
                .expect("retry");
        assert_eq!(landed, ["first", "second"]);
        assert!(pending.is_empty());
        assert_eq!(state.accepted_delivery_ids, ["older", "first", "second"]);
    }

    #[test]
    fn receipt_intent_is_durable_before_delivery_and_survives_a_failed_commit() {
        let root = tempfile::tempdir().expect("temp");
        let state_path = root.path().join("state.json");
        let mut state = receipt_state();
        prepare_delivery_receipt(&state_path, &mut state, "delivery-1").expect("prepare");
        assert_eq!(
            read_state(&state_path)
                .expect("durable intent")
                .pending_delivery_ids,
            ["delivery-1"]
        );

        let blocked_parent = root.path().join("not-a-directory");
        fs::write(&blocked_parent, b"file").expect("blocking file");
        let mut pending = vec!["delivery-1".into()];
        assert!(
            persist_delivery_receipts(&blocked_parent.join("state.json"), &mut state, &mut pending)
                .is_err()
        );
        assert_eq!(state.pending_delivery_ids, ["delivery-1"]);
        assert_eq!(pending, ["delivery-1"]);

        persist_delivery_receipts(&state_path, &mut state, &mut pending).expect("commit retry");
        let durable = read_state(&state_path).expect("durable commit");
        assert!(durable.pending_delivery_ids.is_empty());
        assert_eq!(durable.accepted_delivery_ids, ["older", "delivery-1"]);
    }

    #[test]
    fn signal_allowlist_excludes_arbitrary_and_uncatchable_platform_values() {
        assert!(allowed_signal(libc::SIGINT));
        assert!(allowed_signal(libc::SIGKILL));
        assert!(!allowed_signal(0));
        assert!(!allowed_signal(999));
    }

    #[test]
    fn idempotent_launch_requires_the_live_holder_build() {
        assert!(validate_live_build_id(BUILD_ID).is_ok());
        let error = validate_live_build_id("different-build").expect_err("build mismatch");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("different-build"));
    }
}
