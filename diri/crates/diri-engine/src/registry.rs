//! The set of live sessions, and their persisted records.
//!
//! The registry is what a control channel talks to: spawn, list, write, kill.
//! It also owns the additive `{ version, projects, sessions }` persistence
//! envelope. Unknown project fields survive a read/write cycle.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diri_proto::{
    AgentKind, DateMillis, ExitInfo, ExitReason, Resumability, SessionRecord, SessionStatus,
    TitleSource,
};
use serde::{Deserialize, Serialize};

use crate::detect::ManifestEngine;
use crate::history::CursorTranscriptTurn;
use crate::holder::{HolderClient, HolderManagerPaths, HolderPaths};
use crate::lifecycle::{LifecycleAction, LifecyclePlan};
use crate::session::{HolderConfig, RemoteAdoptSpec, Session, SessionSpec, SessionView};
use crate::state_file::JsonStateFile;
use crate::status::StatusSignal;

/// The versioned on-disk snapshot.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PersistedState {
    pub version: i64,
    #[serde(default)]
    pub projects: Vec<serde_json::Value>,
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
}

impl PersistedState {
    fn current(sessions: Vec<SessionRecord>, projects: Vec<serde_json::Value>) -> Self {
        Self {
            version: 1,
            projects,
            sessions,
        }
    }
}

pub struct Registry {
    engine: Arc<ManifestEngine>,
    sessions: HashMap<String, Session>,
    pending_launches: std::collections::HashSet<String>,
    /// Records for sessions that are no longer live but still listed.
    records: HashMap<String, SessionRecord>,
    /// Project records are kept as additive JSON so fields outside the
    /// Engine's minimal id/root/name model survive persistence.
    projects: Vec<serde_json::Value>,
    /// Sessions the user closed, newest last — the "reopen closed tab" stack.
    recently_closed: Vec<SessionRecord>,
    state_file: JsonStateFile,
    /// Minimal per-session state used only to rediscover surviving local
    /// Holders when the global Registry file is unavailable.
    recovery_root: PathBuf,
    /// Trailing-edge persistence: a mutation inside the debounce window marks
    /// dirty instead of rewriting the whole file (mark-seen fires on every
    /// tab switch), and the flusher or the next persist call writes it out.
    dirty: bool,
    last_persist: Option<std::time::Instant>,
    cursor_title_refresh_at: Option<std::time::Instant>,
    native_title_refresh_at: Option<std::time::Instant>,
}

/// Immutable input for a Cursor provider-store scan. The events watcher builds
/// these while holding the Registry lock, then performs filesystem I/O after
/// releasing it.
pub(crate) struct CursorRefreshRequest {
    id: String,
    cwd: String,
    agent_session_id: Option<String>,
    created_at: DateMillis,
    updated_at: DateMillis,
    claimed: HashSet<String>,
}

pub(crate) struct CursorRefreshResult {
    request: CursorRefreshRequest,
    conversation: Option<crate::history::CursorConversation>,
    turn: Option<CursorTranscriptTurn>,
}

/// Immutable input for a local provider-title refresh. Like Cursor metadata,
/// provider file/database reads happen after releasing the Registry lock.
pub(crate) struct NativeTitleRefreshRequest {
    account_profile: Option<diri_proto::AgentAccountProfile>,
    id: String,
    kind: AgentKind,
    cwd: String,
    agent_session_id: String,
    transcript_path: Option<String>,
}

pub(crate) struct NativeTitleRefreshResult {
    request: NativeTitleRefreshRequest,
    title: Option<crate::history::ProviderTitle>,
}

/// How long consecutive persists coalesce. Matches the Swift daemon's
/// `PersistenceStore` debounce.
const PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

impl Drop for Registry {
    fn drop(&mut self) {
        // A deferred persist must not die with the process: embedders without
        // a flusher thread (tests, short-lived tools) still land their state.
        let _ = self.flush_dirty();
    }
}

/// Flushes deferred persists on a short cadence. One per daemon, next to the
/// events watcher.
pub fn spawn_persist_flusher(
    registry: Arc<std::sync::Mutex<Registry>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("diri-persist-flusher".into())
        .spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(PERSIST_DEBOUNCE);
                let Ok(mut registry) = registry.lock() else {
                    break;
                };
                let _ = registry.flush_dirty();
            }
        })
        .expect("spawn persist flusher")
}

impl Registry {
    pub fn new(engine: Arc<ManifestEngine>, state_file: impl Into<PathBuf>) -> Self {
        let state_path = state_file.into();
        let recovery_root = state_path
            .parent()
            .map(|parent| parent.join(diri_proto::paths::SESSION_RECOVERY_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(diri_proto::paths::SESSION_RECOVERY_DIR_NAME));
        Self {
            engine,
            sessions: HashMap::new(),
            pending_launches: std::collections::HashSet::new(),
            records: HashMap::new(),
            projects: Vec::new(),
            recently_closed: Vec::new(),
            state_file: JsonStateFile::new(state_path),
            recovery_root,
            dirty: false,
            last_persist: None,
            cursor_title_refresh_at: None,
            native_title_refresh_at: None,
        }
    }

    /// Loads a persisted state file.
    ///
    /// A file that exists but will not parse is quarantined rather than
    /// ignored: treating it as a fresh install would make the next write
    /// overwrite every session record the user had.
    pub fn load(&mut self) -> std::io::Result<usize> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.load_with_home(home.as_deref())
    }

