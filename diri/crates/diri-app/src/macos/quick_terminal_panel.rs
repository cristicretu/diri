//! AppKit side of the quick terminal: which screen it drops onto, the panel
//! policy GPUI does not expose, and the slide itself.
//!
//! GPUI opens the window as a non-activating `NSPanel` (`WindowKind::PopUp`),
//! which can become key over another app without activating Diri: the app in
//! front stays in front, no Diri window is raised, and the Dock does not
//! change. Hiding orders the panel out, and key status falls back to
//! whichever window held it before.

use gpui::{ForegroundExecutor, Window};
use objc2::MainThreadMarker;
use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSScreen, NSWindowCollectionBehavior, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::macos::floating_panel::{blur_view, ns_window};
use crate::quick_terminal::{PanelRect, Slide};

/// One level under the menu bar (`NSMainMenuWindowLevel` is 24): above every
/// app window, and the slide passes beneath the menu bar instead of over it.
const PANEL_LEVEL: isize = 23;

fn rect(frame: PanelRect) -> NSRect {
    NSRect::new(
        NSPoint::new(frame.x, frame.y),
        NSSize::new(frame.width, frame.height),
    )
}

fn panel_rect(frame: NSRect) -> PanelRect {
    PanelRect {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    }
}

/// The screen the pointer is on: where the user is looking, including over a
/// full-screen app on another display. Answers its visible frame (below the
/// menu bar, beside the Dock) in AppKit coordinates and its display number.
pub(crate) fn active_screen() -> Option<(PanelRect, u32)> {
    let mtm = MainThreadMarker::new()?;
    let pointer = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    let screen = screens
        .iter()
        .find(|screen| {
            let frame = screen.frame();
            pointer.x >= frame.origin.x
                && pointer.x < frame.origin.x + frame.size.width
                && pointer.y >= frame.origin.y
                && pointer.y < frame.origin.y + frame.size.height
        })
        .or_else(|| NSScreen::mainScreen(mtm))?;
    let number = screen_number(&screen).unwrap_or(0);
    Some((panel_rect(screen.visibleFrame()), number))
}

fn screen_number(screen: &NSScreen) -> Option<u32> {
    let description = screen.deviceDescription();
    let key = NSString::from_str("NSScreenNumber");
    let value = description.objectForKey(&key)?;
    // SAFETY: AppKit documents NSScreenNumber as an NSNumber.
    let number: u32 = unsafe { msg_send![&*value, unsignedIntValue] };
    Some(number)
}

/// Turns GPUI's popup into the quick terminal panel. Runs once, as the
/// window is created and before it is ever ordered in.
pub(crate) fn prepare(window: &Window, radius: f32) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    // Borderless, so AppKit never constrains the frame to the screen: the
    // hidden frame sits just above the visible area. The panel keeps its
    // non-activating style, which is what lets it take keys from the app in
    // front without bringing Diri forward.
    let mut style = ns_window.styleMask();
    style.remove(NSWindowStyleMask::Titled);
    style.insert(NSWindowStyleMask::NonactivatingPanel);
    ns_window.setStyleMask(style);
    ns_window.setLevel(PANEL_LEVEL);
    ns_window.setCollectionBehavior(
        NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    ns_window.setMovable(false);
    ns_window.setHasShadow(true);
    // SAFETY: plain AppKit property setters on the main thread. The panel is
    // an NSPanel subclass, the only receiver of the key and hide policies.
    unsafe {
        let _: () = msg_send![&*ns_window, setHidesOnDeactivate: false];
        let _: () = msg_send![&*ns_window, setBecomesKeyOnlyIfNeeded: false];
        if let Some(content) = ns_window.contentView() {
            content.setWantsLayer(true);
            let layer: *mut AnyObject = msg_send![&*content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: f64::from(radius)];
                // kCALayerMinXMinYCorner | kCALayerMaxXMinYCorner: only the
                // bottom corners; the top edge meets the menu bar flush.
                let _: () = msg_send![layer, setMaskedCorners: 3usize];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
    }
    // The blur shows the moment the window does, before GPUI has painted;
    // `floating_panel::reveal` shows it with the first on-screen frame.
    if let Some(blur) = blur_view(&ns_window) {
        blur.setHidden(true);
    }
}

/// Runs a slide. Deferred to the foreground executor, as every frame change
/// here is: AppKit answers one by calling back into GPUI, which the caller
/// is still inside.
pub(crate) fn slide(window: &Window, slide: Slide, executor: &ForegroundExecutor) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    executor
        .spawn(async move {
            if let Some(from) = slide.order_in_from {
                ns_window.setFrame_display(rect(from), false);
                ns_window.makeKeyAndOrderFront(None);
            } else if slide.take_key {
                ns_window.makeKeyAndOrderFront(None);
            }
            if slide.duration <= 0.0 {
                ns_window.setFrame_display(rect(slide.to), true);
                return;
            }
            let timing = NSString::from_str(if slide.take_key { "easeOut" } else { "easeIn" });
            // SAFETY: NSAnimationContext grouping and the window's animator
            // proxy, on the main thread. CAMediaTimingFunction comes from
            // QuartzCore, which AppKit always links.
            unsafe {
                let context = objc2::class!(NSAnimationContext);
                let _: () = msg_send![context, beginGrouping];
                let current: *mut AnyObject = msg_send![context, currentContext];
                let _: () = msg_send![current, setDuration: slide.duration];
                let function: *mut AnyObject = msg_send![
                    objc2::class!(CAMediaTimingFunction),
                    functionWithName: &*timing
                ];
                let _: () = msg_send![current, setTimingFunction: function];
                let animator: *mut AnyObject = msg_send![&*ns_window, animator];
                let _: () = msg_send![animator, setFrame: rect(slide.to), display: true];
                let _: () = msg_send![context, endGrouping];
            }
        })
        .detach();
}
