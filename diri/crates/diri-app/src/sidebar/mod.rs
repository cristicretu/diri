//! Window-sidebar state, deterministic preview data, and GPUI rendering.

mod disclosure;
mod filter;
mod fixture;
mod state;
mod title_settle;
mod view;

pub use fixture::{PreviewScenario, SidebarPreviewFixture};
pub use state::{
    CursorMove, DragItem, DropZone, Popover, SidebarUiState, drop_zone, move_before, move_past,
    move_to_end,
};
pub(crate) use view::DraggedSidebarItem;
pub use view::Sidebar;
pub(crate) use view::SidebarEvent;
#[cfg(test)]
pub(crate) use view::title_clock_for_test;
