//! Store side of first-launch conversation import.
//!
//! The Engine does the reading (a bounded, read-only scan of `~/.claude` and
//! `~/.codex`) and the writing (resumable records that start nothing). This
//! keeps what the person sees and has ticked: the five newest suggestions
//! arrive pre-checked, "Browse more" lists the rest with a filter, and one
//! press imports the ticked set.
//!
//! The page shows itself once, on a first launch with no sessions. Importing,
//! "Start fresh", or starting any session retires it for good; File ›
//! Import Conversations… brings it back on demand.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use diri_client::DaemonClient;
use diri_proto::{HistoryEntry, HistoryImportCandidatesResult, ImportCandidate};
use tokio::sync::broadcast;

use super::{SessionStore, StoreEffect};
use crate::notifications::StatusTransition;

/// Rows a filtered browse list renders; the filter narrows the rest.
pub const BROWSE_ROWS: usize = 60;

#[derive(Debug, Default)]
pub struct ConversationImportState {
    /// `None` until the first scan answers.
    pub scan: Option<HistoryImportCandidatesResult>,
    pub scanning: bool,
    pub importing: bool,
    /// Conversation ids ticked for import.
    pub selected: BTreeSet<String>,
    /// The expanded list with a filter, instead of the five suggestions.
    pub browsing: bool,
    pub query: crate::query_editor::QueryEditor,
    /// The row whose last message shows underneath it.
    pub focused: Option<String>,
    /// Shown on request (File › Import Conversations…), sessions or not.
    pub open: bool,
}

impl ConversationImportState {
    pub fn suggested(&self) -> &[ImportCandidate] {
        self.scan.as_ref().map_or(&[], |scan| &scan.suggested)
    }

    pub fn more(&self) -> &[ImportCandidate] {
        self.scan.as_ref().map_or(&[], |scan| &scan.more)
    }

    /// Every candidate, newest first.
    pub fn all(&self) -> impl Iterator<Item = &ImportCandidate> {
        self.suggested().iter().chain(self.more())
    }

    pub fn has_candidates(&self) -> bool {
        !self.suggested().is_empty()
    }

    /// The rows on screen: the suggestions, or the filtered full list. A
    /// filter matches every word against title, folder and last message.
    pub fn visible_rows(&self) -> Vec<&ImportCandidate> {
        if !self.browsing {
            return self.suggested().iter().collect();
        }
        let words: Vec<String> = self
            .query
            .text()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.all()
            .filter(|candidate| {
                if words.is_empty() {
                    return true;
                }
                let haystack = format!(
                    "{} {} {}",
                    candidate.entry.title.as_deref().unwrap_or_default(),
                    folder_name(&candidate.entry.cwd),
                    candidate.preview.as_deref().unwrap_or_default(),
                )
                .to_lowercase();
                words.iter().all(|word| haystack.contains(word.as_str()))
            })
            .take(BROWSE_ROWS)
            .collect()
    }

    /// Ticked entries in newest-first order, the order they are imported.
    pub fn selected_entries(&self) -> Vec<HistoryEntry> {
        self.all()
            .filter(|candidate| self.selected.contains(&candidate.entry.id))
            .map(|candidate| candidate.entry.clone())
            .collect()
    }

    pub fn selected_count(&self) -> usize {
        self.all()
            .filter(|candidate| self.selected.contains(&candidate.entry.id))
            .count()
    }
}

/// The folder's own name: `anara`, never `/Users/me/code/anara`.
pub fn folder_name(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map_or_else(|| cwd.to_owned(), |name| name.to_string_lossy().into_owned())
}

impl SessionStore {
    pub fn conversation_import(&self) -> &ConversationImportState {
        &self.conversation_import
    }

    #[cfg(test)]
    pub(crate) fn set_conversation_import_scan(&mut self, scan: HistoryImportCandidatesResult) {
        self.conversation_import.scanning = false;
        self.land_conversation_scan(scan);
    }

