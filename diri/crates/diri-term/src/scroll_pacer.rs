//! Display-paced trackpad scrolling.
//!
//! Precise wheel events arrive on the main thread at the trackpad's rate, with
//! a few milliseconds of jitter, unsynchronised with the display. Applying each
//! one as it is handled shows that jitter: two events land in one refresh and
//! none in the next, so that refresh repeats the previous position.
//!
//! The pacer instead records each event's cumulative travel against the time
//! it was most likely sent (its handling time, pulled back onto the input's
//! own schedule when the main thread was late with it), and every frame
//! samples that curve at the frame's time minus one input interval plus the
//! usual lateness, interpolating between the two events around it.
//! A steady gesture then moves every refresh by its velocity times the frame
//! interval, whatever the events' phase against the display. The first event
//! of a gesture is released whole, so a gesture starts moving on its first
//! frame, and once input stops the curve's end is reached within one interval
//! and frames stop: an idle pacer schedules nothing.
//!
//! This is the same construction as AppKit's responsive scrolling and the
//! input resampling Android and Chromium do at vsync. Momentum needs no
//! special case: AppKit delivers it as ordinary precise deltas.

use std::time::{Duration, Instant};

/// Events closer together than this belong to one gesture. A finger at rest
/// on the trackpad or the gap before momentum can pause a few frames.
const GESTURE_GAP: Duration = Duration::from_millis(60);

/// Bounds on the input interval the pacer assumes: a 240 Hz trackpad to a
/// 60 Hz source.
const MIN_DELAY: Duration = Duration::from_millis(4);
const MAX_INTERVAL: Duration = Duration::from_micros(16_700);

/// Bound on the whole resampling delay, lateness included.
const MAX_DELAY: Duration = Duration::from_millis(25);

/// The interval assumed before a gesture has shown its own: 120 Hz.
const DEFAULT_INTERVAL_SECS: f64 = 1.0 / 120.0;

/// Samples kept: the delay window holds two or three at any input rate.
const SAMPLES: usize = 6;

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    /// Cumulative travel through this event, in points.
    travel: f64,
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
    /// When the newest event was handled. Samples carry the time it was
    /// most likely sent instead.
    last_arrival: Option<Instant>,
    /// Smoothed delay between an event's likely sending and its handling,
    /// in seconds.
    lateness: f64,
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
            lateness: 0.0,
        }
    }
}

impl ScrollPacer {
    /// Records one precise delta, positive toward history.
    pub fn push(&mut self, at: Instant, delta: f32) {
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        let newest = self.samples[self.head];
        let continues = self
            .last_arrival
            .is_some_and(|last| at >= last && at.duration_since(last) <= GESTURE_GAP);
        let mut sampled = at;
        match (newest, self.last_arrival) {
            (Some(previous), Some(last)) if continues => {
                let gap = at.duration_since(last).as_secs_f64();
                self.interval = self.interval * 0.8 + gap * 0.2;
                // Handling only ever delays an event, so an event handled
                // later than one interval after the previous one is taken to
                // have been sent on schedule. That keeps main-thread jitter
                // out of the curve the frames sample, up to one interval of
                // it; a source that really slows down is followed within one.
                let interval = self.interval_duration();
                let on_schedule = previous.at + interval;
                sampled = at
                    .min(on_schedule)
                    .max(at.checked_sub(interval).unwrap_or(at));
                sampled = sampled.max(previous.at);
                // Frames sample one interval plus the usual lateness behind,
                // so the event a frame needs has normally been handled by
                // then. Averaged slowly: a delay that jumped with each late
                // event would move the sampling point and show as the very
                // speed change it is there to hide.
                let late = at.duration_since(sampled).as_secs_f64();
                self.lateness = self.lateness * 0.95 + late * 0.05;
            }
            _ => {
                // A new gesture. Travel the old one has not released yet is
                // kept: it is part of this sample's cumulative travel.
                self.samples = [None; SAMPLES];
                self.head = 0;
                if newest.is_none() {
                    self.interval = DEFAULT_INTERVAL_SECS;
                    self.lateness = 0.0;
                }
            }
        }
        self.last_arrival = Some(at);
        self.received += f64::from(delta);
        if self.samples[self.head].is_some() {
            self.head = (self.head + 1) % SAMPLES;
        }
        self.samples[self.head] = Some(Sample {
            at: sampled,
            travel: self.received,
        });
    }

    /// Travel to apply for a frame drawn at `now`. Never more than has been
    /// received; the whole of it once `now` is one interval past the newest
    /// event.
    pub fn take(&mut self, now: Instant) -> f32 {
        let target = self.target(now);
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
    /// when anything else moves the view.
    pub fn reset(&mut self) {
        *self = Self {
            interval: self.interval,
            lateness: self.lateness,
            ..Self::default()
        };
    }

    fn interval_duration(&self) -> Duration {
        Duration::from_secs_f64(self.interval).clamp(MIN_DELAY, MAX_INTERVAL)
    }

    /// The delay behind real time at which frames sample the gesture: one
    /// input interval, plus how late events are being handled.
    fn delay(&self) -> Duration {
        (self.interval_duration() + Duration::from_secs_f64(2.0 * self.lateness)).min(MAX_DELAY)
    }

    fn target(&self, now: Instant) -> f64 {
        let Some(newest) = self.samples[self.head] else {
            return self.received;
        };
        let Some(at) = now.checked_sub(self.delay()) else {
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

    #[test]
    fn a_gestures_first_event_is_released_on_its_first_frame() {
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        pacer.push(base, 6.0);
        assert_eq!(pacer.take(ms(base, 1.0)), 6.0);
        assert!(pacer.is_settled());
        assert_eq!(pacer.take(ms(base, 9.0)), 0.0, "nothing more to give");
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
                pacer.push(ms(base, handled), 10.0);
                next_event += 1;
            }
            steps.push(pacer.take(ms(base, now)));
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
                pacer.push(ms(base, handled[next]), 12.0);
                next += 1;
            }
            steps.push(pacer.take(ms(base, now)));
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
            pacer.push(ms(base, f64::from(event) * 8.0), 3.0);
        }
        let mut shown = pacer.take(ms(base, 73.0));
        assert!(!pacer.is_settled());
        shown += pacer.take(ms(base, 72.0 + 17.0));
        assert!(pacer.is_settled());
        assert!((shown - 30.0).abs() < 1e-4);
    }

    #[test]
    fn a_reversal_turns_at_once_without_taking_back_shown_travel() {
        let base = Instant::now();
        let mut pacer = ScrollPacer::default();
        pacer.push(base, 10.0);
        pacer.push(ms(base, 8.0), 10.0);
        let forward = pacer.take(ms(base, 9.0));
        assert!(forward >= 10.0);
        pacer.push(ms(base, 16.0), -30.0);
        let mut back = 0.0;
        for frame in 0..4 {
            let step = pacer.take(ms(base, 17.0 + 8.0 * f64::from(frame)));
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
            pacer.push(ms(base, f64::from(event) * 10.0), 20.0);
        }
        let velocity = pacer.velocity();
        assert!((velocity - 2_000.0).abs() < 100.0, "{velocity}");
        pacer.reset();
        assert_eq!(pacer.velocity(), 0.0);
        assert!(pacer.is_settled());
    }
}
