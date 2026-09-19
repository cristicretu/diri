//! The one-time sheen that confirms a finished selection.
//!
//! A soft, slightly slanted band of light crosses the selection once and is
//! gone. Everything here is a pure function of elapsed time and the selection
//! shape, so a frame can be computed, tested, and rendered offline without a
//! clock. Nothing loops and nothing is remembered once the sweep ends.

use std::time::{Duration, Instant};

use gpui::{PaintQuad, Pixels, Point, Rgba, fill, linear_color_stop, linear_gradient};

use crate::contrast;
use crate::selection::SelectionRange;
use crate::selection_shape::{Block, Radii, SelectionShape};
use crate::theme::{TermTheme, ThemeAppearance};

/// Long enough to register in peripheral vision, short enough to be over
/// before the eye travels to it.
pub const DURATION: Duration = Duration::from_millis(420);

/// Horizontal lag per pixel of height: the band leans about 19 degrees, like a
/// reflection sliding over glass rather than a loading bar.
const SLANT: f32 = 0.35;

/// Half the band's width as a share of the selection's width, within bounds
/// that keep it a band on a single word and a band on a full-width paragraph.
const HALF_WIDTH_SHARE: f32 = 0.30;
const HALF_WIDTH_RANGE: (f32, f32) = (26.0, 120.0);

/// The band's profile, sin² sampled at equal steps. Six linear ramps are
/// indistinguishable from the smooth bell at this opacity, and each ramp is a
/// plain two-stop gradient quad.
const PROFILE: [f32; 7] = [0.0, 0.25, 0.75, 1.0, 0.75, 0.25, 0.0];

/// Share of the visible grid a selection may cover before the sheen starts to
/// thin out, and the share at which it is gone. A sweep across a whole screen
/// of text is a flash, not a confirmation.
const COVERAGE_FADE: (f32, f32) = (0.35, 0.85);
/// Below this the sweep would cost frames without being seen.
const MIN_STRENGTH: f32 = 0.25;

/// A live grid that repaints this much of itself is scrolling output; the
/// sweep yields rather than compete with it for frames or attention.
const BUSY_ROW_SHARE: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Run {
    started: Instant,
    range: SelectionRange,
}

/// When the sweep runs. Owned by the element's shared state.
#[derive(Debug, Default)]
pub(crate) struct SelectionShimmer {
    run: Option<Run>,
    pinned_now: Option<Instant>,
}

impl SelectionShimmer {
    pub(crate) fn pin_clock(&mut self, now: Option<Instant>) {
        self.pinned_now = now;
    }

    fn now(&self) -> Instant {
        self.pinned_now.unwrap_or_else(Instant::now)
    }

