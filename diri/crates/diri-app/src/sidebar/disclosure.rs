//! Interruptible folder disclosure motion. Rows stay at their natural size;
//! a growing clip reveals them while their short, staggered arrivals settle.
use std::time::{Duration, Instant};

const OPEN: Duration = Duration::from_millis(300);
const CLOSE: Duration = Duration::from_millis(180);

#[derive(Clone, Debug)]
pub(super) struct Frame {
    pub reveal: f32,
    pub rows: Vec<f32>,
    pub animating: bool,
}

pub(super) struct Disclosure {
    expanded: bool,
    started: Instant,
    from: Frame,
}

impl Disclosure {
    pub fn new(expanded: bool, count: usize, now: Instant) -> Self {
        Self {
            expanded,
            started: now,
            from: Self::settled(expanded, count),
        }
    }

    fn settled(expanded: bool, count: usize) -> Frame {
        let value = if expanded { 1.0 } else { 0.0 };
        Frame {
            reveal: value,
            rows: vec![value; count],
            animating: false,
        }
    }

    pub fn update(
        &mut self,
        expanded: bool,
        count: usize,
        now: Instant,
        reduce_motion: bool,
    ) -> Frame {
        if reduce_motion {
            self.expanded = expanded;
            self.from = Self::settled(expanded, count);
        } else if expanded != self.expanded {
            self.from = self.sample(now);
            self.from.rows.resize(count, self.from.reveal);
            self.from.animating = true;
            self.expanded = expanded;
            self.started = now;
        } else if self.from.rows.len() != count {
            // Data changes aren't disclosure gestures. Keep new/removed rows
            // current without replaying an arrival on every store update.
            self.from = Self::settled(expanded, count);
        }
        let frame = self.sample(now);
        if !frame.animating {
            self.from = frame.clone();
        }
        frame
    }

    fn sample(&self, now: Instant) -> Frame {
        if !self.from.animating {
            return self.from.clone();
        }
        let duration = if self.expanded { OPEN } else { CLOSE };
        let progress = (now.saturating_duration_since(self.started).as_secs_f32()
            / duration.as_secs_f32())
        .clamp(0.0, 1.0);
        let target = if self.expanded { 1.0 } else { 0.0 };
        let interpolate = |from: f32, t: f32| from + (target - from) * (1.0 - (1.0 - t).powi(3));
        Frame {
            reveal: interpolate(self.from.reveal, progress),
            rows: self
                .from
                .rows
                .iter()
                .enumerate()
                .map(|(index, from)| {
                    // 22 ms between arrivals, capped so large projects finish
                    // with the same 300 ms gesture. Exit fades all rows together.
                    let delay = if self.expanded {
                        (index as f32 * 0.073).min(0.36)
                    } else {
                        0.0
                    };
                    let local = ((progress - delay) / (1.0 - delay)).clamp(0.0, 1.0);
                    interpolate(*from, local)
                })
                .collect(),
            animating: progress < 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversing_a_disclosure_preserves_every_rows_position_and_opacity() {
        let now = Instant::now();
        let mut motion = Disclosure::new(false, 6, now);
        motion.update(true, 6, now, false);
        let halfway = now + Duration::from_millis(120);
        let before = motion.update(true, 6, halfway, false);
        let reversed = motion.update(false, 6, halfway, false);
        assert_eq!(before.reveal, reversed.reveal);
        assert_eq!(before.rows, reversed.rows);
        let closing = motion.update(false, 6, halfway + Duration::from_millis(50), false);
        assert!(closing.reveal < reversed.reveal);
        let reopened = motion.update(true, 6, halfway + Duration::from_millis(50), false);
        assert_eq!(closing.rows, reopened.rows);
        assert_eq!(closing.reveal, reopened.reveal);
    }

    #[test]
    fn initial_render_and_reduced_motion_are_settled_without_frame_requests() {
        let now = Instant::now();
        let mut motion = Disclosure::new(true, 6, now);
        assert!(!motion.update(true, 6, now, false).animating);
        assert!(motion.update(false, 6, now, false).animating);
        let reduced = motion.update(false, 6, now, true);
        assert_eq!(reduced.reveal, 0.0);
        assert_eq!(reduced.rows, vec![0.0; 6]);
        assert!(!reduced.animating);
    }

    #[test]
    fn long_lists_stagger_but_finish_on_time() {
        let now = Instant::now();
        let mut motion = Disclosure::new(false, 200, now);
        motion.update(true, 200, now, false);
        let frame = motion.update(true, 200, now + Duration::from_millis(100), false);
        assert!(frame.rows[0] > frame.rows[3]);
        assert!(frame.rows[3] > frame.rows[199]);
        let final_frame = motion.update(true, 200, now + OPEN, false);
        assert_eq!(final_frame.rows, vec![1.0; 200]);
        assert!(!final_frame.animating);
    }
}
