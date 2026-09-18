//! Native-feeling scrolling for GPUI lists.
//!
//! GPUI ships no scrollbar and clamps every scroll offset at the content's
//! edges, so out of the box a list in diri scrolls like a web page from 2005:
//! no indicator of where you are, and a hard stop at either end. AppKit gives
//! every `NSScrollView` two things for free that this module reproduces:
//!
//! - **Overlay scrollers.** A thin rounded knob on the trailing edge that
//!   appears while the content moves, lingers for a second and fades, widens
//!   into a track when the pointer reaches it, and can be dragged. When the
//!   user has set "Show scroll bars: Always" the knob lives in a permanent
//!   track that reserves its own width instead.
//! - **Rubber-band overscroll.** Trackpad gestures past either end pull the
//!   content with increasing resistance and spring it back when the fingers
//!   lift; momentum that runs into an edge bounces off it.
//!
//! One wrapper element, [`ScrollArea`], provides both. It sits around the
//! scrolling element, paints the scroller inside its own bounds, and shifts
//! its child by the overscroll offset at prepaint, so the scroll handle's
//! clamped offset never changes and every existing `scroll_to_*` call keeps
//! working. The geometry and gesture math live in plain functions and structs
//! with no GPUI state, so they are unit-tested directly.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ContentMask, Corners, CursorStyle, DispatchPhase, Display, Edges,
    Element, ElementId, FlexDirection, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Refineable, Rgba, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    Style, StyleRefinement, Styled, TouchPhase, UniformListScrollHandle, Window, fill, point, px,
    quad, size,
};

use crate::{Appearance, SemanticColors};

// ---------------------------------------------------------------------------
// System scroller style
// ---------------------------------------------------------------------------

/// How the system wants scrollers drawn, from System Settings > Appearance >
/// "Show scroll bars".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollerStyle {
    /// "Automatically" / "When scrolling": a translucent knob over the
    /// content that hides itself.
    #[default]
    Overlay,
    /// "Always": a permanent track beside the content.
    Legacy,
}

impl ScrollerStyle {
    /// `NSScroller.scrollerWidth(for: .regular, scrollerStyle: .legacy)`.
    pub const LEGACY_WIDTH: f32 = 15.0;

    /// Width a scroll area gives up to a permanent scroller. Overlay scrollers
    /// float over the content and reserve nothing.
    #[must_use]
    pub fn reserved_width(self) -> Pixels {
        match self {
            Self::Overlay => px(0.0),
            Self::Legacy => px(Self::LEGACY_WIDTH),
        }
    }
}

#[derive(Default)]
struct ScrollerPreference {
    style: ScrollerStyle,
}

impl gpui::Global for ScrollerPreference {}

/// The scroller style every [`ScrollArea`] draws with. Defaults to overlay
/// until the platform layer reports the system preference.
#[must_use]
pub fn scroller_style(cx: &App) -> ScrollerStyle {
    if let Some(forced) = forced_scroller_style() {
        return forced;
    }
    cx.try_global::<ScrollerPreference>()
        .map(|preference| preference.style)
        .unwrap_or_default()
}

/// Records the system scroller style and repaints every window when it
/// changed, since a legacy scroller reserves layout width.
pub fn set_scroller_style(cx: &mut App, style: ScrollerStyle) -> bool {
    let changed = scroller_style(cx) != style;
    cx.set_global(ScrollerPreference { style });
    if changed {
        cx.refresh_windows();
    }
    changed
}

/// Screenshot fixtures cannot toggle System Settings, so `DIRI_VISUAL_SCROLLER`
/// pins the style: `legacy` or `overlay`.
fn forced_scroller_style() -> Option<ScrollerStyle> {
    static FORCED: OnceLock<Option<ScrollerStyle>> = OnceLock::new();
    *FORCED.get_or_init(
        || match std::env::var("DIRI_VISUAL_SCROLLER").ok()?.as_str() {
            "legacy" | "always" => Some(ScrollerStyle::Legacy),
            "overlay" => Some(ScrollerStyle::Overlay),
            _ => None,
        },
    )
}

// ---------------------------------------------------------------------------
// Thumb geometry
// ---------------------------------------------------------------------------

/// Shortest knob macOS draws, so a ten-thousand-line scrollback still has
/// something to grab.
pub const MIN_THUMB_LENGTH: f32 = 20.0;

/// Where the knob sits along its track, in track pixels from the track start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumbGeometry {
    pub start: f32,
    pub length: f32,
}

impl ThumbGeometry {
    #[must_use]
    pub fn end(self) -> f32 {
        self.start + self.length
    }
}

/// Places the knob for content of `content_length` seen through a viewport of
/// `viewport_length`, scrolled `scrolled` pixels from its start, along a track
/// `track_length` long. `overscroll` is how far the content is currently
/// pulled past an edge: positive past the top, negative past the bottom. The
/// knob shrinks by that amount and pins to the edge, as AppKit's does.
///
/// Returns `None` when the content fits and there is nothing to indicate.
#[must_use]
pub fn thumb_geometry(
    track_length: f32,
    viewport_length: f32,
    content_length: f32,
    scrolled: f32,
    overscroll: f32,
) -> Option<ThumbGeometry> {
    if track_length <= 0.0 || viewport_length <= 0.0 || content_length <= viewport_length + 0.5 {
        return None;
    }
    let min_length = MIN_THUMB_LENGTH.min(track_length);
    let length = (track_length * viewport_length / content_length).max(min_length);
    let travel = track_length - length;
    let max_scroll = content_length - viewport_length;
    let fraction = (scrolled / max_scroll).clamp(0.0, 1.0);
    let mut thumb = ThumbGeometry {
        start: travel * fraction,
        length,
    };
    if overscroll > 0.0 {
        thumb.length = (length - overscroll).max(min_length);
        thumb.start = 0.0;
    } else if overscroll < 0.0 {
        thumb.length = (length + overscroll).max(min_length);
        thumb.start = track_length - thumb.length;
    }
    Some(thumb)
}

// ---------------------------------------------------------------------------
// Rubber-band overscroll
// ---------------------------------------------------------------------------

