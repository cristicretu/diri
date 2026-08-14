//! Launch the authoritative Rust Engine (`dirijord-rs`) bundled in `diri.app`.
//!
//! diri talks to the Engine over its owner-only Unix socket. The remote PTY
//! transport exists only in this Rust Engine, so daemon resolution must never
//! silently fall back to a legacy executable.
//!
//! A bundled Engine is content-identified on launch. When an app update
//! replaces it, the old Engine persists state and exits while Holder-owned
//! local and remote Agents keep running; the new Engine then adopts them. This
//! lets the first remote action use the new packaged Helper catalog. The daemon
//! holds an `flock` singleton, so a redundant spawn still exits instantly.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use diri_client::DaemonClient;
use diri_proto::paths::{DirijorPaths, ENV_SOCKET};
use diri_proto::{
    ControlMessage, DaemonShutdownIfIdleResult, HelloParams, HelloResult, Method, RUST_ENGINE_KIND,
};
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

/// Explicit development/test override pointing at an Engine executable.
const ENV_DAEMON_PATH: &str = "DIRIJORD_PATH";

const BOOT_LOG_FILE_NAME: &str = "dirijord-rs.boot.log";
const STARTUP_RELEASE_PUBLICATION_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_RELEASE_CLIENT_GRACE: Duration = Duration::from_millis(400);
const STARTUP_RELEASE_RETRY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupOutcome {
    EngineExpected,
    NoEngineExpected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupPhase {
    Planned,
    Running,
    Cancelling,
    Complete(StartupOutcome),
    Cleaning,
    Cleaned,
}

/// One-shot Engine supervision deferred until the desktop has scheduled its
/// first window.
///
/// This coordinator owns the complete task, rather than merely spawning it.
/// App shutdown cancels future mutation checkpoints and transfers idle release
/// to runtime-owned blocking work that survives GPUI's quit-future budget. The
/// Engine's `flock` remains the final singleton authority across independent
/// app processes.
pub(super) struct DeferredDaemonStartup {
    socket_path: PathBuf,
    cancelled: Arc<AtomicBool>,
    phase: Arc<Mutex<StartupPhase>>,
    task: Mutex<Option<JoinHandle<StartupOutcome>>>,
    cleanup_tasks: Mutex<Vec<JoinHandle<()>>>,
    release: Arc<dyn Fn() + Send + Sync>,
}

impl DeferredDaemonStartup {
    /// Plans app-owned Engine supervision from the current process layout.
    ///
    /// A harness that supplied `DIRIJOR_SOCKET` owns its daemon lifecycle, so
    /// there is no deferred work and the client may connect immediately.
    pub(super) fn for_process() -> Option<Self> {
        if std::env::var_os(ENV_SOCKET).is_some() {
            return None;
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let socket_path = DirijorPaths::socket(home);
        let release_socket = socket_path.clone();
        Some(Self {
            socket_path,
            cancelled: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(Mutex::new(StartupPhase::Planned)),
            task: Mutex::new(None),
            cleanup_tasks: Mutex::new(Vec::new()),
            release: Arc::new(move || release_daemon_if_idle(&release_socket)),
        })
    }

    /// Starts supervision away from GPUI's main thread, then releases the
    /// reconnecting client only after the live Engine is authoritative.
    ///
    /// Call this after `open_main_window`: identity probes can consume two
    /// one-second timeouts, upgrade shutdown can wait three seconds, and
    /// hashing a universal bundled executable performs synchronous file I/O.
    pub(super) fn after_window_open(&self, runtime: &Runtime, client: Arc<DaemonClient>) {
        let socket_path = self.socket_path.clone();
        self.start_after_window_open_with(runtime, client, move |cancelled| {
            ensure_daemon_running(&socket_path, &cancelled)
        });
    }

    fn start_after_window_open_with(
        &self,
        runtime: &Runtime,
        client: Arc<DaemonClient>,
        supervise: impl FnOnce(Arc<AtomicBool>) -> StartupOutcome + Send + 'static,
    ) {
        let mut task = self.task.lock().expect("daemon startup task lock poisoned");
        let mut phase = self
            .phase
            .lock()
            .expect("daemon startup phase lock poisoned");
        if task.is_some() || *phase != StartupPhase::Planned {
            return;
        }
        *phase = StartupPhase::Running;
        drop(phase);

        let cancelled = Arc::clone(&self.cancelled);
        let phase = Arc::clone(&self.phase);
        let release = Arc::clone(&self.release);
        let handle = runtime.handle().clone();
        *task = Some(runtime.spawn_blocking(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                supervise(Arc::clone(&cancelled))
            }))
            .unwrap_or_else(|_| {
                // The supervisor may have panicked after a successful spawn.
                // Conservatively assume cleanup is required.
                eprintln!("diri: Engine startup supervision panicked");
                StartupOutcome::EngineExpected
            });

            // Connect only on the ordinary completion path. `connect` uses
            // Tokio's ambient handle, so explicitly enter the app runtime from
            // this blocking worker.
            if !cancelled.load(Ordering::Acquire) {
                let _runtime = handle.enter();
                client.connect();
            }

            let should_clean = {
                let mut phase = phase.lock().expect("daemon startup phase lock poisoned");
                match *phase {
                    StartupPhase::Running => {
                        *phase = StartupPhase::Complete(outcome);
                        false
                    }
                    StartupPhase::Cancelling => {
                        *phase = StartupPhase::Cleaning;
                        true
                    }
                    _ => false,
                }
            };
            if should_clean {
                if outcome == StartupOutcome::EngineExpected {
                    release();
                }
                *phase.lock().expect("daemon startup phase lock poisoned") = StartupPhase::Cleaned;
            }
            outcome
        }));
    }

    /// Cancels startup within GPUI's synchronous quit callback and transfers
    /// cleanup to a runtime-owned blocking worker.
    ///
    /// GPUI gives quit futures only a short grace period, so this method never
    /// waits and never takes the sole startup handle. Tokio runtime teardown
    /// waits for blocking tasks even after ordinary async tasks are aborted.
    pub(super) fn request_shutdown(&self, runtime: &Runtime, client: &DaemonClient) {
        self.cancelled.store(true, Ordering::Release);
        // GPUI may drop its quit future after 200 ms. Publish this signal in
        // the synchronous callback so the app's persistent control connection
        // can close while raw idle-release requests are being retried.
        client.begin_shutdown();
        let schedule_release = {
            let mut phase = self
                .phase
                .lock()
                .expect("daemon startup phase lock poisoned");
            match *phase {
                StartupPhase::Planned => {
                    *phase = StartupPhase::Cleaned;
                    false
                }
                StartupPhase::Running => {
                    // The startup worker owns cleanup after its last possible
                    // mutation. This preserves ordering even if it has already
                    // crossed the final spawn checkpoint.
                    *phase = StartupPhase::Cancelling;
                    false
                }
                StartupPhase::Complete(StartupOutcome::EngineExpected) => {
                    *phase = StartupPhase::Cleaning;
                    true
                }
                StartupPhase::Complete(StartupOutcome::NoEngineExpected) => {
                    *phase = StartupPhase::Cleaned;
                    false
                }
                StartupPhase::Cancelling | StartupPhase::Cleaning | StartupPhase::Cleaned => false,
            }
        };
        if schedule_release {
            let release = Arc::clone(&self.release);
            let phase = Arc::clone(&self.phase);
            let cleanup = runtime.spawn_blocking(move || {
                release();
                *phase.lock().expect("daemon startup phase lock poisoned") = StartupPhase::Cleaned;
            });
            self.cleanup_tasks
                .lock()
                .expect("daemon cleanup task lock poisoned")
                .push(cleanup);
        }
    }
}

