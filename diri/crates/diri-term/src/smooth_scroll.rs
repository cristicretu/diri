//! Sub-row scroll position for the reading view.
//!
//! The viewport's row model stays integral: fetch windows, anchors, find,
//! selection and links all speak whole rows. A trackpad speaks pixels, so the
//! part of a gesture that does not amount to a row is kept here as presentation
//! state and applied as one vertical translation when the reading view paints.
//!
//! A position is `rows - fraction` rows back from the live edge, with
//! `fraction` in `[0, 1)`. The integral window is the one `rows` names; the
//! fraction slides it *up*, revealing part of the row below it. Writing the
//! position this way keeps two invariants free of special cases: any fraction
//! implies `rows >= 1`, so a partly scrolled view is always a reading view,
//! and the live edge is exactly `rows == 0, fraction == 0`.
//!
//! The fraction is unitless rather than pixels so a font-size change rescales
//! the offset with the rows instead of leaving it out of range.

/// Upward finger travel needed before a precise gesture leaves the live edge.
///
/// Leaving live freezes the view under the reader, so a resting finger's
/// sub-pixel jitter must not do it. The travel is not discarded: once past the
/// slop the whole accumulated distance is applied, so tracking stays exact.
pub const LEAVE_LIVE_SLOP: f32 = 2.0;

/// Distance from a whole row, in rows, that counts as being on it. A thousandth
/// of a device pixel at any plausible line height.
const ROW_EPSILON: f32 = 1e-5;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollPosition {
    /// Whole rows back from the live edge; the viewport's `view_offset`.
    pub rows: i64,
    /// How far the window named by `rows` is slid up, as a part of one row.
    pub fraction: f32,
}

/// Result of feeding one precise wheel delta to [`ScrollPosition::step_pixels`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelStep {
    pub position: ScrollPosition,
    /// Travel held back by [`LEAVE_LIVE_SLOP`], to pass to the next step.
    pub pending: f32,
}

impl ScrollPosition {
    pub const LIVE: Self = Self {
        rows: 0,
        fraction: 0.0,
    };

    #[must_use]
    pub const fn whole(rows: i64) -> Self {
        Self {
            rows,
            fraction: 0.0,
        }
    }

    /// Rows back from the live edge, fractional. `max_rows` bounds the result
    /// and non-finite input lands on the live edge.
    #[must_use]
    pub fn from_rows(rows: f64, max_rows: i64) -> Self {
        if !rows.is_finite() {
            return Self::LIVE;
        }
        let whole = rows.ceil();
        Self {
            rows: whole as i64,
            fraction: (whole - rows) as f32,
        }
        .clamped(max_rows)
    }

    #[must_use]
    pub fn as_rows(self) -> f64 {
        self.rows as f64 - f64::from(self.fraction)
    }

    #[must_use]
    pub fn is_live(self) -> bool {
        self.rows <= 0
    }

    /// The nearest whole row, which a line-based wheel steps from.
    #[must_use]
    pub fn nearest_row(self) -> i64 {
        if self.fraction >= 0.5 {
            self.rows.saturating_sub(1)
        } else {
            self.rows
        }
    }

