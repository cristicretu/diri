//! Setting Cua Driver up from Diri, so nobody needs a terminal: install the
//! tested release in the background, start its daemon without the driver's
//! own permission prompt (Diri guides the person instead), and read back the
//! two grants.
//!
//! The installer is Cua's own (`cua.ai/driver/install.sh`): it verifies the
//! release checksum and code signature, keeps existing grants across a
//! compatible update, and rolls back a failed swap. Diri pins the version,
//! leaves shell profiles alone, keeps Cua's telemetry off, and checks the
//! installed app's signing team afterwards.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{ComputerUse, Shared, locate_driver};

/// The release Diri was tested with: installs pin it, and older ones are
/// offered an update.
pub const TESTED_VERSION: &str = "0.36.0";
const INSTALLER_URL: &str = "https://cua.ai/driver/install.sh";
/// Cua's Developer ID team. An installed app signed by anyone else fails.
const CUA_TEAM_ID: &str = "YCK386LBJ7";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const DOWNLOAD_TIMEOUT_SECS: &str = "60";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(8);
/// The tail of the installer's output kept for "Show log".
const MAX_LOG_BYTES: usize = 32 * 1024;
/// Commands the installer needs, whatever PATH launchd gave the Engine.
const INSTALL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Default)]
pub(super) struct SetupState {
    installing: bool,
    error: Option<String>,
    log: Option<String>,
}

impl ComputerUse {
    /// What Settings shows. `probe` starts the daemon (without its own
    /// prompt) when it is not running, so the grants can be read.
    pub fn status(&self, probe: bool) -> diri_proto::ComputerStatus {
        let mut status = diri_proto::ComputerStatus {
            supported: cfg!(target_os = "macos") || self.shared.binary.is_some(),
            tested_version: TESTED_VERSION.to_owned(),
            ..Default::default()
        };
        {
            let setup = self.setup_state();
            status.installing = setup.installing;
            status.install_error = setup.error.clone();
            status.install_log = setup.log.clone();
        }
        if status.installing {
            return status;
        }
        let Some(binary) = self.binary() else {
            return status;
        };
        status.version = driver_version(&binary);
        status.outdated = status
            .version
            .as_deref()
            .is_none_or(|version| older_than(version, TESTED_VERSION));
        status.app_path = app_bundle(&binary).map(|path| path.to_string_lossy().into_owned());
        if !self.overridden() && !self.daemon_up() {
            if !probe {
                return status;
            }
            self.start_daemon();
        }
        let grants = self.grants(&binary);
        if let Some((accessibility, screen_recording)) = grants {
            status.accessibility = Some(accessibility);
            status.screen_recording = Some(screen_recording);
        }
        let mut inner = self.lock();
        inner.granted_at = matches!(grants, Some((true, true))).then(Instant::now);
        inner.last_activity = Some(Instant::now());
        self.ensure_sweeper(&mut inner);
        status
    }

    pub fn setup(
        &self,
        action: diri_proto::ComputerSetupAction,
    ) -> Result<diri_proto::ComputerStatus, super::ComputerError> {
        match action {
            diri_proto::ComputerSetupAction::Install => self.install(),
            diri_proto::ComputerSetupAction::Restart => self.restart_daemon(),
        }
        Ok(self.status(false))
    }