impl Drop for DeferredDaemonStartup {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let release_now = {
            let mut phase = self
                .phase
                .lock()
                .expect("daemon startup phase lock poisoned");
            match *phase {
                StartupPhase::Planned => {
                    *phase = StartupPhase::Cleaned;
                    false
                }
                StartupPhase::Running => {
                    // Tokio waits for this blocking task while tearing down;
                    // it will perform the release after supervision settles.
                    *phase = StartupPhase::Cancelling;
                    false
                }
                StartupPhase::Complete(StartupOutcome::EngineExpected) => {
                    *phase = StartupPhase::Cleaning;
                    true
                }
                StartupPhase::Complete(StartupOutcome::NoEngineExpected) => {
                    *phase = StartupPhase::Cleaned;
                    false
                }
                StartupPhase::Cancelling | StartupPhase::Cleaning | StartupPhase::Cleaned => false,
            }
        };
        // Early unwinding has no live GPUI callback in which to schedule work.
        // Blocking here is preferable to leaking an Engine the app just made.
        if release_now {
            (self.release)();
            *self
                .phase
                .lock()
                .expect("daemon startup phase lock poisoned") = StartupPhase::Cleaned;
        }
    }
}

/// Ensure a daemon is reachable at `socket_path`, spawning the bundled
/// `dirijord-rs` detached if the socket is dead.
///
/// This function performs blocking probes, hashing, and bounded upgrade waits;
/// it must run through [`DeferredDaemonStartup`]. After a spawn it returns
/// immediately and lets [`DaemonClient`]'s reconnect loop discover the socket.
fn ensure_daemon_running(socket_path: &Path, cancelled: &AtomicBool) -> StartupOutcome {
    let boot_log = boot_log_path();
    ensure_daemon_running_with_cancellation(
        socket_path,
        resolve_daemon_path(),
        boot_log.as_deref(),
        cancelled,
    )
}

#[cfg(test)]
fn ensure_daemon_running_with(
    socket_path: &Path,
    daemon: Option<PathBuf>,
    boot_log: Option<&Path>,
) {
    let cancelled = AtomicBool::new(false);
    let _ = ensure_daemon_running_with_cancellation(socket_path, daemon, boot_log, &cancelled);
}

