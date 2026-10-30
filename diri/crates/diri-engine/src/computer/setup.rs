//! Setting computer use up from Diri, so nobody needs a terminal or a second
//! app: download the signed Cua driver into Diri's own data, run it as part of
//! Diri, and read back the two grants.
//!
//! The driver runs in Cua's embedded mode, spawned directly by the Engine,
//! which the Diri app spawned. macOS attributes Accessibility and Screen
//! Recording along that launch chain, so the grants (and the row in System
//! Settings) are Diri's own.
//!
//! Agent sessions share them. A local Agent's shell is started by a launchd
//! job whose program is the Diri app executable (`diri --exec-agent`), and
//! `tccd` attributes by that responsible path: its log charges `claude`,
//! `/bin/sh`, `python3` and the rest of an Agent's tree to `com.dirijor.diri`.
//! So granting Diri Accessibility or Screen Recording grants every local
//! Agent the same, and anything an Agent runs can use them. (Only when
//! detached launch is unavailable does an Agent run under the Holder
//! manager's own job, whose `diri-holder` program is a separate identity.)
//! `responsibility_get_pid_responsible_for_pid` returns the process itself
//! for an Agent and does not show this; read `tccd`'s log, or preflight from
//! inside the Agent's chain, to see what TCC decides.
//!
//! The release is pinned: its archive must match a SHA-256 baked in here and
//! the binary must carry the pinned Developer ID signature before it is used.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{ComputerUse, Shared};

/// The release Diri was tested with, and downloads: Diri's own build of Cua
/// Driver, upstream main after the cua-driver-rs-v0.38.0 release plus the
/// macOS 27 background-click fix (trycua/cua#5425), background element and
/// keyboard actions on different apps in parallel, and a proxy that answers
/// tool calls as they finish. Source: github.com/cristicretu/cua, tag
/// cua-driver-rs-v0.38.1-diri.
pub const TESTED_VERSION: &str = "0.38.1";
const ARCHIVE_URL: &str = "https://github.com/cristicretu/cua/releases/download/\
     cua-driver-rs-v0.38.1-diri/cua-driver-rs-0.38.1-diri-darwin-universal-binary.tar.gz";
/// From the release's SHA256SUMS.
const ARCHIVE_SHA256: &str = "3204e5535e358b2b084a35566abfa191dc78ba0a9529f89b13c434f2583ca7a8";
/// The Developer ID team that signs Diri's driver build. A binary signed by
/// anyone else is not used.
const DRIVER_TEAM_ID: &str = "A56RVNJ69X";
const DOWNLOAD_TIMEOUT_SECS: &str = "300";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(8);
/// The tail of the setup's output kept for "Show log".
const MAX_LOG_BYTES: usize = 32 * 1024;

/// Where the driver comes from. Tests point it at a local archive.
#[derive(Clone, Debug)]
pub struct Download {
    pub url: String,
    pub sha256: String,
    /// The Developer ID team the binary must be signed by; `None` skips the
    /// check (tests' unsigned fakes).
    pub team: Option<String>,
}

impl Default for Download {
    fn default() -> Self {
        Self {
            url: ARCHIVE_URL.to_owned(),
            sha256: ARCHIVE_SHA256.to_owned(),
            team: Some(DRIVER_TEAM_ID.to_owned()),
        }
    }
}

#[derive(Default)]
pub(super) struct SetupState {
    installing: bool,
    error: Option<String>,
    log: Option<String>,
}

impl ComputerUse {
    /// What Settings shows. `probe` starts the daemon when it is not
    /// running, so the grants can be read.
    pub fn status(&self, probe: bool) -> diri_proto::ComputerStatus {
        let mut status = diri_proto::ComputerStatus {
            supported: cfg!(target_os = "macos") || self.shared.binary.is_some(),
            tested_version: TESTED_VERSION.to_owned(),
            app_path: host_app().map(|path| path.to_string_lossy().into_owned()),
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
        if !self.daemon_up() {
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

    /// Starts the download on its own thread; a second click while one runs
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
                let mut log = String::new();
                let outcome = computer.download(&mut log);
                if outcome.is_ok() {
                    // A daemon from an older driver keeps running the old
                    // binary; the next call starts the new one.
                    computer.stop_daemon();
                }
                let mut setup = computer.setup_state();
                setup.installing = false;
                setup.log = Some(tail(&log, MAX_LOG_BYTES).to_owned());
                setup.error = outcome.err();
            });
        if spawned.is_err() {
            let mut setup = self.setup_state();
            setup.installing = false;
            setup.error = Some("Diri could not start the download.".into());
        }
    }

