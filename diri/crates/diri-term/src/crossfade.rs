//! Theme changes that travel through Oklab.
//!
//! Blending sRGB channels is what a dissolve between two screenshots does.
//! Its lightness runs ahead of the perceptual middle, and two accents of
//! different hue meet in a tinted, dirtier color than either: a blue heading
//! for an ochre passes through olive. Oklab moves lightness at a steady
//! perceived rate and takes the straight line between the two colors, so a
//! crossfade reads as the light changing rather than as one picture
//! dissolving into another.
//!
//! Everything here is a pure function of the two themes and a time. The
//! application owns the clock and decides when frames are worth requesting.

use std::time::{Duration, Instant};

use gpui::Rgba;

use crate::contrast::{self, Oklch};
use crate::theme::{TermTheme, ThemeAppearance};

/// Between two themes of one appearance only hues and accents move.
const FADE: Duration = Duration::from_millis(180);
/// A light/dark flip crosses the whole lightness range; the same duration
/// reads as a flash, so it gets a little longer.
const FLIP_FADE: Duration = Duration::from_millis(240);
/// Background lightness travel, in Oklab, that earns the full flip duration.
const FLIP_TRAVEL: f32 = 0.6;

/// `from` toward `to` along the straight Oklab line; alpha moves linearly.
///
/// The line runs through (a, b), never around the hue circle: a gray has no
/// hue of its own, and interpolating an angle would swing it through
/// unrelated colors on the way to a saturated one.
#[must_use]
pub fn mix_color(from: Rgba, to: Rgba, t: f32) -> Rgba {
    if t <= 0.0 || from == to {
        return if t <= 0.0 { from } else { to };
    }
    if t >= 1.0 {
        return to;
    }
    let [from_l, from_a, from_b] = oklab(from);
    let [to_l, to_a, to_b] = oklab(to);
    let lerp = |from: f32, to: f32| from + (to - from) * t;
    let (a, b) = (lerp(from_a, to_a), lerp(from_b, to_b));
    // The sRGB gamut is not convex in Oklab, so the midpoint of two
    // displayable colors can fall just outside it; `in_gamut` sheds the
    // excess chroma instead of clipping a channel.
    let mixed = contrast::in_gamut(Oklch {
        lightness: lerp(from_l, to_l),
        chroma: a.hypot(b),
        hue: b.atan2(a),
    });
    contrast::rgba(mixed, lerp(from.a, to.a))
}

fn oklab(color: Rgba) -> [f32; 3] {
    let polar = contrast::oklch(contrast::linear(color));
    [
        polar.lightness,
        polar.chroma * polar.hue.cos(),
        polar.chroma * polar.hue.sin(),
    ]
}

impl TermTheme {
    /// This theme on its way to `other`. `t` at or below zero is exactly
    /// `self` and at or above one exactly `other`, so a finished fade leaves
    /// the catalog constant behind, bit for bit.
    ///
    /// The identity is the destination's: everything that asks which theme is
    /// showing already means the one being faded to. Appearance is the
    /// exception. It gates the light-theme contrast correction, and turning
    /// that off over a still-light background would drop white text onto
    /// cream for the length of the fade. So a fade that touches a light theme
    /// is light throughout, and `contrast` phases the correction in with the
    /// lightness of the paper, which leaves the dark end painting exactly
    /// what the dark theme paints.
    #[must_use]
    pub fn mix(&self, other: &Self, t: f32) -> Self {
        if t <= 0.0 {
            return *self;
        }
        if t >= 1.0 {
            return *other;
        }
        // No `..`: a color added to the theme must be given a fade here.
        let Self {
            id,
            name,
            appearance,
            background,
            foreground,
            cursor,
            cursor_text,
            selection,
            find_match,
            find_match_current,
            ansi,
        } = *other;
        let light =
            self.appearance == ThemeAppearance::Light || appearance == ThemeAppearance::Light;
        let mut palette = ansi;
        for (slot, color) in palette.iter_mut().enumerate() {
            *color = mix_color(self.ansi[slot], *color, t);
        }
        Self {
            id,
            name,
            appearance: if light {
                ThemeAppearance::Light
            } else {
                ThemeAppearance::Dark
            },
            background: mix_color(self.background, background, t),
            foreground: mix_color(self.foreground, foreground, t),
            cursor: mix_color(self.cursor, cursor, t),
            cursor_text: mix_color(self.cursor_text, cursor_text, t),
            selection: mix_color(self.selection, selection, t),
            find_match: mix_color(self.find_match, find_match, t),
            find_match_current: mix_color(self.find_match_current, find_match_current, t),
            ansi: palette,
        }
    }
}

