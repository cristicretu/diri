//! Box drawing, braille and the core powerline separators are terminal
//! geometry too.
//!
//! A font draws them for its own advance and line height, so on the cell grid
//! frames gap or overlap, braille graphs change texture with the fallback
//! font, and a separator antialiases into a seam against the cell it continues.
//! Here every shape is laid out in whole device pixels between the snapped
//! edges of its cell: straight strokes are pixel-aligned quads, and only arcs,
//! diagonals and separators are antialiased.

use gpui::{
    Background, Bounds, ContentMask, Edges, Hsla, PaintQuad, Path, Pixels, Point, Rgba, Window,
    fill, point, px, size,
};

use crate::metrics::CellMetrics;

/// Device-pixel edges. Every straight stroke keeps these whole, so GPUI's own
/// snapping leaves them where they are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DeviceRect {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
}

impl DeviceRect {
    const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    fn width(self) -> f32 {
        self.right - self.left
    }

    fn height(self) -> f32 {
        self.bottom - self.top
    }

    /// The rectangle with its axes exchanged, which turns a routine written
    /// for horizontal strokes into the one for vertical strokes.
    const fn transposed(self) -> Self {
        Self::new(self.top, self.left, self.bottom, self.right)
    }
}

/// Which corner of its bounds an arc rounds; the stroke follows the two edges
/// that meet there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Corner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Shape {
    /// A pixel-aligned stroke or dot: never antialiased.
    Rect(DeviceRect),
    /// A stroke along two edges of `bounds`, joined by a quarter ring.
    Arc {
        bounds: DeviceRect,
        corner: Corner,
        thickness: f32,
    },
    /// A convex polygon of `len` points, antialiased.
    Polygon { points: [[f32; 2]; 4], len: usize },
}

/// The device-pixel grid sprites are laid out on.
///
/// Cell edges are snapped exactly as GPUI snaps the background quads built
/// from the same expressions, so a sprite that reaches its cell's edge meets
/// the neighboring background and the neighboring sprite on the same pixel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SpriteGrid {
    origin: Point<Pixels>,
    metrics: CellMetrics,
    scale: f32,
    light: f32,
}

impl SpriteGrid {
    pub(crate) fn new(origin: Point<Pixels>, metrics: CellMetrics, scale: f32) -> Self {
        Self {
            origin,
            metrics,
            scale,
            light: light_thickness(metrics, scale),
        }
    }

    pub(crate) const fn origin(&self) -> Point<Pixels> {
        self.origin
    }

    pub(crate) const fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub(crate) fn x(&self, col: usize) -> f32 {
        snap(f32::from(self.origin.x + self.metrics.cell_width * col as f32) * self.scale)
    }

    pub(crate) fn y(&self, row: usize) -> f32 {
        snap(f32::from(self.origin.y + self.metrics.line_height * row as f32) * self.scale)
    }

    pub(crate) fn cell(&self, col: usize, row: u16) -> DeviceRect {
        let row = usize::from(row);
        DeviceRect::new(self.x(col), self.y(row), self.x(col + 1), self.y(row + 1))
    }

    /// Whether rows prepared on this grid can be moved by `dy` instead of laid
    /// out again: only a whole number of device pixels keeps them snapped.
    pub(crate) fn keeps_snapping(&self, dy: Pixels) -> bool {
        (f32::from(dy) * self.scale).fract() == 0.0
    }

    /// Paint `sprite` in `cell` now: the cursor's inverted glyph, which is
    /// not worth keeping.
    pub(crate) fn paint(&self, sprite: Sprite, cell: DeviceRect, color: Rgba, window: &mut Window) {
        sprite.shapes(cell, self.light, &mut |shape| match shape {
            Shape::Rect(rect) => window.paint_quad(fill(self.bounds(rect), color)),
            Shape::Arc {
                bounds,
                corner,
                thickness,
            } => self.arc(bounds, corner, thickness, color).paint(window),
            Shape::Polygon { points, len } => self.polygon(&points[..len], color).paint(window),
        });
    }

    /// Append `sprite`'s shapes for `cell` in `color`.
    pub(crate) fn append(
        &self,
        sprite: Sprite,
        cell: DeviceRect,
        color: Rgba,
        quads: &mut Vec<PaintQuad>,
        antialiased: &mut Vec<AntialiasedShape>,
    ) {
        sprite.shapes(cell, self.light, &mut |shape| match shape {
            Shape::Rect(rect) => quads.push(fill(self.bounds(rect), color)),
            Shape::Arc {
                bounds,
                corner,
                thickness,
            } => antialiased.push(self.arc(bounds, corner, thickness, color)),
            Shape::Polygon { points, len } => {
                antialiased.push(self.polygon(&points[..len], color));
            }
        });
    }

    fn bounds(&self, rect: DeviceRect) -> Bounds<Pixels> {
        Bounds::new(
            point(px(rect.left / self.scale), px(rect.top / self.scale)),
            size(
                px(rect.width() / self.scale),
                px(rect.height() / self.scale),
            ),
        )
    }

