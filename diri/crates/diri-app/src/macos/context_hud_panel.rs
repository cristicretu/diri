//! AppKit side of the Capture Context HUD: where it appears, the panel
//! policy GPUI does not expose, and the Screen Recording guide.
//!
//! GPUI opens the HUD as a non-activating `NSPanel` (`WindowKind::PopUp`).
//! Made key here, it takes typing over the app in front without activating
//! Diri: no Diri window is raised, the browser stays where it was, and
//! closing the panel hands key status straight back.

use gpui::{Bounds, Pixels, Window, point, px, size};
use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{NSEvent, NSScreen, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// One level under the menu bar (`NSMainMenuWindowLevel` is 24): above every
/// app window, including full-screen ones joined as auxiliary.
const PANEL_LEVEL: isize = 23;

fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI's own NSView, alive as long as the Window.
    let view: &objc2_app_kit::NSView = unsafe { &*handle.ns_view.as_ptr().cast() };
    view.window()
}

/// Centred horizontally on the screen under the pointer, with its top where
/// a `max_height` panel would sit a third of the way down the visible area,
/// in GPUI's top-left global coordinates. The panel then grows and shrinks
/// downward from that top edge.
pub(crate) fn frame(width: f32, height: f32, max_height: f32) -> Option<Bounds<Pixels>> {
    let mtm = MainThreadMarker::new()?;
    let pointer = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    let primary = screens.iter().next()?;
    let screen = screens
        .iter()
        .find(|screen| {
            let f = screen.frame();
            pointer.x >= f.origin.x
                && pointer.x < f.origin.x + f.size.width
                && pointer.y >= f.origin.y
                && pointer.y < f.origin.y + f.size.height
        })
        .unwrap_or_else(|| primary.clone());
    let visible = screen.visibleFrame();
    let primary_height = primary.frame().size.height;
    let x = visible.origin.x + (visible.size.width - f64::from(width)) / 2.0;
    let top_appkit = visible.origin.y + visible.size.height
        - (visible.size.height - f64::from(max_height)) / 3.0;
    // AppKit's origin is bottom-left of the primary screen; GPUI's is top-left.
    let y = primary_height - top_appkit;
    Some(Bounds {
        origin: point(px(x as f32), px(y as f32)),
        size: size(px(width), px(height)),
    })
}

/// Turns GPUI's popup into a key-taking, non-activating HUD with rounded
/// corners. Runs once as the window is created.
pub(crate) fn prepare(window: &Window, radius: f32) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
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
    // SAFETY: AppKit property setters on the main thread; the panel is an
    // NSPanel subclass, the receiver of the key and hide policies.
    unsafe {
        let _: () = msg_send![&*ns_window, setHidesOnDeactivate: false];
        let _: () = msg_send![&*ns_window, setBecomesKeyOnlyIfNeeded: false];
        if let Some(content) = ns_window.contentView() {
            content.setWantsLayer(true);
            let layer: *mut AnyObject = msg_send![&*content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: f64::from(radius)];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
    }
    ns_window.makeKeyAndOrderFront(None);
}

/// Resizes the panel to `height`, keeping its top edge where it is. Deferred
/// to the next turn of the run loop: AppKit must not resize a window from
/// inside its own draw.
pub(crate) fn fit_height(window: &Window, height: f32, executor: &gpui::ForegroundExecutor) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    executor
        .spawn(async move {
            let mut rect = ns_window.frame();
            let top = rect.origin.y + rect.size.height;
            rect.size.height = f64::from(height);
            rect.origin.y = top - rect.size.height;
            ns_window.setFrame_display(rect, true);
        })
        .detach();
}

/// Screen Recording is off: macOS has already shown its own prompt once, so
/// open the Privacy page with the drag-to-grant guide over it.
pub(crate) fn guide_screen_recording() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(app) = std::env::current_exe().ok().and_then(|exe| {
        exe.ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .map(std::path::Path::to_path_buf)
    }) else {
        return;
    };
    let name = app
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Diri".to_owned());
    super::permission_guide::present(
        mtm,
        super::permission_guide::Pane::ScreenRecording,
        &app,
        &name,
        &crate::i18n::tf("context_hud.permission.screen_recording", &[("app", &name)]),
        Box::new(|| {}),
    );
}
