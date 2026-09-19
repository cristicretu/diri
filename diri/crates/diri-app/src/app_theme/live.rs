//! The theme on screen right now.
//!
//! Terminal and chrome both resolve a theme id through `app_theme`, so a
//! theme change animates at that one source: while a fade runs, the id being
//! faded to resolves to the mixed colors, in every window, and to the catalog
//! theme again the moment it lands.
//!
//! The state is per thread. GPUI renders on one thread, and a test or a
//! headless preview that never calls [`enable`] keeps resolving catalog
//! themes immediately.

use std::cell::RefCell;
use std::time::Instant;

use diri_term::crossfade::ThemeFade;
use diri_term::theme::TermTheme;
use gpui::{App, AppContext as _, Window};

struct Live {
    enabled: bool,
    reduce_motion: bool,
    clock: fn() -> Instant,
    /// What every consumer paints. Sampled once per frame so the terminal,
    /// the sidebar and a floating panel never disagree within one.
    shown: Option<TermTheme>,
    fade: Option<ThemeFade>,
}

thread_local! {
    static LIVE: RefCell<Live> = const {
        RefCell::new(Live {
            enabled: false,
            reduce_motion: false,
            clock: Instant::now,
            shown: None,
            fade: None,
        })
    };
}

impl Live {
    /// Heads for `to`, starting from the colors on screen. A fade already
    /// running is abandoned where it stands, never queued behind.
    fn head_for(&mut self, to: TermTheme) {
        if !self.enabled {
            return;
        }
        let Some(shown) = self.shown else {
            // Nothing has been painted yet; there is nothing to fade from.
            self.shown = Some(to);
            return;
        };
        let target = self.fade.map_or(shown, |fade| fade.target());
        if target == to {
            return;
        }
        if self.reduce_motion {
            self.shown = Some(to);
            self.fade = None;
            return;
        }
        let fade = ThemeFade::new(shown, to, (self.clock)());
        self.shown = Some(fade.sample((self.clock)()));
        self.fade = Some(fade);
    }

    /// Advances the displayed colors. Answers whether another frame is owed.
    fn tick(&mut self) -> bool {
        let Some(fade) = self.fade else {
            return false;
        };
        let now = (self.clock)();
        self.shown = Some(fade.sample(now));
        if fade.is_finished(now) || self.reduce_motion {
            self.shown = Some(fade.target());
            self.fade = None;
        }
        self.fade.is_some()
    }
}

/// Turns theme fades on for this thread. The running application calls this
/// once; nothing else does.
pub(crate) fn enable() {
    LIVE.with_borrow_mut(|live| live.enabled = true);
}

/// The colors `theme` should paint with: the fade in flight if `theme` is
/// where it is heading, otherwise `theme` itself.
pub(super) fn resolve(theme: TermTheme) -> TermTheme {
    LIVE.with_borrow(|live| match live.shown {
        Some(shown) if live.enabled && shown.id == theme.id => shown,
        _ => theme,
    })
}

/// Declares `theme` the application's theme. The first live consumer to
/// render after a change starts the fade, whichever window it is in.
pub(super) fn head_for(theme: TermTheme) {
    LIVE.with_borrow_mut(|live| live.head_for(theme));
}

