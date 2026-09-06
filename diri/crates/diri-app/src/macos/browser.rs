//! Native WebKit backing for the workspace Browser surface.
//!
//! GPUI does not expose an arbitrary native-child-view primitive. This owner
//! bridges that small platform seam with typed `objc2` WebKit/AppKit bindings;
//! it owns the child view, converts coordinates once, and tears the child down
//! before releasing it. Navigation stays in WebKit, while the GPUI toolbar
//! receives a compact state projection.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSView};
use objc2_foundation::{
    NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions, NSObject,
    NSObjectNSKeyValueObserverRegistration, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    NSURL, NSURLRequest,
};
use objc2_web_kit::{WKNavigation, WKNavigationDelegate, WKWebView, WKWebViewConfiguration};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::inspector::BrowserState;

const STATE_KEYS: [&str; 5] = ["URL", "title", "canGoBack", "canGoForward", "loading"];

/// Top-left frame coordinates in GPUI points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Keeps a separate native page and navigation history for each workspace tab.
/// Only the selected page can be attached visibly; inactive pages retain state.
pub struct NativeBrowser {
    active: Option<u64>,
    page: BrowserPage,
    pages: std::collections::HashMap<u64, BrowserPage>,
}

impl std::ops::Deref for NativeBrowser {
    type Target = BrowserPage;
    fn deref(&self) -> &Self::Target {
        &self.page
    }
}
impl std::ops::DerefMut for NativeBrowser {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.page
    }
}

impl NativeBrowser {
    pub fn new() -> (Self, tokio::sync::mpsc::Receiver<()>) {
        let (events, receiver) = tokio::sync::mpsc::channel(1);
        (
            Self {
                active: None,
                page: BrowserPage::new(events),
                pages: std::collections::HashMap::new(),
            },
            receiver,
        )
    }

    pub fn select_tab(&mut self, id: u64) {
        if self.active == Some(id) {
            return;
        }
        self.page.set_visible(false);
        let next = self
            .pages
            .remove(&id)
            .unwrap_or_else(|| BrowserPage::new(self.page.events.clone()));
        if let Some(previous) = self.active.replace(id) {
            let old = std::mem::replace(&mut self.page, next);
            self.pages.insert(previous, old);
        } else {
            // Initial page can be primed before the first tab is mounted.
            if !self.page.has_page() {
                self.page = next;
            }
        }
        let _ = self.page.events.try_send(());
    }

    pub fn tab_states(&self) -> Vec<(u64, BrowserState)> {
        self.active
            .map(|id| (id, self.page.state()))
            .into_iter()
            .chain(self.pages.iter().map(|(id, page)| (*id, page.state())))
            .collect()
    }

    pub fn close_tab(&mut self, id: u64) {
        if self.active == Some(id) {
            self.page.clear();
            self.active = None;
        } else {
            self.pages.remove(&id);
        }
    }

    /// Native content shares the measured GPUI body, including its borders,
    /// toolbars and recovery banner. Apply the frame during paint, after layout
    /// has resolved, without a bounds notification or a second layout pass.
    pub fn surface(browser: Rc<RefCell<Self>>) -> impl gpui::IntoElement {
        use gpui::prelude::*;
        gpui::canvas(
            |_, _, _| (),
            move |bounds, (), window, _| {
                let mut browser = browser.borrow_mut();
                let visible = browser.visible;
                browser.sync(
                    window,
                    visible,
                    BrowserFrame {
                        x: bounds.origin.x.into(),
                        y: bounds.origin.y.into(),
                        width: bounds.size.width.into(),
                        height: bounds.size.height.into(),
                    },
                );
            },
        )
        .absolute()
        .inset_0()
    }
}

/// A lazily-attached, single-tab WebKit view.
pub struct BrowserPage {
    web_view: Option<Retained<BrowserWebView>>,
    /// Non-owning: AppKit owns the GPUI root view for the window lifetime.
    parent: Option<*const NSView>,
    pending_url: Option<String>,
    delegate: Option<Retained<BrowserDelegate>>,
    events: tokio::sync::mpsc::Sender<()>,
    visible: bool,
    pointer_passthrough: bool,
    key_monitor: Option<Retained<objc2::runtime::AnyObject>>,
}

impl BrowserPage {
    fn new(events: tokio::sync::mpsc::Sender<()>) -> Self {
        Self {
            web_view: None,
            parent: None,
            pending_url: None,
            delegate: None,
            events,
            visible: false,
            pointer_passthrough: false,
            key_monitor: None,
        }
    }

