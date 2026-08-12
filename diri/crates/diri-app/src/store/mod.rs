//! Headless application state and the thin asynchronous daemon adapter.

mod prefs;
mod projection;
mod residency;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diri_client::{ClientError, ConnectionState, DaemonClient, EventEnvelope};
use diri_proto::paths::DirijorPaths;
use diri_proto::remote_pty::DirectoryListResult;
use diri_proto::{
    AgentDescriptor, AgentKind, AgentReadinessResult, AttentionLevel, DateMillis, EventName,
    ExitReason, GovernorConfigureParams, HelloResult, HostEntry, HostsConfig, Project, ProjectId,
    Resumability, SessionId, SessionListResult, SessionRecord, SessionSpawnParams, SessionStatus,
};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::notifications::{
    PendingAttention, SendTextCommand, StatusTransition, attention_signal,
    immediate_transitions_for_update, migration_transition, prefs_sync_transition,
    reach_failure_transition, settled_attention_transition,
};
use crate::switcher::{
    OverviewArrow, OverviewFilter, OverviewMode, OverviewOutcome, SessionOverviewState,
    SessionSwitcherState, SwitcherKey, SwitcherOutcome,
};

pub use prefs::{InspectorTab, Prefs, WindowMode, WindowPlacement};
pub use projection::{SidebarProject, SidebarProjection, SidebarRow};
pub use residency::{ResidencyUpdate, TerminalResidency};

pub const AUXILIARY_TERMINAL_TITLE: &str = "Terminal";

fn agent_target_key(host: Option<&str>) -> String {
    host.unwrap_or("local").to_owned()
}

pub(crate) fn is_auxiliary_terminal(session: &SessionRecord) -> bool {
    // `parent` was previously written to the wire but had no UI semantics.
    // Shell children now belong to their parent's workbench. Do not key this
    // off the title: shells can update their title through terminal escape
    // sequences while they are running.
    session.kind == AgentKind::SHELL && session.parent.is_some()
}

// Session titles, badges, and sidebar metadata change at human speed. Publishing
// daemon bursts at display refresh rate rebuilt the whole sidebar 60 times/sec
// while several agents were working; terminal grids retain their independent
// 20fps direct path.
const UI_PUBLISH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoreEventChange {
    None,
    Resources,
    Model,
}

impl StoreEventChange {
    fn merge(self, other: Self) -> Self {
        if self == Self::Model || other == Self::Model {
            Self::Model
        } else if self == Self::Resources || other == Self::Resources {
            Self::Resources
        } else {
            Self::None
        }
    }
}

