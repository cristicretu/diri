//! An optional observer of frame timing, for latency tracing.
//!
//! Diri installs one when `DIRI_LATENCY_TRACE=1` to follow a keystroke's echo
//! through the draw, the Metal commit and the compositor. With none
//! installed every probe is a single atomic load.

use std::sync::OnceLock;
use std::time::Instant;

/// A point in a window frame's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStage {
    /// `Window::draw` began building the frame's scene.
    DrawStart,
    /// `Window::draw` finished the scene.
    DrawEnd,
    /// The frame's command buffer was committed to the GPU.
    Committed,
    /// The GPU finished the frame's command buffer.
    GpuCompleted,
    /// The compositor put the frame's drawable on screen.
    Presented,
}

/// Receives a stage and when it happened. Called from the main thread for
/// draw stages and from Metal's callback threads for GPU and present stages.
pub type FrameObserver = fn(FrameStage, Instant);

static OBSERVER: OnceLock<FrameObserver> = OnceLock::new();

/// Installs the process-wide observer. Only the first call takes effect.
pub fn set_frame_observer(observer: FrameObserver) {
    let _ = OBSERVER.set(observer);
}

/// The installed observer, if any.
#[inline]
pub fn frame_observer() -> Option<FrameObserver> {
    OBSERVER.get().copied()
}
