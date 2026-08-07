//! The shared holder manager: many independent holders in one detached
//! process.
//!
//! One manager per holders directory hosts a [`HolderServer`] thread per
//! session. The manager exits only after every hosted child has exited and an
//! idle grace period has elapsed, so daemon crashes and upgrades never take a
//! PTY with them. While any session is hosted there is no timer at all — an
//! active manager has no polling wakeup beyond its watchdog's parked sleep.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::client::HolderClient;
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
}

struct State {
    active: Mutex<HashSet<String>>,
    /// `Some(when)` while no session is hosted: the moment the manager
    /// became idle (or was last pinged while idle). The watchdog exits the
    /// process once `idle_timeout` passes with this unchanged.
    idle_since: Mutex<Option<Instant>>,
    shutting_down: AtomicBool,
    listen_fd: AtomicI32,
}

impl HolderManagerServer {
    pub fn new(directory: &Path, idle_timeout: Duration) -> Self {
        Self {
            paths: HolderManagerPaths::new(directory),
            idle_timeout: idle_timeout.max(Duration::from_millis(100)),
        }
    }

    pub fn run(&self) -> HolderResult<()> {
        std::fs::create_dir_all(&self.paths.directory)
            .map_err(|error| HolderError::io("create holders directory", error))?;
        let listener = socket::listen(&self.paths.socket())?;
        // Raw ownership: the idle watchdog closes this fd to end the accept
        // loop (see `socket::accept_raw`).
        let listen_fd = {
            use std::os::fd::IntoRawFd;
            listener.into_raw_fd()
        };

        let state = Arc::new(State {
            active: Mutex::new(HashSet::new()),
            idle_since: Mutex::new(Some(Instant::now())),
            shutting_down: AtomicBool::new(false),
            listen_fd: AtomicI32::new(listen_fd),
        });

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
                        Ok(request) => self.handle(&state, &request).unwrap_or_else(|error| {
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

        state.shutting_down.store(true, Ordering::SeqCst);
        let _ = watchdog.join();
        self.cleanup_control_files();
        result
    }

    fn handle(
        &self,
        state: &Arc<State>,
        request: &HolderManagerRequest,
    ) -> HolderResult<HolderManagerResponse> {
        if request.version != MANAGER_PROTOCOL_VERSION {
            return Err(HolderError::InvalidRequest(format!(
                "manager protocol {} is unsupported",
                request.version
            )));
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
                    *state.idle_since.lock().expect("idle") = None;
                }

                let state = Arc::clone(state);
                let session_id = spec.session_id.clone();
                std::thread::Builder::new()
                    .name(format!("holder-{session_id}"))
                    .spawn(move || {
                        if let Err(error) = HolderServer::run(spec) {
                            eprintln!("diri-holder manager: session {session_id}: {error}");
                        }
                        let mut active = state.active.lock().expect("active");
                        active.remove(&session_id);
                        if active.is_empty() {
                            *state.idle_since.lock().expect("idle") = Some(Instant::now());
                        }
                    })
                    .map_err(|error| HolderError::io("spawn session holder", error))?;
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
/// grace period. Checks four times a second; the idle window is seconds.
fn watch_idle(state: &State, idle_timeout: Duration) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if state.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let expired = state
            .idle_since
            .lock()
            .expect("idle")
            .is_some_and(|since| since.elapsed() >= idle_timeout);
        if !expired {
            continue;
        }
        if state.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let fd = state.listen_fd.swap(-1, Ordering::SeqCst);
        if fd >= 0 {
            // Shutdown then close — only the close wakes a blocked accept(2)
            // on macOS. `shutting_down` is already set, so the loop exits.
            // SAFETY: raw-owned fd; nothing else closes it after the swap.
            unsafe {
                libc::shutdown(fd, libc::SHUT_RDWR);
                libc::close(fd);
            }
        }
        return;
    }
}

fn write_pid_file(path: &Path) -> HolderResult<()> {
    let contents = format!("{}\n", std::process::id());
    std::fs::write(path, contents).map_err(|error| HolderError::io("write pid file", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