    pub fn has_page(&self) -> bool {
        self.pending_url.is_some()
    }

    pub fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !visible {
            self.hide();
        } else if self.has_page()
            && self.error().is_none()
            && let Some(view) = &self.web_view
        {
            // Cached GPUI bodies may reuse their paint when only an overlay
            // closes. Their native frame remains valid; no relayout is needed.
            view.setHidden(false);
        }
    }

    pub fn set_pointer_passthrough(&mut self, passthrough: bool) {
        self.pointer_passthrough = passthrough;
        if let Some(view) = &self.web_view {
            view.ivars().pointer_passthrough.set(passthrough);
        }
    }

    pub fn sync(&mut self, window: &impl HasWindowHandle, visible: bool, frame: BrowserFrame) {
        if !visible
            || !self.has_page()
            || self.error().is_some()
            || frame.width <= 1.0
            || frame.height <= 1.0
        {
            self.hide();
            return;
        }
        let Some(parent) = parent_view(window) else {
            self.hide();
            return;
        };
        let Some(view) = self.ensure_view(parent) else {
            return;
        };
        // GPUI lays out from the top. AppKit views are usually bottom-left,
        // but a flipped host view uses the same origin and must not be flipped
        // a second time.
        let parent = unsafe { &*parent };
        let bounds = parent.bounds();
        let y = if parent.isFlipped() {
            f64::from(frame.y)
        } else {
            (bounds.size.height - f64::from(frame.y + frame.height)).max(0.0)
        };
        let frame = NSRect::new(
            NSPoint::new(f64::from(frame.x), y),
            NSSize::new(f64::from(frame.width), f64::from(frame.height)),
        );
        if view.frame() != frame {
            view.setFrame(frame);
        }
        if view.isHidden() {
            view.setHidden(false);
        }
    }

    pub fn load(&mut self, url: String) {
        if request_for(&url).is_none() {
            return;
        }
        if let Some(delegate) = &self.delegate {
            *delegate.ivars().error.borrow_mut() = None;
        }
        self.pending_url = Some(url.clone());
        if let (Some(view), Some(request)) = (&self.web_view, request_for(&url)) {
            unsafe { view.loadRequest(&request) };
        }
    }

    pub fn go_back(&self) {
        if let Some(view) = &self.web_view {
            unsafe { view.goBack() };
        }
    }

    pub fn go_forward(&self) {
        if let Some(view) = &self.web_view {
            unsafe { view.goForward() };
        }
    }

    pub fn reload(&self) {
        if let Some(delegate) = &self.delegate {
            *delegate.ivars().error.borrow_mut() = None;
        }
        if let Some(view) = &self.web_view {
            unsafe { view.reload() };
        }
    }

    pub fn state(&self) -> BrowserState {
        let Some(view) = &self.web_view else {
            return BrowserState {
                url: self.pending_url.clone(),
                ..BrowserState::default()
            };
        };
        BrowserState {
            url: unsafe { view.URL() }
                .and_then(|url| url.absoluteString())
                .map(|value| value.to_string()),
            title: unsafe { view.title() }.map(|value| value.to_string()),
            favicon: self
                .delegate
                .as_ref()
                .and_then(|delegate| delegate.ivars().favicon.lock().unwrap().image.clone()),
            can_go_back: unsafe { view.canGoBack() },
            can_go_forward: unsafe { view.canGoForward() },
            is_loading: unsafe { view.isLoading() },
            error: self.error(),
        }
    }

    pub fn clear(&mut self) {
        self.detach();
        self.pending_url = None;
        let _ = self.events.try_send(());
    }

    fn error(&self) -> Option<String> {
        self.delegate
            .as_ref()
            .and_then(|delegate| delegate.ivars().error.borrow().clone())
    }

    pub fn has_focus(&self) -> bool {
        self.web_view
            .as_ref()
            .is_some_and(|view| view_has_focus(view))
    }

    /// GPUI focus handles do not change AppKit's first responder. Address-bar
    /// clicks must perform both halves of the focus handoff.
    pub fn focus_chrome(&self) {
        self.release_focus();
    }

    fn release_focus(&self) {
        if self.has_focus()
            && let Some(parent) = self.parent
        {
            // AppKit owns the parent until detach; return first responder to
            // GPUI before hiding or removing its native child.
            let parent = unsafe { &*parent };
            if let Some(window) = parent.window() {
                window.makeFirstResponder(Some(parent));
            }
        }
    }

    pub fn hide(&mut self) {
        self.release_focus();
        if let Some(view) = &self.web_view
            && !view.isHidden()
        {
            view.setHidden(true);
        }
    }

    fn ensure_view(&mut self, parent: *const NSView) -> Option<&BrowserWebView> {
        if self.parent != Some(parent) {
            self.detach();
            self.parent = Some(parent);
        }
        if self.web_view.is_none() {
            let mtm = MainThreadMarker::new()?;
            let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
            let view = BrowserWebView::new(mtm, &configuration, self.pointer_passthrough);
            let delegate = BrowserDelegate::new(mtm, self.events.clone());
            unsafe {
                view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
                for key in STATE_KEYS {
                    view.addObserver_forKeyPath_options_context(
                        &delegate,
                        &NSString::from_str(key),
                        NSKeyValueObservingOptions::empty(),
                        std::ptr::null_mut(),
                    );
                }
            }
            self.delegate = Some(delegate);
            // The parent receives its own retain. `self` retains the child so
            // it is valid until `detach` removes it, including a host change.
            unsafe { (&*parent).addSubview(&view) };
            self.key_monitor = browser_key_monitor(&view, self.events.clone());
            self.web_view = Some(view);
            if let Some(url) = self.pending_url.clone() {
                self.load(url);
            }
        }
        self.web_view.as_deref()
    }

    fn detach(&mut self) {
        self.release_focus();
        if let Some(monitor) = self.key_monitor.take() {
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
        if let Some(view) = self.web_view.take() {
            // Remove while our retain still exists. The old parent also holds
            // the view, so dropping first could otherwise leave an overlay.
            unsafe {
                if let Some(delegate) = &self.delegate {
                    delegate.reset_favicon();
                    for key in STATE_KEYS {
                        view.removeObserver_forKeyPath(delegate, &NSString::from_str(key));
                    }
                }
                view.setNavigationDelegate(None);
                view.stopLoading();
            }
            view.removeFromSuperview();
        }
        self.delegate = None;
        self.parent = None;
    }
}

