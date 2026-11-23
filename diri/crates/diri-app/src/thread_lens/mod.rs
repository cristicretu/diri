//! Dual-Lens GUI: the GUI Lens (Conductor Mode) over the same live PTY
//! Holder the TUI Lens (Pilot Mode) paints. See `diri/DUAL_LENS.md`.

pub mod diff;
#[cfg(test)]
pub mod fixture;
pub mod model;
pub mod portal;
pub mod setup;
pub mod slash;
pub mod view;

pub use view::{ThreadLens, ThreadLensEvent, needs_portal};

/// Which lens a session's pane shows. Pure view state: flipping it touches
/// neither the thread nor the PTY (`DualLens.lens_flip_commutes`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Lens {
    /// The terminal itself (Pilot Mode).
    #[default]
    Tui,
    /// The structured thread (Conductor Mode).
    Gui,
}

impl Lens {
    pub fn flipped(self) -> Self {
        match self {
            Self::Tui => Self::Gui,
            Self::Gui => Self::Tui,
        }
    }
}
