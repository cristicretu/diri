use diri_proto::{ProjectId, SessionId};
use gpui::{Pixels, Point};

use crate::query_editor::QueryEditor;

pub const DEFAULT_SIDEBAR_WIDTH: f32 = 248.0;
pub const MIN_SIDEBAR_WIDTH: f32 = 200.0;
pub const MAX_SIDEBAR_WIDTH: f32 = 400.0;

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

#[derive(Clone, Debug, PartialEq)]
pub enum Popover {
    NewAgent {
        directory: Option<String>,
        /// Selected spawn target: `None` = Local, `Some(host id)` = remote.
        host: Option<String>,
    },
    Account,
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
    pub renaming: Option<SessionId>,
    pub rename_draft: QueryEditor,
    /// Session whose hover card is showing, plus the pointer's window y at
    /// the moment the card appeared (the card anchors beside that row).
    pub hover_card: Option<(SessionId, f32)>,
    pub drag: Option<DragItem>,
    pub drag_target: Option<String>,
    /// A live drag has staged a reorder in memory that still needs one prefs
    /// write when the gesture ends.
    pub order_dirty: bool,
    pub resize_origin: Option<(f32, f32)>,
    pub preview_account: bool,
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
            renaming: None,
            rename_draft: QueryEditor::default(),
            hover_card: None,
            drag: None,
            drag_target: None,
            order_dirty: false,
            resize_origin: None,
            preview_account: false,
        }
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
    fn empty_rename_is_discarded() {
        let mut state = SidebarUiState::new(DEFAULT_SIDEBAR_WIDTH);
        state.begin_rename(SessionId::new("one"), "  ");
        assert!(state.take_rename().is_none());
    }
}
