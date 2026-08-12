//! Window-sidebar state, deterministic preview data, and GPUI rendering.

mod fixture;
mod state;
mod view;

pub use fixture::{PreviewScenario, SidebarPreviewFixture};
pub use state::{CursorMove, DragItem, Popover, SidebarUiState, move_before, move_to_end};
pub use view::Sidebar;
pub(crate) use view::SidebarEvent;