impl Drop for BrowserPage {
    fn drop(&mut self) {
        self.detach();
    }
}

struct BrowserWebViewIvars {
    pointer_passthrough: Cell<bool>,
}

define_class!(
    #[unsafe(super(WKWebView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriWorkspaceWebView"]
    #[ivars = BrowserWebViewIvars]
    struct BrowserWebView;

    unsafe impl NSObjectProtocol for BrowserWebView {}

    impl BrowserWebView {
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            if self.ivars().pointer_passthrough.get() {
                None
            } else {
                unsafe { msg_send![super(self), hitTest: point] }
            }
        }
    }
);

impl BrowserWebView {
    fn new(
        mtm: MainThreadMarker,
        configuration: &WKWebViewConfiguration,
        passthrough: bool,
    ) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(BrowserWebViewIvars {
            pointer_passthrough: Cell::new(passthrough),
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        unsafe { msg_send![super(this), initWithFrame: frame, configuration: configuration] }
    }
}

fn view_has_focus(view: &NSView) -> bool {
    view.window()
        .and_then(|window| window.firstResponder())
        .is_some_and(|responder| {
            responder
                .downcast_ref::<NSView>()
                .is_some_and(|focused| focused.isDescendantOf(view))
        })
}

fn browser_key_monitor(
    view: &Retained<BrowserWebView>,
    events: tokio::sync::mpsc::Sender<()>,
) -> Option<Retained<objc2::runtime::AnyObject>> {
    let view = view.clone();
    let handler = block2::RcBlock::new(move |event: std::ptr::NonNull<NSEvent>| {
        let native = unsafe { event.as_ref() };
        if view.isHidden() || native.window(view.mtm()) != view.window() {
            return event.as_ptr();
        }
        // Notify after AppKit processes a click, so the address highlight can
        // follow focus moving into the document, without polling.
        if native.r#type() == objc2_app_kit::NSEventType::LeftMouseDown {
            let _ = events.try_send(());
            return event.as_ptr();
        }
        if !view_has_focus(&view) {
            return event.as_ptr();
        }
        let modifiers = native.modifierFlags();
        let key = native
            .charactersIgnoringModifiers()
            .map(|key| key.to_string().to_lowercase())
            .unwrap_or_default();
        let browser_key = modifiers.contains(NSEventModifierFlags::Command)
            && !modifiers.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option)
            && (key == "l"
                || (!modifiers.contains(NSEventModifierFlags::Shift)
                    && matches!(key.as_str(), "t" | "w" | "r" | "[" | "]"))
                || (modifiers.contains(NSEventModifierFlags::Shift) && key == "d"));
        if browser_key && let Some(parent) = (unsafe { view.superview() }) {
            // Deliver exactly once through GPUI's ordinary key path. Returning
            // nil prevents WebKit from also acting on this application shortcut.
            parent.keyDown(native);
            return std::ptr::null_mut();
        }
        event.as_ptr()
    });
    unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyDown | NSEventMask::LeftMouseDown,
            &handler,
        )
    }
}