pub(super) fn follow(theme: TermTheme, window: &mut Window, cx: &mut App) {
    let fading = LIVE.with_borrow_mut(|live| {
        if !live.enabled {
            return false;
        }
        live.reduce_motion = cx.reduce_motion();
        live.head_for(theme);
        live.tick()
    });
    if fading {
        window.on_next_frame(|window, cx| {
            // Sample before anything paints, then repaint every window from
            // that one sample. Views are cached and floating panels are
            // windows of their own, so a plain notify would leave some of
            // them a step behind, or on the final frame behind for good.
            LIVE.with_borrow_mut(Live::tick);
            window.refresh();
            for handle in cx.windows() {
                // This window is on loan to the callback and fails here.
                let _ = cx.update_window(handle, |_, window, _| window.refresh());
            }
        });
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::LIVE;

    thread_local! {
        static NOW: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    fn now() -> Instant {
        NOW.get().expect("manual clock installed")
    }

    /// Restores immediate themes when dropped: a single-threaded test run
    /// shares this thread with every other test.
    pub(crate) struct Fades;

    impl Drop for Fades {
        fn drop(&mut self) {
            LIVE.with_borrow_mut(|live| {
                live.enabled = false;
                live.reduce_motion = false;
                live.clock = Instant::now;
                live.shown = None;
                live.fade = None;
            });
        }
    }

    /// Enables fades on this thread against a clock the test advances.
    #[must_use]
    pub(crate) fn enable_with_manual_clock() -> Fades {
        NOW.set(Some(Instant::now()));
        LIVE.with_borrow_mut(|live| {
            live.enabled = true;
            live.clock = now;
        });
        Fades
    }

    pub(crate) fn advance(by: Duration) {
        NOW.set(Some(now() + by));
    }

    pub(crate) fn tick() -> bool {
        LIVE.with_borrow_mut(super::Live::tick)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const LONG_ENOUGH: Duration = Duration::from_millis(400);

    #[test]
    fn themes_resolve_immediately_until_fades_are_enabled() {
        head_for(TermTheme::DRACULA);
        head_for(TermTheme::NORD);
        assert_eq!(resolve(TermTheme::NORD), TermTheme::NORD);
        assert!(!testing::tick());
    }

    #[test]
    fn the_first_theme_appears_without_a_fade() {
        let _fades = testing::enable_with_manual_clock();
        head_for(TermTheme::DRACULA);
        assert_eq!(resolve(TermTheme::DRACULA), TermTheme::DRACULA);
        assert!(!testing::tick());
    }

    #[test]
    fn a_change_fades_lands_exactly_and_stops_asking_for_frames() {
        let _fades = testing::enable_with_manual_clock();
        head_for(TermTheme::DRACULA);
        head_for(TermTheme::GITHUB_LIGHT);

        // The frame the change lands in still shows the old colors.
        let first = resolve(TermTheme::GITHUB_LIGHT);
        assert_eq!(first.signature(), TermTheme::DRACULA.signature());

        testing::advance(Duration::from_millis(60));
        assert!(testing::tick());
        let midway = resolve(TermTheme::GITHUB_LIGHT);
        assert_ne!(midway.background, TermTheme::DRACULA.background);
        assert_ne!(midway.background, TermTheme::GITHUB_LIGHT.background);
        // Any other theme, a settings swatch say, is never caught up in it.
        assert_eq!(resolve(TermTheme::NORD), TermTheme::NORD);

        testing::advance(LONG_ENOUGH);
        assert!(!testing::tick(), "the landing frame owes no further frame");
        assert_eq!(resolve(TermTheme::GITHUB_LIGHT), TermTheme::GITHUB_LIGHT);
        assert!(!testing::tick());
    }

    #[test]
    fn arrowing_on_mid_fade_continues_from_the_colors_on_screen() {
        let _fades = testing::enable_with_manual_clock();
        head_for(TermTheme::DIRIJOR_DARK);
        head_for(TermTheme::DRACULA);
        testing::advance(Duration::from_millis(40));
        testing::tick();
        let shown = resolve(TermTheme::DRACULA);

        head_for(TermTheme::SOLARIZED_DARK);
        let retargeted = resolve(TermTheme::SOLARIZED_DARK);
        assert_eq!(retargeted.signature(), shown.signature());
        // The abandoned target no longer resolves to the fade.
        assert_eq!(resolve(TermTheme::DRACULA), TermTheme::DRACULA);

        // Asking for the same target again must not restart the fade.
        testing::advance(Duration::from_millis(40));
        testing::tick();
        let moving = resolve(TermTheme::SOLARIZED_DARK);
        head_for(TermTheme::SOLARIZED_DARK);
        assert_eq!(resolve(TermTheme::SOLARIZED_DARK), moving);

        testing::advance(LONG_ENOUGH);
        assert!(!testing::tick());
        assert_eq!(
            resolve(TermTheme::SOLARIZED_DARK),
            TermTheme::SOLARIZED_DARK
        );
    }

    #[test]
    fn reduce_motion_switches_at_once() {
        let _fades = testing::enable_with_manual_clock();
        LIVE.with_borrow_mut(|live| live.reduce_motion = true);
        head_for(TermTheme::DRACULA);
        head_for(TermTheme::GITHUB_LIGHT);
        assert_eq!(resolve(TermTheme::GITHUB_LIGHT), TermTheme::GITHUB_LIGHT);
        assert!(!testing::tick());
    }
}
