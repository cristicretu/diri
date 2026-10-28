//! The focused terminal as a macOS accessibility text area.
//!
//! GPUI draws every window into one Metal view and exposes no accessibility
//! tree, so VoiceOver, Accessibility Inspector and computer-use agents saw a
//! terminal only as pixels. Each window gets one extra `NSView`, beside
//! GPUI's, that answers the `NSAccessibility` text-area protocol for the
//! terminal last focused in that window: its visible rows as the value, the
//! caret or selection, line-for-index, range-for-line, string-for-range,
//! frame-for-range and index-for-position.
//!
//! Cost model. The view draws nothing, has no layer content, and refuses mouse
//! hits, so input and rendering are untouched. It holds only weak handles to
//! the terminal; the text is built when a client asks and cached by the
//! grid's revision. Until a client has asked, a frame does one pointer
//! comparison and one `Cell` read here. Value-changed notifications go out
//! only after a client has read the terminal recently, or while VoiceOver is
//! on, and at most four times a second.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use diri_term::accessibility::AccessibilityText;
use diri_term::element::{AccessibilityRevision, TerminalAccessibility};
use objc2::rc::{Retained, Weak};
use objc2::runtime::AnyObject;
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel,
};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
use objc2_foundation::{
    NSArray, NSAttributedString, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString,
    NSValue,
};

/// Notifications at most this often, so a busy agent does not flood speech.
pub(crate) const MIN_POST_INTERVAL: Duration = Duration::from_millis(250);
/// A client that read the terminal this recently is still listening.
pub(crate) const CLIENT_ACTIVE_FOR: Duration = Duration::from_secs(60);
/// How long a VoiceOver on/off answer is reused.
const VOICEOVER_RECHECK: Duration = Duration::from_secs(1);

/// What to do about a terminal change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Post {
    /// No client is listening, or a post is already scheduled.
    Skip,
    Now,
    After(Duration),
}

/// When value-changed notifications are worth sending. Pure, so the
/// throttle and the zero-cost-until-asked rule are unit tested.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    queried_at: Option<Instant>,
    last_post: Option<Instant>,
    scheduled: bool,
    voiceover: Option<(Instant, bool)>,
}

impl Activity {
    pub(crate) fn queried(&mut self, now: Instant) {
        self.queried_at = Some(now);
    }

    /// True once any client has read the terminal; before that a change
    /// costs nothing.
    pub(crate) const fn ever_queried(&self) -> bool {
        self.queried_at.is_some()
    }

    pub(crate) fn on_change(&mut self, now: Instant, voiceover: impl FnOnce() -> bool) -> Post {
        let Some(queried_at) = self.queried_at else {
            return Post::Skip;
        };
        if self.scheduled {
            return Post::Skip;
        }
        let recent = now.saturating_duration_since(queried_at) < CLIENT_ACTIVE_FOR;
        if !recent {
            let on = match self.voiceover {
                Some((checked, on))
                    if now.saturating_duration_since(checked) < VOICEOVER_RECHECK =>
                {
                    on
                }
                _ => {
                    let on = voiceover();
                    self.voiceover = Some((now, on));
                    on
                }
            };
            if !on {
                return Post::Skip;
            }
        }
        match self.last_post {
            Some(last) if now.saturating_duration_since(last) < MIN_POST_INTERVAL => {
                self.scheduled = true;
                Post::After(MIN_POST_INTERVAL - now.saturating_duration_since(last))
            }
            _ => {
                self.last_post = Some(now);
                Post::Now
            }
        }
    }

    /// A scheduled post went out.
    pub(crate) fn fired(&mut self, now: Instant) {
        self.scheduled = false;
        self.last_post = Some(now);
    }
}

pub(crate) struct Ivars {
    source: RefCell<Option<TerminalAccessibility>>,
    focused: Cell<bool>,
    label: RefCell<String>,
    cache: RefCell<Option<(AccessibilityRevision, Rc<AccessibilityText>)>>,
    /// The revision clients were last told about.
    announced: Cell<Option<AccessibilityRevision>>,
    activity: RefCell<Activity>,
    posts: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DiriTerminalAccessibilityView"]
    #[ivars = Ivars]
    pub(crate) struct TerminalAccessibilityView;

