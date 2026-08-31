//! Versioned wire protocol between the local Engine and a remote PTY Holder.
//!
//! Terminal frame kinds 1 through 10 retain their wire meanings; kind 11 adds
//! raw mouse input. They are not wrapped in another frame. Remote-only control
//! kinds start at 32. Small, infrequent control payloads use JSON; a full
//! terminal snapshot keeps the existing binary grid encoding so reconnect
//! does not serialize every cell as JSON.

use std::error::Error;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::frames::{Frame, FrameType, MAX_FRAME_BYTES};
use crate::grid::{GridCodecError, GridUpdate};
use crate::terminal::MouseModes;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 4;
pub const MOUSE_INPUT_PROTOCOL_MINOR: u16 = 4;
pub const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_ARGUMENTS: usize = 512;
pub const MAX_ENVIRONMENT_VARIABLES: usize = 4096;
pub const MAX_LAUNCH_BYTES: usize = 1024 * 1024;
pub const MAX_ENVIRONMENT_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_TERMINAL_COLS: u16 = 4096;
pub const MAX_TERMINAL_ROWS: u16 = 4096;
pub const MAX_TERMINAL_CELLS: usize = 1_000_000;
pub const MAX_DIRECTORY_ENTRIES: usize = 512;
pub const MAX_DIRECTORY_SCANNED_ENTRIES: usize = 16_384;
pub const MAX_DIRECTORY_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_EXECUTABLE_QUERIES: usize = 128;

const HEADER_BYTES: usize = 5;
const FULL_SNAPSHOT_FIXED_BYTES: usize = 9;
const KIND_HELLO: u8 = 32;
const KIND_HELLO_ACK: u8 = 33;
const KIND_FULL_SNAPSHOT: u8 = 34;
const KIND_PROCESS_EXIT: u8 = 35;
const KIND_SIGNAL: u8 = 36;
const KIND_ACQUIRE_CONTROL: u8 = 37;
const KIND_CONTROL_GRANTED: u8 = 38;
const KIND_CONTROL_REVOKED: u8 = 39;
const KIND_RELEASE_CONTROL: u8 = 40;
const KIND_ERROR: u8 = 41;
const KIND_GRID_DELTA: u8 = 42;
const KIND_SCROLLBACK_REQUEST: u8 = 43;
const KIND_SCROLLBACK_RESPONSE: u8 = 44;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteRole {
    Controller,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteCapability {
    FullSnapshot,
    IncrementalGrid,
    ProcessExit,
    Signal,
    ControllerLease,
    Scrollback,
    /// Helper CLI can launch, inspect, list, kill, and GC one-session Holders.
    SessionManagement,
    /// Helper CLI can capture the account login environment for a target cwd.
    EnvironmentCapture,
    /// Helper CLI can return one bounded directory level.
    DirectoryList,
    /// Helper can resolve a bounded batch of executable names against the
    /// account login PATH and validate user-selected executable paths.
    ExecutableDiscovery,
    /// Helper CLI can execute the detach/supervisor persistence probe.
    PersistenceProbe,
    /// Uploaded Helper can activate itself without replacing different bytes.
    AtomicActivation,
    AgentEvents,
    McpStdio,
    ResourceInspect,
    PortForward,
    RebootRecovery,
    Migration,
    /// A capability introduced by a newer protocol minor. It is ignored
    /// unless the local side explicitly requires it.
    #[serde(other)]
    Unknown,
}

impl RemoteCapability {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::FullSnapshot => "full-snapshot",
            Self::IncrementalGrid => "incremental-grid",
            Self::ProcessExit => "process-exit",
            Self::Signal => "signal",
            Self::ControllerLease => "controller-lease",
            Self::Scrollback => "scrollback",
            Self::SessionManagement => "session-management",
            Self::EnvironmentCapture => "environment-capture",
            Self::DirectoryList => "directory-list",
            Self::ExecutableDiscovery => "executable-discovery",
            Self::PersistenceProbe => "persistence-probe",
            Self::AtomicActivation => "atomic-activation",
            Self::AgentEvents => "agent-events",
            Self::McpStdio => "mcp-stdio",
            Self::ResourceInspect => "resource-inspect",
            Self::PortForward => "port-forward",
            Self::RebootRecovery => "reboot-recovery",
            Self::Migration => "migration",
            Self::Unknown => "unknown",
        }
    }
}

/// Terminal capabilities required by every phase-one Holder attach.
pub const PHASE_ONE_HOLDER_CAPABILITIES: &[RemoteCapability] = &[
    RemoteCapability::FullSnapshot,
    RemoteCapability::IncrementalGrid,
    RemoteCapability::ProcessExit,
    RemoteCapability::Signal,
    RemoteCapability::ControllerLease,
    RemoteCapability::Scrollback,
];

/// Complete phase-one Helper command surface required before the Engine may
/// issue management RPCs. Keeping this list in the wire crate prevents the
/// Engine and bootstrapped Helper from advertising different contracts.
pub const PHASE_ONE_HELPER_CAPABILITIES: &[RemoteCapability] = &[
    RemoteCapability::FullSnapshot,
    RemoteCapability::IncrementalGrid,
    RemoteCapability::ProcessExit,
    RemoteCapability::Signal,
    RemoteCapability::ControllerLease,
    RemoteCapability::Scrollback,
    RemoteCapability::SessionManagement,
    RemoteCapability::EnvironmentCapture,
    RemoteCapability::DirectoryList,
    RemoteCapability::ExecutableDiscovery,
    RemoteCapability::PersistenceProbe,
    RemoteCapability::AtomicActivation,
];

