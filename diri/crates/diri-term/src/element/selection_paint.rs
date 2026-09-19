//! Turns the selection into what `paint` draws: one shape, plus the sheen
//! while it is running. The sheen sits on the tint and under the text, so
//! glyphs stay crisp. Kept apart from row preparation on purpose: nothing
//! here reads or writes the row cache, so a drag or a sweep reshapes no text.

use gpui::{App, Bounds, PaintQuad, Path, Pixels, Window, fill};

use super::{TerminalElement, mutex_lock, read_lock};
use crate::metrics::CellMetrics;
use crate::scrollback::ScrollbackViewport;
use crate::selection::{SelectionRange, SelectionSpan};
use crate::selection_shape::{Clipping, SelectionShape, snap_to_wide_cells};
use crate::selection_shimmer;

/// What the selection needs from `paint` beyond ordinary overlay quads.
#[derive(Default)]
pub(super) struct SelectionPaint {
    /// A stepped outline. A selection that is one rectangle stays a quad, so
    /// only an outline with inner corners pays for GPUI's path pass.
    path: Option<Path<Pixels>>,
    animating: bool,
}

impl SelectionPaint {
    /// Moves the outline with a reading view that is scrolled by part of a
    /// row. The quads travel with `overlay_quads`; the path has to be told.
    pub(super) fn shift_up(&mut self, shift: Pixels) {
        if let Some(path) = &mut self.path {
            path.bounds.origin.y -= shift;
            for vertex in &mut path.vertices {
                vertex.xy_position.y -= shift;
            }
        }
    }
}

impl TerminalElement {
    /// The spans inside the window, and whether the selection carries on past
    /// the window's first or last row.
    fn visible_selection(
        &self,
        viewport: &ScrollbackViewport,
        visible_rows: usize,
        visible_cols: usize,
    ) -> (Vec<SelectionSpan>, Option<SelectionRange>, Clipping) {
        let selection = mutex_lock(&self.shared.selection);
        let spans = selection.visible_spans(viewport, visible_rows, visible_cols);
        let top = viewport.absolute_row(0);
        let clipping = Clipping {
            top: selection.overlaps_row(top - 1, visible_cols),
            bottom: selection.overlaps_row(top + visible_rows as i64, visible_cols),
        };
        (spans, selection.range(), clipping)
    }

    /// Quads go first into `overlay_quads`, under the find highlights as the
    /// per-row rectangles were: the rounded rectangle when the selection is
    /// one, and the sheen's ramps while it runs.
    #[expect(clippy::too_many_arguments)]
    pub(super) fn prepare_selection(
        &self,
        viewport: &ScrollbackViewport,
        visible_rows: usize,
        visible_cols: usize,
        bounds: Bounds<Pixels>,
        metrics: CellMetrics,
        overlay_quads: &mut Vec<PaintQuad>,
        cx: &App,
    ) -> SelectionPaint {
        let (mut spans, range, clipping) =
            self.visible_selection(viewport, visible_rows, visible_cols);
        let mut shimmer = mutex_lock(&self.shared.selection_shimmer);
        if spans.is_empty() {
            shimmer.cancel();
            return SelectionPaint::default();
        }

        let ragged =
            |span: &SelectionSpan| span.start_col > 0 || span.end_col_exclusive < visible_cols;
        if spans.iter().any(ragged) {
            let buffer = read_lock(&self.buffer);
            let mut cells = Vec::new();
            for span in spans.iter_mut().filter(|span| ragged(span)) {
                viewport.window_row_into(&buffer, span.row, &mut cells);
                snap_to_wide_cells(span, &cells);
                span.end_col_exclusive = span.end_col_exclusive.min(visible_cols);
            }
        }

        let shape = SelectionShape::from_spans(&spans, clipping, visible_rows, metrics);
        let mut paint = SelectionPaint::default();
        if let Some((rect, corners)) = shape.as_rounded_rect(bounds.origin) {
            overlay_quads.push(fill(rect, self.theme.selection).corner_radii(corners));
        } else {
            paint.path = shape.path(bounds.origin);
        }

        if cx.reduce_motion() {
            shimmer.cancel();
        }
        if let Some(progress) = shimmer.progress(range) {
            let selected = spans
                .iter()
                .map(|span| span.end_col_exclusive.saturating_sub(span.start_col))
                .sum();
            match selection_shimmer::strength(selected, visible_rows * visible_cols) {
                Some(strength) => {
                    let tint = selection_shimmer::tint(&self.theme);
                    let line_height = f32::from(metrics.line_height);
                    overlay_quads.extend(
                        selection_shimmer::pieces(&shape, line_height, progress)
                            .iter()
                            .map(|piece| piece.quad(bounds.origin, tint, strength)),
                    );
                    paint.animating = true;
                }
                None => shimmer.cancel(),
            }
        }
        paint
    }
}