fn ensure_daemon_running_with_cancellation(
    socket_path: &Path,
    daemon: Option<PathBuf>,
    boot_log: Option<&Path>,
    cancelled: &AtomicBool,
) -> StartupOutcome {
    match probe_daemon(socket_path) {
        Ok(hello) if hello.engine_kind.as_deref() == Some(RUST_ENGINE_KIND) => {
            let Some(daemon) = daemon.as_ref() else {
                // An externally managed Rust Engine has no local artifact to
                // compare. Keep it running rather than guessing ownership.
                return StartupOutcome::EngineExpected;
            };
            let expected_hash = match executable_sha256(daemon) {
                Ok(hash) => hash,
                Err(error) => {
                    eprintln!(
                        "diri: cannot verify bundled daemon {}: {error}; keeping the live Engine",
                        daemon.display()
                    );
                    return StartupOutcome::EngineExpected;
                }
            };
            if !daemon_needs_refresh(&hello, &expected_hash) {
                return StartupOutcome::EngineExpected;
            }
            eprintln!(
                "diri: refreshing Rust Engine {:?} from bundled executable {}",
                hello.build,
                daemon.display()
            );
            if cancelled.load(Ordering::Acquire) {
                return StartupOutcome::EngineExpected;
            }
            if let Err(error) = stop_daemon_for_upgrade(socket_path, Some(hello.pid)) {
                eprintln!(
                    "diri: could not stop the outdated Rust Engine at {}: {error}",
                    socket_path.display()
                );
                return StartupOutcome::EngineExpected;
            }
        }
        Ok(hello) => {
            // An older release left its own daemon owning this socket, and it
            // deliberately outlives the app that started it. Refusing here
            // would strand every upgrading user: the socket stays held, no
            // Engine is ever spawned, and the app comes up empty with the
            // explanation on a stderr a bundled app never shows. Retire it the
            // same way an outdated Rust Engine is retired — `daemon.shutdown`
            // persists state first, and holder-owned sessions outlive it and
            // are re-adopted.
            eprintln!(
                "diri: replacing non-Rust daemon build {:?} at {} with the bundled Rust Engine",
                hello.build,
                socket_path.display()
            );
            if daemon.is_none() {
                eprintln!(
                    "diri: no bundled Engine to replace it with; leaving {} alone",
                    socket_path.display()
                );
                return StartupOutcome::NoEngineExpected;
            }
            if cancelled.load(Ordering::Acquire) {
                return StartupOutcome::NoEngineExpected;
            }
            if let Err(error) = stop_daemon_for_upgrade(socket_path, Some(hello.pid)) {
                eprintln!(
                    "diri: could not stop the previous daemon at {}: {error}",
                    socket_path.display()
                );
                return StartupOutcome::NoEngineExpected;
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
            ) => {}
        // Something is listening but cannot identify itself. The usual cause is
        // not corruption, it is age: every Engine older than `daemon.hello`
        // answers `not_found`, which arrives here as a plain error rather than
        // a refused connection. Returning would strand exactly the people who
        // are upgrading — the shipped 0.4.7 Engine behaves this way, verified
        // against a live one — so an Engine that cannot answer the probe is
        // retired like one that answers with the wrong build.
        //
        // Probed twice before deciding: a healthy but momentarily busy Engine
        // can miss the one-second read timeout, and restarting it over a
        // hiccup is needless churn. `daemon.shutdown` persists state first and
        // holder-owned sessions outlive it either way.
        Err(error) if probe_daemon(socket_path).is_err() => {
            eprintln!(
                "diri: replacing the Engine at {} — it could not answer the identity probe ({error})",
                socket_path.display()
            );
            if daemon.is_none() {
                eprintln!("diri: no bundled Engine to replace it with; leaving it alone");
                return StartupOutcome::NoEngineExpected;
            }
            if cancelled.load(Ordering::Acquire) {
                return StartupOutcome::NoEngineExpected;
            }
            if let Err(error) = stop_daemon_for_upgrade(socket_path, None) {
                eprintln!(
                    "diri: could not stop the unidentified Engine at {}: {error}",
                    socket_path.display()
                );
                return StartupOutcome::NoEngineExpected;
            }
        }
        Err(error) => {
            eprintln!(
                "diri: the Engine at {} answered a retried identity probe; leaving it alone ({error})",
                socket_path.display()
            );
            return StartupOutcome::EngineExpected;
        }
    }

    if cancelled.load(Ordering::Acquire) {
        return StartupOutcome::NoEngineExpected;
    }
    match daemon {
        Some(daemon) => match spawn_detached(&daemon, boot_log) {
            Ok(()) => {
                eprintln!("diri: launched bundled daemon at {}", daemon.display());
                StartupOutcome::EngineExpected
            }
            Err(err) => {
                eprintln!(
                    "diri: failed to launch bundled daemon {}: {err}",
                    daemon.display()
                );
                StartupOutcome::NoEngineExpected
            }
        },
        None => {
            eprintln!(
                "diri: no bundled dirijord-rs found next to the executable; \
                 relying on an externally managed daemon"
            );
            StartupOutcome::NoEngineExpected
        }
    }
}

/// Best-effort synchronous release used by the startup coordinator during app
/// shutdown. It is intentionally independent of `DaemonClient`: a GPUI quit
/// future may be dropped before that client's async shutdown runs. The Engine
/// itself remains the authority and refuses while live sessions need it.
fn release_daemon_if_idle(socket_path: &Path) {
    let publication_deadline = std::time::Instant::now() + STARTUP_RELEASE_PUBLICATION_TIMEOUT;
    let mut client_grace_deadline = None;
    loop {
        match control_request(socket_path, 3, Method::DAEMON_SHUTDOWN_IF_IDLE, None) {
            Ok(value) => match serde_json::from_value::<DaemonShutdownIfIdleResult>(value) {
                Ok(result) if result.will_exit => return,
                Ok(result)
                    if result.reason.as_deref()
                        == Some("another control client still requires the Engine")
                        && std::time::Instant::now()
                            < *client_grace_deadline.get_or_insert_with(|| {
                                std::time::Instant::now() + STARTUP_RELEASE_CLIENT_GRACE
                            }) =>
                {
                    // `request_shutdown` has already signalled the app client.
                    // Give its async socket task one short grace period to
                    // observe that signal, but never pin runtime teardown on a
                    // genuinely independent client.
                }
                Ok(_) => return,
                Err(error) => {
                    eprintln!("diri: invalid idle-release response during shutdown: {error}");
                    return;
                }
            },
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && std::time::Instant::now() < publication_deadline =>
            {
                // A detached Engine may not have published its socket yet.
            }
            Err(error) => {
                eprintln!(
                    "diri: could not release the Engine after cancelled startup at {}: {error}",
                    socket_path.display()
                );
                return;
            }
        }
        std::thread::sleep(STARTUP_RELEASE_RETRY);
    }
}

fn daemon_needs_refresh(hello: &HelloResult, expected_hash: &str) -> bool {
    hello.executable_hash.as_deref() != Some(expected_hash)
}

fn stop_daemon_for_upgrade(socket_path: &Path, pid: Option<i32>) -> io::Result<()> {
    control_request(socket_path, 2, Method::DAEMON_SHUTDOWN, None)?;
    for _ in 0..30 {
        if !socket_is_live(socket_path) {
            if let Some(pid) = pid {
                if !process_is_alive(pid) {
                    return Ok(());
                }
            } else {
                // Old Engines that predate Hello do not reveal a pid. Give
                // their singleton lock one scheduling turn after the socket is
                // removed, then verify that the endpoint stayed down.
                std::thread::sleep(Duration::from_millis(100));
                if !socket_is_live(socket_path) {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the outdated Engine did not release its socket within 3 seconds",
    ))
}

fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 does not deliver a signal; it only probes whether this
    // process id still exists and is visible to the current user.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn executable_sha256(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// True when something is listening on the daemon socket right now.
fn socket_is_live(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

fn probe_daemon(socket_path: &Path) -> io::Result<HelloResult> {
    let params =
        serde_json::to_value(HelloParams::new("diri-launch-probe")).map_err(io::Error::other)?;
    let value = control_request(socket_path, 1, Method::HELLO, Some(params))?;
    serde_json::from_value(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon Hello response is invalid: {error}"),
        )
    })
}

fn control_request(
    socket_path: &Path,
    id: u64,
    method: &str,
    params: Option<serde_json::Value>,
) -> io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let request = ControlMessage::Request {
        id,
        method: method.to_string(),
        params,
    };
    serde_json::to_writer(&mut stream, &request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = Vec::new();
    reader
        .by_ref()
        .take(diri_proto::control::MAX_CONTROL_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut response)?;
    if response.len() > diri_proto::control::MAX_CONTROL_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon response exceeds the control line limit",
        ));
    }
    let message: ControlMessage = serde_json::from_slice(&response).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon response is invalid: {error}"),
        )
    })?;
    match message {
        ControlMessage::Response {
            id: response_id,
            result,
        } if response_id == id => result.map_err(io::Error::other),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon returned the wrong control response",
        )),
    }
}

