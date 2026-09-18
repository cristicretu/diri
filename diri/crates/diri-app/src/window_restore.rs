//! Bringing windows back where they were.
//!
//! macOS keeps two ideas apart. A window's *frame* (size, position, and the
//! screen it sits on) is remembered by every well-behaved app, always. Window
//! *state* (full screen, the other windows that were open) comes back only
//! when System Settings › Desktop & Dock › "Close windows when quitting an
//! application" is off, which AppKit stores as the `NSQuitAlwaysKeepsWindows`
//! user default. This module holds that decision and the geometry that keeps
//! a restored frame on a screen the user can see.

use gpui::{Bounds, Pixels, WindowBounds, point, px, size};

use crate::store::{WindowMode, WindowPlacement};

/// What a launch brings back beyond the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RestorePolicy {
    /// Re-enter full screen (or a zoomed frame) when the app quit that way.
    pub presentation: bool,
    /// Reopen the other windows that were open at quit.
    pub extra_windows: bool,
}

impl RestorePolicy {
    /// The last frame and screen only: a fresh window, never full screen.
    pub const FRAME_ONLY: Self = Self {
        presentation: false,
        extra_windows: false,
    };

    /// The policy that follows the system "Close windows when quitting"
    /// setting; `keep_windows` is the raw `NSQuitAlwaysKeepsWindows` value.
    pub const fn for_system(keep_windows: bool) -> Self {
        Self {
            presentation: keep_windows,
            extra_windows: keep_windows,
        }
    }
}

/// One attached display, in the same coordinate space GPUI reports window
/// bounds in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayFrame {
    pub uuid: Option<String>,
    /// The area a window may occupy: the screen minus the menu bar and Dock.
    pub visible: Bounds<Pixels>,
    pub primary: bool,
}

/// Where a saved placement lands on the displays attached right now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RestoredWindow {
    pub bounds: WindowBounds,
    /// Index into the displays handed to [`resolve`], if any are attached.
    pub display: Option<usize>,
}

/// Map a saved placement onto the current displays. The saved display wins
/// when it is still attached; otherwise the primary display stands in. The
/// frame is then clamped into that display's visible area so the window is
/// never larger than the screen or reachable only by dragging it back.
pub(crate) fn resolve(
    placement: &WindowPlacement,
    displays: &[DisplayFrame],
    policy: RestorePolicy,
) -> RestoredWindow {
    let display = displays
        .iter()
        .position(|display| {
            placement.display_uuid.is_some() && display.uuid == placement.display_uuid
        })
        .or_else(|| displays.iter().position(|display| display.primary))
        .or_else(|| (!displays.is_empty()).then_some(0));
    let saved = Bounds::new(
        point(px(placement.x), px(placement.y)),
        size(px(placement.width), px(placement.height)),
    );
    let visible = display.map(|index| displays[index].visible);
    let frame = visible.map_or(saved, |visible| fit_within(saved, visible));
    let bounds = match placement.mode {
        WindowMode::Fullscreen if policy.presentation => WindowBounds::Fullscreen(frame),
        // AppKit has no zoomed *state* to restore, only the zoomed frame. Open
        // the window at that frame; GPUI's `zoom()` would toggle it back.
        WindowMode::Maximized if policy.presentation => {
            WindowBounds::Windowed(visible.unwrap_or(frame))
        }
        WindowMode::Windowed | WindowMode::Maximized | WindowMode::Fullscreen => {
            WindowBounds::Windowed(frame)
        }
    };
    RestoredWindow { bounds, display }
}

/// Shrink `saved` to fit inside `visible`, then slide it so every edge is on
/// screen. A frame that already fits is returned untouched.
pub(crate) fn fit_within(saved: Bounds<Pixels>, visible: Bounds<Pixels>) -> Bounds<Pixels> {
    let visible_width = f32::from(visible.size.width).max(0.0);
    let visible_height = f32::from(visible.size.height).max(0.0);
    let width = f32::from(saved.size.width).min(visible_width);
    let height = f32::from(saved.size.height).min(visible_height);
    let left = f32::from(visible.origin.x);
    let top = f32::from(visible.origin.y);
    let x = f32::from(saved.origin.x).clamp(left, left + visible_width - width);
    let y = f32::from(saved.origin.y).clamp(top, top + visible_height - height);
    Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
}

/// Whether the system asks apps to bring their windows back after a quit.
/// `DIRI_KEEP_WINDOWS=0|1` overrides the system setting for development.
pub(crate) fn system_keeps_windows_on_quit() -> bool {
    if let Some(value) = std::env::var_os("DIRI_KEEP_WINDOWS") {
        return value != "0";
    }
    platform_keeps_windows_on_quit()
}

#[cfg(target_os = "macos")]
fn platform_keeps_windows_on_quit() -> bool {
    use objc2_foundation::{NSUserDefaults, ns_string};

    // The global default behind System Settings › Desktop & Dock › "Close
    // windows when quitting an application". The key is absent (false) while
    // the box is checked, which is the macOS default.
    NSUserDefaults::standardUserDefaults().boolForKey(ns_string!("NSQuitAlwaysKeepsWindows"))
}

