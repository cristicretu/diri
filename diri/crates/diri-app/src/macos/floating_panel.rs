//! AppKit adjustments for GPUI popup panels that host floating chrome.
//!
//! GPUI opens the panel and blurs its backdrop; what it does not expose is the
//! rounded mask that keeps the blur inside the menu's corners, the key-status
//! policy that lets the main window stay key while rows are clicked, and a
//! frame update that keeps the top-left corner in place.

use gpui::{ForegroundExecutor, Pixels, Point, Size, Window};
use objc2::MainThreadMarker;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2_app_kit::{NSScreen, NSView, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI hands out the pointer to its own NSView, which lives as
    // long as the `Window` it belongs to.
    let view: &NSView = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
    view.window()
}

/// Masks the panel to `radius` corners and keeps clicks on it from stealing
/// key status. Runs once, right after the window is created.
pub(crate) fn prepare(window: &Window, radius: f32) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    // SAFETY: plain AppKit property setters on the main thread; the panel is
    // an NSPanel subclass, which is the only receiver of the key policy.
    unsafe {
        let _: () = msg_send![&*ns_window, setBecomesKeyOnlyIfNeeded: true];
        let _: () = msg_send![&*ns_window, setHasShadow: true];
        // A menu belongs to the moment: hide with the app, stay on this
        // Space, and keep out of Exposé and window cycling. GPUI's popup
        // default joins every Space, which is how a menu ended up floating
        // over another app after the window behind it stopped drawing.
        let _: () = msg_send![&*ns_window, setHidesOnDeactivate: true];
        let behavior = objc2_app_kit::NSWindowCollectionBehavior::Transient
            | objc2_app_kit::NSWindowCollectionBehavior::FullScreenAuxiliary
            | objc2_app_kit::NSWindowCollectionBehavior::IgnoresCycle;
        ns_window.setCollectionBehavior(behavior);
        // The blur is AppKit's own view and shows the moment the window
        // does, before GPUI has painted a single frame. Keep it hidden until
        // `reveal`; the window itself stays visible so its display link runs.
        if let Some(blur) = blur_view(&ns_window) {
            blur.setHidden(true);
        }
        if let Some(content) = ns_window.contentView() {
            content.setWantsLayer(true);
            let layer: *mut AnyObject = msg_send![&*content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: f64::from(radius)];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
    }
}

/// GPUI's `BlurredView`, the `NSVisualEffectView` it slips under its content.
fn blur_view(ns_window: &NSWindow) -> Option<Retained<NSView>> {
    let content = ns_window.contentView()?;
    let effect_class = objc2::class!(NSVisualEffectView);
    content
        .subviews()
        .iter()
        .find(|view| view.isKindOfClass(effect_class))
}

/// Shows the blur once the panel's content has been painted. No fade: a
/// menu that materialises in two steps reads as lag, not as motion.
pub(crate) fn reveal(window: &Window) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    if let Some(blur) = blur_view(&ns_window) {
        blur.setHidden(false);
    }
}

/// Whether AppKit considers the panel on screen yet. GPUI sizes a window's
/// Metal drawable from this same occlusion state, so frames painted before it
/// flips are drawn into a placeholder surface and never reach the screen.
pub(crate) fn is_on_screen(window: &Window) -> bool {
    let Some(ns_window) = ns_window(window) else {
        return true;
    };
    ns_window
        .occlusionState()
        .contains(objc2_app_kit::NSWindowOcclusionState::Visible)
}

/// Moves and resizes the panel so its top-left corner sits at `origin` in
/// GPUI's global coordinates: x from the screen's left edge, y from its top.
///
/// The AppKit call is deferred to the foreground executor, exactly as GPUI's
/// own `resize` is: AppKit answers a frame change by calling back into GPUI
/// synchronously, and that callback needs the app, which the caller of this
/// function is still holding. Resizing inline left the window taller than
/// the viewport GPUI laid out.
pub(crate) fn set_frame(
    window: &Window,
    origin: Point<Pixels>,
    size: Size<Pixels>,
    executor: &ForegroundExecutor,
) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    let screen = ns_window
        .screen()
        .or_else(|| NSScreen::mainScreen(MainThreadMarker::new()?));
    let Some(screen) = screen else {
        return;
    };
    let screen_frame = screen.frame();
    let width = f64::from(f32::from(size.width));
    let height = f64::from(f32::from(size.height));
    let x = screen_frame.origin.x + f64::from(f32::from(origin.x));
    let y =
        screen_frame.origin.y + screen_frame.size.height - f64::from(f32::from(origin.y)) - height;
    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
    executor
        .spawn(async move {
            ns_window.setFrame_display(rect, true);
        })
        .detach();
}