    /// A border-only quad: GPUI's quad shader antialiases the ring and leaves
    /// the straight run-outs on whole pixels. It caps a radius at half the
    /// quad, so the quad reaches past the arc and `bounds` clips it back.
    fn arc(
        &self,
        bounds: DeviceRect,
        corner: Corner,
        thickness: f32,
        color: Rgba,
    ) -> AntialiasedShape {
        let radius = bounds.width().min(bounds.height());
        // Twice the longer side also clears the clip, which culls the strips
        // GPUI cuts for the two borderless edges.
        let reach = 2.0 * bounds.width().max(bounds.height());
        let mut outer = bounds;
        match corner {
            Corner::TopLeft | Corner::BottomLeft => outer.right = outer.left + reach,
            Corner::TopRight | Corner::BottomRight => outer.left = outer.right - reach,
        }
        match corner {
            Corner::TopLeft | Corner::TopRight => outer.bottom = outer.top + reach,
            Corner::BottomLeft | Corner::BottomRight => outer.top = outer.bottom - reach,
        }
        let radius = px(radius / self.scale);
        let width = px(thickness / self.scale);
        let mut quad = fill(self.bounds(outer), gpui::transparent_black());
        quad.border_color = Hsla::from(color);
        let (top, right, bottom, left) = match corner {
            Corner::TopLeft => (width, px(0.0), px(0.0), width),
            Corner::TopRight => (width, width, px(0.0), px(0.0)),
            Corner::BottomRight => (px(0.0), width, width, px(0.0)),
            Corner::BottomLeft => (px(0.0), px(0.0), width, width),
        };
        quad.border_widths = Edges {
            top,
            right,
            bottom,
            left,
        };
        match corner {
            Corner::TopLeft => quad.corner_radii.top_left = radius,
            Corner::TopRight => quad.corner_radii.top_right = radius,
            Corner::BottomRight => quad.corner_radii.bottom_right = radius,
            Corner::BottomLeft => quad.corner_radii.bottom_left = radius,
        }
        AntialiasedShape::Arc {
            quad,
            clip: self.bounds(bounds),
        }
    }

    fn polygon(&self, points: &[[f32; 2]], color: Rgba) -> AntialiasedShape {
        let at = |[x, y]: [f32; 2]| point(px(x / self.scale), px(y / self.scale));
        // A fan from the first point: every polygon here is convex.
        let mut path = Path::new(at(points[0]));
        for &next in &points[1..] {
            path.line_to(at(next));
        }
        AntialiasedShape::Polygon {
            path,
            color: color.into(),
        }
    }
}

/// The sprite shapes a plain quad cannot express, kept ready to paint.
#[derive(Clone)]
pub(crate) enum AntialiasedShape {
    Arc {
        quad: PaintQuad,
        clip: Bounds<Pixels>,
    },
    Polygon {
        path: Path<Pixels>,
        color: Background,
    },
}

impl AntialiasedShape {
    pub(crate) fn move_vertically(&mut self, dy: Pixels) {
        match self {
            Self::Arc { quad, clip } => {
                quad.bounds.origin.y += dy;
                clip.origin.y += dy;
            }
            Self::Polygon { path, .. } => {
                path.bounds.origin.y += dy;
                for vertex in &mut path.vertices {
                    vertex.xy_position.y += dy;
                }
            }
        }
    }

    pub(crate) fn paint(&self, window: &mut Window) {
        match self {
            Self::Arc { quad, clip } => {
                window.with_content_mask(Some(ContentMask { bounds: *clip }), |window| {
                    window.paint_quad(quad.clone());
                });
            }
            Self::Polygon { path, color } => window.paint_path(path.clone(), *color),
        }
    }
}

/// GPUI's device-pixel rounding (half toward zero), so an edge snapped here is
/// the edge GPUI gives a quad built from the same logical coordinate.
fn snap(device: f32) -> f32 {
    (device.abs() - 0.5).ceil().copysign(device)
}

