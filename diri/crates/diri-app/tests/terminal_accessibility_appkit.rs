//! The terminal accessibility text area, read through AppKit's accessibility
//! attribute API (the calls the accessibility server forwards for VoiceOver,
//! Accessibility Inspector and `AXUIElementCopyAttributeValue` clients).
//! Uses a window that is never ordered on screen.
//!
//! Opt in with
//! `DIRI_TEST_APPKIT_A11Y=1 cargo test -p diri-app --test terminal_accessibility_appkit`.
#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)]
#[path = "../src/macos/terminal_accessibility.rs"]
mod terminal_accessibility;

fn main() {
    #[cfg(target_os = "macos")]
    if std::env::var_os("DIRI_TEST_APPKIT_A11Y").is_some() {
        appkit::verify();
        println!("AppKit terminal accessibility regression passed.");
        return;
    }
    println!(
        "AppKit terminal accessibility regression skipped; opt in on macOS with DIRI_TEST_APPKIT_A11Y=1."
    );
}

#[cfg(target_os = "macos")]
mod appkit {
    use std::time::{Duration, Instant};

    use diri_proto::grid::GridCell;
    use diri_term::buffer::{CursorState, GridBuffer};
    use diri_term::element::{AccessibilityGeometry, TerminalElement};
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{ClassType, msg_send};
    use objc2_app_kit::{NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{
        NSArray, NSDate, NSPoint, NSRange, NSRect, NSRunLoop, NSSize, NSString, NSValue,
    };

    use super::terminal_accessibility;

    fn row(text: &str, cols: usize) -> Vec<GridCell> {
        let mut cells = vec![GridCell::BLANK; cols];
        for (col, ch) in text.chars().enumerate() {
            cells[col].scalar = u32::from(ch);
        }
        cells
    }

    fn grid(lines: &[&str], cols: u16) -> GridBuffer {
        let mut buffer = GridBuffer::new(cols, lines.len() as u16);
        for (index, line) in lines.iter().enumerate() {
            let start = index * usize::from(cols);
            buffer.cells[start..start + usize::from(cols)]
                .copy_from_slice(&row(line, usize::from(cols)));
        }
        buffer
    }

    /// Reads `name` the way AppKit's accessibility server does for a remote
    /// client. The public `accessibilityAttributeValue:` does not consult the
    /// `NSAccessibility` protocol methods, so it cannot stand in for the
    /// server here (test-only use of AppKit's private entry point).
    fn attr(element: &AnyObject, name: &str) -> Option<Retained<AnyObject>> {
        let name = NSString::from_str(name);
        let mut error: i32 = 0;
        unsafe {
            msg_send![element, _accessibilityValueForAttribute: &*name, clientError: &mut error]
        }
    }

    fn attribute_names(element: &AnyObject) -> Vec<String> {
        let mut error: i32 = 0;
        let names: Option<Retained<NSArray<NSString>>> =
            unsafe { msg_send![element, _accessibilityAttributeNamesClientError: &mut error] };
        names.map_or_else(Vec::new, |names| {
            names.iter().map(|name| name.to_string()).collect()
        })
    }

    fn param(element: &AnyObject, name: &str, value: &AnyObject) -> Option<Retained<AnyObject>> {
        let name = NSString::from_str(name);
        unsafe { msg_send![element, accessibilityAttributeValue: &*name, forParameter: value] }
    }

    fn string(value: Option<Retained<AnyObject>>) -> String {
        let value = value.expect("attribute value");
        let value = value.downcast::<NSString>().expect("a string");
        value.to_string()
    }

    fn integer(value: Option<Retained<AnyObject>>) -> isize {
        let value = value.expect("attribute value");
        unsafe { msg_send![&*value, integerValue] }
    }

    fn range(value: Option<Retained<AnyObject>>) -> (usize, usize) {
        let value = value.expect("attribute value");
        let range: NSRange = unsafe { msg_send![&*value, rangeValue] };
        (range.location, range.length)
    }

    fn rect(value: Option<Retained<AnyObject>>) -> NSRect {
        let value = value.expect("attribute value");
        unsafe { msg_send![&*value, rectValue] }
    }

    fn number(value: isize) -> Retained<AnyObject> {
        unsafe { msg_send![objc2::class!(NSNumber), numberWithInteger: value] }
    }

    fn range_value(location: usize, length: usize) -> Retained<AnyObject> {
        let value: Retained<NSValue> =
            unsafe { msg_send![NSValue::class(), valueWithRange: NSRange::new(location, length)] };
        Retained::into_super(Retained::into_super(value))
    }

    fn point_value(point: NSPoint) -> Retained<AnyObject> {
        let value: Retained<NSValue> =
            unsafe { msg_send![NSValue::class(), valueWithPoint: point] };
        Retained::into_super(Retained::into_super(value))
    }

    /// Unignored accessibility children of `view`, the way the server walks
    /// a window.
    fn text_areas(view: &NSView) -> Vec<Retained<AnyObject>> {
        let children: Option<Retained<NSArray<AnyObject>>> =
            unsafe { msg_send![view, accessibilityChildren] };
        unsafe extern "C" {
            fn NSAccessibilityUnignoredChildren(children: *const AnyObject) -> *mut AnyObject;
        }
        let Some(children) = children else {
            return Vec::new();
        };
        let unignored =
            unsafe { NSAccessibilityUnignoredChildren(Retained::as_ptr(&children).cast()) };
        let unignored: Retained<NSArray<AnyObject>> =
            unsafe { Retained::retain(unignored.cast()) }.expect("children");
        unignored
            .iter()
            .filter(|child| {
                attr(child, "AXRole").is_some_and(|role| {
                    role.downcast_ref::<NSString>()
                        .is_some_and(|role| role.to_string() == "AXTextArea")
                })
            })
            .collect()
    }

    fn pump(duration: Duration) {
        let until = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
        NSRunLoop::currentRunLoop().runUntilDate(&until);
    }

    /// An `NSView` carrying GPUI's own `accessibilityFocusedUIElement`
    /// (vendored `gpui_macos` patch), so the test exercises the shipped
    /// implementation without a GPUI window behind it.
    fn gpui_focus_host(frame: NSRect) -> Retained<NSView> {
        use objc2::runtime::{AnyClass, Imp, Sel};
        use objc2::sel;
        unsafe extern "C" {
            fn objc_allocateClassPair(
                superclass: &AnyClass,
                name: *const std::ffi::c_char,
                extra: usize,
            ) -> *mut AnyClass;
            fn objc_registerClassPair(class: *mut AnyClass);
            fn class_getMethodImplementation(class: &AnyClass, name: Sel) -> Option<Imp>;
            fn class_addMethod(
                class: *mut AnyClass,
                name: Sel,
                imp: Imp,
                types: *const std::ffi::c_char,
            ) -> bool;
        }
        // Linking gpui_macos registers its classes.
        let _ = gpui_macos::current_physical_key;
        let gpui_view = AnyClass::get(c"GPUIView").expect("gpui_macos registers GPUIView");
        let selector = sel!(accessibilityFocusedUIElement);
        unsafe {
            let class = objc_allocateClassPair(NSView::class(), c"DiriTestGpuiHost".as_ptr(), 0);
            assert!(!class.is_null());
            let imp = class_getMethodImplementation(gpui_view, selector).expect("patched method");
            assert!(class_addMethod(class, selector, imp, c"@@:".as_ptr()));
            objc_registerClassPair(class);
            let object: *mut AnyObject = msg_send![&*class, alloc];
            let object: *mut AnyObject = msg_send![object, initWithFrame: frame];
            Retained::from_raw(object.cast::<NSView>()).expect("host view")
        }
    }

    pub(super) fn verify() {
        let mtm = MainThreadMarker::new().expect("AppKit test must run on the main thread");
        let frame = NSRect::new(NSPoint::new(200.0, 300.0), NSSize::new(400.0, 300.0));
        // Never ordered front: no window appears on screen.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                frame,
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        let content = window.contentView().expect("content view");
        // Stands in for GPUI's view: the sibling the bridge sits beside.
        let host = gpui_focus_host(content.bounds());
        content.addSubview(&host);
        window.makeFirstResponder(Some(&host));

        assert!(
            text_areas(&content).is_empty(),
            "before: the window exposes no text"
        );

        let mut buffer = grid(&["$ echo hello", "hello", "$ "], 20);
        buffer.cursor = CursorState {
            col: 2,
            row: 2,
            visible: true,
            ..CursorState::default()
        };
        let element = TerminalElement::with_buffer(buffer);
        element.note_accessibility_geometry(AccessibilityGeometry {
            origin: gpui::point(gpui::px(12.0), gpui::px(40.0)),
            cell_width: gpui::px(8.0),
            line_height: gpui::px(16.0),
            rows: 3,
            cols: 20,
        });
        let source = element.accessibility();
        terminal_accessibility::sync(&host, &source, true, "Shell");
        let view = terminal_accessibility::view_for(&host).expect("bridge view");
        assert_eq!(view.posts(), 0, "nothing is posted before a client reads");

        // The window now has one text area; GPUI's own view stays as it was.
        let areas = text_areas(&content);
        assert_eq!(areas.len(), 1, "one terminal text area");
        let area = &*areas[0];
        let names = attribute_names(area);
        for required in [
            "AXValue",
            "AXSelectedText",
            "AXSelectedTextRange",
            "AXNumberOfCharacters",
            "AXVisibleCharacterRange",
            "AXInsertionPointLineNumber",
        ] {
            assert!(
                names.iter().any(|name| name == required),
                "{required} in {names:?}"
            );
        }
        let parameterized: Retained<NSArray<NSString>> =
            unsafe { msg_send![area, accessibilityParameterizedAttributeNames] };
        let parameterized: Vec<String> =
            parameterized.iter().map(|name| name.to_string()).collect();
        for required in [
            "AXLineForIndex",
            "AXRangeForLine",
            "AXStringForRange",
            "AXBoundsForRange",
            "AXRangeForPosition",
        ] {
            assert!(
                parameterized.iter().any(|name| name == required),
                "{required}"
            );
        }
        assert_eq!(string(attr(area, "AXValue")), "$ echo hello\nhello\n$ ");
        assert_eq!(
            string(attr(area, "AXDescription").or_else(|| attr(area, "AXTitle"))),
            "Shell"
        );
        assert_eq!(integer(attr(area, "AXNumberOfCharacters")), 21);
        assert_eq!(
            range(attr(area, "AXSelectedTextRange")),
            (21, 0),
            "caret after the prompt"
        );
        assert_eq!(integer(attr(area, "AXInsertionPointLineNumber")), 2);
        assert_eq!(range(attr(area, "AXVisibleCharacterRange")), (0, 21));
        let focused: Retained<AnyObject> = attr(area, "AXFocused").expect("focused");
        let focused: bool = unsafe { msg_send![&*focused, boolValue] };
        assert!(focused);

        // GPUI's view is the first responder; the patched view reports the
        // focused terminal instead of the whole window.
        let focused: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*window, accessibilityFocusedUIElement] };
        let focused = focused.expect("a focused element");
        assert!(
            std::ptr::eq(&*focused, area),
            "keyboard focus resolves to the terminal, got {focused:?}"
        );
        assert_eq!(integer(param(area, "AXLineForIndex", &number(14))), 1);
        assert_eq!(range(param(area, "AXRangeForLine", &number(1))), (13, 6));
        assert_eq!(
            string(param(area, "AXStringForRange", &range_value(13, 5))),
            "hello"
        );
        assert_eq!(range(param(area, "AXRangeForIndex", &number(3))), (3, 1));
        assert!(
            param(area, "AXStringForRange", &number(3)).is_none(),
            "a parameter of the wrong kind answers nothing instead of raising"
        );

