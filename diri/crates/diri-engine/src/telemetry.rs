//! Engine and Holder flight-recorder glue: static names for the enums events
//! carry, the build facts the uploader sends, and the few cross-cutting
//! helpers (RPC timing, terminal modes) several modules share. The event
//! catalog lives in `diri/TELEMETRY.md`.

pub mod crash_reports;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use diri_proto::{ExitReason, HibernationReason, SessionStatus};
use diri_telemetry::Value;

/// An agent that exits (or drops to its login shell) this soon after being
/// launched or resumed did not really start: a failed resume, a missing
/// binary, a crash at boot. That is recorded as an incident.
pub const EARLY_EXIT: Duration = Duration::from_secs(10);

/// RPCs slower than this are recorded individually as `rpc.slow`.
pub const SLOW_RPC: Duration = Duration::from_millis(250);

static HOLDER_TELEMETRY_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set once by the Engine after it starts recording: Holder managers it
/// launches are told to record into the same state directory. Tests and
/// embedders that never initialize the recorder launch quiet Holders.
pub fn set_holder_state_dir(state_dir: &Path) {
    let _ = HOLDER_TELEMETRY_DIR.set(state_dir.to_path_buf());
}

/// The state directory a Holder manager should record under, if any.
pub fn holder_state_dir() -> Option<&'static Path> {
    HOLDER_TELEMETRY_DIR.get().map(PathBuf::as_path)
}

/// The Holder argument that carries [`holder_state_dir`].
pub const HOLDER_TELEMETRY_FLAG: &str = "--telemetry-state-dir";

#[must_use]
pub fn status_name(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::NeedsInput(_) => "needs_input",
        SessionStatus::Exited(_) => "exited",
        SessionStatus::Unknown => "unknown",
    }
}

#[must_use]
pub fn exit_reason_name(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::Exited => "exited",
        ExitReason::Signaled => "signaled",
        ExitReason::DaemonRestart => "daemon_restart",
        ExitReason::External => "external",
        ExitReason::Archived => "archived",
        ExitReason::Unknown => "unknown",
    }
}

#[must_use]
pub fn hibernation_reason_name(reason: HibernationReason) -> &'static str {
    match reason {
        HibernationReason::Idle => "idle",
        HibernationReason::MemoryPressure => "memory_pressure",
        HibernationReason::Manual => "manual",
        HibernationReason::Unknown => "unknown",
    }
}

#[must_use]
pub fn remote_state_name(state: diri_proto::RemoteConnectionState) -> &'static str {
    use diri_proto::RemoteConnectionState as State;
    match state {
        State::Connecting => "connecting",
        State::Connected => "connected",
        State::Reconnecting => "reconnecting",
        State::Failed => "failed",
        State::Exited => "exited",
        State::Unknown => "unknown",
    }
}

#[must_use]
pub fn persistence_name(capability: diri_proto::remote_pty::PersistenceCapability) -> &'static str {
    use diri_proto::remote_pty::PersistenceCapability as Capability;
    match capability {
        Capability::NativeDetach => "native-detach",
        Capability::UserSupervisor => "user-supervisor",
        Capability::NonPersistent => "non-persistent",
    }
}

#[must_use]
pub fn mouse_tracking_name(modes: diri_proto::terminal::MouseModes) -> &'static str {
    use diri_proto::terminal::MouseTrackingMode as Tracking;
    match modes.tracking {
        Tracking::Off => "off",
        Tracking::ButtonEvents => "1000",
        Tracking::ButtonMotion => "1002",
        Tracking::AnyMotion => "1003",
        Tracking::Unknown => "unknown",
    }
}

/// Terminal modes a program turns on for itself and must turn off on exit.
/// Left on after the program that set them has gone, the next program on the
/// PTY (usually the user's shell) receives mouse reports or pastes it never
/// asked for — `^[[<35;14;25M` typed into zsh.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeftModes {
    pub mouse: Option<diri_proto::terminal::MouseModes>,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub app_cursor: bool,
    /// Kitty keyboard enhancements (CSI > u) still pushed.
    pub keyboard: bool,
    /// Focus in/out reporting (DEC 1004).
    pub focus: bool,
}

