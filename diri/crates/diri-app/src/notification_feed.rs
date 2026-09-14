//! Bounded, private per-installation notification history. Execution status is
//! owned by the Engine; reading an event never changes an agent's work state.
use std::io;
use std::path::Path;

use diri_proto::{SessionId, SessionRecord};
use serde::{Deserialize, Serialize};

use crate::notifications::{NotificationRequest, NotificationSound, StatusTransition};
use diri_proto::attention::{ATTENTION_VERSION, AttentionKind};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const LIMIT: usize = 200;
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationKind {
    NeedsInput,
    Done,
    Failed,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationEntry {
    pub id: String,
    pub session_id: SessionId,
    pub incarnation: u64,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    pub created_at_ms: u64,
    pub read: bool,
    pub resolved: bool,
    /// Read only for migration from the former text-identity history.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blocker: String,
}

/// Sole owner of admission, history, receipts, settling and cancellation.
/// SQLite receipts outlive display pruning and are committed with inbox entries.
pub struct NotificationFeed {
    entries: Vec<NotificationEntry>,
    db: Mutex<Connection>,
    healthy: AtomicBool,
    pending: HashMap<String, Instant>,
    guards: HashMap<String, Arc<AtomicBool>>,
}

impl Default for NotificationFeed {
    fn default() -> Self {
        Self::from_connection(Connection::open_in_memory().expect("notification database"))
            .expect("notification schema")
    }
}

impl NotificationFeed {
    pub fn unavailable() -> Self {
        let feed = Self::default();
        feed.healthy.store(false, Ordering::SeqCst);
        feed
    }

    fn from_connection(db: Connection) -> io::Result<Self> {
        db.busy_timeout(Duration::from_secs(1))
            .map_err(io::Error::other)?;
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(io::Error::other)?;
        if version > 1 {
            return Err(io::Error::other("unsupported notification database"));
        }
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS receipt(id TEXT PRIMARY KEY NOT NULL); CREATE TABLE IF NOT EXISTS interruption(id TEXT PRIMARY KEY NOT NULL); CREATE TABLE IF NOT EXISTS history(id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL); PRAGMA user_version=1;").map_err(io::Error::other)?;
        let body: Option<String> = db
            .query_row("SELECT body FROM history WHERE id=1", [], |row| row.get(0))
            .optional()
            .map_err(io::Error::other)?;
        if body
            .as_ref()
            .is_some_and(|body| body.len() as u64 > MAX_BYTES)
        {
            return Err(io::Error::other("notification history is too large"));
        }
        let entries = body
            .map(|body| serde_json::from_str::<Vec<NotificationEntry>>(&body))
            .transpose()
            .map_err(io::Error::other)?
            .unwrap_or_default();
        if entries.len() > LIMIT {
            return Err(io::Error::other(
                "notification history has too many entries",
            ));
        }
        let guards = entries
            .iter()
            .filter(|entry| !entry.read && !entry.resolved)
            .map(|entry| (entry.id.clone(), Arc::new(AtomicBool::new(true))))
            .collect();
        Ok(Self {
            entries,
            db: Mutex::new(db),
            healthy: AtomicBool::new(true),
            pending: HashMap::new(),
            guards,
        })
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let database = path.with_extension("sqlite");
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Create with private permissions before SQLite can write sensitive text.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&database)?;
        let mut feed =
            Self::from_connection(Connection::open(&database).map_err(io::Error::other)?)?;
        let initialized: bool = feed
            .db
            .lock()
            .expect("notification database")
            .query_row("SELECT EXISTS(SELECT 1 FROM history)", [], |row| row.get(0))
            .map_err(io::Error::other)?;
        if !initialized && path.exists() {
            if std::fs::metadata(path)?.len() > MAX_BYTES {
                return Err(io::Error::other("notification history is too large"));
            }
            #[derive(Deserialize)]
            struct Legacy {
                version: u32,
                entries: Vec<NotificationEntry>,
                #[serde(default)]
                dismissed: Vec<String>,
            }
            let legacy: Legacy = serde_json::from_slice(&std::fs::read(path)?)?;
            if legacy.version != 1 {
                return Err(io::Error::other("unsupported notification history"));
            }
            let mut database = feed.db.lock().expect("notification database");
            let tx = database.transaction().map_err(io::Error::other)?;
            for id in legacy
                .dismissed
                .iter()
                .chain(legacy.entries.iter().map(|entry| &entry.id))
            {
                tx.execute("INSERT OR IGNORE INTO receipt VALUES (?1)", [id])
                    .map_err(io::Error::other)?;
            }
            feed.entries = legacy.entries.into_iter().take(LIMIT).collect();
            tx.execute(
                "INSERT OR REPLACE INTO history VALUES (1, ?1)",
                [serde_json::to_string(&feed.entries)?],
            )
            .map_err(io::Error::other)?;
            tx.commit().map_err(io::Error::other)?;
        }
        Ok(feed)
    }

    fn persist(&self) -> io::Result<()> {
        let result = self
            .db
            .lock()
            .expect("notification database")
            .execute(
                "INSERT OR REPLACE INTO history VALUES (1, ?1)",
                [serde_json::to_string(&self.entries)?],
            )
            .map(|_| ())
            .map_err(io::Error::other);
        if result.is_err() {
            self.healthy.store(false, Ordering::SeqCst);
            self.invalidate(self.guards.keys().cloned().collect::<Vec<_>>().as_slice());
            eprintln!("diri: notification history write unavailable; alerts suppressed");
        }
        result
    }

    fn admit(&mut self, entry: NotificationEntry, interrupt: bool, now: Instant) -> bool {
        if !self.healthy.load(Ordering::SeqCst) {
            return false;
        }
        let mut entries = self.entries.clone();
        entries.insert(0, entry.clone());
        entries.truncate(LIMIT);
        let result = (|| -> io::Result<bool> {
            let mut database = self.db.lock().expect("notification database");
            let tx = database.transaction().map_err(io::Error::other)?;
            if tx
                .execute("INSERT OR IGNORE INTO receipt VALUES (?1)", [&entry.id])
                .map_err(io::Error::other)?
                == 0
            {
                return Ok(false);
            }
            tx.execute(
                "INSERT OR REPLACE INTO history VALUES (1, ?1)",
                [serde_json::to_string(&entries)?],
            )
            .map_err(io::Error::other)?;
            tx.commit().map_err(io::Error::other)?;
            Ok(true)
        })();
        match result {
            Ok(true) => {
                self.entries = entries;
                self.guards.retain(|id, guard| {
                    let keep = self.entries.iter().any(|entry| &entry.id == id);
                    if !keep {
                        guard.store(false, Ordering::SeqCst);
                    }
                    keep
                });
                self.pending
                    .retain(|id, _| self.entries.iter().any(|entry| &entry.id == id));
                if !entry.read && !entry.resolved {
                    self.guards
                        .insert(entry.id.clone(), Arc::new(AtomicBool::new(true)));
                }
                if interrupt {
                    self.arm(&entry.id, true, now);
                }
                true
            }
            Ok(false) => false,
            Err(_) => {
                self.disable();
                eprintln!("diri: notification receipt unavailable; interruption suppressed");
                false
            }
        }
    }

    fn disable(&self) {
        self.healthy.store(false, Ordering::SeqCst);
        for guard in self.guards.values() {
            guard.store(false, Ordering::SeqCst);
        }
    }

    /// Reserve the single interruption attempt independently from inbox admission.
    /// Optional questions enter the inbox first and may become blocking later.
    fn arm(&mut self, id: &str, live: bool, now: Instant) {
        if !self.healthy.load(Ordering::SeqCst) {
            return;
        }
        let result = self
            .db
            .lock()
            .expect("notification database")
            .execute("INSERT OR IGNORE INTO interruption VALUES (?1)", [id]);
        match result {
            Ok(1) if live && self.deliverable(id) => {
                self.pending
                    .insert(id.to_owned(), now + Duration::from_secs(1));
            }
            Ok(_) => {}
            Err(_) => self.disable(),
        }
    }

    pub fn observe(
        &mut self,
        session: &SessionRecord,
        descriptor: Option<&diri_proto::AgentDescriptor>,
        focused: bool,
        replay: bool,
        now: Instant,
    ) -> bool {
        let Some(state) = session
            .attention_state
            .as_ref()
            .filter(|state| state.version == ATTENTION_VERSION)
        else {
            return false;
        };
        if session.is_archived() {
            return false;
        }
        let mut changed = false;
        for event in &state.events {
            let id = state.event_id(event);
            let kind = match event.kind {
                AttentionKind::Request => NotificationKind::NeedsInput,
                AttentionKind::Completion => NotificationKind::Done,
                AttentionKind::Failure => NotificationKind::Failed,
            };
            let agent = crate::notifications::display_name(session.effective_kind(), descriptor);
            let title = match kind {
                NotificationKind::NeedsInput => format!("{agent} needs you"),
                NotificationKind::Done => format!("{agent} finished"),
                _ => format!("{agent} stopped"),
            };
            let body = event.detail.as_ref().map_or_else(
                || session.title.clone(),
                |detail| format!("{} · {}", session.title, detail.summary),
            );
            let entry = NotificationEntry {
                id,
                session_id: session.id.clone(),
                incarnation: session.created_at.0.to_bits(),
                kind,
                title: bounded(&title, 160),
                body: bounded(&body, 1000),
                created_at_ms: event.occurred_at.0.max(0.0) as u64,
                read: focused || event.resolved,
                resolved: event.resolved,
                blocker: String::new(),
            };
            changed |= self.admit(entry, false, now);
            if event.blocking {
                let pending = self.pending.len();
                self.arm(&state.event_id(event), !replay && !focused, now);
                changed |= self.pending.len() != pending;
            } else if self.pending.remove(&state.event_id(event)).is_some() {
                self.invalidate(&[state.event_id(event)]);
            }
        }
        changed
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().copied().min()
    }

    pub fn drain_due(&mut self, now: Instant, sounds: bool) -> Vec<StatusTransition> {
        let due: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let mut effects = Vec::new();
        for id in due {
            self.pending.remove(&id);
            if !self.deliverable(&id) {
                continue;
            }
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .expect("deliverable entry");
            effects.push(StatusTransition {
                dismiss: Vec::new(),
                in_app_banner: None,
                sound: sounds.then_some(if entry.kind == NotificationKind::Done {
                    NotificationSound::Done
                } else {
                    NotificationSound::NeedsInput
                }),
                notification: Some(NotificationRequest {
                    session_event: true,
                    identifier: id.clone(),
                    title: entry.title.clone(),
                    body: entry.body.clone(),
                    thread_identifier: Some(entry.session_id.0.clone()),
                    action_data: None,
                    use_system_sound: false,
                    guard: self
                        .guards
                        .get(&id)
                        .cloned()
                        .map(crate::notifications::DeliveryGuard),
                }),
            });
        }
        effects
    }

    pub fn deliverable(&self, id: &str) -> bool {
        self.healthy.load(Ordering::SeqCst)
            && self
                .entries
                .iter()
                .any(|entry| entry.id == id && !entry.read && !entry.resolved)
            && self
                .guards
                .get(id)
                .is_some_and(|guard| guard.load(Ordering::SeqCst))
    }

    pub fn guard(&self, id: &str) -> Option<crate::notifications::DeliveryGuard> {
        self.guards
            .get(id)
            .cloned()
            .map(crate::notifications::DeliveryGuard)
    }

    pub fn invalidate(&self, ids: &[String]) {
        for id in ids {
            if let Some(guard) = self.guards.get(id) {
                guard.store(false, Ordering::SeqCst);
            }
        }
    }

    pub fn entries(&self) -> &[NotificationEntry] {
        &self.entries
    }
    pub fn unread_count(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.read).count()
    }
    pub fn session_unread(&self, id: &SessionId) -> bool {
        self.entries
            .iter()
            .any(|entry| &entry.session_id == id && !entry.read)
    }

    pub fn reconcile(&mut self, sessions: &[&SessionRecord]) -> Vec<String> {
        let mut removed = Vec::new();
        for entry in &mut self.entries {
            let session = sessions.iter().find(|session| {
                session.id == entry.session_id
                    && session.created_at.0.to_bits() == entry.incarnation
                    && !session.is_archived()
            });
            let resolved = session.is_none()
                || (entry.kind != NotificationKind::Custom
                    && session.is_some_and(|session| {
                        session.attention_state.as_ref().is_some_and(|state| {
                            !state
                                .events
                                .iter()
                                .any(|event| !event.resolved && state.event_id(event) == entry.id)
                        })
                    }));
            if resolved && !entry.resolved {
                entry.resolved = true;
                if session.is_none() || entry.kind == NotificationKind::NeedsInput {
                    entry.read = true;
                }
                removed.push(entry.id.clone());
            }
        }
        self.invalidate(&removed);
        for id in &removed {
            self.pending.remove(id);
        }
        if !removed.is_empty() {
            let _ = self.persist();
        }
        removed
    }

    pub fn mark_session_read(&mut self, id: &SessionId) -> Vec<String> {
        self.mark_where(|entry| &entry.session_id == id)
    }
    pub fn mark_all_read(&mut self) -> Vec<String> {
        self.mark_where(|_| true)
    }
    fn mark_where(&mut self, predicate: impl Fn(&NotificationEntry) -> bool) -> Vec<String> {
        let mut ids = Vec::new();
        for entry in &mut self.entries {
            if !entry.read && predicate(entry) {
                entry.read = true;
                ids.push(entry.id.clone());
            }
        }
        self.invalidate(&ids);
        for id in &ids {
            self.pending.remove(id);
        }
        if !ids.is_empty() {
            let _ = self.persist();
        }
        ids
    }
    pub fn set_read(&mut self, id: &str, read: bool) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id)
            && entry.read != read
        {
            entry.read = read;
            if read {
                self.invalidate(&[id.to_owned()]);
                self.pending.remove(id);
            }
            let _ = self.persist();
            return true;
        }
        false
    }
    pub fn clear(&mut self) -> Vec<String> {
        let ids = self
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        self.invalidate(&ids);
        self.pending.clear();
        self.entries.clear();
        let _ = self.persist();
        ids
    }

    pub fn record_custom(
        &mut self,
        session: &SessionRecord,
        event: &diri_proto::SessionNotificationEvent,
        read: bool,
    ) -> bool {
        if self.entries.iter().any(|entry| {
            entry.kind == NotificationKind::Custom
                && entry.session_id == session.id
                && entry.title == bounded(&event.title, 160)
                && entry.body == bounded(&event.body, 1000)
                && (event.occurred_at.0.max(0.0) as u64).saturating_sub(entry.created_at_ms) < 5000
        }) {
            return false;
        }
        self.admit(
            NotificationEntry {
                id: event.id.clone(),
                session_id: session.id.clone(),
                incarnation: session.created_at.0.to_bits(),
                kind: NotificationKind::Custom,
                title: bounded(&event.title, 160),
                body: bounded(&event.body, 1000),
                created_at_ms: event.occurred_at.0.max(0.0) as u64,
                read,
                resolved: false,
                blocker: String::new(),
            },
            false,
            Instant::now(),
        )
    }
}

