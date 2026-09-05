//! Bounded, private per-installation notification history. Execution status is
//! owned by the Engine; reading an event never changes an agent's work state.
use std::io::{self, Write};
use std::path::Path;

use diri_proto::{SessionId, SessionRecord, SessionStatus};
use serde::{Deserialize, Serialize};

use crate::notifications::{NotificationRequest, blocker_key};

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
    pub blocker: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationFeed {
    version: u32,
    entries: Vec<NotificationEntry>,
    #[serde(default)]
    dismissed: Vec<String>,
}

impl Default for NotificationFeed {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
            dismissed: Vec::new(),
        }
    }
}

impl NotificationFeed {
    pub fn load(path: &Path) -> io::Result<Self> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "notification history is too large",
            ));
        }
        let mut feed: Self = serde_json::from_reader(file)?;
        if feed.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported notification history",
            ));
        }
        feed.entries.truncate(LIMIT);
        feed.dismissed.truncate(LIMIT);
        Ok(feed)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer(&mut temp, self)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
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

    pub fn record(
        &mut self,
        session: &SessionRecord,
        request: &NotificationRequest,
        read: bool,
    ) -> bool {
        if self.dismissed.contains(&request.identifier)
            || self
                .entries
                .iter()
                .any(|entry| entry.id == request.identifier)
        {
            return false;
        }
        let kind = match session.status {
            SessionStatus::NeedsInput(_) => NotificationKind::NeedsInput,
            SessionStatus::Exited(_) => NotificationKind::Failed,
            _ => NotificationKind::Done,
        };
        if kind == NotificationKind::NeedsInput
            && self.entries.iter().any(|entry| {
                entry.session_id == session.id
                    && entry.incarnation == session.created_at.0.to_bits()
                    && entry.kind == kind
                    && !entry.resolved
                    && entry.blocker == blocker_key(session)
            })
        {
            return false;
        }
        let created_at_ms = match kind {
            NotificationKind::NeedsInput => session
                .needs_input
                .as_ref()
                .map(|detail| detail.occurred_at.0),
            NotificationKind::Done => session.last_turn_completed_at.map(|date| date.0),
            _ => None,
        }
        .unwrap_or(session.updated_at.0)
        .max(0.0) as u64;
        self.entries.insert(
            0,
            NotificationEntry {
                id: request.identifier.clone(),
                session_id: session.id.clone(),
                incarnation: session.created_at.0.to_bits(),
                kind,
                title: bounded(&request.title, 160),
                body: bounded(&request.body, 1000),
                created_at_ms,
                read,
                resolved: false,
                blocker: blocker_key(session),
            },
        );
        self.entries.truncate(LIMIT);
        true
    }

    pub fn record_custom(
        &mut self,
        session: &SessionRecord,
        event: &diri_proto::SessionNotificationEvent,
        read: bool,
    ) -> bool {
        if self.dismissed.contains(&event.id)
            || self.entries.iter().any(|entry| entry.id == event.id)
        {
            return false;
        }
        // Repeated terminal redraws must not flood the tray or play a chorus.
        if self.entries.iter().any(|entry| {
            entry.kind == NotificationKind::Custom
                && entry.session_id == session.id
                && entry.incarnation == session.created_at.0.to_bits()
                && entry.title == bounded(&event.title, 160)
                && entry.body == bounded(&event.body, 1000)
                && (event.occurred_at.0.max(0.0) as u64).saturating_sub(entry.created_at_ms) < 5000
        }) {
            return false;
        }
        self.entries.insert(
            0,
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
        );
        self.entries.truncate(LIMIT);
        true
    }

    /// Resolving a blocker clears its badge; completion history remains unread
    /// while the agent starts more work. Read and execution state are distinct.
    pub fn reconcile(&mut self, sessions: &[&SessionRecord]) -> Vec<String> {
        let mut removed = Vec::new();
        for entry in &mut self.entries {
            let session = sessions.iter().find(|session| {
                session.id == entry.session_id
                    && session.created_at.0.to_bits() == entry.incarnation
                    && !session.is_archived()
            });
            let resolved = session.is_none()
                || (entry.kind == NotificationKind::NeedsInput
                    && session.is_some_and(|session| {
                        !matches!(session.status, SessionStatus::NeedsInput(_))
                            || blocker_key(session) != entry.blocker
                    }));
            if resolved && !entry.resolved {
                entry.resolved = true;
                entry.read = true;
                removed.push(entry.id.clone());
            }
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
        ids
    }
    pub fn set_read(&mut self, id: &str, read: bool) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id)
            && entry.read != read
        {
            entry.read = read;
            return true;
        }
        false
    }
    pub fn clear(&mut self) -> Vec<String> {
        let ids: Vec<_> = self.entries.drain(..).map(|entry| entry.id).collect();
        self.dismissed.splice(0..0, ids.clone());
        self.dismissed.truncate(LIMIT);
        ids
    }
}

fn bounded(text: &str, max: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::attention_request;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use diri_proto::{DateMillis, NeedsInputDetail, NeedsInputKind, NeedsInputSource, RiskHint};

    fn blocked() -> SessionRecord {
        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .remove(0);
        session.status = SessionStatus::NeedsInput(NeedsInputKind::Permission);
        session.needs_input = Some(NeedsInputDetail {
            kind: NeedsInputKind::Permission,
            source: NeedsInputSource::ScreenScrape,
            tool_name: None,
            summary: "Run tests?".into(),
            prompt_excerpt: None,
            options: None,
            risk_hint: RiskHint::Neutral,
            occurred_at: DateMillis(1000.0),
        });
        session
    }

    #[test]
    fn resolution_clears_only_the_old_prompt_and_keeps_history() {
        let mut session = blocked();
        let mut feed = NotificationFeed::default();
        let first = attention_request(&session, false, None);
        assert!(feed.record(&session, &first, false));
        assert!(!feed.record(&session, &first, false));
        session.needs_input.as_mut().unwrap().summary = "Deploy?".into();
        assert_eq!(feed.reconcile(&[&session]), vec![first.identifier]);
        feed.record(&session, &attention_request(&session, false, None), false);
        assert_eq!(feed.unread_count(), 1);
        assert_eq!(feed.entries().len(), 2);
        assert!(feed.entries()[1].resolved);
    }

    #[test]
    fn reading_completion_never_mutates_execution_and_survives_restart() {
        let mut session = blocked();
        session.status = SessionStatus::Idle;
        session.last_turn_completed_at = Some(DateMillis(2000.0));
        session.last_seen_at = None;
        let mut feed = NotificationFeed::default();
        feed.record(&session, &attention_request(&session, false, None), false);
        session.status = SessionStatus::Working;
        assert!(feed.reconcile(&[&session]).is_empty());
        assert_eq!(feed.unread_count(), 1);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notifications.json");
        feed.save(&path).unwrap();
        let mut restored = NotificationFeed::load(&path).unwrap();
        assert_eq!(restored.unread_count(), 1);
        restored.mark_session_read(&session.id);
        assert_eq!(restored.unread_count(), 0);
        assert_eq!(session.status, SessionStatus::Working);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