/// Resistance of the rubber band. UIKit's constant; the pull asymptotically
/// approaches the viewport height and reaches a third of it after a
/// viewport's worth of raw travel.
const RUBBER_COEFFICIENT: f32 = 0.55;
/// Angular frequency of the critically damped return spring, per second.
/// Settles a 150px pull to under a pixel in 350 ms.
const SPRING_OMEGA: f32 = 20.0;
/// Angular frequency of the momentum bounce: lower than the return so an
/// impact reads as a distinct dip and recovery.
const BOUNCE_OMEGA: f32 = 14.0;
/// A momentum impact never dips further than this fraction of the viewport.
const MAX_BOUNCE_FRACTION: f32 = 0.22;

/// Maps raw finger travel past an edge to the distance the content moves.
#[must_use]
pub fn rubber_band(stretch: f32, dimension: f32) -> f32 {
    if dimension <= 0.0 {
        return 0.0;
    }
    let pull = stretch.abs() * RUBBER_COEFFICIENT / dimension;
    stretch.signum() * (1.0 - 1.0 / (pull + 1.0)) * dimension
}

/// Inverse of [`rubber_band`], so a gesture that starts mid-bounce picks up
/// the band where it currently is instead of snapping.
#[must_use]
pub fn unrubber(offset: f32, dimension: f32) -> f32 {
    let magnitude = offset.abs();
    if dimension <= 0.0 || magnitude >= dimension {
        return offset.signum() * dimension * 1_000.0;
    }
    offset.signum() * (magnitude / (dimension - magnitude)) * dimension / RUBBER_COEFFICIENT
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gesture {
    /// No fingers down; wheel events are momentum or a mouse wheel.
    Idle,
    /// Fingers on the trackpad: the band follows them directly.
    Finger,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    /// Returning to rest from `from` after the fingers lifted.
    Settle { from: f32, since: Instant },
    /// Momentum hit an edge with `velocity` px/s; the content dips and returns.
    Bounce { velocity: f32, since: Instant },
}

/// One wheel event as the overscroll model sees it.
#[derive(Clone, Copy, Debug)]
pub struct WheelSample {
    /// Vertical delta in GPUI's convention: positive moves content down, i.e.
    /// scrolls toward the top.
    pub delta: f32,
    /// Trackpad (pixel) deltas rubber-band; mouse wheels (line deltas) do not.
    pub precise: bool,
    pub phase: TouchPhase,
    /// The scroll target sits at its top edge and cannot move further.
    pub at_top: bool,
    /// The scroll target sits at its bottom edge and cannot move further.
    pub at_bottom: bool,
    /// Which edges are allowed to give.
    pub bounce_top: bool,
    pub bounce_bottom: bool,
    /// Height of the viewport the band is measured against.
    pub viewport: f32,
}

/// What the scroll area should do with a wheel event after the model saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelOutcome {
    /// The band took the event; do not let the scroll target see it.
    pub consumed: bool,
    /// Delta the band gave back: the part of a reversing pull that carried on
    /// past the resting edge and belongs to the scroll target after all.
    pub release: f32,
}

impl WheelOutcome {
    const PASS: Self = Self {
        consumed: false,
        release: 0.0,
    };
    const TAKEN: Self = Self {
        consumed: true,
        release: 0.0,
    };
}

/// The rubber-band state for one scroll area. Pure: every transition takes
/// the clock as an argument.
#[derive(Clone, Debug)]
pub struct Overscroll {
    /// Raw finger travel past the edge, signed like the visual offset.
    stretch: f32,
    /// Visual offset applied to the content, positive when pulled down.
    offset: f32,
    gesture: Gesture,
    motion: Option<Motion>,
    last_event: Option<Instant>,
}

impl Default for Overscroll {
    fn default() -> Self {
        Self::new()
    }
}