fn parent_view(window: &impl HasWindowHandle) -> Option<*const NSView> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    Some(handle.ns_view.as_ptr().cast())
}

fn request_for(url: &str) -> Option<objc2::rc::Retained<NSURLRequest>> {
    let string = NSString::from_str(url);
    let url = NSURL::URLWithString(&string)?;
    Some(NSURLRequest::requestWithURL(&url))
}

#[derive(Default)]
struct FaviconState {
    generation: u64,
    image: Option<std::sync::Arc<gpui::Image>>,
}

struct BrowserDelegateIvars {
    events: tokio::sync::mpsc::Sender<()>,
    error: RefCell<Option<String>>,
    favicon: std::sync::Arc<std::sync::Mutex<FaviconState>>,
    favicon_task: RefCell<Option<Retained<objc2_foundation::NSURLSessionDataTask>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriWorkspaceBrowserDelegate"]
    #[ivars = BrowserDelegateIvars]
    struct BrowserDelegate;

    unsafe impl NSObjectProtocol for BrowserDelegate {}

    impl BrowserDelegate {
        // WebKit marks these properties KVO-compliant, including same-document
        // history and script-driven titles. Coalesce changes on the existing
        // bounded notification channel instead of polling during root renders.
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observed_state(
            &self,
            _key_path: Option<&NSString>,
            _object: Option<&objc2::runtime::AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, objc2::runtime::AnyObject>>,
            _context: *mut std::ffi::c_void,
        ) {
            self.changed();
        }
    }

    unsafe impl WKNavigationDelegate for BrowserDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn started(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.reset_favicon();
            *self.ivars().error.borrow_mut() = None;
            self.changed();
        }
        #[unsafe(method(webView:didCommitNavigation:))]
        fn committed(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.changed();
        }
        #[unsafe(method(webView:didFinishNavigation:))]
        fn finished(&self, view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.discover_favicon(view);
            self.changed();
        }
        #[unsafe(method(webView:didReceiveServerRedirectForProvisionalNavigation:))]
        fn redirected(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.changed();
        }
        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn provisional_failed(
            &self,
            _view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.failed(error);
        }
        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn failed_navigation(
            &self,
            _view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.failed(error);
        }
        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn terminated(&self, _view: &WKWebView) {
            *self.ivars().error.borrow_mut() =
                Some("This page stopped responding. Reload to continue.".into());
            self.changed();
        }
    }
);

