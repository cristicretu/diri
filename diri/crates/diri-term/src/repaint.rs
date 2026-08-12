use std::time::Duration;

/// What the terminal host should do after applying fresh grid damage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepaintAction {
    RepaintNow,
    Schedule(Duration),
    None,
}

/// Converts terminal damage into host repaint requests.
pub struct RepaintPacer {
    minimum_interval: Duration,
    last_repaint: Option<Duration>,
    scheduled_for: Option<Duration>,
    interactive_damage_budget: u8,
}

const INTERACTIVE_DAMAGE_BUDGET: u8 = 2;

impl RepaintPacer {
    #[must_use]
    pub fn new(minimum_interval: Duration) -> Self {
        Self {
            minimum_interval,
            last_repaint: None,
            scheduled_for: None,
            interactive_damage_budget: 0,
        }
    }

    /// Make the next two damage publications paint immediately.
    ///
    /// Keyboard input uses this to avoid making its echo wait behind a timer
    /// armed for unrelated streaming output. Two covers one trailing update
    /// already in flight plus the actual response. The existing timer becomes
    /// a harmless no-op when it fires.
    pub fn prioritize_interactive_damage(&mut self) {
        self.interactive_damage_budget = INTERACTIVE_DAMAGE_BUDGET;
    }

    /// Record newly applied damage.
    ///
    /// The first frame paints immediately. Bursts receive one trailing timer,
    /// so every grid update reaches the buffer while host rendering is capped.
    pub fn on_damage(&mut self, now: Duration) -> RepaintAction {
        if self.interactive_damage_budget > 0 {
            self.interactive_damage_budget -= 1;
            self.last_repaint = Some(now);
            self.scheduled_for = None;
            return RepaintAction::RepaintNow;
        }
        let Some(last_repaint) = self.last_repaint else {
            self.last_repaint = Some(now);
            return RepaintAction::RepaintNow;
        };
        let next_repaint = last_repaint.saturating_add(self.minimum_interval);
        if now >= next_repaint && self.scheduled_for.is_none() {
            self.last_repaint = Some(now);
            return RepaintAction::RepaintNow;
        }
        if self.scheduled_for.is_some() {
            return RepaintAction::None;
        }

        self.scheduled_for = Some(next_repaint);
        RepaintAction::Schedule(next_repaint.saturating_sub(now))
    }

    /// Complete the one trailing repaint scheduled by [`Self::on_damage`].
    pub fn on_timer(&mut self, now: Duration) -> bool {
        let Some(scheduled_for) = self.scheduled_for else {
            return false;
        };
        if now < scheduled_for {
            return false;
        }

        self.scheduled_for = None;
        self.last_repaint = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RepaintAction, RepaintPacer};

    #[test]
    fn sixty_fps_damage_is_paced_without_losing_the_trailing_repaint() {
        let interval = Duration::from_millis(50);
        let mut pacer = RepaintPacer::new(interval);
        let mut scheduled_for = None;
        let mut repaint_count = 0;

        for frame in 0..60 {
            let now = Duration::from_millis(frame * 16);
            if let Some(deadline) = scheduled_for.filter(|deadline| *deadline <= now) {
                assert!(pacer.on_timer(deadline));
                repaint_count += 1;
                scheduled_for = None;
            }

            match pacer.on_damage(now) {
                RepaintAction::RepaintNow => repaint_count += 1,
                RepaintAction::Schedule(delay) => {
                    assert!(scheduled_for.is_none());
                    scheduled_for = Some(now + delay);
                }
                RepaintAction::None => {}
            }
        }

        if let Some(deadline) = scheduled_for {
            assert!(pacer.on_timer(deadline));
            repaint_count += 1;
        }

        assert!(
            repaint_count <= 21,
            "60 grid frames should require at most 21 paints, got {repaint_count}"
        );
        assert!(
            repaint_count >= 19,
            "pacing should remain responsive, got only {repaint_count} paints"
        );
    }

    #[test]
    fn interactive_damage_bypasses_an_armed_background_timer() {
        let interval = Duration::from_millis(16);
        let mut pacer = RepaintPacer::new(interval);
        assert_eq!(pacer.on_damage(Duration::ZERO), RepaintAction::RepaintNow);
        assert_eq!(
            pacer.on_damage(Duration::from_millis(2)),
            RepaintAction::Schedule(Duration::from_millis(14))
        );

        pacer.prioritize_interactive_damage();
        assert_eq!(
            pacer.on_damage(Duration::from_millis(4)),
            RepaintAction::RepaintNow
        );
        assert_eq!(
            pacer.on_damage(Duration::from_millis(5)),
            RepaintAction::RepaintNow
        );
        assert_eq!(
            pacer.on_damage(Duration::from_millis(6)),
            RepaintAction::Schedule(Duration::from_millis(15))
        );
        assert!(!pacer.on_timer(Duration::from_millis(16)));
        assert!(pacer.on_timer(Duration::from_millis(21)));
    }
}
