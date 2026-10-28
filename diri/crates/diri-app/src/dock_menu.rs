//! What the Dock icon's menu lists: the sessions that need you first (a
//! question or permission waiting, then a failure, then a finished turn you
//! have not seen), then the agents still working. Each row carries what it
//! needs to be drawn as one native menu item: its agent, its title, what it
//! is waiting on or doing, and since when.
//!
//! The model is built when the menu opens and thrown away after, from the
//! same session records and inbox the sidebar reads, so nothing is kept up
//! to date while nobody looks (`macos::dock_menu`).

use diri_proto::{SessionId, SessionRecord};
use diri_ui::StatusState;
use unicode_segmentation::UnicodeSegmentation;

use crate::notification_feed::NotificationKind;
use crate::session_presentation::{activity_line, age_label, status_state};
use crate::store::SessionStore;

/// Rows a Dock menu shows per section before folding the rest into "N more".
/// The Dock draws its menu above the icon, so a long list runs off the top
/// of the screen.
pub(crate) const MAX_ATTENTION_ROWS: usize = 12;
pub(crate) const MAX_WORKING_ROWS: usize = 8;

/// Why a session is in the menu, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DockRowState {
    NeedsYou,
    Failed,
    Finished,
    Working,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockMenuRow {
    pub(crate) session_id: SessionId,
    pub(crate) agent_id: String,
    pub(crate) state: DockRowState,
    pub(crate) title: String,
    /// The question it waits on, or what it did or is doing.
    pub(crate) detail: String,
    /// "now", "7m"… since it started waiting, failed or finished.
    pub(crate) age: Option<String>,
}