/// Decelerating cubic: the change answers the keypress at full speed and
/// settles into the destination.
#[must_use]
pub fn ease_out(progress: f32) -> f32 {
    let remaining = 1.0 - progress.clamp(0.0, 1.0);
    1.0 - remaining * remaining * remaining
}

/// One theme change in flight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeFade {
    from: TermTheme,
    to: TermTheme,
    started: Instant,
    duration: Duration,
}

impl ThemeFade {
    #[must_use]
    pub fn new(from: TermTheme, to: TermTheme, now: Instant) -> Self {
        // Judged by the distance left to cover rather than by appearance, so
        // a fade retargeted halfway through a flip is timed for what remains.
        let [from_lightness, ..] = oklab(from.background);
        let [to_lightness, ..] = oklab(to.background);
        let travel = ((from_lightness - to_lightness).abs() / FLIP_TRAVEL).min(1.0);
        let duration = FADE + (FLIP_FADE - FADE).mul_f32(travel);
        Self {
            // The first sample is `from` itself, and it has to answer to the
            // same id as every later frame.
            from: TermTheme {
                id: to.id,
                name: to.name,
                ..from
            },
            to,
            started: now,
            duration,
        }
    }

    #[must_use]
    pub const fn target(&self) -> TermTheme {
        self.to
    }

