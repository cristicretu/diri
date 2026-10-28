//! The shared holder manager: many independent holders in one detached
//! process.
//!
//! One manager per holders directory hosts a [`HolderServer`] thread per
//! session. The manager exits only after every hosted child has exited and an
//! idle grace period has elapsed, so daemon crashes and upgrades never take a
//! PTY with them. While any session is hosted there is no timer at all: the
//! watchdog parks on a condvar and only counts down once the last session ends.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::client::HolderClient;
use super::guard::GroupGuard;
use super::paths::{HolderManagerPaths, HolderPaths, MANAGER_PROTOCOL_VERSION};
use super::protocol::{
    HolderLaunchSpec, HolderManagerOperation, HolderManagerRequest, HolderManagerResponse,
};
use super::server::HolderServer;
use super::socket;
use super::{HolderError, HolderResult};

pub struct HolderManagerServer {
    paths: HolderManagerPaths,
    idle_timeout: Duration,
    group_guard: bool,
    agent_launcher: Option<std::path::PathBuf>,
    /// Development builds: the Engine whose departure, with no other Engine
    /// checking in within [`ABANDON_GRACE`], ends the hosted sessions.
    engine: Option<i32>,
    abandon_grace: Duration,
}

/// How long a development manager waits for an Engine to come back before it
/// ends the sessions nobody will ever attach to again.
pub const ABANDON_GRACE: Duration = Duration::from_secs(10 * 60);
/// `--engine-pid <pid>`: retire abandoned sessions (development builds only).
pub const ENGINE_PID_FLAG: &str = "--engine-pid";

/// The latest Engine to check in, and a wakeup for a waiter on its exit.
struct EngineWatch {
    pid: Mutex<i32>,
    changed: Condvar,
}

struct State {
    active: Mutex<HashSet<String>>,
    /// `Some(when)` while no session is hosted: the moment the manager
    /// became idle (or was last pinged while idle). The watchdog exits the
    /// process once `idle_timeout` passes with this unchanged.
    idle_since: Mutex<Option<Instant>>,
    /// Wakes the watchdog when the last session ends or the manager stops.
    /// Signalled with `idle_since` held, so a wakeup cannot slip between the
    /// watchdog's check and its wait.
    idle_changed: Condvar,
    shutting_down: AtomicBool,
    listen_fd: AtomicI32,
    /// Development builds only (see [`HolderManagerServer::with_engine`]).
    engine: Option<EngineWatch>,
    /// Every return from a watchdog wait, so a test can prove it stays parked.
    #[cfg(test)]
    watchdog_wakeups: std::sync::atomic::AtomicUsize,
}

