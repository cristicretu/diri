pub mod brand_raster;
pub mod browser;
pub(crate) mod floating_panel;
pub mod menu_bar;
pub mod notifier;
pub(crate) mod terminal_keys;

use objc2_foundation::NSBundle;

pub(crate) fn bundle_identifier() -> Option<String> {
    NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|identifier| identifier.to_string())
}

#[cfg(test)]
pub(crate) mod tab_gesture;

/// AppKit respects the current trackpad and the user's haptic preferences.
pub(crate) fn pinch_feedback() {
    use objc2_app_kit::{
        NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
        NSHapticFeedbackPerformer,
    };
    NSHapticFeedbackManager::defaultPerformer().performFeedbackPattern_performanceTime(
        NSHapticFeedbackPattern::Alignment,
        NSHapticFeedbackPerformanceTime::Now,
    );
}
