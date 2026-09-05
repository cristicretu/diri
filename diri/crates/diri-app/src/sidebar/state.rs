use diri_proto::{ProjectId, SessionId};
use gpui::{Bounds, Pixels, Point};

use crate::delegation::SiblingProposal;
use crate::query_editor::QueryEditor;

pub const DEFAULT_SIDEBAR_WIDTH: f32 = 248.0;
pub const MIN_SIDEBAR_WIDTH: f32 = 200.0;
pub const MAX_SIDEBAR_WIDTH: f32 = 400.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMove {
    Up,
    Down,
    Home,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragItem {
    Project(ProjectId),
    Session {
        id: SessionId,
        project: ProjectId,
        /// The session that spawned this one, so a drop can tell a sibling
        /// from a cousin. Reordering only ever moves a row inside its own
        /// sibling run; re-parenting is the daemon's business, not a drag's.
        parent: Option<SessionId>,
        archived: bool,
    },
    Sessions(Vec<SessionId>),
}

/// Where the pointer sits inside a row during a drag. Outline views split a
/// row into two insertion bands and a core: the bands mean "put the dragged
/// row beside this one", the core means "drop it onto this one". Reordering
/// and delegation share the same rows, and this is what keeps them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    Before,
    Onto,
    After,
}

/// Classifies `position` against a row's bounds. `band` is the height of each
/// insertion band; `None` when the pointer is not over the row at all.
pub fn drop_zone(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    band: Pixels,
) -> Option<DropZone> {
    if !bounds.contains(&position) {
        return None;
    }
    let offset = position.y - bounds.origin.y;
    if offset < band {
        Some(DropZone::Before)
    } else if offset > bounds.size.height - band {
        Some(DropZone::After)
    } else {
        Some(DropZone::Onto)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Popover {
    NewAgent {
        directory: Option<String>,
        /// Selected spawn target: `None` = Local, `Some(host id)` = remote.
        host: Option<String>,
    },
    Account,
    SidebarLayout,
    ProjectActions {
        id: ProjectId,
        /// Window position of a right-click; `None` anchors below the header
        /// area like the ellipsis button.
        position: Option<Point<Pixels>>,
    },
    SessionActions {
        id: SessionId,
        position: Point<Pixels>,
    },
}

#[derive(Debug)]
pub struct SidebarUiState {
    pub visible: bool,
    pub width: f32,
    pub hovered_project: Option<ProjectId>,
    pub hovered_session: Option<SessionId>,
    pub hovered_control: Option<&'static str>,
    pub popover: Option<Popover>,
    /// Keyboard cursor within the compact grouping/ordering menu.
    pub layout_menu_index: usize,
    pub renaming: Option<SessionId>,
    pub rename_draft: QueryEditor,
    /// Session whose hover card is showing, plus the pointer's window y at
    /// the moment the card appeared (the card anchors beside that row).
    pub hover_card: Option<(SessionId, f32)>,
    pub drag: Option<DragItem>,
    pub drag_target: Option<String>,
    /// Project order when a header drag began. Headers reorder live under the
    /// pointer, so cancelling the gesture has to put them back.
    pub project_order_at_drag_start: Option<Vec<ProjectId>>,
    /// Keyboard source for the two-step mark-then-delegate equivalent.
    pub delegation_mark: Option<SessionId>,
    /// Empty-space drops stop here until the user confirms the sibling spawn.
    pub pending_sibling: Option<SiblingProposal>,
    /// Inline explanation for a rejected drop or keyboard target.
    pub delegation_notice: Option<String>,
    /// A live drag has staged a reorder in memory that still needs one prefs
    /// write when the gesture ends.
    pub order_dirty: bool,
    pub resize_origin: Option<(f32, f32)>,
    pub preview_account: bool,
    /// Keyboard focus follows a session identity, not a visual index. The
    /// previous order is retained solely to choose the next row (or the
    /// previous row at the end) if that session disappears.
    pub focus_cursor: Option<SessionId>,
    focus_order: Vec<SessionId>,
}

impl SidebarUiState {
    pub fn new(width: f32) -> Self {
        Self {
            visible: true,
            width: width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH),
            hovered_project: None,
            hovered_session: None,
            hovered_control: None,
            popover: None,
            layout_menu_index: 0,
            renaming: None,
            rename_draft: QueryEditor::default(),
            hover_card: None,
            drag: None,
            drag_target: None,
            project_order_at_drag_start: None,
            delegation_mark: None,
            pending_sibling: None,
            delegation_notice: None,
            order_dirty: false,
            resize_origin: None,
            preview_account: false,
            focus_cursor: None,
            focus_order: Vec::new(),
        }
    }

    /// Reconciles the identity cursor with the rows the sidebar is actually
    /// painting. Reorders leave the identity untouched. A removed or newly
    /// hidden row gives its position to the row that followed it, falling back
    /// to the previous row when it was last.
    pub fn reconcile_focus_cursor(&mut self, visible: &[SessionId], preferred: Option<&SessionId>) {
        let next = match self.focus_cursor.as_ref() {
            Some(cursor) if visible.contains(cursor) => Some(cursor.clone()),
            Some(cursor) => {
                let previous_index = self
                    .focus_order
                    .iter()
                    .position(|candidate| candidate == cursor)
                    .unwrap_or(0);
                visible
                    .get(previous_index.min(visible.len().saturating_sub(1)))
                    .cloned()
            }
            None => preferred
                .filter(|candidate| visible.contains(candidate))
                .cloned()
                .or_else(|| visible.first().cloned()),
        };
        self.focus_cursor = next;
        self.focus_order = visible.to_vec();
    }

    pub fn set_focus_cursor(&mut self, id: SessionId, visible: &[SessionId]) {
        if visible.contains(&id) {
            self.focus_cursor = Some(id);
            self.focus_order = visible.to_vec();
        }
    }

    pub fn move_focus_cursor(&mut self, movement: CursorMove, visible: &[SessionId]) -> bool {
        if visible.is_empty() {
            self.focus_cursor = None;
            self.focus_order.clear();
            return false;
        }
        let current = self
            .focus_cursor
            .as_ref()
            .and_then(|cursor| visible.iter().position(|candidate| candidate == cursor));
        let index = match movement {
            CursorMove::Home => 0,
            CursorMove::End => visible.len() - 1,
            CursorMove::Up => current.map_or(visible.len() - 1, |index| index.saturating_sub(1)),
            CursorMove::Down => current.map_or(0, |index| (index + 1).min(visible.len() - 1)),
        };
        let next = visible[index].clone();
        let changed = self.focus_cursor.as_ref() != Some(&next);
        self.focus_cursor = Some(next);
        self.focus_order = visible.to_vec();
        changed
    }

    pub fn set_width(&mut self, width: f32) {
        self.width = width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }

    pub fn reset_width(&mut self) {
        self.width = DEFAULT_SIDEBAR_WIDTH;
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        self.popover = None;
        self.hover_card = None;
    }

    pub fn begin_rename(&mut self, id: SessionId, title: impl Into<String>) {
        self.renaming = Some(id);
        self.rename_draft.clear();
        self.rename_draft.insert(&title.into());
        self.rename_draft.select_all();
        self.hover_card = None;
    }

    pub fn cancel_rename(&mut self) {
        self.renaming = None;
        self.rename_draft.clear();
    }

    pub fn take_rename(&mut self) -> Option<(SessionId, String)> {
        let id = self.renaming.take()?;
        let title = self.rename_draft.text().trim().to_owned();
        self.rename_draft.clear();
        (!title.is_empty()).then_some((id, title))
    }

    pub fn cancel_delegation(&mut self) {
        self.drag = None;
        self.drag_target = None;
        self.delegation_mark = None;
        self.pending_sibling = None;
        self.delegation_notice = None;
    }
}

pub fn move_before<T: Clone + PartialEq>(order: &mut Vec<T>, moved: &T, target: &T) {
    if moved == target {
        return;
    }
    let Some(index) = order.iter().position(|item| item == moved) else {
        return;
    };
    let item = order.remove(index);
    let target_index = order
        .iter()
        .position(|candidate| candidate == target)
        .unwrap_or(order.len());
    order.insert(target_index, item);
}

/// Moves `moved` to the far side of `target`: below it when it came from
/// above, above it when it came from below. Live reordering under a pointer
/// needs this rather than [`move_before`]: a row already sits before its
/// lower neighbour, so "move before it" could never move anything down.
pub fn move_past<T: Clone + PartialEq>(order: &mut Vec<T>, moved: &T, target: &T) {
    let position = |item: &T| order.iter().position(|candidate| candidate == item);
    let (Some(from), Some(to)) = (position(moved), position(target)) else {
        return;
    };
    if from == to {
        return;
    }
    let item = order.remove(from);
    order.insert(to, item);
}

pub fn move_to_end<T: Clone + PartialEq>(order: &mut Vec<T>, moved: &T) {
    let Some(index) = order.iter().position(|item| item == moved) else {
        return;
    };
    let item = order.remove(index);
    order.push(item);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_is_clamped_and_resettable() {
        let mut state = SidebarUiState::new(900.0);
        assert_eq!(state.width, MAX_SIDEBAR_WIDTH);
        state.set_width(120.0);
        assert_eq!(state.width, MIN_SIDEBAR_WIDTH);
        state.reset_width();
        assert_eq!(state.width, DEFAULT_SIDEBAR_WIDTH);
    }

    #[test]
    fn live_reorder_moves_relative_to_target() {
        let mut values = vec![1, 2, 3, 4];
        move_before(&mut values, &4, &2);
        assert_eq!(values, [1, 4, 2, 3]);
        move_to_end(&mut values, &1);
        assert_eq!(values, [4, 2, 3, 1]);
    }

    #[test]
    fn move_past_crosses_the_target_in_either_direction() {
        let mut values = vec![1, 2, 3];
        move_past(&mut values, &1, &2);
        assert_eq!(
            values,
            [2, 1, 3],
            "dragging down by one lands below the neighbour"
        );
        move_past(&mut values, &3, &2);
        assert_eq!(
            values,
            [3, 2, 1],
            "dragging up by one lands above the neighbour"
        );
        move_past(&mut values, &3, &1);
        assert_eq!(values, [2, 1, 3]);
        move_past(&mut values, &2, &2);
        assert_eq!(values, [2, 1, 3], "a row over itself stays put");
    }

    #[test]
    fn drop_zone_splits_a_row_into_bands_and_a_core() {
        use gpui::{px, size};
        let bounds = Bounds::new(gpui::point(px(0.0), px(100.0)), size(px(200.0), px(28.0)));
        let band = px(7.0);
        let at = |y: f32| drop_zone(bounds, gpui::point(px(20.0), px(y)), band);
        assert_eq!(at(99.0), None);
        assert_eq!(at(100.0), Some(DropZone::Before));
        assert_eq!(at(106.0), Some(DropZone::Before));
        assert_eq!(at(107.0), Some(DropZone::Onto));
        assert_eq!(at(114.0), Some(DropZone::Onto));
        assert_eq!(at(121.0), Some(DropZone::Onto));
        assert_eq!(at(122.0), Some(DropZone::After));
        assert_eq!(at(127.9), Some(DropZone::After));
        assert_eq!(at(128.0), None);
    }

    #[test]
    fn empty_rename_is_discarded() {
        let mut state = SidebarUiState::new(DEFAULT_SIDEBAR_WIDTH);
        state.begin_rename(SessionId::new("one"), "  ");
        assert!(state.take_rename().is_none());
    }

    #[test]
    fn focus_cursor_moves_and_clamps_across_visible_rows() {
        let rows = ["one", "two", "three"].map(SessionId::new);
        let mut state = SidebarUiState::new(DEFAULT_SIDEBAR_WIDTH);
        state.reconcile_focus_cursor(&rows, Some(&rows[1]));

        assert_eq!(state.focus_cursor, Some(rows[1].clone()));
        assert!(state.move_focus_cursor(CursorMove::Up, &rows));
        assert_eq!(state.focus_cursor, Some(rows[0].clone()));
        assert!(!state.move_focus_cursor(CursorMove::Up, &rows));
        assert!(state.move_focus_cursor(CursorMove::End, &rows));
        assert_eq!(state.focus_cursor, Some(rows[2].clone()));
        assert!(state.move_focus_cursor(CursorMove::Home, &rows));
        assert_eq!(state.focus_cursor, Some(rows[0].clone()));
    }

    #[test]
    fn focus_cursor_tracks_identity_across_reorders() {
        let rows = ["one", "two", "three"].map(SessionId::new);
        let mut state = SidebarUiState::new(DEFAULT_SIDEBAR_WIDTH);
        state.reconcile_focus_cursor(&rows, Some(&rows[1]));

        state.reconcile_focus_cursor(&[rows[2].clone(), rows[1].clone(), rows[0].clone()], None);

        assert_eq!(state.focus_cursor, Some(rows[1].clone()));
    }

    #[test]
    fn removed_focus_cursor_lands_on_a_sensible_neighbour() {
        let rows = ["one", "two", "three"].map(SessionId::new);
        let mut state = SidebarUiState::new(DEFAULT_SIDEBAR_WIDTH);
        state.reconcile_focus_cursor(&rows, Some(&rows[1]));

        state.reconcile_focus_cursor(&[rows[0].clone(), rows[2].clone()], None);
        assert_eq!(state.focus_cursor, Some(rows[2].clone()));

        state.set_focus_cursor(rows[2].clone(), &[rows[0].clone(), rows[2].clone()]);
        state.reconcile_focus_cursor(&[rows[0].clone()], None);
        assert_eq!(state.focus_cursor, Some(rows[0].clone()));
    }

    #[test]
    fn cancellation_discards_all_unconfirmed_delegation_state() {
        let mut state = SidebarUiState::new(DEFAULT_SIDEBAR_WIDTH);
        state.delegation_mark = Some(SessionId::new("marked"));
        state.drag = Some(DragItem::Session {
            id: SessionId::new("source"),
            project: ProjectId::new("project"),
            parent: None,
            archived: false,
        });
        state.pending_sibling = Some(SiblingProposal {
            source_id: SessionId::new("source"),
            source_title: "Source".to_owned(),
            kind: diri_proto::AgentKind::CODEX,
            project_id: ProjectId::new("project"),
            cwd: "/repo".to_owned(),
            prompt: "Keep going".to_owned(),
            parent: None,
            host: None,
        });
        state.delegation_notice = Some("proposal".to_owned());

        state.cancel_delegation();

        assert!(state.drag.is_none());
        assert!(state.pending_sibling.is_none());
        assert!(state.delegation_notice.is_none());
        assert!(state.delegation_mark.is_none());
    }
}
