//! Trackpad haptics: a small vocabulary for thresholds the hand crosses.
//!
//! A tick marks one discrete, meaningful boundary the user is physically
//! crossing in a drag or gesture: the thing they hold snapped somewhere, it
//! arrived at a limit, a drop was taken. It is never for hover, clicks, or
//! anything the app does on its own, and so [`perform`] is only ever called
//! from pointer and gesture event handlers, never from timers or tasks.
//!
//! AppKit already honours the system haptic preference and does nothing on
//! hardware without a haptic trackpad, so there is no setting and no
//! hardware detection here.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use gpui::{Pixels, Point};

/// Why a tick is being asked for. Call sites name the moment; which AppKit
/// pattern that means is decided once, in [`Haptic::pattern`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Haptic {
    /// What is being dragged snapped to a target: a new slot, a pane that
    /// will take the drop.
    Snap,
    /// What is being dragged arrived at the end of its travel.
    Limit,
    /// A gesture crossed from one discrete level into another.
    LevelChange,
    /// A drop was accepted.
    Accepted,
}

/// AppKit's three feedback patterns, by what Apple documents them for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pattern {
    /// Alignment with a target or guide, and reaching a minimum or maximum.
    Alignment,
    /// Moving between discrete levels.
    LevelChange,
    /// Everything else.
    Generic,
}

impl Haptic {
    pub(crate) const fn pattern(self) -> Pattern {
        match self {
            Self::Snap | Self::Limit => Pattern::Alignment,
            Self::LevelChange => Pattern::LevelChange,
            Self::Accepted => Pattern::Generic,
        }
    }
}

/// Identifies the boundary a tick belongs to, so re-crossing the same one
/// can be told apart from reaching a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TargetKey(u64);

/// `site` namespaces `id`, so a pane and a tab with the same session id are
/// different targets.
pub(crate) fn key(site: &'static str, id: impl Hash) -> TargetKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    site.hash(&mut hasher);
    id.hash(&mut hasher);
    TargetKey(hasher.finish())
}

/// How long a boundary stays quiet after it ticked. Long enough that a hand
/// trembling on a boundary feels one tick, short enough that deliberately
/// going back is answered.
pub(crate) const REPEAT_WINDOW: Duration = Duration::from_millis(100);

/// Drops repeats of the same tick inside [`REPEAT_WINDOW`]. A pure function
/// of what was asked and when; it never reads a clock.
#[derive(Default)]
pub(crate) struct Limiter {
    recent: Vec<(Haptic, TargetKey, Instant)>,
}

impl Limiter {
    /// A refused repeat restarts the window, so a pointer oscillating across
    /// a boundary stays silent until it stops, instead of ticking at the
    /// window's own rhythm. Different targets never suppress each other.
    pub(crate) fn admit(&mut self, haptic: Haptic, target: TargetKey, now: Instant) -> bool {
        self.recent
            .retain(|(_, _, at)| now.saturating_duration_since(*at) < REPEAT_WINDOW);
        if let Some(entry) = self
            .recent
            .iter_mut()
            .find(|(h, t, _)| *h == haptic && *t == target)
        {
            entry.2 = now;
            return false;
        }
        self.recent.push((haptic, target, now));
        true
    }
}

/// Edge detector for what a drag is over. It answers "did the hand just
/// carry the pointer onto a new target": not while holding still on one,
/// and not when the target changed under a pointer that did not move.
/// macOS keeps delivering drag updates for a stationary pointer, and lists
/// reflow on their own, so a change of target alone is not the user's doing.
#[derive(Default)]
pub(crate) struct Crossing {
    current: Option<TargetKey>,
    pointer: Option<Point<Pixels>>,
}