impl LeftModes {
    #[must_use]
    pub fn of(screen: &diri_terminal_state::HeadlessScreen) -> Self {
        let mouse = screen.mouse_modes();
        let keyboard = screen.keyboard_state();
        Self {
            mouse: mouse.is_reporting().then_some(mouse),
            alt_screen: screen.is_alt_screen(),
            bracketed_paste: screen.bracketed_paste(),
            app_cursor: keyboard.application_cursor_keys,
            keyboard: keyboard
                .enhancements
                .is_some_and(|enhancements| enhancements.bits() != 0),
            focus: screen.focus_reporting(),
        }
    }

    #[must_use]
    pub fn any(&self) -> bool {
        self.mouse.is_some()
            || self.alt_screen
            || self.bracketed_paste
            || self.app_cursor
            || self.keyboard
            || self.focus
    }

    /// The modes whose leftovers break the shell that comes back: mouse and
    /// focus reports and kitty key encodings arrive as keystrokes, and the
    /// alternate screen hides its scrollback. Bracketed paste and application
    /// cursor keys are not among them: zsh's line editor turns both on at
    /// every prompt itself, so a shell prompt always shows them.
    #[must_use]
    pub fn corrupts_input(&self) -> bool {
        self.mouse.is_some() || self.alt_screen || self.keyboard || self.focus
    }

    #[must_use]
    pub fn fields(&self) -> Value {
        Value::Obj(vec![
            (
                "mouse",
                Value::from(self.mouse.map(mouse_tracking_name).unwrap_or("off")),
            ),
            (
                "sgr",
                Value::from(self.mouse.is_some_and(|mouse| {
                    mouse.encoding == diri_proto::terminal::MouseEncoding::Sgr
                })),
            ),
            ("alt_screen", Value::from(self.alt_screen)),
            ("bracketed_paste", Value::from(self.bracketed_paste)),
            ("app_cursor", Value::from(self.app_cursor)),
            ("keyboard", Value::from(self.keyboard)),
            ("focus", Value::from(self.focus)),
        ])
    }
}

/// Only the class of a Holder failure, never its subprocess/protocol text.
pub fn holder_error_kind(error: &crate::holder::HolderError) -> &'static str {
    use crate::holder::HolderError;
    match error {
        HolderError::InvalidRequest(_) => "invalid_request",
        HolderError::Transport(_) => "transport",
        HolderError::Rejected(_) => "rejected",
        HolderError::Launch(_) => "launch",
    }
}

/// Records one handled control request: always a count and a timing, an
/// `rpc.slow` warning past [`SLOW_RPC`], and `rpc.error` for an error reply.
pub fn record_rpc(
    method: &str,
    elapsed: Duration,
    error: Option<&diri_proto::control::ControlError>,
    session: Option<&str>,
) {
    if !diri_telemetry::is_enabled() {
        return;
    }
    diri_telemetry::count("rpc.calls", 1);
    diri_telemetry::observe("rpc", elapsed);
    // The Agent blocks on this reply once or twice per tool call, so its whole
    // distribution matters, not only the `rpc.slow` tail.
    if method == diri_proto::Method::HOOK_REPORT {
        diri_telemetry::observe("rpc.hook_report", elapsed);
    }
    // Long polls and waits on the user are slow by design.
    let waits = matches!(
        method,
        diri_proto::Method::EVENTS_WAIT | diri_proto::Method::TASK_GET
    );
    if elapsed >= SLOW_RPC && !waits {
        diri_telemetry::warn_event!(
            "rpc.slow",
            method = diri_telemetry::id(method),
            ms = elapsed,
            ok = error.is_none(),
            session = session.map(diri_telemetry::id),
        );
    }
    if let Some(error) = error {
        diri_telemetry::count("rpc.errors", 1);
        // A folder the user deleted, or an ended session with no screen
        // left to read, is their state, not a fault: keep it visible without
        // counting it as an error.
        if matches!(
            error.code.as_str(),
            diri_proto::control::CWD_MISSING | diri_proto::control::TERMINAL_NOT_RETAINED
        ) {
            diri_telemetry::warn_event!(
                "rpc.error",
                method = diri_telemetry::id(method),
                code = diri_telemetry::id(&error.code),
                ms = elapsed,
                session = session.map(diri_telemetry::id),
            );
        } else {
            diri_telemetry::error_event!(
                "rpc.error",
                method = diri_telemetry::id(method),
                code = diri_telemetry::id(&error.code),
                ms = elapsed,
                session = session.map(diri_telemetry::id),
            );
        }
    }
}

