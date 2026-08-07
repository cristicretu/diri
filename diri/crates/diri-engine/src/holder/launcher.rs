//! Daemon-side holder launching: find or start the shared manager, then ask
//! it to host a session holder.
//!
//! The cross-daemon `flock` launch lock is what makes concurrent daemons (or
//! a daemon racing a manager's idle exit) safe: whoever holds it either finds
//! a live manager or starts exactly one.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::client::{HolderClient, HolderManagerClient};
use super::paths::{HolderManagerPaths, HolderPaths};
use super::protocol::HolderLaunchSpec;
use super::{HolderError, HolderResult};

/// How long to wait for a freshly spawned manager: 250 × 20ms = 5s.
const READINESS_ATTEMPTS: u32 = 250;
const READINESS_INTERVAL: Duration = Duration::from_millis(20);

pub struct HolderLauncher;

impl HolderLauncher {
    /// Ensures a live holder serves `spec`, launching the shared manager if
    /// needed. Returns the pid serving the session (the manager's, or a
    /// pre-manager holder's when one is adopted).
    pub fn launch(
        executable_path: &Path,
        paths: &HolderPaths,
        spec: &HolderLaunchSpec,
    ) -> HolderResult<i32> {
        std::fs::create_dir_all(&paths.directory)
            .map_err(|error| HolderError::io("create holders directory", error))?;

        // A concurrent revive or pre-manager holder may already own this
        // exact session. Adopt it without starting an otherwise-idle manager.
        if HolderClient::new(paths.socket()).is_alive()
            && let Some(serving_pid) = read_pid_file(&paths.pid_file())
        {
            return Ok(serving_pid);
        }

        let manager_paths = HolderManagerPaths::new(&paths.directory);
        let _lock = LaunchLock::acquire(&manager_paths.launch_lock())?;

        let manager = HolderManagerClient::new(manager_paths.socket());
        if !manager.is_alive() {
            spawn_manager(executable_path, &manager_paths.directory)?;
            let ready = (0..READINESS_ATTEMPTS).any(|_| {
                if manager.is_alive() {
                    return true;
                }
                std::thread::sleep(READINESS_INTERVAL);
                false
            });
            if !ready {
                return Err(HolderError::Launch(
                    "shared holder manager did not become ready".into(),
                ));
            }
        }

        match manager.launch(spec) {
            Ok(pid) => Ok(pid),
            Err(error) => {
                // The manager may have crossed its no-session idle boundary
                // between the readiness check and the launch request. One
                // fresh-manager retry is safe while the cross-daemon launch
                // lock is held.
                if manager.is_alive() {
                    return Err(error);
                }
                spawn_manager(executable_path, &manager_paths.directory)?;
                for _ in 0..READINESS_ATTEMPTS {
                    if let Ok(pid) = manager.launch(spec) {
                        return Ok(pid);
                    }
                    std::thread::sleep(READINESS_INTERVAL);
                }
                Err(HolderError::Launch(
                    "shared holder manager did not accept launch".into(),
                ))
            }
        }
    }

    /// Where the holder binary lives: the `DIRIJOR_HOLDER_PATH` override, or
    /// next to the running executable.
    pub fn default_executable_path() -> PathBuf {
        if let Ok(configured) = std::env::var("DIRIJOR_HOLDER_PATH") {
            let path = PathBuf::from(&configured);
            if is_executable(&path) {
                return path;
            }
        }
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok())
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let candidates: Vec<PathBuf> = beside
            .iter()
            .flat_map(|dir| ["diri-holder", "dirijord-holder"].map(|name| dir.join(name)))
            .collect();
        candidates
            .iter()
            .find(|candidate| is_executable(candidate))
            .cloned()
            .unwrap_or_else(|| {
                candidates
                    .first()
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from("diri-holder"))
            })
    }
}

/// A held `flock`; released on drop.
pub(crate) struct LaunchLock {
    file: std::fs::File,
}

impl LaunchLock {
    pub(crate) fn acquire(path: &Path) -> HolderResult<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|error| HolderError::Launch(format!("open {}: {error}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        // SAFETY: flock on an owned fd; blocks until the lock is granted.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(HolderError::Launch(format!(
                "lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self { file })
    }
}

impl Drop for LaunchLock {
    fn drop(&mut self) {
        // SAFETY: unlocking an fd this struct owns.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Starts the manager fully detached: its own session (no terminal/SIGHUP
/// coupling to the daemon), stdio on /dev/null, no inherited descriptors.
/// The OS does not kill it when its daemon parent exits, which is the whole
/// point: every managed PTY survives daemon crashes and upgrades.
fn spawn_manager(executable_path: &Path, directory: &Path) -> HolderResult<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(executable_path);
    command
        .arg("--manager")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure runs between fork and exec and uses only
    // async-signal-safe syscalls.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Nothing of the daemon's may leak into the long-lived manager.
            let max = libc::getdtablesize();
            for fd in 3..max {
                libc::close(fd);
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| {
        HolderError::Launch(format!("spawn {}: {error}", executable_path.display()))
    })?;
    // Reap the direct child when the (detached) manager eventually exits, so
    // it never lingers as a zombie of the daemon.
    std::thread::Builder::new()
        .name("holder-manager-reaper".into())
        .spawn(move || {
            let mut child = child;
            let _ = child.wait();
        })
        .map_err(|error| HolderError::io("spawn reaper", error))?;
    Ok(())
}

fn read_pid_file(path: &Path) -> Option<i32> {
    let pid = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    (pid > 1).then_some(pid)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
