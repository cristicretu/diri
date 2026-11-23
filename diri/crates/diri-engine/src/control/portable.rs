//! Portable profile sync and Mac-to-Mac migration (`sync.*`, `migrate.*`).
//!
//! The work is in `diri-portable`; the Engine supplies what it owns (session
//! records, projects, schedules) and applies what comes back: records into
//! the Registry, schedules into their store, preferences into a pending file
//! the app merges (the app owns `prefs.json`). Sync off costs nothing: no
//! watcher, thread or timer exists until a location is configured with
//! automatic sync on.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use diri_portable::bundle::KdfParams;
use diri_portable::docs::{self, LocalProject};
use diri_portable::files::write_private;
use diri_portable::migrate::{ExportInput, ImportTarget};
use diri_portable::pathmap::{PathMap, expand};
use diri_portable::sync::{LocalProfile, Snapshot, SyncInput};
use diri_proto::DateMillis;
use diri_proto::portable::{
    MigrateExportParams, MigrateImportParams, PENDING_PREFS_FILE_NAME, SyncBackendKind, SyncConfig,
    SyncConfigureParams, SyncStatus, SyncSummary,
};
use notify::{RecursiveMode, Watcher};
use serde_json::{Map, Value};

use super::{ControlError, ControlServer, JsonValue, decode, encode, io_control_error, poisoned};

/// Quiet period after the last local change before an automatic sync.
const AUTO_SYNC_DEBOUNCE: Duration = Duration::from_secs(20);
/// Changes this soon after a sync are its own writes.
const OWN_WRITES: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct PortableState {
    /// Fixture home for tests; `None` is the user's home.
    home: Option<PathBuf>,
    status: Mutex<Option<SyncStatus>>,
    /// One sync or migration at a time.
    work: Mutex<()>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    quiet_until: Mutex<Option<Instant>>,
    server: OnceLock<Weak<ControlServer>>,
}

impl PortableState {
    pub(super) fn with_home(home: PathBuf) -> Self {
        Self {
            home: Some(home),
            ..Self::default()
        }
    }
}

fn device_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let run = |program: &str, args: &[&str]| {
            std::process::Command::new(program)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|name| !name.is_empty())
        };
        #[cfg(target_os = "macos")]
        if let Some(name) = run("/usr/sbin/scutil", &["--get", "ComputerName"]) {
            return name;
        }
        run("hostname", &[]).unwrap_or_else(|| "This Mac".to_owned())
    })
    .clone()
}

fn now_millis() -> DateMillis {
    DateMillis::from(std::time::SystemTime::now())
}

fn bundle_error(error: diri_portable::bundle::BundleError) -> ControlError {
    match error {
        diri_portable::bundle::BundleError::Io(error) => io_control_error(error),
        other => ControlError::new(other.code(), other.to_string()),
    }
}

impl ControlServer {
    fn portable_home(&self) -> Result<PathBuf, ControlError> {
        self.portable
            .home
            .clone()
            .or_else(diri_platform::home_dir)
            .ok_or_else(|| ControlError::internal("HOME is not set"))
    }

    fn sync_dir(&self) -> PathBuf {
        self.state_dir.join("sync")
    }