use rusqlite::OptionalExtension;
fn bounded(text: &str, max: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use diri_proto::DateMillis;
    use diri_proto::attention::{AttentionEvent, AttentionState};

    fn session() -> SessionRecord {
        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .remove(0);
        session.attention_state = Some(AttentionState {
            version: ATTENTION_VERSION,
            epoch: "test".into(),
            sequence: 1,
            turn: 1,
            working: false,
            last_native_completion: None,
            observed_at: None,
            active_tools: Default::default(),
            events: vec![AttentionEvent {
                sequence: 1,
                turn: 1,
                kind: AttentionKind::Request,
                occurred_at: DateMillis(10.0),
                resolved: false,
                blocking: true,
                detail: None,
            }],
            native_requests: Default::default(),
            native_completions: Default::default(),
        });
        session
    }

    #[test]
    fn persisted_receipts_survive_clear_restart_and_history_pruning() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notifications.json");
        let now = Instant::now();
        let original = session();
        let mut feed = NotificationFeed::load(&path).unwrap();
        assert!(feed.observe(&original, None, false, false, now));
        assert_eq!(feed.drain_due(now + Duration::from_secs(2), true).len(), 1);
        feed.clear();
        feed.persist().unwrap();
        drop(feed);
        let mut feed = NotificationFeed::load(&path).unwrap();
        assert!(!feed.observe(&original, None, false, false, now));
        for index in 2..250 {
            let mut next = original.clone();
            let state = next.attention_state.as_mut().unwrap();
            state.events[0].sequence = index;
            state.sequence = index;
            feed.observe(&next, None, true, false, now);
        }
        assert_eq!(feed.entries().len(), LIMIT);
        assert!(!feed.observe(&original, None, false, false, now));
        assert!(
            feed.drain_due(now + Duration::from_secs(2), true)
                .is_empty()
        );
    }

    #[test]
    fn clearing_revokes_already_queued_native_authorization_and_unread_cannot_rearm() {
        let mut feed = NotificationFeed::default();
        let now = Instant::now();
        feed.observe(&session(), None, false, false, now);
        let effects = feed.drain_due(now + Duration::from_secs(2), true);
        let request = effects[0].notification.as_ref().unwrap();
        let guard = request.guard.as_ref().unwrap().0.clone();
        assert!(guard.load(Ordering::SeqCst));
        feed.mark_all_read();
        feed.set_read(&request.identifier, false);
        assert!(!guard.load(Ordering::SeqCst));
        assert!(!feed.deliverable(&request.identifier));
        feed.clear();
        assert!(!feed.deliverable(&request.identifier));
    }

    #[test]
    fn receipt_failure_cannot_dispatch_an_ephemeral_alert() {
        let mut feed = NotificationFeed::default();
        feed.db
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert!(!feed.observe(&session(), None, false, false, Instant::now()));
        assert!(feed.entries().is_empty());
        assert!(feed.next_deadline().is_none());
    }

    #[test]
    fn resolving_one_of_two_requests_does_not_cancel_the_other() {
        let mut session = session();
        let state = session.attention_state.as_mut().unwrap();
        let mut second = state.events[0].clone();
        second.sequence = 2;
        state.events.push(second);
        let now = Instant::now();
        let mut feed = NotificationFeed::default();
        feed.observe(&session, None, false, false, now);
        session.attention_state.as_mut().unwrap().events[0].resolved = true;
        assert_eq!(feed.reconcile(&[&session]), ["attention-v1-test-1"]);
        let effects = feed.drain_due(now + Duration::from_secs(2), true);
        assert_eq!(effects.len(), 1);
        assert_eq!(
            effects[0].notification.as_ref().unwrap().identifier,
            "attention-v1-test-2"
        );
    }
    #[test]
    fn optional_question_can_become_blocking_once_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notifications.json");
        let mut record = session();
        record.attention_state.as_mut().unwrap().events[0].blocking = false;
        let now = Instant::now();
        let mut feed = NotificationFeed::load(&path).unwrap();
        assert!(feed.observe(&record, None, false, false, now));
        assert!(feed.next_deadline().is_none());
        drop(feed);
        let mut feed = NotificationFeed::load(&path).unwrap();
        feed.observe(&record, None, false, true, now);
        record.attention_state.as_mut().unwrap().events[0].blocking = true;
        assert!(feed.observe(&record, None, false, false, now));
        assert_eq!(feed.entries().len(), 1);
        assert_eq!(feed.drain_due(now + Duration::from_secs(2), true).len(), 1);
        feed.observe(&record, None, false, false, now);
        assert!(
            feed.drain_due(now + Duration::from_secs(3), true)
                .is_empty()
        );
    }

    #[test]
    fn hydration_is_silent_and_a_later_turn_can_interrupt() {
        let mut feed = NotificationFeed::default();
        let now = Instant::now();
        let mut record = session();
        feed.observe(&record, None, false, true, now);
        assert_eq!(feed.unread_count(), 1);
        assert!(feed.next_deadline().is_none());
        feed.observe(&record, None, false, false, now);
        assert!(feed.next_deadline().is_none());
        record.attention_state.as_mut().unwrap().events[0].sequence = 2;
        feed.reconcile(&[&record]);
        feed.observe(&record, None, false, false, now);
        assert_eq!(feed.drain_due(now + Duration::from_secs(2), false).len(), 1);
    }

    #[test]
    fn pruning_and_storage_failure_revoke_already_queued_native_requests() {
        let mut feed = NotificationFeed::default();
        let now = Instant::now();
        let mut record = session();
        feed.observe(&record, None, false, false, now);
        let guard = feed.drain_due(now + Duration::from_secs(2), true)[0]
            .notification
            .as_ref()
            .unwrap()
            .guard
            .clone()
            .unwrap();
        for sequence in 2..=201 {
            record.attention_state.as_mut().unwrap().events[0].sequence = sequence;
            feed.observe(&record, None, false, false, now);
        }
        assert!(!guard.0.load(Ordering::SeqCst));
        let guard = feed.guard("attention-v1-test-201").unwrap();
        feed.db
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        record.attention_state.as_mut().unwrap().events[0].sequence = 202;
        feed.observe(&record, None, false, false, now);
        assert!(!guard.0.load(Ordering::SeqCst));
    }

    #[test]
    fn legacy_clear_receipts_migrate_once_and_history_stays_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notifications.json");
        std::fs::write(
            &path,
            br#"{"version":1,"entries":[],"dismissed":["attention-v1-test-1"]}"#,
        )
        .unwrap();
        let mut feed = NotificationFeed::load(&path).unwrap();
        assert!(!feed.observe(&session(), None, false, false, Instant::now()));
        assert!(feed.entries().is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path.with_extension("sqlite"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::write(&path, b"obsolete file is never reread").unwrap();
        assert!(NotificationFeed::load(&path).is_ok());
    }
}
