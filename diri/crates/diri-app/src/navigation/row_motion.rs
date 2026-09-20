//! How result rows travel between two keystrokes.
//!
//! The list itself never animates: ranked rows, the highlight, and every hit
//! target are final the moment a key lands, so Return and clicks act on the new
//! order at once. This only answers "where is this row painted right now".
//!
//! Rows are text on glass with no fill of their own, so two of them can never
//! share pixels. Two earlier versions failed on rendered frames for exactly
//! that reason: sliding every survivor printed rows over each other wherever
//! two traded places, and hiding the rows in a slider's way until it had passed
//! left the list full of holes for most of the gap between two keystrokes.
//!
//! So a row slides only when its whole path is free: it kept its order, and
//! every slot between where it was and where it is going holds nothing but
//! other sliders. And only once the matches fit the view: that is the list
//! closing up after rows dropped out, late in a query, when it is being read.
//! While a query still matches more rows than fit, rows from below the fold
//! refill the slots and every keystroke is the plain cut it always was. The
//! first row never moves at all: it is the one Return runs.
//!
//! Sampling is a pure function of elapsed time and schedules nothing.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// A row's identity across rankings, hashed from whatever names it on its page.
pub(super) type RowKey = u64;

pub(super) fn row_key(page: u8, identity: impl Hash) -> RowKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    page.hash(&mut hasher);
    identity.hash(&mut hasher);
    hasher.finish()
}

/// A keystroke resets the list to its top, so only the rows that fit there can
/// be seen moving. A few more are tracked because a row may rise into view
/// from just below the fold, and to tell a list that fits from one that does
/// not.
pub(super) const TRACKED_ROWS: usize = 12;

const SLIDE: Duration = Duration::from_millis(140);
/// Past this many rows a slide is a streak the eye cannot follow.
const FOLLOWABLE_ROWS: usize = 3;

#[derive(Clone, Copy, Debug)]
struct Track {
    /// Distance from the final slot when the slide began, in points.
    from: f32,
    started: Instant,
}

/// How far from its slot a row that started `from` away is after `elapsed`.
/// Ease-out, so a retarget, which restarts from the painted position, begins
/// at full speed and never reads as a stall.
pub(super) fn offset_after(from: f32, elapsed: Duration) -> f32 {
    let t = (elapsed.as_secs_f32() / SLIDE.as_secs_f32()).clamp(0.0, 1.0);
    from * (1.0 - t).powi(3)
}

/// The rows that slide, as `(slot, previous slot)`: survivors that keep their
/// relative order, travel a distance the eye can follow, and cross no slot
/// that a row painted in place occupies.
fn sliders(before: &[RowKey], after: &[RowKey], visible_rows: usize) -> Vec<(usize, usize)> {
    // A list longer than its view is refilled from below the fold as rows
    // leave. Rows drifting in at its bottom edge, a hole above them, is the
    // flicker fast typing must not have; it moves once it fits.
    if after.len() > visible_rows {
        return Vec::new();
    }
    let survivors: Vec<(usize, usize)> = after
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(index, key)| {
            let was = before.iter().position(|row| row == key)?;
            // Leaving the first slot would mean starting underneath the row
            // that just took it.
            (was > 0 && was.abs_diff(index) <= FOLLOWABLE_ROWS).then_some((index, was))
        })
        .collect();
    // Longest increasing run of previous slots, quadratic over a dozen rows.
    let mut best: Vec<(usize, Option<usize>)> = Vec::with_capacity(survivors.len());
    for (position, (_, was)) in survivors.iter().enumerate() {
        let link = (0..position)
            .filter(|earlier| survivors[*earlier].1 < *was)
            .max_by_key(|earlier| best[*earlier].0);
        best.push((link.map_or(1, |link| best[link].0 + 1), link));
    }
    let mut run = Vec::new();
    let mut cursor = (0..best.len()).max_by_key(|position| (best[*position].0, *position));
    while let Some(position) = cursor {
        run.push(survivors[position]);
        cursor = best[position].1;
    }
    run.reverse();

    // Dropping one slider can put a row in another's way, so repeat until the
    // set stops shrinking.
    loop {
        let clear = |(index, was): &(usize, usize)| {
            (*index.min(was)..=*index.max(was))
                .all(|slot| slot >= after.len() || run.iter().any(|(slider, _)| *slider == slot))
        };
        let kept: Vec<_> = run.iter().copied().filter(clear).collect();
        if kept.len() == run.len() {
            return kept;
        }
        run = kept;
    }
}