impl Overscroll {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stretch: 0.0,
            offset: 0.0,
            gesture: Gesture::Idle,
            motion: None,
            last_event: None,
        }
    }

    /// The content's current displacement, positive when pulled down past
    /// the top.
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// True while a settle or bounce is still moving the content and the
    /// caller must keep requesting frames.
    #[must_use]
    pub fn is_animating(&self) -> bool {
        self.motion.is_some()
    }

    /// Drops any pull and motion, for Reduce Motion or a target that vanished.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Feeds one wheel event and says whether the scroll target may have it.
    pub fn wheel(&mut self, sample: WheelSample, now: Instant) -> WheelOutcome {
        let outcome = self.wheel_inner(sample, now);
        self.last_event = Some(now);
        outcome
    }

    fn wheel_inner(&mut self, sample: WheelSample, now: Instant) -> WheelOutcome {
        match sample.phase {
            TouchPhase::Started => {
                self.gesture = Gesture::Finger;
                self.motion = None;
                self.stretch = unrubber(self.offset, sample.viewport);
                // A Began event carries no travel; a MayBegin with a delta is
                // treated like the first Moved below.
                if sample.delta == 0.0 {
                    return if self.offset != 0.0 {
                        WheelOutcome::TAKEN
                    } else {
                        WheelOutcome::PASS
                    };
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.gesture = Gesture::Idle;
                self.stretch = 0.0;
                if self.offset != 0.0 && self.motion.is_none() {
                    self.motion = Some(Motion::Settle {
                        from: self.offset,
                        since: now,
                    });
                }
                return if self.offset != 0.0 {
                    WheelOutcome::TAKEN
                } else {
                    WheelOutcome::PASS
                };
            }
            TouchPhase::Moved => {}
        }

        if !sample.precise {
            return WheelOutcome::PASS;
        }

        let pushing_out = (sample.delta > 0.0 && sample.at_top && sample.bounce_top)
            || (sample.delta < 0.0 && sample.at_bottom && sample.bounce_bottom);

        match self.gesture {
            Gesture::Finger => {
                if self.stretch != 0.0 {
                    let next = self.stretch + sample.delta;
                    if next == 0.0 || next.signum() != self.stretch.signum() {
                        // The pull reversed past rest: the remainder is a
                        // real scroll again.
                        self.stretch = 0.0;
                        self.offset = 0.0;
                        return WheelOutcome {
                            consumed: true,
                            release: next,
                        };
                    }
                    self.stretch = next;
                    self.offset = rubber_band(self.stretch, sample.viewport);
                    return WheelOutcome::TAKEN;
                }
                if pushing_out {
                    self.stretch = sample.delta;
                    self.offset = rubber_band(self.stretch, sample.viewport);
                    return WheelOutcome::TAKEN;
                }
                WheelOutcome::PASS
            }
            Gesture::Idle => {
                let heading = match self.motion {
                    Some(Motion::Bounce { velocity, .. }) => Some(velocity.signum()),
                    Some(Motion::Settle { from, .. }) => Some(from.signum()),
                    None => None,
                };
                if let Some(heading) = heading {
                    // Momentum keeps arriving while the content is still
                    // returning; swallow the part aimed at the edge so it
                    // cannot restart the motion from zero mid-flight.
                    return if heading == sample.delta.signum() {
                        WheelOutcome::TAKEN
                    } else {
                        WheelOutcome::PASS
                    };
                }
                if pushing_out && sample.delta != 0.0 {
                    let dt = self
                        .last_event
                        .map(|last| now.saturating_duration_since(last).as_secs_f32())
                        .unwrap_or(1.0 / 60.0)
                        .clamp(0.004, 0.05);
                    let limit =
                        MAX_BOUNCE_FRACTION * sample.viewport * BOUNCE_OMEGA * std::f32::consts::E;
                    let velocity = (sample.delta / dt).clamp(-limit, limit);
                    self.motion = Some(Motion::Bounce {
                        velocity,
                        since: now,
                    });
                    return WheelOutcome::TAKEN;
                }
                WheelOutcome::PASS
            }
        }
    }

    /// Advances any settle or bounce to `now` and returns the offset to draw.
    pub fn sample(&mut self, now: Instant) -> f32 {
        match self.motion {
            Some(Motion::Settle { from, since }) => {
                let t = now.saturating_duration_since(since).as_secs_f32();
                let x = from * (1.0 + SPRING_OMEGA * t) * (-SPRING_OMEGA * t).exp();
                if x.abs() < 0.5 {
                    self.motion = None;
                    self.offset = 0.0;
                } else {
                    self.offset = x;
                }
            }
            Some(Motion::Bounce { velocity, since }) => {
                let t = now.saturating_duration_since(since).as_secs_f32();
                let x = velocity * t * (-BOUNCE_OMEGA * t).exp();
                if t > 1.0 / BOUNCE_OMEGA && x.abs() < 0.5 {
                    self.motion = None;
                    self.offset = 0.0;
                } else {
                    self.offset = x;
                }
            }
            None => {}
        }
        self.offset
    }
}

// ---------------------------------------------------------------------------
// Scroll targets
// ---------------------------------------------------------------------------

/// Anything a [`ScrollArea`] can indicate and drive: GPUI's two scroll
/// handles, or an adapter over a bespoke scroller such as the terminal's
/// scrollback viewport.
pub trait ScrollTarget: 'static {
    /// The current offset in GPUI's convention: zero at the top, growing
    /// negative as the content scrolls down.
    fn offset(&self) -> Point<Pixels>;
    /// How far the content can scroll; `offset.y` ranges over `-max.y..=0`.
    fn max_offset(&self) -> Point<Pixels>;
    /// Scrolls to `offset` (the area clamps before calling).
    fn set_offset(&self, offset: Point<Pixels>, window: &mut Window, cx: &mut App);
    /// Whether the top and bottom edges rubber-band. Both by default; a
    /// terminal keeps its live edge rigid.
    fn bounce_edges(&self) -> (bool, bool) {
        (true, true)
    }
}

impl ScrollTarget for ScrollHandle {
    fn offset(&self) -> Point<Pixels> {
        ScrollHandle::offset(self)
    }

    fn max_offset(&self) -> Point<Pixels> {
        ScrollHandle::max_offset(self)
    }

    fn set_offset(&self, offset: Point<Pixels>, _window: &mut Window, _cx: &mut App) {
        ScrollHandle::set_offset(self, offset);
    }
}

impl ScrollTarget for UniformListScrollHandle {
    fn offset(&self) -> Point<Pixels> {
        self.0.borrow().base_handle.offset()
    }

    fn max_offset(&self) -> Point<Pixels> {
        self.0.borrow().base_handle.max_offset()
    }

    fn set_offset(&self, offset: Point<Pixels>, _window: &mut Window, _cx: &mut App) {
        self.0.borrow().base_handle.set_offset(offset);
    }
}

// ---------------------------------------------------------------------------
// Scroller state
// ---------------------------------------------------------------------------

/// How long the overlay knob lingers after the content stops moving.
const LINGER: Duration = Duration::from_millis(1000);
/// Fade-in when the knob appears.
const FADE_IN: Duration = Duration::from_millis(120);
/// Fade-out after the linger.
const FADE_OUT: Duration = Duration::from_millis(300);
/// Overlay hit region and expanded track width, matching `NSScroller`.
const OVERLAY_STRIP: f32 = 16.0;
/// Knob width at rest and while the pointer is on the strip.
const THUMB_REST: f32 = 7.0;
const THUMB_EXPANDED: f32 = 11.0;
/// Trailing inset of the knob from the viewport edge.
const THUMB_INSET_REST: f32 = 3.0;
const THUMB_INSET_EXPANDED: f32 = 2.5;
/// Space between the knob's travel and the viewport's top and bottom.
const TRAVEL_INSET: f32 = 3.0;
/// Legacy knob width inside its 15pt track.
const LEGACY_THUMB: f32 = 9.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pin {
    None,
    Shown,
    Expanded,
}

impl Pin {
    /// `DIRI_VISUAL_SCROLLBAR=1` holds the overlay knob fully shown for
    /// screenshots; `=hover` also expands it into its track.
    fn from_env() -> Self {
        match std::env::var("DIRI_VISUAL_SCROLLBAR").ok().as_deref() {
            Some("hover") | Some("expanded") => Self::Expanded,
            Some(value) if !value.is_empty() && value != "0" => Self::Shown,
            _ => Self::None,
        }
    }
}

