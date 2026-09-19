//! One outline for a terminal selection.
//!
//! Per-row spans are merged into blocks of equal extent. Every exposed convex
//! corner is rounded, and every step between two touching blocks gets the
//! matching concave fillet, so the selection reads as a single shape the way
//! an editor draws it. The pieces never overlap: the tint is translucent, and
//! a doubled pixel would show as a darker seam.

use diri_proto::grid::{GridCell, TermStyle};
use gpui::{Bounds, Corners, Path, Pixels, Point, point, px, size};

use crate::metrics::CellMetrics;
use crate::selection::SelectionSpan;

/// Unit quarter arc, 0 to 90 degrees in four chords. At the largest radius
/// (5 px, 2x scale) a chord strays 0.19 device pixels from the circle.
const QUARTER_ARC: [(f32, f32); 5] = [
    (1.0, 0.0),
    (0.923_879_5, 0.382_683_43),
    (0.707_106_77, 0.707_106_77),
    (0.382_683_43, 0.923_879_5),
    (0.0, 1.0),
];

/// Steps narrower than this stay square; a sub-pixel curve only blurs the edge.
const MIN_RADIUS: f32 = 0.5;

/// The radius follows the font so large text does not look pinched, within the
/// range editors use for the same job.
#[must_use]
pub(crate) fn corner_radius(metrics: CellMetrics) -> f32 {
    (f32::from(metrics.line_height) * 0.22).clamp(2.5, 5.0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Radii {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

/// A run of touching rows with the same columns, in pixels relative to the
/// terminal origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Block {
    pub first_row: usize,
    pub last_row: usize,
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
    pub radii: Radii,
}

/// The inverse corner where a narrower block meets a wider neighbour. `x`/`y`
/// is the inner corner; the fill extends `radius` along `toward_x`/`toward_y`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Fillet {
    pub x: f32,
    pub y: f32,
    pub toward_x: f32,
    pub toward_y: f32,
    pub radius: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SelectionShape {
    pub blocks: Vec<Block>,
    pub fillets: Vec<Fillet>,
}

/// Which viewport edges cut the selection. A cut edge stays square because the
/// shape continues past it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Clipping {
    pub top: bool,
    pub bottom: bool,
}

/// Keep a painted span from cutting a double-width glyph in half, following
/// what Cmd-C copies: a glyph is taken when its leading cell is selected.
pub(crate) fn snap_to_wide_cells(span: &mut SelectionSpan, cells: &[GridCell]) {
    let is_spacer = |col: usize| {
        cells
            .get(col)
            .is_some_and(|cell| cell.style.contains(TermStyle::WIDE_SPACER))
    };
    if span.start_col > 0 && is_spacer(span.start_col) {
        span.start_col += 1;
    }
    if span.end_col_exclusive > 0 && is_spacer(span.end_col_exclusive) {
        span.end_col_exclusive += 1;
    }
}

struct Run {
    first_row: usize,
    last_row: usize,
    start_col: usize,
    end_col: usize,
}

impl Run {
    fn touches(&self, below: &Self) -> bool {
        below.first_row == self.last_row + 1
            && self.start_col.max(below.start_col) < self.end_col.min(below.end_col)
    }
}

impl SelectionShape {
    /// `spans` are in window-row order, as [`TerminalSelection::visible_spans`]
    /// yields them.
    ///
    /// [`TerminalSelection::visible_spans`]: crate::selection::TerminalSelection::visible_spans
    #[must_use]
    pub(crate) fn from_spans(
        spans: &[SelectionSpan],
        clipping: Clipping,
        visible_rows: usize,
        metrics: CellMetrics,
    ) -> Self {
        let mut runs: Vec<Run> = Vec::new();
        for span in spans {
            if span.start_col >= span.end_col_exclusive {
                continue;
            }
            if let Some(run) = runs.last_mut()
                && run.last_row + 1 == span.row
                && run.start_col == span.start_col
                && run.end_col == span.end_col_exclusive
            {
                run.last_row = span.row;
                continue;
            }
            runs.push(Run {
                first_row: span.row,
                last_row: span.row,
                start_col: span.start_col,
                end_col: span.end_col_exclusive,
            });
        }

        let cell_width = f32::from(metrics.cell_width);
        let line_height = f32::from(metrics.line_height);
        let radius = corner_radius(metrics);
        let x = |col: usize| cell_width * col as f32;
        let mut shape = Self {
            blocks: Vec::with_capacity(runs.len()),
            fillets: Vec::new(),
        };
        for (index, run) in runs.iter().enumerate() {
            let above = index
                .checked_sub(1)
                .map(|above| &runs[above])
                .filter(|above| above.touches(run));
            let below = runs.get(index + 1).filter(|below| run.touches(below));
            let width = x(run.end_col) - x(run.start_col);
            let free = radius.min(line_height / 2.0).min(width / 2.0);
            // A corner on a step shares the step with the fillet beside it.
            let stepped = |cols: usize| free.min(x(cols) / 2.0);
            let cut_above = clipping.top && run.first_row == 0;
            let cut_below = clipping.bottom && run.last_row + 1 >= visible_rows;
            let corner = |neighbour: Option<&Run>, cut: bool, left: bool| match neighbour {
                None if cut => 0.0,
                None => free,
                Some(other) => {
                    // Rounded only where this block reaches past its neighbour.
                    let overhang = if left {
                        other.start_col.saturating_sub(run.start_col)
                    } else {
                        run.end_col.saturating_sub(other.end_col)
                    };
                    if overhang > 0 { stepped(overhang) } else { 0.0 }
                }
            };
            let snap = |radius: f32| if radius < MIN_RADIUS { 0.0 } else { radius };
            shape.blocks.push(Block {
                first_row: run.first_row,
                last_row: run.last_row,
                left: x(run.start_col),
                right: x(run.end_col),
                top: line_height * run.first_row as f32,
                bottom: line_height * (run.last_row + 1) as f32,
                radii: Radii {
                    top_left: snap(corner(above, cut_above, true)),
                    top_right: snap(corner(above, cut_above, false)),
                    bottom_right: snap(corner(below, cut_below, false)),
                    bottom_left: snap(corner(below, cut_below, true)),
                },
            });

            let Some(below) = below else { continue };
            let y = line_height * (run.last_row + 1) as f32;
            let reach = radius.min(line_height / 2.0);
            let mut fillet = |inner_col: usize, step_cols: usize, toward_x: f32, toward_y: f32| {
                let radius = reach.min(x(step_cols) / 2.0);
                if radius >= MIN_RADIUS {
                    shape.fillets.push(Fillet {
                        x: x(inner_col),
                        y,
                        toward_x,
                        toward_y,
                        radius,
                    });
                }
            };
            // The inner corner sits on the narrower block's edge and opens
            // toward the wider block's overhang.
            if below.start_col < run.start_col {
                fillet(run.start_col, run.start_col - below.start_col, -1.0, -1.0);
            } else if run.start_col < below.start_col {
                fillet(below.start_col, below.start_col - run.start_col, -1.0, 1.0);
            }
            if below.end_col > run.end_col {
                fillet(run.end_col, below.end_col - run.end_col, 1.0, -1.0);
            } else if run.end_col > below.end_col {
                fillet(below.end_col, run.end_col - below.end_col, 1.0, 1.0);
            }
        }
        shape
    }

    /// A lone rectangle paints as a rounded quad, which needs no path pass.
    #[must_use]
    pub(crate) fn as_rounded_rect(
        &self,
        origin: Point<Pixels>,
    ) -> Option<(Bounds<Pixels>, Corners<Pixels>)> {
        let [block] = self.blocks.as_slice() else {
            return None;
        };
        Some((block.bounds(origin), block.radii.corners()))
    }

    /// Triangles covering the shape exactly once, as `[x0, y0, x1, y1, x2, y2]`
    /// relative to the terminal origin.
    pub(crate) fn triangulate(&self, triangles: &mut Vec<[f32; 6]>) {
        let mut ring: Vec<(f32, f32)> = Vec::with_capacity(4 * QUARTER_ARC.len());
        for block in &self.blocks {
            ring.clear();
            block.ring(&mut ring);
            // The ring is convex, so a fan from its first point covers it.
            for pair in ring[1..].windows(2) {
                triangles.push([
                    ring[0].0, ring[0].1, pair[0].0, pair[0].1, pair[1].0, pair[1].1,
                ]);
            }
        }
        for fillet in &self.fillets {
            let center = (
                fillet.x + fillet.toward_x * fillet.radius,
                fillet.y + fillet.toward_y * fillet.radius,
            );
            let at = |(cos, sin): (f32, f32)| {
                (
                    center.0 - fillet.toward_x * fillet.radius * sin,
                    center.1 - fillet.toward_y * fillet.radius * cos,
                )
            };
            for pair in QUARTER_ARC.windows(2) {
                let (from, to) = (at(pair[0]), at(pair[1]));
                triangles.push([fillet.x, fillet.y, from.0, from.1, to.0, to.1]);
            }
        }
    }

    /// The filled outline as one GPUI path. Built from triangles directly: the
    /// shape is rectilinear, so a general tessellator is wasted work on a path
    /// that is rebuilt for every pointer move of a drag.
    #[must_use]
    pub(crate) fn path(&self, origin: Point<Pixels>) -> Option<Path<Pixels>> {
        let mut triangles = Vec::with_capacity(self.blocks.len() * 2 + self.fillets.len() * 4);
        self.triangulate(&mut triangles);
        let first = triangles.first()?;
        let at = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));
        let mut path = Path::new(at(first[0], first[1]));
        // Constant `st` marks a triangle as solid rather than a curve segment.
        let solid = (point(0.0, 1.0), point(0.0, 1.0), point(0.0, 1.0));
        for [x0, y0, x1, y1, x2, y2] in triangles {
            path.push_triangle((at(x0, y0), at(x1, y1), at(x2, y2)), solid);
        }
        Some(path)
    }
}

