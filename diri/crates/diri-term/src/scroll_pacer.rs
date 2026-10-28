//! Display-paced trackpad scrolling.
//!
//! Precise wheel events arrive on the main thread at the trackpad's rate, with
//! a few milliseconds of jitter, unsynchronised with the display. Applying each
//! one as it is handled shows that jitter: two events land in one refresh and
//! none in the next, so that refresh repeats the previous position.
//!
//! The pacer instead records each event's cumulative travel against the time
//! it was sent, and every frame samples that curve at the frame's display
//! time minus a fixed delay, interpolating between the two events around it.
//! A steady gesture then moves every refresh by its velocity times the frame
//! interval, whatever the events' phase against the display and whenever in
//! its refresh the main thread got round to the frame.
//!
//! Times come from the platform where it has them: the hardware time of each
//! event (`NSEvent.timestamp`) and the refresh the frame is drawn for (the
//! display link's output time). Without them an event is timed at its
//! handling, pulled back onto the input's own schedule when the main thread
//! was late with it, and a frame at its start plus the usual lead.
//!
//! The delay is one input interval plus the worst recent lateness of the
//! main thread, so the event a frame needs has been handled by the time it
//! is drawn; it moves only a little per frame, because a sampling point that
//! jumped would show as the very speed change pacing is there to hide.
//!
//! The first event of a gesture is released whole, so a gesture starts moving
//! on its first frame, and once input stops the curve's end is reached within
//! the delay and frames stop: an idle pacer schedules nothing. Fingers coming
//! down on the trackpad stop the travel still held, as they stop momentum.
//!
//! This is the same construction as AppKit's responsive scrolling and the
//! input resampling Android and Chromium do at vsync.

use std::time::{Duration, Instant};

/// Events closer together than this belong to one gesture. A finger at rest
/// on the trackpad or the gap before momentum can pause a few frames.
const GESTURE_GAP: Duration = Duration::from_millis(60);

/// Bounds on the input interval the pacer assumes: a 240 Hz trackpad to a
/// 60 Hz source.
const MIN_INTERVAL: Duration = Duration::from_millis(4);
const MAX_INTERVAL: Duration = Duration::from_micros(16_700);

/// Bound on how far behind the frame's start the gesture is sampled
/// (interval, lateness and margin together). Lateness beyond it is a stall
/// pacing cannot hide.
const MAX_DELAY: f64 = 0.025;

/// Slack on top of the interval and lateness.
const MARGIN: f64 = 0.001;

/// How fast the delay may rise per frame. Raising it by 1 ms in an 8.3 ms
/// refresh shows as a 12% slower step for that frame, below what the eye
/// picks out.
const DELAY_RISE_PER_FRAME: f64 = 0.001;

/// Per event, how much of the worst lateness seen is kept: it fades over
/// about a second of input at 120 Hz.
const LATENESS_HOLD: f64 = 0.99;

/// The interval assumed before a gesture has shown its own: 120 Hz.
const DEFAULT_INTERVAL_SECS: f64 = 1.0 / 120.0;

/// Samples kept: the delay window holds two to four at any input rate.
const SAMPLES: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    /// Cumulative travel through this event, in points.
    travel: f64,
}

/// One precise wheel delta, as the pacer takes it.
#[derive(Clone, Copy, Debug)]
pub struct PacedDelta {
    /// Points, positive toward history.
    pub points: f32,
    /// When the main thread handled the event.
    pub handled: Instant,
    /// When the hardware produced it, if the platform says.
    pub sent: Option<Instant>,
    /// System-generated momentum after the fingers lifted: it continues the
    /// gesture however long the pause before it.
    pub momentum: bool,
}

#[derive(Clone, Debug)]
pub struct ScrollPacer {
    samples: [Option<Sample>; SAMPLES],
    /// Index of the newest sample.
    head: usize,
    /// Cumulative travel received.
    received: f64,
    /// Cumulative travel handed to the viewport.
    released: f64,
    /// Smoothed time between events of the current gesture, in seconds.
    interval: f64,
    /// When the newest event was handled, and when it was sent if known.
    last_arrival: Option<Instant>,
    last_sent: Option<Instant>,
    /// Worst recent delay between an event's sample time and its handling,
    /// fading slowly, in seconds.
    lateness: f64,
    /// Smoothed time from a frame's start to its display, in seconds.
    lead: Option<f64>,
    /// How far behind display time frames sample the gesture, in seconds.
    delay: Option<f64>,
}

