//! The cursor of a pane that does not have the keyboard.
//!
//! A focused pane fills its cursor block; any other pane outlines it, so the
//! insertion point stays findable and a split workbench shows at a glance
//! which pane is live. The outline is a static marker: it never blinks or
//! glides, and a pane at rest with a hollow cursor schedules no frames.
//!
//! Like `cursor_motion`, nothing here reads a clock. The change between the
//! two states is a short fade of the fill under a constant outline, as a pure
//! function of the time since focus changed.

use std::time::{Duration, Instant};

use gpui::{Bounds, Hsla, PaintQuad, Pixels, point, px, quad, size};

/// Long enough to read as the block emptying rather than being swapped,
/// short enough to be over before the first key lands in the other pane.
pub const FOCUS_MORPH: Duration = Duration::from_millis(120);
/// Logical pixels. One device pixel at 1x, two at 2x.
pub const OUTLINE_WIDTH: f32 = 1.0;

/// Quadratic ease-out. The glide's cubic spends the last third of a fade
/// under two percent of the fill: three frames at 60 Hz that repaint a block
/// nobody can see change. This one is as quick off the mark as a fade needs
/// and every frame of it shows.
#[must_use]
pub fn ease_out(t: f32) -> f32 {
    let inverse = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inverse * inverse
}

/// How much of the block is filled for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FocusFrame {
    /// 1 is the filled block, 0 the bare outline.
    pub fill: f32,
    /// True while the fill is still changing and the next frame is wanted.
    pub morphing: bool,
}

impl FocusFrame {
    pub const FILLED: Self = Self {
        fill: 1.0,
        morphing: false,
    };
    pub const HOLLOW: Self = Self {
        fill: 0.0,
        morphing: false,
    };

    /// The outline is drawn whenever the block is not the plain filled one.
    /// Under a full fill of its own color it could not be seen anyway.
    #[must_use]
    pub fn outlined(self) -> bool {
        self.fill < 1.0
    }
}

#[derive(Clone, Copy, Debug)]
struct Morph {
    from: f32,
    started_at: Instant,
}

/// Per-view focus state of the cursor.
#[derive(Debug, Default)]
pub struct CursorFocus {
    /// `None` until the cursor has been painted once, and again whenever it
    /// stops being painted, so a cursor never fades in from a state nobody
    /// saw.
    focused: Option<bool>,
    morph: Option<Morph>,
    /// Fill opacity as last painted, blink included: a block that loses
    /// focus while dimmed empties from where it was.
    painted: f32,
}

impl CursorFocus {
    /// The cursor is not being painted this frame.
    pub fn note_hidden(&mut self) {
        *self = Self::default();
    }

    /// Records the fill opacity the frame ends up painting.
    pub fn note_painted(&mut self, opacity: f32) {
        self.painted = opacity;
    }

    /// Whether the cursor was last painted for a focused pane, if at all.
    #[cfg(test)]
    pub(crate) fn painted_focused(&self) -> Option<bool> {
        self.focused
    }

    /// Ends a change in flight. A glide takes the block over whole.
    pub fn settle(&mut self) {
        self.morph = None;
    }

    pub fn sample(&mut self, focused: bool, now: Instant, reduce_motion: bool) -> FocusFrame {
        let target = if focused { 1.0 } else { 0.0 };
        match self.focused.replace(focused) {
            Some(previous) if previous != focused => {
                self.morph = Some(Morph {
                    from: self.painted,
                    started_at: now,
                });
            }
            Some(_) => {}
            None => self.morph = None,
        }
        if reduce_motion {
            self.morph = None;
        }
        let Some(morph) = self.morph else {
            return FocusFrame {
                fill: target,
                morphing: false,
            };
        };
        let elapsed = now.saturating_duration_since(morph.started_at);
        if elapsed >= FOCUS_MORPH {
            self.morph = None;
            return FocusFrame {
                fill: target,
                morphing: false,
            };
        }
        let eased = ease_out(elapsed.as_secs_f32() / FOCUS_MORPH.as_secs_f32());
        FocusFrame {
            fill: morph.from + (target - morph.from) * eased,
            morphing: true,
        }
    }
}

