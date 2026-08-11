//! Typed control methods shared by the Rust Engine and its clients.

use crate::control::{JsonValue, WIRE_VERSION};
use crate::model::{
    AgentKind, DateMillis, PortInfo, Project, SessionArtifact, SessionId, SessionRecord,
    SessionStatus, WorktreeInfo,
};
use serde::{Deserialize, Serialize};

/// Stable control-plane identity of the authoritative Rust Engine. Clients
/// use this additive Hello field to reject a protocol-compatible legacy daemon
/// instead of silently routing remote work around the Rust implementation.
pub const RUST_ENGINE_KIND: &str = "diri-rust-engine";

/// Control-channel method names.
pub struct Method;

impl Method {
    pub const HELLO: &'static str = "hello";
    pub const SESSION_SPAWN: &'static str = "session.spawn";
    pub const SESSION_LIST: &'static str = "session.list";
    pub const SESSION_KILL: &'static str = "session.kill";
    pub const SESSION_REMOVE: &'static str = "session.remove";
    pub const SESSION_RENAME: &'static str = "session.rename";
    pub const SESSION_RESUME: &'static str = "session.resume";
    pub const SESSION_SEND_TEXT: &'static str = "session.send_text";
    pub const SESSION_RESIZE: &'static str = "session.resize";
    pub const SESSION_READ_SCREEN: &'static str = "session.read_screen";
    pub const SESSION_READ_SCROLLBACK: &'static str = "session.read_scrollback";
    pub const SESSION_READ_SCROLLBACK_CELLS: &'static str = "session.read_scrollback_cells";
    pub const SESSION_READ_DIFF: &'static str = "session.read_diff";
    pub const SESSION_MARK_SEEN: &'static str = "session.mark_seen";
    pub const SESSION_HIBERNATE: &'static str = "session.hibernate";
    pub const SESSION_WAKE: &'static str = "session.wake";
    pub const SESSION_ARCHIVE: &'static str = "session.archive";
    pub const SESSION_UNARCHIVE: &'static str = "session.unarchive";
    pub const SESSION_REOPEN_LAST: &'static str = "session.reopen_last";
    pub const SESSION_MIGRATE: &'static str = "session.migrate";
    pub const HOST_SYNC_PREFS: &'static str = "host.sync_prefs";
    pub const HOST_LOCATE_REPO: &'static str = "host.locate_repo";
    pub const HOST_INITIALIZE: &'static str = "host.initialize";
    pub const HOST_LIST_DIRECTORIES: &'static str = "host.list_directories";
    pub const SESSION_HISTORY: &'static str = "session.history";
    pub const SESSION_RESUME_FROM_HISTORY: &'static str = "session.resume_from_history";
    pub const WORKTREE_CREATE: &'static str = "worktree.create";
    pub const WORKTREE_LIST: &'static str = "worktree.list";
    pub const WORKTREE_REMOVE: &'static str = "worktree.remove";
    pub const WORKTREE_OVERVIEW: &'static str = "worktree.overview";
    pub const PROJECT_ADD: &'static str = "project.add";
    pub const CLIENT_SET_ACTIVE: &'static str = "client.set_active";
    pub const GOVERNOR_CONFIGURE: &'static str = "governor.configure";
    pub const AGENT_READINESS: &'static str = "agent.readiness";
    pub const EVENTS_SUBSCRIBE: &'static str = "events.subscribe";
    pub const EVENTS_WAIT: &'static str = "events.wait";
    pub const HOOK_REPORT: &'static str = "hook.report";
    pub const TEST_RUN: &'static str = "test.run";
    pub const STATE_SNAPSHOT: &'static str = "state.snapshot";
    pub const DAEMON_PREPARE_SHUTDOWN: &'static str = "daemon.prepare_shutdown";
    pub const DAEMON_SHUTDOWN_IF_IDLE: &'static str = "daemon.shutdown_if_idle";
    pub const DAEMON_SHUTDOWN: &'static str = "daemon.shutdown";
}