/// Authentication bearer shared only by the local Engine and one Holder.
/// Debug formatting is deliberately redacted and owned bytes are zeroed on
/// drop; protocol payloads containing this type must never be logged.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SessionToken(String);

impl SessionToken {
    pub fn new(value: impl Into<String>) -> Result<Self, RemoteCodecError> {
        let value = value.into();
        if value.len() < 16 || value.len() > 512 || value.bytes().any(|byte| byte == 0) {
            return Err(RemoteCodecError::InvalidSessionToken);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionToken([REDACTED])")
    }
}

/// Metadata emitted by `diri-remote probe --format=json` before a protocol
/// channel is opened.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperProbe {
    pub protocol: ProtocolVersion,
    pub build_id: String,
    pub artifact_sha256: String,
    pub target: String,
    pub os: String,
    pub arch: String,
    pub supported: bool,
    pub holder_available: bool,
    pub capabilities: Vec<RemoteCapability>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol: ProtocolVersion,
    pub local_build_id: String,
    pub session_id: String,
    pub session_token: SessionToken,
    pub expected_incarnation: Option<String>,
    pub requested_role: RemoteRole,
    pub client_nonce: String,
    pub required_capabilities: Vec<RemoteCapability>,
    pub last_acknowledged_output_offset: Option<u64>,
    pub last_acknowledged_grid_sequence: Option<u64>,
}

