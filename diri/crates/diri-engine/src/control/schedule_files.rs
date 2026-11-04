//! File-triggered schedules: "when a new file appears in this folder, give
//! it to an agent". A trigger is a schedule whose `when` is
//! [`ScheduleWhen::Files`], so it shares the journal, the runs history, the
//! continued session and every client of ordinary schedules.
//!
//! One thread owns one `notify` watcher (FSEvents on macOS, inotify on
//! Linux) for every armed folder and parks on its channel: with no files
//! arriving it does no work at all, and with no enabled trigger the watcher
//! is dropped. An event only marks its trigger dirty; the folder is then
//! rescanned and diffed against the handled set in the journal (see
//! [`crate::file_trigger`]), so coalesced or dropped events cost nothing in
//! correctness. Files are claimed in a transaction (handled set + run entry)
//! before the session is started, exactly like a due cron occurrence.
use super::*;
use crate::file_trigger::{self, Glob, Ready, ScanError, Settle};
use notify::{RecursiveMode, Watcher};
use std::collections::HashSet;
use std::sync::mpsc;

/// A dirty folder is rescanned at most this often while events stream in.
const MIN_SCAN_GAP_MS: i64 = 250;

pub(super) enum FileMessage {
    /// Schedules changed or a run finished: re-read triggers and rescan any
    /// folder that still has files waiting.
    Reload,
    Event(notify::Result<notify::Event>),
}

pub(super) fn ensure_tables(db: &Connection) -> Result<(), ControlError> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS schedule_files_v1 (
            id TEXT PRIMARY KEY, handled TEXT NOT NULL
        );",
    )
    .map_err(storage_error)
}

pub(super) fn load_handled(db: &Connection, id: &str) -> Result<HashSet<String>, ControlError> {
    let raw: Option<String> = db
        .query_row(
            "SELECT handled FROM schedule_files_v1 WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
        .into_iter()
        .collect())
}

pub(super) fn save_handled(
    db: &Connection,
    id: &str,
    handled: &HashSet<String>,
) -> Result<(), ControlError> {
    let mut sorted: Vec<&String> = handled.iter().collect();
    sorted.sort_unstable();
    let raw = serde_json::to_string(&sorted).unwrap();
    db.execute(
        "INSERT INTO schedule_files_v1 (id, handled) VALUES (?1, ?2)
         ON CONFLICT(id) DO UPDATE SET handled=excluded.handled",
        params![id, raw],
    )
    .map_err(storage_error)?;
    Ok(())
}

pub(super) fn forget_handled(db: &Connection, id: &str) -> Result<(), ControlError> {
    db.execute("DELETE FROM schedule_files_v1 WHERE id=?1", [id])
        .map_err(storage_error)?;
    Ok(())
}