    unsafe impl NSObjectProtocol for TerminalAccessibilityView {}

    impl TerminalAccessibilityView {
        /// Never takes a click: GPUI's view below handles every pointer event.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> {
            None
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            self.alive()
        }

        #[unsafe(method_id(accessibilityRole))]
        fn accessibility_role(&self) -> Retained<NSString> {
            self.touch();
            NSString::from_str("AXTextArea")
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn accessibility_label(&self) -> Retained<NSString> {
            self.touch();
            NSString::from_str(&self.ivars().label.borrow())
        }

        #[unsafe(method(isAccessibilityFocused))]
        fn is_accessibility_focused(&self) -> bool {
            self.ivars().focused.get() && self.alive()
        }

        #[unsafe(method_id(accessibilityValue))]
        fn accessibility_value(&self) -> Retained<NSString> {
            NSString::from_str(self.text().text())
        }

        #[unsafe(method(accessibilityNumberOfCharacters))]
        fn number_of_characters(&self) -> isize {
            self.text().len() as isize
        }

        #[unsafe(method(accessibilityVisibleCharacterRange))]
        fn visible_character_range(&self) -> NSRange {
            NSRange::new(0, self.text().len())
        }

        #[unsafe(method_id(accessibilitySelectedText))]
        fn selected_text(&self) -> Retained<NSString> {
            NSString::from_str(&self.text().selected_text())
        }

        #[unsafe(method(accessibilitySelectedTextRange))]
        fn selected_text_range(&self) -> NSRange {
            to_ns_range(self.text().selected_range())
        }

        #[unsafe(method_id(accessibilitySelectedTextRanges))]
        fn selected_text_ranges(&self) -> Retained<NSArray<NSValue>> {
            let range = to_ns_range(self.text().selected_range());
            let value: Retained<NSValue> =
                unsafe { msg_send![NSValue::class(), valueWithRange: range] };
            NSArray::from_retained_slice(&[value])
        }

        #[unsafe(method(accessibilityInsertionPointLineNumber))]
        fn insertion_point_line_number(&self) -> isize {
            self.text().insertion_line() as isize
        }

        #[unsafe(method(accessibilityLineForIndex:))]
        fn line_for_index(&self, index: isize) -> isize {
            self.ax_line_for_index(index)
        }

        #[unsafe(method(accessibilityRangeForLine:))]
        fn range_for_line(&self, line: isize) -> NSRange {
            self.ax_range_for_line(line)
        }

        #[unsafe(method(accessibilityRangeForIndex:))]
        fn range_for_index(&self, index: isize) -> NSRange {
            self.ax_range_for_index(index)
        }

        #[unsafe(method_id(accessibilityStringForRange:))]
        fn string_for_range(&self, range: NSRange) -> Retained<NSString> {
            self.ax_string_for_range(range)
        }

        #[unsafe(method_id(accessibilityAttributedStringForRange:))]
        fn attributed_string_for_range(&self, range: NSRange) -> Retained<NSAttributedString> {
            NSAttributedString::from_nsstring(&self.ax_string_for_range(range))
        }

        #[unsafe(method(accessibilityFrameForRange:))]
        fn frame_for_range(&self, range: NSRange) -> NSRect {
            self.ax_frame_for_range(range)
        }

        #[unsafe(method(accessibilityRangeForPosition:))]
        fn range_for_position(&self, point: NSPoint) -> NSRange {
            self.ax_range_for_position(point)
        }

        #[unsafe(method(accessibilityFrame))]
        fn accessibility_frame(&self) -> NSRect {
            self.touch();
            self.grid_frame()
        }

        #[unsafe(method_id(accessibilityHitTest:))]
        fn accessibility_hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let frame = self.grid_frame();
            let inside = point.x >= frame.origin.x
                && point.x < frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y < frame.origin.y + frame.size.height;
            (inside && self.alive()).then(|| Retained::into_super(self.retain()))
        }