impl Crossing {
    /// Records what the pointer is over now; `None` is "nothing that would
    /// tick". Returns the target to tick for, if this move entered one.
    #[must_use]
    pub(crate) fn moved_to(
        &mut self,
        target: Option<TargetKey>,
        pointer: Point<Pixels>,
    ) -> Option<TargetKey> {
        let moved = self.pointer != Some(pointer);
        let entered = target != self.current;
        self.pointer = Some(pointer);
        self.current = target;
        target.filter(|_| moved && entered)
    }

    pub(crate) fn is_over_target(&self) -> bool {
        self.current.is_some()
    }

    /// The gesture ended; the next one starts with nothing entered.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

thread_local! {
    // Pointer events only arrive on the main thread, which is also the only
    // thread AppKit's performer may be used from.
    static LIMITER: RefCell<Limiter> = RefCell::new(Limiter::default());
}

/// One tick for `haptic` at `target`, unless that same tick just fired.
pub(crate) fn perform(haptic: Haptic, target: TargetKey) {
    if LIMITER.with_borrow_mut(|limiter| limiter.admit(haptic, target, now())) {
        play(haptic, target);
    }
}

/// The trackpad in the app.
#[cfg(all(target_os = "macos", not(test)))]
fn play(haptic: Haptic, _: TargetKey) {
    crate::macos::perform_haptic(haptic.pattern());
}

/// Platforms without a haptic trackpad.
#[cfg(all(not(target_os = "macos"), not(test)))]
fn play(haptic: Haptic, _: TargetKey) {
    let _ = haptic.pattern();
}

/// Every test build records instead, so a test run never actuates the
/// hardware of the machine running it.
#[cfg(test)]
fn play(haptic: Haptic, target: TargetKey) {
    testing::record(haptic, target);
}

#[cfg(not(test))]
fn now() -> Instant {
    Instant::now()
}

#[cfg(test)]
fn now() -> Instant {
    testing::now()
}

#[cfg(test)]
pub(crate) mod testing {
    use super::{Haptic, TargetKey};
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    thread_local! {
        static PERFORMED: RefCell<Vec<(Haptic, TargetKey)>> = const { RefCell::new(Vec::new()) };
        static EPOCH: Instant = Instant::now();
        static ELAPSED: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    pub(super) fn record(haptic: Haptic, target: TargetKey) {
        PERFORMED.with_borrow_mut(|performed| performed.push((haptic, target)));
    }

    /// The test clock only moves when a test moves it, so the limiter's
    /// verdicts do not depend on how loaded the machine is.
    pub(super) fn now() -> Instant {
        EPOCH.with(|epoch| *epoch) + ELAPSED.get()
    }

    pub(crate) fn advance(by: Duration) {
        ELAPSED.set(ELAPSED.get() + by);
    }

    /// What this thread's trackpad would have been asked to play since the
    /// last call, in order.
    pub(crate) fn take() -> Vec<(Haptic, TargetKey)> {
        PERFORMED.with_borrow_mut(std::mem::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    const STEP: Duration = Duration::from_millis(30);

    #[test]
    fn each_intent_maps_to_the_pattern_apple_documents_for_it() {
        assert_eq!(Haptic::Snap.pattern(), Pattern::Alignment);
        assert_eq!(Haptic::Limit.pattern(), Pattern::Alignment);
        assert_eq!(Haptic::LevelChange.pattern(), Pattern::LevelChange);
        assert_eq!(Haptic::Accepted.pattern(), Pattern::Generic);
    }

    #[test]
    fn rapid_recrossing_ticks_once_per_side_and_then_stays_quiet() {
        let mut limiter = Limiter::default();
        let (a, b) = (key("slot", 0), key("slot", 1));
        let start = Instant::now();
        let admitted: Vec<bool> = (0..8)
            .map(|step| {
                let target = if step % 2 == 0 { a } else { b };
                limiter.admit(Haptic::Snap, target, start + STEP * step)
            })
            .collect();
        assert_eq!(
            admitted,
            [true, true, false, false, false, false, false, false],
            "an oscillating pointer must not buzz, however long it oscillates"
        );
    }

    #[test]
    fn a_boundary_answers_again_once_the_hand_has_settled() {
        let mut limiter = Limiter::default();
        let a = key("slot", 0);
        let start = Instant::now();
        assert!(limiter.admit(Haptic::Snap, a, start));
        assert!(!limiter.admit(Haptic::Snap, a, start + REPEAT_WINDOW / 2));
        // The refused repeat restarted the window.
        assert!(!limiter.admit(Haptic::Snap, a, start + REPEAT_WINDOW));
        assert!(limiter.admit(Haptic::Snap, a, start + REPEAT_WINDOW + REPEAT_WINDOW));
    }

    #[test]
    fn separate_targets_and_separate_intents_do_not_suppress_each_other() {
        let mut limiter = Limiter::default();
        let now = Instant::now();
        assert!(limiter.admit(Haptic::Snap, key("slot", 0), now));
        assert!(limiter.admit(Haptic::Snap, key("slot", 1), now));
        assert!(limiter.admit(Haptic::Snap, key("pane", 0), now));
        // Entering a pane and releasing at once is two events.
        assert!(limiter.admit(Haptic::Accepted, key("pane", 0), now));
    }

    #[test]
    fn the_limiter_forgets_targets_it_no_longer_needs() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        for slot in 0..64_u32 {
            limiter.admit(
                Haptic::Snap,
                key("slot", slot),
                start + REPEAT_WINDOW * slot,
            );
        }
        assert!(limiter.recent.len() <= 1);
    }

    #[test]
    fn a_crossing_ticks_on_entry_and_not_while_the_pointer_stays() {
        let mut crossing = Crossing::default();
        let pane = key("pane", "a");
        assert_eq!(
            crossing.moved_to(Some(pane), point(px(10.0), px(10.0))),
            Some(pane)
        );
        assert_eq!(
            crossing.moved_to(Some(pane), point(px(11.0), px(10.0))),
            None,
            "moving inside a target is hover"
        );
        assert_eq!(
            crossing.moved_to(Some(pane), point(px(11.0), px(10.0))),
            None,
            "macOS repeats drag updates for a pointer that is holding still"
        );
    }

    #[test]
    fn a_crossing_ticks_again_only_after_the_pointer_has_left() {
        let mut crossing = Crossing::default();
        let (a, b) = (key("pane", "a"), key("pane", "b"));
        assert_eq!(crossing.moved_to(Some(a), point(px(1.0), px(0.0))), Some(a));
        assert_eq!(crossing.moved_to(None, point(px(2.0), px(0.0))), None);
        assert_eq!(crossing.moved_to(Some(a), point(px(3.0), px(0.0))), Some(a));
        assert_eq!(crossing.moved_to(Some(b), point(px(4.0), px(0.0))), Some(b));
    }

    #[test]
    fn a_target_that_changes_under_a_still_pointer_is_not_the_users_doing() {
        let mut crossing = Crossing::default();
        let (a, b) = (key("slot", 0), key("slot", 1));
        let pointer = point(px(5.0), px(5.0));
        assert_eq!(crossing.moved_to(Some(a), pointer), Some(a));
        assert_eq!(crossing.moved_to(Some(b), pointer), None);
        // It is remembered all the same: moving on within it is not an entry.
        assert_eq!(crossing.moved_to(Some(b), point(px(6.0), px(5.0))), None);
    }

    #[test]
    fn perform_plays_through_the_recorder_and_the_limiter() {
        let pane = key("pane", "a");
        perform(Haptic::Snap, pane);
        perform(Haptic::Snap, pane);
        assert_eq!(testing::take(), [(Haptic::Snap, pane)]);
        testing::advance(REPEAT_WINDOW * 2);
        perform(Haptic::Snap, pane);
        assert_eq!(testing::take(), [(Haptic::Snap, pane)]);
    }
}