#[derive(Debug)]
struct ScrollerInner {
    last_offset: Option<Point<Pixels>>,
    last_activity: Option<Instant>,
    revealed_since: Option<Instant>,
    hovered: bool,
    drag: Option<Pixels>,
    wake_due: Option<Instant>,
    overscroll: Overscroll,
    pin: Pin,
}

/// Per-list scroller state: fade timing, hover, drag and the rubber band.
/// Cheap to clone; store one next to each scroll handle.
#[derive(Clone, Debug)]
pub struct ScrollerState(Rc<RefCell<ScrollerInner>>);

impl Default for ScrollerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollerState {
    #[must_use]
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(ScrollerInner {
            last_offset: None,
            last_activity: None,
            revealed_since: None,
            hovered: false,
            drag: None,
            wake_due: None,
            overscroll: Overscroll::new(),
            pin: Pin::from_env(),
        })))
    }

    /// Shows the knob now, as if the content had just moved. Callers that
    /// resize or repopulate a list can flash it the way AppKit does.
    pub fn flash(&self) {
        self.0.borrow_mut().last_activity = Some(Instant::now());
    }

    /// The content's rubber-band displacement, for callers that paint
    /// alongside the list.
    #[must_use]
    pub fn overscroll(&self) -> f32 {
        self.0.borrow().overscroll.offset()
    }

    /// True while the pointer is on the scroller strip or dragging the knob.
    #[must_use]
    pub fn is_engaged(&self) -> bool {
        let inner = self.0.borrow();
        inner.hovered || inner.drag.is_some()
    }
}

/// Resolved opacity for one frame plus whether the next frame must be drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Visibility {
    opacity: f32,
    animating: bool,
    /// When a currently shown knob will start fading unless activity
    /// continues; the area schedules a wake for it.
    hide_at: Option<Instant>,
}

