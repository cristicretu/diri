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
use objc2_app_kit::NSView;
use objc2_foundation::{
    NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{WKNavigation, WKNavigationDelegate, WKWebView, WKWebViewConfiguration};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;

use crate::inspector::BrowserState;

/// Top-left frame coordinates in GPUI points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A lazily-attached, single-tab WebKit view.
pub struct NativeBrowser {
    web_view: Option<objc2::rc::Retained<WKWebView>>,
    /// Non-owning: AppKit owns the GPUI root view for the window lifetime.
    parent: Option<*const NSView>,
    pending_url: Option<String>,
    delegate: Option<Retained<BrowserDelegate>>,
    events: tokio::sync::mpsc::Sender<()>,
}

impl NativeBrowser {
    pub fn new() -> (Self, tokio::sync::mpsc::Receiver<()>) {
        let (events, receiver) = tokio::sync::mpsc::channel(1);
        (
            Self {
                web_view: None,
                parent: None,
                pending_url: None,
                delegate: None,
                events,
            },
            receiver,
        )
    }

    pub fn has_page(&self) -> bool {
        self.pending_url.is_some()
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
        view.setFrame(NSRect::new(
            NSPoint::new(f64::from(frame.x), y),
            NSSize::new(f64::from(frame.width), f64::from(frame.height)),
        ));
        view.setHidden(false);
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

    pub fn hide(&mut self) {
        if let Some(view) = &self.web_view {
            view.setHidden(true);
        }
    }

    fn ensure_view(&mut self, parent: *const NSView) -> Option<&WKWebView> {
        if self.parent != Some(parent) {
            self.detach();
            self.parent = Some(parent);
        }
        if self.web_view.is_none() {
            let mtm = MainThreadMarker::new()?;
            let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
            let view = unsafe {
                WKWebView::initWithFrame_configuration(
                    mtm.alloc(),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
                    &configuration,
                )
            };
            let delegate = BrowserDelegate::new(mtm, self.events.clone());
            unsafe {
                view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            }
            self.delegate = Some(delegate);
            // The parent receives its own retain. `self` retains the child so
            // it is valid until `detach` removes it, including a host change.
            unsafe { (&*parent).addSubview(&view) };
            self.web_view = Some(view);
            if let Some(url) = self.pending_url.clone() {
                self.load(url);
            }
        }
        self.web_view.as_deref()
    }

    fn detach(&mut self) {
        if let Some(view) = self.web_view.take() {
            // Remove while our retain still exists. The old parent also holds
            // the view, so dropping first could otherwise leave an overlay.
            unsafe {
                view.setNavigationDelegate(None);
                view.stopLoading();
            }
            view.removeFromSuperview();
        }
        self.delegate = None;
        self.parent = None;
    }
}

impl Drop for NativeBrowser {
    fn drop(&mut self) {
        self.detach();
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

struct BrowserDelegateIvars {
    events: tokio::sync::mpsc::Sender<()>,
    error: RefCell<Option<String>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriWorkspaceBrowserDelegate"]
    #[ivars = BrowserDelegateIvars]
    struct BrowserDelegate;

    unsafe impl NSObjectProtocol for BrowserDelegate {}
    unsafe impl WKNavigationDelegate for BrowserDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn started(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            *self.ivars().error.borrow_mut() = None;
            self.changed();
        }
        #[unsafe(method(webView:didCommitNavigation:))]
        fn committed(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.changed();
        }
        #[unsafe(method(webView:didFinishNavigation:))]
        fn finished(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
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
        });
        unsafe { msg_send![super(this), init] }
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

/// Opt-in native integration fixture. Runs before any app services start and
/// uses only an ephemeral loopback server; never connects to a Diri daemon.
#[cfg(debug_assertions)]
pub fn smoke_test() {
    use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSDate, NSRunLoop};
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
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "native browser fixture timed out"
            );
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
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
                let (title, body) = if request.starts_with("GET /two ") {
                    ("Fixture Two", "<h1>Second fixture page</h1>")
                } else {
                    (
                        "Fixture One",
                        "<h1>Native browser fixture</h1><a id='next' href='/two'>Next page</a>",
                    )
                };
                let html = format!("<!doctype html><title>{title}</title>{body}");
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
    let host = Host(window.contentView().expect("fixture content view"));
    let (mut browser, mut events) = NativeBrowser::new();
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
        "Native browser fixture passed: DOM load, link navigation, history, event delivery, hide/show, close, load failure."
    );
}