impl BrowserDelegate {
    fn new(mtm: MainThreadMarker, events: tokio::sync::mpsc::Sender<()>) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(BrowserDelegateIvars {
            events,
            error: RefCell::new(None),
            favicon: Default::default(),
            favicon_task: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }
    fn reset_favicon(&self) {
        if let Some(task) = self.ivars().favicon_task.borrow_mut().take() {
            task.cancel();
        }
        let mut state = self.ivars().favicon.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        state.image = None;
    }

    fn discover_favicon(&self, view: &WKWebView) {
        let delegate = unsafe { Retained::retain(self as *const Self as *mut Self) }.unwrap();
        let generation = self.ivars().favicon.lock().unwrap().generation;
        let completion = block2::RcBlock::new(
            move |value: *mut objc2::runtime::AnyObject, _error: *mut NSError| {
                let Some(value) =
                    (unsafe { value.as_ref() }).and_then(|value| value.downcast_ref::<NSString>())
                else {
                    return;
                };
                if delegate.ivars().favicon.lock().unwrap().generation != generation {
                    return;
                }
                delegate.load_favicon(&value.to_string(), generation);
            },
        );
        // Resolve relative links in the document (including <base>), with the
        // conventional same-origin fallback. No third-party favicon service.
        let script = NSString::from_str(
            "(() => { const link = document.querySelector('link[rel~=icon], link[rel=apple-touch-icon]'); return link ? link.href : new URL('/favicon.ico', location.href).href; })()",
        );
        unsafe {
            view.evaluateJavaScript_completionHandler(&script, Some(&completion));
        }
    }

    fn load_favicon(&self, address: &str, generation: u64) {
        use objc2_foundation::{NSData, NSURLRequestCachePolicy, NSURLResponse, NSURLSession};
        let Ok(parsed) = url::Url::parse(address) else {
            return;
        };
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return;
        }
        let Some(url) = NSURL::URLWithString(&NSString::from_str(address)) else {
            return;
        };
        let request = NSURLRequest::requestWithURL_cachePolicy_timeoutInterval(
            &url,
            NSURLRequestCachePolicy::UseProtocolCachePolicy,
            5.0,
        );
        let state = self.ivars().favicon.clone();
        let events = self.ivars().events.clone();
        // This completion runs on NSURLSession's queue; capture only Send data.
        let completion = block2::RcBlock::new(
            move |data: *mut NSData, _response: *mut NSURLResponse, error: *mut NSError| {
                if !error.is_null() {
                    return;
                }
                let Some(data) = (unsafe { data.as_ref() }) else {
                    return;
                };
                if data.length() > 1024 * 1024 {
                    return;
                }
                let bytes = data.to_vec();
                let Some(icon) = decode_favicon(&bytes) else {
                    return;
                };
                let mut state = state.lock().unwrap();
                if state.generation != generation {
                    return;
                }
                state.image = Some(std::sync::Arc::new(icon));
                let _ = events.try_send(());
            },
        );
        let task = unsafe {
            NSURLSession::sharedSession()
                .dataTaskWithRequest_completionHandler(&request, &completion)
        };
        task.resume();
        *self.ivars().favicon_task.borrow_mut() = Some(task);
    }

    fn changed(&self) {
        let _ = self.ivars().events.try_send(());
    }
    fn failed(&self, error: &NSError) {
        // NSURLErrorCancelled is an ordinary superseded navigation.
        if error.code() != -999 {
            *self.ivars().error.borrow_mut() =
                Some("This page could not be loaded. Check the address or try again.".into());
            self.changed();
        }
    }
}

fn decode_favicon(bytes: &[u8]) -> Option<gpui::Image> {
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    if let Ok(svg) = std::str::from_utf8(bytes) {
        let svg = svg.trim_start();
        if svg.starts_with("<svg") || (svg.starts_with("<?xml") && svg.contains("<svg")) {
            return Some(gpui::Image::from_bytes(
                gpui::ImageFormat::Svg,
                bytes.to_vec(),
            ));
        }
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?.thumbnail(32, 32);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        png.into_inner(),
    ))
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct BrowserFixtureHostIvars {
    keys: RefCell<Vec<String>>,
}

#[cfg(debug_assertions)]
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriBrowserFixtureHost"]
    #[ivars = BrowserFixtureHostIvars]
    struct BrowserFixtureHost;

    unsafe impl NSObjectProtocol for BrowserFixtureHost {}

    impl BrowserFixtureHost {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool { true }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.ivars().keys.borrow_mut().push(event.charactersIgnoringModifiers().unwrap().to_string());
        }
    }
);

