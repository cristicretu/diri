//! Tooltip warm-up: the first tooltip waits, the ones after it do not.
//!
//! macOS and browsers treat tooltips as a mode. Resting on a control for the
//! full delay "arms" it; after that, every control the pointer reaches shows
//! its tooltip straight away, until the pointer has been away from tooltips
//! long enough that the user has evidently moved on. Without this, scanning a
//! toolbar costs the full delay per button.
//!
//! GPUI fixes a tooltip's show delay when the element paints, and several
//! Diri views are `.cached()`, so a delay chosen at render time can be stale
//! by the time the pointer arrives. The decision is therefore made when GPUI
//! builds the tooltip: every site asks GPUI for the short warm delay, and a
//! cold tooltip stays blank for the remainder of the full delay before it
//! reveals itself. Nothing fades either way, so a warm swap between two
//! controls is one frame, and reduce motion has nothing to turn off.

use std::time::{Duration, Instant};

use gpui::{
    AnyView, App, AppContext, Context, Global, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Task, Window, div,
};

/// The cold delay; GPUI's own default, so the first tooltip is unchanged.
pub(crate) const FULL_DELAY: Duration = Duration::from_millis(500);

/// The warm delay. Not zero: a pointer merely crossing a control on its way
/// elsewhere is on it for a frame or two, and flashing each one's tooltip in
/// passing is noise.
pub(crate) const WARM_DELAY: Duration = Duration::from_millis(50);

/// How long after the last tooltip hides the next one is still immediate.
/// Travel between Diri's furthest-apart tooltip controls (sidebar footer to
/// the title bar) takes a deliberate pointer roughly 400-600 ms, which this
/// covers; a second and more is a pause, after which a hover is a new
/// question and earns the full delay again.
pub(crate) const COOLDOWN: Duration = Duration::from_millis(750);

/// Whether tooltips are currently warm. Pure: time comes in as an argument so
/// tests drive it without a clock.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Warmth {
    visible: usize,
    last_hidden: Option<Instant>,
}

impl Warmth {
    /// A tooltip is on screen, or one left the screen within `COOLDOWN`.
    pub(crate) fn is_warm(&self, now: Instant) -> bool {
        self.visible > 0
            || self
                .last_hidden
                .is_some_and(|hidden| now.saturating_duration_since(hidden) < COOLDOWN)
    }

    /// How much longer a tooltip GPUI is building now should stay blank. GPUI
    /// already waited `WARM_DELAY` before building it.
    pub(crate) fn remaining_wait(&self, now: Instant) -> Duration {
        if self.is_warm(now) {
            Duration::ZERO
        } else {
            FULL_DELAY.saturating_sub(WARM_DELAY)
        }
    }

    pub(crate) fn shown(&mut self) {
        self.visible += 1;
    }

    pub(crate) fn hidden(&mut self, now: Instant) {
        self.visible = self.visible.saturating_sub(1);
        self.last_hidden = Some(now);
    }
}

#[derive(Default)]
struct GlobalWarmth(Warmth);

impl Global for GlobalWarmth {}

/// The one seam every GPUI tooltip in the app goes through.
pub(crate) trait WarmTooltip: StatefulInteractiveElement {
    /// `.tooltip(build)` with warm-up: see the module docs.
    fn warm_tooltip(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self
    where
        Self: Sized,
    {
        self.tooltip_show_delay(WARM_DELAY)
            .tooltip(move |window, cx| {
                let content = build(window, cx);
                cx.new(|cx| WarmTooltipView::new(content, cx)).into()
            })
    }
}

impl<E: StatefulInteractiveElement> WarmTooltip for E {}

/// Wraps a tooltip's content and holds it back until the cold delay is up.
struct WarmTooltipView {
    content: AnyView,
    revealed: bool,
    _reveal: Option<Task<()>>,
}

impl WarmTooltipView {
    fn new(content: AnyView, cx: &mut Context<Self>) -> Self {
        let wait = cx
            .default_global::<GlobalWarmth>()
            .0
            .remaining_wait(Instant::now());
        // GPUI drops this view when the pointer leaves, which is exactly when
        // the tooltip stops counting as visible. A view dropped before it was
        // revealed never counted, so it must not warm anything either.
        cx.on_release(|this, cx| {
            if this.revealed {
                cx.default_global::<GlobalWarmth>().0.hidden(Instant::now());
            }
        })
        .detach();
        let mut view = Self {
            content,
            revealed: false,
            _reveal: None,
        };
        if wait.is_zero() {
            view.reveal(cx);
        } else {
            view._reveal = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                this.update(cx, |this, cx| {
                    this.reveal(cx);
                    cx.notify();
                })
                .ok();
            }));
        }
        view
    }

    fn reveal(&mut self, cx: &mut App) {
        if !self.revealed {
            self.revealed = true;
            cx.default_global::<GlobalWarmth>().0.shown();
        }
    }
}

impl Render for WarmTooltipView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Blank while cold: GPUI already considers the tooltip shown, but an
        // empty element has no size and no hitbox, so nothing is visible or
        // clickable until the full delay has passed.
        let content = self.revealed.then(|| self.content.clone());
        div().children(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn the_first_tooltip_waits_the_full_delay() {
        let warmth = Warmth::default();
        let now = Instant::now();
        assert!(!warmth.is_warm(now));
        assert_eq!(warmth.remaining_wait(now) + WARM_DELAY, FULL_DELAY);
    }

    #[test]
    fn a_visible_tooltip_keeps_the_next_one_immediate() {
        let mut warmth = Warmth::default();
        let start = Instant::now();
        warmth.shown();
        // However long the first stays up, moving on is still warm.
        assert_eq!(warmth.remaining_wait(start + ms(10_000)), Duration::ZERO);
    }

    #[test]
    fn warmth_survives_the_gap_between_two_controls() {
        let mut warmth = Warmth::default();
        let start = Instant::now();
        warmth.shown();
        warmth.hidden(start);
        assert!(warmth.is_warm(start + COOLDOWN - ms(1)));
        assert_eq!(warmth.remaining_wait(start + ms(300)), Duration::ZERO);
    }

    #[test]
    fn a_pause_after_the_last_tooltip_cools_back_down() {
        let mut warmth = Warmth::default();
        let start = Instant::now();
        warmth.shown();
        warmth.hidden(start);
        assert!(!warmth.is_warm(start + COOLDOWN));
        assert_eq!(
            warmth.remaining_wait(start + COOLDOWN),
            FULL_DELAY - WARM_DELAY
        );
    }

    #[test]
    fn a_swap_stays_warm_while_the_old_tooltip_is_released_late() {
        // GPUI may build B before it releases A; the count must not let A's
        // release make B's successor cold.
        let mut warmth = Warmth::default();
        let start = Instant::now();
        warmth.shown();
        warmth.shown();
        warmth.hidden(start);
        assert!(warmth.is_warm(start + ms(5_000)));
        warmth.hidden(start + ms(5_000));
        assert!(!warmth.is_warm(start + ms(5_000) + COOLDOWN));
    }

    #[test]
    fn a_stray_release_never_underflows() {
        let mut warmth = Warmth::default();
        let start = Instant::now();
        warmth.hidden(start);
        assert!(!warmth.is_warm(start + COOLDOWN));
    }
}