    /// Whether the main pane shows the import page instead of the resting
    /// workbench: on request, or on a first launch that found something.
    pub fn conversation_import_visible(&self) -> bool {
        let state = &self.conversation_import;
        state.open
            || (self.first_launch_import_pending() && (state.has_candidates() || state.importing))
    }

    /// A first launch that has not yet answered the import offer.
    pub fn first_launch_import_pending(&self) -> bool {
        self.sessions.is_empty() && !self.prefs.conversation_import_done
    }

    /// Look for importable conversations after the session list arrives.
    /// A first launch scans; a machine that already has sessions retires
    /// the offer for good instead, so deleting every session later never
    /// brings the onboarding back uninvited.
    pub fn request_first_launch_import(&mut self) {
        if self.prefs.conversation_import_done {
            return;
        }
        if !self.sessions.is_empty() {
            let _ = self.update_preferences(|prefs| prefs.conversation_import_done = true);
            return;
        }
        self.request_conversation_scan();
    }

    pub fn request_conversation_scan(&mut self) {
        if self.conversation_import.scanning || self.conversation_import.importing {
            return;
        }
        self.conversation_import.scanning = true;
        self.emit(StoreEffect::ScanConversations);
    }

    /// File › Import Conversations…: show the page and look again.
    pub fn open_conversation_import(&mut self) {
        self.conversation_import.open = true;
        self.conversation_import.browsing = false;
        self.conversation_import.query = Default::default();
        self.request_conversation_scan();
    }

    pub(super) fn land_conversation_scan(&mut self, scan: HistoryImportCandidatesResult) {
        let state = &mut self.conversation_import;
        // Ticks survive a rescan for conversations still offered; the first
        // answer pre-checks the suggestions.
        let first = state.scan.is_none() || state.selected.is_empty();
        let offered: BTreeSet<String> = scan
            .suggested
            .iter()
            .chain(&scan.more)
            .map(|candidate| candidate.entry.id.clone())
            .collect();
        if first {
            state.selected = scan
                .suggested
                .iter()
                .map(|candidate| candidate.entry.id.clone())
                .collect();
        } else {
            state.selected.retain(|id| offered.contains(id));
        }
        if state.focused.as_ref().is_none_or(|id| !offered.contains(id)) {
            state.focused = scan
                .suggested
                .first()
                .map(|candidate| candidate.entry.id.clone());
        }
        state.scan = Some(scan);
    }

    pub fn toggle_conversation(&mut self, id: &str) {
        let state = &mut self.conversation_import;
        if !state.selected.remove(id) {
            state.selected.insert(id.to_owned());
        }
        state.focused = Some(id.to_owned());
    }

    pub fn focus_conversation(&mut self, id: &str) {
        self.conversation_import.focused = Some(id.to_owned());
    }

