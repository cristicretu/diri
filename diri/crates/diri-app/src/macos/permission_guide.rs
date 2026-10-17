//! The "drag the app into the list" guide for macOS privacy permissions.
//!
//! macOS has no API for granting another app Accessibility or Screen
//! Recording; a person has to add it in System Settings. Finding the "+"
//! button and then the app in a file picker is where people give up. So we
//! open the right Privacy & Security page and float a small panel over the
//! bottom of the Settings window, holding the app as a draggable row: drop it
//! onto the list above and macOS adds it, switched on.
//!
//! The panel never activates and never becomes key, so System Settings keeps
//! focus throughout. It follows the Settings window and hides whenever
//! another app comes to the front.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAppearanceCustomization, NSBackingStoreType, NSButton, NSColor, NSDragOperation,
    NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSFont,
    NSFontWeightBold, NSFontWeightMedium, NSFontWeightSemibold, NSImage, NSImageScaling,
    NSImageSymbolConfiguration, NSImageView, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField,
    NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSNotification, NSNumber, NSOperationQueue, NSPoint, NSRect, NSSize,
    NSString, NSTimer, NSURL,
};

/// The Privacy & Security page the guide opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Accessibility,
    ScreenRecording,
}

impl Pane {
    pub fn settings_url(self) -> &'static str {
        match self {
            Pane::Accessibility => {
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility"
            }
            Pane::ScreenRecording => {
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ScreenCapture"
            }
        }
    }
}

const SETTINGS_BUNDLE_ID: &str = "com.apple.systempreferences";
const PANEL_WIDTH: f64 = 460.0;
const PANEL_HEIGHT: f64 = 108.0;
/// Settings' sidebar (macOS 26/27): the panel centres on the page beside it.
const SETTINGS_SIDEBAR: f64 = 215.0;
/// How far above the bottom of the Settings window the panel sits.
const BOTTOM_INSET: f64 = 14.0;
/// Room kept from the edges of the screen.
const SCREEN_MARGIN: f64 = 8.0;
const TRACK_INTERVAL: f64 = 0.15;

/// A rectangle in points, as plain numbers so geometry can be tested.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn area(self) -> f64 {
        self.width * self.height
    }

    fn intersection_area(self, other: Rect) -> f64 {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        if width <= 0.0 || height <= 0.0 {
            0.0
        } else {
            width * height
        }
    }

    fn from_ns(rect: NSRect) -> Self {
        Self {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        }
    }
}

/// Window Server rectangles have their origin at the top left of the primary
/// display, y growing down; AppKit's at the bottom left, y growing up.
pub fn cg_to_appkit(cg: Rect, primary_height: f64) -> Rect {
    Rect {
        y: primary_height - cg.y - cg.height,
        ..cg
    }
}

/// The main Settings window among a process's on-screen windows: a normal
/// (layer 0) window big enough to be the window rather than a sheet or a
/// popover, the largest if there are several.
pub fn pick_settings_window(windows: &[(i64, Rect)]) -> Option<Rect> {
    windows
        .iter()
        .filter(|(layer, rect)| *layer == 0 && rect.width > 320.0 && rect.height > 240.0)
        .map(|(_, rect)| *rect)
        .max_by(|a, b| a.area().total_cmp(&b.area()))
}

/// Where the panel's bottom-left corner goes: centred on the Settings page
/// (right of its sidebar), just above the window's bottom edge, and kept
/// inside the visible part of the screen.
pub fn anchored_origin(settings: Rect, visible: Rect, panel: (f64, f64)) -> (f64, f64) {
    let (width, height) = panel;
    let content_x = settings.x + SETTINGS_SIDEBAR;
    let content_width = (settings.width - SETTINGS_SIDEBAR).max(width);
    let x = content_x + (content_width - width) / 2.0;
    let y = settings.y + BOTTOM_INSET;
    let min_x = visible.x + SCREEN_MARGIN;
    let max_x = (visible.x + visible.width - width - SCREEN_MARGIN).max(min_x);
    let min_y = visible.y + SCREEN_MARGIN;
    let max_y = (visible.y + visible.height - height - SCREEN_MARGIN).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// What a drag carries: the app bundle as a file URL, which is what the
/// Privacy lists accept from Finder.
pub fn drag_payload(app_bundle: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&app_bundle.to_string_lossy()), true)
}