#[derive(Default)]
pub(super) struct RowMotion {
    tracks: HashMap<RowKey, Track>,
    settles_at: Option<Instant>,
}

impl RowMotion {
    /// Start the rows of `after` that can slide from where they are painted
    /// at `now`. `before` and `after` are the leading rows of the list on
    /// either side of one change, in order.
    pub(super) fn retarget(
        &mut self,
        before: &[RowKey],
        after: &[RowKey],
        visible_rows: usize,
        row_height: f32,
        now: Instant,
    ) {
        let tracks: HashMap<RowKey, Track> = sliders(before, after, visible_rows)
            .into_iter()
            .filter_map(|(index, was)| {
                let key = after[index];
                let from = (was as f32 - index as f32) * row_height + self.offset(key, now);
                (from != 0.0).then_some((key, Track { from, started: now }))
            })
            .collect();
        self.settles_at = (!tracks.is_empty()).then(|| now + SLIDE);
        self.tracks = tracks;
    }

    /// Every row at rest: page changes, and anything Reduce Motion covers.
    pub(super) fn snap(&mut self) {
        self.tracks.clear();
        self.settles_at = None;
    }

    /// Vertical distance from the row's final slot, in points.
    pub(super) fn offset(&self, key: RowKey, now: Instant) -> f32 {
        self.tracks.get(&key).map_or(0.0, |track| {
            offset_after(track.from, now.saturating_duration_since(track.started))
        })
    }