    /// ↑/↓ through the rows on screen.
    pub fn move_conversation_focus(&mut self, down: bool) {
        let rows: Vec<String> = self
            .conversation_import
            .visible_rows()
            .iter()
            .map(|candidate| candidate.entry.id.clone())
            .collect();
        if rows.is_empty() {
            return;
        }
        let current = self
            .conversation_import
            .focused
            .as_ref()
            .and_then(|id| rows.iter().position(|row| row == id));
        let next = match (current, down) {
            (None, _) => 0,
            (Some(index), true) => (index + 1).min(rows.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
        };
        self.conversation_import.focused = Some(rows[next].clone());
    }

    /// Space: tick or untick the focused row.
    pub fn toggle_focused_conversation(&mut self) {
        if let Some(id) = self.conversation_import.focused.clone() {
            self.toggle_conversation(&id);
        }
    }

    pub fn set_conversation_browsing(&mut self, browsing: bool) {
        let state = &mut self.conversation_import;
        state.browsing = browsing;
        if !browsing {
            state.query = Default::default();
        }
    }

    pub fn conversation_query_mut(&mut self) -> &mut crate::query_editor::QueryEditor {
        &mut self.conversation_import.query
    }

    /// "Start fresh": retire the page without importing anything.
    pub fn dismiss_conversation_import(&mut self) {
        self.conversation_import.open = false;
        self.conversation_import.browsing = false;
        let _ = self.update_preferences(|prefs| prefs.conversation_import_done = true);
    }

    /// Import every ticked conversation. Returns false when nothing is
    /// ticked or an import is already running.
    pub fn import_selected_conversations(&mut self) -> bool {
        if self.conversation_import.importing {
            return false;
        }
        let entries = self.conversation_import.selected_entries();
        if entries.is_empty() {
            return false;
        }
        self.conversation_import.importing = true;
        self.emit(StoreEffect::ImportConversations(entries));
        true
    }

    /// The Engine answered an import: fold the records in, retire the page.
    pub(super) fn finish_conversation_import(
        &mut self,
        result: Result<diri_proto::SessionImportHistoryResult, String>,
    ) -> Option<StatusTransition> {
        let state = &mut self.conversation_import;
        state.importing = false;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                return Some(crate::notifications::conversation_import_transition(
                    0,
                    Some(&error),
                ));
            }
        };
        let imported = result.sessions.len();
        state.open = false;
        state.browsing = false;
        state.query = Default::default();
        state.selected.clear();
        state.scan = None;
        for record in result.sessions {
            self.upsert_session(record);
        }
        let _ = self.update_preferences(|prefs| prefs.conversation_import_done = true);
        self.invalidate_projection();
        Some(crate::notifications::conversation_import_transition(
            imported, None,
        ))
    }
}

pub(super) async fn scan(
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
) {
    let result = client.import_candidates().await;
    let mut locked = store.write().expect("session store lock poisoned");
    locked.conversation_import.scanning = false;
    // An older Engine without the method answers with an error: nothing to
    // offer, so the page falls back to the plain welcome.
    locked.land_conversation_scan(result.unwrap_or_default());
    drop(locked);
    let _ = change_tx.send(());
}

pub(super) async fn import(
    entries: Vec<HistoryEntry>,
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
    status_tx: broadcast::Sender<StatusTransition>,
) {
    let result = client
        .import_history(entries)
        .await
        .map_err(|error| error.to_string());
    let transition = store
        .write()
        .expect("session store lock poisoned")
        .finish_conversation_import(result);
    let _ = change_tx.send(());
    if let Some(transition) = transition {
        let _ = status_tx.send(transition);
    }
}