        /// AppKit answers parameterized attributes through this informal
        /// protocol only; it does not route them to the `accessibility…:`
        /// methods above, so the bridge spells them out.
        #[unsafe(method_id(accessibilityParameterizedAttributeNames))]
        fn parameterized_attribute_names(&self) -> Retained<NSArray<NSString>> {
            let names: Vec<Retained<NSString>> =
                PARAMETERIZED.iter().map(|name| NSString::from_str(name)).collect();
            NSArray::from_retained_slice(&names)
        }

        #[unsafe(method_id(accessibilityAttributeValue:forParameter:))]
        fn parameterized_value(
            &self,
            attribute: &NSString,
            parameter: Option<&AnyObject>,
        ) -> Option<Retained<AnyObject>> {
            self.ax_parameterized(attribute, parameter)
        }

        #[unsafe(method(diriPostTerminalChange))]
        fn post_scheduled_change(&self) {
            self.ivars().activity.borrow_mut().fired(Instant::now());
            self.post_change();
        }
    }
);

impl TerminalAccessibilityView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(Ivars {
            source: RefCell::new(None),
            focused: Cell::new(false),
            label: RefCell::new(String::new()),
            cache: RefCell::new(None),
            announced: Cell::new(None),
            activity: RefCell::new(Activity::default()),
            posts: Cell::new(0),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn ax_parameterized(
        &self,
        attribute: &NSString,
        parameter: Option<&AnyObject>,
    ) -> Option<Retained<AnyObject>> {
        let parameter = parameter?;
        let name = attribute.to_string();
        if !PARAMETERIZED.contains(&name.as_str()) {
            return unsafe {
                msg_send![super(self), accessibilityAttributeValue: attribute, forParameter: parameter]
            };
        }
        // A client can send anything; reading the wrong kind of value would
        // raise inside AppKit, so a mismatched parameter answers nothing.
        let kind = value_kind(parameter)?;
        let integer = || -> Option<isize> {
            (kind == ValueKind::Number).then(|| unsafe { msg_send![parameter, integerValue] })
        };
        let range = || -> Option<NSRange> {
            (kind == ValueKind::Range).then(|| unsafe { msg_send![parameter, rangeValue] })
        };
        Some(match name.as_str() {
            "AXLineForIndex" => number(self.ax_line_for_index(integer()?)),
            "AXRangeForLine" => range_value(self.ax_range_for_line(integer()?)),
            "AXRangeForIndex" | "AXStyleRangeForIndex" => {
                range_value(self.ax_range_for_index(integer()?))
            }
            "AXStringForRange" => {
                Retained::into_super(Retained::into_super(self.ax_string_for_range(range()?)))
            }
            "AXAttributedStringForRange" => Retained::into_super(Retained::into_super(
                NSAttributedString::from_nsstring(&self.ax_string_for_range(range()?)),
            )),
            "AXBoundsForRange" => {
                let rect = self.ax_frame_for_range(range()?);
                let value: Retained<NSValue> =
                    unsafe { msg_send![NSValue::class(), valueWithRect: rect] };
                Retained::into_super(Retained::into_super(value))
            }
            "AXRangeForPosition" if kind == ValueKind::Point => {
                let point: NSPoint = unsafe { msg_send![parameter, pointValue] };
                range_value(self.ax_range_for_position(point))
            }
            _ => return None,
        })
    }

    fn ax_line_for_index(&self, index: isize) -> isize {
        self.text().line_for_index(index.max(0) as usize) as isize
    }

    fn ax_range_for_line(&self, line: isize) -> NSRange {
        to_ns_range(self.text().range_for_line(line.max(0) as usize))
    }

    fn ax_range_for_index(&self, index: isize) -> NSRange {
        to_ns_range(self.text().range_for_index(index.max(0) as usize))
    }

    fn ax_string_for_range(&self, range: NSRange) -> Retained<NSString> {
        NSString::from_str(&self.text().string_for_range(from_ns_range(range)))
    }

    /// Screen rectangle around every cell `range` covers.
    fn ax_frame_for_range(&self, range: NSRange) -> NSRect {
        let text = self.text();
        let Some(geometry) = self.geometry() else {
            return NSRect::ZERO;
        };
        let mut union: Option<(f64, f64, f64, f64)> = None;
        for span in text.cell_spans(from_ns_range(range)) {
            let rect = geometry.cell_rect(span.row, span.start_col..span.end_col);
            let (x0, y0) = (points(rect.left()), points(rect.top()));
            let (x1, y1) = (points(rect.right()), points(rect.bottom()));
            union = Some(match union {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        }
        union.map_or(NSRect::ZERO, |(x0, y0, x1, y1)| {
            self.window_rect_to_screen(x0, y0, x1 - x0, y1 - y0)
        })
    }

    /// The character under a screen point.
    fn ax_range_for_position(&self, point: NSPoint) -> NSRange {
        let text = self.text();
        let Some(geometry) = self.geometry() else {
            return NSRange::new(0, 0);
        };
        let Some((x, y)) = self.screen_point_to_window(point) else {
            return NSRange::new(0, 0);
        };
        let (row, col) = geometry.cell_at(gpui::point(gpui::px(x as f32), gpui::px(y as f32)));
        to_ns_range(text.range_for_index(text.index_for_cell(row, col)))
    }

    /// The painted grid in screen coordinates.
    fn grid_frame(&self) -> NSRect {
        let Some(geometry) = self.geometry() else {
            return NSRect::ZERO;
        };
        let bounds = geometry.bounds();
        self.window_rect_to_screen(
            points(bounds.left()),
            points(bounds.top()),
            points(bounds.size.width),
            points(bounds.size.height),
        )
    }

    fn alive(&self) -> bool {
        self.ivars()
            .source
            .borrow()
            .as_ref()
            .is_some_and(TerminalAccessibility::is_alive)
    }

    fn touch(&self) {
        self.ivars().activity.borrow_mut().queried(Instant::now());
    }

    fn geometry(&self) -> Option<diri_term::element::AccessibilityGeometry> {
        self.ivars().source.borrow().as_ref()?.geometry()
    }

    /// The current text, rebuilt only when the terminal changed since the
    /// last question. Any read counts as a client listening.
    fn text(&self) -> Rc<AccessibilityText> {
        self.touch();
        let ivars = self.ivars();
        let source = ivars.source.borrow();
        let Some(source) = source.as_ref() else {
            return Rc::default();
        };
        let Some(revision) = source.revision() else {
            return Rc::default();
        };
        let mut cache = ivars.cache.borrow_mut();
        if let Some((cached, text)) = cache.as_ref()
            && *cached == revision
        {
            return Rc::clone(text);
        }
        let text = Rc::new(source.snapshot().unwrap_or_default());
        *cache = Some((revision, Rc::clone(&text)));
        // Clients now hold this state; only later changes are news.
        ivars.announced.set(Some(revision));
        text
    }

    /// Window points from the top-left to screen coordinates (bottom-left
    /// origin), the space `accessibilityFrame` is in.
    fn window_rect_to_screen(&self, x: f64, y: f64, width: f64, height: f64) -> NSRect {
        let height_in_view = self.bounds().size.height;
        let local = NSRect::new(
            NSPoint::new(x, height_in_view - y - height),
            NSSize::new(width, height),
        );
        let in_window = self.convertRect_toView(local, None);
        match self.window() {
            Some(window) => window.convertRectToScreen(in_window),
            None => in_window,
        }
    }

    fn screen_point_to_window(&self, point: NSPoint) -> Option<(f64, f64)> {
        let window = self.window()?;
        let in_window = window.convertPointFromScreen(point);
        let local = self.convertPoint_fromView(in_window, None);
        Some((local.x, self.bounds().size.height - local.y))
    }

    /// Points the bridge at `source`. A different terminal drops the cached
    /// text and, for a listening client, announces the focus move.
    fn set_source(&self, source: &TerminalAccessibility, focused: bool, label: &str) {
        let ivars = self.ivars();
        let same = ivars
            .source
            .borrow()
            .as_ref()
            .is_some_and(|current| current.same_view(source));
        if !same {
            *ivars.source.borrow_mut() = Some(source.clone());
            *ivars.cache.borrow_mut() = None;
            ivars.announced.set(None);
        }
        let focus_moved = ivars.focused.replace(focused) != focused || !same;
        if *ivars.label.borrow() != label {
            label.clone_into(&mut ivars.label.borrow_mut());
        }
        if !ivars.activity.borrow().ever_queried() {
            return;
        }
        if focus_moved && focused {
            post(self, "AXFocusedUIElementChanged");
        }
        self.note_change();
    }

    /// A frame of the focused terminal rendered. Posts only when a client is
    /// listening and the text, caret or selection actually moved.
    fn note_change(&self) {
        let ivars = self.ivars();
        let revision = ivars
            .source
            .borrow()
            .as_ref()
            .and_then(TerminalAccessibility::revision);
        if revision.is_none() || revision == ivars.announced.get() {
            return;
        }
        let decision = ivars
            .activity
            .borrow_mut()
            .on_change(Instant::now(), voiceover_enabled);
        match decision {
            Post::Skip => {}
            Post::Now => self.post_change(),
            Post::After(delay) => unsafe {
                let _: () = msg_send![
                    self,
                    performSelector: sel!(diriPostTerminalChange),
                    withObject: std::ptr::null::<AnyObject>(),
                    afterDelay: delay.as_secs_f64()
                ];
            },
        }
    }

    fn post_change(&self) {
        let ivars = self.ivars();
        let revision = ivars
            .source
            .borrow()
            .as_ref()
            .and_then(TerminalAccessibility::revision);
        if revision.is_none() || revision == ivars.announced.get() {
            return;
        }
        ivars.announced.set(revision);
        ivars.posts.set(ivars.posts.get() + 1);
        post(self, "AXValueChanged");
        post(self, "AXSelectedTextChanged");
    }

    /// Notifications sent so far, read by the AppKit integration test.
    #[allow(dead_code)]
    pub(crate) fn posts(&self) -> u64 {
        self.ivars().posts.get()
    }
}

fn post(element: &TerminalAccessibilityView, name: &str) {
    unsafe extern "C" {
        fn NSAccessibilityPostNotification(element: *mut AnyObject, notification: *const NSString);
    }
    let name = NSString::from_str(name);
    let element: *const AnyObject = (element as &AnyObject) as *const AnyObject;
    unsafe { NSAccessibilityPostNotification(element.cast_mut(), Retained::as_ptr(&name)) };
}

fn voiceover_enabled() -> bool {
    unsafe {
        let workspace: *mut AnyObject = msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
        !workspace.is_null() && msg_send![workspace, isVoiceOverEnabled]
    }
}

const PARAMETERIZED: [&str; 8] = [
    "AXLineForIndex",
    "AXRangeForLine",
    "AXRangeForIndex",
    "AXStyleRangeForIndex",
    "AXStringForRange",
    "AXAttributedStringForRange",
    "AXBoundsForRange",
    "AXRangeForPosition",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueKind {
    Number,
    Range,
    Point,
}

/// What a parameter object holds, from its Objective-C type encoding.
fn value_kind(parameter: &AnyObject) -> Option<ValueKind> {
    let is_value: bool = unsafe { msg_send![parameter, isKindOfClass: NSValue::class()] };
    if !is_value {
        return None;
    }
    let is_number: bool = unsafe { msg_send![parameter, isKindOfClass: objc2::class!(NSNumber)] };
    if is_number {
        return Some(ValueKind::Number);
    }
    let encoding: *const std::ffi::c_char = unsafe { msg_send![parameter, objCType] };
    if encoding.is_null() {
        return None;
    }
    let encoding = unsafe { std::ffi::CStr::from_ptr(encoding) }.to_string_lossy();
    if encoding.starts_with("{_NSRange") {
        Some(ValueKind::Range)
    } else if encoding.starts_with("{CGPoint") || encoding.starts_with("{_NSPoint") {
        Some(ValueKind::Point)
    } else {
        None
    }
}

fn number(value: isize) -> Retained<AnyObject> {
    unsafe { msg_send![objc2::class!(NSNumber), numberWithInteger: value] }
}

fn range_value(range: NSRange) -> Retained<AnyObject> {
    let value: Retained<NSValue> = unsafe { msg_send![NSValue::class(), valueWithRange: range] };
    Retained::into_super(Retained::into_super(value))
}

fn points(value: gpui::Pixels) -> f64 {
    f64::from(f32::from(value))
}

fn to_ns_range(range: std::ops::Range<usize>) -> NSRange {
    NSRange::new(range.start, range.end.saturating_sub(range.start))
}

fn from_ns_range(range: NSRange) -> std::ops::Range<usize> {
    range.location..range.location.saturating_add(range.length)
}

thread_local! {
    /// One accessibility view per native GPUI view, weakly: a closed window
    /// takes its view with it.
    static VIEWS: RefCell<HashMap<usize, Weak<TerminalAccessibilityView>>> =
        RefCell::new(HashMap::new());
}

/// The accessibility view beside `host` (GPUI's `NSView`), created on first
/// use as a sibling below it covering the same content area.
pub(crate) fn view_for(host: &NSView) -> Option<Retained<TerminalAccessibilityView>> {
    let mtm = MainThreadMarker::new()?;
    let key = std::ptr::from_ref(host) as usize;
    if let Some(view) = VIEWS.with_borrow(|views| views.get(&key).and_then(Weak::load)) {
        return Some(view);
    }
    let parent = unsafe { host.superview() }?;
    let view = TerminalAccessibilityView::new(mtm, parent.bounds());
    // Width and height track the content view.
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, Some(host));
    VIEWS.with_borrow_mut(|views| {
        views.retain(|_, view| view.load().is_some());
        views.insert(key, Weak::from_retained(&view));
    });
    Some(view)
}

/// Called from the terminal pane's render for the terminal it shows.
/// `focused` is whether that terminal has keyboard focus.
pub(crate) fn sync(host: &NSView, source: &TerminalAccessibility, focused: bool, label: &str) {
    let Some(view) = view_for(host) else {
        return;
    };
    let ivars = view.ivars();
    let showing_other = ivars
        .source
        .borrow()
        .as_ref()
        .is_some_and(|current| current.is_alive() && !current.same_view(source));
    // An unfocused pane never takes the element from the focused one.
    if showing_other && !focused {
        return;
    }
    view.set_source(source, focused, label);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_posted_before_a_client_reads_the_terminal() {
        let mut activity = Activity::default();
        let now = Instant::now();
        let mut asked_voiceover = false;
        assert_eq!(
            activity.on_change(now, || {
                asked_voiceover = true;
                true
            }),
            Post::Skip
        );
        assert!(!asked_voiceover, "an unread terminal does no work at all");
    }

    #[test]
    fn posts_are_throttled_to_four_a_second() {
        let mut activity = Activity::default();
        let start = Instant::now();
        activity.queried(start);
        assert_eq!(activity.on_change(start, || false), Post::Now);
        let soon = start + Duration::from_millis(100);
        assert_eq!(
            activity.on_change(soon, || false),
            Post::After(Duration::from_millis(150))
        );
        // Already scheduled: further output folds into that post.
        assert_eq!(
            activity.on_change(soon + Duration::from_millis(10), || false),
            Post::Skip
        );
        activity.fired(start + MIN_POST_INTERVAL);
        let later = start + MIN_POST_INTERVAL * 2;
        assert_eq!(activity.on_change(later, || false), Post::Now);
    }

    #[test]
    fn a_quiet_client_stops_receiving_posts_unless_voiceover_is_on() {
        let mut activity = Activity::default();
        let start = Instant::now();
        activity.queried(start);
        let idle = start + CLIENT_ACTIVE_FOR + Duration::from_secs(1);
        assert_eq!(activity.on_change(idle, || false), Post::Skip);
        // The answer is reused for a second, then asked again.
        assert_eq!(activity.on_change(idle, || true), Post::Skip);
        let recheck = idle + VOICEOVER_RECHECK;
        assert_eq!(activity.on_change(recheck, || true), Post::Now);
    }
}