impl ScrollerInner {
    fn visibility(
        &mut self,
        now: Instant,
        style: ScrollerStyle,
        reduce_motion: bool,
    ) -> Visibility {
        let held = self.hovered
            || self.drag.is_some()
            || style == ScrollerStyle::Legacy
            || self.pin != Pin::None;
        let active_until = self.last_activity.map(|at| at + LINGER);
        let wants_shown = held || active_until.is_some_and(|until| now < until);
        if wants_shown {
            let since = *self.revealed_since.get_or_insert(now);
            let opacity = if reduce_motion || held {
                1.0
            } else {
                (now.saturating_duration_since(since).as_secs_f32() / FADE_IN.as_secs_f32())
                    .min(1.0)
            };
            return Visibility {
                opacity,
                animating: opacity < 1.0,
                hide_at: if held { None } else { active_until },
            };
        }
        let Some(until) = active_until else {
            self.revealed_since = None;
            return Visibility {
                opacity: 0.0,
                animating: false,
                hide_at: None,
            };
        };
        if reduce_motion || self.revealed_since.is_none() {
            self.revealed_since = None;
            return Visibility {
                opacity: 0.0,
                animating: false,
                hide_at: None,
            };
        }
        let faded = now.saturating_duration_since(until).as_secs_f32() / FADE_OUT.as_secs_f32();
        let opacity = (1.0 - faded).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            self.revealed_since = None;
        }
        Visibility {
            opacity,
            animating: opacity > 0.0,
            hide_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Scroll area element
// ---------------------------------------------------------------------------

/// Wraps a scrolling element with an overlay scroller and rubber-band
/// overscroll. Size it the way the scrolling element used to be sized and
/// give the child `size_full()`.
pub struct ScrollArea {
    state: ScrollerState,
    target: Rc<dyn ScrollTarget>,
    colors: SemanticColors,
    child: AnyElement,
    style: StyleRefinement,
}

/// Builds a [`ScrollArea`] around `child`, which must be the element that
/// `target` scrolls.
pub fn scroll_area(
    state: &ScrollerState,
    target: impl ScrollTarget,
    colors: SemanticColors,
    child: impl IntoElement,
) -> ScrollArea {
    ScrollArea {
        state: state.clone(),
        target: Rc::new(target),
        colors,
        child: child.into_any_element(),
        style: StyleRefinement::default(),
    }
}

impl Styled for ScrollArea {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for ScrollArea {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Everything paint needs that prepaint worked out.
pub struct ScrollAreaPrepaint {
    area: Hitbox,
    thumb: Option<Hitbox>,
    layout: Option<ScrollerLayout>,
    opacity: f32,
    expanded: bool,
    style: ScrollerStyle,
}

#[derive(Clone, Copy, Debug)]
struct ScrollerLayout {
    /// The whole strip along the trailing edge: hover region and track.
    strip: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
    /// Vertical run the knob can travel, in window coordinates.
    travel_top: Pixels,
    travel_length: Pixels,
    thumb_length: Pixels,
}

impl ScrollArea {
    fn scroller_layout(
        viewport: Bounds<Pixels>,
        style: ScrollerStyle,
        expanded: bool,
        geometry: Option<ThumbGeometry>,
    ) -> Option<ScrollerLayout> {
        let (strip_width, thumb_width, thumb_inset) = match style {
            ScrollerStyle::Legacy => (ScrollerStyle::LEGACY_WIDTH, LEGACY_THUMB, 3.0),
            ScrollerStyle::Overlay if expanded => {
                (OVERLAY_STRIP, THUMB_EXPANDED, THUMB_INSET_EXPANDED)
            }
            ScrollerStyle::Overlay => (OVERLAY_STRIP, THUMB_REST, THUMB_INSET_REST),
        };
        let strip = Bounds::new(
            point(viewport.right() - px(strip_width), viewport.top()),
            size(px(strip_width), viewport.size.height),
        );
        let travel_top = viewport.top() + px(TRAVEL_INSET);
        let travel_length = viewport.size.height - px(TRAVEL_INSET * 2.0);
        let thumb_left = viewport.right() - px(thumb_inset) - px(thumb_width);
        let thumb = match geometry {
            Some(geometry) => Bounds::new(
                point(thumb_left, travel_top + px(geometry.start)),
                size(px(thumb_width), px(geometry.length)),
            ),
            None if style == ScrollerStyle::Legacy => Bounds::new(
                point(thumb_left, travel_top),
                size(px(thumb_width), px(0.0)),
            ),
            None => return None,
        };
        Some(ScrollerLayout {
            strip,
            thumb,
            travel_top,
            travel_length,
            thumb_length: thumb.size.height,
        })
    }

    fn scroll_to_thumb_top(
        target: &dyn ScrollTarget,
        layout: &ScrollerLayout,
        thumb_top: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let travel = f32::from(layout.travel_length - layout.thumb_length).max(0.0);
        let fraction = if travel <= 0.0 {
            0.0
        } else {
            (f32::from(thumb_top - layout.travel_top) / travel).clamp(0.0, 1.0)
        };
        let max = target.max_offset();
        let offset = target.offset();
        target.set_offset(point(offset.x, -max.y * fraction), window, cx);
    }

    fn schedule_wake(state: &ScrollerState, due: Instant, window: &Window, cx: &mut App) {
        {
            let mut inner = state.0.borrow_mut();
            if inner
                .wake_due
                .is_some_and(|scheduled| scheduled <= due && scheduled > Instant::now())
            {
                return;
            }
            inner.wake_due = Some(due);
        }
        let view = window.current_view();
        let delay = due.saturating_duration_since(Instant::now()) + Duration::from_millis(1);
        let state = state.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            state.0.borrow_mut().wake_due = None;
            cx.update(|cx| cx.notify(view));
        })
        .detach();
    }
}

impl Element for ScrollArea {
    type RequestLayoutState = ();
    type PrepaintState = ScrollAreaPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child_layout = self.child.request_layout(window, cx);
        let mut style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            ..Style::default()
        };
        style.refine(&self.style);
        style.padding.right = scroller_style(cx).reserved_width().into();
        (window.request_layout(style, [child_layout], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        let style = scroller_style(cx);

        let overscroll = {
            let mut inner = self.state.0.borrow_mut();
            if reduce_motion {
                inner.overscroll.reset();
            }
            let offset = inner.overscroll.sample(now);
            if inner.overscroll.is_animating() {
                window.request_animation_frame();
            }
            offset
        };

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.with_element_offset(point(px(0.0), px(overscroll)), |window| {
                self.child.prepaint(window, cx);
            });
        });

        let offset = self.target.offset();
        let max = self.target.max_offset();
        let viewport = bounds;
        let content_height = f32::from(viewport.size.height + max.y);
        let geometry = thumb_geometry(
            f32::from(viewport.size.height) - TRAVEL_INSET * 2.0,
            f32::from(viewport.size.height),
            content_height,
            -f32::from(offset.y),
            overscroll,
        );

        let (visibility, expanded) = {
            let mut inner = self.state.0.borrow_mut();
            if inner.last_offset.is_some_and(|last| last != offset) {
                inner.last_activity = Some(now);
            }
            inner.last_offset = Some(offset);
            if geometry.is_none() && inner.drag.is_some() {
                inner.drag = None;
            }
            let visibility = if geometry.is_none() && style == ScrollerStyle::Overlay {
                inner.revealed_since = None;
                Visibility {
                    opacity: 0.0,
                    animating: false,
                    hide_at: None,
                }
            } else {
                inner.visibility(now, style, reduce_motion)
            };
            let expanded = inner.hovered || inner.drag.is_some() || inner.pin == Pin::Expanded;
            (visibility, expanded)
        };

        if visibility.animating {
            window.request_animation_frame();
        } else if let Some(due) = visibility.hide_at {
            Self::schedule_wake(&self.state, due, window, cx);
        }

        let layout = Self::scroller_layout(viewport, style, expanded, geometry);
        let area = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        // While the knob is out, the pointer over it (or over its expanded
        // track) belongs to the scroller: rows beneath must not light up.
        let thumb = layout
            .filter(|_| visibility.opacity > 0.0 && geometry.is_some())
            .map(|layout| {
                let region = if expanded { layout.strip } else { layout.thumb };
                window.insert_hitbox(region, HitboxBehavior::BlockMouseExceptScroll)
            });

        ScrollAreaPrepaint {
            area,
            thumb,
            layout,
            opacity: visibility.opacity,
            expanded,
            style,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.child.paint(window, cx);
        });