/// Validates a file trigger in place: the folder is rewritten to its
/// canonical path so watcher events (which report real paths) match it.
pub(super) fn validate_trigger(spec: &mut ScheduleSpec) -> Result<(), ControlError> {
    let ScheduleWhen::Files { folder, glob, .. } = &mut spec.when else {
        return Ok(());
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let canonical = file_trigger::validate_folder(folder, home.as_deref())
        .map_err(ControlError::bad_request)?;
    *folder = canonical.to_string_lossy().into_owned();
    Glob::parse(glob.as_deref().unwrap_or("")).map_err(ControlError::bad_request)?;
    if glob.as_deref().is_some_and(|glob| glob.trim().is_empty()) {
        *glob = None;
    }
    if spec.wake_mac {
        return Err(ControlError::bad_request(
            "a file trigger cannot wake the Mac: files only arrive while it is awake",
        ));
    }
    if spec.spawn.host.is_some() {
        return Err(ControlError::bad_request(
            "a file trigger watches a folder on this Mac, so its agent must run here too",
        ));
    }
    Ok(())
}

/// The folder's current files: everything already there is not new.
pub(super) fn baseline(when: &ScheduleWhen) -> Result<Option<HashSet<String>>, ControlError> {
    let ScheduleWhen::Files {
        folder,
        glob,
        recursive,
    } = when
    else {
        return Ok(None);
    };
    let glob = Glob::parse(glob.as_deref().unwrap_or("")).map_err(ControlError::bad_request)?;
    match file_trigger::scan(Path::new(folder), *recursive, glob.as_ref()) {
        Ok(entries) => Ok(Some(entries.into_iter().map(|entry| entry.rel).collect())),
        Err(ScanError::TooMany) => Err(too_many()),
        Err(ScanError::Unreadable(error)) => Err(ControlError::bad_request(format!(
            "cannot read the folder {folder}: {error}"
        ))),
    }
}

fn too_many() -> ControlError {
    ControlError::bad_request(format!(
        "the folder holds more than {} files; pick a smaller folder or watch only its top level",
        file_trigger::MAX_TRACKED_FILES
    ))
}

/// One enabled trigger, as the watcher thread sees it.
struct Armed {
    when: ScheduleWhen,
    folder: PathBuf,
    recursive: bool,
    glob: Option<Glob>,
    settle: Settle,
    dirty: bool,
    last_scan_ms: i64,
    /// The folder could not be read last time; logged once.
    unreadable: bool,
}

impl Armed {
    fn new(when: &ScheduleWhen) -> Option<Self> {
        let ScheduleWhen::Files {
            folder,
            glob,
            recursive,
        } = when
        else {
            return None;
        };
        Some(Self {
            when: when.clone(),
            folder: PathBuf::from(folder),
            recursive: *recursive,
            glob: Glob::parse(glob.as_deref().unwrap_or("")).ok().flatten(),
            settle: Settle::default(),
            // A freshly armed trigger scans once: that is the start-up
            // catch-up for files that arrived while diri was down.
            dirty: true,
            last_scan_ms: i64::MIN / 2,
            unreadable: false,
        })
    }

    fn due(&self, now: i64, settle_ms: i64) -> bool {
        (self.dirty && now - self.last_scan_ms >= MIN_SCAN_GAP_MS)
            || self
                .settle
                .next_check_after(self.last_scan_ms, settle_ms)
                .is_some_and(|check| check <= now)
    }

    /// The next moment this trigger needs attention without a new event.
    fn deadline(&self, settle_ms: i64) -> Option<i64> {
        let rescan = self.dirty.then_some(self.last_scan_ms + MIN_SCAN_GAP_MS);
        let settle = self.settle.next_check_after(self.last_scan_ms, settle_ms);
        rescan.into_iter().chain(settle).min()
    }
}

impl super::super::ControlServer {
    pub(super) fn spawn_file_triggers(self: &Arc<Self>) {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut slot) = self.scheduler.files.lock() {
            *slot = Some(sender.clone());
        }
        let server = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("diri-file-triggers".into())
            .spawn(move || server.run_file_triggers(receiver, sender));
    }

    fn run_file_triggers(
        self: Arc<Self>,
        receiver: mpsc::Receiver<FileMessage>,
        sender: mpsc::Sender<FileMessage>,
    ) {
        let settle_ms = self.scheduler.settle_ms;
        let mut armed: HashMap<String, Armed> = HashMap::new();
        let mut watcher: Option<notify::RecommendedWatcher> = None;
        let mut watched: HashMap<PathBuf, RecursiveMode> = HashMap::new();
        let mut reload = true;
        loop {
            if std::mem::take(&mut reload) {
                self.reload_file_triggers(&mut armed);
                reconcile_watches(&armed, &mut watcher, &mut watched, &sender);
            }
            let now = now_ms();
            let due: Vec<String> = armed
                .iter()
                .filter(|(_, trigger)| trigger.due(now, settle_ms))
                .map(|(id, _)| id.clone())
                .collect();
            for id in due {
                if let Some(trigger) = armed.get_mut(&id) {
                    self.check_trigger(&id, trigger, now, settle_ms);
                }
            }
            let deadline = armed
                .values()
                .filter_map(|trigger| trigger.deadline(settle_ms))
                .min();
            // Parks indefinitely while every folder is quiet.
            let first = match deadline {
                None => match receiver.recv() {
                    Ok(message) => Some(message),
                    Err(_) => return,
                },
                Some(deadline) => {
                    let wait = Duration::from_millis((deadline - now_ms()).max(1) as u64);
                    match receiver.recv_timeout(wait) {
                        Ok(message) => Some(message),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            };
            for message in first
                .into_iter()
                .chain(std::iter::from_fn(|| receiver.try_recv().ok()))
            {
                match message {
                    FileMessage::Reload => reload = true,
                    FileMessage::Event(Ok(event)) => {
                        if matches!(event.kind, notify::EventKind::Access(_)) {
                            continue;
                        }
                        for trigger in armed.values_mut() {
                            if event.need_rescan()
                                || event.paths.iter().any(|path| {
                                    file_trigger::concerns(&trigger.folder, trigger.recursive, path)
                                })
                            {
                                trigger.dirty = true;
                            }
                        }
                    }
                    // The backend lost track (queue overflow): rescan all.
                    FileMessage::Event(Err(_)) => {
                        for trigger in armed.values_mut() {
                            trigger.dirty = true;
                        }
                    }
                }
            }
        }
    }

    fn reload_file_triggers(&self, armed: &mut HashMap<String, Armed>) {
        let records = match database(&self.schedules_path()).and_then(|db| load_all(&db)) {
            Ok(records) => records,
            Err(error) => {
                eprintln!("diri-file-triggers: load failed: {}", error.message);
                return;
            }
        };
        let mut live = HashSet::new();
        for record in records {
            if !record.spec.enabled || !matches!(record.spec.when, ScheduleWhen::Files { .. }) {
                continue;
            }
            live.insert(record.id.clone());
            match armed.get_mut(&record.id) {
                Some(trigger) if trigger.when == record.spec.when => {
                    // A finished run may leave files waiting behind it.
                    if !trigger.settle.is_empty() {
                        trigger.dirty = true;
                    }
                }
                _ => {
                    if let Some(trigger) = Armed::new(&record.spec.when) {
                        armed.insert(record.id.clone(), trigger);
                    }
                }
            }
        }
        armed.retain(|id, _| live.contains(id));
    }

    fn check_trigger(self: &Arc<Self>, id: &str, trigger: &mut Armed, now: i64, settle_ms: i64) {
        trigger.dirty = false;
        trigger.last_scan_ms = now;
        let entries =
            match file_trigger::scan(&trigger.folder, trigger.recursive, trigger.glob.as_ref()) {
                Ok(entries) => entries,
                Err(error) => {
                    if !trigger.unreadable {
                        trigger.unreadable = true;
                        let message = match error {
                            ScanError::TooMany => too_many().message,
                            ScanError::Unreadable(error) => {
                                format!("cannot read the watched folder: {error}")
                            }
                        };
                        eprintln!("diri-file-triggers: {id}: {message}");
                        self.record_trigger_failure(id, now, message);
                    }
                    trigger.settle = Settle::default();
                    return;
                }
            };
        trigger.unreadable = false;
        let handled = match self.prune_handled(id, &entries) {
            Ok(handled) => handled,
            Err(error) => {
                eprintln!("diri-file-triggers: {id}: {}", error.message);
                return;
            }
        };
        trigger.settle.observe(&entries, &handled, now);
        if self
            .scheduler
            .file_runs
            .lock()
            .is_ok_and(|runs| runs.contains(id))
        {
            // One run at a time per trigger: what arrives meanwhile becomes
            // the next batch, which also continues the same session.
            return;
        }
        if let Some(ready) = trigger.settle.ready(now, settle_ms) {
            self.claim_files(id, trigger, ready, now);
        }
    }

    /// Drops paths that no longer exist from the handled set, so a file
    /// dropped again under an old name counts as new, and returns the set.
    fn prune_handled(
        &self,
        id: &str,
        entries: &[file_trigger::Entry],
    ) -> Result<HashSet<String>, ControlError> {
        let db = database(&self.schedules_path())?;
        let handled = load_handled(&db, id)?;
        let present: HashSet<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();
        if handled.iter().all(|rel| present.contains(rel.as_str())) {
            return Ok(handled);
        }
        let mut db = db;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        // Re-read inside the write lock: an edit may have re-baselined it.
        let mut handled = load_handled(&tx, id)?;
        handled.retain(|rel| present.contains(rel.as_str()));
        save_handled(&tx, id, &handled)?;
        tx.commit().map_err(storage_error)?;
        Ok(handled)
    }

    /// Claims a batch (handled set and run entry in one transaction), then
    /// starts the run with the paths appended to the prompt.
    fn claim_files(self: &Arc<Self>, id: &str, trigger: &mut Armed, ready: Ready, now: i64) {
        let claimed = (|| -> Result<Option<(ScheduleRecord, Vec<String>)>, ControlError> {
            let mut db = database(&self.schedules_path())?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            let mut record = load(&tx, id)?;
            if !record.spec.enabled || record.spec.when != trigger.when {
                return Ok(None);
            }
            let mut handled = load_handled(&tx, id)?;
            let files: Vec<String> = ready
                .files
                .iter()
                .filter(|rel| !handled.contains(*rel))
                .cloned()
                .collect();
            if files.is_empty() {
                return Ok(None);
            }
            handled.extend(files.iter().cloned());
            save_handled(&tx, id, &handled)?;
            let late = ready.arrived_ms < self.scheduler.started_ms;
            push_run(
                &mut record,
                ScheduleRun {
                    due_at: DateMillis(ready.arrived_ms.min(now) as f64),
                    fired_at: DateMillis(now as f64),
                    outcome: if late {
                        ScheduleOutcome::Late
                    } else {
                        ScheduleOutcome::OnTime
                    },
                    late_reason: late.then_some(LateReason::NotRunning),
                    collapsed: 0,
                    session_id: None,
                    error: None,
                    files: files
                        .iter()
                        .map(|rel| trigger.folder.join(rel).to_string_lossy().into_owned())
                        .collect(),
                },
            );
            record.revision += 1;
            save(&tx, &record)?;
            tx.commit().map_err(storage_error)?;
            Ok(Some((record, files)))
        })();
        trigger.settle.claimed(&ready.files);
        match claimed {
            Ok(Some((record, files))) => {
                self.publish_schedule(id);
                if let Ok(mut runs) = self.scheduler.file_runs.lock() {
                    runs.insert(id.to_owned());
                }
                let mut run = record;
                let user_prompt = run.spec.spawn.initial_prompt.clone().unwrap_or_default();
                run.spec.spawn.initial_prompt =
                    Some(file_trigger::prompt(&user_prompt, &trigger.folder, &files));
                self.start_run(run, now);
            }
            Ok(None) => {}
            Err(error) => eprintln!("diri-file-triggers: claim {id} failed: {}", error.message),
        }
    }

    fn record_trigger_failure(&self, id: &str, now: i64, message: String) {
        let written = (|| -> Result<(), ControlError> {
            let mut db = database(&self.schedules_path())?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            let mut record = load(&tx, id)?;
            push_run(
                &mut record,
                ScheduleRun {
                    due_at: DateMillis(now as f64),
                    fired_at: DateMillis(now as f64),
                    outcome: ScheduleOutcome::Failed,
                    late_reason: None,
                    collapsed: 0,
                    session_id: None,
                    error: Some(message),
                    files: Vec::new(),
                },
            );
            record.revision += 1;
            save(&tx, &record)?;
            tx.commit().map_err(storage_error)
        })();
        if written.is_ok() {
            self.publish_schedule(id);
        }
    }
}

/// Watches exactly the armed folders (recursively when any trigger on that
/// folder asks to); drops the watcher when nothing is armed.
fn reconcile_watches(
    armed: &HashMap<String, Armed>,
    watcher: &mut Option<notify::RecommendedWatcher>,
    watched: &mut HashMap<PathBuf, RecursiveMode>,
    sender: &mpsc::Sender<FileMessage>,
) {
    let mut desired: HashMap<PathBuf, RecursiveMode> = HashMap::new();
    for trigger in armed.values() {
        let mode = desired
            .entry(trigger.folder.clone())
            .or_insert(RecursiveMode::NonRecursive);
        if trigger.recursive {
            *mode = RecursiveMode::Recursive;
        }
    }
    if desired.is_empty() {
        watched.clear();
        *watcher = None;
        return;
    }
    if watcher.is_none() {
        let sender = sender.clone();
        match notify::recommended_watcher(move |event| {
            let _ = sender.send(FileMessage::Event(event));
        }) {
            Ok(created) => *watcher = Some(created),
            Err(error) => {
                eprintln!("diri-file-triggers: cannot start the folder watcher: {error}");
                return;
            }
        }
    }
    let Some(active) = watcher.as_mut() else {
        return;
    };
    watched.retain(|path, mode| {
        let keep = desired.get(path) == Some(mode);
        if !keep {
            let _ = active.unwatch(path);
        }
        keep
    });
    for (path, mode) in desired {
        if watched.contains_key(&path) {
            continue;
        }
        match active.watch(&path, mode) {
            Ok(()) => {
                watched.insert(path, mode);
            }
            Err(error) => eprintln!(
                "diri-file-triggers: cannot watch {}: {error}",
                path.display()
            ),
        }
    }
}