impl Default for ScrollPacer {
    fn default() -> Self {
        Self {
            samples: [None; SAMPLES],
            head: 0,
            received: 0.0,
            released: 0.0,
            interval: DEFAULT_INTERVAL_SECS,
            last_arrival: None,
            last_sent: None,
            lateness: 0.0,
            lead: None,
            delay: None,
        }
    }
}

impl ScrollPacer {
    /// Records one precise delta.
    pub fn push(&mut self, input: PacedDelta) {
        let PacedDelta {
            points,
            handled,
            sent,
            momentum,
        } = input;
        if !points.is_finite() || points == 0.0 {
            return;
        }
        // A hardware time is never after the handling.
        let sent = sent.map(|sent| sent.min(handled));
        let newest = self.samples[self.head];
        let gap = match (sent, self.last_sent, self.last_arrival) {
            (Some(sent), Some(last), _) => sent.checked_duration_since(last),
            (_, _, Some(last)) => handled.checked_duration_since(last),
            _ => None,
        };
        let continues = newest.is_some() && gap.is_some_and(|gap| momentum || gap <= GESTURE_GAP);
        let mut sampled = sent.unwrap_or(handled);
        match newest {
            Some(previous) if continues => {
                // A pause is not the input's rate; a source that really
                // halves its rate is still followed.
                let gap = gap.unwrap_or_default().as_secs_f64();
                if gap > 0.0 && gap <= 2.5 * self.interval {
                    self.interval = self.interval * 0.8 + gap * 0.2;
                }
                if sent.is_none() {
                    // Handling only ever delays an event, so an event handled
                    // later than one interval after the previous one is taken
                    // to have been sent on schedule. That keeps main-thread
                    // jitter out of the curve the frames sample, up to one
                    // interval of it; a source that really slows down is
                    // followed within one.
                    let interval = self.interval_duration();
                    sampled = handled
                        .min(previous.at + interval)
                        .max(handled.checked_sub(interval).unwrap_or(handled));
                }
                sampled = sampled.max(previous.at);
                // After a pause (a finger at rest, the gap before momentum)
                // an event's delta is the motion of its own interval, not of
                // the pause: hold the curve flat until then.
                let interval = self.interval_duration();
                if sampled.duration_since(previous.at) > interval.mul_f64(1.5) {
                    self.head = (self.head + 1) % SAMPLES;
                    self.samples[self.head] = Some(Sample {
                        at: sampled - interval,
                        travel: previous.travel,
                    });
                }
            }
            _ => {
                // A new gesture. Travel the old one has not released yet is
                // kept: it is part of this sample's cumulative travel.
                self.samples = [None; SAMPLES];
                self.head = 0;
                if newest.is_none() {
                    self.interval = DEFAULT_INTERVAL_SECS;
                }
                // Its first event is released whole, so the sampling point
                // can move back to what the machine needs now unseen.
                self.delay = None;
            }
        }
        let late = handled.duration_since(sampled).as_secs_f64().min(MAX_DELAY);
        self.lateness = (self.lateness * LATENESS_HOLD).max(late);
        self.last_arrival = Some(handled);
        self.last_sent = sent;
        self.received += f64::from(points);
        if self.samples[self.head].is_some() {
            self.head = (self.head + 1) % SAMPLES;
        }
        self.samples[self.head] = Some(Sample {
            at: sampled,
            travel: self.received,
        });
    }

    /// Travel to apply for a frame started at `now` and due on the display
    /// at `display_at` (when the platform says; otherwise `now` plus the
    /// usual lead). Never more than has been received; the whole of it once
    /// the sampling point passes the newest event.
    pub fn take(&mut self, now: Instant, display_at: Option<Instant>) -> f32 {
        let display = match display_at {
            Some(display) => {
                let ahead = display.saturating_duration_since(now).as_secs_f64();
                self.lead = Some(match self.lead {
                    Some(lead) => lead * 0.9 + ahead * 0.1,
                    None => ahead,
                });
                display
            }
            None => now + Duration::from_secs_f64(self.lead.unwrap_or(0.0)),
        };
        let desired = self.lead.unwrap_or(0.0)
            + (self.interval_duration().as_secs_f64() + self.lateness + MARGIN).min(MAX_DELAY);
        // Within a gesture the delay only rises, and slowly; it comes back
        // down between gestures (see `push`), where nothing is moving.
        let delay = match self.delay {
            Some(delay) => delay + (desired - delay).clamp(0.0, DELAY_RISE_PER_FRAME),
            None => desired,
        };
        self.delay = Some(delay);
        let target = self.target(display, Duration::from_secs_f64(delay));
        let step = target - self.released;
        self.released = target;
        step as f32
    }