        let look = ScrollerLook::new(self.colors, prepaint.style, prepaint.expanded);
        if let Some(layout) = prepaint.layout
            && prepaint.opacity > 0.0
        {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                if let Some(track) = look.track {
                    window.paint_quad(quad(
                        layout.strip,
                        Corners::default(),
                        Hsla::from(track.fill.opacity(prepaint.opacity)),
                        Edges {
                            left: px(1.0),
                            ..Default::default()
                        },
                        Hsla::from(track.stroke.opacity(prepaint.opacity)),
                        gpui::BorderStyle::Solid,
                    ));
                }
                if f32::from(layout.thumb.size.height) > 0.0 {
                    let radius = layout.thumb.size.width / 2.0;
                    window.paint_quad(
                        fill(
                            layout.thumb,
                            Hsla::from(look.thumb.opacity(prepaint.opacity)),
                        )
                        .corner_radii(Corners::all(radius)),
                    );
                }
            });
        }
        if let Some(thumb) = &prepaint.thumb {
            window.set_cursor_style(CursorStyle::Arrow, thumb);
        }

        let view = window.current_view();
        let state = self.state.clone();
        let target = Rc::clone(&self.target);
        let area = prepaint.area.clone();
        let layout = prepaint.layout;
        let reduce_motion = cx.reduce_motion();
        let scrollable = layout.is_some_and(|layout| f32::from(layout.thumb_length) > 0.0);

        // Pointer on the strip reveals and widens the knob; dragging moves it.
        {
            let state = state.clone();
            let target = Rc::clone(&target);
            let area = area.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                let dragging = state.0.borrow().drag;
                if let Some(grab) = dragging {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    if let Some(layout) = layout {
                        Self::scroll_to_thumb_top(
                            target.as_ref(),
                            &layout,
                            event.position.y - grab,
                            window,
                            cx,
                        );
                        state.0.borrow_mut().last_activity = Some(Instant::now());
                        cx.notify(view);
                    }
                    cx.stop_propagation();
                    return;
                }
                if phase != DispatchPhase::Bubble {
                    return;
                }
                // `should_handle_scroll` is plain hit-test membership: unlike
                // `is_hovered` it stays true under the scroller's own
                // blocking hitbox, which sits above the area.
                let on_strip = scrollable
                    && area.should_handle_scroll(window)
                    && layout.is_some_and(|layout| layout.strip.contains(&event.position));
                let mut inner = state.0.borrow_mut();
                if inner.hovered != on_strip {
                    inner.hovered = on_strip;
                    inner.last_activity = Some(Instant::now());
                    drop(inner);
                    cx.notify(view);
                }
            });
        }
        {
            let state = state.clone();
            let target = Rc::clone(&target);
            let area = area.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !scrollable
                    || !area.should_handle_scroll(window)
                {
                    return;
                }
                let Some(layout) = layout else { return };
                if !layout.strip.contains(&event.position) {
                    return;
                }
                let mut inner = state.0.borrow_mut();
                if layout.thumb.contains(&event.position) {
                    inner.drag = Some(event.position.y - layout.thumb.top());
                } else {
                    // Jump so the knob centres on the click, then drag from it.
                    let half = layout.thumb_length / 2.0;
                    drop(inner);
                    Self::scroll_to_thumb_top(
                        target.as_ref(),
                        &layout,
                        event.position.y - half,
                        window,
                        cx,
                    );
                    inner = state.0.borrow_mut();
                    inner.drag = Some(half);
                }
                inner.last_activity = Some(Instant::now());
                drop(inner);
                cx.notify(view);
                cx.stop_propagation();
            });
        }
        {
            let state = state.clone();
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                let mut inner = state.0.borrow_mut();
                if inner.drag.take().is_some() {
                    inner.last_activity = Some(Instant::now());
                    drop(inner);
                    cx.notify(view);
                }
            });
        }

        // Rubber band: claim wheel travel past an edge before the scrolling
        // child adds it to its (soon to be clamped) offset.
        {
            let state = state.clone();
            let target = Rc::clone(&target);
            let viewport_height = f32::from(bounds.size.height);
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture
                    || reduce_motion
                    || !area.should_handle_scroll(window)
                {
                    return;
                }
                let (delta, precise) = match event.delta {
                    ScrollDelta::Pixels(delta) => (f32::from(delta.y), true),
                    ScrollDelta::Lines(delta) => (delta.y, false),
                };
                let offset = target.offset();
                let max = target.max_offset();
                let (bounce_top, bounce_bottom) = target.bounce_edges();
                let outcome = {
                    let mut inner = state.0.borrow_mut();
                    inner.overscroll.wheel(
                        WheelSample {
                            delta,
                            precise,
                            phase: event.touch_phase,
                            at_top: offset.y >= px(0.0),
                            at_bottom: offset.y <= -max.y,
                            bounce_top,
                            bounce_bottom,
                            viewport: viewport_height,
                        },
                        Instant::now(),
                    )
                };
                if outcome.release != 0.0 {
                    let y = (offset.y + px(outcome.release)).clamp(-max.y, px(0.0));
                    target.set_offset(point(offset.x, y), window, cx);
                }
                if outcome.consumed {
                    state.0.borrow_mut().last_activity = Some(Instant::now());
                    cx.notify(view);
                    cx.stop_propagation();
                }
            });
        }
    }
}

/// Colours for one frame of the scroller.
#[derive(Clone, Copy, Debug)]
struct ScrollerLook {
    thumb: Rgba,
    track: Option<TrackLook>,
}

#[derive(Clone, Copy, Debug)]
struct TrackLook {
    fill: Rgba,
    stroke: Rgba,
}

