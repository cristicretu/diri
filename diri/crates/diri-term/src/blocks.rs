//! Solid Unicode block elements are terminal geometry, not font glyphs.
//!
//! Rectangles use eighths of a cell and shared edges so font bearings, fallback
//! metrics and line spacing cannot introduce seams into adjoining blocks.

use gpui::{Bounds, Pixels, Point, point, size};

use crate::metrics::CellMetrics;

#[derive(Clone, Copy)]
pub(crate) struct BlockGlyph(&'static [[u8; 4]]);

impl BlockGlyph {
    pub(crate) fn from_scalar(scalar: u32) -> Option<Self> {
        Some(Self(match scalar {
            0x2580 => &[[0, 0, 8, 4]], // upper half
            0x2581 => &[[0, 7, 8, 8]],
            0x2582 => &[[0, 6, 8, 8]],
            0x2583 => &[[0, 5, 8, 8]],
            0x2584 => &[[0, 4, 8, 8]], // lower half
            0x2585 => &[[0, 3, 8, 8]],
            0x2586 => &[[0, 2, 8, 8]],
            0x2587 => &[[0, 1, 8, 8]],
            0x2588 => &[[0, 0, 8, 8]], // full block
            0x2589 => &[[0, 0, 7, 8]],
            0x258a => &[[0, 0, 6, 8]],
            0x258b => &[[0, 0, 5, 8]],
            0x258c => &[[0, 0, 4, 8]], // left half
            0x258d => &[[0, 0, 3, 8]],
            0x258e => &[[0, 0, 2, 8]],
            0x258f => &[[0, 0, 1, 8]],
            0x2590 => &[[4, 0, 8, 8]], // right half
            // Shaded blocks (2591–2593) retain their font's stipple pattern.
            0x2594 => &[[0, 0, 8, 1]],
            0x2595 => &[[7, 0, 8, 8]],
            0x2596 => &[[0, 4, 4, 8]],
            0x2597 => &[[4, 4, 8, 8]],
            0x2598 => &[[0, 0, 4, 4]],
            0x2599 => &[[0, 0, 4, 4], [0, 4, 8, 8]],
            0x259a => &[[0, 0, 4, 4], [4, 4, 8, 8]],
            0x259b => &[[0, 0, 8, 4], [0, 4, 4, 8]],
            0x259c => &[[0, 0, 8, 4], [4, 4, 8, 8]],
            0x259d => &[[4, 0, 8, 4]],
            0x259e => &[[4, 0, 8, 4], [0, 4, 4, 8]],
            0x259f => &[[4, 0, 8, 4], [0, 4, 8, 8]],
            _ => return None,
        }))
    }

    pub(crate) fn rectangles(
        self,
        origin: Point<Pixels>,
        metrics: CellMetrics,
        col: usize,
        row: u16,
    ) -> impl Iterator<Item = Bounds<Pixels>> {
        self.0.iter().map(move |&[left, top, right, bottom]| {
            let x = |edge: u8| origin.x + metrics.cell_width * (col as f32 + f32::from(edge) / 8.0);
            let y = |edge: u8| {
                origin.y + metrics.line_height * (f32::from(row) + f32::from(edge) / 8.0)
            };
            Bounds::new(
                point(x(left), y(top)),
                size(x(right) - x(left), y(bottom) - y(top)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{FontId, px};

    #[test]
    fn adjoining_blocks_share_edges_at_fractional_cell_sizes() {
        for width in [7.25, 8.5, 14.449219] {
            let metrics =
                CellMetrics::from_measurements(px(width), px(12.0), px(5.0), px(0.0), FontId(0));
            let origin = point(px(2.25), px(3.5));
            let rect = |ch, col, row| {
                BlockGlyph::from_scalar(ch as u32)
                    .unwrap()
                    .rectangles(origin, metrics, col, row)
                    .next()
                    .unwrap()
            };
            for col in [0, 1, 19, 159] {
                assert_eq!(rect('█', col, 1).right(), rect('█', col + 1, 1).left());
                assert_eq!(rect('█', col, 1).bottom(), rect('█', col, 2).top());
                assert_eq!(rect('▀', col, 1).bottom(), rect('▄', col, 1).top());
                assert_eq!(rect('▌', col, 1).right(), rect('▐', col, 1).left());
            }
        }
    }

    #[test]
    fn fractional_and_quadrant_blocks_have_exact_nonoverlapping_coverage() {
        let areas = [
            32, 8, 16, 24, 32, 40, 48, 56, 64, 56, 48, 40, 32, 24, 16, 8, 32, 0, 0, 0, 8, 8, 16,
            16, 16, 48, 32, 48, 48, 16, 32, 48,
        ];
        for (offset, expected_area) in areas.into_iter().enumerate() {
            let block = BlockGlyph::from_scalar(0x2580 + offset as u32);
            if expected_area == 0 {
                assert!(block.is_none(), "shaded blocks keep their font pattern");
                continue;
            }
            let mut covered = [[false; 8]; 8];
            for &[left, top, right, bottom] in block.unwrap().0 {
                for row in &mut covered[usize::from(top)..usize::from(bottom)] {
                    for pixel in &mut row[usize::from(left)..usize::from(right)] {
                        assert!(!*pixel, "overlap would darken dim/translucent blocks");
                        *pixel = true;
                    }
                }
            }
            assert_eq!(
                covered
                    .into_iter()
                    .flatten()
                    .filter(|covered| *covered)
                    .count(),
                expected_area
            );
        }
        assert!(BlockGlyph::from_scalar('A' as u32).is_none());
        assert!(BlockGlyph::from_scalar('─' as u32).is_none());
    }
}
