//! Daemon-side holder launching: find or start the shared manager, then ask
//! it to host a session holder.
//!
//! The cross-daemon `flock` launch lock is what makes concurrent daemons (or
//! a daemon racing a manager's idle exit) safe: whoever holds it either finds
//! a live manager or starts exactly one.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use super::client::{HolderClient, HolderManagerClient};
use super::paths::{HolderManagerPaths, HolderPaths};
use super::protocol::HolderLaunchSpec;
use super::{HolderError, HolderResult};

/// Holder manager launchd jobs (macOS): `<prefix><hex millis>`.
#[cfg(target_os = "macos")]
const MANAGER_LABEL_PREFIX: &str = "com.dirijor.diri.holders.";
/// What a launchd-started manager would otherwise lose from the Engine's
/// environment: the test idle window and the telemetry switches.
#[cfg(target_os = "macos")]
const MANAGER_ENVIRONMENT: [&str; 3] = [
    "DIRI_HOLDER_IDLE_SECONDS",
    "DIRI_TELEMETRY",
    "DIRI_TELEMETRY_ENDPOINT",
];

/// How long to wait for a freshly spawned manager: 250 × 20ms = 5s.
const READINESS_ATTEMPTS: u32 = 250;
/// Ask launchd whether a manager job already exited every this many waits
/// (about 0.5 s once the delays settle).
#[cfg(target_os = "macos")]
const JOB_CHECK_EVERY: u32 = 25;

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
            start_manager(executable_path, &manager_paths.directory, &manager)?;
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
                start_manager(executable_path, &manager_paths.directory, &manager)?;
                for delay in super::readiness_delays().take(READINESS_ATTEMPTS as usize) {
                    if let Ok(pid) = manager.launch(spec) {
                        return Ok(pid);
                    }
                    std::thread::sleep(delay);
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
        let candidates: Vec<PathBuf> = beside.iter().map(|dir| dir.join("diri-holder")).collect();
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

/// Set once a launchd-started manager failed to come up: this Engine spawns
/// its managers directly from then on rather than charging every new
/// session the same wait.
#[cfg(target_os = "macos")]
static LAUNCHD_MANAGER_FAILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Records, in the managers' directory, that launchd will not run diri's
/// jobs on this Mac. Every later Engine and manager reads it, so neither the
/// manager nor any Agent is offered to launchd again until the Holder
/// executable changes (an update) or `/tmp` is cleared (a restart).
#[cfg(target_os = "macos")]
const LAUNCHD_UNAVAILABLE_MARKER: &str = "launchd-unavailable";

/// Identifies the installed Holder executable: an update rewrites it.
#[cfg(target_os = "macos")]
fn launchd_marker_key(executable: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(executable).ok()?;
    Some(format!(
        "{} {} {}",
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec()
    ))
}

/// Whether launchd already failed this build of diri on this Mac.
#[cfg(target_os = "macos")]
pub(crate) fn launchd_unavailable(directory: &Path, executable: &Path) -> bool {
    use std::sync::atomic::Ordering;
    if LAUNCHD_MANAGER_FAILED.load(Ordering::Relaxed) {
        return true;
    }
    let recorded = std::fs::read_to_string(directory.join(LAUNCHD_UNAVAILABLE_MARKER));
    let failed = recorded.is_ok_and(|recorded| {
        launchd_marker_key(executable).is_some_and(|key| recorded.trim() == key)
    });
    if failed {
        LAUNCHD_MANAGER_FAILED.store(true, Ordering::Relaxed);
    }
    failed
}

/// Remembers that launchd failed to run one of diri's jobs (see
/// [`LAUNCHD_UNAVAILABLE_MARKER`]).
#[cfg(target_os = "macos")]
pub(crate) fn mark_launchd_unavailable(directory: &Path, executable: &Path) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    LAUNCHD_MANAGER_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
    let Some(key) = launchd_marker_key(executable) else {
        return;
    };
    let path = directory.join(LAUNCHD_UNAVAILABLE_MARKER);
    let written = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| file.write_all(key.as_bytes()));
    if let Err(error) = written {
        eprintln!("diri: record {}: {error}", path.display());
    }
}