    /// Download, verify, unpack, sign-check, then move into place, so a
    /// half-finished setup never leaves a driver Diri would run.
    fn download(&self, log: &mut String) -> Result<(), String> {
        let root = &self.shared.root;
        let download = &self.shared.download;
        let staging = root.join(format!(".download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        create_private_dir(root)
            .and_then(|()| create_private_dir(&staging))
            .map_err(|error| format!("Diri couldn't write to {}: {error}", root.display()))?;
        let result = (|| {
            let archive = staging.join("driver.tar.gz");
            log.push_str(&format!("==> downloading {}\n", download.url));
            let output = quiet(Command::new("/usr/bin/curl").args([
                "-fsSL",
                "--retry",
                "2",
                "--max-time",
                DOWNLOAD_TIMEOUT_SECS,
                "-o",
            ]))
            .arg(&archive)
            .arg(&download.url)
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("Diri could not run curl: {error}"))?;
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                return Err(
                    "Couldn't download the driver. Check the connection and try again.".into(),
                );
            }
            let digest = sha256_file(&archive)
                .map_err(|error| format!("Diri couldn't read the download: {error}"))?;
            log.push_str(&format!("==> sha256 {digest}\n"));
            if !digest.eq_ignore_ascii_case(&download.sha256) {
                return Err("The download didn't match the expected release, so Diri \
                            didn't use it. Try again."
                    .into());
            }
            let output = quiet(Command::new("/usr/bin/tar").arg("-xzf"))
                .arg(&archive)
                .arg("-C")
                .arg(&staging)
                .arg("cua-driver")
                .stderr(Stdio::piped())
                .output()
                .map_err(|error| format!("Diri could not run tar: {error}"))?;
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            let binary = staging.join("cua-driver");
            if !output.status.success() || !binary.is_file() {
                return Err("The download had no driver in it. Try again.".into());
            }
            if let Some(team) = &download.team {
                verify_signature(&binary, team)?;
                log.push_str(&format!("==> signed by {team}\n"));
            }
            set_mode(&binary, 0o700)
                .map_err(|error| format!("Diri couldn't prepare the driver: {error}"))?;
            match driver_version(&binary) {
                Some(version) if !older_than(&version, TESTED_VERSION) => {
                    log.push_str(&format!("==> cua-driver {version}\n"));
                }
                Some(version) => {
                    return Err(format!(
                        "The download is Cua Driver {version}; Diri needs {TESTED_VERSION}."
                    ));
                }
                None => return Err("The downloaded driver doesn't run on this Mac.".into()),
            }
            let target = root.join(TESTED_VERSION);
            let _ = std::fs::remove_dir_all(&target);
            create_private_dir(&target)
                .map_err(|error| format!("Diri couldn't write to {}: {error}", root.display()))?;
            std::fs::rename(&binary, target.join("cua-driver"))
                .map_err(|error| format!("Diri couldn't install the driver: {error}"))?;
            log.push_str(&format!("==> installed {}\n", target.display()));
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&staging);
        if result.is_ok() {
            remove_other_versions(root);
        }
        result
    }

    /// Stops the daemon and starts a fresh one: macOS answers a process's
    /// permission checks from what was true when it launched.
    fn restart_daemon(&self) {
        self.stop_daemon();
        self.start_daemon();
    }

    /// Ends the pipe and the daemon Diri runs (never an external one).
    pub(super) fn stop_daemon(&self) {
        self.shutdown();
        if socket_override().is_some() || self.overridden() {
            return;
        }
        let daemon = self.lock().daemon.take();
        match daemon {
            Some(mut child) => {
                terminate_child(&mut child);
            }
            // An Engine restart left one running on Diri's socket.
            None if daemon_listening(&self.shared.socket) => {
                if let Some(binary) = self.binary() {
                    let _ = quiet(Command::new(binary).arg("stop"))
                        .arg("--socket")
                        .arg(&self.shared.socket)
                        .env("CUA_DRIVER_EMBEDDED", "1")
                        .status();
                }
            }
            None => {}
        }
    }

    /// Starts Diri's daemon if none answers on Diri's socket.
    pub(super) fn start_daemon(&self) {
        if self.overridden() || socket_override().is_some() || self.daemon_up() {
            return;
        }
        let Some(binary) = self.binary() else {
            return;
        };
        let mut inner = self.lock();
        if let Some(child) = inner.daemon.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            // Still starting: wait for it below, off the lock.
        } else {
            let _ = std::fs::remove_file(&self.shared.socket);
            inner.daemon = spawn_daemon(&binary, &self.shared.socket);
        }
        drop(inner);
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        while !self.daemon_up() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// (Accessibility, Screen Recording) as the daemon sees them.
    pub(super) fn grants(&self, binary: &Path) -> Option<(bool, bool)> {
        let output = quiet(Command::new(binary).arg("call"))
            .args(["check_permissions", "{}"])
            .args(self.socket_args())
            .stdout(Stdio::piped())
            .output()
            .ok()?;
        parse_grants(&output.stdout)
    }

    pub(super) fn daemon_up(&self) -> bool {
        self.overridden() || daemon_listening(&self.socket())
    }

    /// Tests run against a fake driver with no daemon behind it.
    pub(super) fn overridden(&self) -> bool {
        self.shared.binary.is_some()
    }

    /// The daemon socket: Diri's own, or `DIRI_CUA_DRIVER_SOCKET`.
    pub(super) fn socket(&self) -> PathBuf {
        socket_override().unwrap_or_else(|| self.shared.socket.clone())
    }

    pub(super) fn socket_args(&self) -> Vec<std::ffi::OsString> {
        if self.overridden() {
            return Vec::new();
        }
        vec!["--socket".into(), self.socket().into_os_string()]
    }

    fn setup_state(&self) -> std::sync::MutexGuard<'_, SetupState> {
        self.shared
            .setup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Shared {
    /// The driver Diri runs: a test override, `DIRI_CUA_DRIVER`, or the
    /// newest one setup downloaded.
    pub(super) fn locate(&self) -> Option<PathBuf> {
        if let Some(path) = &self.binary {
            return super::is_executable(path).then(|| path.clone());
        }
        if let Some(configured) = std::env::var_os("DIRI_CUA_DRIVER") {
            let path = PathBuf::from(configured);
            return super::is_executable(&path).then_some(path);
        }
        installed(&self.root)
    }
}

/// The newest `<root>/<version>/cua-driver`.
pub(super) fn installed(root: &Path) -> Option<PathBuf> {
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let version = entry.file_name().to_str()?.to_owned();
            if !version.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let binary = entry.path().join("cua-driver");
            super::is_executable(&binary).then_some((version, binary))
        })
        .collect();
    found.sort_by(|(a, _), (b, _)| {
        if older_than(a, b) {
            std::cmp::Ordering::Less
        } else if older_than(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    found.pop().map(|(_, binary)| binary)
}

fn remove_other_versions(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name != TESTED_VERSION && name.starts_with(|c: char| c.is_ascii_digit()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `serve --embedded`, as a direct child: the daemon stays in Diri's launch
/// chain, so macOS answers its permission checks with Diri's grants and
/// never prompts on its behalf.
fn spawn_daemon(binary: &Path, socket: &Path) -> Option<std::process::Child> {
    if let Some(dir) = socket.parent() {
        let _ = create_private_dir(dir);
    }
    quiet(Command::new(binary).args(["serve", "--embedded", "--socket"]))
        .arg(socket)
        .spawn()
        .ok()
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SIGTERM first: the daemon removes its socket and cursor overlay.
        if let Ok(pid) = i32::try_from(child.id()) {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Fails unless the binary is validly signed by `team`.
fn verify_signature(binary: &Path, team: &str) -> Result<(), String> {
    let valid = quiet(Command::new("/usr/bin/codesign").args(["--verify", "--strict"]))
        .arg(binary)
        .status()
        .is_ok_and(|status| status.success());
    let details = quiet(Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]))
        .arg(binary)
        .stderr(Stdio::piped())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
        .unwrap_or_default();
    let signed_by = details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="));
    if valid && signed_by == Some(team) {
        Ok(())
    } else {
        Err(
            "The downloaded driver isn't signed by Diri's release key, so Diri didn't use it."
                .into(),
        )
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
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

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    diri_platform::private::create_dir_all(dir)?;
    set_mode(dir, 0o700)
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// `cua-driver 0.38.1` → `0.38.1`.
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

/// The app macOS charges the driver's permissions to: the bundle this Engine
/// ships in (`Diri.app/Contents/Resources/bin/dirijord-rs`). `None` for a
/// loose build, where the app falls back to its own bundle.
pub(super) fn host_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    app_containing(&exe)
}

pub(super) fn app_containing(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .skip(1)
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
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
/// socket (used to exercise an ungranted driver without touching Diri's).
/// Diri never starts or stops that daemon.
pub(super) fn socket_override() -> Option<PathBuf> {
    std::env::var_os("DIRI_CUA_DRIVER_SOCKET").map(PathBuf::from)
}

pub(super) fn daemon_listening(socket: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(socket).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
        true
    }
}

/// Every driver process: no stdin, no output, embedded mode, no telemetry,
/// and none of the Agent's identity (the driver would otherwise hand it to
/// every app it launches).
pub(super) fn quiet(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("CUA_DRIVER_EMBEDDED", "1")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "0")
        .env("CUA_DRIVER_HOST_BUNDLE_ID", "com.dirijor.diri")
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
