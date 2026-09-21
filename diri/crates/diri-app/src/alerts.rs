//! Confirmation alerts.
//!
//! In the running app on macOS a confirmation is the system's own alert
//! sheet (`NSAlert`, via `Window::prompt`): the app icon, the bold message
//! and informative text, the default button on Return and Cancel on Escape,
//! all drawn and animated by AppKit. Tests, previews, and other platforms
//! keep the in-window dialog so the flow stays inspectable.

use gpui::{App, Global};

struct NativeAlerts;

impl Global for NativeAlerts {}

// The running app turns this on only on macOS; tests turn it on everywhere to
// drive the alert path through the test platform's prompt. Elsewhere the
// release build never calls it, and it stays compiled so the marker type it
// installs is not reported as never constructed in its place.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn enable(cx: &mut App) {
    cx.set_global(NativeAlerts);
}

pub(crate) fn enabled(cx: &App) -> bool {
    cx.has_global::<NativeAlerts>()
}