    /// Both ends are firm and carry no residue: past the oldest row is
    /// exactly `max_rows`, and at or past the live edge is exactly [`LIVE`].
    ///
    /// [`LIVE`]: Self::LIVE
    #[must_use]
    pub fn clamped(self, max_rows: i64) -> Self {
        let max_rows = max_rows.max(0);
        let mut rows = self.rows;
        let mut fraction = if self.fraction.is_finite() {
            self.fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Float carry can stop a hair short of a whole row. Left alone, a
        // gesture that returns exactly to the live edge would rest one
        // millionth of a row away from it, still holding the reading view.
        if fraction > 1.0 - ROW_EPSILON {
            rows = rows.saturating_sub(1);
            fraction = 0.0;
        } else if fraction < ROW_EPSILON {
            fraction = 0.0;
        }
        if rows <= 0 {
            Self::LIVE
        } else if rows > max_rows {
            Self::whole(max_rows)
        } else {
            Self { rows, fraction }
        }
    }

    /// Moves by `delta` pixels, positive toward history, tracking 1:1.
    ///
    /// The carry is computed on the fraction alone, in `f64`, so precision
    /// does not degrade with the depth of the history being read.
    #[must_use]
    pub fn moved_by_pixels(self, delta: f32, line_height: f32, max_rows: i64) -> Self {
        if !delta.is_finite() || !line_height.is_finite() {
            return self.clamped(max_rows);
        }
        let slid = f64::from(self.fraction) - f64::from(delta) / f64::from(line_height.max(1.0));
        let carried = slid.floor();
        Self {
            rows: self.rows.saturating_sub(carried as i64),
            fraction: (slid - carried) as f32,
        }
        .clamped(max_rows)
    }

    /// [`Self::moved_by_pixels`] with the live-edge slop applied. `pending`
    /// is the travel the previous step held back.
    #[must_use]
    pub fn step_pixels(
        self,
        pending: f32,
        delta: f32,
        line_height: f32,
        max_rows: i64,
    ) -> PixelStep {
        if !self.is_live() {
            return PixelStep {
                position: self.moved_by_pixels(delta, line_height, max_rows),
                pending: 0.0,
            };
        }
        // A reversal abandons the attempt, as it does for the line accumulator.
        if !delta.is_finite() || delta <= 0.0 {
            return PixelStep {
                position: Self::LIVE,
                pending: 0.0,
            };
        }
        let travelled = pending.max(0.0) + delta;
        if travelled < LEAVE_LIVE_SLOP {
            return PixelStep {
                position: Self::LIVE,
                pending: travelled,
            };
        }
        PixelStep {
            position: Self::LIVE.moved_by_pixels(travelled, line_height, max_rows),
            pending: 0.0,
        }
    }

    /// The translation to paint with, in logical pixels, snapped to whole
    /// device pixels. GPUI rasterizes glyphs at whole device rows while quads
    /// antialias, so an unsnapped offset makes text shimmer against its own
    /// backgrounds and underlines as it moves. Hit-testing must use this same
    /// value, not the raw fraction.
    #[must_use]
    pub fn shift(self, line_height: f32, scale_factor: f32) -> f32 {
        let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        (self.fraction * line_height * scale).round() / scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: f32 = 15.0;
    const MAX: i64 = 5_000;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    fn at(rows: f64) -> ScrollPosition {
        ScrollPosition::from_rows(rows, MAX)
    }

    #[test]
    fn many_tiny_deltas_land_where_one_big_delta_does() {
        let start = at(100.0);
        let mut stepped = start;
        for _ in 0..4_000 {
            stepped = stepped.moved_by_pixels(0.25, LINE, MAX);
        }
        let jumped = start.moved_by_pixels(1_000.0, LINE, MAX);
        assert_eq!(stepped.rows, jumped.rows);
        assert!(close(stepped.as_rows(), jumped.as_rows()));
        assert!(close(jumped.as_rows(), 100.0 + 1_000.0 / 15.0));
    }

    #[test]
    fn precision_does_not_depend_on_history_depth() {
        let deep = ScrollPosition::from_rows(9_000_000.0, i64::MAX);
        let moved = deep.moved_by_pixels(1.0, LINE, i64::MAX);
        assert_eq!(moved.rows, 9_000_001);
        assert!((moved.fraction - (1.0 - 1.0 / 15.0)).abs() < 1e-6);
    }

    #[test]
    fn reversing_mid_row_retraces_the_same_pixels() {
        let start = at(40.0);
        let up = start.moved_by_pixels(7.0, LINE, MAX);
        assert_eq!(up.rows, 41);
        assert!(close(up.as_rows(), 40.0 + 7.0 / 15.0));
        let partly_back = up.moved_by_pixels(-3.0, LINE, MAX);
        assert_eq!(partly_back.rows, 41, "still inside the same row");
        assert!(close(partly_back.as_rows(), 40.0 + 4.0 / 15.0));
        let back = partly_back.moved_by_pixels(-4.0, LINE, MAX);
        assert!(close(back.as_rows(), 40.0));
        assert_eq!(back.shift(LINE, 2.0), 0.0);
    }

    #[test]
    fn the_live_edge_is_reached_with_no_residue() {
        let near = at(0.4);
        assert_eq!(near.rows, 1);
        for overshoot in [6.0, 6.5, 400.0] {
            assert_eq!(
                near.moved_by_pixels(-overshoot, LINE, MAX),
                ScrollPosition::LIVE
            );
        }
        // Exactly the remaining distance is live too, never `rows: 0` with a
        // fraction left over.
        let exact = ScrollPosition {
            rows: 1,
            fraction: 0.5,
        }
        .moved_by_pixels(-7.5, LINE, MAX);
        assert_eq!(exact, ScrollPosition::LIVE);
    }

    #[test]
    fn the_oldest_row_is_a_firm_stop() {
        let near = at(4_999.5);
        let stopped = near.moved_by_pixels(300.0, LINE, MAX);
        assert_eq!(stopped, ScrollPosition::whole(MAX));
        // Coming back off the stop tracks from the stop, not from where the
        // overshoot would have been.
        let back = stopped.moved_by_pixels(-3.0, LINE, MAX);
        assert!(close(back.as_rows(), 5_000.0 - 3.0 / 15.0));
        assert_eq!(back.rows, MAX);
    }

    #[test]
    fn a_shrunken_history_clamps_a_resting_offset() {
        let resting = at(80.4);
        assert_eq!(resting.clamped(200), resting);
        assert_eq!(resting.clamped(50), ScrollPosition::whole(50));
        assert_eq!(resting.clamped(0), ScrollPosition::LIVE);
    }

    #[test]
    fn a_font_size_change_rescales_the_offset_with_the_rows() {
        let resting = at(12.5);
        assert_eq!(resting.shift(15.0, 2.0), 7.5);
        assert_eq!(resting.shift(22.0, 2.0), 11.0);
        assert!(resting.shift(22.0, 2.0) < 22.0);
        // And it keeps tracking in the new metric.
        let moved = resting.moved_by_pixels(11.0, 22.0, MAX);
        assert!(close(moved.as_rows(), 13.0));
    }

    #[test]
    fn the_paint_offset_is_whole_device_pixels() {
        for scale in [1.0, 2.0, 3.0] {
            for step in 0..200 {
                let position = at(10.0 + f64::from(step) * 0.005);
                let device = position.shift(LINE, scale) * scale;
                assert!((device - device.round()).abs() < 1e-4, "{position:?}");
                assert!(position.shift(LINE, scale) <= LINE);
            }
        }
        assert_eq!(at(10.5).shift(LINE, 0.0), 8.0, "bad scale means 1x");
    }

    #[test]
    fn leaving_live_needs_deliberate_travel_and_loses_none_of_it() {
        let mut position = ScrollPosition::LIVE;
        let mut pending = 0.0;
        for _ in 0..3 {
            let step = position.step_pixels(pending, 0.5, LINE, MAX);
            (position, pending) = (step.position, step.pending);
            assert_eq!(position, ScrollPosition::LIVE);
        }
        let step = position.step_pixels(pending, 0.5, LINE, MAX);
        assert!(close(step.position.as_rows(), 2.0 / 15.0));
        assert_eq!(step.pending, 0.0);

        // Jitter around a resting finger never accumulates.
        let mut pending = 0.0;
        for delta in [0.9, -0.9, 0.9, -0.9, 0.9] {
            let step = ScrollPosition::LIVE.step_pixels(pending, delta, LINE, MAX);
            assert_eq!(step.position, ScrollPosition::LIVE);
            pending = step.pending;
        }
        // Away from live there is no slop at all.
        let step = at(3.0).step_pixels(0.0, 0.25, LINE, MAX);
        assert!(close(step.position.as_rows(), 3.0 + 0.25 / 15.0));
    }

    #[test]
    fn a_line_wheel_steps_from_the_nearest_row() {
        assert_eq!(at(7.0).nearest_row(), 7);
        assert_eq!(at(6.8).nearest_row(), 7);
        assert_eq!(at(6.4).nearest_row(), 6);
    }

    #[test]
    fn hostile_input_is_inert() {
        let resting = at(9.5);
        assert_eq!(resting.moved_by_pixels(f32::NAN, LINE, MAX), resting);
        assert_eq!(resting.moved_by_pixels(f32::INFINITY, LINE, MAX), resting);
        assert_eq!(resting.moved_by_pixels(1.0, f32::NAN, MAX), resting);
        assert_eq!(
            ScrollPosition::from_rows(f64::NAN, MAX),
            ScrollPosition::LIVE
        );
        // A degenerate line height is treated as one pixel, not divided by.
        let zero_height = resting.moved_by_pixels(1.0, 0.0, MAX);
        assert!(close(zero_height.as_rows(), 10.5));
    }
}
