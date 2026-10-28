//! Whether Diri animates. Every animation already asks `cx.reduce_motion()`;
//! this decides what it answers: reduced when System Settings › Accessibility
//! › Reduce motion is on, or when the person turned motion off in Diri
//! (Settings › Appearance), whichever says so.

use std::sync::atomic::{AtomicBool, Ordering};

/// The in-app preference, mirrored here so the system observer can combine
/// it without reaching for the store.
static PREFERENCE: AtomicBool = AtomicBool::new(false);
/// What macOS last reported.
static SYSTEM: AtomicBool = AtomicBool::new(false);

/// Combines the two sources into the one flag GPUI animates by.
pub(crate) const fn reduced(system: bool, preference: bool) -> bool {
    system || preference
}

fn apply(cx: &mut gpui::App) {
    cx.set_reduce_motion(reduced(
        SYSTEM.load(Ordering::Relaxed),
        PREFERENCE.load(Ordering::Relaxed),
    ));
}

/// Records the in-app preference and applies it at once.
pub(crate) fn set_preference(preference: bool, cx: &mut gpui::App) {
    PREFERENCE.store(preference, Ordering::Relaxed);
    apply(cx);
}

/// Records what macOS reports and applies it at once.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn set_system(system: bool, cx: &mut gpui::App) {
    SYSTEM.store(system, Ordering::Relaxed);
    apply(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_source_turns_motion_off() {
        assert!(!reduced(false, false));
        assert!(reduced(true, false), "the system setting still wins");
        assert!(reduced(false, true), "Diri's own setting works alone");
        assert!(reduced(true, true));
    }
}