/// Event names pushed on subscribed control channels.
pub struct EventName;

impl EventName {
    pub const SESSION_UPDATED: &'static str = "session.updated";
    pub const SESSION_RESOURCES: &'static str = "session.resources";
    pub const SESSION_REMOVED: &'static str = "session.removed";
    pub const PROJECT_UPDATED: &'static str = "project.updated";
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResourcesEvent {
    pub id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listening_ports: Option<Vec<PortInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<SessionArtifact>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EmptyParams {}

pub type EmptyResult = EmptyParams;

/// Result of a best-effort desktop ownership release. The Engine remains
/// alive whenever it still owns a live session or another control client is
/// connected; callers must never turn a refusal into an unconditional kill.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonShutdownIfIdleResult {
    pub will_exit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HelloParams {
    pub proto: u32,
    pub build: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl HelloParams {
    pub fn new(build: impl Into<String>) -> Self {
        Self {
            proto: WIRE_VERSION,
            build: build.into(),
            token: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub proto: u32,
    pub build: String,
    pub pid: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable_hash: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ClientActiveParams {
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GovernorSettingsParams {
    pub idle_threshold_seconds: f64,
    pub hard_memory_bytes: u64,
}

impl GovernorSettingsParams {
    pub fn new(idle_threshold_seconds: f64, hard_memory_bytes: u64) -> Self {
        Self {
            idle_threshold_seconds: idle_threshold_seconds.max(0.0),
            hard_memory_bytes,
        }
    }
}

/// A keystroke answer to a permission prompt, declared by the agent's manifest.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentKeystroke {
    /// Text to type. Empty means "send nothing, just the Return".
    pub text: String,
    /// Whether to append a Return after `text`.
    pub submit: bool,
}

/// Optional, display-only guidance for installing and authenticating an
/// Agent. Clients must never execute either hint; the URL is opened only after
/// an explicit user action and is validated by the client as HTTP(S).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSetup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in_hint: Option<String>,
}

/// The daemon-side manifest descriptor for one agent, as much of it as the
/// client needs. Deliberately partial and tolerant: the daemon owns the full
/// schema (spawn args, env hygiene, injection), and unknown fields are ignored
/// so a newer daemon can grow the manifest without breaking an older client.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDescriptor {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub short_label: String,
    #[serde(default)]
    pub glyph: String,
    #[serde(default)]
    pub first_class: bool,
    /// The keystroke meaning "yes" at this CLI's permission prompt, when one is
    /// safe to send unattended. Absent ⇒ the notification offers no quick
    /// approve, which is the correct conservative default for an agent whose
    /// dialog we haven't verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve: Option<AgentKeystroke>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<AgentKeystroke>,
    /// Additive setup guidance. Older clients ignore this field and older
    /// daemons omit it, so user manifest overrides remain forwards/backwards
    /// compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<AgentSetup>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentReadinessItem {
    pub kind: AgentKind,
    pub binary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The agent's manifest descriptor. This is how the AGENT CATALOG reaches
    /// the client: `agent.readiness` doubles as "what agents exist and what can
    /// they do". Absent when talking to a daemon that predates it — the client
    /// then falls back to its built-in knowledge of the original four.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<AgentDescriptor>,
}

impl AgentReadinessItem {
    pub fn available(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AgentReadinessResult {
    pub agents: Vec<AgentReadinessItem>,
}

impl AgentReadinessResult {
    /// The descriptor for `kind`, when the daemon shipped one.
    #[must_use]
    pub fn descriptor(&self, kind: &AgentKind) -> Option<&AgentDescriptor> {
        self.agents
            .iter()
            .find(|item| item.kind.id() == kind.id())
            .and_then(|item| item.descriptor.as_ref())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSpawnParams {
    pub kind: AgentKind,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_worktree: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_cols: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_rows: Option<i64>,
    /// `HostEntry.id` from `hosts.json` — spawn through the remote PTY Holder
    /// transport instead of locally. Absent means local.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Repo-preserving spawn: open in the checkout of the SAME repository as
    /// this session (matched by origin URL) on the target host. Falls back to
    /// `cwd` / the host's defaultCwd when the repo isn't cloned there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_repo_as: Option<SessionId>,
}

pub type SessionSpawnResult = SessionRecord;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionListResult {
    pub sessions: Vec<SessionRecord>,
    pub projects: Vec<Project>,
}

pub type SessionListParams = EmptyParams;
pub type StateSnapshotResult = SessionListResult;
pub type StateSnapshotParams = EmptyParams;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

/// Swift spelling retained for protocol-oriented callers.
pub use SessionIdParams as SessionIDParams;

pub type SessionKillParams = SessionIdParams;
pub type SessionRemoveParams = SessionIdParams;
pub type SessionResumeParams = SessionIdParams;
pub type SessionReadScreenParams = SessionIdParams;
pub type SessionReadScrollbackParams = SessionIdParams;
pub type SessionMarkSeenParams = SessionIdParams;
pub type SessionHibernateParams = SessionIdParams;
pub type SessionWakeParams = SessionIdParams;
pub type SessionArchiveParams = SessionIdParams;
pub type SessionUnarchiveParams = SessionIdParams;
pub type SessionRefParams = SessionIdParams;

pub type SessionKillResult = EmptyResult;
pub type SessionRemoveResult = EmptyResult;
pub type SessionResumeResult = SessionRecord;
pub type SessionMarkSeenResult = EmptyResult;
pub type SessionHibernateResult = EmptyResult;
pub type SessionWakeResult = EmptyResult;
pub type SessionArchiveResult = EmptyResult;
pub type SessionUnarchiveResult = EmptyResult;
pub type SessionReadScreenResult = ReadScreenResult;
pub type SessionReadScrollbackResult = ReadScrollbackResult;
pub type SessionReadScrollbackCellsResult = ReadScrollbackCellsResult;
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionDiffBase {
    #[default]
    DefaultBranch,
    Head,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReadDiffParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// Missing for older clients, and therefore defaults to the repository's
    /// primary branch rather than only the worktree delta from HEAD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<SessionDiffBase>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub kind: AgentKind,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub transcript_path: String,
    pub last_active_at: DateMillis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateMillis>,
    pub cwd_exists: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionHistoryResult {
    pub entries: Vec<HistoryEntry>,
}

pub type SessionHistoryParams = EmptyParams;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ResumeFromHistoryParams {
    pub entry: HistoryEntry,
}

pub type ResumeFromHistoryResult = SessionRecord;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRenameParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub title: String,
}

pub type SessionRenameResult = EmptyResult;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTextParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub text: String,
    pub submit: bool,
}

pub type SendTextResult = EmptyResult;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub cols: i64,
    pub rows: i64,
}

pub type ResizeResult = EmptyResult;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ClientRole {
    #[default]
    Desktop,
    Mobile,
    Unknown,
}

impl Serialize for ClientRole {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
            Self::Unknown => "unknown",
        })
    }
}

impl<'de> Deserialize<'de> for ClientRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "desktop" => Self::Desktop,
            "mobile" => Self::Mobile,
            _ => Self::Unknown,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ReadScreenResult {
    pub text: String,
    pub cols: i64,
    pub rows: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReadDiffResult {
    #[serde(with = "base64_bytes")]
    pub patch: Vec<u8>,
    pub repo_root: String,
    pub truncated: bool,
    /// The ref actually used (for example `origin/main` or `HEAD`). Older
    /// daemons omit this, so clients must treat it as advisory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScrollbackResult {
    pub lines: Vec<String>,
    pub first_row: i64,
    pub visible_start_row: i64,
    pub cols: i64,
    pub rows: i64,
    pub content_seq: u64,
    pub is_alt_screen: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScrollbackCellsParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub first_row: i64,
    pub max_rows: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScrollbackCellsResult {
    #[serde(with = "base64_bytes")]
    pub payload: Vec<u8>,
    pub first_row: i64,
    pub row_count: i64,
    pub total_rows: i64,
    pub live_start_row: i64,
    pub cols: i64,
    pub content_seq: u64,
}

pub type SessionReopenLastParams = EmptyParams;
pub type SessionReopenLastResult = SessionRecord;

/// `session.migrate`: one-click handoff of a live Claude session between local
/// and a remote host, preserving conversation context (`claude --resume`) and
/// code state (WIP commit + push + hard-sync of the target checkout).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMigrateParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// Target `HostEntry.id`; absent ⇒ migrate back to local.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_host: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMigrateResult {
    /// The migrated record (same id/title; host + cwd updated, respawned).
    pub session: SessionRecord,
    /// False when no transcript existed to shuttle: the session respawned
    /// with a fresh conversation — code state moved, context was lost.
    pub transcript_migrated: bool,
    /// Non-fatal issues (for example Holder cleanup or a missing transcript).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `host.sync_prefs`: push the local user's agent preferences to a host. The
/// include list is FIXED daemon-side and never contains credentials.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct HostSyncPrefsParams {
    /// `HostEntry.id` from `hosts.json`.
    pub host: String,
}

/// Per-tool outcome of a prefs sync. `ok` with an empty `synced` list means
/// the tool had nothing to push (no local config).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PrefsSyncToolReport {
    /// "claude" | "codex"
    pub tool: String,
    pub ok: bool,
    /// Item names that existed locally and were pushed (e.g. "CLAUDE.md").
    pub synced: Vec<String>,
    /// rsync/ssh failure detail; absent on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct HostSyncPrefsResult {
    pub tools: Vec<PrefsSyncToolReport>,
}

/// `host.initialize`: bootstrap and verify the exact packaged Remote Helper,
/// probe logout survival, and capture the remote account/cwd environment.
/// The result deliberately excludes environment values and authentication
/// diagnostics because both can contain secrets.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInitializeParams {
    /// `HostEntry.id` from `hosts.json`.
    pub host: String,
    /// Force the packaged artifact through upload and activation even when
    /// the exact Build ID already probes successfully. Activation remains
    /// content-addressed and never overwrites a different live build.
    #[serde(default)]
    pub force_reinstall: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInitializeResult {
    pub helper_build_id: String,
    pub protocol: crate::remote_pty::ProtocolVersion,
    pub persistence: crate::remote_pty::PersistenceCapability,
    /// Canonical absolute directory returned by the Helper for the configured
    /// default cwd (or the remote home when the host omitted one).
    pub cwd: String,
    /// Login shell selected from the remote account database.
    pub shell: String,
}

/// `host.list_directories`: list one directory level on the selected
/// execution host. `host = None` addresses the Engine's local machine.
///
/// The operation is deliberately shallow and bounded. It is the backend for
/// the New Agent folder picker, not a general remote filesystem protocol.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostListDirectoriesParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub path: String,
}

pub type HostListDirectoriesResult = crate::remote_pty::DirectoryListResult;

/// `host.locate_repo`: find a checkout of a repo on a host by origin URL.
/// Provide either `origin_url` directly, or `session_id` to derive the origin
/// from that session's checkout (cwd + host) — one round trip for pickers.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostLocateRepoParams {
    /// Target host id; absent ⇒ search the daemon's local project roots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(rename = "originURL", skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    #[serde(rename = "sessionID", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct HostLocateRepoResult {
    /// Absolute checkout path on the target host; absent when not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The origin URL that was matched (echoed, or derived from `session_id`).
    /// Absent ⇒ the session's cwd is not a git repo with an origin remote.
    #[serde(rename = "originURL", skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeCreateParams {
    pub repo_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

pub type WorktreeCreateResult = WorktreeInfo;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeListParams {
    pub repo_path: String,
}

pub type WorktreeListResult = Vec<WorktreeInfo>;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRemoveParams {
    pub repo_path: String,
    pub worktree_path: String,
    pub force: bool,
}

pub type WorktreeRemoveResult = EmptyResult;

pub type WorktreeOverviewParams = EmptyParams;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeOverviewEntry {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub project_root: String,
    #[serde(rename = "sessionID", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_status: Option<SessionStatus>,
    pub dirty: bool,
    pub merged: bool,
    pub age_days: i64,
    pub stale_suggestion: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorktreeOverviewResult {
    pub entries: Vec<WorktreeOverviewEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProjectAddParams {
    pub root: String,
}

pub type ProjectAddResult = Project;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TestRunParams {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engines: Option<Vec<String>>,
    pub steps: Vec<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<JsonValue>,
}

pub type TestRunResult = JsonValue;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsSubscribeParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
    /// Deliver only events tagged with one of these sessions. `None` ⇒ all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionId>>,
    /// Deliver only these event names. `None` ⇒ every kind the daemon
    /// publishes. Daemons predating server-side filtering ignore both fields
    /// and send everything, which is why the client must still tolerate names
    /// it did not ask for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EventsSubscribeResult {
    pub subscribed: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsWaitParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub until: Vec<String>,
    pub timeout_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsWaitResult {
    pub session: SessionRecord,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookReportParams {
    pub kind: String,
    #[serde(rename = "dirijorSessionID", skip_serializing_if = "Option::is_none")]
    pub dirijor_session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub payload: JsonValue,
}

pub type HookReportResult = EmptyResult;
pub type GovernorConfigureParams = GovernorSettingsParams;
pub type GovernorConfigureResult = EmptyResult;
pub type AgentReadinessParams = EmptyParams;
pub type ClientSetActiveResult = EmptyResult;
pub type DaemonPrepareShutdownParams = EmptyParams;
pub type DaemonPrepareShutdownResult = EmptyResult;
pub type DaemonShutdownParams = EmptyParams;
pub type DaemonShutdownResult = EmptyResult;

/// First JSON line on a binary session data channel.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachRequest {
    pub attach: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default)]
    pub role: ClientRole,
}

/// First JSON line on a raw TCP forwarding channel.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ForwardRequest {
    pub forward: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ForwardAck {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            encoded.push(ALPHABET[(first >> 2) as usize] as char);
            encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            encoded.push(if chunk.len() > 1 {
                ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
            } else {
                '='
            });
            encoded.push(if chunk.len() > 2 {
                ALPHABET[(third & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        decode(&encoded).map_err(de::Error::custom)
    }

    fn decode(encoded: &str) -> Result<Vec<u8>, &'static str> {
        let bytes = encoded.as_bytes();
        if !bytes.len().is_multiple_of(4) {
            return Err("base64 length must be a multiple of four");
        }
        let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
        for (index, chunk) in bytes.chunks_exact(4).enumerate() {
            let last = index + 1 == bytes.len() / 4;
            let a = value(chunk[0]).ok_or("invalid base64 character")?;
            let b = value(chunk[1]).ok_or("invalid base64 character")?;
            let c = if chunk[2] == b'=' {
                if !last || chunk[3] != b'=' {
                    return Err("invalid base64 padding");
                }
                0
            } else {
                value(chunk[2]).ok_or("invalid base64 character")?
            };
            let d = if chunk[3] == b'=' {
                if !last {
                    return Err("invalid base64 padding");
                }
                0
            } else {
                value(chunk[3]).ok_or("invalid base64 character")?
            };
            decoded.push((a << 2) | (b >> 4));
            if chunk[2] != b'=' {
                decoded.push((b << 4) | (c >> 2));
            }
            if chunk[3] != b'=' {
                decoded.push((c << 6) | d);
            }
        }
        Ok(decoded)
    }

    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
}