    fn load_with_home(&mut self, home: Option<&Path>) -> std::io::Result<usize> {
        let document = match self.state_file.read() {
            Ok(Some(document)) => document,
            Ok(None) => return Ok(0),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                let quarantine = self.state_file.path().with_extension("json.corrupt");
                let _ = std::fs::rename(self.state_file.path(), &quarantine);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "state file did not parse ({error}); quarantined at {}",
                        quarantine.display()
                    ),
                ));
            }
            Err(error) => return Err(error),
        };
        match serde_json::from_value::<PersistedState>(serde_json::Value::Object(document)) {
            Ok(state) => {
                self.projects = state.projects;
                let project_roots = self
                    .projects
                    .iter()
                    .filter_map(|project| {
                        Some((
                            project.get("id")?.as_str()?.to_owned(),
                            project.get("root")?.as_str()?.to_owned(),
                        ))
                    })
                    .collect::<HashMap<_, _>>();
                let mut locations = Vec::with_capacity(state.sessions.len());
                let mut repaired = Vec::new();
                for mut record in state.sessions {
                    if let Some(home) = home
                        && repair_codex_conversation(&mut record, home)
                    {
                        repaired.push(record.id.0.clone());
                    }
                    record.remote_connection = None;
                    repair_persisted_agent_title(&mut record);
                    // Resolve the owning project before repairing its
                    // location namespace. In particular, a linked worktree's
                    // cwd is not its first-level project root.
                    let project_root = project_roots
                        .get(&record.project_id.0)
                        .cloned()
                        .unwrap_or_else(|| record.cwd.clone());
                    record.project_id = session_project_id(&project_root, record.host.as_deref());
                    locations.push((project_root, record.host.clone()));
                    self.records.insert(record.id.0.clone(), record);
                }
                for (root, host) in locations {
                    self.ensure_session_project(&root, host.as_deref());
                }
                if !repaired.is_empty() {
                    self.persist_now()?;
                    for id in repaired {
                        self.write_recovery_capsule(&self.records[&id])?;
                    }
                }
                Ok(self.records.len())
            }
            Err(error) => {
                let quarantine = self.state_file.path().with_extension("json.corrupt");
                let _ = std::fs::rename(self.state_file.path(), &quarantine);
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "state file did not parse ({error}); quarantined at {}",
                        quarantine.display()
                    ),
                ))
            }
        }
    }

    /// Persists the current state — immediately when the last write is older
    /// than the debounce window, otherwise by marking dirty for the flusher
    /// ([`spawn_persist_flusher`]) or the next call to pick up. Serializing
    /// and atomically rewriting every record used to happen on every single
    /// mutation, including each tab switch's mark-seen.
    pub fn persist(&mut self) -> std::io::Result<()> {
        if let Some(last) = self.last_persist
            && last.elapsed() < PERSIST_DEBOUNCE
        {
            self.dirty = true;
            return Ok(());
        }
        self.persist_now()
    }

    /// Commits the latest state before the daemon acknowledges a shutdown.
    ///
    /// Shutdown is a durability boundary, not another debounced mutation: the
    /// process may exit immediately after the acknowledgement, so neither the
    /// background flusher nor [`Drop`] is guaranteed to run. Always write the
    /// current snapshot synchronously, even when a recent persist would
    /// normally be deferred.
    pub fn persist_for_shutdown(&mut self) -> std::io::Result<()> {
        self.persist_now()
    }

    /// Writes out a deferred persist, if one is pending.
    pub fn flush_dirty(&mut self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.persist_now()
    }

    /// Writes the current state atomically, unconditionally.
    pub(crate) fn persist_now(&mut self) -> std::io::Result<()> {
        let state = PersistedState::current(self.records_for_persistence(), self.projects.clone());
        let known = serde_json::to_value(state)?;
        let known = known
            .as_object()
            .expect("PersistedState serializes as an object");
        self.state_file.update(|document| {
            for key in ["version", "projects", "sessions"] {
                document.insert(
                    key.to_owned(),
                    known.get(key).cloned().expect("known persistence key"),
                );
            }
            Ok(())
        })?;
        self.dirty = false;
        self.last_persist = Some(std::time::Instant::now());
        Ok(())
    }

    fn records_for_persistence(&self) -> Vec<SessionRecord> {
        let mut records: Vec<SessionRecord> = self.records.values().cloned().collect();
        for record in &mut records {
            self.fold_live(record);
        }
        records.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        records
    }

    /// Adds (or replaces) a record without a live session — restores,
    /// imports, and tests use this; live sessions come from [`spawn`].
    ///
    /// [`spawn`]: Registry::spawn
    pub fn insert_record(&mut self, record: SessionRecord) {
        self.records.insert(record.id.0.clone(), record);
    }

    /// Exact directory exported to this session's hook/notify process.
    pub fn recovery_directory(&self, id: &str) -> PathBuf {
        self.recovery_root.join(id)
    }

    fn recovery_store(&self, id: &str) -> diri_proto::recovery::SessionRecoveryStore {
        diri_proto::recovery::SessionRecoveryStore::new(self.recovery_directory(id))
    }

    fn write_recovery_capsule(&self, record: &SessionRecord) -> std::io::Result<()> {
        if record.host.is_some() {
            return Ok(());
        }
        self.recovery_store(&record.id.0).write_capsule(
            &diri_proto::recovery::SessionRecoveryCapsule {
                account_profile: record.account_profile.clone(),
                version: diri_proto::recovery::SessionRecoveryCapsule::VERSION,
                session_id: record.id.clone(),
                manifest_id: record.kind.id().to_owned(),
                cwd: record.cwd.clone(),
                created_at: record.created_at,
                agent_session_id: record.agent_session_id.clone(),
                transcript_path: record.transcript_path.clone(),
            },
        )
    }

    /// Starts a session and takes ownership of it.
    pub fn spawn(&mut self, spec: SessionSpec, record: SessionRecord) -> std::io::Result<String> {
        let id = spec.id.clone();
        let recoverable = spec.holder.is_some() && record.host.is_none();
        if recoverable {
            self.write_recovery_capsule(&record)?;
        }
        let session = match Session::spawn(spec, Arc::clone(&self.engine)) {
            Ok(session) => session,
            Err(error) => {
                if recoverable {
                    let _ = self.recovery_store(&id).remove_owned_files();
                }
                return Err(error);
            }
        };
        self.records.insert(id.clone(), record);
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    pub(crate) fn reserve_launch(&mut self, id: &str, new_record: bool) -> std::io::Result<()> {
        if self.sessions.contains_key(id)
            || self.pending_launches.contains(id)
            || (new_record == self.records.contains_key(id))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "session cannot be launched in its current state",
            ));
        }
        self.pending_launches.insert(id.to_owned());
        Ok(())
    }

    pub(crate) fn release_launch(&mut self, id: &str) {
        self.pending_launches.remove(id);
    }

    /// Installs a session constructed outside the Registry. The control server's
    /// per-session operation guard owns resume/fork/stop serialization.
    pub(crate) fn install_session(
        &mut self,
        session: Session,
        record: Option<SessionRecord>,
    ) -> Result<(), Box<Session>> {
        let id = session.id().to_owned();
        if self.sessions.contains_key(&id) || (record.is_none() && !self.records.contains_key(&id))
        {
            return Err(Box::new(session));
        }
        if let Some(record) = record {
            self.records.insert(id.clone(), record);
        } else if let Some(record) = self.records.get_mut(&id) {
            record.status = SessionStatus::Starting;
            record.needs_input = None;
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
        }
        self.sessions.insert(id, session);
        Ok(())
    }

    pub(crate) fn preflight_lifecycle(&self, id: &str) -> std::io::Result<()> {
        self.records.get(id).ok_or_else(|| not_found(id))?;
        self.state_file.verify_editable()
    }

    /// Detaches only the incarnation that actually completed its stop. The
    /// caller drops the returned Session outside the Registry (pump join).
    pub(crate) fn finish_remote_stop(
        &mut self,
        id: &str,
        stop: &crate::session::RemoteStop,
        exit: crate::pty::Exit,
    ) -> Option<Session> {
        if !self
            .sessions
            .get(id)
            .is_some_and(|session| stop.matches(session))
        {
            return None;
        }
        let session = self.sessions.remove(id);
        self.record_exit(id, exit);
        session
    }

    fn record_exit(&mut self, id: &str, exit: crate::pty::Exit) {
        if let Some(record) = self.records.get_mut(id) {
            record.status = SessionStatus::Exited(diri_proto::ExitInfo {
                reason: match exit {
                    crate::pty::Exit::Signal(_) => diri_proto::ExitReason::Signaled,
                    crate::pty::Exit::Code(_) => diri_proto::ExitReason::Exited,
                },
                code: match exit {
                    crate::pty::Exit::Code(code) => Some(code),
                    _ => None,
                },
                signal: match exit {
                    crate::pty::Exit::Signal(signal) => Some(signal),
                    _ => None,
                },
            });
        }
    }

    pub fn adopt_remote(
        &mut self,
        spec: SessionSpec,
        remote: RemoteAdoptSpec,
    ) -> std::io::Result<String> {
        let id = spec.id.clone();
        if !self.records.contains_key(&id) {
            return Err(not_found(&id));
        }
        if self.sessions.contains_key(&id) || self.pending_launches.contains(&id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "session already has an owner or pending launch",
            ));
        }
        let initial_status = self
            .records
            .get(&id)
            .filter(|record| !matches!(record.status, SessionStatus::Exited(_)))
            .map(|record| (record.status.clone(), record.needs_input.clone()));
        let session = Session::adopt_remote_with_status(
            spec,
            remote,
            Arc::clone(&self.engine),
            initial_status,
        )?;
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// Adopts every still-live holder-owned session found under
    /// `holder.holders_dir` that has a persisted record. Call after [`load`]:
    /// this is what makes sessions survive a daemon restart — or the switch
    /// from the Swift daemon to this one.
    ///
    /// Returns the ids adopted. Local sessions whose holder did not survive
    /// are reconciled to `Exited` by [`reap_orphans`], so a record can never
    /// go on claiming a status only a live holder could report.
    ///
    /// [`load`]: Registry::load
    /// [`reap_orphans`]: Registry::reap_orphans
    pub fn restore(&mut self, holder: &HolderConfig, logs_dir: &Path) -> Vec<String> {
        let records_before = self.records.len();
        let adopted = self.adopt_live_holders(holder, logs_dir);
        self.reap_orphans();
        if self.records.len() > records_before {
            let _ = self.persist_now();
        }
        adopted
    }

    /// Marks every local record that no live session backs as exited.
    ///
    /// A record's status is a live holder's claim about a process. When the
    /// machine dies, the holders die with it and nothing is left to retract
    /// the claim — so `load` hands back records still saying `Working`, and
    /// every consumer reads them as running: the app dials a socket that will
    /// never answer and retries "Reconnecting terminal…" forever, offering no
    /// Resume because the conversation still looks live. Retract the claim
    /// here, once, on the only pass that knows which holders answered.
    ///
    /// Remote (`host`-bound) sessions are none of this pass's business: their
    /// authenticated Holders live on another machine and outlive both this
    /// daemon and this Mac, so their records stay untouched.
    fn reap_orphans(&mut self) {
        let orphaned: Vec<String> = self
            .records
            .values()
            .filter(|record| record.host.is_none())
            .filter(|record| !matches!(record.status, SessionStatus::Exited(_)))
            .filter(|record| !self.sessions.contains_key(&record.id.0))
            .map(|record| record.id.0.clone())
            .collect();
        if orphaned.is_empty() {
            return;
        }
        for id in &orphaned {
            if let Some(record) = self.records.get_mut(id) {
                record.status = SessionStatus::Exited(ExitInfo {
                    reason: ExitReason::DaemonRestart,
                    code: None,
                    signal: None,
                });
                record.needs_input = None;
            }
        }
        let _ = self.persist();
    }

    /// Adopts the holders that are still answering. See [`restore`].
    ///
    /// [`restore`]: Registry::restore
    fn adopt_live_holders(&mut self, holder: &HolderConfig, logs_dir: &Path) -> Vec<String> {
        let holders_dir = HolderPaths::new(&holder.holders_dir, "probe").directory;
        let Ok(entries) = std::fs::read_dir(&holders_dir) else {
            return Vec::new();
        };
        let holder_session_ids: Vec<String> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "sock")
                    && !HolderManagerPaths::is_manager_socket(path)
            })
            .filter_map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .collect();

        let mut adopted = Vec::new();
        for session_id in holder_session_ids {
            if self.sessions.contains_key(&session_id) {
                continue;
            }
            let paths = HolderPaths::new(&holder.holders_dir, &session_id);
            let client = HolderClient::new(paths.socket());
            let stat = match client.stat() {
                Ok(stat) => stat,
                Err(error) => {
                    eprintln!("diri-engine: holder recovery {session_id}: stat failed: {error}");
                    continue;
                }
            };
            if !stat.alive {
                continue;
            }
            let recovered_from_capsule = if !self.records.contains_key(&session_id) {
                let Ok(Some(capsule)) = self.recovery_store(&session_id).read_capsule() else {
                    continue;
                };
                if capsule.version != diri_proto::recovery::SessionRecoveryCapsule::VERSION
                    || capsule.session_id.0 != session_id
                    || !Path::new(&capsule.cwd).is_absolute()
                    || self.engine.manifest(&capsule.manifest_id).is_none()
                {
                    continue;
                }
                let mut recovered = recovered_record(capsule);
                if let Some(home) = std::env::var_os("HOME") {
                    repair_codex_conversation(&mut recovered, Path::new(&home));
                }
                self.ensure_session_project(&recovered.cwd, None);
                self.records.insert(session_id.clone(), recovered);
                true
            } else {
                false
            };
            let Some(record) = self.records.get(&session_id) else {
                continue;
            };
            let manifest_id = record.kind.id().to_string();
            let record_status = record.status.clone();
            let record_needs_input = record.needs_input.clone();
            let record_hibernated = record.hibernation.is_some();
            let record_updated_at = record.updated_at.0;
            let spec = SessionSpec {
                id: session_id.clone(),
                // The holder owns the real spec; this one only shapes the
                // emulator until stat's dimensions overwrite it in `adopt`.
                pty: crate::pty::PtySpec::new(Vec::new(), record.cwd.clone()),
                manifest_id: manifest_id.clone(),
                authority: crate::session::authority_for(&manifest_id, &self.engine),
                logs_dir: logs_dir.to_path_buf(),
                holder: Some(holder.clone()),
                remote: None,
                defer_launch: false,
            };
            let seeded = (!matches!(record_status, SessionStatus::Exited(_)))
                .then(|| (record_status.clone(), record_needs_input.clone()));
            let was_hibernated = record_hibernated;
            match Session::adopt_with_status(spec, holder, &stat, Arc::clone(&self.engine), seeded)
            {
                Ok(session) => {
                    if was_hibernated {
                        let _ = session.set_hibernated(true);
                    }
                    self.sessions.insert(session_id.clone(), session);
                    if let Ok(Some(seed)) = self.recovery_store(&session_id).read_activity()
                        && (recovered_from_capsule
                            || seed.occurred_at_ms as f64 >= record_updated_at)
                        && let Some((signal, metadata)) = crate::hooks::parse_activity_seed(&seed)
                    {
                        let home = std::env::var("HOME").ok();
                        let accepted = self
                            .accept_hook_metadata(
                                &session_id,
                                &metadata,
                                home.as_deref().map(Path::new),
                            )
                            .is_some();
                        if accepted
                            && let Some(session) = self.sessions.get(&session_id)
                            && session
                                .view()
                                .attention_state
                                .as_ref()
                                .and_then(|state| state.observed_at)
                                .is_none_or(|observed| seed.occurred_at_ms as f64 > observed.0)
                        {
                            session.feed_identified_signal(signal, metadata.identity.clone());
                        }
                    }
                    adopted.push(session_id);
                }
                Err(error) => {
                    eprintln!(
                        "diri-engine: holder recovery {session_id}: adoption failed: {error}"
                    );
                    continue;
                }
            }
        }
        adopted
    }

    /// The manifest engine these sessions were started with.
    pub fn engine(&self) -> Arc<ManifestEngine> {
        Arc::clone(&self.engine)
    }

    pub(crate) fn reconnect_remote(
        &mut self,
        id: &str,
        owner: &crate::session::RemoteReconnect,
        inspected: diri_proto::remote_pty::RemoteProcessState,
    ) -> std::io::Result<(bool, bool)> {
        let session = self.sessions.get_mut(id).ok_or_else(|| not_found(id))?;
        if !owner.matches(session) {
            return Err(std::io::Error::other(
                "session owner changed during reconnect",
            ));
        }
        session.restart_failed_remote(Arc::clone(&self.engine), inspected)
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn views(&self) -> Vec<SessionView> {
        let mut views: Vec<_> = self.sessions.values().map(Session::view).collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }

    /// Session records with live status and a provisional Agent-provided PTY
    /// title folded in. Structured titles persisted by hooks remain
    /// authoritative; the PTY fallback exists for Agents without hooks.
    pub fn records(&self) -> Vec<SessionRecord> {
        let mut records: Vec<SessionRecord> = self.records.values().cloned().collect();
        for record in &mut records {
            self.fold_live(record);
        }
        records.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        records
    }

    /// One record with live status folded in, without cloning the whole table.
    pub fn record(&self, id: &str) -> Option<SessionRecord> {
        let mut record = self.records.get(id)?.clone();
        self.fold_live(&mut record);
        Some(record)
    }

    /// Folds what only the live session knows into a stored record: its real
    /// status and Agent-provided title, and the resumability that follows
    /// from that status.
    fn fold_live(&self, record: &mut SessionRecord) {
        if let Some(session) = self.sessions.get(&record.id.0) {
            fold_session_view(record, &session.view());
        }
        fold_record_lifecycle(&self.engine, record);
    }

    /// Diffs live sessions' state versions against `published` (updating it in
    /// place) and returns folded records for just the sessions that changed.
    /// The steady-state cost — the events watcher polls this several times a
    /// second — is one integer compare per live session: no clones, no
    /// serialization.
    pub fn take_notifications(&self, id: &str) -> Vec<diri_terminal_state::TerminalNotification> {
        self.sessions
            .get(id)
            .map(|session| session.take_notifications())
            .unwrap_or_default()
    }

    pub fn changed_since(
        &mut self,
        published: &mut HashMap<String, u64>,
    ) -> Vec<(String, SessionRecord)> {
        published.retain(|id, _| self.sessions.contains_key(id));
        let mut changed = Vec::new();
        let changed_views = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                let version = session.state_version();
                (published.get(id) != Some(&version)).then(|| (id.clone(), version, session.view()))
            })
            .collect::<Vec<_>>();
        let mut persistence_changed = false;
        for (id, version, view) in changed_views {
            published.insert(id.clone(), version);
            if let Some(record) = self.records.get_mut(&id) {
                let previous_persisted = (
                    record.title.clone(),
                    record.title_source,
                    record.last_turn_completed_at,
                );
                fold_session_view(record, &view);
                fold_record_lifecycle(&self.engine, record);
                let record_persistence_changed = previous_persisted
                    != (
                        record.title.clone(),
                        record.title_source,
                        record.last_turn_completed_at,
                    );
                if record_persistence_changed {
                    record.updated_at = DateMillis::from(std::time::SystemTime::now());
                    persistence_changed = true;
                }
                changed.push((id, record.clone()));
            }
        }
        if persistence_changed {
            self.dirty = true;
        }
        changed
    }

    /// Captures the local Cursor sessions due for a provider-store refresh.
    /// Filesystem work happens later, after the Registry lock is released.
    pub(crate) fn cursor_refresh_requests(&mut self) -> Vec<CursorRefreshRequest> {
        let now = std::time::Instant::now();
        if self.cursor_title_refresh_at.is_some_and(|previous| {
            now.duration_since(previous) < std::time::Duration::from_secs(1)
        }) {
            return Vec::new();
        }
        self.cursor_title_refresh_at = Some(now);
        let live = self
            .sessions
            .keys()
            .filter(|id| self.records.get(*id).is_some_and(is_local_cursor_record))
            .cloned()
            .collect::<Vec<_>>();
        live.into_iter()
            .filter_map(|id| {
                let record = self.records.get(&id)?;
                Some(CursorRefreshRequest {
                    claimed: self.claimed_agent_ids(Some(&id)),
                    agent_session_id: record.agent_session_id.clone(),
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                    cwd: record.cwd.clone(),
                    id,
                })
            })
            .collect()
    }

    /// Applies provider-store results only if the same local Cursor session is
    /// still live. A respawn or hook identity update makes an in-flight scan
    /// stale and leaves it for the next one-second refresh.
    pub(crate) fn apply_cursor_refreshes(
        &mut self,
        refreshes: Vec<CursorRefreshResult>,
    ) -> Vec<(String, SessionRecord)> {
        let mut changed = Vec::new();
        for refresh in refreshes {
            let id = refresh.request.id;
            let current = self.records.get(&id).is_some_and(|record| {
                is_local_cursor_record(record)
                    && record.cwd == refresh.request.cwd
                    && record.created_at == refresh.request.created_at
                    && record.updated_at == refresh.request.updated_at
                    && record.agent_session_id == refresh.request.agent_session_id
            });
            if !current || !self.sessions.contains_key(&id) {
                continue;
            }
            let mut record_changed = false;
            if let Some(conversation) = refresh.conversation
                && let Some(record) = self.records.get_mut(&id)
                && apply_cursor_conversation(record, conversation)
            {
                record.updated_at = DateMillis::from(std::time::SystemTime::now());
                self.dirty = true;
                record_changed = true;
            }
            let mut status_changed = false;
            if let (Some(turn), Some(session)) = (refresh.turn, self.sessions.get(&id)) {
                let changed = |outcome: crate::status::ReducerOutcome| {
                    outcome.status_change.is_some() || outcome.turn_completed
                };
                status_changed = match turn {
                    CursorTranscriptTurn::Working => {
                        changed(session.feed_signal(StatusSignal::CursorTranscriptWorking))
                    }
                    CursorTranscriptTurn::Idle => {
                        let idle = session.feed_signal(StatusSignal::CursorTranscriptIdle);
                        let tick = session.feed_signal(StatusSignal::Tick);
                        changed(idle) || changed(tick)
                    }
                };
            }
            if !(record_changed || status_changed) {
                continue;
            }
            let Some(record) = self.records.get_mut(&id) else {
                continue;
            };
            if let Some(session) = self.sessions.get(&id) {
                fold_session_view(record, &session.view());
            }
            changed.push((id, record.clone()));
        }
        changed
    }

    /// Captures live local Claude/Codex sessions due for a native title read.
    /// The one-second bound mirrors provider title-generation cadence while
    /// keeping transcript/database I/O off the 150 ms state watcher path.
    pub(crate) fn native_title_refresh_requests(&mut self) -> Vec<NativeTitleRefreshRequest> {
        let now = std::time::Instant::now();
        if self.native_title_refresh_at.is_some_and(|previous| {
            now.duration_since(previous) < std::time::Duration::from_secs(1)
        }) {
            return Vec::new();
        }
        self.native_title_refresh_at = Some(now);
        self.sessions
            .keys()
            .filter_map(|id| {
                let record = self.records.get(id)?;
                if record.host.is_some()
                    || !matches!(
                        record.kind.id(),
                        AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID
                    )
                    || !accepts_native_title(record.title_source)
                {
                    return None;
                }
                Some(NativeTitleRefreshRequest {
                    account_profile: record.account_profile.clone(),
                    id: id.clone(),
                    kind: record.kind.clone(),
                    cwd: record.cwd.clone(),
                    agent_session_id: record.agent_session_id.clone()?,
                    transcript_path: record.transcript_path.clone(),
                })
            })
            .collect()
    }

    /// Applies a title only if the request still describes the same live
    /// local conversation. Diri/user renames always remain authoritative.
    pub(crate) fn apply_native_title_refreshes(
        &mut self,
        refreshes: Vec<NativeTitleRefreshResult>,
    ) -> Vec<(String, SessionRecord)> {
        let mut changed = Vec::new();
        for refresh in refreshes {
            let request = refresh.request;
            if !self.sessions.contains_key(&request.id) {
                continue;
            }
            let Some(record) = self.records.get_mut(&request.id) else {
                continue;
            };
            if record.host.is_some()
                || record.kind != request.kind
                || record.cwd != request.cwd
                || record.agent_session_id.as_deref() != Some(&request.agent_session_id)
                || !accepts_native_title(record.title_source)
            {
                continue;
            }
            let Some(title) = refresh.title else {
                continue;
            };
            if !apply_provider_title(record, &title) {
                continue;
            }
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
            self.dirty = true;
            changed.push((request.id, record.clone()));
        }
        changed
    }

    /// Ends a session but keeps its record, which is what archiving means here.
    pub fn terminate(
        &mut self,
        id: &str,
        grace: std::time::Duration,
    ) -> std::io::Result<Option<crate::pty::Exit>> {
        let Some(mut session) = self.sessions.remove(id) else {
            return Ok(None);
        };
        let exit = match session.terminate(grace) {
            Ok(exit) => exit,
            Err(error) => {
                // A failed stop must not orphan a surviving Holder or let a replacement
                // launch under the same identity (including account continuation).
                self.sessions.insert(id.to_owned(), session);
                return Err(error);
            }
        };
        self.record_exit(id, exit);
        Ok(Some(exit))
    }

    /// Drops a record entirely — the session is gone and not coming back.
    pub fn forget(&mut self, id: &str) {
        self.sessions.remove(id);
        self.records.remove(id);
    }

    /// Ends the session (if live), deletes its record AND its output log.
    /// This is the user closing a tab for good, not archiving.
    pub fn remove(&mut self, id: &str, logs_dir: &Path) -> std::io::Result<()> {
        let record = self.records.get(id).cloned().ok_or_else(|| not_found(id))?;
        let plan = LifecyclePlan::for_record(
            &record,
            LifecycleAction::Remove,
            DateMillis::from(std::time::SystemTime::now()),
        )?;
        self.state_file.verify_editable()?;
        if plan.terminate_live_session && self.sessions.contains_key(id) {
            self.terminate(id, std::time::Duration::from_millis(500))?;
        }
        self.records.remove(id);
        if plan.retain_for_reopen {
            self.recently_closed.push(record.clone());
            if self.recently_closed.len() > 10 {
                self.recently_closed.remove(0);
            }
        }
        if let Err(error) = self.persist_now() {
            self.records.insert(id.to_owned(), record);
            self.recently_closed.retain(|closed| closed.id.0 != id);
            return Err(error);
        }
        if plan.delete_output_log {
            let _ = std::fs::remove_file(logs_dir.join(format!("{id}.bin")));
        }
        let _ = self.recovery_store(id).remove_owned_files();
        Ok(())
    }

    /// Pops the most recently closed session whose folder still exists (a
    /// remote cwd can't be checked locally, so it always qualifies) and
    /// re-lists it. The caller drives the resume path from there.
    pub fn reopen_last_closed(&mut self) -> Option<SessionRecord> {
        while let Some(record) = self.recently_closed.pop() {
            if record.host.is_none() && !Path::new(&record.cwd).exists() {
                continue; // the folder is gone; try the next candidate
            }
            self.records.insert(record.id.0.clone(), record.clone());
            return Some(record);
        }
        None
    }

    /// Respawns a session under an EXISTING record — the resume path.
    pub fn respawn(&mut self, spec: SessionSpec) -> std::io::Result<()> {
        let id = spec.id.clone();
        if !self.records.contains_key(&id) {
            return Err(not_found(&id));
        }
        let record = self.records.get(&id).expect("checked above");
        let recoverable = spec.holder.is_some() && record.host.is_none();
        if recoverable {
            self.write_recovery_capsule(record)?;
        }
        let session = Session::spawn(spec, Arc::clone(&self.engine))?;
        self.sessions.insert(id.clone(), session);
        let record = self.records.get_mut(&id).expect("checked above");
        record.status = SessionStatus::Starting;
        record.needs_input = None;
        record.updated_at = DateMillis::from(std::time::SystemTime::now());
        Ok(())
    }

    /// SIGCONTs a hibernated session's tree, flushes any input queued while
    /// it was frozen, and clears the record. A no-op for sessions whose
    /// metadata and in-memory state both say awake, so hot input paths can
    /// call it unconditionally.
    pub fn wake_session(&mut self, id: &str) -> std::io::Result<()> {
        let hibernated = self
            .records
            .get(id)
            .is_some_and(|record| record.hibernation.is_some())
            || self.sessions.get(id).is_some_and(Session::is_hibernated);
        if !hibernated {
            return Ok(());
        }
        self.ensure_session_awake(id)
    }

    /// Reconciles a user-visible session with the OS process state even when
    /// its hibernation metadata is stale or missing. Fresh data-channel
    /// attaches call this once: SIGCONT is harmless for a running tree, and
    /// it repairs the otherwise permanent "live record, stopped process"
    /// state without putting a process-tree walk on every keystroke.
    pub fn ensure_session_awake(&mut self, id: &str) -> std::io::Result<()> {
        let known_hibernated = self
            .records
            .get(id)
            .is_some_and(|record| record.hibernation.is_some())
            || self.sessions.get(id).is_some_and(Session::is_hibernated);
        if let Some(session) = self.sessions.get(id) {
            session.signal_tree(libc::SIGCONT)?;
            // Flush AFTER the CONT so the tree is drinking again.
            let _ = session.set_hibernated(false);
        }
        if known_hibernated {
            self.set_hibernation(id, None);
        }
        Ok(())
    }

    /// Accept identity and status together: an inherited child callback must
    /// never complete the owning conversation, even when its metadata is ignored.
    pub fn apply_hook_report(
        &mut self,
        id: &str,
        signal: StatusSignal,
        meta: &crate::hooks::HookMetadata,
    ) -> bool {
        let home = std::env::var("HOME").ok();
        let Some(changed) = self.accept_hook_metadata(id, meta, home.as_deref().map(Path::new))
        else {
            return false;
        };
        if let Some(session) = self.sessions.get(id) {
            session.feed_identified_signal(signal, meta.identity.clone());
        }
        changed
    }

    /// Folds identity a hook payload carried into the record: the agent-side
    /// conversation id (what makes resume possible), the live transcript path
    /// (it MOVES when the agent enters a worktree), a first-prompt fallback,
    /// and the provider's native conversation title when it becomes available.
    /// Returns whether anything changed.
    pub fn apply_hook_metadata(&mut self, id: &str, meta: &crate::hooks::HookMetadata) -> bool {
        let Ok(home) = std::env::var("HOME") else {
            return self.apply_hook_metadata_with_home(id, meta, None);
        };
        self.apply_hook_metadata_with_home(id, meta, Some(Path::new(&home)))
    }

    fn apply_hook_metadata_with_home(
        &mut self,
        id: &str,
        meta: &crate::hooks::HookMetadata,
        home: Option<&Path>,
    ) -> bool {
        self.accept_hook_metadata(id, meta, home).unwrap_or(false)
    }

    /// None means rejected, distinct from accepted metadata that did not change.
    fn accept_hook_metadata(
        &mut self,
        id: &str,
        meta: &crate::hooks::HookMetadata,
        home: Option<&Path>,
    ) -> Option<bool> {
        let claimed = self.claimed_agent_ids(Some(id));
        let mut transcript = self.records.get(id).and_then(|record| {
            if record.host.is_some() {
                return None;
            }
            let home = home?;
            let agent_id = meta
                .agent_session_id
                .as_deref()
                .or(record.agent_session_id.as_deref())?;
            let kind = record.effective_kind();
            let validate = |candidate: &str| {
                crate::history::validate_profile_transcript_path(
                    record.account_profile.as_ref(),
                    home,
                    kind,
                    agent_id,
                    &record.cwd,
                    Path::new(candidate),
                )
            };
            meta.transcript_path
                .as_deref()
                .and_then(validate)
                .or_else(|| record.transcript_path.as_deref().and_then(validate))
                .or_else(|| {
                    (kind.id() == diri_proto::AgentKind::CODEX_ID)
                        .then(|| {
                            crate::history::find_profile_codex_transcript(
                                record.account_profile.as_ref(),
                                home,
                                agent_id,
                                &record.cwd,
                            )
                        })
                        .flatten()
                })
        });
        // Codex children inherit the parent's Diri notify command. Their
        // thread IDs and prompts must never replace the owning conversation.
        if self.records.get(id).is_some_and(|record| {
            record.host.is_none() && record.effective_kind() == &AgentKind::CODEX
        }) && meta.agent_session_id.is_some()
            && transcript
                .as_mut()
                .is_none_or(|transcript| transcript.is_codex_subagent())
        {
            return None;
        }
        let native_title = self.records.get(id).and_then(|record| {
            if record.host.is_some() || !accepts_native_title(record.title_source) {
                return None;
            }
            let title = match record.kind.id() {
                diri_proto::AgentKind::CLAUDE_CODE_ID => transcript
                    .as_mut()
                    .and_then(|transcript| transcript.latest_claude_title())
                    .map(crate::history::ProviderTitle::named),
                diri_proto::AgentKind::CODEX_ID => {
                    let home = home?;
                    let agent_id = meta
                        .agent_session_id
                        .as_deref()
                        .or(record.agent_session_id.as_deref())?;
                    crate::history::profile_codex_title_details(
                        record.account_profile.as_ref(),
                        home,
                        &[agent_id],
                    )
                    .remove(agent_id)
                }
                _ => None,
            }?;
            Some(title)
        });
        let cursor = self.records.get(id).and_then(|record| {
            if !is_local_cursor_record(record) {
                return None;
            }
            crate::history::cursor_conversation(
                home?,
                &record.cwd,
                meta.agent_session_id
                    .as_deref()
                    .or(record.agent_session_id.as_deref()),
                record.created_at.0,
                &claimed,
            )
        });
        let record = self.records.get_mut(id)?;
        let mut changed = false;
        if let Some(agent_id) = &meta.agent_session_id
            && record.agent_session_id.as_ref() != Some(agent_id)
        {
            record.agent_session_id = Some(agent_id.clone());
            record.resumability = diri_proto::Resumability::Live;
            changed = true;
        }
        if let Some(transcript) =
            transcript.map(|transcript| transcript.path().to_string_lossy().into_owned())
            && record.transcript_path.as_ref() != Some(&transcript)
        {
            record.transcript_path = Some(transcript);
            changed = true;
        }
        if let Some(conversation) = cursor {
            changed |= apply_cursor_conversation(record, conversation);
        }
        if repair_persisted_agent_title(record) {
            changed = true;
        }
        if let Some(title) = &meta.first_prompt_title
            && record.title_source == TitleSource::Placeholder
        {
            record.title = title.clone();
            record.title_source = TitleSource::FirstPrompt;
            changed = true;
        }
        if let Some(title) = native_title {
            changed |= apply_provider_title(record, &title);
        }
        if changed {
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
        }
        let recovery_snapshot = changed.then(|| record.clone());
        if let Some(record) = recovery_snapshot.as_ref() {
            let _ = self.write_recovery_capsule(record);
        }
        Some(changed)
    }

    /// SIGSTOPs a session's whole tree and records it as hibernated. The PTY
    /// and holder stay alive; wake is one SIGCONT away.
    pub fn hibernate(
        &mut self,
        id: &str,
        reason: diri_proto::HibernationReason,
    ) -> std::io::Result<()> {
        let tree = {
            let session = self.sessions.get(id).ok_or_else(|| not_found(id))?;
            let tree = session.signal_tree(libc::SIGSTOP)?;
            let _ = session.set_hibernated(true);
            tree
        };
        self.set_hibernation(
            id,
            Some(diri_proto::HibernationInfo {
                since: std::time::SystemTime::now().into(),
                reason,
                tree_pids: tree.iter().map(|(pid, _)| *pid).collect(),
                tree_start_times: Some(tree.into_iter().collect()),
            }),
        );
        Ok(())
    }

    /// Folds a governor sample into the record; returns the event to publish
    /// when anything actually changed (carrying only the changed facets, as
    /// the Swift daemon does).
    pub fn apply_resource_sample(
        &mut self,
        id: &str,
        memory_bytes: Option<u64>,
        ports: Option<Vec<diri_proto::PortInfo>>,
        artifacts: Option<Vec<diri_proto::SessionArtifact>>,
    ) -> Option<diri_proto::SessionResourcesEvent> {
        let record = self.records.get_mut(id)?;
        let mut memory_changed = false;
        let mut ports_changed = false;
        let mut artifacts_changed = false;
        if let Some(memory) = memory_bytes
            && record.memory_bytes != Some(memory)
        {
            record.memory_bytes = Some(memory);
            memory_changed = true;
        }
        if let Some(ports) = ports
            && record.listening_ports.as_deref().unwrap_or_default() != ports
        {
            record.listening_ports = Some(ports);
            ports_changed = true;
        }
        if let Some(artifacts) = artifacts
            && record.artifacts.as_deref().unwrap_or_default() != artifacts
        {
            record.artifacts = Some(artifacts);
            artifacts_changed = true;
        }
        if !(memory_changed || ports_changed || artifacts_changed) {
            return None;
        }
        Some(diri_proto::SessionResourcesEvent {
            id: record.id.clone(),
            memory_bytes: memory_changed.then_some(record.memory_bytes).flatten(),
            listening_ports: if ports_changed {
                record.listening_ports.clone()
            } else {
                None
            },
            artifacts: if artifacts_changed {
                record.artifacts.clone()
            } else {
                None
            },
        })
    }

    /// Replaces the record's PR statuses when they materially changed.
    /// Returns whether they did.
    pub fn apply_pull_request_statuses(
        &mut self,
        id: &str,
        statuses: Vec<diri_proto::PullRequestStatus>,
    ) -> bool {
        let Some(record) = self.records.get_mut(id) else {
            return false;
        };
        let current = record.pull_requests.as_deref().unwrap_or_default();
        let materially_same = current.len() == statuses.len()
            && current.iter().zip(&statuses).all(|(a, b)| {
                // fetched_at always moves; compare everything else.
                let mut b_pinned = b.clone();
                b_pinned.fetched_at = a.fetched_at;
                *a == b_pinned
            });
        if materially_same {
            return false;
        }
        record.pull_requests = (!statuses.is_empty()).then_some(statuses);
        record.updated_at = DateMillis::from(std::time::SystemTime::now());
        true
    }

    /// Applies an arbitrary record mutation (migrate's in-place rewrite).
    pub fn update_record(&mut self, id: &str, mutate: impl FnOnce(&mut SessionRecord)) {
        if let Some(record) = self.records.get_mut(id) {
            mutate(record);
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
        }
    }

    pub fn set_hibernation(&mut self, id: &str, info: Option<diri_proto::HibernationInfo>) {
        if let Some(record) = self.records.get_mut(id) {
            record.hibernation = info;
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
        }
    }

    /// Upserts a local project by its deterministic root-derived id.
    pub fn add_project(&mut self, root: &str) -> serde_json::Value {
        self.ensure_session_project(root, None)
    }

    /// Ensures every Session has a concrete first-level Project record. The
    /// host remains an execution property of Sessions; the project id carries
    /// the location namespace and prevents cross-host path collisions.
    pub fn ensure_session_project(&mut self, root: &str, host: Option<&str>) -> serde_json::Value {
        let id = session_project_id(root, host).0;
        if let Some(existing) = self
            .projects
            .iter_mut()
            .find(|project| project.get("id").and_then(|value| value.as_str()) == Some(&id))
        {
            // Records persisted before projects carried their host learn it
            // here; without it a remote project with no live sessions cannot
            // tell the app which machine owns its root.
            if let Some(host) = host
                && existing.get("host").is_none()
            {
                existing["host"] = serde_json::Value::String(host.to_owned());
            }
            return existing.clone();
        }
        let name = Path::new(root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string());
        let mut project = serde_json::json!({ "id": id, "root": root, "name": name });
        if let Some(host) = host {
            project["host"] = serde_json::Value::String(host.to_owned());
        }
        self.projects.push(project.clone());
        project
    }

    pub fn rename(&mut self, id: &str, title: &str) -> std::io::Result<()> {
        let record = self.records.get_mut(id).ok_or_else(|| not_found(id))?;
        record.title = title.to_string();
        record.title_source = TitleSource::UserRename;
        record.updated_at = DateMillis::from(std::time::SystemTime::now());
        Ok(())
    }

    /// Moves an ended resumable record to another checkout of the same
    /// project and persists the change as one logical operation. Validation of
    /// repository membership and target ownership lives in the control method.
    ///
    /// Persistence can fail after the in-memory edit (for example, when the
    /// state directory becomes unavailable). Restore every touched field in
    /// that case so callers never observe a failed request as a hidden move
    /// that a later flush makes durable.
    pub fn reparent_worktree(
        &mut self,
        id: &str,
        cwd: String,
        branch: Option<String>,
    ) -> std::io::Result<SessionRecord> {
        let was_dirty = self.dirty;
        let previous = {
            let record = self.records.get_mut(id).ok_or_else(|| not_found(id))?;
            let previous = (
                record.cwd.clone(),
                record.worktree_path.clone(),
                record.git_branch.clone(),
                record.updated_at,
            );
            record.cwd.clone_from(&cwd);
            record.worktree_path = Some(cwd);
            record.git_branch = branch;
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
            previous
        };

        // A confirmed move is user-visible identity metadata, not a sampled
        // field that may wait for the normal debounce. Force the atomic write
        // now so an `Ok` response means this exact checkout survived a crash.
        if let Err(error) = self.persist_now() {
            let record = self
                .records
                .get_mut(id)
                .expect("record cannot disappear during a locked mutation");
            record.cwd = previous.0;
            record.worktree_path = previous.1;
            record.git_branch = previous.2;
            record.updated_at = previous.3;
            self.dirty = was_dirty;
            return Err(error);
        }

        Ok(self
            .records
            .get(id)
            .expect("record cannot disappear during a locked mutation")
            .clone())
    }

    pub fn mark_seen(&mut self, id: &str) -> std::io::Result<()> {
        let record = self.records.get_mut(id).ok_or_else(|| not_found(id))?;
        record.last_seen_at = Some(DateMillis::from(std::time::SystemTime::now()));
        Ok(())
    }

    /// Ends the session but keeps its record on the shelf: kill-tree,
    /// keep-record, stamp `archivedAt`.
    pub fn archive(&mut self, id: &str) -> std::io::Result<()> {
        let original = self.records.get(id).cloned().ok_or_else(|| not_found(id))?;
        self.archive_record(original)
    }

    pub(crate) fn archive_record(&mut self, original: SessionRecord) -> std::io::Result<()> {
        let id = original.id.0.clone();
        let id = id.as_str();
        let plan = LifecyclePlan::for_record(
            &original,
            LifecycleAction::Archive,
            DateMillis::from(std::time::SystemTime::now()),
        )?;
        self.state_file.verify_editable()?;
        if plan.terminate_live_session && self.sessions.contains_key(id) {
            self.terminate(id, std::time::Duration::from_millis(500))?;
        }
        self.records.insert(
            id.to_owned(),
            plan.replacement.expect("archive keeps the record"),
        );
        if let Err(error) = self.persist_now() {
            self.records.insert(id.to_owned(), original);
            return Err(error);
        }
        Ok(())
    }

    pub fn unarchive(&mut self, id: &str) -> std::io::Result<()> {
        let original = self.records.get(id).cloned().ok_or_else(|| not_found(id))?;
        if original.archived_at.is_none() {
            return Ok(());
        }
        let plan = LifecyclePlan::for_record(
            &original,
            LifecycleAction::Restore,
            DateMillis::from(std::time::SystemTime::now()),
        )?;
        self.records.insert(
            id.to_owned(),
            plan.replacement.expect("restore keeps the record"),
        );
        if let Err(error) = self.persist_now() {
            self.records.insert(id.to_owned(), original);
            return Err(error);
        }
        Ok(())
    }

    /// Agent-side conversation ids already represented here, so a history
    /// scan can exclude conversations that are live sessions.
    pub fn tracked_agent_session_ids(&self) -> Vec<String> {
        self.records
            .values()
            .filter_map(|record| record.agent_session_id.clone())
            .collect()
    }

    /// The additive project list exposed through the control protocol.
    pub fn projects_raw(&self) -> &[serde_json::Value] {
        &self.projects
    }

    pub fn live_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn state_file(&self) -> &Path {
        self.state_file.path()
    }

    fn claimed_agent_ids(&self, except: Option<&str>) -> HashSet<String> {
        self.records
            .iter()
            .filter(|(id, _)| except != Some(id.as_str()))
            .filter_map(|(_, record)| record.agent_session_id.clone())
            .collect()
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub(crate) fn scan_cursor_refreshes(
    requests: Vec<CursorRefreshRequest>,
) -> Vec<CursorRefreshResult> {
    let Some(home) = user_home() else {
        return Vec::new();
    };
    requests
        .into_iter()
        .map(|request| {
            let conversation = crate::history::cursor_conversation(
                &home,
                &request.cwd,
                request.agent_session_id.as_deref(),
                request.created_at.0,
                &request.claimed,
            );
            let turn = conversation
                .as_ref()
                .and_then(|conversation| conversation.transcript_path.as_deref())
                .and_then(|path| crate::history::cursor_transcript_turn(Path::new(path)));
            CursorRefreshResult {
                request,
                conversation,
                turn,
            }
        })
        .collect()
}

pub(crate) fn scan_native_title_refreshes(
    requests: Vec<NativeTitleRefreshRequest>,
) -> Vec<NativeTitleRefreshResult> {
    let Some(home) = user_home() else {
        return Vec::new();
    };
    // Group by the exact bound provider directory for this pass. Opening the
    // same SQLite database and scanning its index once per session made the
    // one-second title refresh scale with repeated filesystem/schema work.
    let mut codex_groups: HashMap<Option<&str>, Vec<&NativeTitleRefreshRequest>> = HashMap::new();
    for request in &requests {
        if request.kind.id() == AgentKind::CODEX_ID {
            codex_groups
                .entry(
                    request
                        .account_profile
                        .as_ref()
                        .map(|p| p.config_home.as_str()),
                )
                .or_default()
                .push(request);
        }
    }
    let mut codex_titles = HashMap::new();
    for group in codex_groups.values() {
        let ids: Vec<_> = group.iter().map(|r| r.agent_session_id.as_str()).collect();
        let titles = crate::history::profile_codex_title_details(
            group[0].account_profile.as_ref(),
            &home,
            &ids,
        );
        for request in group {
            if let Some(title) = titles.get(&request.agent_session_id) {
                codex_titles.insert(request.id.clone(), title.clone());
            }
        }
    }
    requests
        .into_iter()
        .map(|request| {
            let title = match request.kind.id() {
                AgentKind::CLAUDE_CODE_ID => request
                    .transcript_path
                    .as_deref()
                    .and_then(|path| {
                        crate::history::validate_profile_transcript_path(
                            request.account_profile.as_ref(),
                            &home,
                            &request.kind,
                            &request.agent_session_id,
                            &request.cwd,
                            Path::new(path),
                        )
                    })
                    .and_then(|mut transcript| transcript.latest_claude_title())
                    .map(crate::history::ProviderTitle::named),
                AgentKind::CODEX_ID => codex_titles.remove(&request.id),
                _ => None,
            };
            NativeTitleRefreshResult { request, title }
        })
        .collect()
}

fn apply_cursor_conversation(
    record: &mut SessionRecord,
    conversation: crate::history::CursorConversation,
) -> bool {
    let mut changed = false;
    if record.agent_session_id.as_deref() != Some(conversation.id.as_str()) {
        record.agent_session_id = Some(conversation.id);
        record.resumability = diri_proto::Resumability::Live;
        changed = true;
    }
    if let Some(path) = conversation.transcript_path
        && record.transcript_path.as_ref() != Some(&path)
    {
        record.transcript_path = Some(path);
        changed = true;
    }
    let accepts_generated_title = matches!(
        record.title_source,
        TitleSource::Placeholder
            | TitleSource::FirstPrompt
            | TitleSource::TerminalTitle
            | TitleSource::Unknown
    );
    if accepts_generated_title
        && let Some(title) = conversation
            .title
            .and_then(|title| normalize_agent_title(&title))
            .filter(|title| !is_generic_terminal_title(title, record))
        && (record.title != title || record.title_source != TitleSource::AgentProvided)
    {
        record.title = title;
        record.title_source = TitleSource::AgentProvided;
        changed = true;
    }
    changed
}

fn accepts_native_title(source: TitleSource) -> bool {
    matches!(
        source,
        TitleSource::Placeholder
            | TitleSource::FirstPrompt
            | TitleSource::TerminalTitle
            | TitleSource::AgentProvided
            | TitleSource::Unknown
    )
}

fn apply_native_title(record: &mut SessionRecord, title: &str) -> bool {
    if !accepts_native_title(record.title_source) {
        return false;
    }
    let Some(title) =
        normalize_agent_title(title).filter(|title| !is_generic_terminal_title(title, record))
    else {
        return false;
    };
    if record.title == title && record.title_source == TitleSource::AgentProvided {
        return false;
    }
    record.title = title;
    record.title_source = TitleSource::AgentProvided;
    true
}

fn apply_provider_title(
    record: &mut SessionRecord,
    candidate: &crate::history::ProviderTitle,
) -> bool {
    if candidate.source != TitleSource::FirstPrompt {
        return apply_native_title(record, &candidate.title);
    }
    let title = crate::hooks::title_from_prompt(&candidate.title);
    // Older builds promoted the database's prompt preview to AgentProvided.
    // Demote only an exact match confirmed by this identity-bound store read.
    let old_prompt = record.title_source == TitleSource::AgentProvided
        && (record.title == candidate.title || record.title == title);
    if !matches!(
        record.title_source,
        TitleSource::Placeholder | TitleSource::FirstPrompt | TitleSource::Unknown
    ) && !old_prompt
    {
        return false;
    }
    if title.is_empty()
        || (record.title == title && record.title_source == TitleSource::FirstPrompt)
    {
        return false;
    }
    record.title = title;
    record.title_source = TitleSource::FirstPrompt;
    true
}

fn is_local_cursor_record(record: &SessionRecord) -> bool {
    record.kind == diri_proto::AgentKind::CURSOR && record.host.is_none()
}

fn fold_session_view(record: &mut SessionRecord, view: &SessionView) {
    record.remote_connection = view.remote_connection;
    fold_session_status(record, view);
    // cursor-agent (and similar) stamp a brand/status OSC title as soon as
    // they are idle. That must not freeze the record as AgentProvided, or
    // the first real prompt can never name the session.
    repair_persisted_agent_title(record);
    if record.kind == diri_proto::AgentKind::SHELL
        || matches!(
            record.title_source,
            TitleSource::AgentProvided | TitleSource::DirijorAssigned | TitleSource::UserRename
        )
    {
        return;
    }
    let terminal_title = view.terminal_title.as_deref().or_else(|| {
        (view.title_source != Some(TitleSource::FirstPrompt))
            .then_some(view.title.as_deref())
            .flatten()
    });
    if let Some(title) = terminal_title.and_then(|title| normalize_terminal_title(title, record)) {
        record.title = title;
        record.title_source = TitleSource::TerminalTitle;
    } else if matches!(
        record.title_source,
        TitleSource::Placeholder | TitleSource::Unknown
    ) && view.title_source == Some(TitleSource::FirstPrompt)
        && let Some(title) = view.title.as_deref().and_then(normalize_agent_title)
    {
        // Prompt capture belongs to this Session attachment, not the durable
        // conversation. After adoption/resume its first input may be a later
        // turn. Only fill an unnamed record; otherwise this can overwrite a
        // saved/provider first prompt on every live fold and fight refreshes.
        record.title = title;
        record.title_source = TitleSource::FirstPrompt;
    }
}

fn repair_codex_conversation(record: &mut SessionRecord, home: &Path) -> bool {
    if record.host.is_some() || record.effective_kind() != &AgentKind::CODEX {
        return false;
    }
    let Some(agent_id) = record.agent_session_id.as_deref() else {
        return false;
    };
    let Some((root_id, transcript)) = crate::history::codex_root_conversation(
        record.account_profile.as_ref(),
        home,
        agent_id,
        &record.cwd,
        record.transcript_path.as_deref(),
    ) else {
        return false;
    };
    record.agent_session_id = Some(root_id);
    record.transcript_path = Some(transcript.path().to_string_lossy().into_owned());
    true
}

/// Removes terminal-brand decorations accidentally persisted as conversation
/// titles by older builds. User and Diri-assigned names are intentionally
/// untouched; only titles attributed to the Agent/PTY are safe to repair.
fn repair_persisted_agent_title(record: &mut SessionRecord) -> bool {
    // Once Codex has an identified native name, its literal text belongs to
    // the conversation. A valid `/rename Ready` must not be parsed as activity.
    if record.kind == AgentKind::CODEX
        && record.title_source == TitleSource::AgentProvided
        && record.agent_session_id.is_some()
    {
        return false;
    }
    if !matches!(
        record.title_source,
        TitleSource::AgentProvided | TitleSource::TerminalTitle
    ) {
        return false;
    }
    match normalize_terminal_title(&record.title, record) {
        Some(title) if title != record.title => {
            record.title = title;
            if record.kind == AgentKind::CODEX {
                record.title_source = TitleSource::TerminalTitle;
            }
            true
        }
        Some(_)
            if record.kind == AgentKind::CODEX
                && record.agent_session_id.is_none()
                && record.title_source == TitleSource::AgentProvided =>
        {
            record.title_source = TitleSource::TerminalTitle;
            true
        }
        Some(_) => false,
        None => {
            record.title = record.kind.id().to_owned();
            record.title_source = TitleSource::Placeholder;
            true
        }
    }
}

fn fold_session_status(record: &mut SessionRecord, view: &SessionView) {
    record.attention_state.clone_from(&view.attention_state);
    record.status.clone_from(&view.status);
    // Keep evidence only when it explains this exact canonical state. This is
    // both a mixed-version guard and protection against observing the reducer
    // and shared record on opposite sides of an in-flight transition.
    record.status_evidence = view
        .status_evidence
        .as_ref()
        .filter(|evidence| evidence.status == view.status)
        .cloned();
    record.needs_input.clone_from(&view.needs_input);
    if view.last_turn_completed_at > record.last_turn_completed_at {
        record.last_turn_completed_at = view.last_turn_completed_at;
    }
}

/// Resolves status-dependent facts at the one seam shared by snapshots and
/// incremental events, so clients never have to reconstruct Agent behavior.
fn fold_record_lifecycle(engine: &ManifestEngine, record: &mut SessionRecord) {
    // `Live` only records that the agent named its conversation while it was
    // running. After exit, Resume needs the stronger answer: whether that
    // conversation can actually be re-entered through its manifest.
    if matches!(record.status, SessionStatus::Exited(_))
        && record.resumability == diri_proto::Resumability::Live
    {
        record.resumability = if can_reenter(engine, record) {
            diri_proto::Resumability::Resumable
        } else {
            diri_proto::Resumability::NotResumable
        };
    }
    record.capabilities = Some(engine.session_capabilities(record));
}

fn can_reenter(engine: &ManifestEngine, record: &SessionRecord) -> bool {
    engine
        .manifest(record.kind.id())
        .and_then(|manifest| manifest.agent.as_ref())
        .is_some_and(|agent| {
            agent.supports_resume()
                && (record.agent_session_id.is_some() || agent.supports_id_free_resume())
        })
}

fn normalize_agent_title(title: &str) -> Option<String> {
    let line = title.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line.trim_start_matches(|character: char| {
        character.is_whitespace() || (!character.is_alphanumeric() && character != '_')
    });
    let normalized = line
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect::<String>();
    let normalized = normalized.trim();
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

/// OSC is a presentation surface: Codex combines activity, a thread name and
/// the project, and temporarily replaces the name while generation is pending.
/// Only a useful conversation component may become a provisional sidebar name.
fn normalize_terminal_title(title: &str, record: &SessionRecord) -> Option<String> {
    let mut title = normalize_agent_title(title)?;
    if record.kind == AgentKind::CODEX {
        if let Some((name, directory)) = title.rsplit_once(" | ")
            && (directory
                == record
                    .cwd
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                || directory == record.cwd)
        {
            title = name.trim().to_owned();
        }
        let mut parts = Vec::new();
        for part in title.split(" | ") {
            let part = part
                .trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{2800}'..='\u{28ff}'));
            let compact = compact_alnum(&part.to_ascii_lowercase());
            if matches!(
                compact.as_str(),
                "renaming"
                    | "naming"
                    | "untitled"
                    | "newchat"
                    | "actionrequired"
                    | "working"
                    | "thinking"
                    | "idle"
                    | "ready"
                    | "done"
            ) {
                return None;
            }
            if part.is_empty() {
                continue;
            }
            parts.push(part);
        }
        title = parts.join(" | ");
    }
    (!title.is_empty() && !is_generic_terminal_title(&title, record)).then_some(title)
}

fn is_generic_terminal_title(title: &str, record: &SessionRecord) -> bool {
    let title = title.trim().to_ascii_lowercase();
    let compact_title = title
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect::<String>();
    let cwd = record.cwd.trim_end_matches('/').to_ascii_lowercase();
    let directory = cwd.rsplit('/').next().unwrap_or(&cwd);
    title == cwd
        || title == directory
        || matches!(
            compact_title.as_str(),
            "claude"
                | "claudecode"
                | "codex"
                | "cursor"
                | "cursoragent"
                | "gemini"
                | "terminal"
                | "shell"
        )
        || (record.kind == diri_proto::AgentKind::CURSOR && is_cursor_status_title(&title))
}

fn is_cursor_status_title(title: &str) -> bool {
    let rest = title
        .strip_prefix("cursor agent")
        .or_else(|| title.strip_prefix("cursor-agent"));
    if let Some(rest) = rest {
        let compact: String = rest
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect();
        if compact.is_empty() || is_cursor_status_stamp(&compact) {
            return true;
        }
    }
    title
        .rsplit_once(" - ")
        .is_some_and(|(_, stamp)| is_cursor_status_stamp(&compact_alnum(stamp)))
}

fn compact_alnum(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn is_cursor_status_stamp(compact: &str) -> bool {
    compact.starts_with("working")
        || matches!(
            compact,
            "ready"
                | "thinking"
                | "generating"
                | "idle"
                | "newchat"
                | "planning"
                | "queued"
                | "runningshellcommand"
                | "loadingconversation"
                | "reconnecting"
                | "movingtocloud"
                | "reviewingchanges"
                | "waitingforyou"
                | "waitingforconfirmation"
        )
}

fn recovered_record(capsule: diri_proto::recovery::SessionRecoveryCapsule) -> SessionRecord {
    let now = DateMillis::from(std::time::SystemTime::now());
    let project_id = session_project_id(&capsule.cwd, None);
    SessionRecord {
        attention_state: None,
        id: capsule.session_id,
        kind: AgentKind::new(capsule.manifest_id),
        cwd: capsule.cwd,
        project_id,
        worktree_path: None,
        git_branch: None,
        title: "Recovered session".into(),
        title_source: TitleSource::Placeholder,
        account_profile: capsule.account_profile,
        originating_prompt: None,
        agent_session_id: capsule.agent_session_id,
        transcript_path: capsule.transcript_path,
        status: SessionStatus::Starting,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        capabilities: None,
        parent: None,
        created_at: capsule.created_at,
        updated_at: now,
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        remote_connection: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

fn not_found(id: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, format!("no session {id}"))
}

/// Stable FNV-1a-shaped hash over a project location, truncated to 48 bits.
/// The historical multiplier is intentionally retained so existing local
/// project ids remain stable.
fn project_id(root: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_1000_0000_01B3);
    }
    format!("p_{:012x}", hash & 0xFFFF_FFFF_FFFF)
}

/// Stable project identity for the directory and machine that own a Session.
/// Local IDs remain compatible with `project.add`; remote IDs are namespaced
/// by host id so identical paths on different machines never share a node.
pub(crate) fn session_project_id(root: &str, host: Option<&str>) -> diri_proto::ProjectId {
    let location = host.map_or_else(|| root.to_owned(), |host| format!("ssh\0{host}\0{root}"));
    diri_proto::ProjectId(project_id(&location))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{AgentKind, DateMillis, ProjectId, Resumability, SessionId, TitleSource};

    #[test]
    fn title_refresh_batch_keeps_profiles_with_the_same_thread_id_separate() {
        let root = tempfile::tempdir().unwrap();
        let mut requests = Vec::new();
        for label in ["Personal", "Work"] {
            let config = root.path().join(label);
            std::fs::create_dir_all(&config).unwrap();
            let db = rusqlite::Connection::open(config.join("state_1.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT);")
                .unwrap();
            db.execute("INSERT INTO threads VALUES ('same-thread', ?1)", [label])
                .unwrap();
            for index in 0..2 {
                requests.push(NativeTitleRefreshRequest {
                    account_profile: Some(diri_proto::AgentAccountProfile {
                        id: label.into(),
                        label: label.into(),
                        agent: "codex".into(),
                        host: None,
                        config_home: config.to_string_lossy().into_owned(),
                        is_default: false,
                    }),
                    id: format!("{label}-{index}"),
                    kind: AgentKind::CODEX,
                    cwd: "/tmp".into(),
                    agent_session_id: "same-thread".into(),
                    transcript_path: None,
                });
            }
        }
        let refreshed = scan_native_title_refreshes(requests);
        assert_eq!(refreshed.len(), 4);
        for result in refreshed {
            assert_eq!(
                result.title.as_ref().map(|title| title.title.as_str()),
                Some(
                    result
                        .request
                        .account_profile
                        .as_ref()
                        .unwrap()
                        .label
                        .as_str()
                )
            );
        }
    }

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            attention_state: None,
            id: SessionId(id.into()),
            kind: AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: TitleSource::Placeholder,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Starting,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::NotResumable,
            capabilities: None,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    fn engine() -> Arc<ManifestEngine> {
        let dir = crate::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
        Arc::new(engine)
    }

    #[test]
    fn launch_reservation_rejects_overlapping_owners_and_wrong_record_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        assert!(registry.reserve_launch("s_1", false).is_err());
        registry.reserve_launch("s_1", true).unwrap();
        assert!(registry.reserve_launch("s_1", true).is_err());
        registry.release_launch("s_1");
        registry.records.insert("s_1".into(), record("s_1"));
        assert!(registry.reserve_launch("s_1", true).is_err());
        registry.reserve_launch("s_1", false).unwrap();
        assert!(registry.reserve_launch("s_1", false).is_err());
        registry.release_launch("s_1");
        registry.reserve_launch("s_1", false).unwrap();
    }

    #[test]
    fn restart_does_not_restore_a_persisted_connected_transport_claim() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let mut registry = Registry::new(engine(), &path);
        let mut session = record("remote");
        session.host = Some("fixture".into());
        session.remote_connection = Some(diri_proto::RemoteConnection {
            state: diri_proto::RemoteConnectionState::Connected,
            since: DateMillis(123.0),
        });
        registry.insert_record(session);
        registry.persist().unwrap();
        let mut restarted = Registry::new(engine(), path);
        restarted.load().unwrap();
        assert_eq!(restarted.records()[0].remote_connection, None);
    }

    #[test]
    fn state_round_trips_through_the_swift_file_shape() {
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");

        let mut registry = Registry::new(engine(), &state_file);
        registry.records.insert("s_1".into(), record("s_1"));
        registry.persist().expect("persist");

        // The shape on disk is what the Swift daemon expects.
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_file).expect("read")).expect("parse");
        assert_eq!(raw["version"], 1);
        assert!(raw["sessions"].is_array());
        assert!(raw["projects"].is_array());
        assert_eq!(raw["sessions"][0]["id"], "s_1");

        let mut reloaded = Registry::new(engine(), &state_file);
        assert_eq!(reloaded.load().expect("load"), 1);
        assert_eq!(reloaded.records()[0].id.0, "s_1");
    }

    #[test]
    fn shutdown_persistence_commits_a_snapshot_deferred_by_the_debounce() {
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        let mut registry = Registry::new(engine(), &state_file);

        registry.insert_record(record("before"));
        registry.persist().expect("initial persist");
        registry.insert_record(record("latest"));
        registry.persist().expect("debounced persist");

        let deferred: PersistedState =
            serde_json::from_slice(&std::fs::read(&state_file).expect("read deferred state"))
                .expect("parse deferred state");
        assert_eq!(
            deferred.sessions.len(),
            1,
            "the second regular persist should still be waiting for the flusher"
        );

        registry
            .persist_for_shutdown()
            .expect("shutdown persistence");
        let committed: PersistedState =
            serde_json::from_slice(&std::fs::read(&state_file).expect("read committed state"))
                .expect("parse committed state");
        assert_eq!(
            committed
                .sessions
                .iter()
                .map(|record| record.id.0.as_str())
                .collect::<Vec<_>>(),
            ["before", "latest"]
        );
    }

    #[test]
    fn failed_worktree_persistence_rolls_back_every_metadata_field() {
        let temp = tempfile::tempdir().expect("temp");
        let blocked_parent = temp.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"file").expect("blocking file");
        let mut registry = Registry::new(engine(), blocked_parent.join("state.json"));
        let mut original = record("s_move");
        original.cwd = "/repo/main".into();
        original.worktree_path = Some("/repo/main".into());
        original.git_branch = Some("main".into());
        original.updated_at = DateMillis(42.0);
        registry.records.insert("s_move".into(), original.clone());

        let error = registry
            .reparent_worktree("s_move", "/repo/feature".into(), Some("feature".into()))
            .expect_err("unwritable state path must fail");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory
            ),
            "unexpected error: {error}"
        );

        let current = registry.records.get("s_move").expect("record");
        assert_eq!(current.cwd, original.cwd);
        assert_eq!(current.worktree_path, original.worktree_path);
        assert_eq!(current.git_branch, original.git_branch);
        assert_eq!(current.updated_at, original.updated_at);
        assert!(!registry.dirty, "failed edit must not be flushed later");
    }

    #[test]
    fn destructive_lifecycle_edits_stop_before_unwritable_state() {
        let temp = tempfile::tempdir().expect("temp");
        let blocked_parent = temp.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"file").expect("blocking file");
        let mut registry = Registry::new(engine(), blocked_parent.join("state.json"));
        let original = record("guarded");
        registry.insert_record(original.clone());

        registry
            .archive("guarded")
            .expect_err("archive must not outrun persistence");
        assert_eq!(registry.records.get("guarded"), Some(&original));

        registry
            .remove("guarded", temp.path())
            .expect_err("remove must not outrun persistence");
        assert_eq!(registry.records.get("guarded"), Some(&original));
        assert!(registry.recently_closed.is_empty());
    }

    #[test]
    fn loading_repairs_same_path_sessions_into_host_scoped_projects() {
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        let mut forge = record("forge");
        forge.cwd = "/srv/app".into();
        forge.host = Some("forge".into());
        let mut build = record("build");
        build.cwd = "/srv/app".into();
        build.host = Some("build".into());
        let state = PersistedState::current(vec![forge, build], Vec::new());
        std::fs::write(&state_file, serde_json::to_vec(&state).expect("encode")).expect("write");

        let mut registry = Registry::new(engine(), &state_file);
        registry.load().expect("load");
        let records = registry.records();
        assert_ne!(records[0].project_id, records[1].project_id);
        assert_eq!(registry.projects_raw().len(), 2);
    }

    /// The project record — not its sessions — is what tells the app which
    /// machine owns a root: after the last session of a remote project is
    /// closed, launch surfaces must still spawn on that host, not locally
    /// with the remote path as cwd. Pre-host records learn theirs on ensure.
    #[test]
    fn projects_record_their_owning_host_and_legacy_records_learn_it() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));

        let local = registry.ensure_session_project("/workspace/app", None);
        assert_eq!(local.get("host"), None);
        let remote = registry.ensure_session_project("/srv/app", Some("forge"));
        assert_eq!(remote["host"], "forge");

        // A record persisted before projects carried hosts: same id, no host.
        let id = session_project_id("/srv/legacy", Some("forge")).0;
        registry
            .projects
            .push(serde_json::json!({ "id": id, "root": "/srv/legacy", "name": "legacy" }));
        let repaired = registry.ensure_session_project("/srv/legacy", Some("forge"));
        assert_eq!(repaired["host"], "forge");
    }

    /// Older records stored `projectID` as the raw directory path instead of a
    /// hashed id. Load recomputes identity, so those are repaired in place
    /// rather than left as a second, path-shaped namespace — and records that
    /// already carry a hashed id keep it, so an existing sidebar does not
    /// fragment into duplicate project rows.
    #[test]
    fn loading_repairs_path_shaped_project_ids_and_leaves_hashed_ones_alone() {
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        let root = "/workspace/app";

        let mut legacy = record("legacy");
        legacy.cwd = root.into();
        legacy.project_id = ProjectId(root.to_owned());
        let mut hashed = record("hashed");
        hashed.cwd = root.into();
        hashed.project_id = session_project_id(root, None);
        let expected = hashed.project_id.clone();

        let state = PersistedState::current(vec![legacy, hashed], Vec::new());
        std::fs::write(&state_file, serde_json::to_vec(&state).expect("encode")).expect("write");

        let mut registry = Registry::new(engine(), &state_file);
        registry.load().expect("load");
        let records = registry.records();
        assert!(
            records.iter().all(|record| record.project_id == expected),
            "both records should share one repaired project identity: {:?}",
            records
                .iter()
                .map(|record| &record.project_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            registry.projects_raw().len(),
            1,
            "the repair must not leave a second project row behind"
        );
    }

    #[test]
    fn loading_keeps_a_linked_worktree_under_its_project_root() {
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        let project_root = "/workspace/app";
        let project_id = session_project_id(project_root, None);
        let mut worktree = record("worktree");
        worktree.cwd = "/workspace/app-feature".into();
        worktree.worktree_path = Some(worktree.cwd.clone());
        worktree.project_id = project_id.clone();
        let state = PersistedState::current(
            vec![worktree],
            vec![serde_json::json!({
                "id": project_id.0,
                "root": project_root,
                "name": "app"
            })],
        );
        std::fs::write(&state_file, serde_json::to_vec(&state).expect("encode")).expect("write");

        let mut registry = Registry::new(engine(), &state_file);
        registry.load().expect("load");
        let loaded = registry.records().pop().expect("record");
        assert_eq!(loaded.project_id, session_project_id(project_root, None));
        assert_eq!(registry.projects_raw().len(), 1);
    }

    /// An exited record whose agent had named its conversation is the case
    /// every Resume affordance gates on, and each of them checks for
    /// `Resumable` — a record left on `Live` reads to all of them as "cannot
    /// be resumed" and the button is never drawn.
    #[test]
    fn a_conversation_that_outlived_its_session_reports_resumable() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));

        let mut dead = record("s_dead");
        dead.kind = AgentKind::CLAUDE_CODE;
        dead.agent_session_id = Some("conv-1".into());
        dead.resumability = Resumability::Live;
        dead.status = SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(255),
            signal: None,
        });
        registry.records.insert("s_dead".into(), dead);

        assert_eq!(
            registry.record("s_dead").expect("record").resumability,
            Resumability::Resumable
        );
    }

    /// The machine-death case. Holders die with the Mac, so the records they
    /// were reporting for come back saying `Working` with nobody behind them.
    /// Left alone they read as running to every consumer: the app dials a
    /// socket that will never answer and spins "Reconnecting terminal…"
    /// forever, and no Resume is offered because the session still looks live.
    #[test]
    fn a_local_session_whose_holder_died_with_the_machine_is_reaped_into_resumable() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));

        let mut orphan = record("s_orphan");
        orphan.kind = AgentKind::CLAUDE_CODE;
        orphan.agent_session_id = Some("conv-1".into());
        orphan.resumability = Resumability::Live;
        orphan.status = SessionStatus::Working;
        registry.records.insert("s_orphan".into(), orphan);

        // No holder sockets: exactly what an empty holders dir looks like
        // after the machine that owned them went down.
        let holders_dir = temp.path().join("holders");
        std::fs::create_dir_all(&holders_dir).expect("holders dir");
        let holder = HolderConfig {
            holders_dir,
            executable: temp.path().join("diri-holder"),
        };
        assert!(registry.restore(&holder, temp.path()).is_empty());

        let reaped = registry.record("s_orphan").expect("record");
        assert!(matches!(reaped.status, SessionStatus::Exited(_)));
        assert_eq!(reaped.resumability, Resumability::Resumable);
    }

    /// Remote sessions live in authenticated Holders on another machine: they
    /// outlive this daemon and this Mac, so the reap pass must not touch them.
    /// Marking one exited would strand still-running work behind a Resume
    /// button that starts a second agent on top of the first.
    #[test]
    fn a_remote_session_survives_the_reap() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));

        let mut remote = record("s_remote");
        remote.kind = AgentKind::CLAUDE_CODE;
        remote.host = Some("forge".into());
        remote.status = SessionStatus::Working;
        registry.records.insert("s_remote".into(), remote);

        let holders_dir = temp.path().join("holders");
        std::fs::create_dir_all(&holders_dir).expect("holders dir");
        let holder = HolderConfig {
            holders_dir,
            executable: temp.path().join("diri-holder"),
        };
        registry.restore(&holder, temp.path());

        assert_eq!(
            registry.record("s_remote").expect("record").status,
            SessionStatus::Working
        );
    }

    /// Without a conversation id there is nothing to re-enter, and offering
    /// Resume would only produce an agent that fails to launch.
    #[test]
    fn an_exited_session_with_no_conversation_id_is_not_resumable() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));

        let mut dead = record("s_dead");
        dead.kind = AgentKind::CLAUDE_CODE;
        dead.resumability = Resumability::Live;
        dead.status = SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        registry.records.insert("s_dead".into(), dead);

        assert_eq!(
            registry.record("s_dead").expect("record").resumability,
            Resumability::NotResumable
        );
    }

    /// A running session keeps saying `Live`: resumability only becomes a
    /// question once the agent is gone.
    #[test]
    fn a_running_session_keeps_reporting_live() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));

        let mut running = record("s_live");
        running.kind = AgentKind::CLAUDE_CODE;
        running.agent_session_id = Some("conv-1".into());
        running.resumability = Resumability::Live;
        running.status = SessionStatus::Idle;
        registry.records.insert("s_live".into(), running);

        assert_eq!(
            registry.record("s_live").expect("record").resumability,
            Resumability::Live
        );
    }

    /// Interop against the state file the Swift daemon actually maintains.
    ///
    /// Ignored by default because it needs a real one. Point
    /// `DIRI_INTEROP_STATE` at a **copy** — never at the live file, which the
    /// running daemon rewrites:
    ///
    /// ```sh
    /// cp "~/Library/Application Support/Dirijor/state.json" /tmp/state.json
    /// DIRI_INTEROP_STATE=/tmp/state.json cargo test -p diri-engine -- --ignored
    /// ```
    #[test]
    #[ignore = "needs DIRI_INTEROP_STATE pointing at a copy of a Swift-written state.json"]
    fn reads_the_state_file_the_swift_daemon_wrote() {
        let Ok(raw) = std::env::var("DIRI_INTEROP_STATE") else {
            eprintln!("skipped: DIRI_INTEROP_STATE is not set");
            return;
        };
        let path = PathBuf::from(raw);
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
        let session_count = original["sessions"].as_array().map_or(0, Vec::len);
        let project_count = original["projects"].as_array().map_or(0, Vec::len);
        assert!(session_count > 0, "pick a state file with sessions in it");

        let temp = tempfile::tempdir().expect("temp");
        let working = temp.path().join("state.json");
        std::fs::copy(&path, &working).expect("copy");

        let mut registry = Registry::new(engine(), &working);
        assert_eq!(
            registry.load().expect("the real state file must parse"),
            session_count,
            "every session record should survive the round trip"
        );

        // Writing it back must not lose anything the Swift daemon owns.
        registry.persist().expect("persist");
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&working).expect("read")).expect("parse");
        assert_eq!(rewritten["version"], 1);
        assert_eq!(
            rewritten["projects"].as_array().map_or(0, Vec::len),
            project_count,
            "projects this engine does not model must be carried through"
        );
        assert_eq!(
            rewritten["sessions"].as_array().map_or(0, Vec::len),
            session_count
        );
    }

    #[test]
    fn a_missing_state_file_is_a_fresh_start_not_an_error() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("absent.json"));
        assert_eq!(registry.load().expect("load"), 0);
    }

    #[test]
    fn an_unparseable_state_file_is_quarantined_rather_than_overwritten() {
        // Treating a corrupt file as a fresh install would erase every session
        // record on the next write.
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        std::fs::write(&state_file, b"{ not json").expect("write");

        let mut registry = Registry::new(engine(), &state_file);
        let error = registry.load().expect_err("corrupt state must be an error");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        assert!(
            temp.path().join("state.json.corrupt").exists(),
            "the unreadable file should still be recoverable by hand"
        );
    }

    #[test]
    fn unknown_projects_survive_a_write() {
        // Additive fields outside the minimal Project model are not discarded.
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        std::fs::write(
            &state_file,
            br#"{"version":1,"projects":[{"id":"p1","name":"keep me"}],"sessions":[]}"#,
        )
        .expect("write");

        let mut registry = Registry::new(engine(), &state_file);
        registry.load().expect("load");
        registry.persist().expect("persist");

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_file).expect("read")).expect("parse");
        assert_eq!(raw["projects"][0]["name"], "keep me");
    }

    #[test]
    fn unknown_top_level_state_survives_a_registry_write() {
        let temp = tempfile::tempdir().expect("temp");
        let state_file = temp.path().join("state.json");
        std::fs::write(
            &state_file,
            br#"{"version":1,"projects":[],"sessions":[],"future":{"theme":"plum"}}"#,
        )
        .expect("write");

        let mut registry = Registry::new(engine(), &state_file);
        registry.load().expect("load");
        registry.insert_record(record("new"));
        registry.persist().expect("persist");

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_file).expect("read")).expect("parse");
        assert_eq!(raw["future"], serde_json::json!({"theme": "plum"}));
        assert_eq!(raw["sessions"][0]["id"], "new");
    }

    #[test]
    fn project_identity_includes_the_execution_host() {
        let local = session_project_id("/workspace/app", None);
        let forge = session_project_id("/workspace/app", Some("forge"));
        let build = session_project_id("/workspace/app", Some("build"));
        assert_ne!(local, forge);
        assert_ne!(forge, build);
        assert_eq!(forge, session_project_id("/workspace/app", Some("forge")));
    }

    #[test]
    fn live_claude_metadata_promotes_the_generated_conversation_title() {
        let temp = tempfile::tempdir().expect("temp");
        let agent_id = "0199f2c4-1a2b-4c3d-8e9f-000000000009";
        let transcript = temp
            .path()
            .join(".claude/projects/-tmp")
            .join(format!("{agent_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"message\":{\"content\":\"vague prompt\"}}\n\
             {\"type\":\"ai-title\",\"aiTitle\":\"Repair remote session recovery\"}\n",
        )
        .expect("write transcript");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("claude");
        session.kind = AgentKind::CLAUDE_CODE;
        session.agent_session_id = Some(agent_id.to_owned());
        session.title = "vague prompt".to_owned();
        session.title_source = TitleSource::FirstPrompt;
        registry.insert_record(session);

        assert!(registry.apply_hook_metadata_with_home(
            "claude",
            &crate::hooks::HookMetadata {
                transcript_path: Some(transcript.to_string_lossy().into_owned()),
                ..crate::hooks::HookMetadata::default()
            },
            Some(temp.path()),
        ));

        let updated = registry.record("claude").expect("record");
        assert_eq!(updated.title, "Repair remote session recovery");
        assert_eq!(updated.title_source, TitleSource::AgentProvided);
    }

    #[test]
    fn codex_subagent_notify_does_not_replace_the_parent_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".codex/sessions/2026/09/15");
        std::fs::create_dir_all(&root).unwrap();
        let parent = root.join("rollout-now-parent.jsonl");
        std::fs::write(
            &parent,
            serde_json::json!({
                "type": "session_meta", "payload": {
                    "id": "parent", "cwd": "/tmp", "source": "cli"
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.join("rollout-now-child.jsonl"), serde_json::json!({
            "type": "session_meta", "payload": {
                "id": "child", "cwd": "/tmp", "parent_thread_id": "parent",
                "source": {"subagent": {"thread_spawn": {"parent_thread_id": "parent", "depth": 1}}}
            }
        }).to_string()).unwrap();
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("codex");
        session.kind = AgentKind::CODEX;
        session.agent_session_id = Some("parent".into());
        session.transcript_path = Some(parent.to_string_lossy().into_owned());
        registry.insert_record(session);
        let (_, metadata) = crate::hooks::parse_codex_notify(&serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "child",
            "input-messages": ["child task"]
        }))
        .unwrap();
        registry.apply_hook_metadata_with_home("codex", &metadata, Some(temp.path()));
        let updated = registry.record("codex").unwrap();
        assert_eq!(updated.agent_session_id.as_deref(), Some("parent"));
        assert_eq!(updated.transcript_path.as_deref(), parent.to_str());
        assert_ne!(updated.title, "child task");

        // A child can finish before the parent's first notify establishes its
        // identity. Transcript evidence must reject that callback as well.
        let mut unbound = updated.clone();
        unbound.agent_session_id = None;
        unbound.transcript_path = None;
        registry.insert_record(unbound);
        assert_eq!(
            registry.accept_hook_metadata("codex", &metadata, Some(temp.path())),
            None
        );
        assert!(registry.record("codex").unwrap().agent_session_id.is_none());

        // An older Engine may already have persisted the child identity.
        let mut damaged = updated;
        damaged.agent_session_id = Some("child".into());
        damaged.transcript_path = Some(
            root.join("rollout-now-child.jsonl")
                .to_string_lossy()
                .into_owned(),
        );
        registry.insert_record(damaged);
        registry.persist_now().unwrap();
        let mut reloaded = Registry::new(engine(), temp.path().join("state.json"));
        reloaded.load_with_home(Some(temp.path())).unwrap();
        let mut damaged = reloaded.record("codex").unwrap();
        assert_eq!(damaged.agent_session_id.as_deref(), Some("parent"));
        assert_eq!(damaged.transcript_path.as_deref(), parent.to_str());
        assert!(!repair_codex_conversation(&mut damaged, temp.path()));
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("state.json")).unwrap())
                .unwrap();
        assert_eq!(disk["sessions"][0]["agentSessionID"], "parent");
        assert_eq!(
            reloaded
                .recovery_store("codex")
                .read_capsule()
                .unwrap()
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some("parent")
        );

        // A missing/unreadable rollout cannot prove that a new ID is a root.
        let missing = crate::hooks::HookMetadata {
            agent_session_id: Some("unverified-child".into()),
            ..Default::default()
        };
        assert!(!reloaded.apply_hook_metadata_with_home("codex", &missing, Some(temp.path())));
        assert_eq!(
            reloaded
                .record("codex")
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some("parent")
        );
    }

    #[test]
    fn first_codex_notify_associates_the_matching_live_rollout() {
        let temp = tempfile::tempdir().expect("temp");
        let transcript = temp
            .path()
            .join(".codex/sessions/2026/08/13/rollout-now-thread-9.jsonl");
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &transcript,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-9\",\"cwd\":\"/tmp\"}}\n",
        )
        .expect("write transcript");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("codex");
        session.kind = AgentKind::CODEX;
        registry.insert_record(session);

        assert!(registry.apply_hook_metadata_with_home(
            "codex",
            &crate::hooks::HookMetadata {
                agent_session_id: Some("thread-9".to_owned()),
                ..crate::hooks::HookMetadata::default()
            },
            Some(temp.path()),
        ));
        let updated = registry.record("codex").expect("record");
        assert_eq!(updated.agent_session_id.as_deref(), Some("thread-9"));
        assert_eq!(
            updated.transcript_path.as_deref(),
            Some(transcript.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn codex_native_titles_promote_and_follow_rename() {
        let temp = tempfile::tempdir().expect("temp");
        let transcript = temp
            .path()
            .join(".codex/sessions/2026/08/13/rollout-now-thread-9.jsonl");
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &transcript,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-9\",\"cwd\":\"/tmp\"}}\n",
        )
        .expect("write transcript");
        let index = temp.path().join(".codex/session_index.jsonl");
        std::fs::write(
            &index,
            "{\"id\":\"thread-9\",\"thread_name\":\"Repair chat titles\",\"updated_at\":1}\n",
        )
        .expect("write index");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("codex");
        session.kind = AgentKind::CODEX;
        session.agent_session_id = Some("thread-9".to_owned());
        session.title = "initial vague prompt".to_owned();
        session.title_source = TitleSource::FirstPrompt;
        registry.insert_record(session);

        assert!(registry.apply_hook_metadata_with_home(
            "codex",
            &crate::hooks::HookMetadata::default(),
            Some(temp.path()),
        ));
        let titled = registry.record("codex").expect("record");
        assert_eq!(titled.title, "Repair chat titles");
        assert_eq!(titled.title_source, TitleSource::AgentProvided);

        std::fs::write(
            &index,
            "{\"id\":\"thread-9\",\"thread_name\":\"Repair chat titles\",\"updated_at\":1}\n\
             {\"id\":\"thread-9\",\"thread_name\":\"Chosen with slash rename\",\"updated_at\":2}\n",
        )
        .expect("rename index");
        assert!(registry.apply_hook_metadata_with_home(
            "codex",
            &crate::hooks::HookMetadata::default(),
            Some(temp.path()),
        ));
        assert_eq!(
            registry.record("codex").expect("record").title,
            "Chosen with slash rename"
        );
    }

    #[test]
    fn arbitrary_hook_transcript_paths_never_enter_the_record() {
        let temp = tempfile::tempdir().expect("temp");
        let outside = temp.path().join("outside.jsonl");
        std::fs::write(&outside, "{}\n").expect("write");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("claude");
        session.kind = AgentKind::CLAUDE_CODE;
        session.agent_session_id = Some("0199f2c4-1a2b-4c3d-8e9f-000000000009".to_owned());
        registry.insert_record(session);

        assert!(!registry.apply_hook_metadata_with_home(
            "claude",
            &crate::hooks::HookMetadata {
                transcript_path: Some(outside.to_string_lossy().into_owned()),
                ..crate::hooks::HookMetadata::default()
            },
            Some(temp.path()),
        ));
        assert!(
            registry
                .record("claude")
                .expect("record")
                .transcript_path
                .is_none()
        );
    }

    #[test]
    fn legacy_arbitrary_transcript_paths_never_feed_generated_titles() {
        let temp = tempfile::tempdir().expect("temp");
        let outside = temp.path().join("outside.jsonl");
        std::fs::write(
            &outside,
            "{\"type\":\"ai-title\",\"aiTitle\":\"untrusted promoted title\"}\n",
        )
        .expect("write");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("claude");
        session.kind = AgentKind::CLAUDE_CODE;
        session.agent_session_id = Some("0199f2c4-1a2b-4c3d-8e9f-000000000009".to_owned());
        session.transcript_path = Some(outside.to_string_lossy().into_owned());
        session.title = "safe existing title".to_owned();
        session.title_source = TitleSource::FirstPrompt;
        registry.insert_record(session);

        assert!(!registry.apply_hook_metadata_with_home(
            "claude",
            &crate::hooks::HookMetadata::default(),
            Some(temp.path()),
        ));
        let updated = registry.record("claude").expect("record");
        assert_eq!(updated.title, "safe existing title");
        assert_eq!(updated.title_source, TitleSource::FirstPrompt);
    }

    #[test]
    fn pty_titles_are_filtered_fallbacks_and_never_override_user_renames() {
        let view = SessionView {
            remote_connection: None,
            attention_state: None,
            terminal_title: None,
            id: "claude".to_owned(),
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            last_turn_completed_at: None,
            title: Some("Repair remote attach".to_owned()),
            title_source: Some(TitleSource::AgentProvided),
            tail_offset: 0,
            exited: false,
        };
        let mut provisional = record("claude");
        provisional.kind = AgentKind::CLAUDE_CODE;
        fold_session_view(&mut provisional, &view);
        assert_eq!(provisional.title, "Repair remote attach");
        assert_eq!(provisional.title_source, TitleSource::TerminalTitle);

        let mut renamed = record("renamed");
        renamed.kind = AgentKind::CLAUDE_CODE;
        renamed.title = "My fixed title".to_owned();
        renamed.title_source = TitleSource::UserRename;
        fold_session_view(&mut renamed, &view);
        assert_eq!(renamed.title, "My fixed title");

        let mut first_prompt = record("first-prompt");
        first_prompt.kind = AgentKind::CODEX;
        first_prompt.title = "Initial vague request".to_owned();
        first_prompt.title_source = TitleSource::FirstPrompt;
        fold_session_view(&mut first_prompt, &view);
        assert_eq!(first_prompt.title, "Repair remote attach");
        assert_eq!(first_prompt.title_source, TitleSource::TerminalTitle);

        let mut captured_prompt = record("captured-prompt");
        captured_prompt.kind = AgentKind::CODEX;
        let prompt_view = SessionView {
            remote_connection: None,
            title: Some("Implement terminal IME".to_owned()),
            title_source: Some(TitleSource::FirstPrompt),
            ..view.clone()
        };
        fold_session_view(&mut captured_prompt, &prompt_view);
        assert_eq!(captured_prompt.title, "Implement terminal IME");
        assert_eq!(captured_prompt.title_source, TitleSource::FirstPrompt);

        let mut generic = record("generic");
        generic.kind = AgentKind::CODEX;
        generic.cwd = "/work/diri".to_owned();
        let generic_view = SessionView {
            remote_connection: None,
            title: Some("diri".to_owned()),
            ..view
        };
        fold_session_view(&mut generic, &generic_view);
        assert_eq!(generic.title_source, TitleSource::Placeholder);

        let mut decorated = record("decorated");
        decorated.kind = AgentKind::CLAUDE_CODE;
        let decorated_view = SessionView {
            remote_connection: None,
            title: Some("✳ Claude Code".to_owned()),
            ..generic_view.clone()
        };
        fold_session_view(&mut decorated, &decorated_view);
        assert_eq!(decorated.title_source, TitleSource::Placeholder);

        decorated.title = "✳ Claude Code".to_owned();
        decorated.title_source = TitleSource::AgentProvided;
        assert!(repair_persisted_agent_title(&mut decorated));
        assert_eq!(decorated.title, AgentKind::CLAUDE_CODE_ID);
        assert_eq!(decorated.title_source, TitleSource::Placeholder);

        let mut non_cursor_status_suffix = record("non-cursor-status-suffix");
        non_cursor_status_suffix.kind = AgentKind::CLAUDE_CODE;
        non_cursor_status_suffix.title = "Release - Ready".to_owned();
        non_cursor_status_suffix.title_source = TitleSource::AgentProvided;
        assert!(!repair_persisted_agent_title(&mut non_cursor_status_suffix));
        assert_eq!(non_cursor_status_suffix.title, "Release - Ready");
        assert_eq!(
            non_cursor_status_suffix.title_source,
            TitleSource::AgentProvided
        );

        let mut cursor = record("cursor");
        cursor.kind = AgentKind::CURSOR;
        let cursor_ready = SessionView {
            remote_connection: None,
            title: Some("Cursor Agent - \u{2705} Ready".to_owned()),
            title_source: Some(TitleSource::AgentProvided),
            ..generic_view
        };
        fold_session_view(&mut cursor, &cursor_ready);
        assert_eq!(cursor.title_source, TitleSource::Placeholder);

        cursor.title = "Cursor Agent - \u{2705} Ready".to_owned();
        cursor.title_source = TitleSource::AgentProvided;
        let cursor_prompt = SessionView {
            remote_connection: None,
            title: Some("Fix the cursor session title".to_owned()),
            title_source: Some(TitleSource::FirstPrompt),
            ..cursor_ready.clone()
        };
        fold_session_view(&mut cursor, &cursor_prompt);
        assert_eq!(cursor.title, "Fix the cursor session title");
        assert_eq!(cursor.title_source, TitleSource::FirstPrompt);

        cursor.title = "Fix the cursor session title".to_owned();
        cursor.title_source = TitleSource::FirstPrompt;
        let named_working = SessionView {
            remote_connection: None,
            title: Some("Cursor Integration Fix - \u{23f3} Working ...".to_owned()),
            title_source: Some(TitleSource::AgentProvided),
            ..cursor_ready
        };
        fold_session_view(&mut cursor, &named_working);
        assert_eq!(cursor.title, "Fix the cursor session title");
        assert_eq!(cursor.title_source, TitleSource::FirstPrompt);
    }

    #[test]
    fn first_prompt_title_survives_a_later_prompt_after_reconnect() {
        let mut session = record("codex-title");
        session.kind = AgentKind::CODEX;
        session.cwd = "/work/lector".into();
        session.title = "make this in a new worktree from main".into();
        session.title_source = TitleSource::FirstPrompt;
        // A newly attached Session has no captured prompt. Its first input can
        // be a follow-up to the conversation whose title was already saved.
        let view = SessionView {
            remote_connection: None,
            attention_state: None,
            id: session.id.to_string(),
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            last_turn_completed_at: None,
            title: Some("make pr".into()),
            title_source: Some(TitleSource::FirstPrompt),
            terminal_title: Some("Action Required | lector".into()),
            tail_offset: 0,
            exited: false,
        };
        for _ in 0..3 {
            fold_session_view(&mut session, &view);
            assert_eq!(session.title, "make this in a new worktree from main");
            assert_eq!(session.title_source, TitleSource::FirstPrompt);
        }
    }

    #[test]
    fn codex_provider_prompt_refresh_stays_consistent_with_live_records() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("codex-profile");
        std::fs::create_dir_all(&config).unwrap();
        let db = rusqlite::Connection::open(config.join("state_5.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT, title TEXT, first_user_message TEXT);",
        ).unwrap();
        let original = "make this in a new worktree from main";
        db.execute(
            "INSERT INTO threads VALUES ('thread-1', NULL, ?1, ?1)",
            [original],
        )
        .unwrap();
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("codex-title");
        session.kind = AgentKind::CODEX;
        session.agent_session_id = Some("thread-1".into());
        session.account_profile = Some(diri_proto::AgentAccountProfile {
            id: "fixture".into(),
            label: "Fixture".into(),
            agent: "codex".into(),
            host: None,
            config_home: config.to_string_lossy().into_owned(),
            is_default: false,
        });
        // Reproduce a title already overwritten by an older Engine after
        // adoption, while the provider still knows the actual first prompt.
        session.title = "make pr".into();
        session.title_source = TitleSource::FirstPrompt;
        registry
            .spawn(
                SessionSpec {
                    id: "codex-title".into(),
                    pty: crate::PtySpec::new(vec!["/bin/cat".into()], "/tmp"),
                    manifest_id: "codex".into(),
                    authority: crate::Authority::ProcessOnly,
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                session,
            )
            .unwrap();
        registry.sessions["codex-title"]
            .paste_text("make pr")
            .unwrap();
        assert_eq!(
            registry.sessions["codex-title"].view().title.as_deref(),
            Some("make pr")
        );

        for pass in 0..3 {
            // Exercise the refresh and watcher independently, without waiting
            // on their production timers. Both publish session.updated.
            registry.native_title_refresh_at = None;
            let requests = registry.native_title_refresh_requests();
            assert_eq!(requests.len(), 1);
            let refreshed =
                registry.apply_native_title_refreshes(scan_native_title_refreshes(requests));
            assert_eq!(refreshed.len(), usize::from(pass == 0));
            for (_, record) in &refreshed {
                assert_eq!(record.title, original);
            }
            assert_eq!(registry.record("codex-title").unwrap().title, original);
            assert_eq!(registry.records()[0].title, original);
            for (_, record) in registry.changed_since(&mut HashMap::new()) {
                assert_eq!(record.title, original);
            }
            registry.persist_now().unwrap();
            let mut restored = Registry::new(engine(), temp.path().join("state.json"));
            restored.load().unwrap();
            assert_eq!(restored.record("codex-title").unwrap().title, original);
        }
        registry
            .terminate("codex-title", std::time::Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn codex_transient_terminal_titles_never_name_a_conversation() {
        for title in [
            "Action Required | dirijor",
            "renaming... ⠂ | dirijor",
            "Untitled",
        ] {
            let mut session = record("codex-title");
            session.cwd = "/work/dirijor".into();
            session.kind = AgentKind::CODEX;
            session.title = "Fix chat naming".into();
            session.title_source = TitleSource::FirstPrompt;
            let view = SessionView {
                remote_connection: None,
                attention_state: None,
                terminal_title: None,
                id: session.id.to_string(),
                status: SessionStatus::Working,
                status_evidence: None,
                needs_input: None,
                last_turn_completed_at: None,
                title: Some(title.into()),
                title_source: Some(TitleSource::AgentProvided),
                tail_offset: 0,
                exited: false,
            };
            fold_session_view(&mut session, &view);
            assert_eq!(session.title, "Fix chat naming", "OSC title: {title}");
        }
    }

    #[test]
    fn codex_names_follow_terminal_updates_until_a_native_or_manual_name_arrives() {
        let mut session = record("codex-title");
        session.kind = AgentKind::CODEX;
        session.cwd = "/work/anara".into();
        let mut view = SessionView {
            remote_connection: None,
            attention_state: None,
            id: session.id.to_string(),
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            last_turn_completed_at: None,
            title: Some("hey astra, check anara seo, fix it".into()),
            title_source: Some(TitleSource::FirstPrompt),
            terminal_title: Some("renaming... ⠋ | anara".into()),
            tail_offset: 0,
            exited: false,
        };
        fold_session_view(&mut session, &view);
        assert_eq!(session.title, "hey astra, check anara seo, fix it");
        assert_eq!(session.title_source, TitleSource::FirstPrompt);

        for (osc, expected) in [
            ("⠋ Repair Anara SEO | anara", "Repair Anara SEO"),
            ("[ ! ] Action Required | anara", "Repair Anara SEO"),
            ("renaming… ⠙ | anara", "Repair Anara SEO"),
            ("Untitled", "Repair Anara SEO"),
            ("⠙ Audit search indexing | anara", "Audit search indexing"),
        ] {
            view.terminal_title = Some(osc.into());
            fold_session_view(&mut session, &view);
            assert_eq!(session.title, expected);
            assert_eq!(session.title_source, TitleSource::TerminalTitle);
        }

        session.agent_session_id = Some("thread-1".into());
        assert!(apply_native_title(
            &mut session,
            "Full native conversation name"
        ));
        view.terminal_title = Some("Full native convers… | anara".into());
        fold_session_view(&mut session, &view);
        assert_eq!(session.title, "Full native conversation name");
        assert_eq!(session.title_source, TitleSource::AgentProvided);

        assert!(apply_native_title(&mut session, "Ready"));
        fold_session_view(&mut session, &view);
        assert_eq!(
            session.title, "Ready",
            "native names are literal conversation data"
        );

        for source in [TitleSource::UserRename, TitleSource::DirijorAssigned] {
            session.title = "My chosen name".into();
            session.title_source = source;
            fold_session_view(&mut session, &view);
            assert!(!apply_native_title(&mut session, "A later native name"));
            assert_eq!(session.title, "My chosen name");
        }
    }

    #[test]
    fn codex_terminal_input_names_a_session_before_the_first_idle_observation() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut record = record("terminal-input-title");
        record.kind = AgentKind::CODEX;
        registry
            .spawn(
                SessionSpec {
                    id: "terminal-input-title".into(),
                    pty: crate::PtySpec::new(
                        vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            "read -r prompt; read -r done; printf fixture-exit; exit 0".into(),
                        ],
                        "/tmp",
                    ),
                    manifest_id: "codex".into(),
                    authority: crate::Authority::ScreenPrimary,
                    logs_dir: temp.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                record,
            )
            .unwrap();
        let session = &registry.sessions["terminal-input-title"];
        assert_eq!(session.status(), SessionStatus::Starting);
        session
            .write_input(b"\x1b[200~Fix chat naming\x1b[201~")
            .unwrap();
        session.write_input(b"\r").unwrap();
        let record = registry.record("terminal-input-title").unwrap();
        assert_eq!(record.title, "Fix chat naming");
        assert_eq!(record.title_source, TitleSource::FirstPrompt);
        // Do not leave a live shell for Session::drop: Drop stops its reader
        // before kill/wait, which can strand a macOS exiting child with unread
        // PTY bytes. Ask this fixture to finish while its normal pump still
        // drains output and reaps concurrently, then drop only after observed exit.
        let session = &registry.sessions["terminal-input-title"];
        session.write_input(b"fixture complete\r").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !session.view().exited {
            assert!(
                std::time::Instant::now() < deadline,
                "title fixture did not drain and exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn codex_pty_names_update_even_after_the_first_prompt_was_captured() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("pty-title");
        session.kind = AgentKind::CODEX;
        session.cwd = "/tmp".into();
        registry.spawn(SessionSpec {
            id: "pty-title".into(),
            pty: crate::PtySpec::new(vec!["/bin/sh".into(), "-c".into(),
                "read -r prompt; printf '\\033]0;Repair chat naming | tmp\\007'; read -r next; printf '\\033]0;Verify chat naming | tmp\\007'; read -r end".into()], "/tmp"),
            manifest_id: "codex".into(),
            authority: crate::Authority::ProcessOnly,
            logs_dir: temp.path().join("logs"),
            holder: None,
            remote: None,
            defer_launch: false,
        }, session).unwrap();
        let mut published = HashMap::new();
        for (input, expected) in [
            ("please fix these titles", "Repair chat naming"),
            ("verify it", "Verify chat naming"),
        ] {
            registry.sessions["pty-title"]
                .send_text(input, true)
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                registry.changed_since(&mut published);
                if registry.record("pty-title").unwrap().title == expected {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "title never reached {expected}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(
                registry.sessions["pty-title"].view().title_source,
                Some(TitleSource::FirstPrompt)
            );
            assert_eq!(
                registry.record("pty-title").unwrap().title_source,
                TitleSource::TerminalTitle
            );
        }
        registry
            .terminate("pty-title", std::time::Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn codex_stuck_persisted_titles_recover_and_remain_updateable() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state.json");
        let mut registry = Registry::new(engine(), state.clone());
        for (id, title) in [
            ("pending", "renaming... ⠋ | anara"),
            ("action", "Action Required | anara"),
            ("empty", "Untitled"),
            ("named", "⠙ Repair SEO | anara"),
            ("plain", "Repair SEO"),
        ] {
            let mut session = record(id);
            session.kind = AgentKind::CODEX;
            session.cwd = "/work/anara".into();
            session.title = title.into();
            session.title_source = TitleSource::AgentProvided;
            registry.insert_record(session);
        }
        let mut manual = record("manual");
        manual.kind = AgentKind::CODEX;
        manual.title = "Untitled".into();
        manual.title_source = TitleSource::UserRename;
        registry.insert_record(manual);
        registry.persist_now().unwrap();
        let mut restored = Registry::new(engine(), state);
        restored.load().unwrap();
        for id in ["pending", "action", "empty"] {
            assert_eq!(
                restored.record(id).unwrap().title_source,
                TitleSource::Placeholder
            );
        }
        for id in ["named", "plain"] {
            let session = restored.record(id).unwrap();
            assert_eq!(session.title, "Repair SEO");
            assert_eq!(session.title_source, TitleSource::TerminalTitle);
        }
        assert_eq!(restored.record("manual").unwrap().title, "Untitled");
    }

    #[test]
    fn codex_saved_prompt_fallback_never_replaces_a_real_name() {
        let fallback = crate::history::ProviderTitle {
            title: "hey astra can you fix these titles".into(),
            source: TitleSource::FirstPrompt,
        };
        let mut session = record("codex");
        session.kind = AgentKind::CODEX;
        assert!(apply_provider_title(&mut session, &fallback));
        assert_eq!(session.title_source, TitleSource::FirstPrompt);
        session.title_source = TitleSource::AgentProvided;
        assert!(apply_provider_title(&mut session, &fallback));
        assert_eq!(session.title_source, TitleSource::FirstPrompt);
        for source in [
            TitleSource::TerminalTitle,
            TitleSource::AgentProvided,
            TitleSource::UserRename,
            TitleSource::DirijorAssigned,
        ] {
            session.title = "Repair chat naming".into();
            session.title_source = source;
            assert!(!apply_provider_title(&mut session, &fallback));
            assert_eq!(session.title, "Repair chat naming");
        }
    }

    #[test]
    fn codex_prompt_titles_stay_stable_across_native_refresh_and_live_views() {
        for resumed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut registry = Registry::new(engine(), temp.path().join("state.json"));
            let mut record = record("stable-prompt");
            record.kind = AgentKind::CODEX;
            record.agent_session_id = Some("thread-9".into());
            if resumed {
                record.title = "Original conversation prompt".into();
                record.title_source = TitleSource::FirstPrompt;
            }
            registry
                .spawn(
                    SessionSpec {
                        id: "stable-prompt".into(),
                        pty: crate::PtySpec::new(
                            vec!["/bin/sh".into(), "-c".into(), "cat >/dev/null".into()],
                            "/tmp",
                        ),
                        manifest_id: "codex".into(),
                        authority: crate::Authority::ProcessOnly,
                        logs_dir: temp.path().join("logs"),
                        holder: None,
                        remote: None,
                        defer_launch: false,
                    },
                    record,
                )
                .unwrap();
            registry.sessions["stable-prompt"]
                .send_text("Current terminal prompt", true)
                .unwrap();
            let expected = if resumed {
                "Original conversation prompt"
            } else {
                "Current terminal prompt"
            };
            let mut published = HashMap::new();
            registry.changed_since(&mut published);
            assert_eq!(registry.record("stable-prompt").unwrap().title, expected);

            for pass in 0..4 {
                let updates =
                    registry.apply_native_title_refreshes(vec![NativeTitleRefreshResult {
                        request: NativeTitleRefreshRequest {
                            account_profile: None,
                            id: "stable-prompt".into(),
                            kind: AgentKind::CODEX,
                            cwd: "/tmp".into(),
                            agent_session_id: "thread-9".into(),
                            transcript_path: None,
                        },
                        title: Some(crate::history::ProviderTitle {
                            title: "Original conversation prompt".into(),
                            source: TitleSource::FirstPrompt,
                        }),
                    }]);
                let expected = "Original conversation prompt";
                if resumed || pass > 0 {
                    assert!(updates.is_empty(), "unchanged metadata republished a title");
                } else {
                    assert_eq!(updates.len(), 1);
                    assert_eq!(updates[0].1.title, expected);
                }
                assert_eq!(registry.record("stable-prompt").unwrap().title, expected);
                assert_eq!(registry.records()[0].title, expected);
                // Force a watcher publication, including the live view fold.
                published.clear();
                let updates = registry.changed_since(&mut published);
                assert_eq!(updates[0].1.title, expected);
            }
            registry
                .terminate("stable-prompt", std::time::Duration::from_secs(1))
                .unwrap();
        }
    }

    #[test]
    fn native_title_updates_follow_the_provider_but_respect_diri_renames() {
        let mut session = record("codex");
        session.kind = AgentKind::CODEX;
        session.title = "first prompt".into();
        session.title_source = TitleSource::FirstPrompt;

        assert!(apply_native_title(&mut session, "Generated title"));
        assert_eq!(session.title, "Generated title");
        assert_eq!(session.title_source, TitleSource::AgentProvided);

        assert!(apply_native_title(&mut session, "Chosen with slash rename"));
        assert_eq!(session.title, "Chosen with slash rename");

        session.title = "Diri sidebar rename".into();
        session.title_source = TitleSource::UserRename;
        assert!(!apply_native_title(&mut session, "Provider changed again"));
        assert_eq!(session.title, "Diri sidebar rename");
    }

    #[test]
    fn a_cursor_status_title_does_not_block_the_first_prompt_hook() {
        let temp = tempfile::tempdir().expect("temp");
        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("cursor");
        session.kind = AgentKind::CURSOR;
        session.title = "Cursor Agent - \u{2705} Ready".to_owned();
        session.title_source = TitleSource::AgentProvided;
        registry.insert_record(session);

        assert!(registry.apply_hook_metadata(
            "cursor",
            &crate::hooks::HookMetadata {
                first_prompt_title: Some("Rename cursor chats from the first prompt".into()),
                ..crate::hooks::HookMetadata::default()
            }
        ));

        let updated = registry.record("cursor").expect("record");
        assert_eq!(updated.title, "Rename cursor chats from the first prompt");
        assert_eq!(updated.title_source, TitleSource::FirstPrompt);
    }

    #[test]
    fn a_cursor_generated_meta_title_promotes_over_the_first_prompt() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path();
        let cwd = "/Users/alex/GitHub/diri";
        let id = "11111fcb-7655-4342-8b2f-88068c650200";
        let transcripts = home
            .join(".cursor/projects")
            .join("Users-alex-GitHub-diri")
            .join("agent-transcripts")
            .join(id);
        std::fs::create_dir_all(&transcripts).expect("transcripts");
        std::fs::write(transcripts.join(format!("{id}.jsonl")), "{}\n").expect("jsonl");
        let meta_dir = home.join(".cursor/chats/workspace").join(id);
        std::fs::create_dir_all(&meta_dir).expect("meta");
        std::fs::write(
            meta_dir.join("meta.json"),
            format!(
                r#"{{"schemaVersion":1,"createdAtMs":5000,"title":"Cursor Integration Fix","cwd":"{cwd}"}}"#
            ),
        )
        .expect("meta");

        let mut session = record("cursor");
        session.kind = AgentKind::CURSOR;
        session.cwd = cwd.into();
        session.title = "on another pr created from main".into();
        session.title_source = TitleSource::FirstPrompt;
        session.created_at = DateMillis(4_000.0);
        let conversation = crate::history::cursor_conversation(
            home,
            &session.cwd,
            session.agent_session_id.as_deref(),
            session.created_at.0,
            &HashSet::new(),
        )
        .expect("conversation");
        apply_cursor_conversation(&mut session, conversation);
        assert_eq!(session.title, "Cursor Integration Fix");
        assert_eq!(session.title_source, TitleSource::AgentProvided);
        assert_eq!(session.agent_session_id.as_deref(), Some(id));
    }

    #[test]
    fn remote_cursor_metadata_never_reads_the_local_store() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path();
        let cwd = "/srv/diri";
        let id = "11111fcb-7655-4342-8b2f-88068c650200";
        let transcripts = home
            .join(".cursor/projects")
            .join(crate::history::cursor_project_slug(cwd))
            .join("agent-transcripts")
            .join(id);
        std::fs::create_dir_all(&transcripts).expect("transcripts");
        std::fs::write(transcripts.join(format!("{id}.jsonl")), "{}\n").expect("jsonl");
        let meta_dir = home.join(".cursor/chats/workspace").join(id);
        std::fs::create_dir_all(&meta_dir).expect("meta");
        std::fs::write(
            meta_dir.join("meta.json"),
            format!(
                r#"{{"schemaVersion":1,"createdAtMs":5000,"title":"Local conversation","cwd":"{cwd}"}}"#
            ),
        )
        .expect("meta");

        let mut registry = Registry::new(engine(), temp.path().join("state.json"));
        let mut session = record("remote-cursor");
        session.kind = AgentKind::CURSOR;
        session.cwd = cwd.into();
        session.host = Some("forge".into());
        registry.insert_record(session);

        assert!(registry.apply_hook_metadata_with_home(
            "remote-cursor",
            &crate::hooks::HookMetadata {
                agent_session_id: Some(id.into()),
                first_prompt_title: Some("Remote prompt".into()),
                ..crate::hooks::HookMetadata::default()
            },
            Some(home),
        ));

        let updated = registry.record("remote-cursor").expect("record");
        assert_eq!(updated.title, "Remote prompt");
        assert_eq!(updated.title_source, TitleSource::FirstPrompt);
        assert_eq!(updated.transcript_path, None);
    }

    #[test]
    fn completed_turn_time_folds_into_attention_state() {
        let mut session = record("completed");
        session.kind = AgentKind::CLAUDE_CODE;
        session.status = SessionStatus::Working;
        let view = SessionView {
            remote_connection: None,
            attention_state: None,
            terminal_title: None,
            id: "completed".to_owned(),
            status: SessionStatus::Idle,
            status_evidence: None,
            needs_input: None,
            last_turn_completed_at: Some(DateMillis(2_000.0)),
            title: None,
            title_source: None,
            tail_offset: 0,
            exited: false,
        };

        fold_session_status(&mut session, &view);

        assert_eq!(session.last_turn_completed_at, Some(DateMillis(2_000.0)));
        assert_eq!(session.attention(), diri_proto::AttentionLevel::DoneUnseen);
    }
}