impl DockMenuRow {
    /// The item's one line: `title — detail · age`. The detail is left out
    /// when a blocked row has nothing more specific than "Needs you" and the
    /// section header already says so.
    pub(crate) fn label(&self) -> String {
        let mut label = self.title.clone();
        if !self.detail.is_empty() {
            label.push_str(" — ");
            label.push_str(&self.detail);
        }
        if let Some(age) = &self.age {
            label.push_str(" · ");
            label.push_str(age);
        }
        label
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DockMenuModel {
    /// Needs you, then failed, then finished-unseen; newest first in each.
    pub(crate) attention: Vec<DockMenuRow>,
    /// Agents working, in sidebar order.
    pub(crate) working: Vec<DockMenuRow>,
    /// Rows folded away by the per-section caps.
    pub(crate) more: usize,
    /// Whether "Mark All as Seen" would change anything.
    pub(crate) can_mark_seen: bool,
}

impl DockMenuModel {
    /// The menu as text, one item per line; what a test or a PR shows.
    #[cfg(test)]
    pub(crate) fn render_text(&self) -> String {
        use crate::i18n::{t, tf};
        let mut lines = Vec::new();
        if !self.attention.is_empty() {
            lines.push(format!("[{}]", t("dock.section_needs_you")));
            lines.extend(self.attention.iter().map(DockMenuRow::label));
        }
        if !self.working.is_empty() {
            lines.push(format!("[{}]", t("dock.section_working")));
            lines.extend(self.working.iter().map(DockMenuRow::label));
        }
        if self.more > 0 {
            lines.push(tf("dock.more", &[("n", &self.more)]));
        }
        if self.can_mark_seen {
            lines.push("—".into());
            lines.push(t("dock.mark_all_seen").into());
        }
        lines.join("\n")
    }
}

/// Builds the menu from the store as it is now. One pass over the sidebar's
/// sessions and one inbox lookup per session.
pub(crate) fn build(store: &mut SessionStore, now_ms: u64) -> DockMenuModel {
    let projection = store.menu_bar_projection();
    let mut attention = Vec::new();
    let mut working = Vec::new();
    for session in projection.projects.iter().flat_map(|group| &group.active) {
        let state = tab_state(store, session);
        let unseen = store.notifications().latest_unread(&session.id);
        let (row_state, line) = match state {
            StatusState::NeedsInput { .. } => (
                DockRowState::NeedsYou,
                activity_line(session, state, None),
            ),
            StatusState::Working if !session.effective_kind().is_terminal() => (
                DockRowState::Working,
                activity_line(session, state, None),
            ),
            StatusState::Working | StatusState::Hibernated => continue,
            _ => match unseen {
                Some(entry) => (
                    if entry.kind == NotificationKind::Failed {
                        DockRowState::Failed
                    } else {
                        DockRowState::Finished
                    },
                    activity_line(session, StatusState::DoneUnseen, Some(entry)),
                ),
                None if state == StatusState::DoneUnseen => (
                    DockRowState::Finished,
                    activity_line(session, state, None),
                ),
                None => continue,
            },
        };
        let row = (
            line.since_ms,
            DockMenuRow {
                session_id: session.id.clone(),
                agent_id: session.effective_kind().id().to_owned(),
                state: row_state,
                title: compact(&crate::switcher::display_title(session), 40),
                detail: if row_state == DockRowState::NeedsYou
                    && line.text == crate::i18n::t("activity.needs_you")
                {
                    String::new()
                } else {
                    compact(&line.text, 60)
                },
                age: line
                    .since_ms
                    .filter(|_| row_state != DockRowState::Working)
                    .map(|since| age_label(since, now_ms).0),
            },
        );
        if row_state == DockRowState::Working {
            working.push(row.1);
        } else {
            attention.push(row);
        }
    }
    // Stable: equal states and ages keep the sidebar's order.
    attention.sort_by(|(a_since, a), (b_since, b)| {
        a.state
            .cmp(&b.state)
            .then_with(|| b_since.unwrap_or(0).cmp(&a_since.unwrap_or(0)))
    });
    let can_mark_seen = attention
        .iter()
        .any(|(_, row)| row.state != DockRowState::NeedsYou);
    let more = attention.len().saturating_sub(MAX_ATTENTION_ROWS)
        + working.len().saturating_sub(MAX_WORKING_ROWS);
    working.truncate(MAX_WORKING_ROWS);
    DockMenuModel {
        attention: attention
            .into_iter()
            .take(MAX_ATTENTION_ROWS)
            .map(|(_, row)| row)
            .collect(),
        working,
        more,
        can_mark_seen,
    }
}

/// A tab's state: its own, or the most urgent of its split panes', so a
/// question in a side pane surfaces the tab.
fn tab_state(store: &SessionStore, session: &SessionRecord) -> StatusState {
    fn rank(state: StatusState) -> u8 {
        match state {
            StatusState::NeedsInput { .. } => 3,
            StatusState::Working => 2,
            StatusState::DoneUnseen => 1,
            _ => 0,
        }
    }
    let own = status_state(session, store.migrating().contains(&session.id));
    let (_, panes) = store.pane_group(&session.id).unwrap_or_default();
    panes.iter().fold(own, |state, pane| {
        let pane = status_state(pane, false);
        if rank(pane) > rank(state) { pane } else { state }
    })
}

/// One line, at most `limit` graphemes, never splitting an emoji.
fn compact(text: &str, limit: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut graphemes = single_line.graphemes(true);
    let mut result: String = graphemes.by_ref().take(limit).collect();
    if graphemes.next().is_some() {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::AgentKind;

    #[test]
    fn long_titles_fold_onto_one_line_without_splitting_graphemes() {
        assert_eq!(compact("fix\n  the\tlogin", 40), "fix the login");
        assert_eq!(compact(&"👩🏽‍💻".repeat(5), 3), format!("{}…", "👩🏽‍💻".repeat(3)));
    }

    #[test]
    fn a_label_reads_title_detail_and_age() {
        let row = DockMenuRow {
            session_id: SessionId::new("s"),
            agent_id: AgentKind::CLAUDE_CODE_ID.into(),
            state: DockRowState::NeedsYou,
            title: "fix login".into(),
            detail: "Allow deploy to staging?".into(),
            age: Some("3m".into()),
        };
        assert_eq!(row.label(), "fix login — Allow deploy to staging? · 3m");
        let bare = DockMenuRow {
            detail: String::new(),
            age: None,
            ..row
        };
        assert_eq!(bare.label(), "fix login");
    }
}