        // Frames are in screen space, bottom-left origin: "hello" is row 1,
        // columns 0-5 of a grid at (12, 40) with 8x16 cells.
        let window_frame = window.frame();
        let content_frame = window.contentRectForFrameRect(window_frame);
        let bounds = rect(param(area, "AXBoundsForRange", &range_value(13, 5)));
        let expected_x = content_frame.origin.x + 12.0;
        let expected_y = content_frame.origin.y + content_frame.size.height - 40.0 - 16.0 * 2.0;
        assert!((bounds.origin.x - expected_x).abs() < 0.5, "{bounds:?}");
        assert!((bounds.origin.y - expected_y).abs() < 0.5, "{bounds:?}");
        assert!((bounds.size.width - 40.0).abs() < 0.5 && (bounds.size.height - 16.0).abs() < 0.5);
        // And back: the middle of "l" (col 2) finds index 15.
        let point = NSPoint::new(expected_x + 8.0 * 2.5, expected_y + 8.0);
        assert_eq!(
            range(param(area, "AXRangeForPosition", &point_value(point))),
            (15, 1)
        );
        let whole: NSRect = unsafe { msg_send![area, accessibilityFrame] };
        assert!(
            (whole.size.width - 160.0).abs() < 0.5 && (whole.size.height - 48.0).abs() < 0.5,
            "{whole:?}"
        );