struct Guide {
    panel: Retained<NSPanel>,
    timer: Retained<NSTimer>,
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    // Held so the close button's target and the drag source outlive clicks.
    _target: Retained<GuideTarget>,
    _row: Retained<DragRow>,
    on_close: Rc<dyn Fn()>,
}

thread_local! {
    static GUIDE: RefCell<Option<Guide>> = const { RefCell::new(None) };
}

/// Opens `pane` in System Settings and shows the guide over it, replacing any
/// guide already showing. `title` is the sentence above the row; `on_close`
/// runs when the person closes the panel (not on `dismiss`).
pub fn present(
    mtm: MainThreadMarker,
    pane: Pane,
    app_bundle: &Path,
    app_name: &str,
    title: &str,
    on_close: Box<dyn Fn()>,
) {
    dismiss();

    let target = GuideTarget::new(mtm);
    let row = DragRow::new(mtm, app_bundle, app_name);
    let panel = build_panel(mtm, &target, &row, title);

    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(
            TRACK_INTERVAL,
            true,
            &block2::RcBlock::new(|_: std::ptr::NonNull<NSTimer>| refresh()),
        )
    };
    let handler = block2::RcBlock::new(|_: std::ptr::NonNull<NSNotification>| refresh());
    // SAFETY: the block captures nothing and runs on the main queue.
    let observer = unsafe {
        NSWorkspace::sharedWorkspace()
            .notificationCenter()
            .addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &handler,
            )
    };

    GUIDE.with(|guide| {
        *guide.borrow_mut() = Some(Guide {
            panel,
            timer,
            observer,
            _target: target,
            _row: row,
            on_close: Rc::from(on_close),
        });
    });

    if let Some(url) = NSURL::URLWithString(&NSString::from_str(pane.settings_url())) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
    refresh();
}

/// Closes the guide, if one is showing. Does not run `on_close`.
pub fn dismiss() {
    let Some(guide) = GUIDE.with(|guide| guide.borrow_mut().take()) else {
        return;
    };
    guide.timer.invalidate();
    // SAFETY: the observer came from this notification center.
    unsafe {
        NSWorkspace::sharedWorkspace()
            .notificationCenter()
            .removeObserver(AsRef::<AnyObject>::as_ref(&*guide.observer));
    }
    guide.panel.orderOut(None);
    guide.panel.close();
}

pub fn is_presented() -> bool {
    GUIDE.with(|guide| guide.borrow().is_some())
}

/// The person closed the panel: tear it down, then tell the caller.
fn closed_by_person() {
    let on_close = GUIDE.with(|guide| guide.borrow().as_ref().map(|g| g.on_close.clone()));
    dismiss();
    if let Some(on_close) = on_close {
        on_close();
    }
}

/// Follows the Settings window: shown over it while Settings is in front,
/// hidden otherwise.
fn refresh() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let panel = GUIDE.with(|guide| guide.borrow().as_ref().map(|g| g.panel.clone()));
    let Some(panel) = panel else {
        return;
    };
    match settings_placement(mtm) {
        Some((settings, visible)) => {
            let (x, y) = anchored_origin(settings, visible, (PANEL_WIDTH, PANEL_HEIGHT));
            panel.setFrameOrigin(NSPoint::new(x, y));
            if !panel.isVisible() {
                panel.orderFrontRegardless();
            }
        }
        None => {
            if panel.isVisible() {
                panel.orderOut(None);
            }
        }
    }
}