/// How a manager was started.
enum Started {
    Direct,
    /// As a launchd job (macOS, bundled), through a relay job (see
    /// [`RELAY_FLAG`]).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Launchd {
        /// The one-shot job that submits the manager's job, then exits.
        relay: String,
        /// The manager's own job.
        manager: String,
    },
}

/// `diri-holder --relay <label> <manager arguments…>`: submits the manager as
/// launchd job `<label>`, then exits.
///
/// loginwindow remembers every launchd job a process of diri.app's coalition
/// submits as one of the app's "one-shot jobs". When diri.app exits while
/// macOS Background Task Management does not allow it to keep running (Login
/// Items › Allow in the Background switched off), loginwindow unloads those
/// jobs along with the coalition: a manager job submitted by the Engine took
/// every Holder, and with them every Agent, at each quit
/// (`launchJobCheck: … job com.dirijor.diri.holders.… agent not managed by
/// BTM so is not allowed` → `Unloading one-shot jobs for application "diri"`).
/// The Agents' own jobs, which the manager submits from its own coalition,
/// were left alone. So the Engine submits only this relay, which exits at
/// once; the manager's job is submitted from the relay's coalition.
pub const RELAY_FLAG: &str = "--relay";

/// Starts a manager and waits until it answers. Must hold the launch lock.
///
/// launchd can accept a job and then never run it (or run it far too late):
/// 0.9.3 failed every new session with exit 127 on such Macs. That job is
/// booted out, so it cannot come up later beside its replacement, and the
/// manager is spawned directly instead, as before 0.9.3.
fn start_manager(
    executable_path: &Path,
    directory: &Path,
    manager: &HolderManagerClient,
) -> HolderResult<()> {
    let started = spawn_manager(executable_path, directory)?;
    // A job whose process already ran and exited will not answer: stop
    // waiting for it then rather than at the deadline.
    #[cfg(target_os = "macos")]
    let mut job_exited = {
        // The relay job exits by design; only the manager's job exiting
        // means the manager will not answer.
        let label = match &started {
            Started::Launchd { manager, .. } => Some(manager.clone()),
            Started::Direct => None,
        };
        let mut checks = 0u32;
        move || {
            checks += 1;
            label.as_deref().is_some_and(|label| {
                checks.is_multiple_of(JOB_CHECK_EVERY) && {
                    let status = diri_pty::detached::job_status(label);
                    status.known && status.state == "not_running" && status.runs >= Some(1)
                }
            })
        }
    };
    #[cfg(not(target_os = "macos"))]
    let mut job_exited = || false;
    if wait_until_alive(manager, &mut job_exited) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if let Started::Launchd {
        relay,
        manager: label,
    } = started
    {
        if manager.is_alive() {
            return Ok(()); // just made it
        }
        let status = diri_pty::detached::job_status(&label);
        let bootout = diri_pty::detached::bootout_job(&label);
        let _ = diri_pty::detached::bootout_job(&relay);
        // A launchd that never runs the manager does not run the Agents'
        // jobs either: the direct manager starts them as its children.
        mark_launchd_unavailable(directory, executable_path);
        eprintln!(
            "diri-engine: launchd holder manager {label} never answered ({status:?}); spawning it"
        );
        diri_telemetry::incident!(
            "holder.manager_launchd_stuck",
            known = status.known,
            state = status.state,
            runs = status.runs,
            last_exit = status.last_exit,
            bootout_ok = bootout.is_ok(),
        );
        // Booted out, a late-starting job cannot race the direct one for the
        // socket; one that came up just now had no sessions yet.
        spawn_manager(executable_path, directory)?;
        if wait_until_alive(manager, &mut || false) {
            return Ok(());
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = started;
    Err(HolderError::Launch(
        "shared holder manager did not become ready".into(),
    ))
}

/// Waits for the manager to answer, up to the readiness deadline or until
/// `gave_up` says it never will.
fn wait_until_alive(manager: &HolderManagerClient, gave_up: &mut dyn FnMut() -> bool) -> bool {
    for delay in super::readiness_delays().take(READINESS_ATTEMPTS as usize) {
        if manager.is_alive() {
            return true;
        }
        if gave_up() {
            return manager.is_alive();
        }
        std::thread::sleep(delay);
    }
    false
}

/// Starts the manager fully detached: its own session (no terminal/SIGHUP
/// coupling to the daemon), stdio on /dev/null, no inherited descriptors.
/// The OS does not kill it when its daemon parent exits, which is the whole
/// point: every managed PTY survives daemon crashes and upgrades.
fn spawn_manager(executable_path: &Path, directory: &Path) -> HolderResult<Started> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut arguments: Vec<std::ffi::OsString> = vec!["--manager".into(), directory.into()];
    // Only an Engine that records hands its Holders a spool; tests and
    // embedders that never start the recorder launch quiet ones.
    if let Some(state_dir) = crate::telemetry::holder_state_dir() {
        arguments.push(crate::telemetry::HOLDER_TELEMETRY_FLAG.into());
        arguments.push(state_dir.into());
    }
    // A development Engine's manager ends its sessions once no Engine comes
    // back for them; an installed one's outlive every Engine crash.
    if crate::dev_build::is_development_build() {
        arguments.push(super::manager::ENGINE_PID_FLAG.into());
        arguments.push(std::process::id().to_string().into());
    }
    #[cfg(target_os = "macos")]
    let launchd_unavailable = launchd_unavailable(directory, executable_path);
    #[cfg(not(target_os = "macos"))]
    let launchd_unavailable = false;
    let agent_launcher = super::agent_launcher().filter(|_| !launchd_unavailable);
    if let Some(launcher) = &agent_launcher {
        arguments.push(super::AGENT_LAUNCHER_FLAG.into());
        arguments.push(launcher.into());
    }

    // macOS, bundled: the manager as a launchd job too, so it is in no app
    // launch's process coalition. Force-quitting diri.app, or macOS ending
    // the app's coalition at an update, would otherwise take the manager and
    // with it every PTY. One process either way; launchd reaps it.
    #[cfg(target_os = "macos")]
    if agent_launcher.is_some() {
        let (relay, label) = relay_and_manager_labels(&diri_pty::detached::label_suffix());
        let mut program: Vec<&std::ffi::OsStr> = vec![
            executable_path.as_os_str(),
            RELAY_FLAG.as_ref(),
            label.as_ref(),
        ];
        program.extend(arguments.iter().map(std::ffi::OsString::as_os_str));
        // A job starts with launchd's environment; carry over only the few
        // settings the manager reads.
        let environment: Vec<(&str, String)> = MANAGER_ENVIRONMENT
            .iter()
            .filter_map(|&name| std::env::var(name).ok().map(|value| (name, value)))
            .collect();
        match diri_pty::detached::bootstrap_job(&relay, &program, &environment, directory) {
            Ok(()) => {
                let _ = std::thread::Builder::new()
                    .name("holder-job-sweep".into())
                    .spawn(|| {
                        diri_pty::detached::sweep_finished_jobs(
                            MANAGER_LABEL_PREFIX,
                            std::time::Duration::from_secs(120),
                        );
                    });
                return Ok(Started::Launchd {
                    relay,
                    manager: label,
                });
            }
            Err(error) => {
                eprintln!("diri-engine: launchd holder manager unavailable, spawning it: {error}");
                diri_telemetry::incident!(
                    "holder.manager_launchd_unavailable",
                    io = diri_telemetry::io_error(&error),
                );
            }
        }
    }

    let mut command = Command::new(executable_path);
    command.args(&arguments);
    command
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
            let pid = child.id();
            match child.wait() {
                Ok(status) if !status.success() => {
                    eprintln!("diri-engine: holder manager {pid} exited unexpectedly: {status}");
                    use std::os::unix::process::ExitStatusExt;
                    diri_telemetry::incident!(
                        "holder.manager_died",
                        pid = pid,
                        code = status.code(),
                        signal = status.signal(),
                    );
                }
                Err(error) => {
                    eprintln!("diri-engine: holder manager {pid} wait failed: {error}");
                }
                _ => {}
            }
        })
        .map_err(|error| HolderError::io("spawn reaper", error))?;
    Ok(Started::Direct)
}