impl State {
    fn new(listen_fd: i32, engine: Option<i32>) -> Self {
        Self {
            engine: engine.map(|pid| EngineWatch {
                pid: Mutex::new(pid),
                changed: Condvar::new(),
            }),
            active: Mutex::new(HashSet::new()),
            idle_since: Mutex::new(Some(Instant::now())),
            idle_changed: Condvar::new(),
            shutting_down: AtomicBool::new(false),
            listen_fd: AtomicI32::new(listen_fd),
            #[cfg(test)]
            watchdog_wakeups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Records the hosted set becoming empty or not, and tells the watchdog.
    fn set_idle(&self, since: Option<Instant>) {
        *self.idle_since.lock().expect("idle") = since;
        self.idle_changed.notify_all();
    }

    fn stop_watchdog(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Taking the lock orders this after the watchdog's own check of the
        // flag; without it the notify could land before the wait and be lost.
        let _idle = self.idle_since.lock().expect("idle");
        self.idle_changed.notify_all();
    }
}

impl HolderManagerServer {
    pub fn new(directory: &Path, idle_timeout: Duration) -> Self {
        Self {
            paths: HolderManagerPaths::new(directory),
            idle_timeout: idle_timeout.max(Duration::from_millis(100)),
            group_guard: false,
            agent_launcher: None,
            engine: None,
            abandon_grace: ABANDON_GRACE,
        }
    }

    /// Ends the hosted sessions once `engine` has exited and no Engine has
    /// checked in for [`ABANDON_GRACE`]. Only development Engines ask for
    /// this: an installed Engine's sessions must outlive its crashes.
    #[must_use]
    pub fn with_engine(mut self, engine: Option<i32>, grace: Duration) -> Self {
        self.engine = engine.filter(|&pid| pid > 1);
        self.abandon_grace = grace;
        self
    }

    /// Starts each hosted Agent through the app's main executable as a
    /// launchd job of its own (macOS; see [`diri_pty::detached`]). Its
    /// one-shot sockets go in this manager's private directory.
    #[must_use]
    pub fn with_agent_launcher(mut self, helper: Option<std::path::PathBuf>) -> Self {
        self.agent_launcher = helper;
        self
    }

    /// Runs a [`GroupGuard`] beside the manager, spawned from this process's
    /// own executable, so hosted process groups die with the manager rather
    /// than outliving it stopped. Only the real `diri-holder` binary can do
    /// this; an in-process test manager has no executable to re-run.
    #[must_use]
    pub fn with_group_guard(mut self) -> Self {
        self.group_guard = true;
        self
    }

    pub fn run(&self) -> HolderResult<()> {
        diri_platform::private::create_dir_all(&self.paths.directory)
            .map_err(|error| HolderError::io("create holders directory", error))?;
        // Before any PTY or listener exists, so the guard inherits nothing.
        // A guard that fails to start costs only crash cleanup, not sessions.
        let guard = self
            .group_guard
            .then(|| {
                std::env::current_exe()
                    .and_then(|executable| GroupGuard::spawn(&executable))
                    .inspect_err(|error| {
                        eprintln!("diri-holder manager: group guard unavailable: {error}");
                    })
                    .ok()
            })
            .flatten()
            .map(Arc::new);
        let listener = socket::listen(&self.paths.socket())?;
        // Raw ownership: the idle watchdog closes this fd to end the accept
        // loop (see `socket::accept_raw`).
        let listen_fd = {
            use std::os::fd::IntoRawFd;
            listener.into_raw_fd()
        };

        let state = Arc::new(State::new(listen_fd, self.engine));
        if state.engine.is_some() {
            let state = Arc::clone(&state);
            let directory = self.paths.directory.clone();
            std::thread::Builder::new()
                .name("holder-manager-engine".into())
                .spawn({
                    let grace = self.abandon_grace;
                    move || watch_engine(&state, &directory, grace)
                })
                .map_err(|error| HolderError::io("spawn engine watch", error))?;
        }
        diri_telemetry::event!("holder.manager_start", guard = guard.is_some());

        write_pid_file(&self.paths.pid_file())?;

        let watchdog = {
            let state = Arc::clone(&state);
            let idle_timeout = self.idle_timeout;
            std::thread::Builder::new()
                .name("holder-manager-idle".into())
                .spawn(move || watch_idle(&state, idle_timeout))
                .map_err(|error| HolderError::io("spawn watchdog", error))?
        };

        let mut result = Ok(());
        loop {
            match socket::accept_raw(listen_fd, || state.shutting_down.load(Ordering::SeqCst)) {
                Ok(Some(mut client)) => {
                    let response = match socket::read_json_line::<HolderManagerRequest>(&mut client)
                    {
                        Ok(request) => self
                            .handle(&state, guard.as_ref(), &request)
                            .unwrap_or_else(|error| {
                                HolderManagerResponse::failure(error.to_string())
                            }),
                        Err(error) => HolderManagerResponse::failure(error.to_string()),
                    };
                    let _ = socket::write_json_line(&mut client, &response);
                }
                Ok(None) => break, // the watchdog closed the listener
                Err(error) => {
                    result = Err(error);
                    // The fd is still open on this path; close it ourselves.
                    let fd = state.listen_fd.swap(-1, Ordering::SeqCst);
                    if fd >= 0 {
                        // SAFETY: raw-owned fd, surrendered above.
                        unsafe { libc::close(fd) };
                    }
                    break;
                }
            }
        }

        diri_telemetry::event!("holder.manager_exit", ok = result.is_ok());
        state.stop_watchdog();
        let _ = watchdog.join();
        self.cleanup_control_files();
        if let Some(guard) = guard {
            guard.finish();
        }
        result
    }

    fn handle(
        &self,
        state: &Arc<State>,
        guard: Option<&Arc<GroupGuard>>,
        request: &HolderManagerRequest,
    ) -> HolderResult<HolderManagerResponse> {
        if request.version != MANAGER_PROTOCOL_VERSION {
            return Err(HolderError::InvalidRequest(format!(
                "manager protocol {} is unsupported",
                request.version
            )));
        }
        if let (Some(watch), Some(pid)) = (&state.engine, request.engine_pid)
            && pid > 1
        {
            let mut current = watch.pid.lock().expect("engine");
            if *current != pid {
                *current = pid;
                watch.changed.notify_all();
            }
        }
        match request.op {
            HolderManagerOperation::Ping => {
                // A ping while idle restarts the grace period, as the Swift
                // manager's re-armed one-shot timer did.
                let mut idle = state.idle_since.lock().expect("idle");
                if idle.is_some() {
                    *idle = Some(Instant::now());
                }
                Ok(HolderManagerResponse::success(std::process::id() as i32))
            }

            HolderManagerOperation::Launch => {
                let spec = request.spec.clone().ok_or_else(|| {
                    HolderError::InvalidRequest("manager launch requires a spec".into())
                })?;
                self.validate(&spec)?;

                // A session from an older per-session holder may already own
                // this socket. Adopt it instead of creating a second
                // writer/child.
                if HolderClient::new(&spec.socket_path).is_alive() {
                    return Ok(HolderManagerResponse::success(std::process::id() as i32));
                }

                if state.shutting_down.load(Ordering::SeqCst) {
                    return Err(HolderError::Rejected("manager is shutting down".into()));
                }
                {
                    let mut active = state.active.lock().expect("active");
                    if !active.insert(spec.session_id.clone()) {
                        return Ok(HolderManagerResponse::success(std::process::id() as i32));
                    }
                    state.set_idle(None);
                }

                let state = Arc::clone(state);
                let guard = guard.cloned();
                let session_id = spec.session_id.clone();
                #[cfg(target_os = "macos")]
                let launcher =
                    self.agent_launcher
                        .clone()
                        .map(|helper| super::server::AgentLauncher {
                            helper,
                            rendezvous: self.paths.directory.clone(),
                        });
                #[cfg(not(target_os = "macos"))]
                let launcher: Option<super::server::AgentLauncher> = None;
                std::thread::Builder::new()
                    .name(format!("holder-{session_id}"))
                    .spawn(move || {
                        if let Err(error) = HolderServer::run_hosted(spec, guard, launcher.as_ref())
                        {
                            eprintln!("diri-holder manager: session {session_id}: {error}");
                            diri_telemetry::error_event!(
                                "holder.session_failed",
                                session = diri_telemetry::id(&session_id),
                                kind = crate::telemetry::holder_error_kind(&error),
                            );
                        }
                        let mut active = state.active.lock().expect("active");
                        active.remove(&session_id);
                        if active.is_empty() {
                            state.set_idle(Some(Instant::now()));
                        }
                    })
                    .map_err(|error| HolderError::io("spawn session holder", error))?;
                Ok(HolderManagerResponse::success(std::process::id() as i32))
            }
            HolderManagerOperation::ShutdownIfIdle => {
                if !state.active.lock().expect("active").is_empty() {
                    return Err(HolderError::Rejected(
                        "holder manager still owns live sessions".into(),
                    ));
                }
                stop_listener(state);
                Ok(HolderManagerResponse::success(std::process::id() as i32))
            }
        }
    }

    /// A spec must place its control files exactly where this manager's
    /// directory says they belong; anything else is a confused or hostile
    /// client.
    fn validate(&self, spec: &HolderLaunchSpec) -> HolderResult<()> {
        if spec.session_id.is_empty() {
            return Err(HolderError::InvalidRequest("session id is empty".into()));
        }
        let expected = HolderPaths::new(&self.paths.directory, &spec.session_id);
        if Path::new(&spec.socket_path) != expected.socket()
            || Path::new(&spec.pid_file_path) != expected.pid_file()
        {
            return Err(HolderError::InvalidRequest(
                "session control paths are outside manager directory".into(),
            ));
        }
        Ok(())
    }

    /// Remove only this incarnation's endpoint. The same launch lock used by
    /// the launcher closes the idle-exit/new-manager race; the pid check
    /// prevents an old process from unlinking a successor's fresh socket.
    fn cleanup_control_files(&self) {
        let Ok(lock) = super::launcher::LaunchLock::acquire(&self.paths.launch_lock()) else {
            return;
        };
        let owns_endpoint = std::fs::read_to_string(self.paths.pid_file())
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            == Some(std::process::id());
        if owns_endpoint {
            let _ = std::fs::remove_file(self.paths.socket());
            let _ = std::fs::remove_file(self.paths.pid_file());
        }
        drop(lock);
    }
}

/// Exits the process's accept loop once the manager has been idle for the
/// grace period.
///
/// While a session is hosted there is nothing to time, so the wait has no
/// deadline. Once idle it sleeps exactly until the grace period would end and
/// then looks again, which is what lets a ping push the deadline out without
/// having to wake anyone.
fn watch_idle(state: &State, idle_timeout: Duration) {
    let mut idle = state.idle_since.lock().expect("idle");
    loop {
        if state.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        idle = match *idle {
            None => state.idle_changed.wait(idle).expect("idle"),
            Some(since) => {
                let remaining = idle_timeout.saturating_sub(since.elapsed());
                if remaining.is_zero() {
                    break;
                }
                state
                    .idle_changed
                    .wait_timeout(idle, remaining)
                    .expect("idle")
                    .0
            }
        };
        #[cfg(test)]
        state.watchdog_wakeups.fetch_add(1, Ordering::SeqCst);
    }
    drop(idle);
    stop_listener(state);
}

fn stop_listener(state: &State) {
    if state.shutting_down.swap(true, Ordering::SeqCst) {
        return;
    }
    let fd = state.listen_fd.swap(-1, Ordering::SeqCst);
    if fd >= 0 {
        // Shutdown then close — only the close wakes a blocked accept(2) on
        // macOS. `shutting_down` is already set, so the loop exits.
        // SAFETY: raw-owned fd, claimed exactly once by the atomic swap.
        unsafe {
            libc::shutdown(fd, libc::SHUT_RDWR);
            libc::close(fd);
        }
    }
}

fn write_pid_file(path: &Path) -> HolderResult<()> {
    let contents = format!("{}\n", std::process::id());
    diri_platform::private::write(path, contents)
        .map_err(|error| HolderError::io("write pid file", error))
}

/// Development managers: once the Engine that checked in last has exited and
/// none checks in within `grace`, end every hosted session. Nobody will ever
/// attach to them again, and they would otherwise run (Agents included)
/// until reboot. Waits on the Engine's exit without polling.
fn watch_engine(state: &State, directory: &Path, grace: Duration) {
    let Some(watch) = &state.engine else {
        return;
    };
    loop {
        let pid = *watch.pid.lock().expect("engine");
        wait_for_exit(pid);
        let current = watch.pid.lock().expect("engine");
        let (current, waited) = watch
            .changed
            .wait_timeout_while(current, grace, |current| *current == pid)
            .expect("engine");
        if !waited.timed_out() || *current != pid {
            continue; // another Engine checked in: watch that one
        }
        drop(current);
        let sessions: Vec<String> = state
            .active
            .lock()
            .expect("active")
            .iter()
            .cloned()
            .collect();
        diri_telemetry::event!("holder.manager_abandoned", sessions = sessions.len());
        eprintln!(
            "diri-holder manager: no Engine for {grace:?}; ending {} abandoned session(s)",
            sessions.len()
        );
        for session in sessions {
            let _ = HolderClient::new(HolderPaths::new(directory, &session).socket()).kill_tree();
        }
        return;
    }
}

/// Blocks until `pid` exits (or is already gone).
fn wait_for_exit(pid: i32) {
    let Ok(watcher) = diri_pty::ExitWatcher::new(pid as u32) else {
        return; // ESRCH: gone already
    };
    let mut descriptor = libc::pollfd {
        fd: watcher.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd; no timeout. EINTR retried.
    while unsafe { libc::poll(&mut descriptor, 1, -1) } < 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
    {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::{HolderManagerServer, State, watch_idle};
    use crate::holder::{HolderManagerClient, HolderManagerPaths};

    #[cfg(unix)]
    #[test]
    fn manager_pid_files_and_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let holders = temp.path().join("holders");
        crate::holder::launcher::LaunchLock::acquire(&{
            diri_platform::private::create_dir_all(&holders).unwrap();
            holders.join("launch.lock")
        })
        .unwrap();
        let pid = holders.join("manager.pid");
        super::write_pid_file(&pid).unwrap();
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&holders), 0o700);
        assert_eq!(mode(&pid), 0o600);
        assert_eq!(mode(&holders.join("launch.lock")), 0o600);
    }

    /// A watchdog over a manager with no listener to close.
    fn watchdog(state: &Arc<State>, idle_timeout: Duration) -> std::thread::JoinHandle<()> {
        let state = Arc::clone(state);
        std::thread::spawn(move || watch_idle(&state, idle_timeout))
    }

    #[test]
    fn the_watchdog_never_wakes_while_a_session_is_hosted() {
        let idle_timeout = Duration::from_millis(100);
        let state = Arc::new(State::new(-1, None));
        state.set_idle(None);
        let watchdog = watchdog(&state, idle_timeout);

        // Several grace periods, and several of the old 250 ms ticks.
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(state.watchdog_wakeups.load(Ordering::SeqCst), 0);
        assert!(!watchdog.is_finished());

        // The last session ending starts the countdown, in full.
        let ended = Instant::now();
        state.set_idle(Some(ended));
        watchdog.join().expect("watchdog");
        assert!(ended.elapsed() >= idle_timeout);
        assert!(state.shutting_down.load(Ordering::SeqCst));
    }

    #[test]
    fn a_new_session_cancels_the_idle_countdown() {
        let idle_timeout = Duration::from_millis(150);
        let state = Arc::new(State::new(-1, None));
        let watchdog = watchdog(&state, idle_timeout);
        state.set_idle(None);

        std::thread::sleep(idle_timeout * 3);
        assert!(!watchdog.is_finished());
        assert!(!state.shutting_down.load(Ordering::SeqCst));

        state.stop_watchdog();
        watchdog.join().expect("watchdog");
    }

    #[test]
    fn an_idle_manager_accepts_immediate_graceful_shutdown() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let directory = temporary.path().join("holders");
        let server = HolderManagerServer::new(&directory, Duration::from_secs(30));
        let worker = std::thread::spawn(move || server.run());
        let client = HolderManagerClient::new(HolderManagerPaths::new(&directory).socket());
        for _ in 0..100 {
            if client.is_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        client.shutdown_if_idle().expect("idle shutdown");
        worker.join().expect("manager thread").expect("manager run");
        assert!(!client.is_alive());
    }
}