/// Requests that change a session's or the fleet's life: their successes are
/// recorded individually (`rpc.op`) so a timeline shows who did what.
#[must_use]
pub fn is_lifecycle_method(method: &str) -> bool {
    use diri_proto::Method;
    matches!(
        method,
        Method::SESSION_SPAWN
            | Method::SESSION_SPAWN_TRACKED
            | Method::SESSION_KILL
            | Method::SESSION_REMOVE
            | Method::SESSION_ARCHIVE
            | Method::SESSION_UNARCHIVE
            | Method::SESSION_RESUME
            | Method::SESSION_FORK
            | Method::SESSION_RESUME_FROM_HISTORY
            | Method::SESSION_REOPEN_LAST
            | Method::SESSION_MIGRATE
            | Method::SESSION_CONTINUE_ACCOUNT
            | Method::SESSION_RECONNECT
            | Method::SESSION_HIBERNATE
            | Method::SESSION_WAKE
            | Method::SESSION_RESET_TERMINAL
            | Method::SESSION_REPARENT_WORKTREE
            | Method::SESSION_SEND_TEXT
            | Method::ACCOUNT_SWITCH_ALL
            | Method::ACCOUNT_CODEX_LOGIN
            | Method::ACCOUNT_CLAUDE_LOGIN
            | Method::ACCOUNT_CODEX_CAPTURE
            | Method::ACCOUNT_CLAUDE_CAPTURE
            | Method::ACCOUNT_ADOPT
            | Method::ACCOUNT_ADD
            | Method::ACCOUNT_PROFILES_SAVE
            | Method::ACCOUNT_PROFILES_REMOVE
            | Method::HOST_INITIALIZE
            | Method::WORKTREE_CREATE
            | Method::WORKTREE_REMOVE
            | Method::WORKTREE_CLEANUP
            | Method::PROJECT_ADD
            | Method::AGENT_CONFIGURE
            | Method::TASK_SUBMIT
            | Method::DAEMON_PREPARE_SHUTDOWN
            | Method::DAEMON_SHUTDOWN_IF_IDLE
            | Method::DAEMON_SHUTDOWN
    )
}

/// A new session record went live: what was launched, where, and how.
/// `concurrent` says another agent session was live at the time (see
/// [`counts_for_activation`]); it feeds the activation milestones.
pub fn record_session_spawn(
    record: &diri_proto::SessionRecord,
    mode: &'static str,
    elapsed: Duration,
    concurrent: bool,
) {
    record_activation(record, mode, concurrent);
    diri_telemetry::event!(
        "session.spawn",
        session = diri_telemetry::id(&record.id.0),
        agent = diri_telemetry::id(record.kind.id()),
        mode = mode,
        conv = record.agent_session_id.as_deref().map(diri_telemetry::id),
        host = record.host.as_deref().map(diri_telemetry::id),
        project = diri_telemetry::path_hash(&record.cwd),
        worktree = record.worktree_path.is_some(),
        parent = record
            .parent
            .as_ref()
            .map(|parent| diri_telemetry::id(&parent.0)),
        account = record.account_profile.is_some(),
        prompt = record
            .originating_prompt
            .as_ref()
            .is_some_and(|prompt| !prompt.is_empty()),
        ms = elapsed,
    );
}

/// Agent sessions count toward the activation funnel; terminals (`shell`,
/// `generic`) and notes do not.
#[must_use]
pub fn counts_for_activation(kind: &diri_proto::AgentKind) -> bool {
    !kind.is_terminal() && kind.id() != diri_proto::AgentKind::NOTE_ID
}

/// `activation.first_session`, then `activation.second_session`, and
/// `activation.first_helper` for the first session another agent started.
/// Each fires once per install (see `diri_telemetry::activation`).
fn record_activation(record: &diri_proto::SessionRecord, mode: &'static str, concurrent: bool) {
    use diri_telemetry::activation::{self, Milestone};
    if !counts_for_activation(&record.kind) {
        return;
    }
    let helper = record.parent.is_some();
    let agent = diri_telemetry::id(record.kind.id());
    activation::reach_next(
        &[Milestone::FirstSession, Milestone::SecondSession],
        |milestone| {
            let mut fields = vec![
                ("agent", Value::from(agent.clone())),
                ("mode", Value::from(mode)),
                ("helper", Value::from(helper)),
            ];
            if milestone == Milestone::SecondSession {
                fields.push(("concurrent", Value::from(concurrent)));
            }
            fields
        },
    );
    if helper {
        activation::reach(Milestone::FirstHelper, vec![("agent", Value::from(agent))]);
    }
}