/// The relay's and the manager's job labels for one launch. Both keep the
/// hex-millis suffix [`diri_pty::detached::sweep_finished_jobs`] ages by;
/// the manager's is one millisecond later so the two never collide.
#[cfg(target_os = "macos")]
fn relay_and_manager_labels(suffix: &str) -> (String, String) {
    let millis = u64::from_str_radix(suffix, 16).unwrap_or_default();
    (
        format!("{MANAGER_LABEL_PREFIX}{millis:x}"),
        format!("{MANAGER_LABEL_PREFIX}{:x}", millis + 1),
    )
}

/// Runs the relay job (see [`RELAY_FLAG`]): `arguments` are what follows the
/// flag, the manager's job label and then the manager's own arguments.
/// Submits the manager's job and returns. Should launchd refuse it, the
/// relay becomes the manager itself, as every manager was before the relay;
/// an error means even that failed.
#[cfg(target_os = "macos")]
pub fn run_relay(executable: &Path, arguments: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let Some((label, manager_arguments)) = arguments.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "relay needs a label",
        ));
    };
    let mut program: Vec<&std::ffi::OsStr> = vec![executable.as_os_str()];
    program.extend(manager_arguments.iter().map(std::ffi::OsStr::new));
    // The relay's job carried exactly these from the Engine.
    let environment: Vec<(&str, String)> = MANAGER_ENVIRONMENT
        .iter()
        .filter_map(|&name| std::env::var(name).ok().map(|value| (name, value)))
        .collect();
    let directory = manager_arguments
        .iter()
        .position(|argument| argument == "--manager")
        .and_then(|index| manager_arguments.get(index + 1))
        .map_or_else(std::env::temp_dir, PathBuf::from);
    match diri_pty::detached::bootstrap_job(label, &program, &environment, &directory) {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("diri-holder: relay could not submit {label} ({error}); serving directly");
            Err(std::process::Command::new(executable)
                .args(manager_arguments)
                .exec())
        }
    }
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn relay_and_manager_jobs_get_distinct_sweepable_labels() {
        let (relay, manager) = relay_and_manager_labels("1a1162879da");
        assert_eq!(relay, "com.dirijor.diri.holders.1a1162879da");
        assert_eq!(manager, "com.dirijor.diri.holders.1a1162879db");
    }

    #[test]
    fn the_launchd_marker_names_one_build_of_the_holder() {
        let root = tempfile::tempdir().expect("tempdir");
        let executable = root.path().join("diri-holder");
        std::fs::write(&executable, b"build one").expect("executable");

        mark_launchd_unavailable(root.path(), &executable);
        let marker = root.path().join(LAUNCHD_UNAVAILABLE_MARKER);
        let recorded = std::fs::read_to_string(&marker).expect("marker");
        assert_eq!(Some(recorded), launchd_marker_key(&executable));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&marker)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // An update rewrites the executable: its key no longer matches.
        std::fs::write(&executable, b"build two, longer").expect("update");
        assert_ne!(
            std::fs::read_to_string(&marker).ok(),
            launchd_marker_key(&executable)
        );
    }
}