impl SelectionPaint {
    /// Paths sort after the quads of their layer, so a stepped outline is
    /// painted between the background layer and an overlay layer of its own.
    pub(super) fn take_path(&mut self) -> Option<Path<Pixels>> {
        self.path.take()
    }

    /// A frame is requested only while the sweep has light left to show.
    pub(super) fn request_frame(&mut self, window: &mut Window) {
        if std::mem::take(&mut self.animating) {
            window.request_animation_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
    use gpui::{
        Context, IntoElement, ParentElement, Render, Styled, TestAppContext, VisualTestContext,
        Window, div,
    };

    use crate::buffer::GridBuffer;
    use crate::element::{TerminalElement, mutex_lock};
    use crate::selection_shape::Clipping;
    use crate::selection_shimmer::DURATION;

    struct Bare(TerminalElement);

    impl Render for Bare {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    fn frame(full: bool, rows: std::ops::Range<u16>) -> GridUpdate {
        GridUpdate {
            cols: 40,
            rows: 12,
            cursor_col: 0,
            cursor_row: 11,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows: rows
                .map(|y| {
                    ChangedRow::new(
                        y,
                        vec![
                            GridCell::new(
                                u32::from('x'),
                                TermColor::Default,
                                TermColor::DefaultInverted,
                                TermStyle::empty(),
                            );
                            40
                        ],
                    )
                })
                .collect(),
        }
    }

    fn window(cx: &mut TestAppContext) -> (TerminalElement, &mut VisualTestContext) {
        let element = TerminalElement::with_buffer(GridBuffer::new(40, 12)).focused(true);
        element.apply_damage(frame(true, 0..12));
        let shown = element.clone();
        let (_, cx) = cx.add_window_view(move |_, _| Bare(shown));
        cx.run_until_parked();
        (element, cx)
    }

    /// Deliver the pending frame, redraw, and report how many frames that
    /// draw asked for in turn.
    fn next_frame(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.simulate_next_frame(cx))
    }

    #[test]
    fn a_selection_scrolled_past_the_window_is_cut_not_rounded() {
        let element = TerminalElement::with_buffer(GridBuffer::new(40, 12));
        element.apply_damage(frame(true, 0..12));
        let history = vec![
            GridCell::new(
                u32::from('h'),
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            );
            40
        ];
        {
            let mut viewport = mutex_lock(&element.shared.viewport);
            viewport.apply_rows(vec![history.clone(), history], 6, 8, 20, 1, 12);
            assert!(viewport.set_view_offset(2, 12));
        }
        // Two history rows, then the live grid: the selection crosses the seam.
        element.begin_selection(5, 0);
        element.drag_selection(20, 3);
        let seen = |element: &TerminalElement| {
            let viewport = mutex_lock(&element.shared.viewport);
            let (spans, _, clipping) = element.visible_selection(&viewport, 12, 40);
            (spans.len(), clipping)
        };
        assert_eq!(seen(&element), (4, Clipping::default()));

        // One line toward live: the first selected row leaves through the top.
        assert!(mutex_lock(&element.shared.viewport).set_view_offset(1, 12));
        assert_eq!(
            seen(&element),
            (
                3,
                Clipping {
                    top: true,
                    bottom: false
                }
            )
        );

        // Scrolling back pushes the tail of a selection out through the bottom.
        element.begin_selection(0, 10);
        element.drag_selection(6, 11);
        assert!(mutex_lock(&element.shared.viewport).set_view_offset(2, 12));
        let (count, clipping) = seen(&element);
        assert!(count >= 1);
        assert!(clipping.bottom && !clipping.top);
    }

    #[gpui::test]
    fn frames_are_requested_only_while_the_sweep_runs(cx: &mut TestAppContext) {
        let (element, cx) = window(cx);
        let started = Instant::now();
        element.pin_selection_shimmer_clock(Some(started));
        assert_eq!(next_frame(cx), 0, "an idle terminal asks for nothing");

        element.begin_selection(30, 2);
        element.drag_selection(8, 4);
        assert_eq!(next_frame(cx), 0, "a drag is static between pointer moves");

        assert!(element.complete_selection());
        for elapsed in [0, 100, 250, 419] {
            element.pin_selection_shimmer_clock(Some(started + Duration::from_millis(elapsed)));
            assert_eq!(next_frame(cx), 1, "running at {elapsed} ms");
        }

        element.pin_selection_shimmer_clock(Some(started + DURATION));
        assert_eq!(next_frame(cx), 0, "the ending frame schedules nothing");
        assert!(!element.selection_shimmer_running());
        assert_eq!(next_frame(cx), 0);
        assert!(element.selection_range().is_some());
    }

    #[gpui::test]
    fn the_sweep_never_touches_the_row_cache(cx: &mut TestAppContext) {
        let (element, cx) = window(cx);
        let started = Instant::now();
        element.pin_selection_shimmer_clock(Some(started));
        element.begin_selection(30, 2);
        element.drag_selection(8, 4);
        next_frame(cx);
        element.reset_stats();
        assert!(element.complete_selection());
        for elapsed in [0, 100, 250, 420] {
            element.pin_selection_shimmer_clock(Some(started + Duration::from_millis(elapsed)));
            next_frame(cx);
        }
        let stats = element.stats();
        assert!(stats.frames > 0);
        assert_eq!(stats.shape_cache_misses, 0);
    }

    #[gpui::test]
    fn reduce_motion_never_starts_the_sweep(cx: &mut TestAppContext) {
        let (element, cx) = window(cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        element.begin_selection(30, 2);
        element.drag_selection(8, 4);
        assert!(element.complete_selection());
        assert_eq!(next_frame(cx), 0);
        assert!(!element.selection_shimmer_running());
    }

    #[gpui::test]
    fn scrolling_output_ends_the_sweep_but_a_spinner_does_not(cx: &mut TestAppContext) {
        let (element, cx) = window(cx);
        element.pin_selection_shimmer_clock(Some(Instant::now()));
        element.begin_selection(30, 2);
        element.drag_selection(8, 4);
        assert!(element.complete_selection());
        element.apply_damage(frame(false, 10..11));
        assert_eq!(next_frame(cx), 1);
        element.apply_damage(frame(false, 5..12));
        assert_eq!(next_frame(cx), 0);
        assert!(element.selection_range().is_some());
    }

    #[gpui::test]
    fn a_whole_screen_selection_is_not_swept(cx: &mut TestAppContext) {
        let (element, cx) = window(cx);
        element.pin_selection_shimmer_clock(Some(Instant::now()));
        element.begin_selection(0, 0);
        element.drag_selection(39, 11);
        assert!(element.complete_selection());
        assert_eq!(next_frame(cx), 0);
        assert!(!element.selection_shimmer_running());
    }
}