fn event_publication_policy(change: StoreEventChange, active: bool) -> (bool, bool) {
    let publish_snapshot = active || change == StoreEventChange::Model;
    let notify_views = active;
    (publish_snapshot, notify_views)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Connecting,
    Connected,
    Unreachable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StoreEffect {
    /// Repaint subscribers after a purely local navigation change.
    UiChanged,
    /// Push one fresh snapshot to watch subscribers. The menu-bar panel skips
    /// rebuilds while hidden, so opening it asks for a current snapshot.
    PublishSnapshot,
    MarkSeen(SessionId),
    Remove(SessionId),
    Resume {
        id: SessionId,
        automatic: bool,
    },
    Archive(SessionId),
    Unarchive(SessionId),
    Rename {
        id: SessionId,
        title: String,
    },
    Spawn(SessionSpawnParams),
    /// A shell owned by a workbench pane. Unlike a top-level spawn, its
    /// response must not replace the selected sidebar session.
    SpawnAuxiliary(SessionSpawnParams),
    /// Wake the client's idempotent reconnect loop out of backoff.
    RetryConnection,
    /// `session.migrate` — move a Claude session between local and a host.
    Migrate {
        id: SessionId,
        target_host: Option<String>,
    },
    /// `host.sync_prefs` — push agent preferences to a remote host.
    SyncPrefs {
        host: String,
        host_name: String,
    },
    /// `host.locate_repo` — resolve the reference session's repo on a host;
    /// the answer lands back in the store as a `RepoTarget`.
    LocateRepo {
        key: String,
        host: Option<String>,
        session_id: SessionId,
    },
    /// One bounded level for the New Agent folder picker. Results are keyed by
    /// generation so a slow host cannot overwrite a newer navigation click.
    ListDirectories {
        request_id: u64,
        host: Option<String>,
        path: String,
    },
    RefreshAgents {
        host: Option<String>,
        force: bool,
    },
    ConfigureAgent(diri_proto::AgentConfigureParams),
    ReopenLast,
    SetActive(bool),
    ConfigureGovernor(GovernorConfigureParams),
    /// T11 consumes this by closing and dropping the corresponding attachment.
    DetachAttachment(SessionId),
    /// T15 consumes this without involving daemon lifecycle operations.
    StatusTransition(StatusTransition),
}

#[derive(Clone, Debug, PartialEq)]
enum RetryAction {
    Rename { id: SessionId, title: String },
    SyncPrefs { host: String, host_name: String },
}

impl RetryAction {
    fn effect(self) -> StoreEffect {
        match self {
            Self::Rename { id, title } => StoreEffect::Rename { id, title },
            Self::SyncPrefs { host, host_name } => StoreEffect::SyncPrefs { host, host_name },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionFailure {
    pub title: String,
    pub detail: String,
    pub retrying: bool,
    retry: Option<RetryAction>,
}

impl ActionFailure {
    #[must_use]
    pub fn can_retry(&self) -> bool {
        self.retry.is_some()
    }

    #[cfg(test)]
    pub(crate) fn fixture(
        title: impl Into<String>,
        detail: impl Into<String>,
        can_retry: bool,
        retrying: bool,
    ) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            retrying,
            retry: can_retry.then(|| RetryAction::Rename {
                id: SessionId::new("fixture"),
                title: "fixture".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoreSnapshot {
    /// Shared, not owned: a snapshot publish happens up to every 50ms while
    /// events flow, and its consumers only read.
    pub sessions: Vec<Arc<SessionRecord>>,
    pub projects: Vec<Project>,
    pub selected_session_id: Option<SessionId>,
    pub global_attention: AttentionLevel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingClose {
    pub ids: Vec<SessionId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClickModifiers {
    pub command: bool,
    pub shift: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeSpawn {
    pub create: bool,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpawnOptions {
    pub cwd: Option<String>,
    pub worktree: Option<WorktreeSpawn>,
    pub title: Option<String>,
    pub initial_prompt: Option<String>,
    pub parent: Option<SessionId>,
    pub initial_cols: Option<i64>,
    pub initial_rows: Option<i64>,
    /// `HostEntry.id` from hosts.json — spawn on that remote host. When set,
    /// `cwd` (or the host's `defaultCwd`) is a REMOTE path, and worktree
    /// options are ignored (a local-git feature).
    pub host: Option<String>,
    /// Repo-preserving spawn: the daemon opens the session in the checkout of
    /// the SAME repository as this session on the target host (matched by
    /// origin URL), falling back to `cwd` / defaultCwd when not cloned there.
    pub same_repo_as: Option<SessionId>,
}

/// Async resolution state for "the active session's repo on host X" — drives
/// the new-agent popover's target path + fallback subtitle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoTarget {
    Pending,
    /// Absolute checkout path on the target host.
    Resolved(String),
    /// The repo has an origin but no clone on the target host — spawns fall
    /// back to the host's default directory (and the popover says so).
    NotCloned,
    /// The reference session isn't in a git repo with an origin (or the
    /// lookup failed): plain default-directory behavior, no subtitle.
    NoOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryListingState {
    Loading,
    Ready(DirectoryListResult),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryListing {
    pub request_id: u64,
    pub host: Option<String>,
    pub requested_path: String,
    pub state: DirectoryListingState,
}

fn repo_target_key(host: Option<&str>) -> String {
    host.unwrap_or("local").to_owned()
}

/// Pure application model. Side effects are emitted onto a channel for the daemon adapter.
pub struct SessionStore {
    daemon_state: DaemonState,
    daemon_identity: Option<HelloResult>,
    sessions: HashMap<SessionId, Arc<SessionRecord>>,
    projects: HashMap<ProjectId, Project>,
    selected_session_id: Option<SessionId>,
    sidebar_selection: HashSet<SessionId>,
    pending_close: Option<PendingClose>,
    /// Sessions with a `session.remove` in flight. The daemon terminates the
    /// process tree before it publishes `session.removed`, which takes upwards
    /// of half a second; rows hide the moment the close is dispatched so the
    /// click reads as done instead of inviting a second one.
    closing: HashSet<SessionId>,
    auto_resuming: HashSet<SessionId>,
    /// Sessions with a `session.migrate` in flight — rows render busy and
    /// migration actions are suppressed until the daemon answers.
    migrating: HashSet<SessionId>,
    /// Host ids with a `host.sync_prefs` in flight.
    syncing_prefs: HashSet<String>,
    /// Popover repo resolution: host key → state (see `RepoTarget`).
    repo_targets: HashMap<String, RepoTarget>,
    /// The session whose repo the popover preserves (selected at open time).
    repo_target_session: Option<SessionId>,
    directory_request_seq: u64,
    directory_listing: Option<DirectoryListing>,
    prefs: Prefs,
    terminal_residency: TerminalResidency,
    app_is_active: bool,
    last_action_failure: Option<ActionFailure>,
    sidebar_selection_anchor: Option<SessionId>,
    mru_order: Vec<SessionId>,
    switcher: SessionSwitcherState,
    overview: SessionOverviewState,
    auto_resume_attempted: HashSet<SessionId>,
    revision: u64,
    cached_projection: Option<(u64, Arc<SidebarProjection>)>,
    prefs_path: Option<PathBuf>,
    /// Remote host catalog from hosts.json. Empty when the file is absent or
    /// invalid (pickers show Local only). Reloaded on picker open.
    hosts: Vec<HostEntry>,
    /// Agent catalog from the daemon's `agent.readiness`: what agents exist,
    /// whether their CLI is installed, and each one's manifest descriptor.
    /// Empty until the first successful connect, and empty forever against a
    /// daemon too old to send descriptors — every reader must have a fallback.
    agents: HashMap<String, AgentReadinessResult>,
    agent_catalog_loading: HashSet<String>,
    agent_catalog_errors: HashMap<String, String>,
    /// Attention states serving out their settle window, newest arming wins.
    /// Drained by the settle task in `StoreHandle`, which is what turns one of
    /// these into a chime and a banner — see `drain_settled_attention`.
    attention_settle: HashMap<SessionId, PendingAttention>,
    /// Wakes the settle task when a window is armed early enough to beat the
    /// one it is currently sleeping on.
    attention_wake: Arc<Notify>,
    effects: mpsc::UnboundedSender<StoreEffect>,
}

impl SessionStore {
    pub fn headless(prefs: Prefs) -> (Self, mpsc::UnboundedReceiver<StoreEffect>) {
        Self::with_path(prefs, None)
    }

    pub fn load(
        path: impl Into<PathBuf>,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<StoreEffect>)> {
        let path = path.into();
        let prefs = Prefs::load(&path)?;
        let (mut store, receiver) = Self::with_path(prefs, Some(path));
        store.reload_hosts();
        Ok((store, receiver))
    }

    fn with_path(
        prefs: Prefs,
        prefs_path: Option<PathBuf>,
    ) -> (Self, mpsc::UnboundedReceiver<StoreEffect>) {
        let (effects, receiver) = mpsc::unbounded_channel();
        let selected_session_id = prefs.last_selected_session.clone();
        (
            Self {
                daemon_state: DaemonState::Connecting,
                daemon_identity: None,
                sessions: HashMap::new(),
                projects: HashMap::new(),
                selected_session_id: selected_session_id.clone(),
                sidebar_selection: HashSet::new(),
                pending_close: None,
                closing: HashSet::new(),
                auto_resuming: HashSet::new(),
                migrating: HashSet::new(),
                syncing_prefs: HashSet::new(),
                repo_targets: HashMap::new(),
                repo_target_session: None,
                directory_request_seq: 0,
                directory_listing: None,
                prefs,
                terminal_residency: TerminalResidency::default(),
                app_is_active: true,
                last_action_failure: None,
                sidebar_selection_anchor: None,
                mru_order: selected_session_id.into_iter().collect(),
                switcher: SessionSwitcherState::default(),
                overview: SessionOverviewState::default(),
                auto_resume_attempted: HashSet::new(),
                revision: 0,
                cached_projection: None,
                prefs_path,
                hosts: Vec::new(),
                agents: HashMap::new(),
                agent_catalog_loading: HashSet::new(),
                agent_catalog_errors: HashMap::new(),
                attention_settle: HashMap::new(),
                attention_wake: Arc::new(Notify::new()),
                effects,
            },
            receiver,
        )
    }

    /// Re-reads hosts.json (daemon-owned, same machine). Called at startup and
    /// whenever the new-agent picker opens so edits show up without a relaunch.
    pub fn reload_hosts(&mut self) {
        self.hosts = std::env::var_os("HOME")
            .map(|home| HostsConfig::load(DirijorPaths::hosts_config_file(home)).hosts)
            .unwrap_or_default();
        self.repair_default_spawn_host();
    }

    /// Installs the agent catalog fetched on connect.
    pub fn set_agent_catalog(&mut self, agents: AgentReadinessResult) {
        let local = agents.host.is_none();
        let key = agent_target_key(agents.host.as_deref());
        self.agent_catalog_loading.remove(&key);
        self.agent_catalog_errors.remove(&key);
        self.agents.insert(key, agents);
        // Preserve Main's repair policy for the local preference. A remote
        // target may legitimately have a different installed set and must not
        // rewrite the user's global default merely because one host lacks it.
        if local && let Some(catalog) = self.agent_catalog(None) {
            let repaired =
                crate::agent_catalog::resolved_default_agent(&self.prefs.default_agent, catalog);
            if repaired != self.prefs.default_agent {
                self.prefs.default_agent = repaired;
                let _ = self.persist_preferences();
                self.invalidate_projection();
            }
        }
    }

    pub fn agent_catalog(&self, host: Option<&str>) -> Option<&AgentReadinessResult> {
        self.agents.get(&agent_target_key(host))
    }

    pub fn agent_catalogs(&self) -> &HashMap<String, AgentReadinessResult> {
        &self.agents
    }

    pub fn request_agent_catalog(&mut self, host: Option<String>, force: bool) {
        let key = agent_target_key(host.as_deref());
        // A failed scan leaves the catalog absent, and the failure's change
        // broadcast re-renders the surfaces that ask for absent catalogs; a
        // non-forced request after an error would therefore retry in a loop
        // for as long as such a surface stays open. Only an explicit rescan
        // gets past a recorded failure.
        if !force && self.agent_catalog_errors.contains_key(&key) {
            return;
        }
        if self.agent_catalog_loading.insert(key) {
            self.emit(StoreEffect::RefreshAgents { host, force });
        }
    }

    pub fn configure_agent(&mut self, params: diri_proto::AgentConfigureParams) {
        // Configuration is a user mutation, not a cache refresh: it must reach
        // the engine even while a scan for the same target is in flight, or a
        // toggle flipped during a slow remote scan silently reverts.
        self.agent_catalog_loading
            .insert(agent_target_key(params.host.as_deref()));
        self.emit(StoreEffect::ConfigureAgent(params));
    }

    pub fn agent_catalog_is_loading(&self, host: Option<&str>) -> bool {
        self.agent_catalog_loading.contains(&agent_target_key(host))
    }

    pub fn agent_catalog_error(&self, host: Option<&str>) -> Option<&str> {
        self.agent_catalog_errors
            .get(&agent_target_key(host))
            .map(String::as_str)
    }

    /// Manifest descriptor for a kind, when the daemon shipped one.
    pub fn agent_descriptor(&self, kind: &AgentKind) -> Option<&AgentDescriptor> {
        self.agent_catalog(None)
            .and_then(|catalog| catalog.descriptor(kind))
            .or_else(|| {
                self.agents
                    .values()
                    .find_map(|catalog| catalog.descriptor(kind))
            })
    }

    /// Test/preview seam: inject a host catalog without touching the disk.
    pub fn set_hosts(&mut self, hosts: Vec<HostEntry>) {
        self.hosts = hosts;
        self.repair_default_spawn_host();
    }

    pub fn hosts(&self) -> &[HostEntry] {
        &self.hosts
    }

    pub fn host(&self, id: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|host| host.id == id)
    }

    /// Display name for a session's host badge; falls back to the raw id when
    /// the entry has since been removed from hosts.json.
    pub fn host_display_name(&self, id: &str) -> String {
        self.host(id)
            .map_or_else(|| id.to_owned(), |host| host.display_name().to_owned())
    }

    /// Host used by the global new-session shortcuts. `None` means this Mac.
    /// A removed host can never remain the effective default.
    pub fn default_spawn_host(&self) -> Option<String> {
        self.prefs
            .default_spawn_host
            .as_deref()
            .filter(|id| self.host(id).is_some())
            .map(str::to_owned)
    }

    /// Selects where global new-session shortcuts run and persists the choice.
    /// The new-agent picker calls this when its target changes, making the
    /// checkmarked machine and the shortcut destination one coherent state.
    pub fn set_default_spawn_host(&mut self, host: Option<String>) {
        let host = host.filter(|id| self.host(id).is_some());
        if self.prefs.default_spawn_host == host {
            return;
        }
        self.prefs.default_spawn_host = host;
        if let Err(error) = self.persist_preferences() {
            eprintln!("diri: could not save the default spawn host: {error}");
        }
    }

    fn repair_default_spawn_host(&mut self) {
        if self
            .prefs
            .default_spawn_host
            .as_deref()
            .is_some_and(|id| self.host(id).is_none())
        {
            self.prefs.default_spawn_host = None;
            if let Err(error) = self.persist_preferences() {
                eprintln!("diri: could not clear a removed default host: {error}");
            }
        }
    }

    pub fn load_default() -> io::Result<(Self, mpsc::UnboundedReceiver<StoreEffect>)> {
        Self::load(Prefs::path())
    }

    pub fn persist_preferences(&self) -> io::Result<()> {
        self.prefs_path
            .as_deref()
            .map_or(Ok(()), |path| self.prefs.save(path))
    }

    /// Update high-frequency window state in memory. The view debounces disk
    /// writes, and the application quit hook performs a final synchronous
    /// flush so the last resize or move cannot be lost.
    pub fn remember_window_placement(&mut self, placement: WindowPlacement) {
        self.prefs.window_placement = Some(placement);
    }

    pub fn daemon_state(&self) -> &DaemonState {
        &self.daemon_state
    }

    pub fn daemon_identity(&self) -> Option<&HelloResult> {
        self.daemon_identity.as_ref()
    }

    pub fn sessions(&self) -> &HashMap<SessionId, Arc<SessionRecord>> {
        &self.sessions
    }

    pub fn auxiliary_terminal_for(&self, parent: &SessionId) -> Option<Arc<SessionRecord>> {
        self.sessions
            .values()
            .filter(|session| {
                session.parent.as_ref() == Some(parent)
                    && is_auxiliary_terminal(session)
                    && !session.is_archived()
                    && !self.closing.contains(&session.id)
            })
            .max_by(|left, right| {
                left.created_at
                    .partial_cmp(&right.created_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }

    pub fn projects(&self) -> &HashMap<ProjectId, Project> {
        &self.projects
    }

    pub fn selected_session_id(&self) -> Option<&SessionId> {
        self.selected_session_id.as_ref()
    }

    pub fn sidebar_selection(&self) -> &HashSet<SessionId> {
        &self.sidebar_selection
    }

    pub fn pending_close(&self) -> Option<&PendingClose> {
        self.pending_close.as_ref()
    }

    pub fn auto_resuming(&self) -> &HashSet<SessionId> {
        &self.auto_resuming
    }

    pub fn migrating(&self) -> &HashSet<SessionId> {
        &self.migrating
    }

    pub fn syncing_prefs(&self) -> &HashSet<String> {
        &self.syncing_prefs
    }

    /// Kicks off a `session.migrate` for a Claude session. No-ops (rather than
    /// erroring) when the session isn't eligible or a move is already running.
    pub fn migrate_session(&mut self, id: SessionId, target_host: Option<String>) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if session.kind != AgentKind::CLAUDE_CODE
            || session.host == target_host
            || self.migrating.contains(&id)
        {
            return;
        }
        self.migrating.insert(id.clone());
        self.emit(StoreEffect::Migrate { id, target_host });
    }

    pub fn finish_migration(&mut self, id: &SessionId) {
        self.migrating.remove(id);
    }

    /// Kicks off a `host.sync_prefs` push; one in flight per host.
    pub fn sync_prefs(&mut self, host: String) {
        if self.syncing_prefs.contains(&host) {
            return;
        }
        let host_name = self.host_display_name(&host);
        self.syncing_prefs.insert(host.clone());
        self.emit(StoreEffect::SyncPrefs { host, host_name });
    }

    pub fn finish_prefs_sync(&mut self, host: &str) {
        self.syncing_prefs.remove(host);
    }

    /// Called when the new-agent popover opens: repo resolution restarts
    /// against the currently selected session, while the target defaults to
    /// the configured shortcut destination.
    pub fn begin_repo_targeting(&mut self) -> Option<String> {
        self.repo_targets.clear();
        self.repo_target_session = self.selected_session_id.clone();
        self.prefs
            .default_spawn_host
            .clone()
            .filter(|host| self.host(host).is_some())
    }

    /// Requests async resolution of the reference session's repo on `host`
    /// (None = local). Local→local needs no resolution (the popover already
    /// targets the active project), so it is skipped at the call site.
    pub fn request_repo_target(&mut self, host: Option<String>) {
        let Some(session_id) = self.repo_target_session.clone() else {
            return;
        };
        let key = repo_target_key(host.as_deref());
        if self.repo_targets.contains_key(&key) {
            return;
        }
        self.repo_targets.insert(key.clone(), RepoTarget::Pending);
        self.emit(StoreEffect::LocateRepo {
            key,
            host,
            session_id,
        });
    }

    pub fn repo_target(&self, host: Option<&str>) -> Option<&RepoTarget> {
        self.repo_targets.get(&repo_target_key(host))
    }

    pub fn request_directory_listing(&mut self, host: Option<String>, path: String) {
        self.directory_request_seq = self.directory_request_seq.wrapping_add(1);
        let request_id = self.directory_request_seq;
        self.directory_listing = Some(DirectoryListing {
            request_id,
            host: host.clone(),
            requested_path: path.clone(),
            state: DirectoryListingState::Loading,
        });
        self.emit(StoreEffect::ListDirectories {
            request_id,
            host,
            path,
        });
    }

    pub fn directory_listing(
        &self,
        host: Option<&str>,
        requested_path: &str,
    ) -> Option<&DirectoryListingState> {
        self.directory_listing
            .as_ref()
            .filter(|listing| {
                listing.host.as_deref() == host && listing.requested_path == requested_path
            })
            .map(|listing| &listing.state)
    }

    fn finish_directory_listing(
        &mut self,
        request_id: u64,
        result: Result<DirectoryListResult, String>,
    ) {
        let Some(listing) = self
            .directory_listing
            .as_mut()
            .filter(|listing| listing.request_id == request_id)
        else {
            return;
        };
        listing.state = match result {
            Ok(result) => DirectoryListingState::Ready(result),
            Err(error) => DirectoryListingState::Error(error),
        };
    }

    pub fn set_repo_target(&mut self, key: String, target: RepoTarget) {
        self.repo_targets.insert(key, target);
    }

    pub fn preferences(&self) -> &Prefs {
        &self.prefs
    }

    pub fn terminal_residency(&self) -> &TerminalResidency {
        &self.terminal_residency
    }

    pub fn app_is_active(&self) -> bool {
        self.app_is_active
    }

    pub fn last_action_error(&self) -> Option<&str> {
        self.last_action_failure
            .as_ref()
            .map(|failure| failure.detail.as_str())
    }

    pub fn action_failure(&self) -> Option<&ActionFailure> {
        self.last_action_failure.as_ref()
    }

    pub fn dismiss_action_failure(&mut self) {
        self.last_action_failure = None;
        self.emit(StoreEffect::UiChanged);
    }

    pub fn retry_last_action(&mut self) {
        let Some(failure) = self.last_action_failure.as_mut() else {
            return;
        };
        let Some(retry) = failure.retry.clone() else {
            return;
        };
        failure.retrying = true;
        self.emit(retry.effect());
    }

    pub fn retry_connection(&self) {
        self.emit(StoreEffect::RetryConnection);
    }

    pub fn switcher_state(&self) -> &SessionSwitcherState {
        &self.switcher
    }

    pub fn overview_state(&self) -> &SessionOverviewState {
        &self.overview
    }

    /// Ask the runtime for one immediate snapshot publish. Used when a passive
    /// consumer (the menu-bar panel) becomes visible and needs current data
    /// without waiting for the next daemon event.
    pub fn request_snapshot_publish(&mut self) {
        self.emit(StoreEffect::PublishSnapshot);
    }

    pub fn update_preferences(&mut self, update: impl FnOnce(&mut Prefs)) -> io::Result<()> {
        update(&mut self.prefs);
        self.prefs.normalize();
        self.invalidate_projection();
        self.persist_preferences()?;
        if matches!(self.daemon_state, DaemonState::Connected) {
            self.emit(StoreEffect::ConfigureGovernor(self.governor_settings()));
        }
        Ok(())
    }

    pub fn zoom_terminal(&mut self, delta: f32) -> io::Result<()> {
        self.prefs.zoom_terminal(delta);
        self.persist_preferences()
    }

    pub fn reset_terminal_zoom(&mut self) -> io::Result<()> {
        self.prefs.reset_terminal_zoom();
        self.persist_preferences()
    }

    /// Brings the persisted sidebar order back in line with what actually
    /// exists: dead ids are dropped, and anything new is appended.
    ///
    /// This is what makes the order a *total* one. Before it existed the
    /// comparator split rows into "manually ordered" and "everything else",
    /// which had two visible costs: a new session sorted into the middle of
    /// the list rather than the end, and a row dragged to the bottom of a
    /// group snapped back above its unordered siblings on the next frame.
    ///
    /// New ids are appended in arrival order — the same order the projection
    /// falls back to — so materialising the order never moves a single row.
    /// Runs on membership changes only, and reports whether it wrote anything.
    fn reconcile_sidebar_order(&mut self) -> bool {
        let live_sessions: HashSet<&SessionId> = self.sessions.keys().collect();
        let live_projects: HashSet<&ProjectId> = self.projects.keys().collect();
        let mut arrivals: Vec<(f64, &SessionId)> = self
            .sessions
            .values()
            .map(|session| (session.created_at.0, &session.id))
            .collect();
        arrivals.sort_by(|(left, left_id), (right, right_id)| {
            left.total_cmp(right)
                .then_with(|| left_id.0.cmp(&right_id.0))
        });

        // A project arrives with its oldest session, matching the projection.
        let mut project_arrivals: HashMap<&ProjectId, f64> = HashMap::new();
        for session in self.sessions.values() {
            let arrival = project_arrivals
                .entry(&session.project_id)
                .or_insert(f64::INFINITY);
            *arrival = arrival.min(session.created_at.0);
        }
        let mut projects: Vec<(f64, &ProjectId)> = self
            .projects
            .keys()
            .map(|id| {
                (
                    project_arrivals.get(id).copied().unwrap_or(f64::INFINITY),
                    id,
                )
            })
            .collect();
        projects.sort_by(|(left, left_id), (right, right_id)| {
            left.total_cmp(right)
                .then_with(|| left_id.0.cmp(&right_id.0))
        });

        let sessions = reconcile_order(
            &self.prefs.sidebar_session_order,
            &live_sessions,
            arrivals.into_iter().map(|(_, id)| id),
        );
        let ordered_projects = reconcile_order(
            &self.prefs.sidebar_project_order,
            &live_projects,
            projects.into_iter().map(|(_, id)| id),
        );
        // Collapse and pin state for rows that no longer exist would otherwise
        // accumulate in prefs.json forever and reattach to a recycled id.
        let collapsed_sessions =
            retain_live(&self.prefs.sidebar_collapsed_sessions, &live_sessions);
        let pinned_sessions = retain_live(&self.prefs.sidebar_pinned_sessions, &live_sessions);

        let mut changed = false;
        if let Some(order) = sessions {
            self.prefs.sidebar_session_order = order;
            changed = true;
        }
        if let Some(order) = ordered_projects {
            self.prefs.sidebar_project_order = order;
            changed = true;
        }
        if let Some(collapsed) = collapsed_sessions {
            self.prefs.sidebar_collapsed_sessions = collapsed;
            changed = true;
        }
        if let Some(pinned) = pinned_sessions {
            self.prefs.sidebar_pinned_sessions = pinned;
            changed = true;
        }
        if changed {
            self.invalidate_projection();
        }
        changed
    }

    /// [`Self::reconcile_sidebar_order`] plus the one prefs write it earns.
    /// Session membership changes at human speed, so this is not hot.
    fn sync_sidebar_order(&mut self) {
        if self.reconcile_sidebar_order() {
            let _ = self.persist_preferences();
        }
    }

    /// The manual session order, guaranteed to name every live session, for a
    /// caller about to move one row within it.
    pub fn sidebar_session_order(&mut self) -> Vec<SessionId> {
        self.reconcile_sidebar_order();
        self.prefs.sidebar_session_order.clone()
    }

    /// See [`Self::sidebar_session_order`].
    pub fn sidebar_project_order(&mut self) -> Vec<ProjectId> {
        self.reconcile_sidebar_order();
        self.prefs.sidebar_project_order.clone()
    }

    pub fn set_project_order(&mut self, order: Vec<ProjectId>) -> io::Result<()> {
        self.update_preferences(|prefs| prefs.sidebar_project_order = order)
    }

    pub fn set_session_order(&mut self, order: Vec<SessionId>) -> io::Result<()> {
        self.update_preferences(|prefs| prefs.sidebar_session_order = order)
    }

    /// In-memory reorder for live drag feedback. `drag_over` runs on every
    /// frame the pointer hovers a row, so the prefs file write is deferred to
    /// the drop ([`Self::persist_preferences`]) instead of happening per frame.
    /// Returns whether the order actually moved.
    pub fn stage_project_order(&mut self, order: Vec<ProjectId>) -> bool {
        if self.prefs.sidebar_project_order == order {
            return false;
        }
        self.prefs.sidebar_project_order = order;
        self.prefs.normalize();
        self.invalidate_projection();
        true
    }

    /// See [`Self::stage_project_order`].
    pub fn stage_session_order(&mut self, order: Vec<SessionId>) -> bool {
        if self.prefs.sidebar_session_order == order {
            return false;
        }
        self.prefs.sidebar_session_order = order;
        self.prefs.normalize();
        self.invalidate_projection();
        true
    }

    pub fn toggle_project_pin(&mut self, id: ProjectId) -> io::Result<()> {
        self.update_preferences(|prefs| toggle_vec_member(&mut prefs.sidebar_pinned_projects, id))
    }

    pub fn toggle_session_pin(&mut self, id: SessionId) -> io::Result<()> {
        self.update_preferences(|prefs| toggle_vec_member(&mut prefs.sidebar_pinned_sessions, id))
    }

    pub fn toggle_project_collapsed(&mut self, id: ProjectId) -> io::Result<()> {
        self.update_preferences(|prefs| {
            toggle_vec_member(&mut prefs.sidebar_collapsed_projects, id)
        })
    }

    /// Folds or unfolds a session's spawned children. Collapsing the ancestor
    /// of the current selection would hide it, so the selection moves up to
    /// the row doing the folding rather than disappearing under it.
    pub fn toggle_session_collapsed(&mut self, id: SessionId) -> io::Result<()> {
        let collapsing = !self.prefs.sidebar_collapsed_sessions.contains(&id);
        if collapsing
            && let Some(selected) = self.selected_session_id.clone()
            && self.is_descendant_of(&selected, &id)
        {
            self.select(id.clone());
        }
        self.update_preferences(|prefs| {
            toggle_vec_member(&mut prefs.sidebar_collapsed_sessions, id)
        })
    }

    fn is_descendant_of(&self, candidate: &SessionId, ancestor: &SessionId) -> bool {
        let mut seen = HashSet::from([candidate.clone()]);
        let mut cursor = self
            .sessions
            .get(candidate)
            .and_then(|session| session.parent.clone());
        while let Some(node) = cursor {
            if &node == ancestor {
                return true;
            }
            if !seen.insert(node.clone()) {
                return false;
            }
            cursor = self
                .sessions
                .get(&node)
                .and_then(|session| session.parent.clone());
        }
        false
    }

    pub fn toggle_archive_expanded(&mut self, id: ProjectId) -> io::Result<()> {
        let was_expanded = self.prefs.sidebar_expanded_archives.contains(&id);
        if was_expanded {
            self.sidebar_selection.retain(|session_id| {
                !self
                    .sessions
                    .get(session_id)
                    .is_some_and(|session| session.project_id == id && session.is_archived())
            });
        }
        self.update_preferences(|prefs| toggle_vec_member(&mut prefs.sidebar_expanded_archives, id))
    }

    pub fn governor_settings(&self) -> GovernorConfigureParams {
        GovernorConfigureParams::new(
            f64::from(self.prefs.hibernate_after_minutes) * 60.0,
            self.prefs
                .memory_hard_limit_gb
                .saturating_mul(1024 * 1024 * 1024),
        )
    }

    pub fn sidebar_projection(&mut self) -> Arc<SidebarProjection> {
        if let Some((revision, projection)) = &self.cached_projection
            && *revision == self.revision
        {
            return Arc::clone(projection);
        }
        let projection = Arc::new(projection::build_projection(
            &self.sessions,
            &self.projects,
            &self.prefs,
            self.selected_session_id.as_ref(),
            &self.closing,
        ));
        self.cached_projection = Some((self.revision, Arc::clone(&projection)));
        projection
    }

    pub fn ordered_sessions(&mut self) -> Vec<SessionRecord> {
        self.sidebar_projection()
            .ordered_sessions
            .iter()
            .map(|session| session.as_ref().clone())
            .collect()
    }

    pub fn selected_session(&self) -> Option<&SessionRecord> {
        self.selected_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .map(Arc::as_ref)
    }

    pub fn hydrate(&mut self, result: SessionListResult) {
        // A full resync is the authority: anything still listed here outlived
        // its close (daemon restart, failed terminate) and belongs on screen.
        self.closing.clear();
        self.sessions = result
            .sessions
            .into_iter()
            .map(|session| (session.id.clone(), Arc::new(session)))
            .collect();
        self.projects = result
            .projects
            .into_iter()
            .map(|project| (project.id.clone(), project))
            .collect();
        if self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| !self.sessions.contains_key(id))
        {
            self.selected_session_id = None;
        }
        self.invalidate_projection();
        self.sync_sidebar_order();
        self.auto_select_if_needed();
        // A restored selection did not travel through `focus_session`, so it
        // still needs terminal residency before the pane can attach.
        if let Some(id) = self.selected_session_id.clone()
            && self
                .sessions
                .get(&id)
                .is_some_and(|session| !session.is_archived())
            && !self.terminal_residency.contains(&id)
        {
            let update = self.terminal_residency.touch(id);
            if let Some(evicted) = update.evicted {
                self.emit(StoreEffect::DetachAttachment(evicted));
            }
        }
        self.auto_resume_selected_if_needed();
        self.reconcile_navigation();
    }

    /// Applies a daemon event and reports whether any UI-visible store state
    /// changed. The daemon can repeat snapshots and also carries events for
    /// direct consumers such as terminal attachments; neither should rebuild
    /// the sidebar.
    pub fn handle_event(&mut self, event: EventEnvelope) -> bool {
        self.handle_event_change(event) != StoreEventChange::None
    }

    fn handle_event_change(&mut self, event: EventEnvelope) -> StoreEventChange {
        match event.name.as_str() {
            EventName::SESSION_UPDATED => {
                if let Ok(session) = serde_json::from_value::<SessionRecord>(event.params) {
                    if self
                        .sessions
                        .get(&session.id)
                        .is_some_and(|existing| existing.as_ref() == &session)
                    {
                        return StoreEventChange::None;
                    }
                    self.upsert_session(session);
                    return StoreEventChange::Model;
                }
            }
            EventName::SESSION_RESOURCES => {
                if let Ok(resources) =
                    serde_json::from_value::<diri_proto::SessionResourcesEvent>(event.params)
                    && let Some(existing) = self.sessions.get_mut(&resources.id)
                {
                    let record = Arc::make_mut(existing);
                    let mut changed = false;
                    if let Some(memory) = resources.memory_bytes
                        && record.memory_bytes != Some(memory)
                    {
                        record.memory_bytes = Some(memory);
                        changed = true;
                    }
                    if let Some(ports) = resources.listening_ports
                        && record.listening_ports.as_deref() != Some(ports.as_slice())
                    {
                        record.listening_ports = Some(ports);
                        changed = true;
                    }
                    if let Some(artifacts) = resources.artifacts
                        && record.artifacts.as_deref() != Some(artifacts.as_slice())
                    {
                        record.artifacts = Some(artifacts);
                        changed = true;
                    }
                    if changed {
                        self.invalidate_projection();
                        return StoreEventChange::Resources;
                    }
                }
            }
            EventName::SESSION_REMOVED => {
                let id = event
                    .params
                    .get("id")
                    .or_else(|| event.params.get("sessionID"))
                    .and_then(|value| value.as_str())
                    .map(SessionId::new);
                if let Some(id) = id
                    && self.sessions.contains_key(&id)
                {
                    self.remove_session_record(&id);
                    return StoreEventChange::Model;
                }
            }
            EventName::PROJECT_UPDATED => {
                if let Ok(project) = serde_json::from_value::<Project>(event.params) {
                    if self.projects.get(&project.id) == Some(&project) {
                        return StoreEventChange::None;
                    }
                    self.projects.insert(project.id.clone(), project);
                    self.invalidate_projection();
                    self.sync_sidebar_order();
                    return StoreEventChange::Model;
                }
            }
            _ => {}
        }
        StoreEventChange::None
    }

    pub fn upsert_session(&mut self, session: SessionRecord) {
        let previous = self.sessions.get(&session.id).cloned();
        let is_new = previous.is_none();
        let id = session.id.clone();
        let transitions = immediate_transitions_for_update(
            previous.as_deref(),
            &session,
            self.prefs.status_sounds,
        );
        let attention = attention_signal(previous.as_deref(), &session);
        let arriving_attention = session.attention();
        let arriving_archived = session.is_archived();
        // Closing the tab also drops the Engine record and deletes the
        // session's output log, so it may only happen where nothing is lost.
        // A clean `exit 0` from something with no conversation to return to —
        // a shell — is that case. A crash, a signal (macOS memory pressure
        // kills agents with SIGTERM), or anything resumable stays listed with
        // its exit pill and Resume button: that is the whole point of deriving
        // resumability for exited sessions, and the scrollback is the only
        // record of what went wrong.
        let should_auto_close = !self.closing.contains(&id)
            && matches!(
                &session.status,
                SessionStatus::Exited(info)
                    if info.reason == ExitReason::Exited
                        && info.code == Some(0)
                        && session.resumability != Resumability::Resumable
            )
            && previous
                .as_deref()
                .is_none_or(|record| !matches!(record.status, SessionStatus::Exited(_)));
        self.sessions.insert(id.clone(), Arc::new(session));
        self.settle_attention(&id, attention, arriving_attention, arriving_archived);
        // Spawn selects the id before the authoritative record arrives, and
        // only focus_session grants terminal residency -- without this, a
        // session created from the UI stays "Preparing terminal" forever.
        if is_new
            && self.selected_session_id.as_ref() == Some(&id)
            && !arriving_archived
            && !self.terminal_residency.contains(&id)
        {
            let update = self.terminal_residency.touch(id.clone());
            if let Some(evicted) = update.evicted {
                self.emit(StoreEffect::DetachAttachment(evicted));
            }
        }
        self.invalidate_projection();
        // Only membership moves the order, and a session record is republished
        // on every title, status, and resource tick.
        if is_new {
            self.sync_sidebar_order();
        }
        for transition in transitions {
            self.emit(StoreEffect::StatusTransition(transition));
        }
        if should_auto_close {
            // Process exit is terminal UI state, not a historical tab. Hide
            // the row and detach its terminal immediately; `session.remove`
            // then clears the authoritative Engine record.
            self.remove_sessions(vec![id]);
            return;
        }
        self.auto_resume_if_needed(&id);
        if is_new {
            if self.selected_session_id.as_ref() == Some(&id) {
                self.focus_session(id);
            } else {
                self.auto_select_if_needed();
            }
        }
        self.reconcile_navigation();
    }

    /// Arms, replaces, or abandons a session's settle window after an update.
    ///
    /// A fresh signal always re-arms: the newest reason to interrupt is the one
    /// worth waiting on. Without a signal, a window whose state the session no
    /// longer holds is dropped here rather than at the deadline, so a blip that
    /// resolves immediately never even keeps the settle task awake.
    fn settle_attention(
        &mut self,
        id: &SessionId,
        signal: Option<AttentionLevel>,
        attention: AttentionLevel,
        archived: bool,
    ) {
        if let Some(level) = signal {
            self.attention_settle.insert(
                id.clone(),
                PendingAttention::armed_at(level, Instant::now()),
            );
            self.attention_wake.notify_one();
        } else if self
            .attention_settle
            .get(id)
            .is_some_and(|pending| archived || pending.level != attention)
        {
            self.attention_settle.remove(id);
        }
    }

    /// The earliest settle window still to run, for the task that sleeps on it.
    #[must_use]
    pub fn next_attention_deadline(&self) -> Option<Instant> {
        self.attention_settle
            .values()
            .map(|pending| pending.deadline)
            .min()
    }

    /// Handle to the signal raised whenever a settle window is armed.
    #[must_use]
    pub fn attention_wake(&self) -> Arc<Notify> {
        Arc::clone(&self.attention_wake)
    }

    /// Turns every expired settle window into the chime and banner it earned,
    /// judged against the session as it is now — not as it was when the window
    /// was armed. States that did not survive produce nothing.
    #[must_use]
    pub fn drain_settled_attention(&mut self, now: Instant) -> Vec<StatusTransition> {
        let due: Vec<SessionId> = self
            .attention_settle
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let mut transitions = Vec::with_capacity(due.len());
        for id in due {
            let Some(pending) = self.attention_settle.remove(&id) else {
                continue;
            };
            let Some(session) = self.sessions.get(&id) else {
                continue;
            };
            if let Some(transition) = settled_attention_transition(
                session,
                pending.level,
                self.selected_session_id.as_ref(),
                self.app_is_active,
                self.prefs.status_sounds,
                self.agent_descriptor(session.effective_kind()),
            ) {
                transitions.push(transition);
            }
        }
        transitions
    }

    pub fn remove_session_record(&mut self, id: &SessionId) {
        if self.selected_session_id.as_ref() == Some(id) {
            self.focus_neighbor(&HashSet::from([id.clone()]));
        }
        self.sessions.remove(id);
        self.attention_settle.remove(id);
        self.closing.remove(id);
        self.sidebar_selection.remove(id);
        self.mru_order.retain(|candidate| candidate != id);
        self.auto_resuming.remove(id);
        self.migrating.remove(id);
        if self.terminal_residency.remove(id) {
            self.emit(StoreEffect::DetachAttachment(id.clone()));
        }
        self.invalidate_projection();
        self.sync_sidebar_order();
        self.reconcile_navigation();
    }

    pub fn select(&mut self, id: SessionId) {
        if !self.sessions.contains_key(&id) {
            return;
        }
        self.sidebar_selection.clear();
        self.sidebar_selection_anchor = Some(id.clone());
        self.focus_session(id);
    }

    fn apply_spawn_result(&mut self, id: SessionId) {
        // session.updated and the spawn response travel on separate channels,
        // so either can arrive first. focus_session handles both orderings:
        // if the record is already present it grants terminal residency now;
        // otherwise upsert_session will finish that work when the event lands.
        self.focus_session(id);
    }

    pub fn sidebar_click(&mut self, id: SessionId, modifiers: ClickModifiers) {
        if !self.sessions.contains_key(&id) {
            return;
        }
        if modifiers.shift {
            let order = self.sidebar_visible_order();
            let Some(clicked) = order.iter().position(|candidate| candidate == &id) else {
                return;
            };
            let anchor = self
                .sidebar_selection_anchor
                .as_ref()
                .and_then(|anchor| order.iter().position(|candidate| candidate == anchor))
                .or_else(|| {
                    self.selected_session_id.as_ref().and_then(|selected| {
                        order.iter().position(|candidate| candidate == selected)
                    })
                })
                .unwrap_or(clicked);
            let range = anchor.min(clicked)..=anchor.max(clicked);
            self.sidebar_selection = order[range].iter().cloned().collect();
        } else if modifiers.command {
            if self.sidebar_selection.is_empty()
                && let Some(focused) = self.selected_session_id.clone()
                && focused != id
                && self.sessions.contains_key(&focused)
            {
                self.sidebar_selection.insert(focused);
            }
            if self.sidebar_selection.remove(&id) {
                if self.selected_session_id.as_ref() == Some(&id)
                    && let Some(next) = self.sidebar_selection_ordered().first().cloned()
                {
                    self.focus_session(next);
                }
            } else {
                self.sidebar_selection.insert(id.clone());
                self.sidebar_selection_anchor = Some(id);
            }
        } else {
            self.select(id);
        }
    }

    pub fn clear_sidebar_selection(&mut self) {
        self.sidebar_selection.clear();
    }

    pub fn sidebar_selection_ordered(&mut self) -> Vec<SessionId> {
        self.sidebar_projection()
            .display_order
            .iter()
            .filter(|id| self.sidebar_selection.contains(*id))
            .cloned()
            .collect()
    }

    pub fn focus_neighbor(&mut self, excluded: &HashSet<SessionId>) {
        let order: Vec<_> = self
            .sidebar_projection()
            .ordered_sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        let survivors: Vec<_> = order
            .iter()
            .filter(|id| !excluded.contains(*id))
            .cloned()
            .collect();
        let Some(current) = self.selected_session_id.clone() else {
            self.set_selected_survivor(survivors.first().cloned());
            return;
        };
        let Some(index) = order.iter().position(|id| id == &current) else {
            self.set_selected_survivor(survivors.first().cloned());
            return;
        };
        let project = self
            .sessions
            .get(&current)
            .map(|session| &session.project_id);
        let eligible = |id: &&SessionId| !excluded.contains(*id);
        let same_project = |id: &&SessionId| {
            eligible(id) && self.sessions.get(*id).map(|session| &session.project_id) == project
        };
        let next = order[index..]
            .iter()
            .find(same_project)
            .or_else(|| order[..index].iter().rev().find(same_project))
            .or_else(|| order[index..].iter().find(eligible))
            .or_else(|| order[..index].iter().rev().find(eligible))
            .cloned()
            .or_else(|| survivors.first().cloned());
        self.set_selected_survivor(next);
    }

    pub fn mru_sessions(&mut self) -> Vec<SessionId> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for id in &self.mru_order {
            if self.sessions.contains_key(id) && seen.insert(id.clone()) {
                result.push(id.clone());
            }
        }
        for session in &self.sidebar_projection().ordered_sessions {
            if seen.insert(session.id.clone()) {
                result.push(session.id.clone());
            }
        }
        result
    }

    /// Opens or advances the Ctrl-Tab switcher. Ctrl-Tab is consumed even when
    /// there is only one session, matching the Swift event monitor and keeping
    /// the chord out of the terminal.
    pub fn handle_switcher_key(&mut self, key: SwitcherKey) -> bool {
        let order = if matches!(key, SwitcherKey::Tab { control: true, .. })
            && !self.switcher.is_visible()
        {
            self.mru_sessions()
        } else {
            Vec::new()
        };
        let outcome = self.switcher.key_down(key, &order);
        let consumed = outcome.consumed();
        self.apply_switcher_outcome(outcome);
        consumed
    }

    /// Returns false because the modifier event itself remains available to
    /// GPUI; releasing Ctrl still commits the highlighted session.
    pub fn handle_switcher_modifiers_changed(&mut self, control_held: bool) -> bool {
        let outcome = self.switcher.modifiers_changed(control_held);
        self.apply_switcher_outcome(outcome);
        false
    }

    pub fn commit_switcher_index(&mut self, index: usize) {
        if let Some(id) = self.switcher.commit_index(index) {
            self.select(id);
        }
    }

    pub fn cancel_switcher(&mut self) {
        self.switcher.cancel();
    }

    pub fn toggle_overview(&mut self) {
        let sessions = self.ordered_sessions();
        self.overview.toggle(&sessions);
        if self.overview.is_visible() {
            self.switcher.cancel();
        }
    }

    pub fn dismiss_overview(&mut self) {
        self.overview.dismiss();
    }

    pub fn set_overview_mode(&mut self, mode: OverviewMode) {
        let sessions = self.ordered_sessions();
        self.overview.set_mode(mode, &sessions);
    }

    pub fn set_overview_filter(&mut self, filter: OverviewFilter) {
        let sessions = self.ordered_sessions();
        self.overview.set_filter(filter, &sessions);
    }

    pub fn append_overview_query(&mut self, text: &str) -> bool {
        let sessions = self.ordered_sessions();
        self.overview.append_query(text, &sessions)
    }

    pub fn overview_backspace(&mut self) -> bool {
        let sessions = self.ordered_sessions();
        let outcome = self.overview.backspace(&sessions);
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn overview_escape(&mut self) -> bool {
        let sessions = self.ordered_sessions();
        let outcome = self.overview.escape(&sessions);
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn move_overview_focus(&mut self, arrow: OverviewArrow) -> bool {
        let sessions = self.ordered_sessions();
        self.overview.move_focus(arrow, &sessions)
    }

    pub fn activate_overview_focus(&mut self) -> bool {
        let outcome = self.overview.activate_focused();
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn activate_overview_session(&mut self, id: SessionId) {
        let outcome = self.overview.activate(id);
        self.apply_overview_outcome(outcome);
    }

    pub fn toggle_overview_selection(&mut self, id: SessionId) {
        if self.sessions.contains_key(&id) {
            self.overview.toggle_selection(id);
        }
    }

    pub fn clear_overview_selection(&mut self) {
        self.overview.clear_selection();
    }

    pub fn select_all_overview_sessions(&mut self) {
        let sessions = self.ordered_sessions();
        self.overview.select_all_visible(&sessions);
    }

    pub fn close_overview_selection(&mut self) -> bool {
        let outcome = self.overview.close_selected();
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn close_overview_session(&mut self, id: SessionId) {
        let outcome = self.overview.close_one(id);
        self.apply_overview_outcome(outcome);
    }

    pub fn global_attention(&self) -> AttentionLevel {
        self.sessions
            .values()
            .fold(AttentionLevel::None, |rollup, session| {
                let attention = session.attention();
                if attention_rank(&attention) > attention_rank(&rollup) {
                    attention
                } else {
                    rollup
                }
            })
    }

    pub fn needs_input_sessions(&self) -> Vec<SessionRecord> {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|session| session.attention() == AttentionLevel::NeedsInput)
            .map(|session| session.as_ref().clone())
            .collect();
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .partial_cmp(&left.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        sessions
    }

    pub fn snapshot(&self) -> StoreSnapshot {
        let mut sessions: Vec<_> = self.sessions.values().map(Arc::clone).collect();
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .partial_cmp(&left.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        let mut projects: Vec<_> = self.projects.values().cloned().collect();
        projects.sort_by(|left, right| left.name.cmp(&right.name));
        StoreSnapshot {
            sessions,
            projects,
            selected_session_id: self.selected_session_id.clone(),
            global_attention: self.global_attention(),
        }
    }

    pub fn request_close(&mut self, ids: Vec<SessionId>) {
        // Rows already on their way out ignore further clicks: without this a
        // second ✕ re-arms the confirmation for a session that is gone.
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| !self.closing.contains(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        let has_running = ids.iter().any(|id| {
            self.sessions
                .get(id)
                .is_some_and(|session| !matches!(session.status, SessionStatus::Exited(_)))
        });
        if self.prefs.confirm_before_closing_session && has_running {
            self.pending_close = Some(PendingClose { ids });
        } else {
            self.remove_sessions(ids);
        }
    }

    pub fn confirm_pending_close(&mut self) {
        if let Some(pending) = self.pending_close.take() {
            self.remove_sessions(pending.ids);
        }
    }

    pub fn cancel_pending_close(&mut self) {
        self.pending_close = None;
    }

    pub fn remove_sessions(&mut self, ids: Vec<SessionId>) {
        let mut ids = ids;
        let parents: HashSet<_> = ids.iter().cloned().collect();
        ids.extend(
            self.sessions
                .values()
                .filter(|session| {
                    session
                        .parent
                        .as_ref()
                        .is_some_and(|parent| parents.contains(parent))
                        && is_auxiliary_terminal(session)
                })
                .map(|session| session.id.clone()),
        );
        let mut unique = HashSet::new();
        ids.retain(|id| unique.insert(id.clone()));
        let excluded: HashSet<_> = ids.iter().cloned().collect();
        if self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| excluded.contains(id))
        {
            self.focus_neighbor(&excluded);
        }
        for id in ids {
            self.sidebar_selection.remove(&id);
            if self.terminal_residency.remove(&id) {
                self.emit(StoreEffect::DetachAttachment(id.clone()));
            }
            // Hide now; `session.removed` (or a resync) settles the record.
            self.closing.insert(id.clone());
            self.emit(StoreEffect::Remove(id));
        }
        self.invalidate_projection();
        self.reconcile_navigation();
    }

    pub fn archive_sessions(&mut self, ids: Vec<SessionId>) {
        let targets: Vec<_> = ids
            .into_iter()
            .filter(|id| {
                self.sessions
                    .get(id)
                    .is_some_and(|session| !session.is_archived())
            })
            .collect();
        let excluded: HashSet<_> = targets.iter().cloned().collect();
        if self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| excluded.contains(id))
        {
            self.focus_neighbor(&excluded);
        }
        for id in targets {
            self.sidebar_selection.remove(&id);
            if let Some(session) = self.sessions.get_mut(&id) {
                Arc::make_mut(session).archived_at = Some(now_millis());
            }
            if self.terminal_residency.remove(&id) {
                self.emit(StoreEffect::DetachAttachment(id.clone()));
            }
            self.emit(StoreEffect::Archive(id));
        }
        self.invalidate_projection();
    }

    pub fn revive_sessions(&mut self, ids: Vec<SessionId>) {
        let mut revived = Vec::new();
        for id in ids {
            let Some(session) = self.sessions.get_mut(&id) else {
                continue;
            };
            let session = Arc::make_mut(session);
            if !session.is_archived() {
                continue;
            }
            session.archived_at = None;
            let resumable = session.resumability == Resumability::Resumable;
            revived.push(id.clone());
            self.emit(if resumable {
                StoreEffect::Resume {
                    id,
                    automatic: false,
                }
            } else {
                StoreEffect::Unarchive(id)
            });
        }
        self.invalidate_projection();
        if let Some(first) = revived.first().cloned() {
            self.select(first);
        }
    }

    pub fn auto_resume_if_needed(&mut self, id: &SessionId) -> bool {
        let eligible = self.selected_session_id.as_ref() == Some(id)
            && self.sessions.get(id).is_some_and(|session| {
                !session.is_archived()
                    && session.resumability == Resumability::Resumable
                    && matches!(
                        &session.status,
                        SessionStatus::Exited(info) if info.reason == ExitReason::DaemonRestart
                    )
            });
        if !eligible || !self.auto_resume_attempted.insert(id.clone()) {
            return false;
        }
        self.auto_resuming.insert(id.clone());
        self.emit(StoreEffect::Resume {
            id: id.clone(),
            automatic: true,
        });
        true
    }

    pub fn finish_auto_resume(&mut self, id: &SessionId) {
        self.auto_resuming.remove(id);
    }

    pub fn resume(&self, id: SessionId) {
        self.emit(StoreEffect::Resume {
            id,
            automatic: false,
        });
    }

    pub fn rename(&mut self, id: SessionId, title: impl Into<String>) {
        let title = title.into();
        if let Some(session) = self.sessions.get_mut(&id) {
            let session = Arc::make_mut(session);
            session.title.clone_from(&title);
            session.title_source = diri_proto::TitleSource::UserRename;
            self.invalidate_projection();
        }
        self.emit(StoreEffect::Rename { id, title });
    }

    pub fn reopen_last(&self) {
        self.emit(StoreEffect::ReopenLast);
    }

    pub fn spawn_default(&mut self, mut options: SpawnOptions) {
        if options.host.is_none() && options.cwd.is_none() && options.same_repo_as.is_none() {
            options.host = self.default_spawn_host();
        }
        self.spawn_kind(self.prefs.default_agent.clone(), options);
    }

    pub fn spawn_shell(&mut self, mut options: SpawnOptions) {
        if options.host.is_none() && options.cwd.is_none() && options.same_repo_as.is_none() {
            options.host = self.default_spawn_host();
        }
        self.spawn_kind(AgentKind::SHELL, options);
    }

    /// Creates the shell shown by the lower workbench pane. It inherits the
    /// primary session's execution context and is deliberately not selected:
    /// pane focus is local UI state, while sidebar selection remains on the
    /// owning agent.
    pub fn spawn_auxiliary_terminal(&mut self, parent: SessionId) -> bool {
        let Some(session) = self.sessions.get(&parent) else {
            return false;
        };
        if self.auxiliary_terminal_for(&parent).is_some() {
            return false;
        }
        self.last_action_failure = None;
        self.emit(StoreEffect::SpawnAuxiliary(SessionSpawnParams {
            kind: AgentKind::SHELL,
            cwd: session.cwd.clone(),
            new_worktree: None,
            worktree_branch: None,
            title: Some(AUXILIARY_TERMINAL_TITLE.to_owned()),
            initial_prompt: None,
            parent: Some(parent),
            initial_cols: None,
            initial_rows: None,
            host: session.host.clone(),
            same_repo_as: None,
        }));
        true
    }

    pub fn spawn_kind(&mut self, kind: AgentKind, options: SpawnOptions) {
        let host = options.host;
        let cwd = if let Some(host_id) = &host {
            // Remote spawn: local directories are meaningless — use the
            // explicit remote override or the host's default cwd.
            options
                .cwd
                .or_else(|| self.host(host_id).and_then(|host| host.default_cwd.clone()))
                .unwrap_or_else(|| "~".to_owned())
        } else {
            options.cwd.unwrap_or_else(|| self.active_directory())
        };
        // Worktrees are a local-git feature; drop them for remote spawns (the
        // daemon rejects the combination outright).
        let worktree = if host.is_some() {
            None
        } else {
            options.worktree
        };
        let (new_worktree, worktree_branch) = worktree.map_or((None, None), |worktree| {
            (Some(worktree.create), worktree.branch)
        });
        self.emit(StoreEffect::Spawn(SessionSpawnParams {
            kind,
            cwd,
            new_worktree,
            worktree_branch,
            title: options.title,
            initial_prompt: options.initial_prompt,
            parent: options.parent,
            initial_cols: options.initial_cols,
            initial_rows: options.initial_rows,
            host,
            same_repo_as: options.same_repo_as,
        }));
    }

    /// Fallback cwd for spawning LOCALLY while a REMOTE session is active:
    /// its remote cwd is useless as a local path, so prefer the first project
    /// root that exists on this machine, then home.
    pub fn local_fallback_directory(&self) -> String {
        if self
            .selected_session()
            .is_none_or(|session| session.host.is_none())
        {
            return self.default_new_agent_directory();
        }
        let mut roots: Vec<_> = self
            .projects
            .values()
            .map(|project| project.root.clone())
            .filter(|root| Path::new(root).is_dir())
            .collect();
        roots.sort();
        roots
            .into_iter()
            .next()
            .or_else(|| std::env::var("HOME").ok())
            .unwrap_or_else(|| "/".to_owned())
    }

    /// Target for a new agent opened from the top-level "New Agent" control:
    /// the repo root of the active project, never the selected session's
    /// worktree cwd (⌘T should default to the main checkout).
    pub fn default_new_agent_directory(&self) -> String {
        if let Some(session) = self.selected_session()
            && let Some(project) = self.projects.get(&session.project_id)
        {
            return project.root.clone();
        }
        self.active_directory()
    }

    pub fn active_directory(&self) -> String {
        if let Some(session) = self.selected_session() {
            return session.cwd.clone();
        }
        let projection = projection::build_projection(
            &self.sessions,
            &self.projects,
            &self.prefs,
            self.selected_session_id.as_ref(),
            &self.closing,
        );
        projection
            .first_active()
            .map(|session| session.cwd.clone())
            .or_else(|| {
                projection
                    .projects
                    .first()
                    .map(|group| group.project.root.clone())
            })
            .or_else(|| std::env::var("HOME").ok())
            .unwrap_or_else(|| "/".to_owned())
    }

    pub fn set_active(&mut self, active: bool) {
        if self.app_is_active == active {
            return;
        }
        self.app_is_active = active;
        self.emit(StoreEffect::SetActive(active));
    }

    fn focus_session(&mut self, id: SessionId) {
        let selection_changed = self.selected_session_id.as_ref() != Some(&id);
        self.selected_session_id = Some(id.clone());
        let revealed = self.reveal(&id);
        if selection_changed || revealed || self.prefs.last_selected_session.as_ref() != Some(&id) {
            self.prefs.last_selected_session = Some(id.clone());
            if let Err(error) = self.persist_preferences() {
                eprintln!("diri: could not remember the selected session: {error}");
            }
        }
        self.mru_order.retain(|candidate| candidate != &id);
        self.mru_order.insert(0, id.clone());
        self.invalidate_projection();
        if self
            .sessions
            .get(&id)
            .is_some_and(|session| !session.is_archived())
        {
            let update = self.terminal_residency.touch(id.clone());
            if let Some(evicted) = update.evicted {
                self.emit(StoreEffect::DetachAttachment(evicted));
            }
        }
        if self
            .sessions
            .get(&id)
            .is_some_and(|session| !session.is_archived())
        {
            // Selection is also the PR/artifact visibility signal. The
            // daemon uses mark_seen to wake a fresh foreground refresh, even
            // when there was no unseen completion to acknowledge.
            self.emit(StoreEffect::MarkSeen(id.clone()));
        }
        self.auto_resume_if_needed(&id);
        self.emit(StoreEffect::UiChanged);
    }

    fn set_selected_survivor(&mut self, survivor: Option<SessionId>) {
        if let Some(id) = survivor {
            self.focus_session(id);
        } else {
            self.selected_session_id = None;
            if self.prefs.last_selected_session.take().is_some()
                && let Err(error) = self.persist_preferences()
            {
                eprintln!("diri: could not clear the selected session: {error}");
            }
            self.invalidate_projection();
        }
    }

    fn auto_select_if_needed(&mut self) {
        if self.selected_session_id.is_some() {
            return;
        }
        let first = self
            .sidebar_projection()
            .first_active()
            .map(|session| session.id.clone());
        self.set_selected_survivor(first);
    }

    fn auto_resume_selected_if_needed(&mut self) {
        let Some(id) = self.selected_session_id.clone() else {
            return;
        };
        self.auto_resume_if_needed(&id);
    }

    fn apply_switcher_outcome(&mut self, outcome: SwitcherOutcome) {
        if let SwitcherOutcome::Committed(id) = outcome {
            self.select(id);
        }
    }

    fn apply_overview_outcome(&mut self, outcome: OverviewOutcome) {
        match outcome {
            OverviewOutcome::Activate(id) => self.select(id),
            OverviewOutcome::RequestClose(ids) => self.request_close(ids),
            OverviewOutcome::Ignored | OverviewOutcome::Changed | OverviewOutcome::Dismissed => {}
        }
    }

    fn reconcile_navigation(&mut self) {
        let live_ids: HashSet<_> = self.sessions.keys().cloned().collect();
        self.switcher.reconcile(&live_ids);
        let sessions = self.ordered_sessions();
        self.overview.reconcile(&sessions);
    }

    /// Unfolds whatever is hiding `id` so the selected row is on screen: its
    /// project, every session up its spawn chain, and the archive bucket when
    /// the row is parked. Reports whether anything actually moved.
    ///
    /// This is the invariant that keeps collapse honest. Without it ⌘J, the
    /// switcher, and an MCP focus can all land on a row the user cannot see,
    /// and the sidebar shows no selection at all while the workbench shows a
    /// session — which reads as the app losing track of itself.
    fn reveal(&mut self, id: &SessionId) -> bool {
        let Some(session) = self.sessions.get(id).cloned() else {
            return false;
        };
        let mut changed = false;
        let project = session.project_id.clone();
        if let Some(index) = self
            .prefs
            .sidebar_collapsed_projects
            .iter()
            .position(|candidate| candidate == &project)
        {
            self.prefs.sidebar_collapsed_projects.remove(index);
            changed = true;
        }
        if session.is_archived() && !self.prefs.sidebar_expanded_archives.contains(&project) {
            self.prefs.sidebar_expanded_archives.push(project);
            changed = true;
        }
        // Walk up the spawn chain. `seen` guards against a cycle the daemon
        // should never produce but which would spin here if it did.
        let mut seen = HashSet::from([id.clone()]);
        let mut cursor = session.parent.clone();
        while let Some(ancestor) = cursor {
            if !seen.insert(ancestor.clone()) {
                break;
            }
            if let Some(index) = self
                .prefs
                .sidebar_collapsed_sessions
                .iter()
                .position(|candidate| candidate == &ancestor)
            {
                self.prefs.sidebar_collapsed_sessions.remove(index);
                changed = true;
            }
            cursor = self
                .sessions
                .get(&ancestor)
                .and_then(|record| record.parent.clone());
        }
        if changed {
            self.invalidate_projection();
        }
        changed
    }

    fn sidebar_visible_order(&mut self) -> Vec<SessionId> {
        let expanded: HashSet<_> = self
            .prefs
            .sidebar_expanded_archives
            .iter()
            .cloned()
            .collect();
        self.sidebar_projection()
            .projects
            .iter()
            .flat_map(|group| {
                group.sessions.iter().map(|row| row.id().clone()).chain(
                    group
                        .archived
                        .iter()
                        .filter(|_| expanded.contains(&group.project.id))
                        .map(|session| session.id.clone()),
                )
            })
            .collect()
    }

    fn invalidate_projection(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.cached_projection = None;
    }

    fn emit(&self, effect: StoreEffect) {
        let _ = self.effects.send(effect);
    }
}

fn attention_rank(attention: &AttentionLevel) -> u8 {
    match attention {
        AttentionLevel::None | AttentionLevel::Unknown => 0,
        AttentionLevel::IdleSeen => 1,
        AttentionLevel::Working => 2,
        AttentionLevel::DoneUnseen => 3,
        AttentionLevel::NeedsInput => 4,
    }
}

/// Drops ids that no longer exist and appends the ones the order has not seen,
/// in `arriving` order. Returns `None` when the order already agreed, so a
/// caller can skip a prefs write on the overwhelmingly common no-op.
fn reconcile_order<'a, T>(
    order: &[T],
    live: &HashSet<&T>,
    arriving: impl Iterator<Item = &'a T>,
) -> Option<Vec<T>>
where
    T: Clone + Eq + std::hash::Hash + 'a,
{
    let mut next: Vec<T> = order
        .iter()
        .filter(|id| live.contains(id))
        .cloned()
        .collect();
    let known: HashSet<&T> = order.iter().collect();
    let appended: Vec<T> = arriving
        .filter(|id| !known.contains(*id))
        .cloned()
        .collect();
    if next.len() == order.len() && appended.is_empty() {
        return None;
    }
    next.extend(appended);
    Some(next)
}

/// Prunes ids that no longer exist, or `None` when there was nothing to prune.
fn retain_live<T: Clone + Eq + std::hash::Hash>(
    values: &[T],
    live: &HashSet<&T>,
) -> Option<Vec<T>> {
    let kept: Vec<T> = values
        .iter()
        .filter(|id| live.contains(id))
        .cloned()
        .collect();
    (kept.len() != values.len()).then_some(kept)
}

fn toggle_vec_member<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if let Some(index) = values.iter().position(|candidate| candidate == &value) {
        values.remove(index);
    } else {
        values.push(value);
    }
}

fn now_millis() -> DateMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
    DateMillis(millis)
}

/// Running daemon bridge. It owns only tasks and shared channels; UI state remains in `SessionStore`.
pub struct StoreRuntime {
    pub store: Arc<RwLock<SessionStore>>,
    client: Arc<DaemonClient>,
    detach_tx: broadcast::Sender<SessionId>,
    change_tx: broadcast::Sender<()>,
    status_tx: broadcast::Sender<StatusTransition>,
    snapshot_tx: tokio::sync::watch::Sender<StoreSnapshot>,
    action_tx: mpsc::UnboundedSender<SendTextCommand>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl StoreRuntime {
    pub fn start(client: Arc<DaemonClient>, prefs_path: impl Into<PathBuf>) -> io::Result<Self> {
        let (store, effects) = SessionStore::load(prefs_path)?;
        Ok(Self::start_with_store(client, store, effects))
    }

    pub fn start_default(client: Arc<DaemonClient>) -> io::Result<Self> {
        Self::start(client, Prefs::path())
    }

    /// A task-free runtime for deterministic previews. The preview sidebar
    /// owns its fixture store; this bridge exists only to satisfy the shared
    /// application-service interface without connecting to the real daemon.
    pub fn inert() -> Self {
        let (store, _effects) = SessionStore::headless(Prefs::default());
        let store = Arc::new(RwLock::new(store));
        let (detach_tx, _) = broadcast::channel(1);
        let (change_tx, _) = broadcast::channel(1);
        let (status_tx, _) = broadcast::channel(1);
        let snapshot = store
            .read()
            .expect("session store lock poisoned")
            .snapshot();
        let (snapshot_tx, _) = tokio::sync::watch::channel(snapshot);
        let (action_tx, _action_rx) = mpsc::unbounded_channel();
        Self {
            store,
            client: Arc::new(DaemonClient::new()),
            detach_tx,
            change_tx,
            status_tx,
            snapshot_tx,
            action_tx,
            tasks: Mutex::new(Vec::new()),
        }
    }

    fn start_with_store(
        client: Arc<DaemonClient>,
        store: SessionStore,
        effects: mpsc::UnboundedReceiver<StoreEffect>,
    ) -> Self {
        let store = Arc::new(RwLock::new(store));
        let (detach_tx, _) = broadcast::channel(16);
        let (change_tx, _) = broadcast::channel(128);
        let (status_tx, _) = broadcast::channel(32);
        let initial_snapshot = store
            .read()
            .expect("session store lock poisoned")
            .snapshot();
        let (snapshot_tx, _) = tokio::sync::watch::channel(initial_snapshot);
        let (action_tx, mut action_rx) = mpsc::unbounded_channel::<SendTextCommand>();
        let mut tasks = Vec::new();

        let (event_publish_tx, mut event_publish_rx) = mpsc::channel::<StoreEventChange>(128);
        let publish_store = Arc::clone(&store);
        let publish_changes = change_tx.clone();
        let publish_snapshots = snapshot_tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(mut change) = event_publish_rx.recv().await {
                // Apply every daemon event immediately, but collapse bursts
                // into one UI/menu publication per display interval. Terminal
                // grid chunks use their own direct path and are unaffected.
                tokio::time::sleep(UI_PUBLISH_INTERVAL).await;
                while let Ok(next) = event_publish_rx.try_recv() {
                    change = change.merge(next);
                }
                let (active, snapshot) = {
                    let store = publish_store.read().expect("session store lock poisoned");
                    (store.app_is_active, store.snapshot())
                };
                // Full model changes still update the menu-bar snapshot while
                // backgrounded. Resource samples are memory-only until the UI
                // is active again, and neither wakes GPUI views in background.
                let (publish_snapshot, notify_views) = event_publication_policy(change, active);
                if publish_snapshot {
                    publish_snapshots.send_replace(snapshot);
                }
                if notify_views {
                    let _ = publish_changes.send(());
                }
            }
        }));

        let mut events = client.events();
        let event_store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let changed = event_store
                            .write()
                            .expect("session store lock poisoned")
                            .handle_event_change(event);
                        if changed != StoreEventChange::None {
                            let _ = event_publish_tx.try_send(changed);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));

        let state_client = Arc::clone(&client);
        let state_store = Arc::clone(&store);
        let state_changes = change_tx.clone();
        let state_snapshots = snapshot_tx.clone();
        let mut states = client.connection_state();
        tasks.push(tokio::spawn(async move {
            loop {
                let state = states.borrow_and_update().clone();
                match state {
                    ConnectionState::Connecting => {
                        state_store
                            .write()
                            .expect("session store lock poisoned")
                            .daemon_state = DaemonState::Connecting;
                    }
                    ConnectionState::Disconnected(error) => {
                        let mut store = state_store.write().expect("session store lock poisoned");
                        store.daemon_state = DaemonState::Unreachable(error);
                        store.daemon_identity = None;
                    }
                    ConnectionState::Connected(identity) => {
                        {
                            let mut store =
                                state_store.write().expect("session store lock poisoned");
                            store.daemon_state = DaemonState::Connected;
                            store.daemon_identity = Some(identity);
                            store.last_action_failure = None;
                        }
                        // The agent catalog first: `hydrate` runs the notification
                        // policy for every arriving session, and that policy reads
                        // descriptors for banner copy and approve keystrokes.
                        // Failure is non-fatal — an old daemon has no descriptors
                        // to give and every reader falls back.
                        if let Ok(agents) = state_client
                            .agent_readiness(diri_proto::AgentReadinessParams::default())
                            .await
                        {
                            state_store
                                .write()
                                .expect("session store lock poisoned")
                                .set_agent_catalog(agents);
                        }
                        if let Ok(list) = state_client.sessions().await {
                            let snapshot = {
                                let mut store =
                                    state_store.write().expect("session store lock poisoned");
                                store.hydrate(list);
                                store.snapshot()
                            };
                            state_snapshots.send_replace(snapshot);
                        }
                        let (active, governor) = {
                            let store = state_store.read().expect("session store lock poisoned");
                            (store.app_is_active, store.governor_settings())
                        };
                        let _ = state_client.set_active(active).await;
                        let _ = state_client.configure_governor(governor).await;
                        // Warm the default remote only after local catalog,
                        // session hydration and lifecycle state have completed.
                        // A slow SSH login can then never delay first paint.
                        let default_host = state_store
                            .read()
                            .expect("session store lock poisoned")
                            .default_spawn_host();
                        if let Some(host) = default_host {
                            let client = Arc::clone(&state_client);
                            let store = Arc::clone(&state_store);
                            let changes = state_changes.clone();
                            tokio::spawn(async move {
                                if let Ok(catalog) = client
                                    .agent_readiness(diri_proto::AgentReadinessParams {
                                        host: Some(host),
                                        force_refresh: false,
                                    })
                                    .await
                                {
                                    store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .set_agent_catalog(catalog);
                                    let _ = changes.send(());
                                }
                            });
                        }
                    }
                }
                let _ = state_changes.send(());
                if states.changed().await.is_err() {
                    break;
                }
            }
        }));

        let effect_client = Arc::clone(&client);
        let effect_store = Arc::clone(&store);
        let effect_detach = detach_tx.clone();
        let effect_changes = change_tx.clone();
        let effect_snapshots = snapshot_tx.clone();
        tasks.push(tokio::spawn(run_effects(
            effects,
            effect_client,
            effect_store,
            effect_detach,
            effect_changes,
            effect_snapshots,
            status_tx.clone(),
        )));

        tasks.push(tokio::spawn(run_attention_settle(
            Arc::clone(&store),
            status_tx.clone(),
        )));

        let action_client = Arc::clone(&client);
        let action_status = status_tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(command) = action_rx.recv().await {
                if action_client
                    .send_text(&command.session_id, command.text, command.submit)
                    .await
                    .is_err()
                {
                    let _ = action_status.send(reach_failure_transition());
                }
            }
        }));

        client.connect();
        Self {
            store,
            client,
            detach_tx,
            change_tx,
            status_tx,
            snapshot_tx,
            action_tx,
            tasks: Mutex::new(tasks),
        }
    }

    pub fn detachments(&self) -> broadcast::Receiver<SessionId> {
        self.detach_tx.subscribe()
    }

    /// Event-driven invalidation stream for GPUI views. No timer is needed
    /// while daemon/store state is unchanged.
    pub fn changes(&self) -> broadcast::Receiver<()> {
        self.change_tx.subscribe()
    }

    pub fn client(&self) -> &Arc<DaemonClient> {
        &self.client
    }

    pub fn status_events(&self) -> broadcast::Receiver<StatusTransition> {
        self.status_tx.subscribe()
    }

    pub fn snapshots(&self) -> tokio::sync::watch::Receiver<StoreSnapshot> {
        self.snapshot_tx.subscribe()
    }

    pub fn notification_action_sender(&self) -> mpsc::UnboundedSender<SendTextCommand> {
        self.action_tx.clone()
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
        let tasks = std::mem::take(&mut *self.tasks.lock().expect("runtime task lock poisoned"));
        for task in tasks {
            task.abort();
        }
    }
}

impl Drop for StoreRuntime {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.get_mut() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

/// Announces attention states that outlived their settle window.
///
/// Every chime and banner for a blocked or finished session comes from here,
/// one window after the transition and only if the session still holds the
/// state — see `SessionStore::drain_settled_attention`. The loop sleeps on the
/// nearest deadline and is woken early whenever a closer one is armed, so a
/// quiet fleet costs no timer at all.
async fn run_attention_settle(
    store: Arc<RwLock<SessionStore>>,
    status_tx: broadcast::Sender<StatusTransition>,
) {
    let wake = store
        .read()
        .expect("session store lock poisoned")
        .attention_wake();
    loop {
        let deadline = store
            .read()
            .expect("session store lock poisoned")
            .next_attention_deadline();
        match deadline {
            Some(deadline) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline.into()) => {}
                    // A window closer than the one being slept on, or one that
                    // replaced it: recompute rather than oversleep.
                    () = wake.notified() => continue,
                }
            }
            None => wake.notified().await,
        }
        let transitions = store
            .write()
            .expect("session store lock poisoned")
            .drain_settled_attention(Instant::now());
        for transition in transitions {
            let _ = status_tx.send(transition);
        }
    }
}

async fn run_effects(
    mut effects: mpsc::UnboundedReceiver<StoreEffect>,
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    detach_tx: broadcast::Sender<SessionId>,
    change_tx: broadcast::Sender<()>,
    snapshot_tx: tokio::sync::watch::Sender<StoreSnapshot>,
    status_tx: broadcast::Sender<StatusTransition>,
) {
    while let Some(effect) = effects.recv().await {
        let action_context = action_context(&effect);
        let force_snapshot = matches!(
            &effect,
            StoreEffect::SetActive(true) | StoreEffect::PublishSnapshot
        );
        let result: Result<(), ClientError> = match effect {
            StoreEffect::UiChanged | StoreEffect::PublishSnapshot => Ok(()),
            StoreEffect::MarkSeen(id) => client.mark_seen(&id).await,
            StoreEffect::Remove(id) => client.remove(&id).await,
            StoreEffect::Resume { id, automatic } => {
                let result = client.resume(&id).await.map(|_| ());
                if automatic {
                    store
                        .write()
                        .expect("session store lock poisoned")
                        .finish_auto_resume(&id);
                }
                result
            }
            StoreEffect::Archive(id) => client.archive(&id).await,
            StoreEffect::Unarchive(id) => client.unarchive(&id).await,
            StoreEffect::Rename { id, title } => client.rename(&id, title).await,
            StoreEffect::Spawn(params) => match client.spawn(params).await {
                Ok(id) => {
                    // The authoritative record still arrives through session.updated.
                    store
                        .write()
                        .expect("session store lock poisoned")
                        .apply_spawn_result(id);
                    Ok(())
                }
                Err(error) => Err(error),
            },
            StoreEffect::SpawnAuxiliary(params) => client.spawn(params).await.map(|_| ()),
            StoreEffect::RetryConnection => {
                client.retry_now();
                Ok(())
            }
            StoreEffect::Migrate { id, target_host } => {
                let destination = {
                    let locked = store.read().expect("session store lock poisoned");
                    target_host
                        .as_deref()
                        .map_or_else(|| "local".to_owned(), |host| locked.host_display_name(host))
                };
                let result = client.migrate(&id, target_host).await;
                let (transition, outcome) = {
                    let mut locked = store.write().expect("session store lock poisoned");
                    locked.finish_migration(&id);
                    match result {
                        Ok(migrated) => {
                            let title = migrated.session.title.clone();
                            let warning = migrated.warning.clone();
                            locked.upsert_session(migrated.session);
                            (
                                migration_transition(&title, &destination, Ok(warning.as_deref())),
                                Ok(()),
                            )
                        }
                        Err(error) => (
                            migration_transition("", &destination, Err(&error.to_string())),
                            Err(error),
                        ),
                    }
                };
                if let Some(transition) = transition {
                    let _ = status_tx.send(transition);
                }
                outcome
            }
            StoreEffect::SyncPrefs { host, host_name } => {
                let result = client.sync_prefs(&host).await;
                store
                    .write()
                    .expect("session store lock poisoned")
                    .finish_prefs_sync(&host);
                let transition = match &result {
                    Ok(report) => prefs_sync_transition(&host_name, Ok(report)),
                    Err(error) => prefs_sync_transition(&host_name, Err(&error.to_string())),
                };
                let _ = status_tx.send(transition);
                result.map(|_| ())
            }
            StoreEffect::LocateRepo {
                key,
                host,
                session_id,
            } => {
                let result = client
                    .locate_repo(diri_proto::HostLocateRepoParams {
                        host,
                        origin_url: None,
                        session_id: Some(session_id),
                    })
                    .await;
                let target = match &result {
                    Ok(found) => match (&found.path, &found.origin_url) {
                        (Some(path), _) => RepoTarget::Resolved(path.clone()),
                        (None, Some(_)) => RepoTarget::NotCloned,
                        (None, None) => RepoTarget::NoOrigin,
                    },
                    // Resolution is best-effort UI sugar: fall back to the
                    // default directory instead of surfacing an error.
                    Err(_) => RepoTarget::NoOrigin,
                };
                store
                    .write()
                    .expect("session store lock poisoned")
                    .set_repo_target(key, target);
                Ok(())
            }
            StoreEffect::ListDirectories {
                request_id,
                host,
                path,
            } => {
                let client = Arc::clone(&client);
                let store = Arc::clone(&store);
                let change_tx = change_tx.clone();
                tokio::spawn(async move {
                    let result = client
                        .list_directories(host, path)
                        .await
                        .map_err(|error| error.to_string());
                    store
                        .write()
                        .expect("session store lock poisoned")
                        .finish_directory_listing(request_id, result);
                    let _ = change_tx.send(());
                });
                Ok(())
            }
            StoreEffect::RefreshAgents { host, force } => {
                let client = Arc::clone(&client);
                let store = Arc::clone(&store);
                let change_tx = change_tx.clone();
                tokio::spawn(async move {
                    let result = client
                        .agent_readiness(diri_proto::AgentReadinessParams {
                            host: host.clone(),
                            force_refresh: force,
                        })
                        .await;
                    let mut locked = store.write().expect("session store lock poisoned");
                    let key = agent_target_key(host.as_deref());
                    // Clear the requested key, not the one the response names:
                    // a daemon too old for per-host params echoes no host, and
                    // keying off the response would leave this target loading
                    // (and every further request suppressed) forever.
                    locked.agent_catalog_loading.remove(&key);
                    match result {
                        Ok(catalog) => {
                            locked.agent_catalog_errors.remove(&key);
                            locked.set_agent_catalog(catalog);
                        }
                        Err(error) => {
                            locked.agent_catalog_errors.insert(key, error.to_string());
                        }
                    }
                    let _ = change_tx.send(());
                });
                Ok(())
            }
            StoreEffect::ConfigureAgent(params) => {
                let client = Arc::clone(&client);
                let store = Arc::clone(&store);
                let change_tx = change_tx.clone();
                tokio::spawn(async move {
                    let host = params.host.clone();
                    let result = client.configure_agent(params).await;
                    let mut locked = store.write().expect("session store lock poisoned");
                    let key = agent_target_key(host.as_deref());
                    // Requested key, not response key — see RefreshAgents.
                    locked.agent_catalog_loading.remove(&key);
                    match result {
                        Ok(catalog) => {
                            locked.agent_catalog_errors.remove(&key);
                            locked.set_agent_catalog(catalog);
                        }
                        Err(error) => {
                            locked.agent_catalog_errors.insert(key, error.to_string());
                        }
                    }
                    let _ = change_tx.send(());
                });
                Ok(())
            }
            StoreEffect::ReopenLast => match client.reopen_last().await {
                Ok(record) => {
                    let id = record.id.clone();
                    let mut store = store.write().expect("session store lock poisoned");
                    store.upsert_session(record);
                    store.select(id);
                    Ok(())
                }
                Err(error) => Err(error),
            },
            StoreEffect::SetActive(active) => client.set_active(active).await,
            StoreEffect::ConfigureGovernor(settings) => client.configure_governor(settings).await,
            StoreEffect::DetachAttachment(id) => {
                let _ = detach_tx.send(id);
                Ok(())
            }
            StoreEffect::StatusTransition(transition) => {
                let _ = status_tx.send(transition);
                Ok(())
            }
        };
        let mut store = store.write().expect("session store lock poisoned");
        if let Some(context) = action_context {
            store.last_action_failure = match &result {
                Ok(()) => None,
                Err(error) => Some(ActionFailure {
                    title: context.title.to_owned(),
                    detail: error.to_string(),
                    retrying: false,
                    retry: context.retry,
                }),
            };
        }
        let active = store.app_is_active;
        let activation_snapshot = force_snapshot.then(|| store.snapshot());
        drop(store);
        if let Some(snapshot) = activation_snapshot {
            snapshot_tx.send_replace(snapshot);
        }
        if active {
            let _ = change_tx.send(());
        }
    }
}

struct ActionContext {
    title: &'static str,
    retry: Option<RetryAction>,
}

fn action_context(effect: &StoreEffect) -> Option<ActionContext> {
    let (title, retry) = match effect {
        StoreEffect::Remove(_) => ("Close session failed", None),
        StoreEffect::Resume {
            automatic: true, ..
        } => ("Automatic resume failed", None),
        StoreEffect::Resume {
            automatic: false, ..
        } => ("Resume session failed", None),
        StoreEffect::Archive(_) => ("Archive session failed", None),
        StoreEffect::Unarchive(_) => ("Restore session failed", None),
        StoreEffect::Rename { id, title } => (
            "Rename session failed",
            Some(RetryAction::Rename {
                id: id.clone(),
                title: title.clone(),
            }),
        ),
        StoreEffect::Spawn(_) => ("Create session failed", None),
        StoreEffect::SpawnAuxiliary(_) => ("Open terminal failed", None),
        StoreEffect::Migrate { .. } => ("Move session failed", None),
        StoreEffect::SyncPrefs { host, host_name } => (
            "Sync preferences failed",
            Some(RetryAction::SyncPrefs {
                host: host.clone(),
                host_name: host_name.clone(),
            }),
        ),
        StoreEffect::ReopenLast => ("Reopen session failed", None),
        StoreEffect::UiChanged
        | StoreEffect::PublishSnapshot
        | StoreEffect::MarkSeen(_)
        | StoreEffect::RetryConnection
        | StoreEffect::LocateRepo { .. }
        | StoreEffect::ListDirectories { .. }
        | StoreEffect::SetActive(_)
        | StoreEffect::ConfigureGovernor(_)
        | StoreEffect::DetachAttachment(_)
        | StoreEffect::StatusTransition(_) => return None,
        // Catalog failures land in `agent_catalog_errors`, which the Agents
        // settings page and launch surfaces render in place — a toast on top
        // would double-report every unreachable host.
        StoreEffect::RefreshAgents { .. } | StoreEffect::ConfigureAgent(_) => return None,
    };
    Some(ActionContext { title, retry })
}

pub fn prefs_path_in_home(home: &Path) -> PathBuf {
    Prefs::path_in_home(home)
}

#[cfg(test)]
mod tests;