        // The bridge never takes a click from GPUI's view.
        let hit: Option<Retained<NSView>> =
            unsafe { msg_send![&*view, hitTest: NSPoint::new(50.0, 50.0)] };
        assert!(hit.is_none());

        // A client has read: a change now posts, at most four times a second.
        element.buffer().write().expect("grid").cursor.col = 1;
        terminal_accessibility::sync(&host, &source, true, "Shell");
        assert_eq!(view.posts(), 1, "first change posts at once");
        element.buffer().write().expect("grid").cursor.col = 0;
        terminal_accessibility::sync(&host, &source, true, "Shell");
        assert_eq!(view.posts(), 1, "a second change within 250 ms waits");
        pump(Duration::from_millis(400));
        assert_eq!(view.posts(), 2, "the waiting change goes out once");
        terminal_accessibility::sync(&host, &source, true, "Shell");
        pump(Duration::from_millis(300));
        assert_eq!(view.posts(), 2, "no change, no post");

        // An unfocused pane in the same window does not steal the element.
        let other = TerminalElement::with_buffer(grid(&["other"], 10));
        terminal_accessibility::sync(&host, &other.accessibility(), false, "Other");
        assert_eq!(string(attr(area, "AXValue")), "$ echo hello\nhello\n$");

        measure(&content, &host, &element);

