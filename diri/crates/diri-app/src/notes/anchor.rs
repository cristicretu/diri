//! Scroll anchoring: the row at the top of the viewport holds still while
//! rows above it change height.
//!
//! Off-screen blocks are spacers sized from estimates until they are first
//! laid out. A block entering the layout window above the viewport swaps its
//! estimate for its real height, which moved everything under it, the text
//! being read included: scrolling up a note opened at its end jumped by a
//! line or two every few steps. A resize re-wrapping the rows above does the
//! same. [`Anchored`] wraps the note column: once layout has placed the rows
//! and before anything is positioned, it reads where the held row landed,
//! moves the scroll by the difference, and positions the column in that same
//! frame, so no frame shows the jump. Edits and folds are left alone: there
//! the content really moved, and the caret's autoscroll owns the view.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ScrollHandle, Window, point, px,
};

/// What the anchor carries from one frame to the next.
#[derive(Default)]
pub(super) struct AnchorState {
    /// The row held still, and its top's offset from the column's top
    /// when it was last laid out.
    held: Option<(u64, Pixels)>,
    /// The held row's layout this frame, from its [`Probe`].
    layout: Option<LayoutId>,
    /// Each laid-out row's top this frame, in window coordinates.
    tops: Vec<(u64, Pixels)>,
    /// The content the held offset was measured against: an edit or a fold
    /// since then moves rows for real, and nothing is held across it.
    content: u64,
    /// The column's width when last laid out.
    width: Pixels,
    /// How far this frame's correction moved the column up, for the
    /// elements after it on the page to [`follow`].
    shift: Pixels,
}

pub(super) type ScrollAnchor = Rc<RefCell<AnchorState>>;

impl AnchorState {
    /// The column's width when last laid out.
    pub(super) fn width(&self) -> Pixels {
        self.width
    }

    /// The row to wrap in a [`Probe`] this frame.
    pub(super) fn held(&self) -> Option<u64> {
        self.held.map(|(id, _)| id)
    }

    /// Starts a frame for `content`. A row a fresh frame does not probe is
    /// not on the page, and nothing is held.
    pub(super) fn begin(&mut self, content: u64) {
        if content != self.content {
            self.held = None;
            self.content = content;
        }
        self.layout = None;
        self.tops.clear();
        self.shift = px(0.0);
    }

    /// Records a laid-out row's top, from its prepaint.
    pub(super) fn row(&mut self, id: u64, top: Pixels) {
        self.tops.push((id, top));
    }

    /// The row to hold next frame: the first to start inside the viewport,
    /// or failing that the one the viewport's top cuts through.
    fn choose(&mut self, viewport_top: Pixels, column_top: Pixels) {
        let inside = self
            .tops
            .iter()
            .filter(|(_, top)| *top >= viewport_top)
            .min_by(|a, b| f32::from(a.1).total_cmp(&f32::from(b.1)));
        let above = || {
            self.tops
                .iter()
                .filter(|(_, top)| *top < viewport_top)
                .max_by(|a, b| f32::from(a.1).total_cmp(&f32::from(b.1)))
        };
        self.held = inside
            .or_else(above)
            .map(|&(id, top)| (id, top - column_top));
    }
}

/// The note column, positioned so its held row stays where it was.
pub(super) struct Anchored {
    child: AnyElement,
    state: ScrollAnchor,
    scroll: ScrollHandle,
}

pub(super) fn anchored(
    child: impl IntoElement,
    state: ScrollAnchor,
    scroll: ScrollHandle,
) -> Anchored {
    Anchored {
        child: child.into_any_element(),
        state,
        scroll,
    }
}

impl IntoElement for Anchored {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Anchored {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let drift = {
            let state = self.state.borrow();
            match (state.held, state.layout) {
                (Some((_, was)), Some(layout)) => {
                    window.layout_bounds(layout).top() - bounds.top() - was
                }
                _ => px(0.0),
            }
        };
        // Never past the top of the note: there is nothing above to show.
        let offset = self.scroll.offset();
        let shift = if drift.abs() >= px(0.5) {
            let y = (offset.y - drift).min(px(0.0));
            self.scroll.set_offset(point(offset.x, y));
            offset.y - y
        } else {
            px(0.0)
        };
        window.with_element_offset(point(px(0.0), -shift), |window| {
            self.child.prepaint(window, cx)
        });
        let viewport_top = self.scroll.bounds().top();
        let mut state = self.state.borrow_mut();
        state.width = bounds.size.width;
        state.shift = shift;
        state.choose(viewport_top, bounds.top() - shift);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// The held row, reporting its layout so [`Anchored`] can find it.
pub(super) struct Probe {
    child: AnyElement,
    state: ScrollAnchor,
}

pub(super) fn probe(child: impl IntoElement, state: ScrollAnchor) -> Probe {
    Probe {
        child: child.into_any_element(),
        state,
    }
}

impl IntoElement for Probe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Probe {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let layout = self.child.request_layout(window, cx);
        self.state.borrow_mut().layout = Some(layout);
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// An element below the note column, moved with it by this frame's
/// correction: laid out under the column, it would otherwise paint one frame
/// where the column was.
pub(super) struct Follow {
    child: AnyElement,
    state: ScrollAnchor,
}

pub(super) fn follow(child: impl IntoElement, state: ScrollAnchor) -> Follow {
    Follow {
        child: child.into_any_element(),
        state,
    }
}

impl IntoElement for Follow {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Follow {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let shift = self.state.borrow().shift;
        window.with_element_offset(point(px(0.0), -shift), |window| {
            self.child.prepaint(window, cx)
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}