/// Cells the cursor block spans at `col` of `row`: both cells of a
/// double-width glyph, so neither the fill nor the outline cuts it in half.
#[must_use]
pub fn cursor_cols(row: &[diri_proto::grid::GridCell], col: u16) -> u16 {
    let wide = row.get(usize::from(col) + 1).is_some_and(|next| {
        next.style
            .contains(diri_proto::grid::TermStyle::WIDE_SPACER)
    });
    if wide { 2 } else { 1 }
}

/// The outline of a cursor block: `block` with its edges moved to the nearest
/// device pixel, stroked inward so it never reaches into a neighboring cell
/// by more than that rounding. Cell edges fall between device pixels (Menlo
/// 13 is 7.83 px wide), where a one-pixel line would smear over two at half
/// strength.
#[must_use]
pub fn outline_quad(block: Bounds<Pixels>, scale_factor: f32, color: impl Into<Hsla>) -> PaintQuad {
    let scale = if scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let snap = |value: Pixels| (f32::from(value) * scale).round() / scale;
    let (left, top) = (snap(block.left()), snap(block.top()));
    let (right, bottom) = (snap(block.right()), snap(block.bottom()));
    let width = (OUTLINE_WIDTH * scale).round().max(1.0) / scale;
    let bounds = Bounds::new(
        point(px(left), px(top)),
        size(px((right - left).max(width)), px((bottom - top).max(width))),
    );
    quad(
        bounds,
        px(0.0),
        gpui::transparent_black(),
        px(width),
        color,
        gpui::BorderStyle::Solid,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::grid::{GridCell, TermStyle};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// One painted frame with no blink on top.
    fn paint(focus: &mut CursorFocus, focused: bool, now: Instant, reduce_motion: bool) {
        let frame = focus.sample(focused, now, reduce_motion);
        focus.note_painted(frame.fill);
    }

    #[test]
    fn a_cursor_first_seen_unfocused_is_hollow_at_once() {
        let mut focus = CursorFocus::default();
        assert_eq!(
            focus.sample(false, Instant::now(), false),
            FocusFrame::HOLLOW
        );
        let mut focus = CursorFocus::default();
        assert_eq!(
            focus.sample(true, Instant::now(), false),
            FocusFrame::FILLED
        );
    }

    #[test]
    fn losing_focus_empties_the_block_then_stops_asking_for_frames() {
        let start = Instant::now();
        let mut focus = CursorFocus::default();
        paint(&mut focus, true, start, false);

        let first = focus.sample(false, start, false);
        assert_eq!(first.fill, 1.0);
        // Still the plain block for this one frame; the outline shows as
        // soon as there is anything less than a full fill over it.
        assert!(first.morphing && !first.outlined());

        // Ease-out: three quarters of the fill is gone by half time.
        let halfway = focus.sample(false, start + FOCUS_MORPH / 2, false);
        assert!((halfway.fill - 0.25).abs() < 1e-3);
        assert!(halfway.morphing);

        let mut previous = 1.0;
        for step in 1..=12 {
            let frame = focus.sample(false, start + ms(step * 10), false);
            assert!(frame.fill <= previous);
            previous = frame.fill;
        }
        assert_eq!(
            focus.sample(false, start + FOCUS_MORPH, false),
            FocusFrame::HOLLOW
        );
        assert_eq!(
            focus.sample(false, start + Duration::from_secs(3600), false),
            FocusFrame::HOLLOW
        );
    }

    #[test]
    fn gaining_focus_fills_and_rests() {
        let start = Instant::now();
        let mut focus = CursorFocus::default();
        paint(&mut focus, false, start, false);
        let first = focus.sample(true, start + ms(500), false);
        assert_eq!(first.fill, 0.0);
        assert!(first.morphing);
        let later = focus.sample(true, start + ms(560), false);
        assert!((later.fill - 0.75).abs() < 1e-3);
        assert_eq!(
            focus.sample(true, start + ms(500) + FOCUS_MORPH, false),
            FocusFrame::FILLED
        );
    }

    #[test]
    fn a_change_of_mind_continues_from_what_is_on_screen() {
        let start = Instant::now();
        let mut focus = CursorFocus::default();
        paint(&mut focus, true, start, false);
        paint(&mut focus, false, start, false);
        let mid = focus.sample(false, start + ms(20), false);
        focus.note_painted(mid.fill);
        let back = focus.sample(true, start + ms(21), false);
        assert_eq!(back.fill, mid.fill);
        let end = focus.sample(true, start + ms(21) + FOCUS_MORPH, false);
        assert_eq!(end, FocusFrame::FILLED);
    }

    #[test]
    fn a_dimmed_block_empties_from_its_dimmed_opacity() {
        let start = Instant::now();
        let mut focus = CursorFocus::default();
        let _ = focus.sample(true, start, false);
        focus.note_painted(0.28);
        assert_eq!(focus.sample(false, start, false).fill, 0.28);
    }

    #[test]
    fn reduce_motion_cuts() {
        let start = Instant::now();
        let mut focus = CursorFocus::default();
        paint(&mut focus, true, start, true);
        assert_eq!(focus.sample(false, start, true), FocusFrame::HOLLOW);
        assert_eq!(focus.sample(true, start + ms(5), true), FocusFrame::FILLED);
    }

    #[test]
    fn a_hidden_cursor_returns_without_a_fade() {
        let start = Instant::now();
        let mut focus = CursorFocus::default();
        paint(&mut focus, true, start, false);
        focus.note_hidden();
        assert_eq!(
            focus.sample(false, start + ms(1), false),
            FocusFrame::HOLLOW
        );
    }

    mod window {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        use diri_proto::grid::{ChangedRow, GridCell, GridUpdate};
        use gpui::{
            Context, IntoElement, ParentElement, Render, Styled, TestAppContext, VisualTestContext,
            Window, div,
        };

        use crate::buffer::GridBuffer;
        use crate::cursor_focus::FOCUS_MORPH;
        use crate::cursor_motion::CursorSchedule;
        use crate::element::TerminalElement;

        struct Pane {
            element: TerminalElement,
            focused: Rc<Cell<bool>>,
            reduce_motion: bool,
        }

        impl Render for Pane {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(
                    self.element
                        .clone()
                        .focused(self.focused.get())
                        .reduce_motion(self.reduce_motion),
                )
            }
        }

        fn terminal() -> TerminalElement {
            let element = TerminalElement::with_buffer(GridBuffer::new(20, 4));
            element.apply_damage(GridUpdate {
                cols: 20,
                rows: 4,
                cursor_col: 3,
                cursor_row: 1,
                cursor_visible: true,
                is_full_snapshot: true,
                changed_rows: (0..4)
                    .map(|y| ChangedRow::new(y, vec![GridCell::BLANK; 20]))
                    .collect(),
            });
            element
        }

        fn pane(
            focused: bool,
            reduce_motion: bool,
            cx: &mut TestAppContext,
        ) -> (TerminalElement, Rc<Cell<bool>>, &mut VisualTestContext) {
            let element = terminal();
            let focus = Rc::new(Cell::new(focused));
            let shown = Pane {
                element: element.clone(),
                focused: Rc::clone(&focus),
                reduce_motion,
            };
            let (_, cx) = cx.add_window_view(move |_, _| shown);
            cx.run_until_parked();
            (element, focus, cx)
        }

        /// Redraw, and report how many frames that draw asked for in turn.
        fn next_frame(cx: &mut VisualTestContext) -> usize {
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.refresh();
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.simulate_next_frame(cx))
        }

        #[gpui::test]
        fn an_unfocused_pane_asks_for_no_frames(cx: &mut TestAppContext) {
            let (element, _, cx) = pane(false, false, cx);
            for _ in 0..3 {
                assert_eq!(next_frame(cx), 0);
                assert_eq!(element.cursor_schedule(), Some(CursorSchedule::Rest));
            }
            // Output moves a hollow cursor like any other; it still rests.
            element.apply_damage(GridUpdate {
                cols: 20,
                rows: 4,
                cursor_col: 4,
                cursor_row: 1,
                cursor_visible: true,
                is_full_snapshot: false,
                changed_rows: vec![ChangedRow::new(1, vec![GridCell::BLANK; 20])],
            });
            assert_eq!(next_frame(cx), 0);
            assert_eq!(element.cursor_schedule(), Some(CursorSchedule::Rest));
        }

        #[gpui::test]
        fn losing_focus_asks_for_frames_only_until_the_block_is_empty(cx: &mut TestAppContext) {
            let (element, focus, cx) = pane(true, false, cx);
            let started = Instant::now();
            focus.set(false);
            assert_eq!(next_frame(cx), 1, "the block starts emptying");
            assert_eq!(element.cursor_schedule(), Some(CursorSchedule::NextFrame));

            // GPUI test windows run on the wall clock.
            std::thread::sleep(
                (FOCUS_MORPH + Duration::from_millis(5)).saturating_sub(started.elapsed()),
            );
            assert_eq!(next_frame(cx), 0, "the ending frame schedules nothing");
            assert_eq!(element.cursor_schedule(), Some(CursorSchedule::Rest));
            assert_eq!(next_frame(cx), 0);
        }

        struct Handled {
            element: TerminalElement,
            focus: gpui::FocusHandle,
        }

        impl Render for Handled {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .child(self.element.clone().focus_handle(self.focus.clone()))
            }
        }

        /// No override: the pane's own focus handle and the window's key
        /// status decide, as they do in the app.
        #[gpui::test]
        fn another_focus_or_an_inactive_window_hollows_the_cursor(cx: &mut TestAppContext) {
            let element = terminal();
            let shown = element.clone();
            let (view, cx) = cx.add_window_view(move |_, cx| Handled {
                element: shown,
                focus: cx.focus_handle(),
            });
            let painted = TerminalElement::cursor_painted_focused;
            view.update_in(cx, |view, window, cx| {
                window.activate_window();
                window.focus(&view.focus, cx);
            });
            cx.run_until_parked();
            assert_eq!(painted(&element), Some(true));

            // Another pane, the palette, a menu: anything else with the keyboard.
            let elsewhere = view.update_in(cx, |_, window, cx| {
                let elsewhere = cx.focus_handle();
                window.focus(&elsewhere, cx);
                elsewhere
            });
            cx.run_until_parked();
            assert_eq!(painted(&element), Some(false));
            view.update_in(cx, |view, window, cx| window.focus(&view.focus, cx));
            cx.run_until_parked();
            assert_eq!(painted(&element), Some(true));
            drop(elsewhere);

            // The focused pane of a window that is not key is hollow too.
            cx.deactivate_window();
            cx.run_until_parked();
            assert_eq!(painted(&element), Some(false));
            view.update_in(cx, |_, window, _| window.activate_window());
            cx.run_until_parked();
            assert_eq!(painted(&element), Some(true));
        }

        #[gpui::test]
        fn the_change_never_touches_the_row_cache(cx: &mut TestAppContext) {
            let (element, focus, cx) = pane(true, false, cx);
            next_frame(cx);
            element.reset_stats();
            for focused in [false, true, false] {
                focus.set(focused);
                next_frame(cx);
            }
            let stats = element.stats();
            assert!(stats.frames > 0);
            assert_eq!(stats.shape_cache_misses, 0);
        }

        #[gpui::test]
        fn reduce_motion_cuts_without_a_frame(cx: &mut TestAppContext) {
            let (element, focus, cx) = pane(true, true, cx);
            focus.set(false);
            assert_eq!(next_frame(cx), 0);
            assert_eq!(element.cursor_schedule(), Some(CursorSchedule::Rest));
        }
    }

    #[test]
    fn the_block_spans_both_cells_of_a_double_width_glyph() {
        let mut row = vec![GridCell::BLANK; 6];
        row[3].style |= TermStyle::WIDE_SPACER;
        assert_eq!(cursor_cols(&row, 0), 1);
        assert_eq!(cursor_cols(&row, 2), 2);
        assert_eq!(cursor_cols(&row, 3), 1);
        assert_eq!(cursor_cols(&row, 5), 1);
        assert_eq!(cursor_cols(&[], 0), 1);
    }

    #[test]
    fn the_outline_lands_on_whole_device_pixels() {
        let block = Bounds::new(
            point(px(10.0 + 7.83 * 3.0), px(12.0)),
            size(px(7.83), px(15.0)),
        );
        for scale in [1.0_f32, 2.0] {
            let outline = outline_quad(block, scale, gpui::red());
            let bounds = outline.bounds;
            for edge in [
                bounds.left(),
                bounds.top(),
                bounds.right(),
                bounds.bottom(),
                outline.border_widths.left,
            ] {
                let device = f32::from(edge) * scale;
                assert!((device - device.round()).abs() < 1e-3, "{device}");
            }
            // Never further than half a device pixel from the block.
            let tolerance = 0.5 / scale + 1e-3;
            assert!(f32::from(bounds.left() - block.left()).abs() <= tolerance);
            assert!(f32::from(bounds.right() - block.right()).abs() <= tolerance);
            assert_eq!(f32::from(outline.border_widths.top) * scale, scale.round());
        }
    }
}