    /// Starts the installer on its own thread; a second click while one runs
    /// does nothing.
    fn install(&self) {
        {
            let mut setup = self.setup_state();
            if setup.installing {
                return;
            }
            *setup = SetupState {
                installing: true,
                ..SetupState::default()
            };
        }
        let shared = std::sync::Arc::downgrade(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("diri-computer-install".into())
            .spawn(move || {
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                let computer = ComputerUse { shared };
                let (outcome, log) = computer.run_installer();
                let mut setup = computer.setup_state();
                setup.installing = false;
                setup.log = Some(log);
                setup.error = outcome.err();
            });
        if spawned.is_err() {
            let mut setup = self.setup_state();
            setup.installing = false;
            setup.error = Some("Diri could not start the installer.".into());
        }
    }

    /// Download, run, verify. Returns a person-facing error and the log.
    fn run_installer(&self) -> (Result<(), String>, String) {
        let mut log = String::new();
        let script = match self.installer_script(&mut log) {
            Ok(script) => script,
            Err(error) => return (Err(error), log),
        };
        let outcome = run_script(&script, &mut log);
        // The installer stops every running daemon before it swaps the app;
        // the pipe to it is gone either way.
        self.shutdown();
        if let Err(error) = outcome {
            return (Err(error), log);
        }
        let Some(binary) = self.binary() else {
            return (
                Err("The installer finished, but Cua Driver isn't where Diri looks for it.".into()),
                log,
            );
        };
        if !self.overridden() {
            if let Some(app) = app_bundle(&binary)
                && let Err(error) = verify_signature(&app)
            {
                return (Err(error), log);
            }
            // Persisted, so the driver stays quiet however it is started.
            let _ = quiet(Command::new(&binary).args(["telemetry", "disable"])).status();
        }
        match driver_version(&binary) {
            Some(version) if !older_than(&version, TESTED_VERSION) => (Ok(()), log),
            Some(version) => (
                Err(format!(
                    "The installer left Cua Driver {version}; Diri needs {TESTED_VERSION} or newer."
                )),
                log,
            ),
            None => (
                Err("The installed Cua Driver doesn't run. Try again.".into()),
                log,
            ),
        }
    }

    fn installer_script(&self, log: &mut String) -> Result<Vec<u8>, String> {
        if let Some(path) = &self.shared.installer {
            return std::fs::read(path).map_err(|error| format!("installer: {error}"));
        }
        log.push_str(&format!("==> downloading {INSTALLER_URL}\n"));
        let output = quiet(Command::new("/usr/bin/curl").args([
            "-fsSL",
            "--retry",
            "2",
            "--max-time",
            DOWNLOAD_TIMEOUT_SECS,
            INSTALLER_URL,
        ]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Diri could not run curl: {error}"))?;
        if !output.status.success() || output.stdout.is_empty() {
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            return Err("Couldn't reach cua.ai. Check the connection and try again.".into());
        }
        Ok(output.stdout)
    }

    /// Stops the daemon and starts a fresh one: Screen Recording applies to
    /// a process only from its launch.
    fn restart_daemon(&self) {
        let Some(binary) = self.binary() else {
            return;
        };
        self.shutdown();
        if self.overridden() {
            return;
        }
        let _ = quiet(Command::new(&binary).args(socket_args()).arg("stop")).status();
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        while self.daemon_up() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut inner = self.lock();
        inner.started_daemon = true;
        drop(inner);
        self.launch_daemon(&binary);
    }

    /// Starts the daemon if none answers, remembering that Diri did so it
    /// is stopped again when idle.
    pub(super) fn start_daemon(&self) {
        if self.overridden() || self.daemon_up() {
            return;
        }
        let Some(binary) = self.binary() else {
            return;
        };
        self.lock().started_daemon = true;
        self.launch_daemon(&binary);
    }

    /// Through LaunchServices, so macOS attributes the grants to
    /// CuaDriver.app, and without the driver's permission gate, which would
    /// open System Settings on its own while an Agent waits.
    fn launch_daemon(&self, binary: &Path) {
        if socket_override().is_some() {
            return;
        }
        let Some(app) = app_bundle(binary) else {
            return;
        };
        let launched = quiet(Command::new("/usr/bin/open").args(["-n", "-g", "-a"]))
            .arg(&app)
            .args(["--args", "serve", "--no-permissions-gate"])
            .status()
            .is_ok_and(|status| status.success());
        if !launched {
            return;
        }
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        while !self.daemon_up() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// (Accessibility, Screen Recording) as the daemon sees them.
    pub(super) fn grants(&self, binary: &Path) -> Option<(bool, bool)> {
        let output = quiet(Command::new(binary).arg("call"))
            .args(["check_permissions", "{}"])
            .args(socket_args())
            .stdout(Stdio::piped())
            .output()
            .ok()?;
        parse_grants(&output.stdout)
    }

    pub(super) fn daemon_up(&self) -> bool {
        self.overridden() || super::daemon_listening()
    }

    /// Tests run against a fake driver with no daemon behind it.
    pub(super) fn overridden(&self) -> bool {
        self.shared.binary.is_some()
    }

    fn setup_state(&self) -> std::sync::MutexGuard<'_, SetupState> {
        self.shared
            .setup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Shared {
    pub(super) fn locate(&self) -> Option<PathBuf> {
        match &self.binary {
            Some(path) => super::is_executable(path).then(|| path.clone()),
            None => locate_driver(),
        }
    }
}

/// Runs the installer script under bash with Diri's pins, collecting its
/// output (stdout and stderr, interleaved) into `log`.
fn run_script(script: &[u8], log: &mut String) -> Result<(), String> {
    let (mut reader, writer) =
        std::io::pipe().map_err(|error| format!("Diri could not start the installer: {error}"))?;
    let stderr = writer
        .try_clone()
        .map_err(|error| format!("Diri could not start the installer: {error}"))?;
    let mut child = Command::new("/bin/bash")
        .args(["-s", "--", "--no-modify-path"])
        .env("PATH", INSTALL_PATH)
        .env("CUA_DRIVER_RS_VERSION", TESTED_VERSION)
        .env("CUA_DRIVER_RS_NO_MODIFY_PATH", "1")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "0")
        .env_remove("DIRIJOR_SOCKET")
        .env_remove("DIRIJOR_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(writer)
        .stderr(stderr)
        .spawn()
        .map_err(|error| format!("Diri could not start the installer: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = stdin.write_all(script);
    }
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + INSTALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // `child` (and with it the last copies of the pipe's write end) is done.
    drop(child);
    let bytes = output.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    log.push_str(tail(&text, MAX_LOG_BYTES));
    match status {
        Some(status) if status.success() => Ok(()),
        Some(_) => Err(install_failure(&text)),
        None => Err("The install took more than 10 minutes and was stopped. Try again.".into()),
    }
}

/// The installer's own words, turned into what the person can do about it.
pub(super) fn install_failure(output: &str) -> String {
    if output.contains("/Applications is not writable") {
        return "Diri couldn't write to /Applications. Use an administrator account, or ask one \
                to set up computer use."
            .into();
    }
    if output.contains("Could not resolve host")
        || output.contains("Failed to connect")
        || output.contains("timed out")
    {
        return "Couldn't reach the download. Check the connection and try again.".into();
    }
    let last = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("error:"))
        .map(|line| line.trim_start_matches("error:").trim().to_owned());
    match last {
        Some(line) if !line.is_empty() => format!("The installer stopped: {line}"),
        _ => "The installer stopped. Show the log for details.".into(),
    }
}

/// Fails unless the app is validly signed by Cua.
fn verify_signature(app: &Path) -> Result<(), String> {
    let valid = quiet(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]))
        .arg(app)
        .status()
        .is_ok_and(|status| status.success());
    let details = quiet(Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]))
        .arg(app)
        .stderr(Stdio::piped())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
        .unwrap_or_default();
    let team = details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="));
    if valid && team == Some(CUA_TEAM_ID) {
        Ok(())
    } else {
        Err(format!(
            "{} isn't signed by Cua, so Diri won't use it.",
            app.display()
        ))
    }
}

/// `cua-driver 0.36.0` → `0.36.0`.
fn driver_version(binary: &Path) -> Option<String> {
    let output = quiet(Command::new(binary).arg("--version"))
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().nth(1)?.trim_start_matches('v');
    (output.status.success() && version.chars().next()?.is_ascii_digit())
        .then(|| version.to_owned())
}

/// Dotted numeric comparison; pre-release suffixes are ignored.
pub(super) fn older_than(version: &str, minimum: &str) -> bool {
    let parts = |text: &str| -> Vec<u64> {
        text.split(['.', '-', '+'])
            .map_while(|part| part.parse().ok())
            .collect()
    };
    parts(version) < parts(minimum)
}

/// The `.app` a driver binary lives in, through the `~/.local/bin` symlink.
pub(super) fn app_bundle(binary: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    let macos = resolved.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_path_buf())
}

pub(super) fn parse_grants(stdout: &[u8]) -> Option<(bool, bool)> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let value = value.get("structuredContent").unwrap_or(&value);
    Some((
        value.get("accessibility")?.as_bool()?,
        value.get("screen_recording")?.as_bool()?,
    ))
}

/// `DIRI_CUA_DRIVER_SOCKET`: a daemon somebody else started on its own
/// socket (used to exercise an ungranted driver without touching the real
/// one). Diri never starts or stops that daemon.
pub(super) fn socket_override() -> Option<PathBuf> {
    std::env::var_os("DIRI_CUA_DRIVER_SOCKET").map(PathBuf::from)
}

pub(super) fn socket_args() -> Vec<std::ffi::OsString> {
    socket_override()
        .map(|socket| vec!["--socket".into(), socket.into_os_string()])
        .unwrap_or_default()
}

fn quiet(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DIRIJOR_SOCKET")
        .env_remove("DIRIJOR_SESSION_ID")
}

fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