    fn local_profile(&self, home: &Path) -> LocalProfile {
        let init_dir = ["DIRI_CONFIG_DIR", "XDG_CONFIG_HOME"]
            .iter()
            .filter(|_| self.portable.home.is_none())
            .find_map(|name| {
                std::env::var_os(name)
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|path| {
                        if *name == "XDG_CONFIG_HOME" {
                            path.join("diri")
                        } else {
                            path
                        }
                    })
            })
            .unwrap_or_else(|| home.join(".config/diri"));
        let app_support = if self.portable.home.is_some() {
            self.config_dir.clone()
        } else {
            diri_proto::paths::DirijorPaths::app_support(home)
        };
        LocalProfile {
            notes_dir: self
                .resolved_notes_dir()
                .unwrap_or_else(|| app_support.join("notes")),
            app_skills_dir: app_support.join("computer-use/app-skills"),
            init_lua: Some(init_dir.join("init.lua")),
            overrides_dir: self
                .config_dir
                .join(diri_proto::paths::MANIFEST_OVERRIDES_RELATIVE_PATH),
        }
    }

    fn prefs_path(&self, home: &Path) -> PathBuf {
        if self.portable.home.is_some() {
            return self.config_dir.join(diri_proto::paths::PREFS_FILE_NAME);
        }
        diri_proto::paths::DirijorPaths::prefs_file(home)
    }

    fn read_prefs(&self, home: &Path) -> Map<String, Value> {
        std::fs::read(self.prefs_path(home))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    /// Leaves portable preferences for the app beside `prefs.json`. A pending
    /// file the app has not applied yet is folded in, not replaced.
    fn write_pending_prefs(
        &self,
        home: &Path,
        pending: Map<String, Value>,
    ) -> Result<(), ControlError> {
        let path = self
            .prefs_path(home)
            .with_file_name(PENDING_PREFS_FILE_NAME);
        let mut merged = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        diri_proto::portable::overlay_portable_prefs(&mut merged, &pending);
        let bytes = serde_json::to_vec_pretty(&Value::Object(merged))
            .map_err(|_| ControlError::internal("cannot encode preferences"))?;
        write_private(&path, &bytes).map_err(io_control_error)
    }

    fn local_projects(&self) -> Result<Vec<LocalProject>, ControlError> {
        let registry = self.registry.lock().map_err(poisoned)?;
        Ok(registry
            .projects_raw()
            .iter()
            .filter(|project| project.get("host").is_none())
            .filter_map(|project| {
                Some(LocalProject {
                    id: project.get("id")?.as_str()?.to_owned(),
                    root: project.get("root")?.as_str()?.to_owned(),
                    name: project
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            })
            .collect())
    }

    /// The Engine-owned profile documents in portable form.
    fn portable_docs(
        &self,
        home: &Path,
    ) -> Result<(Snapshot, Vec<diri_proto::portable::SkippedItem>), ControlError> {
        let home_text = home.to_string_lossy().into_owned();
        let projects = self.local_projects()?;
        let (prefs, recipes, mut skipped) =
            docs::export_prefs(&home_text, &self.read_prefs(home), &projects);
        let (schedules, more) = docs::export_schedules(&home_text, &self.portable_schedules()?);
        skipped.extend(more);
        let projects = docs::export_projects(&home_text, &projects);
        let mut snapshot = Snapshot::new();
        for (rel, doc) in [
            ("prefs.json", prefs),
            ("recipes.json", recipes),
            ("schedules.json", schedules),
            ("projects.json", projects),
        ] {
            snapshot.insert(rel.to_owned(), serde_json::to_vec(&doc).unwrap_or_default());
        }
        Ok((snapshot, skipped))
    }

    fn sync_config(&self) -> Option<SyncConfig> {
        std::fs::read(self.sync_dir().join("config.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    fn current_sync_status(&self) -> SyncStatus {
        let mut guard = self.portable.status.lock().expect("sync status");
        let status = guard.get_or_insert_with(|| {
            let mut status: SyncStatus = std::fs::read(self.sync_dir().join("status.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            status.running = false;
            status
        });
        status.config = self.sync_config();
        status.device = device_name();
        status.clone()
    }

    fn store_sync_status(&self, status: &SyncStatus) {
        if let Ok(bytes) = serde_json::to_vec_pretty(status) {
            let _ = write_private(&self.sync_dir().join("status.json"), &bytes);
        }
        *self.portable.status.lock().expect("sync status") = Some(status.clone());
    }

    pub(super) fn sync_status(&self) -> Result<JsonValue, ControlError> {
        encode(&self.current_sync_status())
    }

    pub(super) fn sync_configure(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let SyncConfigureParams { config } = decode(params)?;
        let dir = self.sync_dir();
        let previous = self.sync_config();
        match &config {
            Some(config) => {
                let location = config.location.trim();
                if location.is_empty() {
                    return Err(ControlError::bad_request("Choose where to sync"));
                }
                if config.backend == SyncBackendKind::Folder
                    && !location.starts_with('/')
                    && !location.starts_with('~')
                {
                    return Err(ControlError::bad_request(
                        "Choose a folder (an absolute path or ~/…)",
                    ));
                }
                let config = SyncConfig {
                    location: location.to_owned(),
                    ..config.clone()
                };
                // A new place starts from no shared history.
                if previous
                    .as_ref()
                    .is_none_or(|p| p.backend != config.backend || p.location != config.location)
                {
                    let _ = std::fs::remove_file(dir.join("base.json"));
                }
                let bytes = serde_json::to_vec_pretty(&config)
                    .map_err(|_| ControlError::internal("cannot encode sync settings"))?;
                write_private(&dir.join("config.json"), &bytes).map_err(io_control_error)?;
            }
            None => {
                let _ = std::fs::remove_file(dir.join("config.json"));
                let _ = std::fs::remove_file(dir.join("base.json"));
            }
        }
        self.refresh_auto_sync();
        self.sync_status()
    }

    pub(super) fn sync_now(&self) -> Result<JsonValue, ControlError> {
        if self.sync_config().is_none() {
            return Err(ControlError::bad_request("Set up sync first"));
        }
        let status = self.run_profile_sync();
        encode(&status)
    }

    /// Runs one sync and records its outcome. Never panics on a failed
    /// remote: the error is kept in the status.
    fn run_profile_sync(&self) -> SyncStatus {
        let _work = self.portable.work.lock().expect("portable work");
        let mut status = self.current_sync_status();
        status.running = true;
        *self.portable.status.lock().expect("sync status") = Some(status.clone());
        let result = self.sync_once();
        *self.portable.quiet_until.lock().expect("quiet") = Some(Instant::now() + OWN_WRITES);
        status.running = false;
        match result {
            Ok(summary) => {
                status.last_synced_at = Some(now_millis());
                status.last_summary = Some(summary);
                status.last_error = None;
            }
            Err(error) => {
                eprintln!("diri-engine: profile sync failed: {}", error.message);
                status.last_error = Some(error.message);
            }
        }
        self.store_sync_status(&status);
        status
    }

    fn sync_once(&self) -> Result<SyncSummary, ControlError> {
        let config = self
            .sync_config()
            .ok_or_else(|| ControlError::bad_request("Set up sync first"))?;
        let home = self.portable_home()?;
        let home_text = home.to_string_lossy().into_owned();
        let profile = self.local_profile(&home);
        let (docs_snapshot, _) = self.portable_docs(&home)?;
        let device = device_name();
        let stamp = format!(
            "{}-{}",
            device
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>(),
            now_millis().0 as u64 / 1000
        );
        let base = self.sync_dir().join("base.json");
        let input = SyncInput {
            profile: &profile,
            docs: docs_snapshot,
            base_path: &base,
            device: &device,
            stamp: &stamp,
        };
        let outcome = match config.backend {
            SyncBackendKind::Folder => {
                let root = PathBuf::from(expand(&home_text, &config.location));
                diri_portable::sync::sync_folder(&input, &root)
            }
            SyncBackendKind::Git => {
                let clone = self.sync_dir().join(format!(
                    "repo-{}",
                    &diri_portable::files::sha256_hex(config.location.as_bytes())[..12]
                ));
                diri_portable::sync::sync_git(&input, &expand(&home_text, &config.location), &clone)
            }
        }
        .map_err(io_control_error)?;
        let mut summary = outcome.summary;
        summary.prefs_pending = false;
        self.apply_synced_docs(&home, &outcome.docs, &mut summary)?;
        Ok(summary)
    }

    /// Applies merged documents a sync brought back.
    fn apply_synced_docs(
        &self,
        home: &Path,
        merged: &BTreeMap<String, Value>,
        summary: &mut SyncSummary,
    ) -> Result<(), ControlError> {
        let home_text = home.to_string_lossy().into_owned();
        let map = PathMap::new(&home_text, &home_text);
        let project_id = |root: &str| crate::registry::session_project_id(root, None).0;
        if let Some(doc) = merged.get("projects.json") {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            for root in docs::project_roots(doc, &home_text).into_keys() {
                if Path::new(&root).is_dir() {
                    registry.ensure_session_project(&root, None);
                }
            }
            let _ = registry.persist();
        }
        if let Some(doc) = merged.get("schedules.json") {
            let wanted = docs::import_schedules(doc, &map);
            let ids: HashSet<&str> = wanted.iter().map(|s| s.id.as_str()).collect();
            for schedule in &wanted {
                self.put_portable_schedule(&schedule.id, schedule.spec.clone())?;
            }
            for local in self.portable_schedules()? {
                // Only schedules that travel can be deleted by a sync.
                if local.spec.spawn.host.is_none() && !ids.contains(local.id.as_str()) {
                    self.remove_portable_schedule(&local.id)?;
                }
            }
        }
        if merged.contains_key("prefs.json") || merged.contains_key("recipes.json") {
            let current = self.read_prefs(home);
            let portable = match merged.get("prefs.json") {
                Some(doc) => doc.clone(),
                None => self
                    .portable_docs(home)
                    .ok()
                    .and_then(|(docs, _)| serde_json::from_slice(docs.get("prefs.json")?).ok())
                    .unwrap_or_default(),
            };
            let pending = docs::sync_prefs(
                &portable,
                merged.get("recipes.json"),
                &current,
                &map,
                &project_id,
            );
            if pending
                .iter()
                .any(|(key, value)| current.get(key) != Some(value))
            {
                self.write_pending_prefs(home, pending)?;
                summary.prefs_pending = true;
            }
        }
        Ok(())
    }

    pub(super) fn migrate_export(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let params: MigrateExportParams = decode(params)?;
        let _work = self.portable.work.lock().map_err(poisoned)?;
        let home = self.portable_home()?;
        let home_text = home.to_string_lossy().into_owned();
        let out = PathBuf::from(expand(&home_text, params.path.trim()));
        if !out.is_absolute() {
            return Err(ControlError::bad_request("Choose where to save the backup"));
        }
        let profile = self.local_profile(&home);
        let (docs_snapshot, mut skipped) = self.portable_docs(&home)?;
        let sessions = self.registry.lock().map_err(poisoned)?.records();
        let result = diri_portable::migrate::export(
            ExportInput {
                home: &home,
                device: &device_name(),
                diri_version: env!("CARGO_PKG_VERSION"),
                profile: &profile,
                docs: docs_snapshot,
                sessions,
                projects: self.local_projects()?,
                skip_transcripts: params.skip_transcripts,
                kdf: if self.portable.home.is_some() {
                    KdfParams::TEST
                } else {
                    KdfParams::default()
                },
            },
            &out,
            params.passphrase.as_deref(),
        )
        .map_err(io_control_error)?;
        let mut result = result;
        skipped.append(&mut result.skipped);
        result.skipped = skipped;
        encode(&result)
    }

    pub(super) fn migrate_import(
        &self,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        let params: MigrateImportParams = decode(params)?;
        let _work = self.portable.work.lock().map_err(poisoned)?;
        let home = self.portable_home()?;
        let home_text = home.to_string_lossy().into_owned();
        let bundle = PathBuf::from(expand(&home_text, params.path.trim()));
        let profile = self.local_profile(&home);
        let (existing, live) = {
            let registry = self.registry.lock().map_err(poisoned)?;
            let records = registry.records();
            let live = records
                .iter()
                .filter(|record| registry.is_live(&record.id.0))
                .map(|record| record.id.0.clone())
                .collect::<HashSet<_>>();
            (records, live)
        };
        let project_id = |root: &str| crate::registry::session_project_id(root, None).0;
        let output = diri_portable::migrate::import(
            &bundle,
            params.passphrase.as_deref(),
            &params.maps,
            params.dry_run,
            ImportTarget {
                home: &home,
                profile: &profile,
                existing,
                live,
                local_projects: self.local_projects()?,
                schedules: self.portable_schedules()?,
                prefs: self.read_prefs(&home),
                project_id: &project_id,
                scan_roots: Vec::new(),
            },
        )
        .map_err(bundle_error)?;
        if params.dry_run {
            return encode(&output.plan);
        }
        {
            let mut registry = self.registry.lock().map_err(poisoned)?;
            let mut known: HashSet<String> = HashSet::new();
            for root in &output.projects {
                let project = registry.ensure_session_project(root, None);
                if let Some(id) = project.get("id").and_then(Value::as_str) {
                    known.insert(id.to_owned());
                }
            }
            for project in registry.projects_raw() {
                if let Some(id) = project.get("id").and_then(Value::as_str) {
                    known.insert(id.to_owned());
                }
            }
            for mut record in output.sessions.iter().cloned() {
                if registry.is_live(&record.id.0) {
                    continue;
                }
                if !known.contains(&record.project_id.0) {
                    // Its project folder is not here yet: file it under its
                    // own folder, as loading would.
                    let project = registry.ensure_session_project(&record.cwd, None);
                    if let Some(id) = project.get("id").and_then(Value::as_str) {
                        record.project_id = diri_proto::ProjectId(id.to_owned());
                        known.insert(id.to_owned());
                    }
                }
                registry.insert_record(record);
            }
            registry.persist().map_err(io_control_error)?;
            for record in &output.sessions {
                self.publish_updated(&registry, &record.id.0);
            }
        }
        for schedule in &output.schedules {
            self.put_portable_schedule(&schedule.id, schedule.spec.clone())?;
        }
        if let Some(pending) = output.pending_prefs.clone() {
            self.write_pending_prefs(&home, pending)?;
        }
        encode(&output.plan)
    }

    /// Starts automatic sync if it is configured. Called once at startup.
    pub(super) fn spawn_profile_sync(self: &std::sync::Arc<Self>) {
        let _ = self.portable.server.set(std::sync::Arc::downgrade(self));
        self.refresh_auto_sync();
    }

    /// Starts or stops the change watcher to match the settings.
    fn refresh_auto_sync(&self) {
        let mut slot = self.portable.watcher.lock().expect("sync watcher");
        // Dropping the watcher closes its channel; the debounce thread ends.
        *slot = None;
        if self.sync_config().is_none_or(|c| !c.auto) {
            return;
        }
        let Some(server) = self.portable.server.get().cloned() else {
            return;
        };
        let Ok(home) = self.portable_home() else {
            return;
        };
        let profile = self.local_profile(&home);
        let prefs = self.prefs_path(&home);
        let (tx, rx) = mpsc::channel::<()>();
        let filter = {
            let profile = profile.clone();
            let prefs = prefs.clone();
            move |path: &Path| {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if name.starts_with('.') {
                    return false;
                }
                (path.parent() == Some(profile.notes_dir.as_path()) && name.ends_with(".md"))
                    || (path.starts_with(&profile.app_skills_dir) && name.ends_with(".json"))
                    || path.starts_with(&profile.overrides_dir)
                    || Some(path) == profile.init_lua.as_deref()
                    || path == prefs
            }
        };
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event
                && !matches!(event.kind, notify::EventKind::Access(_))
                && event.paths.iter().any(|path| filter(path))
            {
                let _ = tx.send(());
            }
        });
        let Ok(mut watcher) = watcher else {
            return;
        };
        let watch = |watcher: &mut notify::RecommendedWatcher, path: &Path, mode| {
            if path.is_dir() {
                let _ = watcher.watch(path, mode);
            }
        };
        watch(
            &mut watcher,
            &profile.notes_dir,
            RecursiveMode::NonRecursive,
        );
        watch(
            &mut watcher,
            &profile.app_skills_dir,
            RecursiveMode::Recursive,
        );
        watch(
            &mut watcher,
            &profile.overrides_dir,
            RecursiveMode::NonRecursive,
        );
        if let Some(dir) = profile.init_lua.as_deref().and_then(Path::parent) {
            watch(&mut watcher, dir, RecursiveMode::NonRecursive);
        }
        if let Some(dir) = prefs.parent() {
            watch(&mut watcher, dir, RecursiveMode::NonRecursive);
        }
        let spawned = std::thread::Builder::new()
            .name("diri-profile-sync".into())
            .spawn(move || {
                // Blocks until a change; then waits for a quiet period.
                while rx.recv().is_ok() {
                    let first = Instant::now();
                    loop {
                        match rx.recv_timeout(AUTO_SYNC_DEBOUNCE) {
                            Ok(()) => continue,
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    let Some(server) = server.upgrade() else {
                        return;
                    };
                    // Changes that began during or just after a sync are
                    // that sync's own writes.
                    let own = server
                        .portable
                        .quiet_until
                        .lock()
                        .ok()
                        .and_then(|until| *until)
                        .is_some_and(|until| first < until)
                        || server
                            .portable
                            .status
                            .lock()
                            .ok()
                            .is_some_and(|s| s.as_ref().is_some_and(|s| s.running));
                    if !own {
                        server.run_profile_sync();
                    }
                }
            });
        if spawned.is_ok() {
            *slot = Some(watcher);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use diri_proto::Method;
    use serde_json::json;

    use super::*;
    use crate::detect::ManifestEngine;
    use crate::registry::Registry;

    fn engine() -> Arc<ManifestEngine> {
        let dir = crate::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests");
        Arc::new(ManifestEngine::load_dir(&dir).expect("load").0)
    }

    /// One fixture Mac: a home, an app-support root, a state dir.
    struct Mac {
        home: PathBuf,
        support: PathBuf,
        server: Arc<ControlServer>,
    }

    fn mac(root: &Path, user: &str, records: Vec<diri_proto::SessionRecord>) -> Mac {
        let home = root.join("Users").join(user);
        let support = home.join("Library/Application Support/Dirijor");
        std::fs::create_dir_all(&support).unwrap();
        let mut registry = Registry::new(engine(), support.join("state.json"));
        for record in records {
            let root = record.cwd.clone();
            registry.ensure_session_project(&root, None);
            registry.insert_record(record);
        }
        let server =
            ControlServer::new(Arc::new(Mutex::new(registry)), support.join("daemon.sock"))
                .with_config_dir(&support)
                .with_state_dir(&support)
                .with_notes_dir(support.join("notes"))
                .with_portable_home(&home);
        Mac {
            home,
            support,
            server: Arc::new(server),
        }
    }

    fn call(server: &ControlServer, method: &str, params: Value) -> Value {
        server
            .dispatch(method, Some(params))
            .unwrap_or_else(|error| panic!("{method}: {} {}", error.code, error.message))
    }

    #[test]
    fn migrate_through_the_engine_resumes_on_the_new_mac_and_imports_once() {
        let temp = tempfile::tempdir().unwrap();
        let alice_repo = temp.path().join("Users/alice/work/repo");
        std::fs::create_dir_all(alice_repo.join(".git")).unwrap();
        std::fs::write(
            alice_repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:acme/repo.git\n",
        )
        .unwrap();
        let cwd = alice_repo.to_string_lossy().into_owned();
        let conversation = "7f3c2a10-1111-4abc-8def-0123456789ab";
        let mut record = crate::control::new_record("s_claude", "claude-code", &cwd);
        record.agent_session_id = Some(conversation.into());
        record.title = "Fix the parser".into();
        let a = mac(temp.path(), "alice", vec![record]);
        let slug = diri_portable::pathmap::claude_project_slug(&cwd);
        assert_eq!(slug, crate::inject::claude_project_slug(&cwd));
        let transcript = a.home.join(".claude/projects").join(&slug);
        std::fs::create_dir_all(&transcript).unwrap();
        std::fs::write(
            transcript.join(format!("{conversation}.jsonl")),
            format!(
                "{}\n",
                json!({"type": "user", "cwd": cwd, "sessionId": conversation})
            ),
        )
        .unwrap();
        std::fs::create_dir_all(a.support.join("notes")).unwrap();
        std::fs::write(a.support.join("notes/plan.md"), "# Plan\n").unwrap();
        std::fs::write(a.support.join("accounts.json"), r#"{"token":"PLANTED"}"#).unwrap();
        std::fs::write(
            a.support.join("prefs.json"),
            r#"{"terminalTheme":"Dusk","windowPlacement":{"x":1}}"#,
        )
        .unwrap();

        let bundle = temp.path().join("move.diribundle");
        let exported = call(
            &a.server,
            Method::MIGRATE_EXPORT,
            json!({"path": bundle, "passphrase": "pw"}),
        );
        assert_eq!(exported["counts"]["sessions"], 1);
        assert_eq!(exported["counts"]["transcripts"], 1);
        assert_eq!(exported["encrypted"], true);

        let b = mac(temp.path(), "bob", Vec::new());
        let bob_repo = b.home.join("code/repo");
        std::fs::create_dir_all(bob_repo.join(".git")).unwrap();
        std::fs::write(
            bob_repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/repo\n",
        )
        .unwrap();
        let missing = b
            .server
            .dispatch(
                Method::MIGRATE_IMPORT,
                Some(json!({"path": bundle, "dryRun": true})),
            )
            .unwrap_err();
        assert_eq!(missing.code, "bundle_passphrase_required");
        let plan = call(
            &b.server,
            Method::MIGRATE_IMPORT,
            json!({"path": bundle, "passphrase": "pw", "dryRun": true}),
        );
        assert_eq!(plan["projects"][0]["match"], "origin");
        assert!(
            b.server.registry.lock().unwrap().records().is_empty(),
            "dry run imported"
        );

        let plan = call(
            &b.server,
            Method::MIGRATE_IMPORT,
            json!({"path": bundle, "passphrase": "pw"}),
        );
        assert_eq!(plan["added"]["sessions"], 1);
        let record = b
            .server
            .registry
            .lock()
            .unwrap()
            .record("s_claude")
            .unwrap();
        let bob_cwd = bob_repo.to_string_lossy().into_owned();
        assert_eq!(record.cwd, bob_cwd);
        assert_eq!(record.resumability, diri_proto::Resumability::Resumable);
        assert_eq!(
            record.project_id,
            crate::registry::session_project_id(&bob_cwd, None)
        );
        assert!(
            b.home
                .join(".claude/projects")
                .join(crate::inject::claude_project_slug(&bob_cwd))
                .join(format!("{conversation}.jsonl"))
                .is_file()
        );
        assert!(b.support.join("notes/plan.md").is_file());
        assert!(!b.support.join("accounts.json").exists());
        let pending: Value = serde_json::from_slice(
            &std::fs::read(b.support.join(PENDING_PREFS_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(pending["terminalTheme"], "Dusk");
        assert!(pending.get("windowPlacement").is_none());

        let again = call(
            &b.server,
            Method::MIGRATE_IMPORT,
            json!({"path": bundle, "passphrase": "pw"}),
        );
        assert_eq!(again["added"]["sessions"], 0);
        assert!(again["kept"].as_u64().unwrap() >= 1);
        assert_eq!(b.server.registry.lock().unwrap().records().len(), 1);
    }

    #[test]
    fn folder_sync_through_the_engine_and_off_means_off() {
        let temp = tempfile::tempdir().unwrap();
        let a = mac(temp.path(), "alice", Vec::new());
        let b = mac(temp.path(), "bob", Vec::new());
        let folder = temp.path().join("iCloud/Diri");
        std::fs::create_dir_all(a.support.join("notes")).unwrap();
        std::fs::write(a.support.join("notes/plan.md"), "# Plan\n").unwrap();
        std::fs::write(a.support.join("prefs.json"), r#"{"terminalTheme":"Dusk"}"#).unwrap();
        let status = call(&a.server, Method::SYNC_STATUS, json!({}));
        assert!(status.get("config").is_none());
        assert!(
            a.server
                .dispatch(Method::SYNC_NOW, Some(json!({})))
                .is_err()
        );
        for mac in [&a, &b] {
            call(
                &mac.server,
                Method::SYNC_CONFIGURE,
                json!({"config": {"backend": "folder", "location": folder}}),
            );
        }
        let status = call(&a.server, Method::SYNC_NOW, json!({}));
        assert!(status["lastError"].is_null(), "{status}");
        assert!(status["lastSummary"]["pushed"].as_u64().unwrap() >= 2);
        let status = call(&b.server, Method::SYNC_NOW, json!({}));
        assert!(status["lastError"].is_null(), "{status}");
        assert_eq!(status["lastSummary"]["prefsPending"], true);
        assert_eq!(
            std::fs::read_to_string(b.support.join("notes/plan.md")).unwrap(),
            "# Plan\n"
        );
        let pending: Value = serde_json::from_slice(
            &std::fs::read(b.support.join(PENDING_PREFS_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(pending["terminalTheme"], "Dusk");
        // Nothing but profile files reached the folder.
        for entry in std::fs::read_dir(&folder).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                [
                    "notes",
                    "prefs.json",
                    "recipes.json",
                    "schedules.json",
                    "projects.json",
                    "diri-profile.json"
                ]
                .contains(&name.as_str()),
                "{name}"
            );
        }
        // Turning sync off forgets the place and starts no watcher.
        call(&a.server, Method::SYNC_CONFIGURE, json!({"config": null}));
        assert!(a.server.portable.watcher.lock().unwrap().is_none());
        assert!(
            call(&a.server, Method::SYNC_STATUS, json!({}))
                .get("config")
                .is_none()
        );
    }
}
