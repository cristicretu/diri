//! A short eased scroll to a find match that is off screen.
//!
//! Stepping to a match used to cut to it, and a reader who was looking at one
//! part of the history was suddenly looking at another with no sense of which
//! way they had gone. Pixel-smooth scrollback can carry the view there
//! instead. The view is pinned to the find capture for the whole trip, so
//! every row on the way is already in memory and the glide fetches nothing.
//!
//! A pure function of elapsed time: the element samples it once per painted
//! frame and asks for the next frame only while it is running.

use std::time::{Duration, Instant};

/// Long enough to read as motion, short enough that holding Return to walk
/// through matches never waits on it.
pub const DURATION: Duration = Duration::from_millis(160);

/// Longest trip, as a share of the window's height. A match hundreds of rows
/// away is reached by cutting most of the way and gliding the rest: the
/// direction still reads, and a glide never shapes more than about a screen
/// of rows it will only show for a frame.
pub const MAX_TRAVEL: f64 = 0.75;

/// Rows back from the live edge, fractional, as `ScrollPosition::as_rows`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollGlide {
    from: f64,
    to: f64,
    started: Instant,
}

impl ScrollGlide {
    /// A glide from what is `shown` to `target`, or `None` when the view does
    /// not have to move.
    #[must_use]
    pub fn new(shown: f64, target: f64, visible_rows: usize, now: Instant) -> Option<Self> {
        let distance = shown - target;
        if !distance.is_finite() || distance.abs() < f64::EPSILON {
            return None;
        }
        let reach = (visible_rows as f64 * MAX_TRAVEL).max(1.0);
        let from = if distance.abs() > reach {
            target + reach * distance.signum()
        } else {
            shown
        };
        Some(Self {
            from,
            to: target,
            started: now,
        })
    }

    /// Where the first frame is painted: `shown`, or nearer when the trip was
    /// shortened.
    #[must_use]
    pub const fn start(&self) -> f64 {
        self.from
    }

    #[must_use]
    pub const fn target(&self) -> f64 {
        self.to
    }

    #[must_use]
    pub fn is_finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= DURATION
    }

    /// The position to paint at `now`. Exactly the target once finished, so
    /// the view rests on the whole row navigation chose.
    #[must_use]
    pub fn sample(&self, now: Instant) -> f64 {
        if self.is_finished(now) {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(self.started);
        let progress = (elapsed.as_secs_f64() / DURATION.as_secs_f64()).clamp(0.0, 1.0);
        // Ease-out cubic: the view answers the keypress at full speed and
        // settles onto the match.
        let remaining = 1.0 - progress;
        let eased = 1.0 - remaining * remaining * remaining;
        self.from + (self.to - self.from) * eased
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_trip_starts_where_the_reader_is_and_lands_exactly_on_the_match() {
        let now = Instant::now();
        let glide = ScrollGlide::new(40.0, 52.0, 30, now).unwrap();
        assert_eq!(glide.start(), 40.0);
        assert_eq!(glide.sample(now), 40.0);
        assert_eq!(glide.sample(now + DURATION), 52.0);
        assert_eq!(glide.sample(now + Duration::from_secs(60)), 52.0);
        assert!(glide.is_finished(now + DURATION));
        assert!(!glide.is_finished(now + DURATION - Duration::from_millis(1)));
    }

    #[test]
    fn a_long_trip_is_cut_short_on_the_side_it_came_from() {
        let now = Instant::now();
        // 600 rows further back: arrive from below, the way the view was going.
        let back = ScrollGlide::new(10.0, 610.0, 40, now).unwrap();
        assert_eq!(back.start(), 610.0 - 30.0);
        // And toward the live edge: arrive from above.
        let forward = ScrollGlide::new(610.0, 10.0, 40, now).unwrap();
        assert_eq!(forward.start(), 10.0 + 30.0);
    }

    #[test]
    fn a_match_already_in_place_does_not_glide() {
        assert!(ScrollGlide::new(12.0, 12.0, 30, Instant::now()).is_none());
        assert!(ScrollGlide::new(f64::NAN, 12.0, 30, Instant::now()).is_none());
    }

    #[test]
    fn the_view_never_overshoots_or_moves_backwards() {
        let now = Instant::now();
        let glide = ScrollGlide::new(5.0, 25.0, 40, now).unwrap();
        let mut previous = glide.sample(now);
        for millis in 1..=200 {
            let position = glide.sample(now + Duration::from_millis(millis));
            assert!(position >= previous, "{millis} ms went backwards");
            assert!(position <= 25.0, "{millis} ms overshot");
            previous = position;
        }
        // Ease-out: more than half the way there at half time.
        let halfway = glide.sample(now + DURATION / 2);
        assert!(halfway > 15.0 + 2.0, "{halfway}");
    }
}