        // A closed terminal leaves nothing behind.
        drop(element);
        assert!(
            text_areas(&content).is_empty(),
            "a closed terminal is not exposed"
        );
        drop(other);
        window.close();
    }

    /// Per-frame and per-read costs, printed for the PR.
    fn measure(content: &NSView, host: &NSView, element: &TerminalElement) {
        let mtm = MainThreadMarker::new().expect("main thread");
        let frames = 100_000u32;
        let time_frames = |host: &NSView, source: &diri_term::element::TerminalAccessibility| {
            let started = Instant::now();
            for _ in 0..frames {
                terminal_accessibility::sync(host, source, true, "Shell");
            }
            started.elapsed().as_nanos() as f64 / f64::from(frames)
        };
        // A window no client has read: the cost every user pays.
        let idle_host = NSView::initWithFrame(mtm.alloc(), content.bounds());
        content.addSubview(&idle_host);
        let idle = TerminalElement::with_buffer(grid(&["idle"], 10));
        let idle_source = idle.accessibility();
        terminal_accessibility::sync(&idle_host, &idle_source, true, "Shell");
        let unread = time_frames(&idle_host, &idle_source);
        // A window a client is reading, grid unchanged between frames.
        let read = time_frames(host, &element.accessibility());
        println!("sync per frame, no client: {unread:.0} ns; client reading: {read:.0} ns");

        let mut big = GridBuffer::new(200, 50);
        for line in 0..50usize {
            let text = format!("{line:03} {}", "lorem ipsum dolor sit amet ".repeat(8));
            big.cells[line * 200..line * 200 + 200].copy_from_slice(&row(&text[..200], 200));
        }
        let big = TerminalElement::with_buffer(big);
        let handle = big.accessibility();
        let reads = 200u32;
        let started = Instant::now();
        for _ in 0..reads {
            std::hint::black_box(handle.snapshot());
        }
        let per_snapshot = started.elapsed().as_secs_f64() * 1e6 / f64::from(reads);
        println!(
            "text snapshot of a 200x50 grid (built only when a client asks): {per_snapshot:.1} us"
        );
        idle_host.removeFromSuperview();
    }
}