    /// The user finished selecting `range`. Returns whether a sweep began.
    pub(crate) fn begin(&mut self, range: Option<SelectionRange>) -> bool {
        self.run = range.map(|range| Run {
            started: self.now(),
            range,
        });
        self.run.is_some()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub(crate) fn cancel(&mut self) {
        self.run = None;
    }

    pub(crate) fn yield_to_output(&mut self, changed_rows: usize, rows: usize) {
        if changed_rows as f32 >= rows as f32 * BUSY_ROW_SHARE {
            self.cancel();
        }
    }

    /// Linear progress in `[0, 1)` while the sweep is running. The run is
    /// forgotten the moment it ends or the selection stops being the one that
    /// was completed, so callers stop asking for frames on that same frame.
    pub(crate) fn progress(&mut self, range: Option<SelectionRange>) -> Option<f32> {
        let run = self.run?;
        let elapsed = self.now().saturating_duration_since(run.started);
        if range != Some(run.range) || elapsed >= DURATION {
            self.run = None;
            return None;
        }
        Some(elapsed.as_secs_f32() / DURATION.as_secs_f32())
    }
}

/// Position along the sweep. Ease-out: the light answers the release at once
/// and settles, where a constant speed reads as a progress bar.
#[must_use]
pub(crate) fn travel(progress: f32) -> f32 {
    1.0 - (1.0 - progress.clamp(0.0, 1.0)).powi(2)
}

/// Brightness over the sweep: up within the first frames, then a long fade so
/// the band dissolves as it slows instead of stopping at the far edge.
#[must_use]
pub(crate) fn envelope(progress: f32) -> f32 {
    let smooth = |t: f32| {
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    smooth(progress / 0.12) * (1.0 - smooth((progress - 0.5) / 0.5))
}

/// How much of the sweep a selection covering `selected_cells` of the visible
/// grid gets, or `None` when it should not run at all.
#[must_use]
pub(crate) fn strength(selected_cells: usize, visible_cells: usize) -> Option<f32> {
    if selected_cells == 0 || visible_cells == 0 {
        return None;
    }
    let coverage = selected_cells as f32 / visible_cells as f32;
    let (from, to) = COVERAGE_FADE;
    let strength = 1.0 - ((coverage - from) / (to - from)).clamp(0.0, 1.0);
    (strength >= MIN_STRENGTH).then_some(strength)
}

/// The band's color at full strength. It is a step of the selection tint, not
/// white: lighter on dark themes, deeper on light ones where a white band
/// would read as a hole in the selection.
#[must_use]
pub(crate) fn tint(theme: &TermTheme) -> Rgba {
    let (delta, chroma, alpha) = match theme.appearance {
        ThemeAppearance::Dark => (0.34, 0.55, 0.16),
        ThemeAppearance::Light => (-0.22, 1.15, 0.16),
    };
    Rgba {
        a: alpha,
        ..contrast::relit(
            Rgba {
                a: 1.0,
                ..theme.selection
            },
            delta,
            chroma,
        )
    }
}

/// One gradient ramp of the band, clipped to one row of the selection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
    pub radii: Radii,
    /// Band opacity in `[0, 1]` at the left and right edges.
    pub alpha_left: f32,
    pub alpha_right: f32,
}

/// The band at `progress`, as non-overlapping ramps inside the shape's rows.
#[must_use]
pub(crate) fn pieces(shape: &SelectionShape, line_height: f32, progress: f32) -> Vec<Piece> {
    let Some(first) = shape.blocks.first() else {
        return Vec::new();
    };
    let brightness = envelope(progress);
    if brightness <= 0.0 || line_height <= 0.0 {
        return Vec::new();
    }
    let (left, right, top, bottom) = shape.blocks.iter().fold(
        (first.left, first.right, first.top, first.bottom),
        |(left, right, top, bottom), block| {
            (
                left.min(block.left),
                right.max(block.right),
                top.min(block.top),
                bottom.max(block.bottom),
            )
        },
    );
    let half = ((right - left) * HALF_WIDTH_SHARE).clamp(HALF_WIDTH_RANGE.0, HALF_WIDTH_RANGE.1);
    // The band's leading edge starts on the shape's left edge, so light shows
    // on the first frame, and the sweep ends once the last row is clear.
    let from = left - half;
    let to = right + half + SLANT * (bottom - top);
    let lead = from + (to - from) * travel(progress);

    let mut pieces = Vec::new();
    for block in &shape.blocks {
        for row in block.first_row..=block.last_row {
            let row_top = block.top + line_height * (row - block.first_row) as f32;
            let center = lead - SLANT * (row_top + line_height / 2.0 - top);
            append_row(
                block,
                row,
                row_top,
                line_height,
                center,
                half,
                brightness,
                &mut pieces,
            );
        }
    }
    pieces
}

#[expect(clippy::too_many_arguments)]
fn append_row(
    block: &Block,
    row: usize,
    top: f32,
    line_height: f32,
    center: f32,
    half: f32,
    brightness: f32,
    pieces: &mut Vec<Piece>,
) {
    let step = half * 2.0 / (PROFILE.len() - 1) as f32;
    for (index, pair) in PROFILE.windows(2).enumerate() {
        let ramp_left = center - half + step * index as f32;
        let ramp_right = ramp_left + step;
        let left = ramp_left.max(block.left);
        let right = ramp_right.min(block.right);
        if right - left <= 0.01 {
            continue;
        }
        let alpha_at =
            |x: f32| (pair[0] + (pair[1] - pair[0]) * (x - ramp_left) / step) * brightness;
        // Only a ramp that ends on the block's own edge inherits its rounding.
        let first = row == block.first_row;
        let last = row == block.last_row;
        let on_left = left == block.left;
        let on_right = right == block.right;
        let pick = |applies: bool, radius: f32| if applies { radius } else { 0.0 };
        pieces.push(Piece {
            left,
            right,
            top,
            bottom: top + line_height,
            radii: Radii {
                top_left: pick(first && on_left, block.radii.top_left),
                top_right: pick(first && on_right, block.radii.top_right),
                bottom_right: pick(last && on_right, block.radii.bottom_right),
                bottom_left: pick(last && on_left, block.radii.bottom_left),
            },
            alpha_left: alpha_at(left),
            alpha_right: alpha_at(right),
        });
    }
}

impl Piece {
    #[must_use]
    pub(crate) fn quad(&self, origin: Point<Pixels>, tint: Rgba, strength: f32) -> PaintQuad {
        let block = Block {
            first_row: 0,
            last_row: 0,
            left: self.left,
            right: self.right,
            top: self.top,
            bottom: self.bottom,
            radii: self.radii,
        };
        let stop = |alpha: f32, at: f32| {
            linear_color_stop(
                Rgba {
                    a: tint.a * strength * alpha,
                    ..tint
                },
                at,
            )
        };
        // 90 degrees runs left to right across the quad's own bounds.
        fill(
            block.bounds(origin),
            linear_gradient(
                90.0,
                stop(self.alpha_left, 0.0),
                stop(self.alpha_right, 1.0),
            ),
        )
        .corner_radii(self.radii.corners())
    }
}

#[cfg(test)]
mod tests {
    use gpui::{FontId, px};

