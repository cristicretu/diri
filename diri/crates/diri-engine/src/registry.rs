//! The set of live sessions, and their persisted records.
//!
//! The registry is what a control channel talks to: spawn, list, write, kill.
//! It also owns persistence, in the same `state.json` shape the Swift daemon
//! writes — `{ version, projects, sessions }` — so the two engines can read
//! each other's state file and a switch does not lose anybody's session list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diri_proto::{DateMillis, SessionRecord, SessionStatus, TitleSource};
use serde::{Deserialize, Serialize};

use crate::detect::ManifestEngine;
use crate::holder::{HolderClient, HolderManagerPaths, HolderPaths};
use crate::session::{HolderConfig, Session, SessionSpec, SessionView};

/// The on-disk snapshot. Field names and the version match the Swift
/// `PersistedState` exactly.
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
    /// Records for sessions that are no longer live but still listed.
    records: HashMap<String, SessionRecord>,
    /// Carried through untouched: this engine has no project model yet, and
    /// dropping the key would erase the Swift daemon's projects on first write.
    projects: Vec<serde_json::Value>,
    /// Sessions the user closed, newest last — the "reopen closed tab" stack.
    recently_closed: Vec<SessionRecord>,
    state_file: PathBuf,
    /// Trailing-edge persistence: a mutation inside the debounce window marks
    /// dirty instead of rewriting the whole file (mark-seen fires on every
    /// tab switch), and the flusher or the next persist call writes it out.
    dirty: bool,
    last_persist: Option<std::time::Instant>,
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
        Self {
            engine,
            sessions: HashMap::new(),
            records: HashMap::new(),
            projects: Vec::new(),
            recently_closed: Vec::new(),
            state_file: state_file.into(),
            dirty: false,
            last_persist: None,
        }
    }

    /// Loads a persisted state file.
    ///
    /// A file that exists but will not parse is quarantined rather than
    /// ignored: treating it as a fresh install would make the next write
    /// overwrite every session record the user had.
    pub fn load(&mut self) -> std::io::Result<usize> {
        let bytes = match std::fs::read(&self.state_file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        match serde_json::from_slice::<PersistedState>(&bytes) {
            Ok(state) => {
                self.projects = state.projects;
                for record in state.sessions {
                    self.records.insert(record.id.0.clone(), record);
                }
                Ok(self.records.len())
            }
            Err(error) => {
                let quarantine = self.state_file.with_extension("json.corrupt");
                let _ = std::fs::rename(&self.state_file, &quarantine);
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

    /// Writes out a deferred persist, if one is pending.
    pub fn flush_dirty(&mut self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.persist_now()
    }

    /// Writes the current state atomically, unconditionally.
    fn persist_now(&mut self) -> std::io::Result<()> {
        let state = PersistedState::current(self.records(), self.projects.clone());
        let bytes = serde_json::to_vec(&state)?;
        let temp = self.state_file.with_extension("json.tmp");
        if let Some(parent) = self.state_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&temp, &bytes)?;
        // Rename is atomic, so a crash mid-write cannot truncate the real file.
        std::fs::rename(&temp, &self.state_file)?;
        self.dirty = false;
        self.last_persist = Some(std::time::Instant::now());
        Ok(())
    }

    /// Adds (or replaces) a record without a live session — restores,
    /// imports, and tests use this; live sessions come from [`spawn`].
    ///
    /// [`spawn`]: Registry::spawn
    pub fn insert_record(&mut self, record: SessionRecord) {
        self.records.insert(record.id.0.clone(), record);
    }

    /// Starts a session and takes ownership of it.
    pub fn spawn(&mut self, spec: SessionSpec, record: SessionRecord) -> std::io::Result<String> {
        let id = spec.id.clone();
        let session = Session::spawn(spec, Arc::clone(&self.engine))?;
        self.records.insert(id.clone(), record);
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// Adopts every still-live holder-owned session found under
    /// `holder.holders_dir` that has a persisted record. Call after [`load`]:
    /// this is what makes sessions survive a daemon restart — or the switch
    /// from the Swift daemon to this one.
    ///
    /// Returns the ids adopted. Sessions whose holder is gone are left as
    /// records only, exactly as [`load`] left them.
    ///
    /// [`load`]: Registry::load
    pub fn restore(&mut self, holder: &HolderConfig, logs_dir: &Path) -> Vec<String> {
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
            let Some(record) = self.records.get(&session_id) else {
                continue; // a holder without a record is not ours to run
            };
            if self.sessions.contains_key(&session_id) {
                continue;
            }
            let paths = HolderPaths::new(&holder.holders_dir, &session_id);
            let client = HolderClient::new(paths.socket());
            let Ok(stat) = client.stat() else { continue };
            if !stat.alive {
                continue;
            }
            let manifest_id = record.kind.id().to_string();
            let record_status = record.status.clone();
            let record_needs_input = record.needs_input.clone();
            let record_hibernated = record.hibernation.is_some();
            let spec = SessionSpec {
                id: session_id.clone(),
                // The holder owns the real spec; this one only shapes the
                // emulator until stat's dimensions overwrite it in `adopt`.
                pty: crate::pty::PtySpec::new(Vec::new(), record.cwd.clone()),
                manifest_id: manifest_id.clone(),
                authority: crate::session::authority_for(&manifest_id, &self.engine),
                logs_dir: logs_dir.to_path_buf(),
                holder: Some(holder.clone()),
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
                    adopted.push(session_id);
                }
                Err(_) => continue,
            }
        }
        adopted
    }

    /// The manifest engine these sessions were started with.
    pub fn engine(&self) -> Arc<ManifestEngine> {
        Arc::clone(&self.engine)
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn views(&self) -> Vec<SessionView> {
        let mut views: Vec<_> = self.sessions.values().map(Session::view).collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }

    /// Session records with live status folded in.
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
    /// status, and the resumability that follows from it.
    fn fold_live(&self, record: &mut SessionRecord) {
        if let Some(session) = self.sessions.get(&record.id.0) {
            let view = session.view();
            record.status = view.status;
            record.needs_input = view.needs_input;
        }
        // `Live` only records that the agent named its conversation while it
        // was running. Once the session is gone the question every Resume
        // affordance asks is a different one — can that conversation be
        // re-entered — so answer it here rather than leaving a stale `Live`
        // that reads as "not resumable" to each of them.
        if matches!(record.status, SessionStatus::Exited(_))
            && record.resumability == diri_proto::Resumability::Live
        {
            record.resumability = if self.can_reenter(record) {
                diri_proto::Resumability::Resumable
            } else {
                diri_proto::Resumability::NotResumable
            };
        }
    }

    /// Whether this record's agent can be relaunched back into its own
    /// conversation — a known conversation id plus a manifest that declares
    /// how to resume one.
    fn can_reenter(&self, record: &SessionRecord) -> bool {
        let Some(agent_session_id) = record.agent_session_id.as_deref() else {
            return false;
        };
        self.engine
            .manifest(record.kind.id())
            .and_then(|manifest| manifest.agent.as_ref())
            .and_then(|agent| agent.resume_args(Some(agent_session_id)))
            .is_some()
    }

    /// Diffs live sessions' state versions against `published` (updating it in
    /// place) and returns folded records for just the sessions that changed.
    /// The steady-state cost — the events watcher polls this several times a
    /// second — is one integer compare per live session: no clones, no
    /// serialization.
    pub fn changed_since(
        &self,
        published: &mut HashMap<String, u64>,
    ) -> Vec<(String, SessionRecord)> {
        published.retain(|id, _| self.sessions.contains_key(id));
        let mut changed = Vec::new();
        for (id, session) in &self.sessions {
            let version = session.state_version();
            if published.get(id) == Some(&version) {
                continue;
            }
            published.insert(id.clone(), version);
            if let Some(record) = self.record(id) {
                changed.push((id.clone(), record));
            }
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
        let exit = session.terminate(grace)?;
        if let Some(record) = self.records.get_mut(id) {
            record.status = SessionStatus::Exited(diri_proto::ExitInfo {
                reason: match exit {
                    crate::pty::Exit::Signal(_) => diri_proto::ExitReason::Signaled,
                    crate::pty::Exit::Code(_) => diri_proto::ExitReason::Exited,
                },
                code: match exit {
                    crate::pty::Exit::Code(code) => Some(code),
                    crate::pty::Exit::Signal(_) => None,
                },
                signal: match exit {
                    crate::pty::Exit::Signal(signal) => Some(signal),
                    crate::pty::Exit::Code(_) => None,
                },
            });
        }
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
        if self.sessions.contains_key(id) {
            let _ = self.terminate(id, std::time::Duration::from_millis(500));
        }
        let Some(record) = self.records.remove(id) else {
            return Err(not_found(id));
        };
        self.recently_closed.push(record);
        if self.recently_closed.len() > 10 {
            self.recently_closed.remove(0);
        }
        self.sessions.remove(id);
        let _ = std::fs::remove_file(logs_dir.join(format!("{id}.bin")));
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
        let session = Session::spawn(spec, Arc::clone(&self.engine))?;
        self.sessions.insert(id.clone(), session);
        let record = self.records.get_mut(&id).expect("checked above");
        record.status = SessionStatus::Starting;
        record.needs_input = None;
        record.updated_at = DateMillis::from(std::time::SystemTime::now());
        Ok(())
    }

    /// SIGCONTs a hibernated session's tree, flushes any input queued while
    /// it was frozen, and clears the record. A no-op for awake sessions, so
    /// every input path can call it unconditionally.
    pub fn wake_session(&mut self, id: &str) -> std::io::Result<()> {
        let hibernated = self
            .records
            .get(id)
            .is_some_and(|record| record.hibernation.is_some())
            || self.sessions.get(id).is_some_and(Session::is_hibernated);
        if !hibernated {
            return Ok(());
        }
        if let Some(session) = self.sessions.get(id) {
            session.signal_tree(libc::SIGCONT)?;
            // Flush AFTER the CONT so the tree is drinking again.
            let _ = session.set_hibernated(false);
        }
        self.set_hibernation(id, None);
        Ok(())
    }

    /// Folds identity a hook payload carried into the record: the agent-side
    /// conversation id (what makes resume possible), the live transcript path
    /// (it MOVES when the agent enters a worktree), and a first-prompt title
    /// when nothing better has been assigned. Returns whether anything
    /// changed.
    pub fn apply_hook_metadata(&mut self, id: &str, meta: &crate::hooks::HookMetadata) -> bool {
        let Some(record) = self.records.get_mut(id) else {
            return false;
        };
        let mut changed = false;
        if let Some(agent_id) = &meta.agent_session_id
            && record.agent_session_id.as_ref() != Some(agent_id)
        {
            record.agent_session_id = Some(agent_id.clone());
            record.resumability = diri_proto::Resumability::Live;
            changed = true;
        }
        if let Some(transcript) = &meta.transcript_path
            && record.transcript_path.as_ref() != Some(transcript)
        {
            record.transcript_path = Some(transcript.clone());
            changed = true;
        }
        if let Some(title) = &meta.first_prompt_title
            && record.title_source == TitleSource::Placeholder
        {
            record.title = title.clone();
            record.title_source = TitleSource::FirstPrompt;
            changed = true;
        }
        if changed {
            record.updated_at = DateMillis::from(std::time::SystemTime::now());
        }
        changed
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

    /// Upserts a project by its deterministic root-derived id and returns it
    /// as wire JSON. The id rule matches Swift's `ProjectID(root:)` FNV-1a,
    /// so re-adding a folder either engine already listed never duplicates.
    pub fn add_project(&mut self, root: &str) -> serde_json::Value {
        let id = project_id(root);
        if let Some(existing) = self
            .projects
            .iter()
            .find(|project| project.get("id").and_then(|value| value.as_str()) == Some(&id))
        {
            return existing.clone();
        }
        let name = Path::new(root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string());
        let project = serde_json::json!({ "id": id, "root": root, "name": name });
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

    pub fn mark_seen(&mut self, id: &str) -> std::io::Result<()> {
        let record = self.records.get_mut(id).ok_or_else(|| not_found(id))?;
        record.last_seen_at = Some(DateMillis::from(std::time::SystemTime::now()));
        Ok(())
    }

    /// Ends the session but keeps its record on the shelf: kill-tree,
    /// keep-record, stamp `archivedAt`.
    pub fn archive(&mut self, id: &str) -> std::io::Result<()> {
        if !self.records.contains_key(id) {
            return Err(not_found(id));
        }
        if self.sessions.contains_key(id) {
            let _ = self.terminate(id, std::time::Duration::from_millis(500));
        }
        let record = self.records.get_mut(id).expect("checked above");
        record.archived_at = Some(DateMillis::from(std::time::SystemTime::now()));
        if !matches!(record.status, SessionStatus::Exited(_)) {
            record.status = SessionStatus::Exited(diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::Archived,
                code: None,
                signal: None,
            });
        }
        record.needs_input = None;
        Ok(())
    }

    pub fn unarchive(&mut self, id: &str) -> std::io::Result<()> {
        let record = self.records.get_mut(id).ok_or_else(|| not_found(id))?;
        if record.archived_at.is_none() {
            return Ok(());
        }
        record.archived_at = None;
        record.updated_at = DateMillis::from(std::time::SystemTime::now());
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

    /// The project list, verbatim as loaded — this engine does not model
    /// projects yet, but the list response carries them.
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
        &self.state_file
    }
}

fn not_found(id: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, format!("no session {id}"))
}

/// Swift's `ProjectID(root:)`: FNV-1a-shaped hash over the root, low 48 bits
/// as hex. The multiplier is Swift's literal `0x1000_0000_01b3` — NOT the
/// classic FNV prime (one extra zero) — and must stay byte-identical or the
/// same folder gets a second project id after an engine switch.
fn project_id(root: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_1000_0000_01B3);
    }
    format!("p_{:012x}", hash & 0xFFFF_FFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{AgentKind, DateMillis, ProjectId, Resumability, SessionId, TitleSource};

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            id: SessionId(id.into()),
            kind: AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: TitleSource::Placeholder,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Starting,
            needs_input: None,
            resumability: Resumability::NotResumable,
            parent: None,
            created_at: DateMillis(0.0),
            updated_at: DateMillis(0.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            remote_active: false,
            host: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    fn engine() -> Arc<ManifestEngine> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../Sources/DirijorCore/Resources/manifests")
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
        Arc::new(engine)
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
        // This engine has no project model yet. Dropping the key would erase
        // the Swift daemon's projects the first time the Rust one persisted.
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
}
