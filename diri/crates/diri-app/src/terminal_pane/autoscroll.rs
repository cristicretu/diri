//! Speed curve and row accumulator for selection autoscroll.
//!
//! Dragging a selection past the top or bottom of the grid scrolls the view,
//! faster the further out the pointer goes, the way AppKit text views do. The
//! pointer's distance past the edge is the only input: holding it still holds
//! the speed, so the reader steers with one axis.

use std::time::Duration;

/// Pointer travel past the edge that still counts as on it. A selection that
/// ends on the first or last row lands a few pixels outside as often as not;
/// scrolling there would move the text out from under a precise release.
pub(super) const DEAD_ZONE_PX: f32 = 4.0;

/// Speed just past the dead zone. Two rows a second is slow enough to stop on
/// the intended row by eye, and still visibly moving, so the reader knows the
/// gesture is live.
pub(super) const MIN_LINES_PER_SEC: f32 = 2.0;

/// Ceiling, reached [`RAMP_PX`] past the dead zone. A few screens a second
/// crosses long history quickly without the rows turning into a blur that
/// cannot be stopped on.
pub(super) const MAX_LINES_PER_SEC: f32 = 240.0;

/// Distance over which the speed ramps from minimum to maximum. About a third
/// of a laptop screen: the pointer reaches the cap while it is still in the
/// window's neighborhood, and the first ~50 px stay in single-digit speeds
/// where precise selection happens.
pub(super) const RAMP_PX: f32 = 300.0;

/// Longest frame interval a tick will integrate. A stalled main thread must
/// not turn into one leap of many rows when it resumes.
pub(super) const MAX_TICK: Duration = Duration::from_millis(50);

/// Tick period: one display frame at 60 Hz. The fractional position repaints
/// every tick, so this is the motion's frame rate.
pub(super) const TICK: Duration = Duration::from_millis(16);

/// Rows per second for a pointer `distance_px` past the grid edge.
///
/// Zero inside the dead zone, then `MIN + (MAX - MIN) * t²` with `t` the
/// share of [`RAMP_PX`] covered. The quadratic starts with zero slope, so
/// speed changes least where the reader is being most careful, and grows
/// fastest where they plainly want distance.
#[must_use]
pub(super) fn lines_per_second(distance_px: f32) -> f32 {
    if !distance_px.is_finite() || distance_px <= DEAD_ZONE_PX {
        return 0.0;
    }
    let t = ((distance_px - DEAD_ZONE_PX) / RAMP_PX).min(1.0);
    MIN_LINES_PER_SEC + (MAX_LINES_PER_SEC - MIN_LINES_PER_SEC) * t * t
}

/// Turns speed and elapsed time into row travel, carrying what a tick could
/// not apply.
///
/// The smooth path applies every fraction as sub-row scroll, so nothing is
/// carried. Under reduced motion the view moves in whole rows, and the
/// remainder waits here until it amounts to one; two rows a second is then a
/// row every half second rather than no motion at all.
#[derive(Debug, Default)]
pub(super) struct Accumulator {
    carry: f64,
}

impl Accumulator {
    /// Rows to scroll this tick, positive toward history. `velocity` is signed
    /// rows per second.
    pub(super) fn advance(&mut self, velocity: f32, elapsed: Duration, whole_rows: bool) -> f64 {
        let elapsed = elapsed.min(MAX_TICK).as_secs_f64();
        let travel = f64::from(velocity) * elapsed;
        // Reversing direction starts fresh: travel owed upward must not be
        // paid out as a hesitation on the way down.
        if travel * self.carry < 0.0 || travel == 0.0 {
            self.carry = 0.0;
        }
        let total = self.carry + travel;
        if !whole_rows {
            self.carry = 0.0;
            return total;
        }
        let rows = total.trunc();
        self.carry = total - rows;
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_zone_holds_still() {
        assert_eq!(lines_per_second(0.0), 0.0);
        assert_eq!(lines_per_second(DEAD_ZONE_PX), 0.0);
        assert_eq!(lines_per_second(-20.0), 0.0);
        assert_eq!(lines_per_second(f32::NAN), 0.0);
    }

    #[test]
    fn speed_starts_slow_and_ramps() {
        let edge = lines_per_second(DEAD_ZONE_PX + 0.01);
        assert!((edge - MIN_LINES_PER_SEC).abs() < 0.01, "{edge}");
        assert!(lines_per_second(20.0) < 3.0);
        let hundred = lines_per_second(100.0);
        assert!((20.0..40.0).contains(&hundred), "{hundred}");
        assert_eq!(lines_per_second(DEAD_ZONE_PX + RAMP_PX), MAX_LINES_PER_SEC);
        assert_eq!(lines_per_second(5_000.0), MAX_LINES_PER_SEC);
    }

    #[test]
    fn speed_is_monotonic() {
        let mut last = 0.0;
        for px in 0..400 {
            let speed = lines_per_second(px as f32);
            assert!(speed >= last, "{px}px: {speed} < {last}");
            last = speed;
        }
    }

    #[test]
    fn smooth_travel_is_exact_and_carries_nothing() {
        let mut acc = Accumulator::default();
        let rows = acc.advance(60.0, TICK, false);
        assert!((rows - 0.96).abs() < 1e-9, "{rows}");
        assert_eq!(acc.carry, 0.0);
    }

    #[test]
    fn whole_rows_accumulate_slow_speeds() {
        // Two rows a second at 16 ms ticks: a row roughly every 31 ticks.
        let mut acc = Accumulator::default();
        let mut moved = 0.0;
        let mut first = None;
        for tick in 0..63 {
            let rows = acc.advance(2.0, TICK, true);
            assert_eq!(rows.fract(), 0.0);
            if rows != 0.0 && first.is_none() {
                first = Some(tick);
            }
            moved += rows;
        }
        assert_eq!(first, Some(31));
        assert_eq!(moved, 2.0);
    }

    #[test]
    fn reversing_drops_the_carry() {
        let mut acc = Accumulator::default();
        acc.advance(30.0, TICK, true);
        assert!(acc.carry > 0.0);
        let rows = acc.advance(-30.0, TICK, true);
        assert_eq!(rows, 0.0);
        assert!(acc.carry < 0.0);
    }

    #[test]
    fn a_stalled_tick_is_capped() {
        let mut acc = Accumulator::default();
        let rows = acc.advance(MAX_LINES_PER_SEC, Duration::from_secs(2), false);
        assert!((rows - f64::from(MAX_LINES_PER_SEC) * 0.05).abs() < 1e-6);
    }
}