/// What `session.resume` decided to launch for `record`, and which
/// conversation it points the agent at. `fresh_unwritten` is Claude's
/// never-written conversation, started afresh instead of `--resume`d into
/// "No conversation found".
pub fn record_resume(
    record: &diri_proto::SessionRecord,
    decision: &'static str,
    conversation: Option<&str>,
) {
    diri_telemetry::event!(
        "session.resume",
        session = diri_telemetry::id(&record.id.0),
        agent = diri_telemetry::id(record.kind.id()),
        decision = decision,
        conv = conversation.map(diri_telemetry::id),
        recorded_conv = record.agent_session_id.as_deref().map(diri_telemetry::id),
        transcript = record.transcript_path.is_some(),
        host = record.host.as_deref().map(diri_telemetry::id),
        status = status_name(&record.status),
        exit_reason = match &record.status {
            SessionStatus::Exited(info) if info.system_restart => Some("system_restart"),
            SessionStatus::Exited(info) => Some(exit_reason_name(info.reason)),
            _ => None,
        },
        archived = record.is_archived(),
    );
}

/// The `sessionId` a request names, for correlating RPC events with a
/// session's timeline.
#[must_use]
pub fn request_session(params: Option<&serde_json::Value>) -> Option<String> {
    let params = params?;
    ["sessionId", "sessionID", "session_id", "id"]
        .iter()
        .find_map(|key| params.get(*key)?.as_str())
        .map(str::to_owned)
}

/// Build facts for the uploader's batch header.
#[must_use]
pub fn upload_meta(exe_dir: &Path) -> diri_telemetry::upload::Meta {
    let bundle = bundle_versions(exe_dir);
    let released = bundle.is_some() || (cfg!(target_os = "linux") && linux_package(exe_dir));
    diri_telemetry::upload::Meta {
        app_version: bundle
            .as_ref()
            .map(|(short, _)| short.clone())
            .unwrap_or_else(|| env!("DIRI_APP_VERSION").to_owned()),
        build: build_id(),
        channel: if released && !cfg!(debug_assertions) {
            "stable"
        } else {
            "dev"
        }
        .to_owned(),
        os: std::env::consts::OS.to_owned(),
        os_version: os_version().unwrap_or_default(),
        arch: std::env::consts::ARCH.to_owned(),
    }
}

/// The source commit a release was built from when the build exported it,
/// otherwise the Engine's handshake identity (version + Agent catalog).
#[must_use]
pub fn build_id() -> String {
    option_env!("SOURCE_COMMIT")
        .or(option_env!("DIRI_SOURCE_COMMIT"))
        .filter(|commit| !commit.is_empty())
        .unwrap_or(crate::control::BUILD)
        .to_owned()
}

/// `CFBundleShortVersionString` / `CFBundleVersion` of the `.app` this Engine
/// ships in (`diri.app/Contents/{MacOS,Resources/bin}/dirijord-rs`).
fn bundle_versions(exe_dir: &Path) -> Option<(String, Option<String>)> {
    let contents = exe_dir
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "Contents"))?;
    let plist = plist::Value::from_file(contents.join("Info.plist")).ok()?;
    let dict = plist.as_dictionary()?;
    let short = dict
        .get("CFBundleShortVersionString")?
        .as_string()?
        .to_owned();
    let build = dict
        .get("CFBundleVersion")
        .and_then(plist::Value::as_string)
        .map(str::to_owned);
    Some((short, build))
}

/// A Linux AppImage or Debian package: `<prefix>/bin/dirijord-rs` beside
/// `<prefix>/lib/diri` (see `DirijorPaths::packaged_resources`). A cargo build
/// runs from `target/<profile>/` and has neither.
fn linux_package(exe_dir: &Path) -> bool {
    exe_dir.file_name().is_some_and(|name| name == "bin")
        && exe_dir
            .parent()
            .is_some_and(|prefix| prefix.join("lib/diri").is_dir())
}