    /// Whether another frame would paint anything different.
    pub(super) fn is_moving(&self, now: Instant) -> bool {
        self.settles_at.is_some_and(|end| now < end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: f32 = 36.0;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn moved(before: &[RowKey], after: &[RowKey]) -> (RowMotion, Instant) {
        let start = Instant::now();
        let mut motion = RowMotion::default();
        motion.retarget(before, after, 9, ROW, start);
        (motion, start)
    }

    #[test]
    fn survivors_close_the_gap_a_leaver_left_and_land_exactly() {
        // Rows 2 and 3 drop out; 4 and 5 move up two slots.
        let (motion, start) = moved(&[1, 2, 3, 4, 5], &[1, 4, 5]);

        assert_eq!(motion.offset(4, start), 2.0 * ROW);
        assert_eq!(motion.offset(5, start), 2.0 * ROW);

        let midway = motion.offset(4, start + ms(70));
        assert!(
            midway > 0.0 && midway < ROW,
            "ease-out is past half: {midway}"
        );

        assert_eq!(motion.offset(4, start + SLIDE), 0.0);
        assert_eq!(motion.offset(5, start + ms(5_000)), 0.0);
    }

    #[test]
    fn sliding_rows_never_close_on_each_other() {
        // 5 travels three slots, 3 only one: the faster row must not catch up.
        let (motion, start) = moved(&[1, 2, 3, 4, 6, 5], &[1, 3, 5]);
        assert_eq!(motion.offset(3, start), ROW);
        assert_eq!(motion.offset(5, start), 3.0 * ROW);
        for step in 0..=14 {
            let now = start + ms(step * 10);
            let upper = ROW + motion.offset(3, now);
            let lower = 2.0 * ROW + motion.offset(5, now);
            assert!(lower - upper >= ROW - 0.01, "rows overlap at {step}0 ms");
        }
    }

    #[test]
    fn the_first_row_is_painted_in_place_whatever_it_did() {
        let (motion, start) = moved(&[1, 2, 3], &[3, 1, 2]);
        assert_eq!(motion.offset(3, start), 0.0);
        // The row it displaced would have to start underneath it, so it does
        // not slide either.
        assert_eq!(motion.offset(1, start), 0.0);
        assert!(!motion.is_moving(start));
    }

    #[test]
    fn a_leaver_does_not_slide_and_is_forgotten() {
        let (motion, start) = moved(&[1, 2, 3], &[1, 3]);
        assert_eq!(motion.offset(2, start), 0.0);
        assert_eq!(motion.offset(3, start), ROW);
    }

    #[test]
    fn a_row_that_changes_order_is_painted_in_its_slot() {
        // 4 jumped over 2 and 3, which keep their slots.
        let (motion, start) = moved(&[1, 9, 2, 3, 4], &[1, 4, 2, 3]);
        assert_eq!(motion.offset(4, start), 0.0);
        assert!(!motion.is_moving(start));
    }

    #[test]
    fn no_row_slides_through_a_row_painted_in_place() {
        // 7 is new and sits in the slot row 3 would start from.
        let (motion, start) = moved(&[1, 2, 3], &[1, 3, 7]);
        assert_eq!(motion.offset(3, start), 0.0);
        assert_eq!(motion.offset(7, start), 0.0);

        // 4 jumped to slot 1 and is painted there, so 2 and 3, one slot down
        // from where they were, may still slide: their paths stop short of it.
        let (motion, start) = moved(&[1, 2, 3, 4], &[1, 4, 2, 3]);
        assert_eq!(
            motion.offset(2, start),
            0.0,
            "slot 1 is its start and is taken"
        );
        assert_eq!(motion.offset(3, start), 0.0, "and then row 2 is in the way");
    }

    #[test]
    fn a_list_that_is_backfilled_does_not_move_at_all() {
        // The usual early keystroke: rows leave, rows from below the fold take
        // the freed slots. Nothing has a clear path, so it is a plain cut.
        let (motion, start) = moved(&[1, 2, 3, 4, 5], &[1, 3, 5, 8, 9]);
        assert!(!motion.is_moving(start));
    }

    #[test]
    fn a_retarget_continues_from_the_painted_position_and_never_queues() {
        let (mut motion, start) = moved(&[1, 2, 3, 4, 5], &[1, 4, 5]);
        // 60 ms later row 4 drops out too: row 5 goes from slot 2 to slot 1.
        let second = start + ms(60);
        let painted = motion.offset(5, second);
        assert!(painted > 0.0 && painted < 2.0 * ROW);
        motion.retarget(&[1, 4, 5], &[1, 5], 9, ROW, second);

        // One slot alone would start at ROW; it starts from the row's pixels
        // instead, and one slide later it is home.
        assert_eq!(motion.offset(5, second), ROW + painted);
        assert_eq!(motion.offset(5, second + SLIDE), 0.0);
        assert!(!motion.is_moving(second + SLIDE));
    }

    #[test]
    fn a_row_that_would_streak_across_the_list_is_painted_in_place() {
        let (motion, start) = moved(&[1, 2, 3, 4, 5, 6, 7, 8], &[1, 8]);
        assert_eq!(motion.offset(8, start), 0.0);
    }

    #[test]
    fn a_list_longer_than_its_view_cuts_and_one_that_fits_closes_up() {
        let before: Vec<RowKey> = (1..=12).collect();
        let still_long: Vec<RowKey> = (1..=12).filter(|row| *row != 4).collect();
        let (motion, start) = moved(&before, &still_long);
        assert!(!motion.is_moving(start));

        // Nine rows fit: 10 rises from under the fold with the rest.
        let fits: Vec<RowKey> = (1..=12).filter(|row| ![2, 3, 12].contains(row)).collect();
        let (motion, start) = moved(&before, &fits);
        assert_eq!(motion.offset(4, start), 2.0 * ROW);
        assert_eq!(motion.offset(10, start), 2.0 * ROW);
    }

    #[test]
    fn motion_reports_moving_only_until_everything_has_landed() {
        let start = Instant::now();
        let mut motion = RowMotion::default();
        assert!(!motion.is_moving(start));

        motion.retarget(&[1, 2], &[1, 2], 9, ROW, start);
        assert!(
            !motion.is_moving(start),
            "an unchanged list has nothing to paint"
        );

        motion.retarget(&[1, 2, 3], &[1, 3], 9, ROW, start);
        assert!(motion.is_moving(start));
        assert!(motion.is_moving(start + ms(139)));
        assert!(!motion.is_moving(start + ms(140)));

        motion.retarget(&[1, 2, 3], &[1, 3], 9, ROW, start);
        motion.snap();
        assert!(!motion.is_moving(start));
        assert_eq!(motion.offset(3, start), 0.0);
    }
}