/// The frontmost Settings window and the visible frame of its screen, in
/// AppKit coordinates; `None` unless System Settings is the front app.
fn settings_placement(mtm: MainThreadMarker) -> Option<(Rect, Rect)> {
    let front = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    if front.bundleIdentifier()?.to_string() != SETTINGS_BUNDLE_ID {
        return None;
    }
    let screens = NSScreen::screens(mtm);
    let primary = screens.firstObject()?;
    let primary_height = primary.frame().size.height;
    let screen_rects: Vec<(Rect, Rect)> = screens
        .iter()
        .map(|screen| {
            (
                Rect::from_ns(screen.frame()),
                Rect::from_ns(screen.visibleFrame()),
            )
        })
        .collect();

    // SAFETY: a pid_t property getter (the typed one needs the libc feature).
    let pid: i32 = unsafe { msg_send![&*front, processIdentifier] };
    let settings = settings_windows(pid)
        .and_then(|windows| pick_settings_window(&windows))
        .map(|cg| cg_to_appkit(cg, primary_height));
    let Some(settings) = settings else {
        // Settings is in front but its window isn't listed yet (it is still
        // opening): sit near the bottom of the main screen meanwhile.
        let visible = Rect::from_ns(NSScreen::mainScreen(mtm)?.visibleFrame());
        let fallback = Rect {
            x: visible.x + 80.0,
            y: visible.y + 70.0,
            width: visible.width - 160.0,
            height: visible.height - 140.0,
        };
        return Some((fallback, visible));
    };
    let visible = screen_rects
        .iter()
        .max_by(|a, b| {
            a.0.intersection_area(settings)
                .total_cmp(&b.0.intersection_area(settings))
        })
        .map(|(_, visible)| *visible)?;
    Some((settings, visible))
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *mut AnyObject;
}

const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

/// Layer and Window Server bounds of each on-screen window `pid` owns. Owner
/// and bounds need no Screen Recording grant; only titles would.
fn settings_windows(pid: i32) -> Option<Vec<(i64, Rect)>> {
    // SAFETY: a Copy-rule CFArrayRef of CFDictionaryRefs, toll-free bridged to
    // NSArray<NSDictionary>; `from_raw` takes over the +1 reference.
    let list: Retained<NSArray<NSDictionary<NSString, AnyObject>>> = unsafe {
        let raw = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, 0);
        Retained::from_raw(raw.cast())?
    };
    let number = |info: &NSDictionary<NSString, AnyObject>, key: &str| -> Option<f64> {
        let value = info.objectForKey(&NSString::from_str(key))?;
        Some(value.downcast_ref::<NSNumber>()?.doubleValue())
    };
    let mut windows = Vec::new();
    for info in list.iter() {
        if number(&info, "kCGWindowOwnerPID")? as i32 != pid {
            continue;
        }
        let layer = number(&info, "kCGWindowLayer").unwrap_or(0.0) as i64;
        let Some(bounds) = info.objectForKey(&NSString::from_str("kCGWindowBounds")) else {
            continue;
        };
        let Some(bounds) = bounds.downcast_ref::<NSDictionary>() else {
            continue;
        };
        // SAFETY: the bounds dictionary maps CFString keys to CFNumbers.
        let bounds: &NSDictionary<NSString, AnyObject> = unsafe { bounds.cast_unchecked() };
        let field = |key: &str| number(bounds, key).unwrap_or(0.0);
        windows.push((
            layer,
            Rect {
                x: field("X"),
                y: field("Y"),
                width: field("Width"),
                height: field("Height"),
            },
        ));
    }
    Some(windows)
}

fn build_panel(
    mtm: MainThreadMarker,
    target: &GuideTarget,
    row: &DragRow,
    title: &str,
) -> Retained<NSPanel> {
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
    );
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: plain AppKit setters on a window this thread owns.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel.setLevel(NSStatusWindowLevel);
    panel.setHidesOnDeactivate(false);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    let material = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
    material.setMaterial(NSVisualEffectMaterial::Popover);
    material.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    material.setState(NSVisualEffectState::Active);
    material.setWantsLayer(true);
    // SAFETY: the effect view is layer-backed, so `layer` is a CALayer.
    unsafe {
        let layer: *mut AnyObject = msg_send![&*material, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: 16.0_f64];
            let _: () = msg_send![layer, setMasksToBounds: true];
            let _: () = msg_send![layer, setBorderWidth: 0.5_f64];
        }
    }

    // Top line: an arrow pointing at the list, and what to do.
    let arrow = symbol_view(mtm, "arrow.up", 18.0, unsafe { NSFontWeightBold });
    arrow.setContentTintColor(Some(&NSColor::systemBlueColor()));
    arrow.setFrame(NSRect::new(
        NSPoint::new(18.0, 70.0),
        NSSize::new(24.0, 24.0),
    ));
    material.addSubview(&arrow);

    let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe {
        NSFontWeightMedium
    })));
    label.setTextColor(Some(&NSColor::labelColor()));
    label.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
    label.setFrame(NSRect::new(
        NSPoint::new(48.0, 72.0),
        NSSize::new(PANEL_WIDTH - 96.0, 20.0),
    ));
    material.addSubview(&label);

    let close = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(PANEL_WIDTH - 34.0, 72.0),
            NSSize::new(20.0, 20.0),
        ),
    );
    close.setBordered(false);
    if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str("xmark.circle.fill"),
        Some(&NSString::from_str("Close")),
    ) {
        close.setImage(Some(&image));
    }
    close.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    close.setContentTintColor(Some(&NSColor::tertiaryLabelColor()));
    // SAFETY: `target` outlives the button: the guide holds both.
    unsafe {
        close.setTarget(Some(target));
        close.setAction(Some(sel!(closeGuide:)));
    }
    material.addSubview(&close);

    row.setFrame(NSRect::new(
        NSPoint::new(16.0, 14.0),
        NSSize::new(PANEL_WIDTH - 32.0, 46.0),
    ));
    material.addSubview(row);
    row.apply_colors();

    panel.setContentView(Some(&material));
    panel
}