/// The kind of systemd unit this process runs in, from `/proc/self/cgroup`.
/// Holders `setsid` but stay in the Engine's cgroup, so stopping that unit
/// (closing the terminal tab whose scope launched diri, an app scope torn
/// down at quit, systemd-oomd) kills every session at once without an exit
/// record. Only the unit's class is kept, never its name.
#[must_use]
pub fn cgroup_class(proc_self_cgroup: &str) -> &'static str {
    let Some(path) = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .or_else(|| {
            proc_self_cgroup
                .lines()
                .find_map(|line| line.split_once(":name=systemd:").map(|(_, path)| path))
        })
    else {
        return "unknown";
    };
    let unit = path.trim().rsplit('/').next().unwrap_or_default();
    if unit.is_empty() {
        return "root";
    }
    let terminal = [
        "vte-spawn-",
        "konsole",
        "alacritty",
        "kitty",
        "wezterm",
        "ghostty",
        "foot",
        "terminal",
        "tilix",
        "terminator",
        "code-",
        "cursor-",
    ];
    if let Some(name) = unit.strip_suffix(".scope") {
        let name = name.to_ascii_lowercase();
        return if terminal.iter().any(|needle| name.contains(needle)) {
            "terminal_scope"
        } else if name.starts_with("session-") {
            "session_scope"
        } else if name.starts_with("app-") {
            "app_scope"
        } else {
            "other_scope"
        };
    }
    if unit.ends_with(".service") {
        return if unit.starts_with("app-") {
            "app_service"
        } else {
            "service"
        };
    }
    "other"
}

#[cfg(target_os = "macos")]
fn os_version() -> Option<String> {
    let mut buffer = [0_u8; 64];
    let mut length = buffer.len();
    // SAFETY: sysctlbyname writes at most `length` bytes into the buffer and
    // updates `length`; the name is a NUL-terminated literal.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    let bytes = &buffer[..length.min(buffer.len())];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned)
}

#[cfg(not(target_os = "macos"))]
fn os_version() -> Option<String> {
    // os-release(5): /etc first, /usr/lib as the vendor fallback.
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|release| os_release_version(&release))
}