/// Opt-in native integration fixture. Runs before any app services start and
/// uses only an ephemeral loopback server; never connects to a Diri daemon.
#[cfg(debug_assertions)]
pub fn smoke_test() {
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
    use raw_window_handle::{AppKitWindowHandle, HandleError, WindowHandle};
    use std::io::{Read, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    struct Host(Retained<NSView>);
    impl HasWindowHandle for Host {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let handle = AppKitWindowHandle::new(std::ptr::NonNull::from(&*self.0).cast());
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AppKit(handle)) })
        }
    }
    fn pump() {
        let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
        while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantPast()),
            unsafe { NSDefaultRunLoopMode },
            true,
        ) {
            app.sendEvent(&event);
        }
        app.updateWindows();
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.008));
    }
    #[track_caller]
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "native browser fixture timed out"
            );
            pump();
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let mut request = [0; 4096];
                let count = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                if request.starts_with("GET /site-icon.png ") {
                    let icon =
                        image::RgbaImage::from_pixel(16, 16, image::Rgba([52, 120, 246, 255]));
                    let mut png = std::io::Cursor::new(Vec::new());
                    icon.write_to(&mut png, image::ImageFormat::Png).unwrap();
                    let bytes = png.into_inner();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(&bytes);
                    continue;
                }
                let (title, body) = if request.starts_with("GET /two ") {
                    ("Fixture Two", "<h1>Second fixture page</h1>")
                } else {
                    (
                        "Fixture One",
                        "<h1>Native browser fixture</h1><a id='next' href='/two'>Next page</a>",
                    )
                };
                let html = format!(
                    "<!doctype html><title>{title}</title><link rel=icon href=/site-icon.png>{body}"
                );
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    });
    let mtm = MainThreadMarker::new().expect("fixture must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 480.0)),
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window.makeKeyAndOrderFront(None);
    let fixture_host: Retained<BrowserFixtureHost> = unsafe {
        let this = mtm.alloc().set_ivars(BrowserFixtureHostIvars::default());
        msg_send![super(this), initWithFrame: window.contentView().unwrap().bounds()]
    };
    window.setContentView(Some(&fixture_host));
    let host = Host(window.contentView().expect("fixture content view"));
    let (mut browser, mut events) = NativeBrowser::new();
    browser.select_tab(1);
    browser.set_visible(true);
    let frame = BrowserFrame {
        x: 0.0,
        y: 0.0,
        width: 640.0,
        height: 480.0,
    };
    browser.sync(&host, true, frame);
    assert!(
        browser.web_view.is_none(),
        "blank surface must not cover its prompt"
    );
    browser.load(format!("http://{address}/one"));
    browser.sync(&host, true, frame);
    until(|| {
        browser.state().title.as_deref() == Some("Fixture One") && !browser.state().is_loading
    });
    assert!(
        events.try_recv().is_ok(),
        "native navigation must notify the toolbar"
    );
    let view = browser.web_view.as_ref().unwrap().clone();
    unsafe {
        view.evaluateJavaScript_completionHandler(
            &NSString::from_str("document.getElementById('next').click()"),
            None,
        );
    }
    until(|| {
        browser.state().title.as_deref() == Some("Fixture Two") && !browser.state().is_loading
    });
    assert!(browser.state().url.unwrap().ends_with("/two"));
    assert!(browser.state().can_go_back);
    browser.go_back();
    until(|| {
        browser.state().title.as_deref() == Some("Fixture One") && !browser.state().is_loading
    });
    assert!(browser.state().can_go_forward);
    browser.go_forward();
    until(|| {
        browser.state().title.as_deref() == Some("Fixture Two") && !browser.state().is_loading
    });
    // The tab fixture also runs in noninteractive desktop sessions where
    // WebKit intentionally withholds requestAnimationFrame callbacks.
    let tabs_only = std::env::var_os("DIRI_NATIVE_BROWSER_TABS_ONLY").is_some();
    let mut resize_time = Duration::ZERO;
    if !tabs_only {
        while events.try_recv().is_ok() {}
        unsafe {
            view.evaluateJavaScript_completionHandler(
                &NSString::from_str("history.pushState({}, '', '/same-document')"),
                None,
            );
        }
        until(|| {
            browser
                .state()
                .url
                .as_deref()
                .is_some_and(|url| url.ends_with("/same-document"))
        });
        assert!(
            events.try_recv().is_ok(),
            "same-document URLs must notify the toolbar"
        );
        let script_done = Rc::new(Cell::new(false));
        let done = script_done.clone();
        let completion = block2::RcBlock::new(
            move |_: *mut objc2::runtime::AnyObject, error: *mut NSError| {
                assert!(error.is_null(), "resize script failed: {:?}", unsafe {
                    error.as_ref()
                });
                done.set(true);
            },
        );
        unsafe {
            view.evaluateJavaScript_completionHandler(&NSString::from_str(
            "window.resizeFrames = 0; window.resizeEvents = 0; addEventListener('resize', () => resizeEvents++); function tick() { resizeFrames++; document.title = innerWidth + ':' + resizeFrames + ':' + resizeEvents; requestAnimationFrame(tick); } requestAnimationFrame(tick);"
        ), Some(&completion));
        }
        until(|| script_done.get());
        until(|| {
            browser
                .state()
                .title
                .as_deref()
                .is_some_and(|title| title.contains(':'))
        });
        while events.try_recv().is_ok() {}
        until(|| events.try_recv().is_ok());
        browser.set_pointer_passthrough(true);
        assert!(
            view.hitTest(NSPoint::new(20.0, 20.0)).is_none(),
            "resize motion must reach GPUI"
        );
        for step in 0..240 {
            let width = 320.0 + (step % 120) as f32 * 2.0;
            let resized = BrowserFrame { width, ..frame };
            let start = Instant::now();
            // Repeated root paints without geometry changes are common while a
            // terminal streams output next to the page.
            for _ in 0..8 {
                browser.sync(&host, true, resized);
            }
            resize_time += start.elapsed();
            assert!(!view.isHidden(), "resize must keep the website painted");
            assert_eq!(view.frame().size.width, f64::from(width));
            assert!(std::ptr::eq(&**browser.web_view.as_ref().unwrap(), &*view));
            pump();
            if step == 119 || step == 239 {
                until(|| {
                    browser
                        .state()
                        .title
                        .as_deref()
                        .is_some_and(|title| title.starts_with("558:"))
                });
                let title = browser.state().title.unwrap();
                let values: Vec<u32> = title
                    .split(':')
                    .map(|value| value.parse().unwrap())
                    .collect();
                assert!(
                    values[1] > 10 && values[2] > 10,
                    "page must keep painting and reflowing during the drag: {title}"
                );
            }
        }
    }
    until(|| browser.state().favicon.is_some());
    assert!(window.makeFirstResponder(browser.web_view.as_deref().map(|view| &****view)));
    assert!(browser.has_focus());
    // Model a URL-bar click: focus changes at both the GPUI and AppKit seams.
    browser.focus_chrome();
    assert!(!browser.has_focus(), "address focus must leave WebKit");
    let send_key = |key: &str, modifiers: NSEventModifierFlags| {
        let key = NSString::from_str(key);
        let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            objc2_app_kit::NSEventType::KeyDown, NSPoint::new(0.0, 0.0), modifiers, 0.0,
            window.windowNumber(), None, &key, &key, false, 0,
        ).unwrap();
        app.sendEvent(&event);
        pump();
    };
    send_key("\r", NSEventModifierFlags::empty());
    assert_eq!(
        fixture_host
            .ivars()
            .keys
            .borrow()
            .last()
            .map(String::as_str),
        Some("\r"),
        "Enter belongs to chrome after address focus"
    );
    for (key, modifiers) in [
        ("l", NSEventModifierFlags::Command),
        (
            "L",
            NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        ),
        ("t", NSEventModifierFlags::Command),
        ("w", NSEventModifierFlags::Command),
        ("r", NSEventModifierFlags::Command),
    ] {
        assert!(window.makeFirstResponder(browser.web_view.as_deref().map(|view| &****view)));
        let before = fixture_host.ivars().keys.borrow().len();
        send_key(key, modifiers);
        assert_eq!(
            fixture_host.ivars().keys.borrow().len(),
            before + 1,
            "browser shortcut must reach GPUI exactly once"
        );
    }

    // A hidden tab must relinquish native keyboard focus as well as pixels.
    assert!(window.makeFirstResponder(browser.web_view.as_deref().map(|view| &****view)));
    browser.set_visible(false);
    assert!(
        window.firstResponder().is_none_or(|responder| {
            responder
                .downcast_ref::<NSView>()
                .is_none_or(|view| !view.isDescendantOf(browser.web_view.as_ref().unwrap()))
        }),
        "hidden browser must not keep receiving Enter and other keys"
    );
    browser.set_visible(true);

    let expected_url = browser.state().url;
    browser.select_tab(2);
    browser.set_visible(true);
    assert!(view.isHidden(), "inactive native tabs must be hidden");
    browser.load(format!("http://{address}/one"));
    browser.sync(&host, true, frame);
    until(|| {
        browser.state().title.as_deref() == Some("Fixture One") && !browser.state().is_loading
    });
    let second_view = browser.web_view.as_ref().unwrap().clone();
    browser.select_tab(1);
    browser.set_visible(true);
    browser.sync(&host, true, frame);
    assert!(second_view.isHidden());
    assert!(!view.isHidden());
    assert!(
        std::ptr::eq(&**browser.web_view.as_ref().unwrap(), &*view),
        "tab activation preserves the live web view"
    );
    assert_eq!(browser.state().url, expected_url);
    assert!(browser.state().can_go_back);
    browser.close_tab(2);
    assert!(
        unsafe { second_view.superview() }.is_none(),
        "closing a background tab detaches only its page"
    );
    assert!(!view.isHidden());
    browser.set_pointer_passthrough(false);
    assert!(
        view.hitTest(NSPoint::new(20.0, 20.0)).is_some(),
        "page interaction must resume after release"
    );
    if tabs_only {
        println!(
            "Native browser tabs: independent pages, preserved history, and background close passed"
        );
    } else {
        println!(
            "Native resize sweep: 240 sizes / 1920 syncs in {resize_time:?} of main-thread sync work"
        );
    }
    // Window resizing also changes the AppKit coordinate conversion. Keep the
    // page aligned below a toolbar, with no reload or visibility transition.
    for step in 0..32 {
        let width = 500.0 + f64::from(step) * 8.0;
        let height = 380.0 + f64::from(step) * 4.0;
        window.setContentSize(NSSize::new(width, height));
        browser.sync(
            &host,
            true,
            BrowserFrame {
                x: 200.0,
                y: 84.0,
                width: (width - 200.0) as f32,
                height: (height - 84.0) as f32,
            },
        );
        pump();
        assert!(!view.isHidden());
        assert_eq!(view.frame().origin, NSPoint::new(200.0, 0.0));
        assert_eq!(view.frame().size, NSSize::new(width - 200.0, height - 84.0));
    }
    // Overlay close may reuse the cached GPUI body without another paint.
    browser.set_visible(true);
    browser.set_visible(false);
    assert!(view.isHidden());
    browser.set_visible(true);
    assert!(!view.isHidden());
    browser.sync(&host, false, frame);
    assert!(view.isHidden());
    browser.sync(&host, true, frame);
    assert!(!view.isHidden());
    browser.clear();
    assert!(unsafe { view.superview() }.is_none());
    assert!(!browser.has_page());
    stopping.store(true, Ordering::Relaxed);
    server.join().expect("fixture server cleanup");
    browser.load(format!("http://{address}/unavailable"));
    browser.sync(&host, true, frame);
    until(|| browser.state().error.is_some());
    browser.sync(&host, true, frame);
    assert!(browser.web_view.as_ref().unwrap().isHidden());
    browser.clear();
    window.close();
    println!(
        "Native browser fixture passed: DOM load, navigation/history, independent tabs, address focus and Enter routing, browser shortcuts, favicon discovery, window resize alignment, cached overlay restoration, close, load failure. Live animation sweep: {}.",
        if tabs_only { "skipped" } else { "passed" }
    );
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    #[test]
    fn favicon_decoding_supports_site_formats_and_rejects_bad_data() {
        let icon = image::RgbaImage::from_pixel(16, 16, image::Rgba([52, 120, 246, 255]));
        for format in [image::ImageFormat::Png, image::ImageFormat::Ico] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            icon.write_to(&mut encoded, format).unwrap();
            let decoded = decode_favicon(encoded.get_ref()).unwrap();
            assert_eq!(decoded.format, gpui::ImageFormat::Png);
            let raster = image::load_from_memory(&decoded.bytes).unwrap();
            assert!(raster.width() <= 32 && raster.height() <= 32);
        }
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="blue"/></svg>"#;
        assert_eq!(decode_favicon(svg).unwrap().format, gpui::ImageFormat::Svg);
        assert!(decode_favicon(b"not an image").is_none());
        assert!(decode_favicon(&vec![0; 1024 * 1024 + 1]).is_none());
    }

    #[test]
    fn browser_tabs_keep_urls_and_close_only_the_addressed_page() {
        let (mut browser, _) = NativeBrowser::new();
        browser.select_tab(10);
        browser.load("https://example.com/first".into());
        browser.select_tab(20);
        assert!(!browser.has_page());
        browser.load("https://example.com/second".into());
        browser.select_tab(10);
        assert_eq!(
            browser.state().url.as_deref(),
            Some("https://example.com/first")
        );
        browser.close_tab(20);
        assert_eq!(
            browser.state().url.as_deref(),
            Some("https://example.com/first")
        );
        browser.select_tab(30);
        assert!(!browser.has_page());
        browser.select_tab(10);
        assert_eq!(
            browser.state().url.as_deref(),
            Some("https://example.com/first")
        );
    }
}