    use super::*;
    use crate::metrics::CellMetrics;
    use crate::selection::{SelectionPoint, SelectionSpan};
    use crate::selection_shape::Clipping;

    fn range(end_col: usize) -> SelectionRange {
        SelectionRange {
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint {
                row: 0,
                col: end_col,
            },
        }
    }

    fn shape(spans: &[(usize, usize, usize)]) -> SelectionShape {
        let spans = spans
            .iter()
            .map(|&(row, start_col, end_col_exclusive)| SelectionSpan {
                row,
                start_col,
                end_col_exclusive,
            })
            .collect::<Vec<_>>();
        let metrics =
            CellMetrics::from_measurements(px(8.0), px(11.0), px(4.0), px(0.0), FontId(0));
        SelectionShape::from_spans(&spans, Clipping::default(), 24, metrics)
    }

    #[test]
    fn the_sweep_runs_once_and_forgets_itself_the_moment_it_ends() {
        let start = Instant::now();
        let mut shimmer = SelectionShimmer::default();
        shimmer.pin_clock(Some(start));
        assert!(shimmer.begin(Some(range(5))));
        assert_eq!(shimmer.progress(Some(range(5))), Some(0.0));

        shimmer.pin_clock(Some(start + DURATION / 2));
        assert_eq!(shimmer.progress(Some(range(5))), Some(0.5));

        shimmer.pin_clock(Some(start + DURATION - Duration::from_millis(1)));
        assert!(shimmer.progress(Some(range(5))).is_some_and(|p| p < 1.0));

        shimmer.pin_clock(Some(start + DURATION));
        assert_eq!(shimmer.progress(Some(range(5))), None);
        // Nothing is left to wake up: time moving on, or even back, stays idle.
        shimmer.pin_clock(Some(start));
        assert_eq!(shimmer.progress(Some(range(5))), None);
    }

    #[test]
    fn an_empty_selection_never_starts_a_sweep() {
        let mut shimmer = SelectionShimmer::default();
        assert!(!shimmer.begin(None));
        assert_eq!(shimmer.progress(None), None);
    }

    #[test]
    fn changing_or_losing_the_selection_ends_the_sweep() {
        let start = Instant::now();
        for next in [Some(range(6)), None] {
            let mut shimmer = SelectionShimmer::default();
            shimmer.pin_clock(Some(start));
            shimmer.begin(Some(range(5)));
            assert_eq!(shimmer.progress(next), None);
            assert_eq!(shimmer.progress(Some(range(5))), None);
        }
    }

    #[test]
    fn scrolling_output_cancels_but_a_spinner_does_not() {
        let mut shimmer = SelectionShimmer::default();
        shimmer.pin_clock(Some(Instant::now()));
        shimmer.begin(Some(range(5)));
        shimmer.yield_to_output(2, 40);
        assert!(shimmer.progress(Some(range(5))).is_some());
        shimmer.yield_to_output(20, 40);
        assert_eq!(shimmer.progress(Some(range(5))), None);
    }

    #[test]
    fn travel_is_monotonic_ease_out() {
        assert_eq!(travel(0.0), 0.0);
        assert_eq!(travel(1.0), 1.0);
        let samples = (0..=100)
            .map(|i| travel(i as f32 / 100.0))
            .collect::<Vec<_>>();
        assert!(samples.windows(2).all(|pair| pair[1] > pair[0]));
        // Front-loaded: most of the distance is covered in the first half.
        assert!(travel(0.5) >= 0.75);
    }