/// `"<ID> <VERSION_ID>"` from an os-release file, e.g. `fedora 44` or
/// `ubuntu 26.04`; just `arch` for rolling releases, which have no
/// VERSION_ID. A bare VERSION_ID (`44`, `4.0.1`) did not say which distro it
/// numbered. Both keys are machine-readable by spec; anything else in a value
/// is dropped so free text cannot ride along.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn os_release_version(release: &str) -> Option<String> {
    let value = |key: &str| {
        release.lines().find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix('=')?;
            let value: String = value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .take(24)
                .collect::<String>()
                .to_ascii_lowercase();
            (!value.is_empty()).then_some(value)
        })
    };
    match (value("ID"), value("VERSION_ID")) {
        (Some(id), Some(version)) => Some(format!("{id} {version}")),
        (Some(id), None) => Some(id),
        (None, version) => version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_names_the_distro_and_its_version() {
        let fedora = "NAME=\"Fedora Linux\"\nVERSION=\"44 (Workstation Edition)\"\nID=fedora\nVERSION_ID=44\nPRETTY_NAME=\"Fedora Linux 44\"\n";
        assert_eq!(os_release_version(fedora).as_deref(), Some("fedora 44"));
        let ubuntu = "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"26.04\"\n";
        assert_eq!(os_release_version(ubuntu).as_deref(), Some("ubuntu 26.04"));
        // Rolling releases have no VERSION_ID: the old parser sent "".
        let arch = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
        assert_eq!(os_release_version(arch).as_deref(), Some("arch"));
        let quoted = "ID='nixos'\nVERSION_ID='25.11 <me@host>'\n";
        assert_eq!(
            os_release_version(quoted).as_deref(),
            Some("nixos 25.11mehost")
        );
        assert_eq!(os_release_version("PRETTY_NAME=x\n"), None);
    }

    #[test]
    fn cgroup_units_are_reduced_to_their_class() {
        let v2 = |path: &str| format!("0::{path}\n");
        assert_eq!(
            cgroup_class(&v2(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-diri-4412.scope"
            )),
            "app_scope"
        );
        assert_eq!(
            cgroup_class(&v2(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-0f1e.scope"
            )),
            "terminal_scope"
        );
        assert_eq!(
            cgroup_class(&v2(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.kde.konsole-1234.scope"
            )),
            "terminal_scope"
        );
        assert_eq!(
            cgroup_class(&v2("/user.slice/user-1000.slice/session-3.scope")),
            "session_scope"
        );
        assert_eq!(
            cgroup_class(&v2(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/app-diri@a1b2.service"
            )),
            "app_service"
        );
        assert_eq!(cgroup_class(&v2("/")), "root");
        assert_eq!(
            cgroup_class("12:pids:/\n1:name=systemd:/user.slice/user-1000.slice/session-2.scope\n"),
            "session_scope"
        );
        assert_eq!(cgroup_class(""), "unknown");
    }

    #[test]
    fn linux_packages_are_told_apart_from_cargo_builds() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("usr");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        assert!(!linux_package(&prefix.join("bin")));
        std::fs::create_dir_all(prefix.join("lib/diri")).unwrap();
        assert!(linux_package(&prefix.join("bin")));
        let cargo = temp.path().join("target/release");
        std::fs::create_dir_all(&cargo).unwrap();
        assert!(!linux_package(&cargo));
    }

    #[test]
    fn the_reported_app_version_is_the_products_not_the_crates() {
        assert_eq!(
            env!("DIRI_APP_VERSION"),
            include_str!("../../diri-app/Cargo.toml")
                .lines()
                .find_map(|line| line.strip_prefix("version = "))
                .map(|value| value.trim_matches('"'))
                .unwrap()
        );
        assert_ne!(env!("DIRI_APP_VERSION"), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn bundle_versions_come_from_the_enclosing_app() {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp.path().join("diri.app/Contents");
        let bin = contents.join("Resources/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>0.9.0</string>
<key>CFBundleVersion</key><string>90</string>
</dict></plist>"#,
        )
        .unwrap();
        assert_eq!(
            bundle_versions(&bin),
            Some(("0.9.0".into(), Some("90".into())))
        );
        assert_eq!(bundle_versions(temp.path()), None);
    }

    #[test]
    fn left_modes_name_the_dec_modes() {
        let left = LeftModes {
            mouse: Some(diri_proto::terminal::MouseModes::new(
                diri_proto::terminal::MouseTrackingMode::ButtonMotion,
                diri_proto::terminal::MouseEncoding::Sgr,
            )),
            ..LeftModes::default()
        };
        assert!(left.any() && left.corrupts_input());
        let json = serde_json::to_value(left.fields()).unwrap();
        assert_eq!(json["mouse"], "1002");
        assert_eq!(json["sgr"], true);
        assert!(!LeftModes::default().any());
    }

    /// 0.8.10 reported `session.modes_left_on_exit` for shells whose only
    /// "leftover" was bracketed paste: zsh re-enables it (and application
    /// cursor keys) at its own prompt.
    #[test]
    fn a_shell_prompts_own_modes_are_not_leftovers() {
        let prompt = LeftModes {
            bracketed_paste: true,
            app_cursor: true,
            ..LeftModes::default()
        };
        assert!(prompt.any());
        assert!(!prompt.corrupts_input());
        let mut screen =
            diri_terminal_state::HeadlessScreen::new_with_keyboard_enhancements(80, 24);
        // What zsh's line editor prints before each prompt: bracketed paste
        // and keypad transmit (application cursor keys).
        screen.feed(b"\x1b[?2004h\x1b[?1h\x1b=% ");
        assert_eq!(LeftModes::of(&screen), prompt);
        screen.feed(b"\x1b[?1004h");
        assert!(LeftModes::of(&screen).corrupts_input());
        screen.feed(b"\x1b[?1004l\x1b[>1u");
        assert!(LeftModes::of(&screen).keyboard);
        for left in [
            LeftModes {
                alt_screen: true,
                ..prompt
            },
            LeftModes {
                keyboard: true,
                ..prompt
            },
            LeftModes {
                focus: true,
                ..prompt
            },
        ] {
            assert!(left.corrupts_input(), "{left:?}");
        }
    }

    #[test]
    fn request_session_reads_the_usual_keys() {
        let params = serde_json::json!({ "sessionId": "s_1" });
        assert_eq!(request_session(Some(&params)).as_deref(), Some("s_1"));
        assert_eq!(request_session(None), None);
    }
}