#[cfg(not(target_os = "macos"))]
fn platform_keeps_windows_on_quit() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    fn placement(
        mode: WindowMode,
        display: Option<&str>,
        frame: Bounds<Pixels>,
    ) -> WindowPlacement {
        WindowPlacement {
            display_uuid: display.map(str::to_owned),
            mode,
            x: f32::from(frame.origin.x),
            y: f32::from(frame.origin.y),
            width: f32::from(frame.size.width),
            height: f32::from(frame.size.height),
        }
    }

    fn displays() -> Vec<DisplayFrame> {
        vec![
            DisplayFrame {
                uuid: Some("builtin".to_owned()),
                // A 1512×982 laptop screen under a 38 pt menu bar and above a
                // 70 pt Dock.
                visible: bounds(0.0, 38.0, 1512.0, 874.0),
                primary: true,
            },
            DisplayFrame {
                uuid: Some("studio".to_owned()),
                visible: bounds(0.0, 38.0, 2560.0, 1402.0),
                primary: false,
            },
        ]
    }

    #[test]
    fn keeps_a_frame_that_fits() {
        let saved = bounds(120.0, 80.0, 1100.0, 700.0);
        assert_eq!(fit_within(saved, bounds(0.0, 38.0, 1512.0, 874.0)), saved);
    }

    #[test]
    fn slides_an_off_screen_frame_back_on_screen() {
        let visible = bounds(0.0, 38.0, 1512.0, 874.0);
        // Hanging off the bottom-right corner.
        assert_eq!(
            fit_within(bounds(1000.0, 600.0, 1100.0, 700.0), visible),
            bounds(412.0, 212.0, 1100.0, 700.0)
        );
        // Wholly above the menu bar and left of the screen.
        assert_eq!(
            fit_within(bounds(-2000.0, -900.0, 1100.0, 700.0), visible),
            bounds(0.0, 38.0, 1100.0, 700.0)
        );
    }

    #[test]
    fn shrinks_a_frame_larger_than_the_display() {
        let visible = bounds(0.0, 38.0, 1512.0, 874.0);
        assert_eq!(
            fit_within(bounds(100.0, 100.0, 2560.0, 1402.0), visible),
            visible
        );
        // Only the oversized axis shrinks.
        assert_eq!(
            fit_within(bounds(200.0, 100.0, 900.0, 2000.0), visible),
            bounds(200.0, 38.0, 900.0, 874.0)
        );
    }

    #[test]
    fn restores_onto_the_saved_display_when_it_is_attached() {
        let saved = bounds(300.0, 200.0, 1800.0, 1100.0);
        let restored = resolve(
            &placement(WindowMode::Windowed, Some("studio"), saved),
            &displays(),
            RestorePolicy::FRAME_ONLY,
        );
        assert_eq!(restored.display, Some(1));
        assert_eq!(restored.bounds, WindowBounds::Windowed(saved));
    }

    #[test]
    fn falls_back_to_the_primary_display_and_clamps_when_the_saved_display_is_gone() {
        let restored = resolve(
            &placement(
                WindowMode::Windowed,
                Some("unplugged"),
                bounds(300.0, 200.0, 1800.0, 1100.0),
            ),
            &displays(),
            RestorePolicy::FRAME_ONLY,
        );
        assert_eq!(restored.display, Some(0));
        assert_eq!(
            restored.bounds,
            WindowBounds::Windowed(bounds(0.0, 38.0, 1512.0, 874.0))
        );
    }

    #[test]
    fn a_placement_without_a_display_uses_the_primary() {
        let restored = resolve(
            &placement(
                WindowMode::Windowed,
                None,
                bounds(50.0, 60.0, 1100.0, 700.0),
            ),
            &displays(),
            RestorePolicy::for_system(true),
        );
        assert_eq!(restored.display, Some(0));
        assert_eq!(
            restored.bounds,
            WindowBounds::Windowed(bounds(50.0, 60.0, 1100.0, 700.0))
        );
    }

    #[test]
    fn without_displays_the_saved_frame_is_used_as_is() {
        let saved = bounds(50.0, 60.0, 1100.0, 700.0);
        let restored = resolve(
            &placement(WindowMode::Windowed, None, saved),
            &[],
            RestorePolicy::FRAME_ONLY,
        );
        assert_eq!(restored.display, None);
        assert_eq!(restored.bounds, WindowBounds::Windowed(saved));
    }

    #[test]
    fn full_screen_comes_back_only_when_the_system_keeps_windows() {
        let saved = bounds(120.0, 80.0, 1100.0, 700.0);
        let placement = placement(WindowMode::Fullscreen, Some("builtin"), saved);
        assert_eq!(
            resolve(&placement, &displays(), RestorePolicy::for_system(true)).bounds,
            WindowBounds::Fullscreen(saved),
            "the windowed frame travels with full screen so leaving it lands where it was"
        );
        assert_eq!(
            resolve(&placement, &displays(), RestorePolicy::for_system(false)).bounds,
            WindowBounds::Windowed(saved)
        );
        assert_eq!(
            resolve(&placement, &displays(), RestorePolicy::FRAME_ONLY).bounds,
            WindowBounds::Windowed(saved)
        );
    }

    #[test]
    fn a_zoomed_window_reopens_filling_the_display_without_toggling_zoom() {
        let placement = placement(
            WindowMode::Maximized,
            Some("builtin"),
            bounds(0.0, 38.0, 1512.0, 874.0),
        );
        assert_eq!(
            resolve(&placement, &displays(), RestorePolicy::for_system(true)).bounds,
            WindowBounds::Windowed(bounds(0.0, 38.0, 1512.0, 874.0))
        );
        assert_eq!(
            resolve(&placement, &displays(), RestorePolicy::for_system(false)).bounds,
            WindowBounds::Windowed(bounds(0.0, 38.0, 1512.0, 874.0))
        );
    }

    #[test]
    fn the_system_setting_decides_state_but_never_the_frame() {
        assert_eq!(
            RestorePolicy::for_system(true),
            RestorePolicy {
                presentation: true,
                extra_windows: true
            }
        );
        assert_eq!(RestorePolicy::for_system(false), RestorePolicy::FRAME_ONLY);
    }
}
