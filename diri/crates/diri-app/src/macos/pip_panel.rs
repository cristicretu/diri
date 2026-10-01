//! AppKit adjustments for the picture-in-picture panel.
//!
//! GPUI opens the panel as a non-activating `NSPanel` (`WindowKind::PopUp`)
//! and gives it a tracking area that reports the pointer while diri is in
//! the background. What it does not expose is the rest of what makes a PiP:
//! floating level instead of the popup level menus use, staying visible when
//! another app is active, following the user across Spaces and over full
//! screen apps, edge resizing, and moving the window from app code. All of
//! that lives here, in AppKit's bottom-left global coordinates, so the drag
//! and the corner snap never convert through a display-local space.

use gpui::{ForegroundExecutor, Window};
use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSEvent, NSFloatingWindowLevel, NSWindowButton, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::NSPoint;

use super::floating_panel::{blur_view, ns_window};
use crate::picture_in_picture::Rect;

/// Floats the panel, lets it resize from its edges, and masks it to `radius`
/// corners. Runs once, right after the window is created.
pub(crate) fn prepare(window: &Window, radius: f32) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    // Floating, not GPUI's popup level: a PiP sits above ordinary windows of
    // every app, but under menus, alerts, and the screen saver.
    ns_window.setLevel(NSFloatingWindowLevel);
    // The whole point is to stay on screen while another app is in front.
    ns_window.setHidesOnDeactivate(false);
    // Follow the user to every Space and over full-screen apps; keep out of
    // ⌘` cycling, since the panel never takes keyboard focus.
    ns_window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    ns_window.setStyleMask(ns_window.styleMask() | NSWindowStyleMask::Resizable);
    // A titled style mask brings AppKit's traffic lights with it; the panel
    // draws its own close and return controls on hover.
    for button in [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ] {
        if let Some(button) = ns_window.standardWindowButton(button) {
            button.setHidden(true);
        }
    }
    // SAFETY: plain AppKit property setters on the main thread; GPUI builds
    // PopUp windows from its NSPanel subclass, the only receiver of the key
    // policy.
    unsafe {
        // Clicks look at the terminal; they never take keyboard focus from
        // the app the user is working in.
        let _: () = msg_send![&*ns_window, setBecomesKeyOnlyIfNeeded: true];
        let _: () = msg_send![&*ns_window, setHasShadow: true];
        if let Some(content) = ns_window.contentView() {
            content.setWantsLayer(true);
            let layer: *mut AnyObject = msg_send![&*content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: f64::from(radius)];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
    }
    // As with menu panels, the blur shows before GPUI's first frame reaches
    // the screen; `floating_panel::reveal` shows it with that frame.
    if let Some(blur) = blur_view(&ns_window) {
        blur.setHidden(true);
    }
}

/// Where a drag started: the pointer and the window origin, both in AppKit
/// global coordinates, so every later position is a plain offset.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DragAnchor {
    mouse: NSPoint,
    origin: NSPoint,
}

pub(crate) fn begin_drag(window: &Window) -> Option<DragAnchor> {
    let ns_window = ns_window(window)?;
    Some(DragAnchor {
        mouse: NSEvent::mouseLocation(),
        origin: ns_window.frame().origin,
    })
}

/// Moves the panel so it keeps its offset from the pointer. Reads the pointer
/// from AppKit rather than from the event: the event is window-relative, and
/// the window is what is moving.
pub(crate) fn drag_to(window: &Window, anchor: DragAnchor, executor: &ForegroundExecutor) {
    let mouse = NSEvent::mouseLocation();
    set_origin(
        window,
        anchor.origin.x + mouse.x - anchor.mouse.x,
        anchor.origin.y + mouse.y - anchor.mouse.y,
        executor,
    );
}

/// The panel's frame and the visible area (screen minus menu bar and Dock)
/// of the screen it is mostly on, in AppKit's y-up global coordinates.
pub(crate) fn frame_and_visible(window: &Window) -> Option<(Rect, Rect)> {
    let ns_window = ns_window(window)?;
    let screen = ns_window.screen()?;
    let frame = ns_window.frame();
    let visible = screen.visibleFrame();
    let rect = |r: objc2_foundation::NSRect| Rect {
        x: r.origin.x,
        y: r.origin.y,
        width: r.size.width,
        height: r.size.height,
    };
    Some((rect(frame), rect(visible)))
}

/// Moves the panel's bottom-left corner to `(x, y)` in AppKit global
/// coordinates. Deferred like `floating_panel::set_frame`: AppKit answers a
/// frame change by calling back into GPUI, which needs the app the caller is
/// still holding.
pub(crate) fn set_origin(window: &Window, x: f64, y: f64, executor: &ForegroundExecutor) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    executor
        .spawn(async move {
            ns_window.setFrameOrigin(NSPoint::new(x, y));
        })
        .detach();
}