/// A light stroke in device pixels; a heavy stroke is twice this.
///
/// An eighth of the advance is the stem weight of a regular monospace face,
/// so frames read as the same weight as the text beside them at every size.
pub(crate) fn light_thickness(metrics: CellMetrics, scale: f32) -> f32 {
    (f32::from(metrics.cell_width) * scale / 8.0)
        .round()
        .max(1.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sprite(u32);

// Arm weights. Light is 1, the weight every other stroke is measured against.
const NONE: u16 = 0;
const HEAVY: u16 = 2;
const DOUBLE: u16 = 3;

/// The arms of each box-drawing line glyph from U+2500, one hex digit per arm
/// in the order up, right, down, left. Zero entries are the dashes, arcs and
/// diagonals, which are not made of arms.
#[rustfmt::skip]
const ARMS: [u16; 0x80] = [
    0x0101, 0x0202, 0x1010, 0x2020, 0, 0, 0, 0,             // ─ ━ │ ┃ ┄ ┅ ┆ ┇
    0, 0, 0, 0, 0x0110, 0x0210, 0x0120, 0x0220,             // ┈ ┉ ┊ ┋ ┌ ┍ ┎ ┏
    0x0011, 0x0012, 0x0021, 0x0022, 0x1100, 0x1200, 0x2100, 0x2200, // ┐ ┑ ┒ ┓ └ ┕ ┖ ┗
    0x1001, 0x1002, 0x2001, 0x2002, 0x1110, 0x1210, 0x2110, 0x1120, // ┘ ┙ ┚ ┛ ├ ┝ ┞ ┟
    0x2120, 0x2210, 0x1220, 0x2220, 0x1011, 0x1012, 0x2011, 0x1021, // ┠ ┡ ┢ ┣ ┤ ┥ ┦ ┧
    0x2021, 0x2012, 0x1022, 0x2022, 0x0111, 0x0112, 0x0211, 0x0212, // ┨ ┩ ┪ ┫ ┬ ┭ ┮ ┯
    0x0121, 0x0122, 0x0221, 0x0222, 0x1101, 0x1102, 0x1201, 0x1202, // ┰ ┱ ┲ ┳ ┴ ┵ ┶ ┷
    0x2101, 0x2102, 0x2201, 0x2202, 0x1111, 0x1112, 0x1211, 0x1212, // ┸ ┹ ┺ ┻ ┼ ┽ ┾ ┿
    0x2111, 0x1121, 0x2121, 0x2112, 0x2211, 0x1122, 0x1221, 0x2212, // ╀ ╁ ╂ ╃ ╄ ╅ ╆ ╇
    0x1222, 0x2122, 0x2221, 0x2222, 0, 0, 0, 0,             // ╈ ╉ ╊ ╋ ╌ ╍ ╎ ╏
    0x0303, 0x3030, 0x0310, 0x0130, 0x0330, 0x0013, 0x0031, 0x0033, // ═ ║ ╒ ╓ ╔ ╕ ╖ ╗
    0x1300, 0x3100, 0x3300, 0x1003, 0x3001, 0x3003, 0x1310, 0x3130, // ╘ ╙ ╚ ╛ ╜ ╝ ╞ ╟
    0x3330, 0x1013, 0x3031, 0x3033, 0x0313, 0x0131, 0x0333, 0x1303, // ╠ ╡ ╢ ╣ ╤ ╥ ╦ ╧
    0x3101, 0x3303, 0x1313, 0x3131, 0x3333, 0, 0, 0,        // ╨ ╩ ╪ ╫ ╬ ╭ ╮ ╯
    0, 0, 0, 0, 0x0001, 0x1000, 0x0100, 0x0010,             // ╰ ╱ ╲ ╳ ╴ ╵ ╶ ╷
    0x0002, 0x2000, 0x0200, 0x0020, 0x0201, 0x1020, 0x0102, 0x2010, // ╸ ╹ ╺ ╻ ╼ ╽ ╾ ╿
];

impl Sprite {
    /// A range check: every scalar in these blocks is drawn here.
    pub(crate) const fn from_scalar(scalar: u32) -> Option<Self> {
        match scalar {
            0x2500..=0x257f | 0x2800..=0x28ff | 0xe0b0..=0xe0b3 => Some(Self(scalar)),
            _ => None,
        }
    }

    /// Strokes that cross the whole cell and nothing else (─ ━ ═): a run of
    /// them is the same strokes over a wider cell.
    pub(crate) const fn spans_cell_width(self) -> bool {
        matches!(self.0, 0x2500 | 0x2501 | 0x2550)
    }

    /// The shapes of this sprite in `cell`, with strokes `light` device pixels
    /// thick. Shapes never overlap, so a translucent color is not darkened.
    pub(crate) fn shapes(self, cell: DeviceRect, light: f32, emit: &mut impl FnMut(Shape)) {
        match self.0 {
            0x2800..=0x28ff => braille(self.0 as u8, cell, emit),
            0xe0b0..=0xe0b3 => powerline(self.0 - 0xe0b0, cell, light, emit),
            scalar => match scalar - 0x2500 {
                offset @ (0x04..=0x0b | 0x4c..=0x4f) => {
                    let count = if offset >= 0x4c {
                        2
                    } else {
                        3 + (offset - 4) / 4
                    };
                    let thickness = if offset & 1 == 0 { light } else { 2.0 * light };
                    if offset & 2 == 0 {
                        dashes(count, thickness, cell, &mut |rect| emit(Shape::Rect(rect)));
                    } else {
                        dashes(count, thickness, cell.transposed(), &mut |rect| {
                            emit(Shape::Rect(rect.transposed()));
                        });
                    }
                }
                offset @ 0x6d..=0x70 => arc(offset - 0x6d, cell, light, emit),
                offset @ 0x71..=0x73 => diagonals(offset - 0x70, cell, light, emit),
                offset => lines(ARMS[offset as usize], cell, light, &mut |rect| {
                    emit(Shape::Rect(rect));
                }),
            },
        }
    }
}

/// Where a centered stroke of `thickness` lies between `start` and `end`.
fn band(start: f32, end: f32, thickness: f32) -> (f32, f32) {
    let thickness = thickness.min(end - start);
    let near = start + ((end - start - thickness) / 2.0).floor();
    (near, near + thickness)
}

fn stroke(weight: u16, light: f32) -> f32 {
    if weight == HEAVY { 2.0 * light } else { light }
}

fn lines(arms: u16, cell: DeviceRect, light: f32, emit: &mut impl FnMut(DeviceRect)) {
    let [up, right, down, left] = [arms >> 12, (arms >> 8) & 0xf, (arms >> 4) & 0xf, arms & 0xf];
    if [up, right, down, left].contains(&DOUBLE) {
        let double = |weight| weight == NONE || weight == DOUBLE;
        if [up, right, down, left].into_iter().all(double) {
            double_lines(
                [up != NONE, right != NONE, down != NONE, left != NONE],
                cell,
                light,
                emit,
            );
        } else if left == DOUBLE || right == DOUBLE {
            mixed_lines(
                [up, right, down, left].map(|arm| arm != NONE),
                cell,
                light,
                emit,
            );
        } else {
            mixed_lines(
                [left, down, right, up].map(|arm| arm != NONE),
                cell.transposed(),
                light,
                &mut |rect| emit(rect.transposed()),
            );
        }
        return;
    }

    // The center column and row, as wide as the heaviest stroke through them.
    let column = band(cell.left, cell.right, stroke(up.max(down), light));
    let row = band(cell.top, cell.bottom, stroke(left.max(right), light));
    let horizontal = |weight| band(cell.top, cell.bottom, stroke(weight, light));
    let vertical = |weight| band(cell.left, cell.right, stroke(weight, light));

    // A stroke that crosses the cell unchanged is one rectangle and owns the
    // center; otherwise the heavier horizontal arm does, and vertical arms
    // stop at the horizontal ink.
    let crosses = left != NONE && left == right;
    let descends = !crosses && up != NONE && up == down;
    if crosses {
        let (top, bottom) = horizontal(left);
        emit(DeviceRect::new(cell.left, top, cell.right, bottom));
    } else {
        let split = if descends || left > right {
            column.1
        } else {
            column.0
        };
        if left != NONE {
            let (top, bottom) = horizontal(left);
            let end = if descends {
                column.0
            } else if right == NONE {
                column.1
            } else {
                split
            };
            emit(DeviceRect::new(cell.left, top, end, bottom));
        }
        if right != NONE {
            let (top, bottom) = horizontal(right);
            let start = if left == NONE && !descends {
                column.0
            } else {
                split
            };
            emit(DeviceRect::new(start, top, cell.right, bottom));
        }
    }
    if descends {
        let (near, far) = vertical(up);
        emit(DeviceRect::new(near, cell.top, far, cell.bottom));
        return;
    }
    let (up_end, down_start) = if left != NONE || right != NONE {
        row
    } else if up != NONE && down != NONE {
        let split = if up > down { row.1 } else { row.0 };
        (split, split)
    } else {
        (row.1, row.0)
    };
    if up != NONE {
        let (near, far) = vertical(up);
        emit(DeviceRect::new(near, cell.top, far, up_end));
    }
    if down != NONE {
        let (near, far) = vertical(down);
        emit(DeviceRect::new(near, down_start, far, cell.bottom));
    }
}

/// The two tracks of a double stroke around the light band between `start`
/// and `end`: `[outer, inner, inner, outer]` edges.
fn tracks(start: f32, end: f32, light: f32) -> [f32; 4] {
    let (near, far) = band(start, end, light);
    [(near - light).max(start), near, far, (far + light).min(end)]
}

/// Glyphs whose every arm is double. Horizontal tracks own the corners.
fn double_lines(
    [up, right, down, left]: [bool; 4],
    cell: DeviceRect,
    light: f32,
    emit: &mut impl FnMut(DeviceRect),
) {
    let x = tracks(cell.left, cell.right, light);
    let y = tracks(cell.top, cell.bottom, light);
    // Each vertical track, with whether an arm leaves the cell on its side.
    for (near, far, arm) in [(x[0], x[1], left), (x[2], x[3], right)] {
        if !arm && (up || down) {
            let top = if up { cell.top } else { y[1] };
            let bottom = if down { cell.bottom } else { y[2] };
            emit(DeviceRect::new(near, top, far, bottom));
            continue;
        }
        if up {
            emit(DeviceRect::new(near, cell.top, far, y[0]));
        }
        if down {
            emit(DeviceRect::new(near, y[3], far, cell.bottom));
        }
    }
    for (near, far, arm) in [(y[0], y[1], up), (y[2], y[3], down)] {
        if !arm && (left || right) {
            let start = if left { cell.left } else { x[0] };
            let end = if right { cell.right } else { x[3] };
            emit(DeviceRect::new(start, near, end, far));
            continue;
        }
        if left {
            emit(DeviceRect::new(cell.left, near, x[1], far));
        }
        if right {
            emit(DeviceRect::new(x[2], near, cell.right, far));
        }
    }
}

/// Double horizontal arms meeting light vertical ones; the transposed call
/// serves the other mix.
fn mixed_lines(
    [up, right, down, left]: [bool; 4],
    cell: DeviceRect,
    light: f32,
    emit: &mut impl FnMut(DeviceRect),
) {
    let (near, far) = band(cell.left, cell.right, light);
    let y = tracks(cell.top, cell.bottom, light);
    if left && right {
        emit(DeviceRect::new(cell.left, y[0], cell.right, y[1]));
        emit(DeviceRect::new(cell.left, y[2], cell.right, y[3]));
        if up {
            emit(DeviceRect::new(near, cell.top, far, y[0]));
        }
        if up && down {
            emit(DeviceRect::new(near, y[1], far, y[2]));
        }
        if down {
            emit(DeviceRect::new(near, y[3], far, cell.bottom));
        }
        return;
    }
    let top = if up { cell.top } else { y[0] };
    let bottom = if down { cell.bottom } else { y[3] };
    emit(DeviceRect::new(near, top, far, bottom));
    let (start, end) = if left {
        (cell.left, near)
    } else {
        (far, cell.right)
    };
    emit(DeviceRect::new(start, y[0], end, y[1]));
    emit(DeviceRect::new(start, y[2], end, y[3]));
}

/// `count` dashes and as many gaps across the cell, half a gap at each end so
/// the rhythm carries into the next cell.
fn dashes(count: u32, thickness: f32, cell: DeviceRect, emit: &mut impl FnMut(DeviceRect)) {
    let (top, bottom) = band(cell.top, cell.bottom, thickness);
    let count = count as f32;
    let width = cell.width();
    if width < 2.0 * count {
        emit(DeviceRect::new(cell.left, top, cell.right, bottom));
        return;
    }
    let gap = thickness.min((width / (2.0 * count)).floor()).max(1.0);
    let pitch = width / count;
    for dash in 0..count as u32 {
        let start = (dash as f32 * pitch + gap / 2.0).floor();
        let end = ((dash + 1) as f32 * pitch - gap / 2.0)
            .floor()
            .max(start + 1.0);
        emit(DeviceRect::new(
            cell.left + start,
            top,
            cell.left + end,
            bottom,
        ));
    }
}

/// ╭ ╮ ╯ ╰: the arc takes the whole half cell, so it ends tangent to both
/// cell edges and runs straight where the cell is taller than wide.
fn arc(which: u32, cell: DeviceRect, light: f32, emit: &mut impl FnMut(Shape)) {
    let (left, right) = band(cell.left, cell.right, light);
    let (top, bottom) = band(cell.top, cell.bottom, light);
    let (bounds, corner) = match which {
        0 => (
            DeviceRect::new(left, top, cell.right, cell.bottom),
            Corner::TopLeft,
        ),
        1 => (
            DeviceRect::new(cell.left, top, right, cell.bottom),
            Corner::TopRight,
        ),
        2 => (
            DeviceRect::new(cell.left, cell.top, right, bottom),
            Corner::BottomRight,
        ),
        _ => (
            DeviceRect::new(left, cell.top, cell.right, bottom),
            Corner::BottomLeft,
        ),
    };
    emit(Shape::Arc {
        bounds,
        corner,
        thickness: light,
    });
}

/// ╱ ╲ ╳, corner to corner. The stroke is cut level with the cell's top and
/// bottom and overhangs its sides, so a staircase of them reads as one line.
fn diagonals(which: u32, cell: DeviceRect, light: f32, emit: &mut impl FnMut(Shape)) {
    // A one-pixel stroke off the pixel grid antialiases to a faint grey, so
    // the thinnest diagonal is a pixel and a half.
    let half = light.max(1.5) * cell.width().hypot(cell.height()) / cell.height() / 2.0;
    let mut stroke = |top: f32, bottom: f32| {
        emit(Shape::Polygon {
            points: [
                [top - half, cell.top],
                [top + half, cell.top],
                [bottom + half, cell.bottom],
                [bottom - half, cell.bottom],
            ],
            len: 4,
        });
    };
    if which & 1 != 0 {
        stroke(cell.right, cell.left);
    }
    if which & 2 != 0 {
        stroke(cell.left, cell.right);
    }
}

/// U+E0B0–U+E0B3. The flat side lies on the cell's snapped edge, so a solid
/// separator meets the background it continues with no antialiased seam.
fn powerline(which: u32, cell: DeviceRect, light: f32, emit: &mut impl FnMut(Shape)) {
    let middle = (cell.top + cell.bottom) / 2.0;
    // Separators pointing left are the mirror image of those pointing right.
    let (flat, tip) = if which < 2 {
        (cell.left, cell.right)
    } else {
        (cell.right, cell.left)
    };
    if which & 1 == 0 {
        emit(Shape::Polygon {
            points: [
                [flat, cell.top],
                [tip, middle],
                [flat, cell.bottom],
                [0.0; 2],
            ],
            len: 3,
        });
        return;
    }
    // The chevron is the triangle's outline: each leg is the band between the
    // triangle's edge and the same edge moved toward the flat side.
    let rise = cell.height() / 2.0;
    let run = cell.width();
    let across = (light * run.hypot(rise) / rise).min(run);
    let inner = tip + across.copysign(flat - tip);
    let inset = across * rise / run;
    for (end, inset) in [(cell.top, inset), (cell.bottom, -inset)] {
        emit(Shape::Polygon {
            points: [
                [flat, end],
                [tip, middle],
                [inner, middle],
                [flat, end + inset],
            ],
            len: 4,
        });
    }
}

/// Dots on a regular 2×4 lattice with half a pitch of margin, so graphs keep
/// their rhythm across cell boundaries.
fn braille(dots: u8, cell: DeviceRect, emit: &mut impl FnMut(Shape)) {
    let dot = ((cell.width() / 2.0).min(cell.height() / 4.0) / 2.0)
        .round()
        .max(1.0);
    // Unicode numbers the dots 1-2-3 down the left, 4-5-6 down the right, and
    // adds 7 and 8 as a fourth row.
    const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    for (column, bits) in BITS.into_iter().enumerate() {
        let center = cell.width() * (2 * column + 1) as f32 / 4.0;
        let left = cell.left + (center - dot / 2.0).round();
        for (row, bit) in bits.into_iter().enumerate() {
            if dots & bit == 0 {
                continue;
            }
            let center = cell.height() * (2 * row + 1) as f32 / 8.0;
            let top = cell.top + (center - dot / 2.0).round();
            emit(Shape::Rect(DeviceRect::new(
                left,
                top,
                left + dot,
                top + dot,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::FontId;

    /// Menlo's proportions at `font_size`.
    fn grid(font_size: f32, scale: f32) -> SpriteGrid {
        let metrics = CellMetrics::from_measurements(
            px(font_size * 0.602),
            px(font_size * 0.928),
            px(font_size * 0.236),
            px(0.0),
            FontId(0),
        );
        SpriteGrid::new(point(px(12.5), px(40.25)), metrics, scale)
    }

    fn rects(ch: char, cell: DeviceRect, light: f32) -> Vec<DeviceRect> {
        let mut rects = Vec::new();
        Sprite::from_scalar(ch as u32)
            .unwrap()
            .shapes(cell, light, &mut |shape| match shape {
                Shape::Rect(rect) => rects.push(rect),
                // An arc's strokes leave its bounds along two edges.
                Shape::Arc {
                    bounds,
                    corner,
                    thickness,
                } => {
                    let DeviceRect {
                        left,
                        top,
                        right,
                        bottom,
                    } = bounds;
                    let (row, rest) = match corner {
                        Corner::TopLeft | Corner::TopRight => {
                            ((top, top + thickness), (top + thickness, bottom))
                        }
                        _ => ((bottom - thickness, bottom), (top, bottom - thickness)),
                    };
                    let column = match corner {
                        Corner::TopLeft | Corner::BottomLeft => (left, left + thickness),
                        _ => (right - thickness, right),
                    };
                    rects.push(DeviceRect::new(left, row.0, right, row.1));
                    rects.push(DeviceRect::new(column.0, rest.0, column.1, rest.1));
                }
                Shape::Polygon { .. } => {}
            });
        rects
    }

    /// The spans along the other axis of the strokes touching one cell edge.
    fn touching(
        rects: &[DeviceRect],
        edge: impl Fn(&DeviceRect) -> bool,
        span: impl Fn(&DeviceRect) -> (f32, f32),
    ) -> Vec<(f32, f32)> {
        let mut spans: Vec<_> = rects.iter().filter(|rect| edge(rect)).map(span).collect();
        spans.sort_by(|a, b| a.partial_cmp(b).unwrap());
        spans
    }

    #[test]
    fn adjacent_cells_join_without_gap_or_overlap_at_every_size_and_scale() {
        let sideways = [
            ('─', '─'),
            ('─', '┼'),
            ('├', '┤'),
            ('╭', '╮'),
            ('╰', '╯'),
            ('╶', '╴'),
            ('━', '╋'),
            ('┣', '┫'),
            ('┝', '┥'),
            ('═', '╬'),
            ('╠', '╣'),
            ('╞', '╡'),
        ];
        let downward = [
            ('│', '│'),
            ('│', '┼'),
            ('┬', '┴'),
            ('╭', '╰'),
            ('╮', '╯'),
            ('╷', '╵'),
            ('┃', '╋'),
            ('┳', '┻'),
            ('┰', '┸'),
            ('║', '╬'),
            ('╦', '╩'),
            ('╥', '╨'),
        ];
        for font_size in 10..=24 {
            for scale in [1.0, 2.0] {
                let grid = grid(font_size as f32, scale);
                for (col, row) in [(0, 0), (1, 1), (7, 3), (79, 23), (158, 48)] {
                    let here = grid.cell(col, row);
                    let right = grid.cell(col + 1, row);
                    let below = grid.cell(col, row + 1);
                    // Cells tile the device grid exactly.
                    assert_eq!(here.right, right.left);
                    assert_eq!(here.bottom, below.top);
                    assert_eq!((here.top, here.bottom), (right.top, right.bottom));
                    assert_eq!((here.left, here.right), (below.left, below.right));
                    for edge in [here.left, here.top, here.right, here.bottom] {
                        assert_eq!(edge.fract(), 0.0);
                    }
                    for (first, second) in sideways {
                        // Every stroke leaving `first` rightward ends on the
                        // shared edge, where a stroke of `second` begins over
                        // the same rows.
                        let leaving = touching(
                            &rects(first, here, grid.light),
                            |rect| rect.right == here.right,
                            |rect| (rect.top, rect.bottom),
                        );
                        let entering = touching(
                            &rects(second, right, grid.light),
                            |rect| rect.left == right.left,
                            |rect| (rect.top, rect.bottom),
                        );
                        assert!(!leaving.is_empty(), "{first} at {font_size}px");
                        assert_eq!(leaving, entering, "{first}{second} at {font_size}px");
                    }
                    for (first, second) in downward {
                        let leaving = touching(
                            &rects(first, here, grid.light),
                            |rect| rect.bottom == here.bottom,
                            |rect| (rect.left, rect.right),
                        );
                        let entering = touching(
                            &rects(second, below, grid.light),
                            |rect| rect.top == below.top,
                            |rect| (rect.left, rect.right),
                        );
                        assert!(!leaving.is_empty(), "{first} at {font_size}px");
                        assert_eq!(leaving, entering, "{first} over {second} at {font_size}px");
                    }
                }
            }
        }
    }

    #[test]
    fn strokes_reach_exactly_the_edges_their_arms_name_and_never_overlap() {
        for (width, height, light) in [(7.0, 15.0, 1.0), (8.0, 17.0, 1.0), (16.0, 34.0, 2.0)] {
            let cell = DeviceRect::new(3.0, 5.0, 3.0 + width, 5.0 + height);
            for (offset, arms) in ARMS.into_iter().enumerate() {
                let ch = char::from_u32(0x2500 + offset as u32).unwrap();
                let rects = rects(ch, cell, light);
                let mut covered = vec![false; (width * height) as usize];
                for rect in &rects {
                    assert!(rect.left < rect.right && rect.top < rect.bottom, "{ch}");
                    assert!(rect.left >= cell.left && rect.right <= cell.right, "{ch}");
                    assert!(rect.top >= cell.top && rect.bottom <= cell.bottom, "{ch}");
                    for y in (rect.top - cell.top) as usize..(rect.bottom - cell.top) as usize {
                        for x in (rect.left - cell.left) as usize..(rect.right - cell.left) as usize
                        {
                            let pixel = &mut covered[y * width as usize + x];
                            assert!(!*pixel, "{ch} overlaps: translucent ink would darken");
                            *pixel = true;
                        }
                    }
                }
                if arms == 0 {
                    continue;
                }
                let reaches = [
                    rects.iter().any(|rect| rect.top == cell.top),
                    rects.iter().any(|rect| rect.right == cell.right),
                    rects.iter().any(|rect| rect.bottom == cell.bottom),
                    rects.iter().any(|rect| rect.left == cell.left),
                ];
                let named = [arms >> 12, (arms >> 8) & 0xf, (arms >> 4) & 0xf, arms & 0xf];
                assert_eq!(reaches, named.map(|arm| arm != NONE), "{ch}");
                // All ink is one connected figure through the cell's center,
                // so its area follows from the arms: no stray rectangle.
                let ink = covered.iter().filter(|pixel| **pixel).count() as f32;
                assert!(ink >= light * (width.min(height) / 2.0).floor(), "{ch}");
            }
        }
    }

    #[test]
    fn heavy_strokes_are_twice_light_and_doubles_are_two_light_strokes() {
        let cell = DeviceRect::new(0.0, 0.0, 16.0, 34.0);
        let thickness = |ch| {
            let mut rows: Vec<_> = rects(ch, cell, 2.0)
                .iter()
                .map(|rect| rect.height())
                .collect();
            rows.dedup();
            rows
        };
        assert_eq!(thickness('─'), [2.0]);
        assert_eq!(thickness('━'), [4.0]);
        assert_eq!(thickness('═'), [2.0]);
        assert_eq!(rects('═', cell, 2.0).len(), 2);
        // The light stroke sits inside the heavy one and between the doubles.
        let light = rects('─', cell, 2.0)[0];
        let heavy = rects('━', cell, 2.0)[0];
        let double = rects('═', cell, 2.0);
        assert!(heavy.top <= light.top && light.bottom <= heavy.bottom);
        assert_eq!((double[0].bottom, double[1].top), (light.top, light.bottom));
    }

    #[test]
    fn dashes_keep_their_rhythm_across_cells() {
        for (width, height, light) in [(7.0, 15.0, 1.0), (8.0, 17.0, 1.0), (16.0, 34.0, 2.0)] {
            let cell = DeviceRect::new(0.0, 0.0, width, height);
            for (ch, count) in [('╌', 2), ('┄', 3), ('┈', 4), ('╍', 2), ('┅', 3), ('┉', 4)]
            {
                let dashes = rects(ch, cell, light);
                if width < 2.0 * count as f32 {
                    // Too narrow for a gap per dash: a solid stroke.
                    assert_eq!((dashes[0].left, dashes[0].right), (0.0, width));
                    continue;
                }
                assert_eq!(dashes.len(), count, "{ch} in {width}");
                // A gap after every dash, the last one shared with the next
                // cell, whose first dash starts where this cell's does.
                for (index, dash) in dashes.iter().enumerate() {
                    let next = dashes
                        .get(index + 1)
                        .map_or(width + dashes[0].left, |d| d.left);
                    assert!(dash.right < next, "{ch} in {width}");
                }
                let upright = char::from_u32(ch as u32 + 2).unwrap();
                let turned: Vec<_> = rects(upright, cell.transposed(), light)
                    .into_iter()
                    .map(DeviceRect::transposed)
                    .collect();
                assert_eq!(turned, dashes, "{upright} is {ch} turned");
            }
        }
    }

    #[test]
    fn thickness_grows_with_font_size_and_never_vanishes() {
        for scale in [1.0, 2.0] {
            let mut previous = 0.0;
            for tenths in 40..=720 {
                let light = grid(tenths as f32 / 10.0, scale).light;
                assert!(light >= 1.0 && light.fract() == 0.0);
                assert!(light >= previous, "{tenths} at {scale}x");
                previous = light;
            }
        }
        assert_eq!(grid(13.0, 1.0).light, 1.0);
        assert_eq!(grid(13.0, 2.0).light, 2.0);
        assert_eq!(grid(24.0, 2.0).light, 4.0);
    }

    #[test]
    fn braille_bits_map_to_their_dots_on_an_even_lattice() {
        for (width, height) in [(7.0, 15.0), (8.0, 17.0), (16.0, 34.0), (29.0, 56.0)] {
            let cell = DeviceRect::new(10.0, 20.0, 10.0 + width, 20.0 + height);
            let dots = |bits: u32| rects(char::from_u32(0x2800 + bits).unwrap(), cell, 1.0);
            assert!(dots(0).is_empty());
            let all = dots(0xff);
            assert_eq!(all.len(), 8);
            let size = all[0].width();
            for dot in &all {
                assert_eq!((dot.width(), dot.height()), (size, size));
                assert!(dot.left >= cell.left && dot.right <= cell.right);
                assert!(dot.top >= cell.top && dot.bottom <= cell.bottom);
            }
            // Dot numbers 1–8 as (column, row) on the 2×4 lattice.
            let places = [
                (0, 0),
                (0, 1),
                (0, 2),
                (1, 0),
                (1, 1),
                (1, 2),
                (0, 3),
                (1, 3),
            ];
            let mut columns = [f32::NAN; 2];
            let mut rows = [f32::NAN; 4];
            for (bit, (column, row)) in places.into_iter().enumerate() {
                let [dot] = dots(1 << bit)[..] else {
                    panic!("one bit is one dot");
                };
                assert!(all.contains(&dot));
                for (known, edge) in [(&mut columns[column], dot.left), (&mut rows[row], dot.top)] {
                    assert!(known.is_nan() || *known == edge, "dot {} strays", bit + 1);
                    *known = edge;
                }
            }
            assert!(columns[0] + size <= columns[1]);
            for pair in rows.windows(2) {
                assert!(pair[0] + size <= pair[1]);
                // Rows keep one pitch to within the pixel rounding costs.
                assert!((pair[1] - pair[0] - height / 4.0).abs() <= 1.0);
            }
        }
    }

    #[test]
    fn separators_lie_on_their_cell_edges() {
        let cell = DeviceRect::new(40.0, 60.0, 56.0, 94.0);
        let polygons = |scalar| {
            let mut polygons = Vec::new();
            Sprite::from_scalar(scalar)
                .unwrap()
                .shapes(cell, 2.0, &mut |shape| match shape {
                    Shape::Polygon { points, len } => polygons.push(points[..len].to_vec()),
                    other => panic!("{other:?}"),
                });
            polygons
        };
        assert_eq!(
            polygons(0xe0b0),
            [[[40.0, 60.0], [56.0, 77.0], [40.0, 94.0]]]
        );
        assert_eq!(
            polygons(0xe0b2),
            [[[56.0, 60.0], [40.0, 77.0], [56.0, 94.0]]]
        );
        for (scalar, flat, tip) in [(0xe0b1, 40.0, 56.0), (0xe0b3, 56.0, 40.0)] {
            let legs = polygons(scalar);
            assert_eq!(legs.len(), 2);
            for leg in legs {
                assert_eq!(leg[1], [tip, 77.0]);
                assert_eq!((leg[0][0], leg[3][0]), (flat, flat));
                for [x, y] in leg {
                    assert!((40.0..=56.0).contains(&x) && (60.0..=94.0).contains(&y));
                }
            }
        }
    }

    #[test]
    fn only_the_drawn_ranges_are_sprites() {
        for scalar in [0x2500, 0x257f, 0x2800, 0x28ff, 0xe0b0, 0xe0b3] {
            assert!(Sprite::from_scalar(scalar).is_some());
        }
        for scalar in [0x41, 0x24ff, 0x2580, 0x2588, 0x27ff, 0x2900, 0xe0af, 0xe0b4] {
            assert!(Sprite::from_scalar(scalar).is_none());
        }
        for ch in ['─', '━', '═'] {
            assert!(Sprite::from_scalar(ch as u32).unwrap().spans_cell_width());
        }
        for ch in ['│', '┼', '┄', '╴', '╭', '⣿'] {
            assert!(!Sprite::from_scalar(ch as u32).unwrap().spans_cell_width());
        }
    }
}
