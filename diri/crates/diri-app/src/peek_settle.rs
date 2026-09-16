//! A finite presentation transition. Sampling has no scheduling or terminal effects.
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Settle {
    from: f32,
    pub(crate) to: f32,
    started: Instant,
}

impl Settle {
    pub(crate) const DURATION: Duration = Duration::from_millis(200);
    pub(crate) fn new(from: f32, to: f32, started: Instant) -> Option<Self> {
        (from != to).then_some(Self { from, to, started })
    }
    pub(crate) fn sample(self, now: Instant) -> (f32, bool) {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / Self::DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        (self.from + (self.to - self.from) * eased, t >= 1.0)
    }
}