impl ScrollerLook {
    fn new(colors: SemanticColors, style: ScrollerStyle, expanded: bool) -> Self {
        let dark = colors.appearance == Appearance::Dark;
        let hairline = if dark {
            crate::rgba_f32(1.0, 1.0, 1.0, 0.08)
        } else {
            crate::rgba_f32(0.0, 0.0, 0.0, 0.10)
        };
        match style {
            ScrollerStyle::Overlay => {
                let (rest, hover) = if dark { (0.50, 0.62) } else { (0.45, 0.58) };
                Self {
                    thumb: colors.primary.alpha(if expanded { hover } else { rest }),
                    // AppKit's hovered track: a neutral tint that reads on
                    // any surface, darker on dark and whiter on light.
                    track: expanded.then(|| TrackLook {
                        fill: if dark {
                            crate::rgba_f32(0.0, 0.0, 0.0, 0.40)
                        } else {
                            crate::rgba_f32(1.0, 1.0, 1.0, 0.85)
                        },
                        stroke: hairline,
                    }),
                }
            }
            ScrollerStyle::Legacy => {
                let (rest, hover) = if dark { (0.36, 0.46) } else { (0.28, 0.38) };
                let fill = colors.background.blend(colors.primary.alpha(0.02));
                Self {
                    thumb: colors.primary.alpha(if expanded { hover } else { rest }),
                    track: Some(TrackLook {
                        fill,
                        stroke: hairline,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-3;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    #[test]
    fn content_that_fits_has_no_thumb() {
        assert_eq!(thumb_geometry(200.0, 300.0, 300.0, 0.0, 0.0), None);
        assert_eq!(thumb_geometry(200.0, 300.0, 100.0, 0.0, 0.0), None);
        assert_eq!(thumb_geometry(0.0, 300.0, 900.0, 0.0, 0.0), None);
    }

    #[test]
    fn thumb_length_is_the_visible_fraction_of_the_track() {
        let thumb = thumb_geometry(300.0, 300.0, 900.0, 0.0, 0.0).unwrap();
        assert!(close(thumb.length, 100.0), "{thumb:?}");
        assert!(close(thumb.start, 0.0), "{thumb:?}");
    }

    #[test]
    fn thumb_travels_the_track_as_the_content_scrolls() {
        let max_scroll = 900.0 - 300.0;
        let halfway = thumb_geometry(300.0, 300.0, 900.0, max_scroll / 2.0, 0.0).unwrap();
        assert!(close(halfway.start, 100.0), "{halfway:?}");
        let end = thumb_geometry(300.0, 300.0, 900.0, max_scroll, 0.0).unwrap();
        assert!(close(end.end(), 300.0), "{end:?}");
        let past = thumb_geometry(300.0, 300.0, 900.0, max_scroll * 3.0, 0.0).unwrap();
        assert_eq!(past, end);
    }

    #[test]
    fn a_huge_document_keeps_a_grabbable_thumb() {
        let thumb = thumb_geometry(300.0, 300.0, 300_000.0, 0.0, 0.0).unwrap();
        assert!(close(thumb.length, MIN_THUMB_LENGTH), "{thumb:?}");
        let end = thumb_geometry(300.0, 300.0, 300_000.0, 299_700.0, 0.0).unwrap();
        assert!(close(end.end(), 300.0), "{end:?}");
    }

    #[test]
    fn a_short_track_never_asks_for_a_thumb_longer_than_itself() {
        let thumb = thumb_geometry(12.0, 300.0, 3000.0, 0.0, 0.0).unwrap();
        assert!(thumb.length <= 12.0, "{thumb:?}");
    }

    #[test]
    fn overscroll_shrinks_the_thumb_and_pins_it_to_the_edge() {
        let top = thumb_geometry(300.0, 300.0, 900.0, 0.0, 40.0).unwrap();
        assert!(close(top.start, 0.0), "{top:?}");
        assert!(close(top.length, 60.0), "{top:?}");
        let bottom = thumb_geometry(300.0, 300.0, 900.0, 600.0, -40.0).unwrap();
        assert!(close(bottom.end(), 300.0), "{bottom:?}");
        assert!(close(bottom.length, 60.0), "{bottom:?}");
        let extreme = thumb_geometry(300.0, 300.0, 900.0, 0.0, 500.0).unwrap();
        assert!(close(extreme.length, MIN_THUMB_LENGTH), "{extreme:?}");
    }

    #[test]
    fn rubber_band_resists_and_saturates() {
        let dim = 400.0;
        assert_eq!(rubber_band(0.0, dim), 0.0);
        let small = rubber_band(50.0, dim);
        assert!(small > 0.0 && small < 50.0, "{small}");
        let large = rubber_band(4000.0, dim);
        assert!(large < dim, "{large}");
        assert!(large > rubber_band(2000.0, dim));
        assert!(close(rubber_band(-50.0, dim), -small));
        assert_eq!(rubber_band(50.0, 0.0), 0.0);
    }

    #[test]
    fn unrubber_inverts_the_band() {
        let dim = 400.0;
        for stretch in [10.0, 120.0, 800.0, -35.0] {
            let back = unrubber(rubber_band(stretch, dim), dim);
            assert!((back - stretch).abs() < 0.05, "{stretch} -> {back}");
        }
        assert_eq!(unrubber(0.0, dim), 0.0);
    }

    fn sample(delta: f32, phase: TouchPhase, at_top: bool, at_bottom: bool) -> WheelSample {
        WheelSample {
            delta,
            precise: true,
            phase,
            at_top,
            at_bottom,
            bounce_top: true,
            bounce_bottom: true,
            viewport: 400.0,
        }
    }

    #[test]
    fn a_finger_pull_past_the_top_is_taken_and_springs_back() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        assert_eq!(
            band.wheel(sample(0.0, TouchPhase::Started, true, false), t0),
            WheelOutcome::PASS
        );
        // Scrolling away from the edge is the list's business.
        assert_eq!(
            band.wheel(sample(-10.0, TouchPhase::Moved, true, false), t0),
            WheelOutcome::PASS
        );
        let taken = band.wheel(sample(30.0, TouchPhase::Moved, true, false), t0);
        assert!(taken.consumed && taken.release == 0.0, "{taken:?}");
        assert!(
            band.offset() > 0.0 && band.offset() < 30.0,
            "{}",
            band.offset()
        );
        band.wheel(sample(30.0, TouchPhase::Moved, false, false), t0);
        let pulled = band.offset();
        assert!(pulled > 30.0 * 0.5, "{pulled}");

        band.wheel(sample(0.0, TouchPhase::Ended, true, false), t0);
        assert!(band.is_animating());
        let mid = band.sample(t0 + Duration::from_millis(60));
        assert!(mid > 0.0 && mid < pulled, "{mid} vs {pulled}");
        let settled = band.sample(t0 + Duration::from_millis(600));
        assert_eq!(settled, 0.0);
        assert!(!band.is_animating());
    }

    #[test]
    fn reversing_a_pull_hands_the_remainder_back_to_the_list() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        band.wheel(sample(0.0, TouchPhase::Started, true, false), t0);
        band.wheel(sample(20.0, TouchPhase::Moved, true, false), t0);
        // Ease off by more than the stretch: 20 of it undoes the pull, the
        // remaining 15 scrolls the list.
        let outcome = band.wheel(sample(-35.0, TouchPhase::Moved, true, false), t0);
        assert!(outcome.consumed);
        assert!(close(outcome.release, -15.0), "{outcome:?}");
        assert_eq!(band.offset(), 0.0);
        // Now that the band is slack, further travel passes straight through.
        assert_eq!(
            band.wheel(sample(-5.0, TouchPhase::Moved, false, false), t0),
            WheelOutcome::PASS
        );
    }

    #[test]
    fn momentum_into_an_edge_bounces_and_swallows_the_rest_of_the_run() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        // No Started: the fingers already lifted, this is the momentum tail.
        let first = band.wheel(sample(-40.0, TouchPhase::Moved, false, false), t0);
        assert_eq!(first, WheelOutcome::PASS);
        let hit = band.wheel(
            sample(-40.0, TouchPhase::Moved, false, true),
            t0 + Duration::from_millis(16),
        );
        assert!(hit.consumed, "{hit:?}");
        assert!(band.is_animating());
        let peak = band.sample(t0 + Duration::from_millis(16 + 70));
        assert!(peak < 0.0, "bounce should dip past the bottom: {peak}");
        assert!(peak.abs() <= 0.22 * 400.0 + 0.5, "{peak}");
        // Momentum still aimed at the edge is absorbed; a reversal passes.
        let more = band.wheel(
            sample(-30.0, TouchPhase::Moved, false, true),
            t0 + Duration::from_millis(32),
        );
        assert!(more.consumed);
        let reversed = band.wheel(
            sample(30.0, TouchPhase::Moved, false, true),
            t0 + Duration::from_millis(48),
        );
        assert!(!reversed.consumed);
        assert_eq!(band.sample(t0 + Duration::from_secs(2)), 0.0);
        assert!(!band.is_animating());
    }

    #[test]
    fn momentum_during_a_settle_cannot_restart_it_from_zero() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        band.wheel(sample(0.0, TouchPhase::Started, true, false), t0);
        band.wheel(sample(60.0, TouchPhase::Moved, true, false), t0);
        band.wheel(sample(0.0, TouchPhase::Ended, true, false), t0);
        let mid = band.sample(t0 + Duration::from_millis(30));
        assert!(mid > 0.0);
        // Momentum still pushing into the top edge is absorbed and the
        // settle keeps its trajectory instead of snapping to a fresh bounce.
        let outcome = band.wheel(
            sample(25.0, TouchPhase::Moved, true, false),
            t0 + Duration::from_millis(30),
        );
        assert!(outcome.consumed);
        let next = band.sample(t0 + Duration::from_millis(31));
        assert!((next - mid).abs() < 5.0, "{mid} -> {next}");
        assert!(matches!(band.motion, Some(Motion::Settle { .. })));
    }

    #[test]
    fn mouse_wheels_never_rubber_band() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        let mut wheel = sample(40.0, TouchPhase::Moved, true, false);
        wheel.precise = false;
        assert_eq!(band.wheel(wheel, t0), WheelOutcome::PASS);
        assert_eq!(band.offset(), 0.0);
    }

    #[test]
    fn a_rigid_edge_does_not_give() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        band.wheel(sample(0.0, TouchPhase::Started, false, true), t0);
        let mut push = sample(-40.0, TouchPhase::Moved, false, true);
        push.bounce_bottom = false;
        assert_eq!(band.wheel(push, t0), WheelOutcome::PASS);
        assert_eq!(band.offset(), 0.0);
    }