impl Hello {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        validate_identifier("local build id", &self.local_build_id)?;
        validate_identifier("session id", &self.session_id)?;
        if let Some(incarnation) = &self.expected_incarnation {
            validate_identifier("expected incarnation", incarnation)?;
        }
        validate_identifier("client nonce", &self.client_nonce)?;
        SessionToken::new(self.session_token.expose_secret().to_string()).map(|_| ())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum RemoteProcessState {
    Running {
        pid: u32,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAck {
    pub protocol: ProtocolVersion,
    pub holder_build_id: String,
    pub session_incarnation: String,
    pub capabilities: Vec<RemoteCapability>,
    pub controller_epoch: u64,
    pub process_state: RemoteProcessState,
    pub output_offset: u64,
    pub snapshot_sequence: u64,
}

impl HelloAck {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        validate_identifier("holder build id", &self.holder_build_id)?;
        validate_identifier("session incarnation", &self.session_incarnation)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullSnapshot {
    pub sequence: u64,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseModes,
    pub grid: GridUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridDelta {
    pub sequence: u64,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseModes,
    pub grid: GridUpdate,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceCapability {
    NativeDetach,
    UserSupervisor,
    NonPersistent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceProbeAction {
    BeginNative,
    BeginSupervisor,
    Check,
    Cleanup,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceProbeRequest {
    pub nonce: String,
    pub action: PersistenceProbeAction,
}

impl PersistenceProbeRequest {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        validate_identifier("persistence probe nonce", &self.nonce)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceProbeResult {
    pub alive: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentVariable {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentCaptureRequest {
    pub cwd: Option<String>,
    pub timeout_millis: u64,
}

impl EnvironmentCaptureRequest {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        if !(1..=10_000).contains(&self.timeout_millis) {
            return Err(RemoteCodecError::InvalidLaunch(
                "environment timeout must be between 1 and 10000 ms".into(),
            ));
        }
        if let Some(cwd) = &self.cwd
            && (!(Path::new(cwd).is_absolute() || cwd == "~" || cwd.starts_with("~/"))
                || cwd.as_bytes().contains(&0)
                || cwd.split('/').any(|component| component == ".."))
        {
            return Err(RemoteCodecError::InvalidLaunch(
                "environment cwd must be absolute or home-relative, normalized and NUL-free".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentCaptureResult {
    pub shell: String,
    pub cwd: String,
    pub environment: Vec<EnvironmentVariable>,
    pub diagnostics: String,
    pub diagnostics_truncated: bool,
}

/// A shallow, read-only directory request used by the desktop folder picker.
/// It is intentionally separate from Holder/session state.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DirectoryListRequest {
    pub path: String,
    #[serde(default)]
    pub mode: DirectoryListMode,
}

impl DirectoryListRequest {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        if self.path.is_empty()
            || self.path.len() > 4_096
            || self.path.as_bytes().contains(&0)
            || (!(Path::new(&self.path).is_absolute()
                || self.path == "~"
                || self.path.starts_with("~/")))
            || self
                .path
                .split('/')
                .any(|component| matches!(component, "." | ".."))
        {
            return Err(RemoteCodecError::InvalidLaunch(
                "directory path must be absolute or home-relative, normalized, NUL-free, and at most 4096 bytes"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DirectoryListMode {
    #[default]
    Directories,
    Executables,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DirectoryEntryKind {
    #[default]
    Directory,
    Executable,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub kind: DirectoryEntryKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListResult {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableQuery {
    pub id: String,
    pub binary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutableDiscoveryRequest {
    pub queries: Vec<ExecutableQuery>,
    /// When present, capture the same login environment and resolved working
    /// directory that will be used to launch the selected Agent. Catalog
    /// scans omit this and pay for no separate cwd lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default = "default_environment_timeout_millis")]
    pub timeout_millis: u64,
}

const fn default_environment_timeout_millis() -> u64 {
    10_000
}

impl ExecutableDiscoveryRequest {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        if self.queries.is_empty() || self.queries.len() > MAX_EXECUTABLE_QUERIES {
            return Err(RemoteCodecError::InvalidLaunch(format!(
                "executable discovery requires 1..={MAX_EXECUTABLE_QUERIES} queries"
            )));
        }
        if !(100..=10_000).contains(&self.timeout_millis) {
            return Err(RemoteCodecError::InvalidLaunch(
                "executable discovery timeout must be between 100 and 10000 ms".into(),
            ));
        }
        EnvironmentCaptureRequest {
            cwd: self.cwd.clone(),
            timeout_millis: self.timeout_millis,
        }
        .validate()?;
        let mut total_bytes = self.cwd.as_ref().map_or(0, String::len);
        for query in &self.queries {
            validate_identifier("agent id", &query.id)?;
            if query.binary.is_empty()
                || query.binary.len() > 512
                || query.binary.as_bytes().contains(&0)
            {
                return Err(RemoteCodecError::InvalidLaunch(
                    "agent binary must be non-empty, NUL-free, and at most 512 bytes".into(),
                ));
            }
            if let Some(path) = &query.configured_path
                && (path.is_empty() || path.len() > 4_096 || path.as_bytes().contains(&0))
            {
                return Err(RemoteCodecError::InvalidLaunch(
                    "configured executable path must be NUL-free and at most 4096 bytes".into(),
                ));
            }
            total_bytes = total_bytes
                .saturating_add(query.id.len())
                .saturating_add(query.binary.len())
                .saturating_add(query.configured_path.as_ref().map_or(0, String::len));
        }
        if total_bytes > MAX_LAUNCH_BYTES {
            return Err(RemoteCodecError::InvalidLaunch(format!(
                "executable discovery payload exceeds {MAX_LAUNCH_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableDiscoveryItem {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableDiscoveryResult {
    /// The single login environment used to resolve every query. Spawn paths
    /// reuse it directly, avoiding a second remote shell startup.
    pub environment: EnvironmentCaptureResult,
    pub items: Vec<ExecutableDiscoveryItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    pub session_id: String,
    pub session_token: SessionToken,
    pub argv: Vec<String>,
    pub cwd: String,
    pub environment: Vec<EnvironmentVariable>,
    pub cols: u16,
    pub rows: u16,
    pub persistence: PersistenceCapability,
}

impl LaunchRequest {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        validate_identifier("session id", &self.session_id)?;
        SessionToken::new(self.session_token.expose_secret().to_string()).map(|_| ())?;
        if self.argv.is_empty() || self.argv.len() > MAX_ARGUMENTS {
            return Err(RemoteCodecError::InvalidLaunch(format!(
                "argv must contain 1..={MAX_ARGUMENTS} entries"
            )));
        }
        if self.environment.len() > MAX_ENVIRONMENT_VARIABLES {
            return Err(RemoteCodecError::InvalidLaunch(format!(
                "environment exceeds {MAX_ENVIRONMENT_VARIABLES} entries"
            )));
        }
        if !Path::new(&self.cwd).is_absolute() || self.cwd.as_bytes().contains(&0) {
            return Err(RemoteCodecError::InvalidLaunch(
                "cwd must be an absolute NUL-free path".into(),
            ));
        }
        validate_terminal_dimensions(self.cols, self.rows)?;

        let mut bytes = self.cwd.len();
        for argument in &self.argv {
            if argument.as_bytes().contains(&0) {
                return Err(RemoteCodecError::InvalidLaunch(
                    "argv contains a NUL byte".into(),
                ));
            }
            bytes = bytes.saturating_add(argument.len());
        }
        for variable in &self.environment {
            let valid_name = !variable.name.is_empty()
                && !variable.name.bytes().any(|byte| byte == 0 || byte == b'=');
            if !valid_name || variable.value.as_bytes().contains(&0) {
                return Err(RemoteCodecError::InvalidLaunch(
                    "environment contains an invalid name or NUL byte".into(),
                ));
            }
            if variable.value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
                return Err(RemoteCodecError::InvalidLaunch(format!(
                    "environment value exceeds {MAX_ENVIRONMENT_VALUE_BYTES} bytes"
                )));
            }
            bytes = bytes.saturating_add(variable.name.len() + variable.value.len());
        }
        if bytes > MAX_LAUNCH_BYTES {
            return Err(RemoteCodecError::InvalidLaunch(format!(
                "launch payload exceeds {MAX_LAUNCH_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

pub fn validate_terminal_dimensions(cols: u16, rows: u16) -> Result<(), RemoteCodecError> {
    if cols == 0
        || rows == 0
        || cols > MAX_TERMINAL_COLS
        || rows > MAX_TERMINAL_ROWS
        || usize::from(cols).saturating_mul(usize::from(rows)) > MAX_TERMINAL_CELLS
    {
        return Err(RemoteCodecError::InvalidLaunch(format!(
            "terminal dimensions must be non-zero, at most {MAX_TERMINAL_COLS}x{MAX_TERMINAL_ROWS}, and at most {MAX_TERMINAL_CELLS} cells"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchResult {
    pub session_id: String,
    pub session_incarnation: String,
    pub holder_pid: u32,
    pub process_pid: u32,
    pub persistence: PersistenceCapability,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspection {
    pub session_id: String,
    pub session_incarnation: String,
    pub holder_build_id: String,
    pub holder_pid: u32,
    pub process_state: RemoteProcessState,
    pub cols: u16,
    pub rows: u16,
    pub output_offset: u64,
    pub snapshot_sequence: u64,
    pub controller_epoch: u64,
    pub persistence: PersistenceCapability,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcResult {
    pub removed_sessions: usize,
    pub retained_sessions: usize,
    pub removed_helper_builds: usize,
    pub retained_helper_builds: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSelector {
    pub session_id: String,
    pub session_token: SessionToken,
    pub expected_incarnation: Option<String>,
}

impl SessionSelector {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        validate_identifier("session id", &self.session_id)?;
        SessionToken::new(self.session_token.expose_secret().to_string()).map(|_| ())?;
        if let Some(incarnation) = &self.expected_incarnation {
            validate_identifier("expected incarnation", incarnation)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signal {
    pub controller_epoch: u64,
    pub signal: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcquireControl {
    pub client_nonce: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlGranted {
    pub controller_epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRevoked {
    pub controller_epoch: u64,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseControl {
    pub controller_epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteError {
    pub code: String,
    pub message: String,
    pub fatal: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollbackRequest {
    pub request_id: u64,
    pub first_row: i64,
    pub max_rows: i64,
}

impl ScrollbackRequest {
    pub fn validate(&self) -> Result<(), RemoteCodecError> {
        if self.request_id == 0 || self.first_row < 0 || !(0..=1024).contains(&self.max_rows) {
            Err(RemoteCodecError::InvalidControlPayload {
                kind: KIND_SCROLLBACK_REQUEST,
                detail: "scrollback request id/range is invalid".into(),
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollbackResponse {
    pub request_id: u64,
    pub result: crate::ReadScrollbackCellsResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteMessage {
    Terminal(Frame),
    Hello(Hello),
    HelloAck(HelloAck),
    FullSnapshot(FullSnapshot),
    GridDelta(GridDelta),
    ProcessExit(ProcessExit),
    Signal(Signal),
    AcquireControl(AcquireControl),
    ControlGranted(ControlGranted),
    ControlRevoked(ControlRevoked),
    ReleaseControl(ReleaseControl),
    ScrollbackRequest(ScrollbackRequest),
    ScrollbackResponse(ScrollbackResponse),
    Error(RemoteError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteCodecError {
    UnknownMessageType(u8),
    FrameTooLarge { length: usize, max: usize },
    ControlFrameTooLarge { length: usize, max: usize },
    PayloadLengthOverflow(usize),
    InvalidControlPayload { kind: u8, detail: String },
    InvalidFullSnapshot(String),
    InvalidIdentifier { field: &'static str, value: String },
    InvalidSessionToken,
    InvalidLaunch(String),
    Grid(GridCodecError),
}

impl fmt::Display for RemoteCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMessageType(kind) => {
                write!(formatter, "unknown remote message type {kind}")
            }
            Self::FrameTooLarge { length, max } => {
                write!(
                    formatter,
                    "remote frame is {length} bytes; maximum is {max}"
                )
            }
            Self::ControlFrameTooLarge { length, max } => write!(
                formatter,
                "remote control frame is {length} bytes; maximum is {max}"
            ),
            Self::PayloadLengthOverflow(length) => {
                write!(
                    formatter,
                    "remote payload length {length} does not fit in u32"
                )
            }
            Self::InvalidControlPayload { kind, detail } => {
                write!(
                    formatter,
                    "invalid remote control payload for type {kind}: {detail}"
                )
            }
            Self::InvalidFullSnapshot(detail) => {
                write!(formatter, "invalid full snapshot: {detail}")
            }
            Self::InvalidIdentifier { field, value } => {
                write!(formatter, "invalid {field} {value:?}")
            }
            Self::InvalidSessionToken => formatter.write_str("invalid session token"),
            Self::InvalidLaunch(detail) => write!(formatter, "invalid launch request: {detail}"),
            Self::Grid(error) => error.fmt(formatter),
        }
    }
}

impl Error for RemoteCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Grid(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GridCodecError> for RemoteCodecError {
    fn from(error: GridCodecError) -> Self {
        Self::Grid(error)
    }
}

/// Incrementally decodes the SSH stdio byte stream.
#[derive(Clone, Debug, Default)]
pub struct RemoteCodec {
    buffer: Vec<u8>,
}

impl RemoteCodec {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes into a fresh buffer. Hot paths can use [`Self::encode_into`] to
    /// reuse an allocation.
    pub fn encode(message: &RemoteMessage) -> Result<Vec<u8>, RemoteCodecError> {
        let mut encoded = Vec::new();
        Self::encode_into(message, &mut encoded)?;
        Ok(encoded)
    }

    /// Appends one encoded message to `output` without an intermediate
    /// terminal-frame wrapper.
    pub fn encode_into(
        message: &RemoteMessage,
        output: &mut Vec<u8>,
    ) -> Result<(), RemoteCodecError> {
        let start = output.len();
        output.resize(start + HEADER_BYTES, 0);

        let result = (|| match message {
            RemoteMessage::Terminal(frame) => {
                output[start] = frame.frame_type as u8;
                output.extend_from_slice(&frame.payload);
                Ok(())
            }
            RemoteMessage::Hello(value) => {
                value.validate()?;
                append_json(KIND_HELLO, value, output, start)
            }
            RemoteMessage::HelloAck(value) => {
                value.validate()?;
                append_json(KIND_HELLO_ACK, value, output, start)
            }
            RemoteMessage::FullSnapshot(value) => {
                validate_grid_update(&value.grid)?;
                output[start] = KIND_FULL_SNAPSHOT;
                output.extend_from_slice(&value.sequence.to_be_bytes());
                let modes = u8::from(value.alt_screen)
                    | (u8::from(value.bracketed_paste) << 1)
                    // Keep bit 2 as the historical any-reporting flag so an
                    // older peer remains compatible with this additive mode
                    // byte extension.
                    | (u8::from(value.mouse.is_reporting()) << 2)
                    | (value.mouse.detail_bits() << 3);
                output.push(modes);
                if !value.grid.is_full_snapshot {
                    return rollback(
                        output,
                        start,
                        RemoteCodecError::InvalidFullSnapshot(
                            "grid update is not marked as a full snapshot".into(),
                        ),
                    );
                }
                output.extend_from_slice(&value.grid.encode()?);
                Ok(())
            }
            RemoteMessage::GridDelta(value) => {
                validate_grid_update(&value.grid)?;
                output[start] = KIND_GRID_DELTA;
                output.extend_from_slice(&value.sequence.to_be_bytes());
                let modes = u8::from(value.alt_screen)
                    | (u8::from(value.bracketed_paste) << 1)
                    | (u8::from(value.mouse.is_reporting()) << 2)
                    | (value.mouse.detail_bits() << 3);
                output.push(modes);
                if value.grid.is_full_snapshot {
                    return rollback(
                        output,
                        start,
                        RemoteCodecError::InvalidFullSnapshot(
                            "grid delta is marked as a full snapshot".into(),
                        ),
                    );
                }
                output.extend_from_slice(&value.grid.encode()?);
                Ok(())
            }
            RemoteMessage::ProcessExit(value) => {
                append_json(KIND_PROCESS_EXIT, value, output, start)
            }
            RemoteMessage::Signal(value) => append_json(KIND_SIGNAL, value, output, start),
            RemoteMessage::AcquireControl(value) => {
                validate_identifier("client nonce", &value.client_nonce)?;
                append_json(KIND_ACQUIRE_CONTROL, value, output, start)
            }
            RemoteMessage::ControlGranted(value) => {
                append_json(KIND_CONTROL_GRANTED, value, output, start)
            }
            RemoteMessage::ControlRevoked(value) => {
                append_json(KIND_CONTROL_REVOKED, value, output, start)
            }
            RemoteMessage::ReleaseControl(value) => {
                append_json(KIND_RELEASE_CONTROL, value, output, start)
            }
            RemoteMessage::ScrollbackRequest(value) => {
                value.validate()?;
                append_json(KIND_SCROLLBACK_REQUEST, value, output, start)
            }
            RemoteMessage::ScrollbackResponse(value) => {
                append_json(KIND_SCROLLBACK_RESPONSE, value, output, start)
            }
            RemoteMessage::Error(value) => append_json(KIND_ERROR, value, output, start),
        })();

        if let Err(error) = result {
            output.truncate(start);
            return Err(error);
        }

        let payload_length = output.len() - start - HEADER_BYTES;
        if payload_length > MAX_FRAME_BYTES {
            return rollback(
                output,
                start,
                RemoteCodecError::FrameTooLarge {
                    length: payload_length,
                    max: MAX_FRAME_BYTES,
                },
            );
        }
        if output[start] >= KIND_HELLO
            && !matches!(
                output[start],
                KIND_FULL_SNAPSHOT | KIND_GRID_DELTA | KIND_SCROLLBACK_RESPONSE
            )
            && payload_length > MAX_CONTROL_FRAME_BYTES
        {
            return rollback(
                output,
                start,
                RemoteCodecError::ControlFrameTooLarge {
                    length: payload_length,
                    max: MAX_CONTROL_FRAME_BYTES,
                },
            );
        }
        let payload_length = u32::try_from(payload_length)
            .map_err(|_| RemoteCodecError::PayloadLengthOverflow(payload_length))?;
        output[start + 1..start + HEADER_BYTES].copy_from_slice(&payload_length.to_be_bytes());
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<RemoteMessage>, RemoteCodecError> {
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        let mut consumed = 0;

        while self.buffer.len() - consumed >= HEADER_BYTES {
            let kind = self.buffer[consumed];
            validate_kind(kind)?;
            let length = u32::from_be_bytes(
                self.buffer[consumed + 1..consumed + HEADER_BYTES]
                    .try_into()
                    .expect("header length checked"),
            ) as usize;
            if length > MAX_FRAME_BYTES {
                return Err(RemoteCodecError::FrameTooLarge {
                    length,
                    max: MAX_FRAME_BYTES,
                });
            }
            if kind >= KIND_HELLO
                && !matches!(
                    kind,
                    KIND_FULL_SNAPSHOT | KIND_GRID_DELTA | KIND_SCROLLBACK_RESPONSE
                )
                && length > MAX_CONTROL_FRAME_BYTES
            {
                return Err(RemoteCodecError::ControlFrameTooLarge {
                    length,
                    max: MAX_CONTROL_FRAME_BYTES,
                });
            }
            let frame_end = consumed + HEADER_BYTES + length;
            if self.buffer.len() < frame_end {
                break;
            }
            messages.push(decode_message(
                kind,
                &self.buffer[consumed + HEADER_BYTES..frame_end],
            )?);
            consumed = frame_end;
        }

        if consumed != 0 {
            self.buffer.drain(..consumed);
        }
        Ok(messages)
    }

    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

fn append_json<T: Serialize>(
    kind: u8,
    value: &T,
    output: &mut Vec<u8>,
    start: usize,
) -> Result<(), RemoteCodecError> {
    output[start] = kind;
    serde_json::to_writer(output, value).map_err(|error| RemoteCodecError::InvalidControlPayload {
        kind,
        detail: error.to_string(),
    })
}

fn decode_json<T: DeserializeOwned>(kind: u8, payload: &[u8]) -> Result<T, RemoteCodecError> {
    serde_json::from_slice(payload).map_err(|error| RemoteCodecError::InvalidControlPayload {
        kind,
        detail: error.to_string(),
    })
}

fn decode_message(kind: u8, payload: &[u8]) -> Result<RemoteMessage, RemoteCodecError> {
    if kind <= FrameType::Mouse as u8 {
        return Ok(RemoteMessage::Terminal(Frame::new(
            FrameType::try_from(kind).map_err(|_| RemoteCodecError::UnknownMessageType(kind))?,
            payload.to_vec(),
        )));
    }
    match kind {
        KIND_HELLO => {
            let value: Hello = decode_json(kind, payload)?;
            value.validate()?;
            Ok(RemoteMessage::Hello(value))
        }
        KIND_HELLO_ACK => {
            let value: HelloAck = decode_json(kind, payload)?;
            value.validate()?;
            Ok(RemoteMessage::HelloAck(value))
        }
        KIND_FULL_SNAPSHOT => {
            if payload.len() < FULL_SNAPSHOT_FIXED_BYTES {
                return Err(RemoteCodecError::InvalidFullSnapshot(format!(
                    "need at least {FULL_SNAPSHOT_FIXED_BYTES} bytes, got {}",
                    payload.len()
                )));
            }
            let sequence = u64::from_be_bytes(payload[..8].try_into().expect("length checked"));
            let modes = payload[8];
            let grid = GridUpdate::decode(&payload[FULL_SNAPSHOT_FIXED_BYTES..])?;
            validate_grid_update(&grid)?;
            if !grid.is_full_snapshot {
                return Err(RemoteCodecError::InvalidFullSnapshot(
                    "grid update is not marked as a full snapshot".into(),
                ));
            }
            Ok(RemoteMessage::FullSnapshot(FullSnapshot {
                sequence,
                alt_screen: modes & 1 != 0,
                bracketed_paste: modes & 2 != 0,
                mouse: MouseModes::from_detail_bits(modes >> 3, modes & 4 != 0),
                grid,
            }))
        }
        KIND_GRID_DELTA => {
            if payload.len() < 9 {
                return Err(RemoteCodecError::InvalidFullSnapshot(format!(
                    "grid delta needs at least 8 bytes, got {}",
                    payload.len()
                )));
            }
            let sequence = u64::from_be_bytes(payload[..8].try_into().expect("length checked"));
            let modes = payload[8];
            let grid = GridUpdate::decode(&payload[9..])?;
            validate_grid_update(&grid)?;
            if grid.is_full_snapshot {
                return Err(RemoteCodecError::InvalidFullSnapshot(
                    "grid delta is marked as a full snapshot".into(),
                ));
            }
            Ok(RemoteMessage::GridDelta(GridDelta {
                sequence,
                alt_screen: modes & 1 != 0,
                bracketed_paste: modes & 2 != 0,
                mouse: MouseModes::from_detail_bits(modes >> 3, modes & 4 != 0),
                grid,
            }))
        }
        KIND_PROCESS_EXIT => Ok(RemoteMessage::ProcessExit(decode_json(kind, payload)?)),
        KIND_SIGNAL => Ok(RemoteMessage::Signal(decode_json(kind, payload)?)),
        KIND_ACQUIRE_CONTROL => {
            let value: AcquireControl = decode_json(kind, payload)?;
            validate_identifier("client nonce", &value.client_nonce)?;
            Ok(RemoteMessage::AcquireControl(value))
        }
        KIND_CONTROL_GRANTED => Ok(RemoteMessage::ControlGranted(decode_json(kind, payload)?)),
        KIND_CONTROL_REVOKED => Ok(RemoteMessage::ControlRevoked(decode_json(kind, payload)?)),
        KIND_RELEASE_CONTROL => Ok(RemoteMessage::ReleaseControl(decode_json(kind, payload)?)),
        KIND_SCROLLBACK_REQUEST => {
            let value: ScrollbackRequest = decode_json(kind, payload)?;
            value.validate()?;
            Ok(RemoteMessage::ScrollbackRequest(value))
        }
        KIND_SCROLLBACK_RESPONSE => Ok(RemoteMessage::ScrollbackResponse(decode_json(
            kind, payload,
        )?)),
        KIND_ERROR => Ok(RemoteMessage::Error(decode_json(kind, payload)?)),
        _ => Err(RemoteCodecError::UnknownMessageType(kind)),
    }
}

fn validate_kind(kind: u8) -> Result<(), RemoteCodecError> {
    if (1..=FrameType::Mouse as u8).contains(&kind)
        || (KIND_HELLO..=KIND_SCROLLBACK_RESPONSE).contains(&kind)
    {
        Ok(())
    } else {
        Err(RemoteCodecError::UnknownMessageType(kind))
    }
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), RemoteCodecError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(RemoteCodecError::InvalidIdentifier {
            field,
            value: value.to_string(),
        })
    }
}

fn validate_grid_update(grid: &GridUpdate) -> Result<(), RemoteCodecError> {
    validate_terminal_dimensions(grid.cols, grid.rows).map_err(|error| {
        RemoteCodecError::InvalidFullSnapshot(format!("grid dimensions are invalid: {error}"))
    })?;
    if grid.cursor_col >= grid.cols || grid.cursor_row >= grid.rows {
        return Err(RemoteCodecError::InvalidFullSnapshot(
            "cursor is outside the terminal grid".into(),
        ));
    }
    for row in &grid.changed_rows {
        if row.y >= grid.rows || row.cells.len() != usize::from(grid.cols) {
            return Err(RemoteCodecError::InvalidFullSnapshot(
                "changed row is outside the grid or has the wrong width".into(),
            ));
        }
    }
    Ok(())
}

fn rollback<T>(
    output: &mut Vec<u8>,
    start: usize,
    error: RemoteCodecError,
) -> Result<T, RemoteCodecError> {
    output.truncate(start);
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{ChangedRow, GridCell};

    fn hello() -> Hello {
        Hello {
            protocol: ProtocolVersion::CURRENT,
            local_build_id: "build-abc".into(),
            session_id: "s_123".into(),
            session_token: SessionToken::new("0123456789abcdef").expect("token"),
            expected_incarnation: Some("incarnation-1".into()),
            requested_role: RemoteRole::Controller,
            client_nonce: "nonce-1".into(),
            required_capabilities: vec![RemoteCapability::FullSnapshot],
            last_acknowledged_output_offset: Some(6),
            last_acknowledged_grid_sequence: Some(7),
        }
    }

    #[test]
    fn unknown_optional_capability_is_forward_compatible() {
        let capability: RemoteCapability =
            serde_json::from_str("\"future-optional-capability\"").expect("capability");
        assert_eq!(capability, RemoteCapability::Unknown);
    }

    #[test]
    fn directory_requests_accept_only_normalized_absolute_or_home_paths() {
        for valid in ["/", "/srv/app", "~", "~/code"] {
            assert!(
                DirectoryListRequest {
                    path: valid.into(),
                    mode: DirectoryListMode::Directories,
                }
                .validate()
                .is_ok(),
                "{valid}"
            );
        }
        for invalid in ["relative", "/srv/../etc", "/srv/./app", "~/../etc"] {
            assert!(
                DirectoryListRequest {
                    path: invalid.into(),
                    mode: DirectoryListMode::Directories,
                }
                .validate()
                .is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn executable_discovery_is_bounded_and_validates_its_launch_context() {
        let request = ExecutableDiscoveryRequest {
            queries: vec![ExecutableQuery {
                id: "codex".into(),
                binary: "codex".into(),
                configured_path: Some("~/.local/bin/codex".into()),
            }],
            cwd: Some("~/code".into()),
            timeout_millis: 1_000,
        };
        assert!(request.validate().is_ok());

        let mut empty = request.clone();
        empty.queries.clear();
        assert!(empty.validate().is_err());

        let mut traversal = request;
        traversal.cwd = Some("~/../secrets".into());
        assert!(traversal.validate().is_err());
    }

    fn snapshot() -> FullSnapshot {
        FullSnapshot {
            sequence: 42,
            alt_screen: true,
            bracketed_paste: true,
            mouse: MouseModes::new(
                crate::terminal::MouseTrackingMode::ButtonMotion,
                crate::terminal::MouseEncoding::Sgr,
            ),
            grid: GridUpdate {
                cols: 2,
                rows: 1,
                cursor_col: 1,
                cursor_row: 0,
                cursor_visible: true,
                is_full_snapshot: true,
                changed_rows: vec![ChangedRow::new(0, vec![GridCell::BLANK; 2])],
            },
        }
    }

    #[test]
    fn terminal_frames_keep_their_existing_wire_kind() {
        for frame in [
            Frame::input(b"abc".to_vec()),
            Frame::mouse(b"mouse".to_vec()),
        ] {
            let expected_kind = frame.frame_type as u8;
            let message = RemoteMessage::Terminal(frame);
            let encoded = RemoteCodec::encode(&message).expect("encode");
            assert_eq!(encoded[0], expected_kind);

            let decoded = RemoteCodec::new().feed(&encoded).expect("decode");
            assert_eq!(decoded, vec![message]);
        }
    }

    #[test]
    fn handshake_reassembles_at_every_partial_read_boundary() {
        let message = RemoteMessage::Hello(hello());
        let encoded = RemoteCodec::encode(&message).expect("encode");
        for split in 0..encoded.len() {
            let mut codec = RemoteCodec::new();
            assert!(codec.feed(&encoded[..split]).expect("prefix").is_empty());
            assert_eq!(
                codec.feed(&encoded[split..]).expect("suffix"),
                vec![message.clone()]
            );
            assert_eq!(codec.buffered_len(), 0);
        }
    }

    #[test]
    fn full_snapshot_round_trips_binary_grid_and_modes() {
        let message = RemoteMessage::FullSnapshot(snapshot());
        let encoded = RemoteCodec::encode(&message).expect("encode");
        assert_eq!(encoded[0], KIND_FULL_SNAPSHOT);
        assert_ne!(encoded[13] & 0b100, 0, "historical any-mouse bit");
        assert_eq!(
            RemoteCodec::new().feed(&encoded).expect("decode"),
            vec![message]
        );
    }

    #[test]
    fn full_snapshot_keeps_pre_1_4_mouse_details_unknown() {
        let message = RemoteMessage::FullSnapshot(snapshot());
        let mut encoded = RemoteCodec::encode(&message).expect("encode");
        // Header (5), sequence (8), then the historical flags byte. Strip
        // every detailed bit and leave alt/bracketed/any-mouse enabled.
        encoded[13] = 0b111;
        let decoded = RemoteCodec::new().feed(&encoded).expect("decode");
        let RemoteMessage::FullSnapshot(decoded) = &decoded[0] else {
            panic!("snapshot");
        };
        assert_eq!(decoded.mouse, MouseModes::UNKNOWN);
    }

    #[test]
    fn each_pre_1_4_mouse_regime_decodes_without_a_false_guess() {
        // Protocol 1.3 represented all of these states with the same bit. A
        // new peer must therefore preserve that ambiguity instead of choosing
        // a tracking mode or coordinate encoding that may be wrong.
        for legacy_state in ["1000-legacy", "1002-sgr", "1003-legacy"] {
            let message = RemoteMessage::FullSnapshot(snapshot());
            let mut encoded = RemoteCodec::encode(&message).expect("encode");
            encoded[13] = 0b100;
            let decoded = RemoteCodec::new().feed(&encoded).expect("decode");
            let RemoteMessage::FullSnapshot(decoded) = &decoded[0] else {
                panic!("snapshot");
            };
            assert_eq!(decoded.mouse, MouseModes::UNKNOWN, "{legacy_state}");
        }
    }

    #[test]
    fn grid_delta_round_trips_with_its_sequence() {
        let mut grid = snapshot().grid;
        grid.is_full_snapshot = false;
        grid.changed_rows.truncate(1);
        let message = RemoteMessage::GridDelta(GridDelta {
            sequence: 43,
            alt_screen: true,
            bracketed_paste: true,
            mouse: MouseModes::new(
                crate::terminal::MouseTrackingMode::ButtonMotion,
                crate::terminal::MouseEncoding::Sgr,
            ),
            grid,
        });
        let encoded = RemoteCodec::encode(&message).expect("encode");
        assert_eq!(encoded[0], KIND_GRID_DELTA);
        assert_eq!(
            RemoteCodec::new().feed(&encoded).expect("decode"),
            vec![message]
        );
    }

    #[test]
    fn launch_validation_rejects_shell_and_path_ambiguity() {
        let mut request = LaunchRequest {
            session_id: "session-1".into(),
            session_token: SessionToken::new("0123456789abcdef").expect("token"),
            argv: vec!["/bin/printf".into(), "a value; untouched".into()],
            cwd: "/tmp/project with spaces".into(),
            environment: vec![EnvironmentVariable {
                name: "DIRI_VALUE".into(),
                value: "literal $(command)".into(),
            }],
            cols: 80,
            rows: 24,
            persistence: PersistenceCapability::NonPersistent,
        };
        request.validate().expect("structured values are valid");
        request.cwd = "relative".into();
        assert!(matches!(
            request.validate(),
            Err(RemoteCodecError::InvalidLaunch(_))
        ));
    }

    #[test]
    fn terminal_dimensions_have_a_memory_bound() {
        validate_terminal_dimensions(80, 24).expect("ordinary terminal");
        assert!(validate_terminal_dimensions(0, 24).is_err());
        assert!(validate_terminal_dimensions(MAX_TERMINAL_COLS + 1, 24).is_err());
        assert!(validate_terminal_dimensions(2_000, 2_000).is_err());
    }

    #[test]
    fn oversized_snapshot_dimensions_are_rejected_before_allocation() {
        let mut oversized = snapshot();
        oversized.grid.cols = u16::MAX;
        oversized.grid.rows = u16::MAX;
        oversized.grid.cursor_col = 0;
        oversized.grid.cursor_row = 0;
        oversized.grid.changed_rows.clear();
        assert!(matches!(
            RemoteCodec::encode(&RemoteMessage::FullSnapshot(oversized.clone())),
            Err(RemoteCodecError::InvalidFullSnapshot(_))
        ));

        // Build the hostile bytes with the lower-level grid codec so the
        // receiver-side check is exercised independently of the encoder.
        let mut payload = Vec::new();
        payload.extend_from_slice(&oversized.sequence.to_be_bytes());
        payload.push(0);
        payload.extend_from_slice(&oversized.grid.encode().expect("raw grid encoding"));
        let mut frame = vec![KIND_FULL_SNAPSHOT];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        assert!(matches!(
            RemoteCodec::new().feed(&frame),
            Err(RemoteCodecError::InvalidFullSnapshot(_))
        ));
    }

    #[test]
    fn incremental_grid_is_rejected_as_a_full_snapshot() {
        let mut snapshot = snapshot();
        snapshot.grid.is_full_snapshot = false;
        let error = RemoteCodec::encode(&RemoteMessage::FullSnapshot(snapshot))
            .expect_err("reject an incremental update");
        assert!(matches!(error, RemoteCodecError::InvalidFullSnapshot(_)));
    }

    #[test]
    fn identifiers_cannot_be_used_as_path_components() {
        let mut hello = hello();
        hello.session_id = "../holder".into();
        let error =
            RemoteCodec::encode(&RemoteMessage::Hello(hello)).expect_err("reject path traversal");
        assert!(matches!(
            error,
            RemoteCodecError::InvalidIdentifier {
                field: "session id",
                ..
            }
        ));
    }

    #[test]
    fn unknown_and_oversized_headers_fail_before_payload_arrives() {
        let mut codec = RemoteCodec::new();
        assert_eq!(
            codec.feed(&[31, 0, 0, 0, 0]),
            Err(RemoteCodecError::UnknownMessageType(31))
        );

        let mut codec = RemoteCodec::new();
        let oversized = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        let mut header = vec![FrameType::Grid as u8];
        header.extend_from_slice(&oversized);
        assert_eq!(
            codec.feed(&header),
            Err(RemoteCodecError::FrameTooLarge {
                length: MAX_FRAME_BYTES + 1,
                max: MAX_FRAME_BYTES,
            })
        );
    }
}