/// Draws the guide's panel, without showing it, into a PNG at `path`, in the
/// light or dark appearance. For screenshots: the material renders as its
/// flat fallback, since nothing is behind an offscreen window. Only the
/// `permission_guide_appkit` demo calls it.
#[allow(dead_code)]
pub fn render_png(
    mtm: MainThreadMarker,
    app_bundle: &Path,
    app_name: &str,
    title: &str,
    dark: bool,
    path: &Path,
) -> bool {
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSBitmapImageFileType,
    };
    let target = GuideTarget::new(mtm);
    let row = DragRow::new(mtm, app_bundle, app_name);
    let panel = build_panel(mtm, &target, &row, title);
    let name = unsafe {
        if dark {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    panel.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
    row.apply_colors();
    let Some(view) = panel.contentView() else {
        return false;
    };
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else {
        return false;
    };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // SAFETY: an empty properties dictionary is valid for PNG.
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    };
    panel.close();
    data.is_some_and(|data| std::fs::write(path, data.to_vec()).is_ok())
}

/// Core Graphics' colour, opaque here: only handed from NSColor to CALayer.
#[repr(C)]
struct CGColor {
    _private: [u8; 0],
}

// SAFETY: matches the `^{CGColor=}` AppKit declares for `-[NSColor CGColor]`.
unsafe impl objc2::encode::RefEncode for CGColor {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("CGColor", &[]));
}

/// The colour's CGColor, valid while `color` lives (the caller keeps it).
fn cg_color(color: &NSColor) -> *mut CGColor {
    // SAFETY: `CGColor` is a plain getter on every NSColor.
    unsafe { msg_send![color, CGColor] }
}

fn symbol_view(
    mtm: MainThreadMarker,
    name: &str,
    size: f64,
    weight: objc2_app_kit::NSFontWeight,
) -> Retained<NSImageView> {
    let view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size)),
    );
    if let Some(image) =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
    {
        view.setImage(Some(&image));
    }
    view.setSymbolConfiguration(Some(
        &NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight),
    ));
    view
}

define_class!(
    /// Receives the close button.
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriPermissionGuideTarget"]
    struct GuideTarget;

    unsafe impl NSObjectProtocol for GuideTarget {}

    impl GuideTarget {
        #[unsafe(method(closeGuide:))]
        fn close_guide(&self, _sender: Option<&AnyObject>) {
            // After this click returns: tearing the panel down inside its
            // own button's action would free the button mid-call.
            let block = block2::RcBlock::new(closed_by_person);
            // SAFETY: the block captures nothing and runs on the main queue.
            unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
        }
    }
);

impl GuideTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

struct DragRowIvars {
    bundle: std::path::PathBuf,
}

define_class!(
    /// The app as a draggable row: icon and name in a rounded tile. A press
    /// anywhere on it starts a file drag of the app bundle.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriPermissionGuideDragRow"]
    #[ivars = DragRowIvars]
    struct DragRow;

    unsafe impl NSObjectProtocol for DragRow {}

    impl DragRow {
        // The panel never becomes key; the first click must still count.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // The icon and label inside must not swallow the press.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let frame = self.frame();
            let inside = point.x >= frame.origin.x
                && point.x <= frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y <= frame.origin.y + frame.size.height;
            inside.then(|| Retained::from(self as &NSView))
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.begin_drag(event);
        }

        // Layer colours are resolved once; light/dark changes re-resolve them.
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn appearance_changed(&self) {
            self.apply_colors();
        }
    }

    unsafe impl NSDraggingSource for DragRow {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn will_begin(&self, _session: &NSDraggingSession, _point: NSPoint) {
            self.setAlphaValue(0.0);
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, _operation: NSDragOperation) {
            self.setAlphaValue(1.0);
        }
    }
);

