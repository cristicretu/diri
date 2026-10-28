use gpui::{Font, FontId, Pixels, TextSystem, px};

/// Exact terminal-cell geometry shared by sizing and painting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub ascent: Pixels,
    pub font_id: FontId,
    /// How far each glyph sits right of its cell's left edge: half of what a
    /// widened cell adds to the font's own advance, so glyphs stay centred.
    pub glyph_inset: Pixels,
}

impl CellMetrics {
    /// Measure the configured monospace font. The same `cell_width` is used for
    /// both daemon geometry and every painted x coordinate.
    #[must_use]
    pub fn measure(text_system: &TextSystem, font: &Font, font_size: Pixels) -> Self {
        let font_id = text_system.resolve_font(font);
        Self::measure_font(text_system, font_id, font_size)
    }

    /// Measure an already-resolved font. Terminal renderers cache this value
    /// by `(FontId, size)` so unchanged frames do no CoreText metric work.
    #[must_use]
    pub fn measure_font(text_system: &TextSystem, font_id: FontId, font_size: Pixels) -> Self {
        let cell_width = text_system
            .advance(font_id, font_size, 'M')
            .map_or(font_size * 0.6, |advance| advance.width);
        let ascent = text_system.ascent(font_id, font_size);
        // GPUI preserves the OpenType convention of a negative descent.
        let descent = px(f32::from(text_system.descent(font_id, font_size)).abs());
        Self::from_measurements(cell_width, ascent, descent, px(0.0), font_id)
    }

    #[must_use]
    pub fn from_measurements(
        cell_width: Pixels,
        ascent: Pixels,
        descent: Pixels,
        line_gap: Pixels,
        font_id: FontId,
    ) -> Self {
        let raw_height = f32::from(ascent + descent + line_gap);
        Self {
            cell_width: if cell_width > px(0.0) {
                cell_width
            } else {
                px(1.0)
            },
            line_height: px(raw_height.round().max(1.0)),
            ascent,
            font_id,
            glyph_inset: px(0.0),
        }
    }

    /// Widen or narrow cells to `scale` times the font's advance (Ghostty's
    /// `adjust-cell-width`). Glyphs keep their own width and are centred in
    /// the cell; 1.0 leaves the metrics untouched.
    #[must_use]
    pub fn with_cell_width_scale(mut self, scale: f32) -> Self {
        if scale.is_finite() && scale > 0.0 && scale != 1.0 {
            let natural = self.cell_width;
            self.cell_width = px((f32::from(natural) * scale).max(1.0));
            self.glyph_inset = (self.cell_width - natural) / 2.0;
        }
        self
    }

    /// Where a line of text starting at `origin` (a cell's left edge) is
    /// painted from.
    #[must_use]
    pub fn text_origin(self, origin: gpui::Point<Pixels>) -> gpui::Point<Pixels> {
        gpui::point(origin.x + self.glyph_inset, origin.y)
    }

    /// Stretch rows to `scale` times the font's own height, the way
    /// `line-height` works in editors. GPUI centres each shaped line in the
    /// row it is painted into, so the extra space splits above and below.
    #[must_use]
    pub fn with_line_height_scale(mut self, scale: f32) -> Self {
        if scale.is_finite() && scale > 0.0 && scale != 1.0 {
            self.line_height = px((f32::from(self.line_height) * scale).round().max(1.0));
        }
        self
    }

    #[must_use]
    pub fn cols_for_width(self, width: Pixels) -> u16 {
        cells_that_fit(width, self.cell_width)
    }

    #[must_use]
    pub fn rows_for_height(self, height: Pixels) -> u16 {
        cells_that_fit(height, self.line_height)
    }

    #[must_use]
    pub fn x_for_col(self, col: u16) -> Pixels {
        self.cell_width * f32::from(col)
    }

    #[must_use]
    pub fn y_for_row(self, row: u16) -> Pixels {
        self.line_height * f32::from(row)
    }
}

fn cells_that_fit(available: Pixels, cell: Pixels) -> u16 {
    if available <= px(0.0) || cell <= px(0.0) {
        return 0;
    }
    let count = (f32::from(available) / f32::from(cell)).floor();
    count.clamp(0.0, f32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> CellMetrics {
        CellMetrics::from_measurements(px(7.75), px(10.2), px(3.1), px(0.8), FontId(7))
    }

    #[test]
    fn line_height_is_rounded_font_metrics() {
        let metrics = metrics();
        assert_eq!(metrics.line_height, px(14.0));
        assert_eq!(metrics.ascent, px(10.2));
    }

    #[test]
    fn line_height_scale_stretches_rows_to_whole_pixels() {
        assert_eq!(metrics().with_line_height_scale(1.0).line_height, px(14.0));
        assert_eq!(metrics().with_line_height_scale(1.2).line_height, px(17.0));
        assert_eq!(metrics().with_line_height_scale(1.5).line_height, px(21.0));
        assert_eq!(
            metrics().with_line_height_scale(f32::NAN).line_height,
            px(14.0)
        );
        assert_eq!(metrics().with_line_height_scale(1.2).cell_width, px(7.75));
    }

    #[test]
    fn cell_width_scale_widens_cells_and_centres_glyphs() {
        let natural = metrics();
        assert_eq!(natural.with_cell_width_scale(1.0), natural);
        assert_eq!(natural.with_cell_width_scale(f32::NAN), natural);
        let wide = natural.with_cell_width_scale(1.2);
        assert!((f32::from(wide.cell_width) - 9.3).abs() < 1e-4);
        assert!((f32::from(wide.glyph_inset) - 0.775).abs() < 1e-4);
        assert_eq!(wide.line_height, natural.line_height);
        let narrow = natural.with_cell_width_scale(0.9);
        assert!(narrow.glyph_inset < px(0.0));
        assert_eq!(
            wide.text_origin(gpui::point(px(10.0), px(4.0))),
            gpui::point(px(10.0) + wide.glyph_inset, px(4.0))
        );
        // Columns step by the widened cell, with no drift.
        assert_eq!(wide.x_for_col(10), wide.cell_width * 10.0);
    }

    #[test]
    fn columns_use_floor_without_drift() {
        let metrics = metrics();
        assert_eq!(metrics.cols_for_width(px(77.49)), 9);
        assert_eq!(metrics.cols_for_width(px(77.5)), 10);
        assert_eq!(metrics.x_for_col(10), px(77.5));
    }

    #[test]
    fn rows_and_zero_sized_surfaces_are_safe() {
        let metrics = metrics();
        assert_eq!(metrics.rows_for_height(px(42.0)), 3);
        assert_eq!(metrics.cols_for_width(px(0.0)), 0);
        assert_eq!(metrics.y_for_row(3), px(42.0));
    }
}