    /// Whether every received delta has been released.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.received == self.released
    }

    /// Received travel not yet released, in points.
    #[must_use]
    pub fn outstanding(&self) -> f32 {
        (self.received - self.released) as f32
    }

    /// The gesture's recent velocity in points per second, from the samples
    /// the pacer holds; zero for a single event.
    #[must_use]
    pub fn velocity(&self) -> f32 {
        let newest = self.samples[self.head];
        let oldest = (1..=SAMPLES)
            .map(|back| self.samples[(self.head + back) % SAMPLES])
            .find_map(|sample| sample);
        match (oldest, newest) {
            (Some(oldest), Some(newest)) if newest.at > oldest.at => {
                let secs = newest.at.duration_since(oldest.at).as_secs_f64();
                ((newest.travel - oldest.travel) / secs) as f32
            }
            _ => 0.0,
        }
    }

    /// Drops all held travel; the next event starts a fresh gesture. Called
    /// when anything else moves the view, and when fingers come down on the
    /// trackpad: like momentum, held travel stops where it is shown. What
    /// the pacer learned about this machine's timing is kept.
    pub fn reset(&mut self) {
        *self = Self {
            interval: self.interval,
            lateness: self.lateness,
            lead: self.lead,
            delay: self.delay,
            ..Self::default()
        };
    }

    fn interval_duration(&self) -> Duration {
        Duration::from_secs_f64(self.interval).clamp(MIN_INTERVAL, MAX_INTERVAL)
    }

    fn target(&self, display: Instant, delay: Duration) -> f64 {
        let Some(newest) = self.samples[self.head] else {
            return self.received;
        };
        let Some(at) = display.checked_sub(delay) else {
            return self.received;
        };
        if at >= newest.at {
            return self.received;
        }
        // Walk back to the newest sample at or before `at`, interpolating
        // toward the one after it.
        let mut later = newest;
        for back in 1..SAMPLES {
            let Some(earlier) = self.samples[(self.head + SAMPLES - back) % SAMPLES] else {
                break;
            };
            if earlier.at <= at {
                let span = later.at.duration_since(earlier.at).as_secs_f64();
                if span <= 0.0 {
                    return self.released_floor(later.travel);
                }
                let part = at.duration_since(earlier.at).as_secs_f64() / span;
                return self
                    .released_floor(earlier.travel + (later.travel - earlier.travel) * part);
            }
            later = earlier;
        }
        // Before the oldest sample held: a gesture's first event is released
        // whole so its first frame moves.
        self.released_floor(later.travel)
    }

    /// Sampling never takes back travel already shown: within a gesture the
    /// curve only advances, and a reversal is new travel the other way.
    fn released_floor(&self, travel: f64) -> f64 {
        let direction = self.received - self.released;
        if direction > 0.0 {
            travel.clamp(self.released, self.received)
        } else if direction < 0.0 {
            travel.clamp(self.received, self.released)
        } else {
            self.received
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(base: Instant, millis: f64) -> Instant {
        base + Duration::from_secs_f64(millis / 1000.0)
    }

    /// A delta handled at `handled`, with no hardware time.
    fn untimed(handled: Instant, points: f32) -> PacedDelta {
        PacedDelta {
            points,
            handled,
            sent: None,
            momentum: false,
        }
    }

    #[test]
    fn a_gestures_first_event_is_released_on_its_first_frame() {
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        pacer.push(untimed(base, 6.0));
        assert_eq!(pacer.take(ms(base, 1.0), None), 6.0);
        assert!(pacer.is_settled());
        assert_eq!(pacer.take(ms(base, 9.0), None), 0.0, "nothing more to give");
    }

    #[test]
    fn jittered_events_move_every_refresh_by_the_same_amount() {
        // Events sent every 8.33 ms, handled in pairs: two land in one
        // refresh and none in the next. Applied as handled, every other
        // refresh repeats. Paced, every refresh moves by about one delta.
        let base = Instant::now();
        let period = 1000.0 / 120.0;
        let mut pacer = ScrollPacer::default();
        let mut steps = Vec::new();
        let mut next_event = 0;
        for frame in 0..40 {
            let now = period * f64::from(frame) + 0.5;
            // Events 2k and 2k+1 both arrive just before frame 2k+1.
            while next_event < 60 {
                let sent = period * f64::from(next_event);
                let handled = if next_event % 2 == 0 {
                    sent + period - 0.6
                } else {
                    sent - 0.4
                };
                if handled > now {
                    break;
                }
                pacer.push(untimed(ms(base, handled), 10.0));
                next_event += 1;
            }
            steps.push(pacer.take(ms(base, now), None));
        }
        let steady = &steps[4..36];
        assert!(
            steady.iter().all(|step| *step > 2.0),
            "no refresh repeats: {steady:?}"
        );
        let total: f32 = steady.iter().sum();
        let mean = total / steady.len() as f32;
        assert!((mean - 10.0).abs() < 1.0, "tracks the input rate: {mean}");
    }

    #[test]
    fn handling_delays_do_not_reach_the_frames() {
        // Events sent every 8.33 ms but handled up to 7 ms late, at random:
        // what the main thread does to them must not show as speed changes.
        let base = Instant::now();
        let period = 1000.0 / 120.0;
        let mut seed = 0x9e37_79b9_u32;
        let mut delay = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            f64::from(seed % 7000) / 1000.0
        };
        let handled: Vec<f64> = (0..80)
            .map(|event| period * f64::from(event) + delay())
            .collect();
        let mut pacer = ScrollPacer::default();
        let mut next = 0;
        let mut steps = Vec::new();
        for frame in 0..80 {
            let now = period * f64::from(frame) + 0.5;
            while next < handled.len() && handled[next] <= now {
                pacer.push(untimed(ms(base, handled[next]), 12.0));
                next += 1;
            }
            steps.push(pacer.take(ms(base, now), None));
        }
        let steady = &steps[6..74];
        let worst = steady
            .iter()
            .map(|step| (step - 12.0).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            worst <= 4.0,
            "a frame strayed {worst} points from 12: {steady:?}"
        );
    }

    #[test]
    fn input_stopping_settles_within_one_interval_and_loses_nothing() {
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        for event in 0..10 {
            pacer.push(untimed(ms(base, f64::from(event) * 8.0), 3.0));
        }
        let mut shown = pacer.take(ms(base, 73.0), None);
        assert!(!pacer.is_settled());
        shown += pacer.take(ms(base, 72.0 + 17.0), None);
        assert!(pacer.is_settled());
        assert!((shown - 30.0).abs() < 1e-4);
    }

    #[test]
    fn a_reversal_turns_at_once_without_taking_back_shown_travel() {
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        pacer.push(untimed(base, 10.0));
        pacer.push(untimed(ms(base, 8.0), 10.0));
        let forward = pacer.take(ms(base, 9.0), None);
        assert!(forward >= 10.0);
        pacer.push(untimed(ms(base, 16.0), -30.0));
        let mut back = 0.0;
        for frame in 0..4 {
            let step = pacer.take(ms(base, 17.0 + 8.0 * f64::from(frame)), None);
            assert!(step <= 0.0, "never steps forward again: {step}");
            back += step;
        }
        assert!(pacer.is_settled());
        assert!((forward + back + 10.0).abs() < 1e-4);
    }

    #[test]
    fn velocity_follows_recent_events() {
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        for event in 0..30 {
            pacer.push(untimed(ms(base, f64::from(event) * 10.0), 20.0));
        }
        let velocity = pacer.velocity();
        assert!((velocity - 2_000.0).abs() < 100.0, "{velocity}");
        pacer.reset();
        assert_eq!(pacer.velocity(), 0.0);
        assert!(pacer.is_settled());
    }

    /// A steady 120 Hz gesture of 12-point events, each handled 0.3–7 ms
    /// after it was sent, drawn by frames that start anywhere in the first
    /// 6 ms of their refresh and are shown two refreshes later. Returns the
    /// step of every frame, given hardware and display times or not.
    fn jittered_gesture(timed: bool) -> Vec<f32> {
        let base = Instant::now();
        let period = 1000.0 / 120.0;
        let mut seed = 0x2545_f491_u64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 10_000) as f64 / 10_000.0
        };
        let events: Vec<(f64, f64)> = (0..120)
            .map(|event| {
                let sent = period * f64::from(event);
                (sent, sent + 0.3 + 6.7 * random())
            })
            .collect();
        let mut pacer = ScrollPacer::default();
        let mut next = 0;
        let mut steps = Vec::new();
        for frame in 0..120 {
            let vsync = period * f64::from(frame);
            let start = vsync + 6.0 * random();
            let display = vsync + 2.0 * period;
            while next < events.len() && events[next].1 <= start {
                let (sent, handled) = events[next];
                pacer.push(PacedDelta {
                    points: 12.0,
                    handled: ms(base, handled),
                    sent: timed.then(|| ms(base, sent)),
                    momentum: false,
                });
                next += 1;
            }
            let display = timed.then(|| ms(base, display));
            steps.push(pacer.take(ms(base, start), display));
        }
        steps
    }

    fn worst_stray(steps: &[f32], expected: f32) -> f32 {
        steps
            .iter()
            .map(|step| (step - expected).abs())
            .fold(0.0_f32, f32::max)
    }

    #[test]
    fn with_hardware_and_display_times_every_refresh_moves_by_the_same_amount() {
        // Main-thread jitter in both the events and the frames. Sampled at
        // each frame's display time on the events' own clock, every refresh
        // shows exactly one event interval of travel.
        let timed = jittered_gesture(true);
        let steady = &timed[30..110];
        assert!(
            steady.iter().all(|step| *step > 6.0),
            "a refresh repeated: {steady:?}"
        );
        let timed_stray = worst_stray(steady, 12.0);
        // The only strays left are the sampling delay rising after a late
        // event, at most 1 ms in an 8.3 ms refresh.
        assert!(
            timed_stray <= 1.5,
            "strayed {timed_stray} points: {steady:?}"
        );
        let exact = steady
            .iter()
            .filter(|step| (**step - 12.0).abs() < 0.05)
            .count();
        assert!(
            exact * 10 >= steady.len() * 9,
            "{exact} of {} exact",
            steady.len()
        );
        // Timed by handling and frame start instead, the same input judders.
        let untimed = jittered_gesture(false);
        let untimed_stray = worst_stray(&untimed[30..110], 12.0);
        assert!(
            untimed_stray > 3.0 * timed_stray,
            "untimed {untimed_stray} vs timed {timed_stray}"
        );
    }

    #[test]
    fn a_pause_holds_the_curve_flat_instead_of_spreading_the_next_delta() {
        // A finger at rest for 40 ms, then moving again: the next event's
        // 10 points are the motion of its own 8 ms, not of the pause.
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        let timed = |at: f64| PacedDelta {
            points: 10.0,
            handled: ms(base, at + 1.0),
            sent: Some(ms(base, at)),
            momentum: false,
        };
        for event in 0..4 {
            pacer.push(timed(8.0 * f64::from(event)));
        }
        pacer.push(timed(64.0));
        let delay = 8.0 + 1.0 + 1.0;
        let mut shown = Vec::new();
        for display in [40.0, 48.0, 56.0, 60.0, 64.0, 68.0, 72.0] {
            let display: f64 = display + delay;
            pacer.take(ms(base, display), Some(ms(base, display)));
            shown.push(pacer.released);
        }
        // Still at the fourth event through the pause, then the fifth event
        // over its own interval.
        assert!(
            shown[..3]
                .iter()
                .all(|travel| (*travel - 40.0).abs() < 1e-6),
            "{shown:?}"
        );
        assert!(shown[3] > 40.0 && shown[3] < 50.0, "{shown:?}");
        assert!((shown[4] - 50.0).abs() < 0.6, "{shown:?}");
    }

    #[test]
    fn momentum_continues_the_gesture_after_a_long_pause() {
        let base = Instant::now();
        let event = |at: f64, momentum: bool| PacedDelta {
            points: 5.0,
            handled: ms(base, at),
            sent: Some(ms(base, at)),
            momentum,
        };
        let mut pacer = ScrollPacer::default();
        pacer.push(event(0.0, false));
        pacer.push(event(8.0, false));
        pacer.take(ms(base, 30.0), None);
        assert!(pacer.is_settled());
        let mut finger = pacer.clone();
        // 90 ms later momentum is paced like the rest of the gesture...
        pacer.push(event(98.0, true));
        assert_eq!(pacer.take(ms(base, 98.5), None), 0.0);
        assert!(pacer.take(ms(base, 106.0), None) > 0.0);
        // ...where a finger would start a new gesture, released whole.
        finger.push(event(98.0, false));
        assert_eq!(finger.take(ms(base, 98.5), None), 5.0);
    }

    #[test]
    fn the_sampling_delay_moves_a_little_per_frame() {
        // One event handled 15 ms late raises the lateness at once, but the
        // point frames sample at moves by at most 1 ms per frame.
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        let mut previous = None;
        for event in 0..40 {
            let sent = 8.0 * f64::from(event);
            let late = if event == 20 { 15.0 } else { 1.0 };
            pacer.push(PacedDelta {
                points: 4.0,
                handled: ms(base, sent + late),
                sent: Some(ms(base, sent)),
                momentum: false,
            });
            pacer.take(ms(base, sent + late), Some(ms(base, sent + late + 10.0)));
            let delay = pacer.delay.unwrap();
            if let Some(previous) = previous {
                let moved: f64 = delay - previous;
                assert!(
                    moved.abs() <= 0.001 + 1e-9,
                    "delay moved {moved} s at {event}"
                );
            }
            previous = Some(delay);
        }
    }
}