impl DragRow {
    fn new(mtm: MainThreadMarker, bundle: &Path, name: &str) -> Retained<Self> {
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(PANEL_WIDTH - 32.0, 46.0),
        );
        let this = mtm.alloc::<Self>().set_ivars(DragRowIvars {
            bundle: bundle.to_path_buf(),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setWantsLayer(true);
        // SAFETY: layer-backed just above.
        unsafe {
            let layer: *mut AnyObject = msg_send![&*this, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: 9.0_f64];
                let _: () = msg_send![layer, setBorderWidth: 0.5_f64];
            }
        }

        let icon = NSWorkspace::sharedWorkspace()
            .iconForFile(&NSString::from_str(&bundle.to_string_lossy()));
        let icon_view = NSImageView::imageViewWithImage(&icon, mtm);
        icon_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        icon_view.setFrame(NSRect::new(
            NSPoint::new(10.0, 7.0),
            NSSize::new(32.0, 32.0),
        ));
        this.addSubview(&icon_view);

        let label = NSTextField::labelWithString(&NSString::from_str(name), mtm);
        label.setFont(Some(&NSFont::systemFontOfSize_weight(14.0, unsafe {
            NSFontWeightSemibold
        })));
        label.setTextColor(Some(&NSColor::labelColor()));
        label.setFrame(NSRect::new(
            NSPoint::new(52.0, 13.0),
            NSSize::new(frame.size.width - 96.0, 20.0),
        ));
        this.addSubview(&label);

        // A grip on the right says "this moves".
        let grip = symbol_view(mtm, "line.3.horizontal", 14.0, unsafe {
            NSFontWeightMedium
        });
        grip.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        grip.setFrame(NSRect::new(
            NSPoint::new(frame.size.width - 32.0, 15.0),
            NSSize::new(16.0, 16.0),
        ));
        this.addSubview(&grip);
        this
    }

    /// Fill and hairlines for the row, and the panel's border, in the
    /// appearance the row is drawn in now.
    fn apply_colors(&self) {
        let row: &NSView = self;
        // SAFETY: read on the main thread; the panel body outlives the row.
        let panel_body = unsafe { self.superview() };
        let apply = || {
            let fill = NSColor::labelColor().colorWithAlphaComponent(0.06);
            let border = NSColor::labelColor().colorWithAlphaComponent(0.14);
            let outline = NSColor::separatorColor();
            // SAFETY: both views are layer-backed; the colours outlive the
            // calls, and CALayer retains what it is given.
            unsafe {
                let layer: *mut AnyObject = msg_send![row, layer];
                if !layer.is_null() {
                    let _: () = msg_send![layer, setBackgroundColor: cg_color(&fill)];
                    let _: () = msg_send![layer, setBorderColor: cg_color(&border)];
                }
                if let Some(body) = &panel_body {
                    let layer: *mut AnyObject = msg_send![&**body, layer];
                    if !layer.is_null() {
                        let _: () = msg_send![layer, setBorderColor: cg_color(&outline)];
                    }
                }
            }
        };
        self.effectiveAppearance()
            .performAsCurrentDrawingAppearance(&block2::RcBlock::new(apply));
    }