    #[test]
    fn a_new_gesture_picks_the_band_up_where_it_is() {
        let t0 = Instant::now();
        let mut band = Overscroll::new();
        band.wheel(sample(0.0, TouchPhase::Started, true, false), t0);
        band.wheel(sample(60.0, TouchPhase::Moved, true, false), t0);
        band.wheel(sample(0.0, TouchPhase::Ended, true, false), t0);
        let mid = band.sample(t0 + Duration::from_millis(40));
        assert!(mid > 0.0);
        // Fingers land again mid-settle: no snap, the pull continues.
        let restart = band.wheel(
            sample(0.0, TouchPhase::Started, true, false),
            t0 + Duration::from_millis(40),
        );
        assert!(restart.consumed);
        assert!(!band.is_animating());
        assert!(close(band.offset(), mid), "{} vs {mid}", band.offset());
        band.wheel(
            sample(10.0, TouchPhase::Moved, true, false),
            t0 + Duration::from_millis(56),
        );
        assert!(band.offset() > mid);
    }

    #[test]
    fn scroller_visibility_lingers_then_fades() {
        let t0 = Instant::now();
        let mut inner = ScrollerInner {
            last_offset: None,
            last_activity: Some(t0),
            revealed_since: None,
            hovered: false,
            drag: None,
            wake_due: None,
            overscroll: Overscroll::new(),
            pin: Pin::None,
        };
        // The first frame after activity starts the fade-in; a frame later
        // the knob is fully shown and knows when it will start hiding.
        let revealing = inner.visibility(t0, ScrollerStyle::Overlay, false);
        assert_eq!(revealing.opacity, 0.0);
        assert!(revealing.animating);
        let shown = inner.visibility(t0 + FADE_IN, ScrollerStyle::Overlay, false);
        assert_eq!(shown.opacity, 1.0);
        assert!(!shown.animating);
        assert_eq!(shown.hide_at, Some(t0 + LINGER));
        let fading = inner.visibility(t0 + LINGER + FADE_OUT / 2, ScrollerStyle::Overlay, false);
        assert!(fading.animating);
        assert!(fading.opacity > 0.4 && fading.opacity < 0.6, "{fading:?}");
        let gone = inner.visibility(t0 + LINGER + FADE_OUT * 2, ScrollerStyle::Overlay, false);
        assert_eq!(gone.opacity, 0.0);
        assert!(!gone.animating);

        // Hovering the strip holds it fully shown with no fade pending.
        inner.hovered = true;
        let held = inner.visibility(t0 + LINGER * 5, ScrollerStyle::Overlay, false);
        assert_eq!(held.opacity, 1.0);
        assert_eq!(held.hide_at, None);

        // Reduce Motion: shown or hidden, never in between.
        inner.hovered = false;
        inner.last_activity = Some(t0 + LINGER * 5);
        let instant = inner.visibility(t0 + LINGER * 5, ScrollerStyle::Overlay, true);
        assert_eq!(instant.opacity, 1.0);
        assert!(!instant.animating);
        let off = inner.visibility(t0 + LINGER * 7, ScrollerStyle::Overlay, true);
        assert_eq!(off.opacity, 0.0);
        assert!(!off.animating);

        // Legacy scrollers never hide.
        inner.last_activity = None;
        let legacy = inner.visibility(t0 + LINGER * 9, ScrollerStyle::Legacy, false);
        assert_eq!(legacy.opacity, 1.0);
    }

    #[test]
    fn legacy_style_reserves_its_track_width() {
        assert_eq!(ScrollerStyle::Overlay.reserved_width(), px(0.0));
        assert_eq!(
            ScrollerStyle::Legacy.reserved_width(),
            px(ScrollerStyle::LEGACY_WIDTH)
        );
    }
}