/// Seven recent conversations, two Agents, three projects: what a fixture
/// or a screenshot shows. `more` extra rows pad the browse list.
#[cfg(test)]
pub(crate) fn preview_scan(now_ms: f64, more: usize) -> HistoryImportCandidatesResult {
    use diri_proto::{AgentKind, DateMillis};
    let candidate = |n: usize, kind: AgentKind, folder: &str, title: &str, preview: &str, ago_min: f64| {
        ImportCandidate {
            entry: HistoryEntry {
                id: format!("00000000-0000-4000-8000-{n:012}"),
                kind,
                cwd: format!("/Users/you/code/{folder}"),
                title: Some(title.to_owned()),
                transcript_path: format!("/Users/you/.claude/projects/x/{n}.jsonl"),
                last_active_at: DateMillis(now_ms - ago_min * 60_000.0),
                created_at: None,
                cwd_exists: true,
            },
            preview: Some(preview.to_owned()),
        }
    };
    let claude = AgentKind::CLAUDE_CODE;
    let codex = AgentKind::CODEX;
    let suggested = vec![
        candidate(1, claude.clone(), "anara", "Fix the login redirect loop", "Done. The redirect now keeps the query string, and the test covers it.", 12.0),
        candidate(2, codex.clone(), "diri", "Speed up sidebar rendering", "Profiled it: the row cache was rebuilt every frame. Now it is keyed by revision.", 125.0),
        candidate(3, claude.clone(), "diri", "Draft the 0.12 release notes", "Here is a first draft grouped by theme, with the three headline changes first.", 300.0),
        candidate(4, claude.clone(), "website", "Dark mode for the docs site", "Dark tokens are in. Code blocks now follow the system appearance.", 1_560.0),
        candidate(5, codex.clone(), "anara", "Migrate billing to the new schema", "The migration is written and tested against a copy of production.", 3_000.0),
    ];
    let titles = [
        ("diri", "Investigate flaky CI on macOS"),
        ("anara", "Add rate limiting to the export API"),
        ("website", "Rewrite the pricing page copy"),
        ("diri", "Rename the terminal crate"),
        ("anara", "Audit unused feature flags"),
        ("diri", "Cursor blink timing"),
        ("anara", "Flaky upload test on Linux"),
        ("website", "Compress hero images"),
        ("diri", "Split the settings page"),
        ("anara", "Retry webhooks with backoff"),
        ("diri", "Keyboard shortcuts cheat sheet"),
        ("website", "Fix the RSS feed dates"),
        ("anara", "Search index for comments"),
        ("diri", "Sidebar drag and drop polish"),
    ];
    let more = (0..more)
        .map(|index| {
            let (folder, title) = titles[index % titles.len()];
            let kind = if index % 3 == 1 { codex.clone() } else { claude.clone() };
            candidate(10 + index, kind, folder, title, "Wrapped up and pushed.", 4_500.0 + index as f64 * 900.0)
        })
        .collect();
    HistoryImportCandidatesResult {
        suggested,
        more,
        more_truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Prefs, SessionStore, StoreEffect};
    use diri_proto::{ExitInfo, Resumability, SessionStatus};

    fn landed(more: usize) -> (SessionStore, tokio::sync::mpsc::UnboundedReceiver<StoreEffect>) {
        let (mut store, effects) = SessionStore::headless(Prefs::default());
        store.set_conversation_import_scan(preview_scan(1_800_000_000_000.0, more));
        (store, effects)
    }

    #[test]
    fn the_first_launch_page_preticks_the_five_suggestions_only() {
        let (store, _effects) = landed(7);
        let state = store.conversation_import();
        assert_eq!(state.selected_count(), 5);
        assert_eq!(state.visible_rows().len(), 5);
        assert!(store.conversation_import_visible());
        assert_eq!(state.focused.as_deref(), Some(state.suggested()[0].entry.id.as_str()));
    }

    #[test]
    fn browsing_lists_everything_and_the_filter_matches_every_word() {
        let (mut store, _effects) = landed(7);
        store.set_conversation_browsing(true);
        assert_eq!(store.conversation_import().visible_rows().len(), 12);
        for key in "flaky mac".chars() {
            store
                .conversation_query_mut()
                .apply(crate::query_editor::LocalEdit::Insert(key.to_string()));
        }
        let rows = store.conversation_import().visible_rows();
        assert_eq!(rows.len(), 1, "'flaky' alone also matches a Linux test");
        assert_eq!(
            rows[0].entry.title.as_deref(),
            Some("Investigate flaky CI on macOS")
        );
        // Ticking a browsed row adds to the five.
        let id = rows[0].entry.id.clone();
        store.toggle_conversation(&id);
        assert_eq!(store.conversation_import().selected_count(), 6);
        // Leaving browse clears the filter, keeps the ticks.
        store.set_conversation_browsing(false);
        assert!(store.conversation_import().query.is_empty());
        assert_eq!(store.conversation_import().selected_count(), 6);
    }

    #[test]
    fn import_sends_the_ticked_entries_newest_first_once() {
        let (mut store, mut effects) = landed(3);
        let second = store.conversation_import().suggested()[1].entry.id.clone();
        store.toggle_conversation(&second);
        store.move_conversation_focus(true);
        store.toggle_focused_conversation(); // the third row
        assert!(store.import_selected_conversations());
        assert!(!store.import_selected_conversations(), "one import at a time");
        let Ok(StoreEffect::ImportConversations(entries)) = effects.try_recv() else {
            panic!("import emits one effect");
        };
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        let suggested = store.conversation_import().suggested();
        assert_eq!(
            ids,
            vec![
                suggested[0].entry.id.as_str(),
                suggested[3].entry.id.as_str(),
                suggested[4].entry.id.as_str(),
            ]
        );
    }

    #[test]
    fn nothing_ticked_imports_nothing() {
        let (mut store, mut effects) = landed(0);
        let ids: Vec<_> = store
            .conversation_import()
            .suggested()
            .iter()
            .map(|candidate| candidate.entry.id.clone())
            .collect();
        for id in &ids {
            store.toggle_conversation(id);
        }
        assert!(!store.import_selected_conversations());
        assert!(effects.try_recv().is_err());
    }

    #[test]
    fn a_finished_import_adds_resumable_rows_and_retires_the_page() {
        let (mut store, _effects) = landed(0);
        assert!(store.import_selected_conversations());
        let mut record = crate::store::tests::session("imported-1", "anara", 1.0);
        record.status = SessionStatus::Exited(ExitInfo::imported());
        record.resumability = Resumability::Resumable;
        record.agent_session_id = Some("conversation".into());
        let transition = store.finish_conversation_import(Ok(diri_proto::SessionImportHistoryResult {
            sessions: vec![record],
            skipped: Vec::new(),
        }));
        assert!(transition.is_some());
        assert_eq!(store.sessions().len(), 1);
        // Zero spawn: nothing is selected for the person, so nothing resumes.
        assert_eq!(store.selected_session_id(), None);
        assert!(!store.conversation_import().importing);
        assert!(!store.conversation_import_visible());
        assert!(store.preferences().conversation_import_done);
        // Imported rows are not restart casualties: no "Resume all" batch.
        assert!(store.restart_ended_resumable().is_empty());
    }

    #[test]
    fn start_fresh_and_existing_sessions_retire_the_offer_for_good() {
        let (mut store, mut effects) = landed(0);
        store.dismiss_conversation_import();
        assert!(!store.conversation_import_visible());
        assert!(store.preferences().conversation_import_done);

        let (mut store, mut fresh_effects) = SessionStore::headless(Prefs::default());
        store.upsert_session(crate::store::tests::session("existing", "p", 1.0));
        while fresh_effects.try_recv().is_ok() {}
        store.request_first_launch_import();
        assert!(store.preferences().conversation_import_done);
        assert!(fresh_effects.try_recv().is_err(), "no scan for an established Mac");
        // The menu still opens it.
        store.open_conversation_import();
        assert!(matches!(fresh_effects.try_recv(), Ok(StoreEffect::ScanConversations)));
        let _ = effects.try_recv();
    }

    #[test]
    fn selecting_an_imported_session_resumes_it_once() {
        let (mut store, mut effects) = SessionStore::headless(Prefs::default());
        let mut record = crate::store::tests::session("imported", "anara", 1.0);
        record.status = SessionStatus::Exited(ExitInfo::imported());
        record.resumability = Resumability::Resumable;
        record.agent_session_id = Some("conversation".into());
        record.capabilities = None;
        let id = record.id.clone();
        store.upsert_session(record);
        while effects.try_recv().is_ok() {}
        // What every window does for the session it selects.
        assert!(store.auto_resume_referenced(&id));
        assert!(matches!(
            effects.try_recv(),
            Ok(StoreEffect::Resume { id: resumed, automatic: true }) if resumed == id
        ));
        assert!(!store.auto_resume_referenced(&id), "never twice");
    }
}