    #[must_use]
    pub fn is_finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration
    }

    /// The colors to show at `now`. Exactly the target once finished.
    #[must_use]
    pub fn sample(&self, now: Instant) -> TermTheme {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.duration {
            return self.to;
        }
        let progress = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        self.from.mix(&self.to, ease_out(progress))
    }

    /// A fade to `to` that starts from whatever is on screen at `now`.
    ///
    /// Arrowing through a theme list retargets several times inside one fade.
    /// Starting from the displayed colors, rather than from the abandoned
    /// target, keeps every frame continuous with the last, and there is never
    /// more than one fade to finish: the colors chase the selection.
    #[must_use]
    pub fn retarget(&self, to: TermTheme, now: Instant) -> Self {
        Self::new(self.sample(now), to, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: u32) -> Rgba {
        Rgba {
            r: ((value >> 16) & 0xff) as f32 / 255.0,
            g: ((value >> 8) & 0xff) as f32 / 255.0,
            b: (value & 0xff) as f32 / 255.0,
            a: 1.0,
        }
    }

    fn polar(color: Rgba) -> Oklch {
        contrast::oklch(contrast::linear(color))
    }

    fn distance(left: Rgba, right: Rgba) -> f32 {
        let [l, a, b] = oklab(left);
        let [other_l, other_a, other_b] = oklab(right);
        ((l - other_l).powi(2) + (a - other_a).powi(2) + (b - other_b).powi(2)).sqrt()
    }

    fn colors(theme: &TermTheme) -> Vec<Rgba> {
        let mut colors = vec![
            theme.background,
            theme.foreground,
            theme.cursor,
            theme.cursor_text,
            theme.selection,
            theme.find_match,
            theme.find_match_current,
        ];
        colors.extend(theme.ansi);
        colors
    }

    #[test]
    fn endpoints_are_exact() {
        let from = TermTheme::TOKYO_NIGHT;
        let to = TermTheme::GRUVBOX_LIGHT;
        assert_eq!(from.mix(&to, 0.0), from);
        assert_eq!(from.mix(&to, -1.0), from);
        assert_eq!(from.mix(&to, 1.0), to);
        assert_eq!(from.mix(&to, 1.0).signature(), to.signature());
        assert_eq!(mix_color(hex(0x123456), hex(0x123456), 0.4), hex(0x123456));
    }

    #[test]
    fn lightness_moves_evenly_between_a_dark_and_a_light_background() {
        let dark = hex(0x1a1b26);
        let cream = hex(0xfbf1c7);
        let (from, to) = (polar(dark).lightness, polar(cream).lightness);
        for step in 1..8 {
            let t = step as f32 / 8.0;
            let mixed = polar(mix_color(dark, cream, t)).lightness;
            assert!(
                (mixed - (from + (to - from) * t)).abs() < 0.004,
                "step {step}"
            );
        }
    }

    #[test]
    fn two_accents_meet_in_a_cleaner_color_than_their_srgb_blend() {
        let (blue, ochre) = (hex(0x7aa2f7), hex(0xd79921));
        let blend = Rgba {
            r: (blue.r + ochre.r) * 0.5,
            g: (blue.g + ochre.g) * 0.5,
            b: (blue.b + ochre.b) * 0.5,
            a: 1.0,
        };
        let mixed = polar(mix_color(blue, ochre, 0.5));
        assert!(mixed.chroma < polar(blend).chroma * 0.5);
    }

    #[test]
    fn a_gray_takes_on_only_the_hue_it_is_heading_for() {
        for gray in [0x000000, 0x666666, 0xe5e5e5, 0xffffff] {
            for color in [0xcd3131, 0x0dbc79, 0x2472c8, 0xe5e510, 0xbc3fbc] {
                let target = polar(hex(color));
                let mut chroma = 0.0;
                for step in 1..20 {
                    let mixed = polar(mix_color(hex(gray), hex(color), step as f32 / 20.0));
                    if mixed.chroma > 0.02 {
                        let turn = (mixed.hue - target.hue).abs().to_degrees();
                        let turn = turn.min(360.0 - turn);
                        assert!(turn < 4.0, "{gray:06x}->{color:06x} step {step}: {turn}");
                    }
                    // Chroma only grows: the gray never detours through
                    // another color on its way.
                    assert!(mixed.chroma >= chroma - 0.004);
                    chroma = mixed.chroma;
                }
            }
        }
    }

    #[test]
    fn complementary_colors_cross_through_neutral_not_around_the_wheel() {
        let mid = polar(mix_color(hex(0xff0000), hex(0x00ffff), 0.5));
        assert!(mid.chroma < 0.06, "midpoint chroma {}", mid.chroma);
    }

    #[test]
    fn every_color_field_fades() {
        // Two themes that differ in every color. A field `mix` forgot would
        // sit at one end; the destructuring in `mix` catches a new field at
        // compile time, this catches one that was bound and not mixed.
        let from = TermTheme::DIRIJOR_DARK;
        let mut to = TermTheme::GRUVBOX_LIGHT;
        to.ansi[15] = hex(0xfdf6e3);
        to.find_match = Rgba {
            r: 0.2,
            g: 0.5,
            b: 0.9,
            a: 0.5,
        };
        to.find_match_current = Rgba {
            r: 0.1,
            g: 0.4,
            b: 0.8,
            a: 0.9,
        };
        let mixed = from.mix(&to, 0.5);
        for (index, ((from, to), mixed)) in colors(&from)
            .into_iter()
            .zip(colors(&to))
            .zip(colors(&mixed))
            .enumerate()
        {
            assert_ne!(from, to, "fixture field {index} must differ");
            assert_ne!(mixed, from, "field {index} did not move");
            assert_ne!(mixed, to, "field {index} jumped to the end");
        }
        assert_eq!(colors(&mixed).len(), 7 + 16);
        assert_eq!((mixed.id, mixed.name), (to.id, to.name));
    }

    #[test]
    fn a_fade_touching_a_light_theme_is_light_throughout() {
        let dark = TermTheme::DIRIJOR_DARK;
        let light = TermTheme::DIRIJOR_LIGHT;
        for t in [0.01, 0.5, 0.99] {
            assert_eq!(dark.mix(&light, t).appearance, ThemeAppearance::Light);
            assert_eq!(light.mix(&dark, t).appearance, ThemeAppearance::Light);
            assert_eq!(
                dark.mix(&TermTheme::DRACULA, t).appearance,
                ThemeAppearance::Dark
            );
        }
    }

    #[test]
    fn the_dark_end_of_a_flip_paints_what_the_dark_theme_paints() {
        use diri_proto::grid::{GridCell, TermColor, TermStyle};

        // The mixed theme is light by rule while its paper is still dark. If
        // correction came with the flag, black-on-black and faint text would
        // jump on the first frame of a fade and again on the last.
        for (dark, light) in [
            (TermTheme::DIRIJOR_DARK, TermTheme::DIRIJOR_LIGHT),
            (TermTheme::SOLARIZED_DARK, TermTheme::SOLARIZED_LIGHT),
        ] {
            for t in [0.02, 0.1] {
                let mixed = dark.mix(&light, t);
                let unflagged = TermTheme {
                    appearance: ThemeAppearance::Dark,
                    ..mixed
                };
                for index in 0..16 {
                    for style in [TermStyle::empty(), TermStyle::DIM] {
                        let cell = GridCell::new(
                            u32::from('x'),
                            TermColor::Ansi(index),
                            TermColor::Default,
                            style,
                        );
                        assert_eq!(mixed.resolve_cell(cell), unflagged.resolve_cell(cell));
                    }
                }
            }
        }
    }

    #[test]
    fn no_text_color_pops_anywhere_along_a_flip() {
        use diri_proto::grid::{GridCell, TermColor, TermStyle};

        let (dark, light) = (TermTheme::TOKYO_NIGHT, TermTheme::GRUVBOX_LIGHT);
        let colors = (0..16)
            .map(TermColor::Ansi)
            .chain([TermColor::Rgb(255, 255, 255), TermColor::Rgb(255, 215, 0)]);
        for color in colors {
            let cell = GridCell::new(
                u32::from('x'),
                color,
                TermColor::Default,
                TermStyle::empty(),
            );
            let mut last = dark.resolve_cell(cell).foreground;
            let mut longest: f32 = 0.0;
            for step in 1..=240 {
                let next = dark
                    .mix(&light, step as f32 / 240.0)
                    .resolve_cell(cell)
                    .foreground;
                longest = longest.max(distance(last, next));
                last = next;
            }
            assert!(longest < 0.05, "{color:?} jumped {longest}");
            assert_eq!(last, light.resolve_cell(cell).foreground);
        }
    }

    #[test]
    fn a_fade_lands_exactly_on_the_catalog_theme_and_stays_there() {
        let start = Instant::now();
        let fade = ThemeFade::new(TermTheme::NORD, TermTheme::VESPER, start);
        let first = fade.sample(start);
        assert_eq!(first.signature(), TermTheme::NORD.signature());
        assert_eq!(first.id, TermTheme::VESPER.id);
        assert!(!fade.is_finished(start + FADE - Duration::from_millis(1)));
        assert_ne!(
            fade.sample(start + FADE - Duration::from_millis(1)),
            TermTheme::VESPER
        );
        assert!(fade.is_finished(start + FLIP_FADE));
        assert_eq!(fade.sample(start + FLIP_FADE), TermTheme::VESPER);
        assert_eq!(
            fade.sample(start + Duration::from_secs(60)),
            TermTheme::VESPER
        );
    }

    #[test]
    fn a_light_dark_flip_takes_longer_than_a_change_of_palette() {
        let start = Instant::now();
        let flip = ThemeFade::new(TermTheme::NORD, TermTheme::GITHUB_LIGHT, start);
        assert!(!flip.is_finished(start + FLIP_FADE - Duration::from_millis(1)));
        assert!(flip.is_finished(start + FLIP_FADE));
        let palette = ThemeFade::new(TermTheme::DRACULA, TermTheme::ONE_DARK, start);
        assert!(palette.is_finished(start + FADE + Duration::from_millis(5)));
    }

    #[test]
    fn retargeting_starts_from_the_colors_on_screen() {
        let start = Instant::now();
        let fade = ThemeFade::new(TermTheme::DIRIJOR_DARK, TermTheme::DRACULA, start);
        let now = start + Duration::from_millis(60);
        let shown = fade.sample(now);
        let chased = fade.retarget(TermTheme::SOLARIZED_DARK, now);
        assert_eq!(chased.sample(now), shown);
        let next = chased.sample(now + Duration::from_millis(8));
        assert_eq!(next.id, TermTheme::SOLARIZED_DARK.id);
        assert!(distance(next.background, shown.background) < 0.02);
        assert_eq!(chased.target(), TermTheme::SOLARIZED_DARK);
        assert_eq!(chased.sample(now + FLIP_FADE), TermTheme::SOLARIZED_DARK);
    }

    #[test]
    fn holding_the_arrow_key_never_jumps() {
        // Key repeat walks the whole catalog at 30 ms per theme while frames
        // land every 8 ms. No frame may move the background further than an
        // undisturbed fade between the two most distant themes does.
        let frame = Duration::from_millis(8);
        let start = Instant::now();
        let widest = {
            let fade = ThemeFade::new(TermTheme::VESPER, TermTheme::GITHUB_LIGHT, start);
            distance(
                fade.sample(start).background,
                fade.sample(start + frame).background,
            )
        };
        let mut fade = ThemeFade::new(TermTheme::CATALOG[0], TermTheme::CATALOG[1], start);
        let mut shown = fade.sample(start).background;
        let mut longest: f32 = 0.0;
        for tick in 1..=(30 * TermTheme::CATALOG.len() as u32 / 8 + 40) {
            let now = start + frame * tick;
            let wanted = TermTheme::CATALOG
                [((tick * 8 / 30) as usize + 1).min(TermTheme::CATALOG.len() - 1)];
            if wanted != fade.target() {
                fade = fade.retarget(wanted, now);
            }
            let next = fade.sample(now).background;
            longest = longest.max(distance(shown, next));
            shown = next;
        }
        assert!(longest <= widest * 1.05, "{longest} > {widest}");
        assert_eq!(
            shown,
            TermTheme::CATALOG[TermTheme::CATALOG.len() - 1].background
        );
    }

    #[test]
    fn easing_decelerates_into_the_destination() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(2.0), 1.0);
        assert!(ease_out(0.25) > 0.5);
        let mut last = 0.0;
        for step in 1..=10 {
            let eased = ease_out(step as f32 / 10.0);
            assert!(eased > last);
            last = eased;
        }
    }
}
