//! DIRI PATCH (input timing): when the platform says the input event being
//! dispatched happened, and when the frame being drawn will reach the display.
//!
//! Display-paced scrolling samples a gesture at each frame's display time, so
//! it needs both: the hardware time of every wheel event (the main thread
//! handles them several milliseconds late, and unevenly) and the refresh the
//! frame is drawn for (a frame starts anywhere in the refresh before it).
//!
//! They travel beside the events rather than in them because the platform
//! crates that are not vendored here build `ScrollWheelEvent` and
//! `RequestFrameOptions` by struct literal; a new field would not compile on
//! Linux or Windows. Both values are set by the platform around one dispatch
//! on the main thread and read back during it; elsewhere they are `None`.

use std::cell::Cell;
use std::time::Instant;

use crate::TouchPhase;

/// Timing the platform attached to the input event being dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformEventTiming {
    /// When the hardware produced the event, on the `Instant` clock.
    pub timestamp: Instant,
    /// For a scroll-wheel event the system generates after the fingers lift
    /// (inertia), where in that momentum it is; `None` for direct input.
    pub momentum_phase: Option<TouchPhase>,
}

thread_local! {
    static EVENT_TIMING: Cell<Option<PlatformEventTiming>> = const { Cell::new(None) };
    static FRAME_TARGET: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Restores the previous value when the dispatch returns or unwinds.
struct Restore<T: Copy + 'static> {
    key: &'static std::thread::LocalKey<Cell<Option<T>>>,
    previous: Option<T>,
}

impl<T: Copy + 'static> Drop for Restore<T> {
    fn drop(&mut self) {
        self.key.with(|cell| cell.set(self.previous));
    }
}

fn scoped<T: Copy + 'static, R>(
    key: &'static std::thread::LocalKey<Cell<Option<T>>>,
    value: Option<T>,
    run: impl FnOnce() -> R,
) -> R {
    let previous = key.with(|cell| cell.replace(value));
    let _restore = Restore { key, previous };
    run()
}

/// Runs `dispatch` with `timing` readable through [`platform_event_timing`].
/// Called by the platform around the dispatch of one input event.
pub fn with_platform_event_timing<R>(
    timing: Option<PlatformEventTiming>,
    dispatch: impl FnOnce() -> R,
) -> R {
    scoped(&EVENT_TIMING, timing, dispatch)
}

/// Timing of the input event being dispatched on this thread, when the
/// platform provides it (macOS does).
#[must_use]
pub fn platform_event_timing() -> Option<PlatformEventTiming> {
    EVENT_TIMING.with(Cell::get)
}

/// Runs `frame` with `target` readable through [`frame_target_time`].
/// Called by the platform around one frame request.
pub fn with_frame_target_time<R>(target: Option<Instant>, frame: impl FnOnce() -> R) -> R {
    scoped(&FRAME_TARGET, target, frame)
}

/// When the frame being drawn on this thread is expected on the display:
/// the refresh the platform's display link is pacing it for. `None` outside
/// a frame or on platforms that do not report it.
#[must_use]
pub fn frame_target_time() -> Option<Instant> {
    FRAME_TARGET.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_scoped_to_one_dispatch_and_restored() {
        assert_eq!(frame_target_time(), None);
        let at = Instant::now();
        let seen = with_frame_target_time(Some(at), || {
            let inner = with_frame_target_time(None, frame_target_time);
            (frame_target_time(), inner)
        });
        assert_eq!(seen, (Some(at), None));
        assert_eq!(frame_target_time(), None);

        let timing = PlatformEventTiming {
            timestamp: at,
            momentum_phase: Some(TouchPhase::Moved),
        };
        let result = std::panic::catch_unwind(|| {
            with_platform_event_timing(Some(timing), || {
                assert_eq!(platform_event_timing(), Some(timing));
                panic!("unwinds");
            })
        });
        assert!(result.is_err());
        assert_eq!(platform_event_timing(), None, "restored on unwind");
    }
}
