//! The one place the motion tokens become executable.
//!
//! `Motion` in `tokens` names the curves and durations the product is supposed
//! to move on, but until this module the springs were four inert pairs of
//! floats: nothing evaluated them, and the only file that referenced them was
//! the gallery example, which printed their numbers. This is what turns them
//! into motion, so a transition in the sidebar and a transition in a panel are
//! the same motion rather than two hand-rolled approximations that drifted.
//!
//! Everything here is transition motion: started by a state change, and it
//! settles. Nothing in this module loops, and nothing samples a clock. Periodic
//! motion is a separate decision with its own measured cost -- see the note on
//! `status::StatusGlyph` -- and is deliberately not reachable from here.

use std::time::Duration;

use crate::{Motion, Spring};

/// The step response of a critically damped spring, normalised to span exactly
/// 0..1. It leaves fast and settles soft, which is the "spring" part -- but it
/// never crosses its target, unlike the overshooting curves that usually carry
/// that name.
///
/// Overshoot is deliberately absent. The surfaces this drives are panels and
/// rows that push their neighbours, so a bounce past the target drags adjacent
/// layout back and forth rather than reading as elasticity on one small
/// control. A curve that only ever approaches its target composes safely
/// anywhere; an overshooting one has to be audited per site.
#[must_use]
pub fn settle(delta: f32) -> f32 {
    settle_with(Spring::SEAM_STIFFNESS, delta)
}

/// `settle` at an explicit stiffness, for callers that want a tauter or looser
/// approach than the shared default.
#[must_use]
pub fn settle_with(stiffness: f32, delta: f32) -> f32 {
    let delta = delta.clamp(0.0, 1.0);
    let remaining = |t: f32| (1.0 + stiffness * t) * (-stiffness * t).exp();
    // The raw response is still short of 1 at t = 1. Divide that out, or every
    // transition ends on a visible one-frame snap onto its settled value.
    (1.0 - remaining(delta)) / (1.0 - remaining(1.0))
}

impl Spring {
    /// Stiffness of the shared `settle` curve, in units of "per transition
    /// duration" rather than per second — the curve is always evaluated over a
    /// normalised 0..1 progress, so duration is the caller's business.
    pub const SEAM_STIFFNESS: f32 = 7.0;

    /// This spring's stiffness, derived from its damping fraction. A looser
    /// damping fraction approaches its target later, which is what separates
    /// `POP` from `SETTLE` at the same duration.
    #[must_use]
    pub fn stiffness(self) -> f32 {
        Self::SEAM_STIFFNESS * self.damping_fraction / 0.82
    }

    /// This spring's normalised step response at `delta`.
    #[must_use]
    pub fn settle(self, delta: f32) -> f32 {
        settle_with(self.stiffness(), delta)
    }
}

impl Motion {
    /// Row selection and focus fills, as a `Duration` an animation can take.
    pub const ROW_SELECT_TIME: Duration = Duration::from_millis((Self::ROW_SELECT * 1000.0) as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_spans_its_whole_range() {
        assert_eq!(settle(0.0), 0.0);
        assert_eq!(settle(1.0), 1.0);
    }

    #[test]
    fn the_curve_never_overshoots_or_backs_up() {
        // Overshoot would shove a pushed neighbour past its resting edge and
        // drag it back; a non-monotonic curve would read as a stutter.
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = settle(step as f32 / 100.0);
            assert!(
                (0.0..=1.0).contains(&value),
                "settle({step}/100) left 0..=1 at {value}"
            );
            assert!(value >= previous, "settle went backwards at {step}");
            previous = value;
        }
    }

    #[test]
    fn the_curve_front_loads_its_travel() {
        // The "spring" read comes from covering most of the distance early and
        // easing into the target, rather than moving linearly.
        assert!(settle(0.5) > 0.8, "{}", settle(0.5));
    }

    #[test]
    fn progress_outside_the_animation_is_clamped() {
        // GPUI clamps its own delta, but the curve is public and is also fed
        // raw elapsed-over-duration ratios by the seam.
        assert_eq!(settle(-1.0), 0.0);
        assert_eq!(settle(2.0), 1.0);
    }

    #[test]
    fn every_spring_settles_without_overshooting() {
        for spring in [
            Motion::SNAP,
            Motion::POP,
            Motion::SETTLE,
            Motion::FOOTER_PIN,
        ] {
            assert_eq!(spring.settle(0.0), 0.0);
            assert_eq!(spring.settle(1.0), 1.0);
            let mut previous = 0.0;
            for step in 0..=50 {
                let value = spring.settle(step as f32 / 50.0);
                assert!((0.0..=1.0).contains(&value));
                assert!(value >= previous);
                previous = value;
            }
        }
    }

    #[test]
    fn a_looser_damping_fraction_approaches_its_target_later() {
        // POP (0.60) stays behind SETTLE (0.82) at the midpoint -- that
        // difference is the whole reason both tokens exist.
        assert!(Motion::POP.settle(0.5) < Motion::SETTLE.settle(0.5));
    }

    #[test]
    fn the_token_duration_survives_the_conversion_to_time() {
        assert_eq!(Motion::ROW_SELECT_TIME, Duration::from_millis(160));
    }
}