    fn begin_drag(&self, event: &NSEvent) {
        let url = drag_payload(&self.ivars().bundle);
        let item = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*url),
        );
        let bounds = self.bounds();
        // SAFETY: `contents` must be an NSImage; it is.
        unsafe { item.setDraggingFrame_contents(bounds, self.snapshot().as_deref()) };
        let session = self.beginDraggingSessionWithItems_event_source(
            &NSArray::from_retained_slice(&[item]),
            event,
            ProtocolObject::from_ref(self),
        );
        session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    }

    /// The row as it looks now, so the drag lifts the tile itself.
    fn snapshot(&self) -> Option<Retained<AnyObject>> {
        let bounds = self.bounds();
        let rep = self.bitmapImageRepForCachingDisplayInRect(bounds)?;
        self.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
        let image = NSImage::initWithSize(NSImage::alloc(), bounds.size);
        image.addRepresentation(&rep);
        Some(Retained::into_super(Retained::into_super(image)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn permission_guide_urls_open_the_privacy_pages() {
        assert!(
            Pane::Accessibility
                .settings_url()
                .ends_with("?Privacy_Accessibility")
        );
        assert!(
            Pane::ScreenRecording
                .settings_url()
                .ends_with("?Privacy_ScreenCapture")
        );
        for pane in [Pane::Accessibility, Pane::ScreenRecording] {
            assert!(pane.settings_url().starts_with(
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension"
            ));
        }
    }

    #[test]
    fn permission_guide_flips_window_server_coordinates() {
        // A 700x500 window 100 pt from the top of a 1000 pt display.
        let appkit = cg_to_appkit(rect(200.0, 100.0, 700.0, 500.0), 1000.0);
        assert_eq!(appkit, rect(200.0, 400.0, 700.0, 500.0));
        // Above the primary display (a second screen on top): negative CG y.
        let above = cg_to_appkit(rect(0.0, -800.0, 700.0, 500.0), 1000.0);
        assert_eq!(above.y, 1300.0);
    }

    #[test]
    fn permission_guide_picks_the_main_settings_window() {
        let windows = [
            (0, rect(0.0, 0.0, 300.0, 200.0)),   // too small: a popover
            (0, rect(10.0, 10.0, 715.0, 600.0)), // the window
            (8, rect(0.0, 0.0, 2000.0, 2000.0)), // not a normal window
            (0, rect(20.0, 20.0, 500.0, 300.0)), // a sheet
        ];
        assert_eq!(
            pick_settings_window(&windows),
            Some(rect(10.0, 10.0, 715.0, 600.0))
        );
        assert_eq!(pick_settings_window(&[]), None);
    }

    #[test]
    fn permission_guide_centres_on_the_page_above_the_bottom_edge() {
        let visible = rect(0.0, 0.0, 1512.0, 945.0);
        let settings = rect(300.0, 150.0, 715.0, 640.0);
        let (x, y) = anchored_origin(settings, visible, (460.0, 108.0));
        // The page is the window minus the 215 pt sidebar: 500 wide at 515.
        assert_eq!(x, 515.0 + (500.0 - 460.0) / 2.0);
        assert_eq!(y, 150.0 + 14.0);
    }

    #[test]
    fn permission_guide_stays_on_screen() {
        let visible = rect(0.0, 25.0, 1512.0, 900.0);
        // Window hanging off the bottom-left corner.
        let (x, y) = anchored_origin(rect(-400.0, -300.0, 715.0, 640.0), visible, (460.0, 108.0));
        assert_eq!((x, y), (8.0, 33.0));
        // Off the top-right corner.
        let (x, y) = anchored_origin(rect(1400.0, 2000.0, 715.0, 640.0), visible, (460.0, 108.0));
        assert_eq!((x, y), (1512.0 - 460.0 - 8.0, 25.0 + 900.0 - 108.0 - 8.0));
        // A page narrower than the panel: the panel starts at the page.
        let (x, _) = anchored_origin(rect(500.0, 200.0, 400.0, 500.0), visible, (460.0, 108.0));
        assert_eq!(x, 715.0);
    }

    #[test]
    fn permission_guide_drag_carries_the_app_bundle_url() {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
        let bundle = std::env::temp_dir().join(format!("Diri Guide {}.app", std::process::id()));
        let url = drag_payload(&bundle);
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        pasteboard.clearContents();
        assert!(pasteboard.writeObjects(&NSArray::from_retained_slice(&[
            ProtocolObject::from_retained(url)
        ])));
        let written = pasteboard
            .stringForType(unsafe { NSPasteboardTypeFileURL })
            .expect("a file URL on the pasteboard")
            .to_string();
        // SAFETY: the pasteboard is this test's own, unused after this.
        let _: () = unsafe { msg_send![&*pasteboard, releaseGlobally] };
        let read = NSURL::URLWithString(&NSString::from_str(&written)).expect("a URL");
        let path = read.path().expect("a file path").to_string();
        assert_eq!(Path::new(&path), bundle.as_path());
        assert!(written.starts_with("file:///"), "{written}");
        assert!(written.ends_with(".app/"), "a directory URL: {written}");
    }
}
