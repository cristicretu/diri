//! Keyboard-first state for the Ctrl-Tab switcher and session overview.
//!
//! The state machines are deliberately independent of GPUI. This keeps the
//! release-to-commit behavior deterministic and lets the app shell render the
//! same state as either a board or a compact list.

use std::collections::HashSet;

use diri_proto::{AttentionLevel, SessionId, SessionRecord, SessionStatus, TitleSource};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleDirection {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitcherKey {
    Tab { control: bool, shift: bool },
    Escape,
    Enter,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SwitcherOutcome {
    #[default]
    Ignored,
    Consumed,
    Opened,
    HighlightChanged,
    Committed(SessionId),
    Cancelled,
}

impl SwitcherOutcome {
    #[must_use]
    pub const fn consumed(&self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

/// Cmd-Tab-style transient selection. The active session is never changed
/// until Ctrl is released or Return is pressed, so Escape can cancel cleanly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSwitcherState {
    visible: bool,
    order: Vec<SessionId>,
    index: usize,
}

impl SessionSwitcherState {
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    #[must_use]
    pub fn order(&self) -> &[SessionId] {
        &self.order
    }

    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub fn highlighted(&self) -> Option<&SessionId> {
        self.visible.then(|| self.order.get(self.index)).flatten()
    }

    pub fn open(&mut self, order: Vec<SessionId>, direction: CycleDirection) -> bool {
        if order.len() <= 1 {
            return false;
        }
        self.index = match direction {
            CycleDirection::Forward => 1,
            CycleDirection::Reverse => order.len() - 1,
        };
        self.order = order;
        self.visible = true;
        true
    }

    pub fn advance(&mut self, direction: CycleDirection) -> bool {
        if !self.visible || self.order.is_empty() {
            return false;
        }
        self.index = match direction {
            CycleDirection::Forward => (self.index + 1) % self.order.len(),
            CycleDirection::Reverse => (self.index + self.order.len() - 1) % self.order.len(),
        };
        true
    }

    pub fn cancel(&mut self) -> bool {
        std::mem::take(&mut self.visible)
    }

    pub fn commit(&mut self) -> Option<SessionId> {
        let selected = self.highlighted().cloned();
        self.visible = false;
        selected
    }

    pub fn commit_index(&mut self, index: usize) -> Option<SessionId> {
        if !self.visible || index >= self.order.len() {
            return None;
        }
        self.index = index;
        self.commit()
    }

    /// Mirrors `AppModel.handleSwitcherKey`: Ctrl-Tab always stays out of the
    /// terminal, Shift reverses, arrows cycle, Escape cancels, and every stray
    /// key is swallowed while the overlay is open.
    pub fn key_down(&mut self, key: SwitcherKey, mru_order: &[SessionId]) -> SwitcherOutcome {
        if let SwitcherKey::Tab {
            control: true,
            shift,
        } = key
        {
            let direction = if shift {
                CycleDirection::Reverse
            } else {
                CycleDirection::Forward
            };
            if self.visible {
                self.advance(direction);
                return SwitcherOutcome::HighlightChanged;
            }
            return if self.open(mru_order.to_vec(), direction) {
                SwitcherOutcome::Opened
            } else {
                SwitcherOutcome::Consumed
            };
        }

        if !self.visible {
            return SwitcherOutcome::Ignored;
        }
        match key {
            SwitcherKey::Escape => {
                self.cancel();
                SwitcherOutcome::Cancelled
            }
            SwitcherKey::Enter => self
                .commit()
                .map_or(SwitcherOutcome::Consumed, SwitcherOutcome::Committed),
            SwitcherKey::ArrowLeft | SwitcherKey::ArrowUp => {
                self.advance(CycleDirection::Reverse);
                SwitcherOutcome::HighlightChanged
            }
            SwitcherKey::ArrowRight | SwitcherKey::ArrowDown => {
                self.advance(CycleDirection::Forward);
                SwitcherOutcome::HighlightChanged
            }
            SwitcherKey::Tab { .. } | SwitcherKey::Other => SwitcherOutcome::Consumed,
        }
    }

    /// Modifier changes are observed separately from key-down events. Like the
    /// Swift local monitor, releasing Ctrl commits but does not swallow the
    /// modifier event itself.
    pub fn modifiers_changed(&mut self, control_held: bool) -> SwitcherOutcome {
        if self.visible && !control_held {
            self.commit()
                .map_or(SwitcherOutcome::Ignored, SwitcherOutcome::Committed)
        } else {
            SwitcherOutcome::Ignored
        }
    }

    /// Drop sessions removed by another client without changing the highlighted
    /// identity when it still exists.
    pub fn reconcile(&mut self, live_ids: &HashSet<SessionId>) {
        let highlighted = self.highlighted().cloned();
        self.order.retain(|id| live_ids.contains(id));
        if self.order.len() <= 1 {
            self.visible = false;
        }
        self.index = highlighted
            .and_then(|id| self.order.iter().position(|candidate| candidate == &id))
            .unwrap_or_else(|| self.index.min(self.order.len().saturating_sub(1)));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverviewMode {
    #[default]
    Board,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverviewLane {
    Running,
    NeedsInput,
    Done,
    Asleep,
    Ended,
}

impl OverviewLane {
    pub const ALL: [Self; 5] = [
        Self::Running,
        Self::NeedsInput,
        Self::Done,
        Self::Asleep,
        Self::Ended,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::NeedsInput => "Needs Input",
            Self::Done => "Done",
            Self::Asleep => "Asleep",
            Self::Ended => "Ended",
        }
    }

    #[must_use]
    pub fn for_session(session: &SessionRecord) -> Self {
        if session.hibernation.is_some() {
            Self::Asleep
        } else if matches!(session.status, SessionStatus::Exited(_)) {
            Self::Ended
        } else {
            match session.attention() {
                AttentionLevel::NeedsInput => Self::NeedsInput,
                AttentionLevel::DoneUnseen => Self::Done,
                AttentionLevel::None
                | AttentionLevel::IdleSeen
                | AttentionLevel::Working
                | AttentionLevel::Unknown => Self::Running,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverviewFilter {
    #[default]
    All,
    Lane(OverviewLane),
}

impl OverviewFilter {
    #[must_use]
    pub fn matches(self, session: &SessionRecord) -> bool {
        match self {
            Self::All => true,
            Self::Lane(lane) => OverviewLane::for_session(session) == lane,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverviewArrow {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OverviewOutcome {
    #[default]
    Ignored,
    Changed,
    Dismissed,
    Activate(SessionId),
    RequestClose(Vec<SessionId>),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionOverviewState {
    visible: bool,
    mode: OverviewMode,
    filter: OverviewFilter,
    query: String,
    selection: HashSet<SessionId>,
    focused: Option<SessionId>,
}

impl SessionOverviewState {
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    #[must_use]
    pub const fn mode(&self) -> OverviewMode {
        self.mode
    }

    #[must_use]
    pub const fn filter(&self) -> OverviewFilter {
        self.filter
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub fn selection(&self) -> &HashSet<SessionId> {
        &self.selection
    }

    #[must_use]
    pub fn focused(&self) -> Option<&SessionId> {
        self.focused.as_ref()
    }

    pub fn open(&mut self, sessions: &[SessionRecord]) {
        self.visible = true;
        self.focus_first_visible(sessions);
    }

    pub fn toggle(&mut self, sessions: &[SessionRecord]) {
        if self.visible {
            self.visible = false;
        } else {
            self.open(sessions);
        }
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    pub fn set_mode(&mut self, mode: OverviewMode, sessions: &[SessionRecord]) {
        self.mode = mode;
        self.reconcile(sessions);
    }

    pub fn set_filter(&mut self, filter: OverviewFilter, sessions: &[SessionRecord]) {
        self.filter = filter;
        self.reconcile(sessions);
    }

    pub fn append_query(&mut self, text: &str, sessions: &[SessionRecord]) -> bool {
        if text.is_empty() || text.chars().any(char::is_control) {
            return false;
        }
        self.query.push_str(text);
        self.reconcile(sessions);
        true
    }

    pub fn backspace(&mut self, sessions: &[SessionRecord]) -> OverviewOutcome {
        if self.query.pop().is_some() {
            self.reconcile(sessions);
            OverviewOutcome::Changed
        } else if self.selection.is_empty() {
            OverviewOutcome::Ignored
        } else {
            self.close_selected()
        }
    }

    /// Swift's overview escape cascade: filter → selection → overlay.
    pub fn escape(&mut self, sessions: &[SessionRecord]) -> OverviewOutcome {
        if !self.query.is_empty() {
            self.query.clear();
            self.reconcile(sessions);
            OverviewOutcome::Changed
        } else if !self.selection.is_empty() {
            self.selection.clear();
            OverviewOutcome::Changed
        } else if self.visible {
            self.visible = false;
            OverviewOutcome::Dismissed
        } else {
            OverviewOutcome::Ignored
        }
    }

    pub fn activate(&mut self, id: SessionId) -> OverviewOutcome {
        if self.selection.is_empty() {
            self.visible = false;
            OverviewOutcome::Activate(id)
        } else {
            self.toggle_selection(id);
            OverviewOutcome::Changed
        }
    }

    pub fn activate_focused(&mut self) -> OverviewOutcome {
        self.focused
            .clone()
            .map_or(OverviewOutcome::Ignored, |id| self.activate(id))
    }

    pub fn toggle_selection(&mut self, id: SessionId) {
        if !self.selection.remove(&id) {
            self.selection.insert(id);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// Union, rather than replacement, preserves picks made under another
    /// lane filter exactly like the Swift scope cuts.
    pub fn select_all_visible(&mut self, sessions: &[SessionRecord]) {
        let visible: Vec<_> = self
            .visible_sessions(sessions)
            .map(|session| session.id.clone())
            .collect();
        self.selection.extend(visible);
    }

    pub fn close_selected(&mut self) -> OverviewOutcome {
        if self.selection.is_empty() {
            return OverviewOutcome::Ignored;
        }
        let mut ids: Vec<_> = self.selection.drain().collect();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        OverviewOutcome::RequestClose(ids)
    }

    pub fn close_one(&mut self, id: SessionId) -> OverviewOutcome {
        self.selection.remove(&id);
        OverviewOutcome::RequestClose(vec![id])
    }

    pub fn move_focus(&mut self, arrow: OverviewArrow, sessions: &[SessionRecord]) -> bool {
        let visible: Vec<_> = self.visible_sessions(sessions).collect();
        if visible.is_empty() {
            self.focused = None;
            return false;
        }
        let current = self
            .focused
            .as_ref()
            .and_then(|id| visible.iter().position(|session| &session.id == id))
            .unwrap_or(0);

        let next = match self.mode {
            OverviewMode::List => match arrow {
                OverviewArrow::Left | OverviewArrow::Up => current.saturating_sub(1),
                OverviewArrow::Right | OverviewArrow::Down => (current + 1).min(visible.len() - 1),
            },
            OverviewMode::Board => self.board_target(arrow, &visible, current),
        };
        let changed = self.focused.as_ref() != Some(&visible[next].id);
        self.focused = Some(visible[next].id.clone());
        changed
    }

    pub fn visible_sessions<'a>(
        &'a self,
        sessions: &'a [SessionRecord],
    ) -> impl Iterator<Item = &'a SessionRecord> + 'a {
        sessions.iter().filter(|session| {
            self.filter.matches(session) && fuzzy_matches(&self.query, &display_title(session))
        })
    }

    #[must_use]
    pub fn lane_sessions<'a>(
        &'a self,
        lane: OverviewLane,
        sessions: &'a [SessionRecord],
    ) -> Vec<&'a SessionRecord> {
        self.visible_sessions(sessions)
            .filter(|session| OverviewLane::for_session(session) == lane)
            .collect()
    }

    pub fn reconcile(&mut self, sessions: &[SessionRecord]) {
        let live: HashSet<_> = sessions.iter().map(|session| session.id.clone()).collect();
        self.selection.retain(|id| live.contains(id));
        if self.focused.as_ref().is_none_or(|id| {
            !self
                .visible_sessions(sessions)
                .any(|session| &session.id == id)
        }) {
            self.focus_first_visible(sessions);
        }
    }

    fn focus_first_visible(&mut self, sessions: &[SessionRecord]) {
        let first = self
            .visible_sessions(sessions)
            .next()
            .map(|session| session.id.clone());
        self.focused = first;
    }

    fn board_target(
        &self,
        arrow: OverviewArrow,
        visible: &[&SessionRecord],
        current: usize,
    ) -> usize {
        let current_lane = OverviewLane::for_session(visible[current]);
        let lane_items: Vec<_> = visible
            .iter()
            .enumerate()
            .filter(|(_, session)| OverviewLane::for_session(session) == current_lane)
            .map(|(index, _)| index)
            .collect();
        let lane_position = lane_items
            .iter()
            .position(|index| *index == current)
            .unwrap_or(0);

        match arrow {
            OverviewArrow::Up => lane_items[lane_position.saturating_sub(1)],
            OverviewArrow::Down => lane_items[(lane_position + 1).min(lane_items.len() - 1)],
            OverviewArrow::Left | OverviewArrow::Right => {
                let step = if arrow == OverviewArrow::Left { -1 } else { 1 };
                let current_lane_index = OverviewLane::ALL
                    .iter()
                    .position(|lane| *lane == current_lane)
                    .unwrap_or(0) as isize;
                let mut target_lane = current_lane_index + step;
                while (0..OverviewLane::ALL.len() as isize).contains(&target_lane) {
                    let lane = OverviewLane::ALL[target_lane as usize];
                    let candidates: Vec<_> = visible
                        .iter()
                        .enumerate()
                        .filter(|(_, session)| OverviewLane::for_session(session) == lane)
                        .map(|(index, _)| index)
                        .collect();
                    if !candidates.is_empty() {
                        return candidates[lane_position.min(candidates.len() - 1)];
                    }
                    target_lane += step;
                }
                current
            }
        }
    }
}

#[must_use]
pub fn display_title(session: &SessionRecord) -> String {
    if session.title_source == TitleSource::Placeholder {
        if matches!(session.status, SessionStatus::Exited(_)) {
            "Ended".to_owned()
        } else {
            "Untitled".to_owned()
        }
    } else {
        session.title.clone()
    }
}

fn fuzzy_matches(query: &str, candidate: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|needle| candidate.by_ref().any(|character| character == needle))
}

#[cfg(test)]
mod tests {
    use diri_proto::{
        AgentKind, DateMillis, ExitInfo, ExitReason, ProjectId, Resumability, SessionRecord,
    };

    use super::*;

    fn id(value: &str) -> SessionId {
        SessionId::new(value)
    }

    fn session(value: &str, status: SessionStatus) -> SessionRecord {
        SessionRecord {
            id: id(value),
            kind: AgentKind::CLAUDE_CODE,
            cwd: "/work/project".to_owned(),
            project_id: ProjectId::new("project"),
            worktree_path: None,
            git_branch: None,
            title: value.to_owned(),
            title_source: TitleSource::UserRename,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
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
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    #[test]
    fn switcher_matches_swift_hold_cycle_release_and_cancel_semantics() {
        let order = vec![id("current"), id("previous"), id("older")];
        let mut switcher = SessionSwitcherState::default();

        assert_eq!(
            switcher.key_down(
                SwitcherKey::Tab {
                    control: true,
                    shift: false,
                },
                &order,
            ),
            SwitcherOutcome::Opened
        );
        assert_eq!(switcher.highlighted(), Some(&id("previous")));
        assert_eq!(
            switcher.key_down(SwitcherKey::ArrowLeft, &order),
            SwitcherOutcome::HighlightChanged
        );
        assert_eq!(switcher.highlighted(), Some(&id("current")));
        assert_eq!(
            switcher.modifiers_changed(false),
            SwitcherOutcome::Committed(id("current"))
        );
        assert!(!switcher.is_visible());

        switcher.key_down(
            SwitcherKey::Tab {
                control: true,
                shift: true,
            },
            &order,
        );
        assert_eq!(switcher.highlighted(), Some(&id("older")));
        assert_eq!(
            switcher.key_down(SwitcherKey::Escape, &order),
            SwitcherOutcome::Cancelled
        );
        assert_eq!(switcher.modifiers_changed(false), SwitcherOutcome::Ignored);
    }

    #[test]
    fn switcher_wraps_and_swallows_stray_keys_while_open() {
        let order = vec![id("one"), id("two"), id("three")];
        let mut switcher = SessionSwitcherState::default();
        switcher.open(order.clone(), CycleDirection::Reverse);
        assert_eq!(switcher.highlighted(), Some(&id("three")));
        switcher.advance(CycleDirection::Forward);
        assert_eq!(switcher.highlighted(), Some(&id("one")));
        switcher.advance(CycleDirection::Reverse);
        assert_eq!(switcher.highlighted(), Some(&id("three")));
        assert_eq!(
            switcher.key_down(SwitcherKey::Other, &order),
            SwitcherOutcome::Consumed
        );
    }

    #[test]
    fn overview_lanes_are_partitioned_by_lifecycle_then_attention() {
        let mut done = session("done", SessionStatus::Idle);
        done.last_turn_completed_at = Some(DateMillis(10.0));
        done.last_seen_at = Some(DateMillis(1.0));
        let waiting = session(
            "waiting",
            SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question),
        );
        let ended = session(
            "ended",
            SessionStatus::Exited(ExitInfo {
                reason: ExitReason::Exited,
                code: Some(0),
                signal: None,
            }),
        );
        let mut asleep = session("asleep", SessionStatus::Working);
        asleep.hibernation = Some(diri_proto::HibernationInfo {
            reason: diri_proto::HibernationReason::Idle,
            since: DateMillis(1.0),
            tree_pids: Vec::new(),
            tree_start_times: None,
        });

        assert_eq!(OverviewLane::for_session(&done), OverviewLane::Done);
        assert_eq!(
            OverviewLane::for_session(&waiting),
            OverviewLane::NeedsInput
        );
        assert_eq!(OverviewLane::for_session(&ended), OverviewLane::Ended);
        assert_eq!(OverviewLane::for_session(&asleep), OverviewLane::Asleep);
        assert_eq!(
            OverviewLane::for_session(&session("live", SessionStatus::Working)),
            OverviewLane::Running
        );
    }

    #[test]
    fn overview_escape_and_bulk_close_match_swift_cascade() {
        let sessions = vec![
            session("alpha", SessionStatus::Working),
            session("beta", SessionStatus::Idle),
        ];
        let mut overview = SessionOverviewState::default();
        overview.open(&sessions);
        overview.toggle_selection(id("alpha"));
        overview.append_query("a", &sessions);

        assert_eq!(overview.escape(&sessions), OverviewOutcome::Changed);
        assert!(overview.query().is_empty());
        assert_eq!(overview.selection().len(), 1);
        assert_eq!(overview.escape(&sessions), OverviewOutcome::Changed);
        assert!(overview.selection().is_empty());
        assert_eq!(overview.escape(&sessions), OverviewOutcome::Dismissed);

        overview.open(&sessions);
        overview.select_all_visible(&sessions);
        assert_eq!(
            overview.close_selected(),
            OverviewOutcome::RequestClose(vec![id("alpha"), id("beta")])
        );
    }

    #[test]
    fn board_arrows_preserve_row_across_nonempty_lanes_and_list_is_linear() {
        let sessions = vec![
            session("run-1", SessionStatus::Working),
            session("run-2", SessionStatus::Starting),
            session(
                "wait-1",
                SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission),
            ),
            session(
                "wait-2",
                SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question),
            ),
            session(
                "ended",
                SessionStatus::Exited(ExitInfo {
                    reason: ExitReason::Exited,
                    code: Some(0),
                    signal: None,
                }),
            ),
        ];
        let mut overview = SessionOverviewState::default();
        overview.open(&sessions);
        overview.move_focus(OverviewArrow::Down, &sessions);
        assert_eq!(overview.focused(), Some(&id("run-2")));
        overview.move_focus(OverviewArrow::Right, &sessions);
        assert_eq!(overview.focused(), Some(&id("wait-2")));
        overview.move_focus(OverviewArrow::Right, &sessions);
        assert_eq!(overview.focused(), Some(&id("ended")));

        overview.set_mode(OverviewMode::List, &sessions);
        overview.move_focus(OverviewArrow::Up, &sessions);
        assert_eq!(overview.focused(), Some(&id("wait-2")));
    }

    #[test]
    fn filters_query_and_selection_reconcile_without_ghosts() {
        let sessions = vec![
            session("Alpha compile", SessionStatus::Working),
            session("Beta review", SessionStatus::Idle),
        ];
        let mut overview = SessionOverviewState::default();
        overview.open(&sessions);
        overview.append_query("ac", &sessions);
        assert_eq!(
            overview
                .visible_sessions(&sessions)
                .map(|session| session.id.clone())
                .collect::<Vec<_>>(),
            vec![id("Alpha compile")]
        );
        overview.toggle_selection(id("Alpha compile"));
        overview.reconcile(&sessions[1..]);
        assert!(overview.selection().is_empty());
        assert!(overview.focused().is_none());
    }
}