impl Radii {
    #[must_use]
    pub(crate) fn corners(self) -> Corners<Pixels> {
        Corners {
            top_left: px(self.top_left),
            top_right: px(self.top_right),
            bottom_right: px(self.bottom_right),
            bottom_left: px(self.bottom_left),
        }
    }
}

impl Block {
    #[must_use]
    pub(crate) fn bounds(&self, origin: Point<Pixels>) -> Bounds<Pixels> {
        Bounds::new(
            point(origin.x + px(self.left), origin.y + px(self.top)),
            size(px(self.right - self.left), px(self.bottom - self.top)),
        )
    }

    /// Clockwise outline from the top-left corner.
    fn ring(&self, ring: &mut Vec<(f32, f32)>) {
        let Self {
            left,
            right,
            top,
            bottom,
            radii,
            ..
        } = *self;
        let mut corner = |radius: f32, x: f32, y: f32, arc: fn(f32, f32, f32) -> (f32, f32)| {
            if radius == 0.0 {
                ring.push((x, y));
            } else {
                ring.extend(QUARTER_ARC.iter().map(|&(cos, sin)| {
                    let (dx, dy) = arc(radius, cos, sin);
                    (x + dx, y + dy)
                }));
            }
        };
        corner(radii.top_left, left, top, |r, cos, sin| {
            (r - r * cos, r - r * sin)
        });
        corner(radii.top_right, right, top, |r, cos, sin| {
            (r * sin - r, r - r * cos)
        });
        corner(radii.bottom_right, right, bottom, |r, cos, sin| {
            (r * cos - r, r * sin - r)
        });
        corner(radii.bottom_left, left, bottom, |r, cos, sin| {
            (r - r * sin, r * cos - r)
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::FontId;

    use super::*;

    fn metrics() -> CellMetrics {
        CellMetrics::from_measurements(px(8.0), px(11.0), px(4.0), px(0.0), FontId(0))
    }

    fn span(row: usize, start_col: usize, end_col_exclusive: usize) -> SelectionSpan {
        SelectionSpan {
            row,
            start_col,
            end_col_exclusive,
        }
    }

    fn shape(spans: &[SelectionSpan]) -> SelectionShape {
        SelectionShape::from_spans(spans, Clipping::default(), 24, metrics())
    }

    fn area(shape: &SelectionShape) -> f32 {
        let mut triangles = Vec::new();
        shape.triangulate(&mut triangles);
        triangles
            .iter()
            .map(|[x0, y0, x1, y1, x2, y2]| {
                ((x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0)).abs() / 2.0
            })
            .sum()
    }

    #[test]
    fn radius_tracks_the_font_within_editor_bounds() {
        assert_eq!(corner_radius(metrics()), 15.0 * 0.22);
        let small = CellMetrics::from_measurements(px(4.0), px(6.0), px(2.0), px(0.0), FontId(0));
        let large =
            CellMetrics::from_measurements(px(20.0), px(30.0), px(10.0), px(0.0), FontId(0));
        assert_eq!(corner_radius(small), 2.5);
        assert_eq!(corner_radius(large), 5.0);
    }

    #[test]
    fn single_row_is_one_fully_rounded_rectangle() {
        let shape = shape(&[span(2, 3, 9)]);
        let r = corner_radius(metrics());
        assert_eq!(shape.blocks.len(), 1);
        assert!(shape.fillets.is_empty());
        let block = shape.blocks[0];
        assert_eq!(
            (block.left, block.right, block.top, block.bottom),
            (24.0, 72.0, 30.0, 45.0)
        );
        assert_eq!(
            block.radii,
            Radii {
                top_left: r,
                top_right: r,
                bottom_right: r,
                bottom_left: r
            }
        );
        let (bounds, corners) = shape.as_rounded_rect(point(px(10.0), px(5.0))).unwrap();
        assert_eq!(bounds.origin, point(px(34.0), px(35.0)));
        assert_eq!(corners.top_left, px(r));
    }

    #[test]
    fn equal_rows_merge_so_block_selections_have_no_inner_edges() {
        let shape = shape(&[span(1, 4, 10), span(2, 4, 10), span(3, 4, 10)]);
        assert_eq!(shape.blocks.len(), 1);
        assert_eq!(
            (shape.blocks[0].first_row, shape.blocks[0].last_row),
            (1, 3)
        );
        assert_eq!((shape.blocks[0].top, shape.blocks[0].bottom), (15.0, 60.0));
        assert!(shape.as_rounded_rect(Point::default()).is_some());
    }

    #[test]
    fn ragged_rows_round_outer_corners_and_fillet_inner_ones() {
        // The usual reading-order selection: a tail, full rows, a head.
        let shape = shape(&[
            span(0, 10, 40),
            span(1, 0, 40),
            span(2, 0, 40),
            span(3, 0, 12),
        ]);
        let r = corner_radius(metrics());
        assert_eq!(shape.blocks.len(), 3);
        let [first, middle, last] = [shape.blocks[0], shape.blocks[1], shape.blocks[2]];
        assert_eq!(
            first.radii,
            Radii {
                top_left: r,
                top_right: r,
                bottom_right: 0.0,
                bottom_left: 0.0
            }
        );
        assert_eq!(
            middle.radii,
            Radii {
                top_left: r,
                top_right: 0.0,
                bottom_right: r,
                bottom_left: 0.0
            }
        );
        assert_eq!(
            last.radii,
            Radii {
                top_left: 0.0,
                top_right: 0.0,
                bottom_right: r,
                bottom_left: r
            }
        );
        assert_eq!(
            shape.fillets,
            vec![
                Fillet {
                    x: 80.0,
                    y: 15.0,
                    toward_x: -1.0,
                    toward_y: -1.0,
                    radius: r
                },
                Fillet {
                    x: 96.0,
                    y: 45.0,
                    toward_x: 1.0,
                    toward_y: 1.0,
                    radius: r
                },
            ]
        );
    }

    #[test]
    fn a_one_cell_step_splits_its_width_between_corner_and_fillet() {
        let narrow = CellMetrics::from_measurements(px(5.0), px(11.0), px(4.0), px(0.0), FontId(0));
        let shape = SelectionShape::from_spans(
            &[span(0, 1, 20), span(1, 0, 20)],
            Clipping::default(),
            24,
            narrow,
        );
        assert_eq!(shape.blocks[1].radii.top_left, 2.5);
        assert_eq!(shape.fillets[0].radius, 2.5);
        // Together they span exactly the step, so the S-curve never overshoots.
        assert_eq!(
            shape.blocks[1].radii.top_left + shape.fillets[0].radius,
            5.0
        );
    }

    #[test]
    fn viewport_cut_edges_stay_square() {
        let spans = [span(0, 0, 40), span(1, 0, 40), span(2, 0, 7)];
        let cut_top = SelectionShape::from_spans(
            &spans,
            Clipping {
                top: true,
                bottom: false,
            },
            3,
            metrics(),
        );
        assert_eq!(cut_top.blocks[0].radii.top_left, 0.0);
        assert_eq!(cut_top.blocks[0].radii.top_right, 0.0);
        assert!(cut_top.blocks[1].radii.bottom_left > 0.0);

        let spans = [span(1, 30, 40), span(2, 0, 40)];
        let cut_bottom = SelectionShape::from_spans(
            &spans,
            Clipping {
                top: false,
                bottom: true,
            },
            3,
            metrics(),
        );
        assert!(cut_bottom.blocks[0].radii.top_left > 0.0);
        assert_eq!(cut_bottom.blocks[1].radii.bottom_left, 0.0);
        assert_eq!(cut_bottom.blocks[1].radii.bottom_right, 0.0);
        // The step above the cut edge is still inside the viewport.
        assert!(cut_bottom.blocks[1].radii.top_left > 0.0);

        // A cut only applies to a block that actually reaches the edge.
        let inside = SelectionShape::from_spans(
            &[span(1, 0, 4)],
            Clipping {
                top: true,
                bottom: true,
            },
            3,
            metrics(),
        );
        assert!(inside.blocks[0].radii.top_left > 0.0);
        assert!(inside.blocks[0].radii.bottom_left > 0.0);
    }

    #[test]
    fn rows_that_do_not_touch_are_separate_rounded_shapes() {
        // A row gap, and two rows whose columns only meet at a point.
        for spans in [
            [span(0, 0, 10), span(2, 0, 10)],
            [span(0, 10, 20), span(1, 0, 10)],
        ] {
            let shape = shape(&spans);
            assert_eq!(shape.blocks.len(), 2);
            assert!(shape.fillets.is_empty());
            for block in &shape.blocks {
                assert!(block.radii.top_left > 0.0 && block.radii.bottom_right > 0.0);
            }
        }
    }

    #[test]
    fn pieces_cover_the_outline_exactly_once() {
        let spans = [span(0, 10, 40), span(1, 0, 40), span(2, 0, 12)];
        let shape = shape(&spans);
        let r = corner_radius(metrics());
        let cells: f32 = spans
            .iter()
            .map(|span| (span.end_col_exclusive - span.start_col) as f32 * 8.0 * 15.0)
            .sum();
        // Each rounded corner removes, and each fillet adds, the sliver
        // between a square corner and its chorded quarter disc.
        let disc: f32 = QUARTER_ARC
            .windows(2)
            .map(|pair| (pair[0].0 * pair[1].1 - pair[1].0 * pair[0].1) / 2.0)
            .sum();
        let sliver = r * r * (1.0 - disc);
        let rounded = 6.0;
        let fillets = 2.0;
        let expected = cells - rounded * sliver + fillets * sliver;
        assert!((area(&shape) - expected).abs() < 0.01, "{}", area(&shape));
    }

    #[test]
    fn path_bounds_match_the_selected_cells() {
        let shape = shape(&[span(1, 10, 40), span(2, 0, 12)]);
        let path = shape.path(point(px(100.0), px(50.0))).unwrap();
        assert_eq!(path.bounds.origin, point(px(100.0), px(65.0)));
        assert_eq!(path.bounds.size, size(px(320.0), px(30.0)));
        assert!(SelectionShape::default().path(Point::default()).is_none());
    }

    #[test]
    fn wide_glyphs_are_never_cut_in_half() {
        let mut cells = vec![GridCell::BLANK; 10];
        cells[4].style |= TermStyle::WIDE_SPACER;
        // Ending on the glyph's leading cell takes the whole glyph.
        let mut ends_inside = span(0, 0, 4);
        snap_to_wide_cells(&mut ends_inside, &cells);
        assert_eq!(ends_inside, span(0, 0, 5));
        // Starting on the spacer leaves the glyph out, as copy does.
        let mut starts_inside = span(0, 4, 9);
        snap_to_wide_cells(&mut starts_inside, &cells);
        assert_eq!(starts_inside, span(0, 5, 9));
        let mut clear = span(0, 5, 9);
        snap_to_wide_cells(&mut clear, &cells);
        assert_eq!(clear, span(0, 5, 9));
    }
}
