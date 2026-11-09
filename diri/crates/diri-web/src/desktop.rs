//! What the Mac app shares with the phone beyond the Engine.
//!
//! The sidebar's manual order and pins live in the app's preferences, not in
//! the Engine, so a phone that only read the Engine would order rows its own
//! way. The app hands the embedded gateway a [`Desktop`] so the phone mirrors
//! the Mac's order and can change it. The standalone `diri-web` binary has no
//! app behind it and serves without one.

use serde::{Deserialize, Serialize};

/// The Mac sidebar's order, as it is drawn there.
///
/// These are the orders the Mac computed, not its raw preferences: the Mac
/// can also sort by activity, and a phone re-deriving that would drift the
/// moment the two disagreed about a timestamp.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarLayout {
    /// Projects, top to bottom.
    pub project_order: Vec<String>,
    /// Every session, project by project in tree order, folded or not.
    pub session_order: Vec<String>,
    pub pinned_sessions: Vec<String>,
    pub pinned_projects: Vec<String>,
    /// Whether the Mac sorts by hand. When it sorts by activity a drag means
    /// nothing there, so the phone does not offer one either.
    pub reorderable: bool,
}

/// Which kind of sidebar row a move names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    Session,
    Project,
}

/// Where a moved row lands, relative to a row the phone could see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarPlace {
    Before(String),
    After(String),
    /// After every other row.
    End,
}

/// One row moved, as one drag in the Mac sidebar moves it.
///
/// A move, not a whole order: the Mac applies it to its own saved order, so
/// rows the phone did not touch keep wherever the Mac last put them, and the
/// order the phone was shown (pinned rows first, rows it never sees left
/// out) is never written back as if it were the saved one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarMove {
    pub row: SidebarRow,
    pub id: String,
    pub place: SidebarPlace,
}

/// Why the Mac did not apply a move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRefusal {
    /// The Mac's sidebar no longer allows it (it sorts by activity now, or a
    /// named row is gone). The phone's layout is stale: refresh it.
    Conflict(String),
    /// The move was valid but could not be saved.
    Failed(String),
}

pub trait Desktop: Send + Sync {
    /// The current layout. Called on every session-list read from async
    /// tasks, so it must answer from a cache and never wait on the UI.
    fn sidebar(&self) -> SidebarLayout;

    /// Applies one move to the manual order, exactly as a drag in the Mac
    /// sidebar would. Refused while the Mac is not sorting by hand. May
    /// block briefly on the app's store; call it off the async workers.
    fn move_sidebar_row(&self, row: SidebarMove) -> Result<(), SidebarRefusal>;

    /// Ticks whenever [`Self::sidebar`] would answer differently, so a phone
    /// sees a reorder on the Mac without waiting for its next poll.
    fn sidebar_changes(&self) -> tokio::sync::watch::Receiver<u64>;
}
