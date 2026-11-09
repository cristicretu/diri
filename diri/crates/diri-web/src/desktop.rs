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

pub trait Desktop: Send + Sync {
    /// The current layout. Called on every session-list read, so it must be
    /// cheap and must not block on the UI thread.
    fn sidebar(&self) -> SidebarLayout;

    /// Replaces the manual session and/or project order, exactly as a drag
    /// in the Mac sidebar would. `None` leaves that order alone. Refused
    /// while the Mac is not sorting by hand.
    fn set_sidebar_order(
        &self,
        sessions: Option<Vec<String>>,
        projects: Option<Vec<String>>,
    ) -> Result<(), String>;

    /// Ticks whenever [`Self::sidebar`] would answer differently, so a phone
    /// sees a reorder on the Mac without waiting for its next poll.
    fn sidebar_changes(&self) -> tokio::sync::watch::Receiver<u64>;
}