    #[test]
    fn envelope_starts_and_ends_dark() {
        assert_eq!(envelope(0.0), 0.0);
        assert_eq!(envelope(1.0), 0.0);
        assert!((envelope(0.2) - 1.0).abs() < 1e-6);
        assert!((0..=100).all(|i| (0.0..=1.0).contains(&envelope(i as f32 / 100.0))));
    }

    #[test]
    fn large_selections_get_a_thinner_sweep_or_none() {
        assert_eq!(strength(0, 1000), None);
        assert_eq!(strength(10, 0), None);
        assert_eq!(strength(100, 1000), Some(1.0));
        assert_eq!(strength(350, 1000), Some(1.0));
        assert!(strength(600, 1000).is_some_and(|s| (0.4..0.6).contains(&s)));
        assert_eq!(strength(800, 1000), None);
        assert_eq!(strength(1000, 1000), None);
    }

    #[test]
    fn tint_steps_toward_the_viewer_for_the_appearance() {
        for theme in TermTheme::CATALOG {
            let tint = tint(&theme);
            let luma = |color: Rgba| 0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b;
            match theme.appearance {
                ThemeAppearance::Dark => {
                    assert!(luma(tint) > luma(theme.selection), "{}", theme.id);
                }
                ThemeAppearance::Light => {
                    assert!(luma(tint) < luma(theme.selection), "{}", theme.id);
                    assert!(luma(tint) < 0.95, "{}: never a white band", theme.id);
                }
            }
            assert!(tint.a <= 0.2, "{}", theme.id);
        }
    }

    #[test]
    fn pieces_stay_inside_their_rows_and_never_overlap() {
        let shape = shape(&[(0, 10, 40), (1, 0, 40), (2, 0, 12)]);
        let mut lit = 0;
        for step in 0..=40 {
            let pieces = pieces(&shape, 15.0, step as f32 / 40.0);
            lit += pieces.len();
            for piece in &pieces {
                let block = shape
                    .blocks
                    .iter()
                    .find(|block| piece.top >= block.top && piece.bottom <= block.bottom)
                    .expect("piece sits in a block");
                assert!(piece.left >= block.left && piece.right <= block.right);
                assert!(piece.left < piece.right);
                assert!((0.0..=1.0).contains(&piece.alpha_left));
                assert!((0.0..=1.0).contains(&piece.alpha_right));
            }
            for (index, piece) in pieces.iter().enumerate() {
                for other in &pieces[index + 1..] {
                    let same_row = piece.top == other.top;
                    let apart = piece.right <= other.left || other.right <= piece.left;
                    assert!(!same_row || apart);
                }
            }
        }
        assert!(lit > 0);
    }

    #[test]
    fn the_band_is_dark_at_both_ends_of_the_sweep() {
        let shape = shape(&[(0, 0, 40)]);
        assert!(pieces(&shape, 15.0, 0.0).is_empty());
        assert!(pieces(&shape, 15.0, 1.0).is_empty());
        assert!(!pieces(&shape, 15.0, 0.2).is_empty());
    }

    #[test]
    fn lower_rows_trail_the_band() {
        let shape = shape(&[(0, 0, 60), (1, 0, 60), (2, 0, 60)]);
        // One block of three rows still sweeps per text row.
        assert_eq!(shape.blocks.len(), 1);
        let pieces = pieces(&shape, 15.0, 0.15);
        let peak = |top: f32| {
            pieces
                .iter()
                .filter(|piece| piece.top == top)
                .max_by(|a, b| a.alpha_right.total_cmp(&b.alpha_right))
                .map(|piece| piece.right)
                .unwrap()
        };
        assert!(peak(0.0) > peak(15.0) && peak(15.0) > peak(30.0));
    }

    #[test]
    fn only_ramps_on_a_rounded_edge_are_rounded() {
        let shape = shape(&[(0, 0, 40)]);
        let radius = shape.blocks[0].radii.top_left;
        let all = (0..=40)
            .flat_map(|step| pieces(&shape, 15.0, step as f32 / 40.0))
            .collect::<Vec<_>>();
        assert!(all.iter().any(|piece| piece.radii.top_left == radius));
        assert!(all.iter().any(|piece| piece.radii.bottom_right == radius));
        for piece in all {
            assert_eq!(piece.radii.top_left > 0.0, piece.left == 0.0);
            assert_eq!(piece.radii.bottom_right > 0.0, piece.right == 320.0);
        }
    }
}