/// Resolve the Rust Engine executable to launch, using the live process layout.
pub fn resolve_daemon_path() -> Option<PathBuf> {
    resolve_daemon_path_from(
        std::env::var_os(ENV_DAEMON_PATH).map(PathBuf::from),
        std::env::current_exe().ok(),
        std::env::current_dir().ok(),
    )
}

/// Pure resolver, split out so the bundle layout can be unit-tested without a
/// real `diri.app`.
///
/// Search order (first executable wins):
///   1. `DIRIJORD_PATH` override (dev/tests).
///   2. Bundled: `Contents/MacOS/diri` → `../Resources/bin/dirijord-rs`.
///   3. Next to the executable (loose dev copy).
///   4. Cargo build outputs under the working dir: `target/{release,debug}/dirijord-rs`.
fn resolve_daemon_path_from(
    env_override: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    current_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = env_override
        && is_executable(&path)
    {
        return Some(path);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(exe) = current_exe
        && let Some(macos_dir) = exe.parent()
    {
        // Contents/MacOS/diri → Contents/Resources/bin/dirijord-rs
        if let Some(contents) = macos_dir.parent() {
            candidates.push(contents.join("Resources/bin/dirijord-rs"));
        }
        // Loose copy sitting right next to the executable.
        candidates.push(macos_dir.join("dirijord-rs"));
    }

    if let Some(cwd) = current_dir {
        candidates.push(cwd.join("target/release/dirijord-rs"));
        candidates.push(cwd.join("target/debug/dirijord-rs"));
    }

    candidates.into_iter().find(|path| is_executable(path))
}

/// Spawn the Engine in its own process group so it outlives diri, with
/// stdout/stderr appended to `dirijord-rs.boot.log`. We never wait on the
/// child: the daemon is meant to run independently.
fn spawn_detached(daemon: &Path, boot_log: Option<&Path>) -> io::Result<()> {
    let mut command = Command::new(daemon);
    command.stdin(Stdio::null());

    match boot_log {
        Some(log_path) => {
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)?;
            let err = out.try_clone()?;
            command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    // New process group (setpgid to the child's own pid): decouples the daemon
    // from diri's signal/terminal group so quitting diri never SIGHUPs the
    // daemon or its PTYs. Equivalent intent to the Swift POSIX_SPAWN_SETSID path.
    command.process_group(0);

    // Spawn and deliberately drop the handle — we do not (and must not) wait.
    command.spawn().map(|_child| ())
}

/// The platform log directory's early-boot log, created before the Engine can
/// initialize its own diagnostics. Returns `None` when `HOME` is unset.
fn boot_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let logs = DirijorPaths::logs_dir(PathBuf::from(home));
    std::fs::create_dir_all(&logs).ok()?;
    Some(logs.join(BOOT_LOG_FILE_NAME))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex, mpsc};

    fn touch_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn serve_control(
        socket: &Path,
        responses: Vec<serde_json::Value>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(socket).expect("bind fixture daemon");
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&methods);
        std::thread::spawn(move || {
            for result in responses {
                let (mut stream, _) = listener.accept().expect("accept fixture request");
                let mut request = String::new();
                BufReader::new(stream.try_clone().expect("clone fixture stream"))
                    .read_line(&mut request)
                    .expect("read fixture request");
                let request: ControlMessage =
                    serde_json::from_str(&request).expect("decode fixture request");
                let (id, method) = match request {
                    ControlMessage::Request { id, method, .. } => (id, method),
                    other => panic!("unexpected fixture message: {other:?}"),
                };
                recorded.lock().expect("methods").push(method);
                serde_json::to_writer(
                    &mut stream,
                    &ControlMessage::Response {
                        id,
                        result: Ok(result),
                    },
                )
                .expect("write fixture response");
                stream.write_all(b"\n").expect("terminate fixture response");
            }
            recorded.lock().expect("methods").clone()
        })
    }

    fn test_startup(release: impl Fn() + Send + Sync + 'static) -> Arc<DeferredDaemonStartup> {
        Arc::new(DeferredDaemonStartup {
            socket_path: PathBuf::from("/nonexistent/diri-test.sock"),
            cancelled: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(Mutex::new(StartupPhase::Planned)),
            task: Mutex::new(None),
            cleanup_tasks: Mutex::new(Vec::new()),
            release: Arc::new(release),
        })
    }

    fn wait_for_phase(startup: &DeferredDaemonStartup, expected: StartupPhase) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let actual = *startup
                .phase
                .lock()
                .expect("daemon startup phase lock poisoned");
            if actual == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "startup phase stayed at {actual:?}; expected {expected:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct FixtureConnectionGuard(Arc<AtomicUsize>);

    impl Drop for FixtureConnectionGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// A small Engine control fixture that applies the real active-connection
    /// rule: the idle-release connection may exit only when it is the sole
    /// connection. Long-lived Hello clients remain open until their peer
    /// closes, so these tests catch ordering mistakes hidden by a scripted
    /// one-request server.
    struct CountAwareDaemon {
        stop: Arc<AtomicBool>,
        active: Arc<AtomicUsize>,
        refusals: mpsc::Receiver<()>,
        accepted: mpsc::Receiver<()>,
        server: std::thread::JoinHandle<Vec<String>>,
    }

    impl CountAwareDaemon {
        fn bind(socket: &Path) -> Self {
            let listener = UnixListener::bind(socket).expect("bind count-aware daemon");
            listener
                .set_nonblocking(true)
                .expect("configure count-aware listener");
            let stop = Arc::new(AtomicBool::new(false));
            let active = Arc::new(AtomicUsize::new(0));
            let methods = Arc::new(Mutex::new(Vec::new()));
            let (refusal_tx, refusals) = mpsc::channel();
            let (accepted_tx, accepted) = mpsc::channel();
            let server_stop = Arc::clone(&stop);
            let server_active = Arc::clone(&active);
            let server_methods = Arc::clone(&methods);
            let server = std::thread::spawn(move || {
                let mut workers = Vec::new();
                while !server_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream
                                .set_nonblocking(false)
                                .expect("configure counted connection");
                            server_active.fetch_add(1, Ordering::AcqRel);
                            let active = Arc::clone(&server_active);
                            let methods = Arc::clone(&server_methods);
                            let refusal_tx = refusal_tx.clone();
                            let accepted_tx = accepted_tx.clone();
                            workers.push(std::thread::spawn(move || {
                                let _connection = FixtureConnectionGuard(Arc::clone(&active));
                                serve_counted_connection(
                                    stream,
                                    &active,
                                    &methods,
                                    &refusal_tx,
                                    &accepted_tx,
                                );
                            }));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept count-aware connection: {error}"),
                    }
                }
                for worker in workers {
                    worker.join().expect("count-aware connection worker");
                }
                server_methods.lock().expect("recorded methods").clone()
            });
            Self {
                stop,
                active,
                refusals,
                accepted,
                server,
            }
        }

        fn finish(self) -> Vec<String> {
            self.stop.store(true, Ordering::Release);
            self.server.join().expect("count-aware daemon")
        }
    }

    fn serve_counted_connection(
        mut stream: UnixStream,
        active: &AtomicUsize,
        methods: &Mutex<Vec<String>>,
        refusal_tx: &mpsc::Sender<()>,
        accepted_tx: &mpsc::Sender<()>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone counted connection"));
        loop {
            let mut request = String::new();
            match reader.read_line(&mut request) {
                Ok(0) => return,
                Err(error) => panic!("read counted request: {error}"),
                Ok(_) => {}
            }
            let request: ControlMessage =
                serde_json::from_str(&request).expect("decode counted request");
            let (id, method) = match request {
                ControlMessage::Request { id, method, .. } => (id, method),
                other => panic!("unexpected counted message: {other:?}"),
            };
            methods.lock().expect("record methods").push(method.clone());
            let result = match method.as_str() {
                Method::HELLO => serde_json::json!({
                    "proto": diri_proto::WIRE_VERSION,
                    "build": "fixture",
                    "pid": 42,
                    "engineKind": RUST_ENGINE_KIND,
                }),
                Method::DAEMON_SHUTDOWN_IF_IDLE if active.load(Ordering::Acquire) > 1 => {
                    let _ = refusal_tx.send(());
                    serde_json::json!({
                        "willExit": false,
                        "reason": "another control client still requires the Engine",
                    })
                }
                Method::DAEMON_SHUTDOWN_IF_IDLE => {
                    let _ = accepted_tx.send(());
                    serde_json::json!({ "willExit": true })
                }
                other => panic!("unexpected counted method: {other}"),
            };
            serde_json::to_writer(
                &mut stream,
                &ControlMessage::Response {
                    id,
                    result: Ok(result),
                },
            )
            .expect("write counted response");
            stream.write_all(b"\n").expect("terminate counted response");
        }
    }

    fn open_fixture_client(socket: &Path) -> UnixStream {
        let mut stream = UnixStream::connect(socket).expect("connect fixture client");
        fixture_hello(&mut stream, 99);
        stream
    }

    fn fixture_hello(stream: &mut UnixStream, id: u64) {
        serde_json::to_writer(
            &mut *stream,
            &ControlMessage::Request {
                id,
                method: Method::HELLO.to_owned(),
                params: Some(serde_json::json!({
                    "proto": diri_proto::WIRE_VERSION,
                    "build": "fixture-client",
                })),
            },
        )
        .expect("write fixture Hello");
        stream.write_all(b"\n").expect("terminate fixture Hello");
        let mut response = String::new();
        BufReader::new(stream.try_clone().expect("clone fixture client"))
            .read_line(&mut response)
            .expect("read fixture Hello");
        let response: ControlMessage = serde_json::from_str(&response).expect("decode Hello reply");
        assert!(
            matches!(response, ControlMessage::Response { id: response_id, result: Ok(_)} if response_id == id)
        );
    }

    #[test]
    fn normal_completion_releases_the_client_only_after_supervision() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let startup = test_startup(|| {});
        let client = Arc::new(DaemonClient::with_socket_path(
            "/nonexistent/diri-test.sock",
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        // The call represents the line immediately after `open_main_window`.
        // It must return while arbitrarily slow supervision remains blocked.
        startup.start_after_window_open_with(&runtime, Arc::clone(&client), move |_| {
            started_tx.send(()).expect("publish worker start");
            release_rx.recv().expect("release supervision");
            StartupOutcome::EngineExpected
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking work starts away from the caller");
        assert!(matches!(
            &*client.connection_state().borrow(),
            diri_client::ConnectionState::Disconnected(message)
                if message == "not connected to daemon"
        ));
        release_tx.send(()).expect("finish supervision");
        wait_for_phase(
            &startup,
            StartupPhase::Complete(StartupOutcome::EngineExpected),
        );
        assert!(
            !matches!(
                &*client.connection_state().borrow(),
                diri_client::ConnectionState::Disconnected(message)
                    if message == "not connected to daemon"
            ),
            "normal completion must release the reconnect loop"
        );
    }

    #[test]
    fn completed_startup_signals_its_client_then_owns_real_idle_release() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let tmp = tempfile::tempdir().expect("temporary lifecycle fixture");
        let socket = tmp.path().join("daemon.sock");
        let fixture = CountAwareDaemon::bind(&socket);
        let release_socket = socket.clone();
        let startup = test_startup(move || release_daemon_if_idle(&release_socket));
        let client = Arc::new(DaemonClient::with_socket_path(socket));

        startup.start_after_window_open_with(&runtime, Arc::clone(&client), |_| {
            StartupOutcome::EngineExpected
        });
        wait_for_phase(
            &startup,
            StartupPhase::Complete(StartupOutcome::EngineExpected),
        );
        runtime
            .block_on(client.wait_until_connected(Duration::from_secs(2)))
            .expect("client performs authoritative Hello");

        let quit_started = std::time::Instant::now();
        startup.request_shutdown(&runtime, &client);
        assert!(
            quit_started.elapsed() < Duration::from_millis(200),
            "real idle release must be transferred within GPUI's quit budget"
        );
        fixture
            .refusals
            .recv_timeout(Duration::from_secs(1))
            .expect("idle release refuses while the paused app client is open");

        // `request_shutdown` already sent the synchronous signal. Merely
        // polling the runtime must close the app connection; no quit future or
        // awaited DaemonClient::shutdown is required.
        runtime.block_on(async {
            let mut states = client.connection_state();
            tokio::time::timeout(Duration::from_secs(1), async {
                while !matches!(
                    &*states.borrow_and_update(),
                    diri_client::ConnectionState::Disconnected(_)
                ) {
                    states.changed().await.expect("client state channel");
                }
            })
            .await
            .expect("begin_shutdown closes the app client");
        });
        fixture
            .accepted
            .recv_timeout(Duration::from_secs(1))
            .expect("retry becomes the sole connection and is accepted");
        wait_for_phase(&startup, StartupPhase::Cleaned);
        let methods = fixture.finish();
        assert_eq!(methods.first().map(String::as_str), Some(Method::HELLO));
        assert!(
            methods
                .iter()
                .filter(|method| method.as_str() == Method::DAEMON_SHUTDOWN_IF_IDLE)
                .count()
                >= 2
        );
    }

    #[test]
    fn persistent_second_client_does_not_pin_runtime_teardown() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let tmp = tempfile::tempdir().expect("temporary lifecycle fixture");
        let socket = tmp.path().join("daemon.sock");
        let fixture = CountAwareDaemon::bind(&socket);
        let release_socket = socket.clone();
        let startup = test_startup(move || release_daemon_if_idle(&release_socket));
        let client = Arc::new(DaemonClient::with_socket_path(&socket));

        startup.start_after_window_open_with(&runtime, Arc::clone(&client), |_| {
            StartupOutcome::EngineExpected
        });
        runtime
            .block_on(client.wait_until_connected(Duration::from_secs(2)))
            .expect("app client connects");
        wait_for_phase(
            &startup,
            StartupPhase::Complete(StartupOutcome::EngineExpected),
        );
        let mut external_client = open_fixture_client(&socket);

        let shutdown_started = std::time::Instant::now();
        startup.request_shutdown(&runtime, &client);
        fixture
            .refusals
            .recv_timeout(Duration::from_secs(1))
            .expect("external client makes idle release authoritative refusal");
        drop(runtime);
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(1),
            "an independent client must not make Tokio teardown wait for the 10s publication timeout"
        );
        assert_eq!(
            *startup.phase.lock().unwrap(),
            StartupPhase::Cleaned,
            "runtime teardown waits for the bounded cleanup worker"
        );
        let connection_deadline = std::time::Instant::now() + Duration::from_millis(200);
        while fixture.active.load(Ordering::Acquire) != 1
            && std::time::Instant::now() < connection_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            fixture.active.load(Ordering::Acquire),
            1,
            "only the independent client remains after app shutdown"
        );
        assert!(
            fixture.accepted.try_recv().is_err(),
            "the Engine must honor the independent client's refusal"
        );
        fixture_hello(&mut external_client, 100);

        drop(external_client);
        drop(client);
        let methods = fixture.finish();
        assert!(
            methods
                .iter()
                .filter(|method| method.as_str() == Method::DAEMON_SHUTDOWN_IF_IDLE)
                .count()
                >= 2,
            "cleanup retries only for its short client-close grace"
        );
    }

    #[test]
    fn delayed_socket_publication_keeps_the_long_retry_window() {
        let tmp = tempfile::tempdir().expect("temporary delayed daemon fixture");
        let socket = tmp.path().join("daemon.sock");
        let server_socket = socket.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(STARTUP_RELEASE_CLIENT_GRACE + Duration::from_millis(200));
            let listener = UnixListener::bind(server_socket).expect("publish delayed socket");
            let (mut stream, _) = listener.accept().expect("accept delayed idle release");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone delayed release"))
                .read_line(&mut request)
                .expect("read delayed release");
            let request: ControlMessage =
                serde_json::from_str(&request).expect("decode delayed release");
            let id = match request {
                ControlMessage::Request { id, method, .. }
                    if method == Method::DAEMON_SHUTDOWN_IF_IDLE =>
                {
                    id
                }
                other => panic!("unexpected delayed release request: {other:?}"),
            };
            serde_json::to_writer(
                &mut stream,
                &ControlMessage::Response {
                    id,
                    result: Ok(serde_json::json!({ "willExit": true })),
                },
            )
            .expect("write delayed release response");
            stream
                .write_all(b"\n")
                .expect("terminate delayed release response");
        });

        let release_started = std::time::Instant::now();
        release_daemon_if_idle(&socket);
        let elapsed = release_started.elapsed();
        server.join().expect("delayed publication fixture");
        assert!(
            elapsed >= STARTUP_RELEASE_CLIENT_GRACE + Duration::from_millis(150),
            "socket publication retries must outlive the unrelated client-close grace"
        );
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn quit_returns_inside_gpui_budget_and_worker_cancels_pending_mutation() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let startup = test_startup(|| {});
        let client = Arc::new(DaemonClient::with_socket_path(
            "/nonexistent/diri-test.sock",
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mutated_tx, mutated_rx) = mpsc::channel();

        startup.start_after_window_open_with(&runtime, Arc::clone(&client), move |cancelled| {
            started_tx.send(()).expect("publish worker start");
            release_rx.recv().expect("release supervision");
            if cancelled.load(Ordering::Acquire) {
                StartupOutcome::NoEngineExpected
            } else {
                mutated_tx.send(()).expect("publish mutation");
                StartupOutcome::EngineExpected
            }
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("supervision starts");

        let quit_started = std::time::Instant::now();
        startup.request_shutdown(&runtime, &client);
        assert!(
            quit_started.elapsed() < Duration::from_millis(200),
            "startup cancellation must fit inside GPUI's quit-future budget"
        );
        assert_eq!(
            *startup.phase.lock().unwrap(),
            StartupPhase::Cancelling,
            "the worker retains cleanup ownership while supervision is blocked"
        );
        assert!(
            startup.task.lock().unwrap().is_some(),
            "quit must never take and detach the sole startup handle"
        );

        release_tx.send(()).expect("release supervision");
        wait_for_phase(&startup, StartupPhase::Cleaned);
        assert!(
            mutated_rx.try_recv().is_err(),
            "a mutation behind a cancellation checkpoint must not run during quit"
        );
    }

    #[test]
    fn dropped_quit_future_cannot_leak_an_engine_from_inflight_startup() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let (cleaned_tx, cleaned_rx) = mpsc::channel();
        let startup = test_startup(move || cleaned_tx.send(()).expect("publish idle release"));
        let client = Arc::new(DaemonClient::with_socket_path(
            "/nonexistent/diri-test.sock",
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        startup.start_after_window_open_with(&runtime, Arc::clone(&client), move |_| {
            started_tx.send(()).expect("publish worker start");
            release_rx.recv().expect("release supervision");
            // Model supervision that crossed its final spawn checkpoint just
            // before quit published cancellation.
            StartupOutcome::EngineExpected
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("supervision starts");
        startup.request_shutdown(&runtime, &client);

        // No future is awaited here. Releasing the in-flight worker models
        // GPUI dropping its quit future; the supervisor must clean itself.
        release_tx.send(()).expect("release supervision");
        cleaned_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("runtime-owned worker performs idle release");
        wait_for_phase(&startup, StartupPhase::Cleaned);
    }

    #[test]
    fn cancellation_checkpoint_prevents_a_pending_daemon_spawn() {
        let tmp = tempfile::tempdir().expect("temporary daemon fixture");
        let socket = tmp.path().join("absent.sock");
        let marker = tmp.path().join("spawned");
        let daemon = tmp.path().join("dirijord-rs");
        std::fs::write(
            &daemon,
            format!("#!/bin/sh\nprintf launched > '{}'\n", marker.display()),
        )
        .expect("write fixture daemon");
        let mut permissions = std::fs::metadata(&daemon).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).unwrap();
        let cancelled = AtomicBool::new(true);

        assert_eq!(
            ensure_daemon_running_with_cancellation(&socket, Some(daemon), None, &cancelled),
            StartupOutcome::NoEngineExpected
        );
        assert!(
            !marker.exists(),
            "quit cancellation must be observed before detached spawn"
        );
    }

    #[test]
    fn rust_engine_identity_is_read_from_the_live_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("daemon.sock");
        let server = serve_control(
            &socket,
            vec![serde_json::json!({
                "proto": diri_proto::WIRE_VERSION,
                "build": "fixture",
                "pid": 42,
                "engineKind": RUST_ENGINE_KIND
            })],
        );

        let hello = probe_daemon(&socket).expect("probe Rust Engine");
        assert_eq!(hello.engine_kind.as_deref(), Some(RUST_ENGINE_KIND));
        assert_eq!(server.join().expect("fixture server"), vec![Method::HELLO]);
    }

    #[test]
    fn daemon_refresh_requires_an_exact_executable_hash() {
        let hello = |hash: Option<&str>| {
            serde_json::from_value::<HelloResult>(serde_json::json!({
                "proto": diri_proto::WIRE_VERSION,
                "build": "fixture",
                "pid": 42,
                "engineKind": RUST_ENGINE_KIND,
                "executableHash": hash,
            }))
            .expect("hello")
        };

        assert!(!daemon_needs_refresh(&hello(Some("current")), "current"));
        assert!(daemon_needs_refresh(&hello(Some("stale")), "current"));
        assert!(daemon_needs_refresh(&hello(None), "current"));
    }

    #[test]
    fn executable_hash_is_streamed_as_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join("engine");
        std::fs::write(&executable, b"abc").unwrap();
        assert_eq!(
            executable_sha256(&executable).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn upgrade_shutdown_uses_the_persisting_daemon_rpc() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("daemon.sock");
        let server = serve_control(&socket, vec![serde_json::json!({})]);

        stop_daemon_for_upgrade(&socket, None).expect("fixture daemon releases its listener");
        assert_eq!(
            server.join().expect("fixture server"),
            vec![Method::DAEMON_SHUTDOWN]
        );
    }

    #[test]
    fn an_outdated_live_daemon_is_replaced_by_the_resolved_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("daemon.sock");
        let marker = tmp.path().join("new-daemon-launched");
        let daemon = tmp.path().join("dirijord-rs");
        std::fs::write(
            &daemon,
            format!("#!/bin/sh\nprintf launched > '{}'\n", marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&daemon).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).unwrap();

        let server = serve_control(
            &socket,
            vec![
                serde_json::json!({
                    "proto": diri_proto::WIRE_VERSION,
                    "build": "outdated",
                    "pid": i32::MAX,
                    "engineKind": RUST_ENGINE_KIND,
                    "executableHash": "old-bytes",
                }),
                serde_json::json!({}),
            ],
        );

        ensure_daemon_running_with(&socket, Some(daemon), None);
        assert_eq!(
            server.join().expect("fixture server"),
            vec![Method::HELLO, Method::DAEMON_SHUTDOWN]
        );
        // Wait on a deadline, not a fixed iteration count. This is a real
        // spawned process writing a real file, and 50 × 10ms only ever bought
        // half a second — enough on an idle machine, not enough on a loaded CI
        // runner, where this failed with the marker simply not written yet.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(marker).expect("new daemon marker"),
            "launched"
        );
    }

    #[test]
    fn resolves_bundled_daemon_from_contents_macos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let exe = root.join("Contents/MacOS/diri");
        touch_executable(&exe);
        let daemon = root.join("Contents/Resources/bin/dirijord-rs");
        touch_executable(&daemon);

        let resolved =
            resolve_daemon_path_from(None, Some(exe), None).expect("bundled daemon should resolve");
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(daemon).unwrap(),
        );
    }

    #[test]
    fn env_override_wins_when_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let override_path = tmp.path().join("custom/dirijord-rs");
        touch_executable(&override_path);

        let resolved = resolve_daemon_path_from(Some(override_path.clone()), None, None).unwrap();
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn ignores_non_executable_override_and_falls_back_next_to_exe() {
        let tmp = tempfile::tempdir().unwrap();
        // A non-executable override must be skipped.
        let bad_override = tmp.path().join("not-exec");
        std::fs::write(&bad_override, b"plain").unwrap();

        let exe = tmp.path().join("bin/diri");
        touch_executable(&exe);
        let sibling = tmp.path().join("bin/dirijord-rs");
        touch_executable(&sibling);

        let resolved = resolve_daemon_path_from(Some(bad_override), Some(exe), None).unwrap();
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(sibling).unwrap(),
        );
    }

    #[test]
    fn returns_none_when_nothing_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("Contents/MacOS/diri");
        touch_executable(&exe);
        // No daemon anywhere; cwd points at an empty dir.
        assert!(
            resolve_daemon_path_from(None, Some(exe), Some(tmp.path().to_path_buf())).is_none()
        );
    }
}
