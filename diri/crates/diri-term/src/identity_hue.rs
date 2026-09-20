//! Identity colors that are equally loud on every theme.
//!
//! A project is told apart by a small colored mark. A mark that is brighter
//! or more saturated than its neighbours reads as more important, so the
//! colors here differ in Oklch hue and in nothing else: one chroma for all of
//! them, and one lightness per theme, set a fixed step away from the theme's
//! background. The step is what makes the mark weigh the same on a near-black
//! theme as on a cream one.
//!
//! The lightness is a continuous function of the background alone, so a theme
//! crossfade carries the mark along with the surface it sits on.

use gpui::Rgba;

use crate::contrast::{Oklch, contrast_ratio, in_gamut, linear, oklch, rgba};

/// Hue angles, in degrees, a mark may take. Two arcs are left out because
/// the application already speaks in them: red through amber (errors,
/// needs-input, the delegation clay, 10° to 95°) and the green of a finished
/// turn (around 147°). What remains holds six hues 45° apart. Eight at 30°
/// were tried first: side by side they differ, but a 3 px mark is seen alone,
/// and at this chroma teal, cyan and blue could not be told apart from
/// memory. Six is also about as many colors as anyone names reliably.
pub const WHEEL: [f32; 6] = [110.0, 170.0, 215.0, 260.0, 305.0, 350.0];

/// Low enough to stay inside sRGB at every wheel hue and every catalog
/// lightness (cyan on a light theme runs out first, just under 0.10), so no
/// hue has to shed chroma and come out quieter than the rest; and well under
/// the status inks (0.13 to 0.21), which must stay the loudest color on a row.
pub const CHROMA: f32 = 0.09;

/// Oklab lightness between a background and a mark on it: the smallest
/// steps that keep every catalog theme at 3:1 against its sidebar, the floor
/// WCAG sets for interface graphics. Nothing about the mark needs to be
/// brighter than "there", and the two steps coming out nearly equal is what
/// lets a dark and a light theme weigh the same (3.1 to 4.3:1 dark, 3.1 to
/// 3.8:1 light).
const DARK_STEP: f32 = 0.38;
const LIGHT_STEP: f32 = 0.39;

/// Background lightness up to which a theme is dark, and from which it is
/// light. Every catalog theme is outside the gap; only a crossfade passes
/// through it, and the mark's lightness slides across rather than flipping.
const DARK_PAPER: f32 = 0.40;
const LIGHT_PAPER: f32 = 0.88;

/// The mark color for `hue_degrees` on a theme with this background.
#[must_use]
pub fn mark(background: Rgba, hue_degrees: f32) -> Rgba {
    rgba(
        in_gamut(Oklch {
            lightness: mark_lightness(oklch(linear(background)).lightness),
            chroma: CHROMA,
            hue: hue_degrees.to_radians(),
        }),
        1.0,
    )
}

fn mark_lightness(paper: f32) -> f32 {
    if paper <= DARK_PAPER {
        paper + DARK_STEP
    } else if paper >= LIGHT_PAPER {
        paper - LIGHT_STEP
    } else {
        let along = (paper - DARK_PAPER) / (LIGHT_PAPER - DARK_PAPER);
        (DARK_PAPER + DARK_STEP) * (1.0 - along) + (LIGHT_PAPER - LIGHT_STEP) * along
    }
}

/// How a color sits on a surface, in the terms the marks are specified in.
/// Public so the application can hold its status inks to the same ruler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loudness {
    /// Absolute Oklab lightness distance from the surface.
    pub step: f32,
    pub chroma: f32,
    /// Degrees in `0..360`.
    pub hue: f32,
    /// WCAG contrast ratio against the surface.
    pub contrast: f32,
}

#[must_use]
pub fn loudness(color: Rgba, surface: Rgba) -> Loudness {
    let ink = oklch(linear(color));
    let paper = oklch(linear(surface));
    Loudness {
        step: (ink.lightness - paper.lightness).abs(),
        chroma: ink.chroma,
        hue: ink.hue.to_degrees().rem_euclid(360.0),
        contrast: contrast_ratio(color, surface),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contrast::{MINIMUM_DIM_CONTRAST, hue_delta};
    use crate::theme::{TermTheme, ThemeAppearance};

    /// Chrome draws the sidebar and the tab strip on the background mixed 8%
    /// toward the text color; marks sit on that, not on the bare background.
    fn sidebar_surface(theme: &TermTheme) -> Rgba {
        let mix = |paper: f32, ink: f32| paper * 0.92 + ink * 0.08;
        Rgba {
            r: mix(theme.background.r, theme.foreground.r),
            g: mix(theme.background.g, theme.foreground.g),
            b: mix(theme.background.b, theme.foreground.b),
            a: 1.0,
        }
    }

    #[test]
    fn every_mark_is_equally_loud_on_every_theme() {
        for theme in TermTheme::CATALOG {
            let step = match theme.appearance {
                ThemeAppearance::Dark => DARK_STEP,
                ThemeAppearance::Light => LIGHT_STEP,
            };
            for hue in WHEEL {
                let measured = loudness(mark(theme.background, hue), theme.background);
                // 8-bit-free floats: only the gamut search could move these,
                // and it must not have run.
                assert!(
                    (measured.chroma - CHROMA).abs() < 0.002,
                    "{} {hue}: shed chroma to {}",
                    theme.id,
                    measured.chroma
                );
                assert!(
                    (measured.step - step).abs() < 0.004,
                    "{} {hue}: step {}",
                    theme.id,
                    measured.step
                );
                assert!(
                    hue_delta(measured.hue.to_radians(), hue.to_radians())
                        .to_degrees()
                        .abs()
                        < 1.5,
                    "{} {hue}: hue drifted to {}",
                    theme.id,
                    measured.hue
                );
            }
        }
    }

    #[test]
    fn marks_are_visible_on_chrome_and_quieter_than_body_text() {
        for theme in TermTheme::CATALOG {
            let surface = sidebar_surface(&theme);
            let text = contrast_ratio(theme.foreground, surface);
            let ratios = WHEEL.map(|hue| contrast_ratio(mark(theme.background, hue), surface));
            let (low, high) = ratios
                .iter()
                .fold((f32::MAX, 0.0_f32), |(low, high), ratio| {
                    (low.min(*ratio), high.max(*ratio))
                });
            // The floor WCAG sets for interface graphics.
            assert!(low >= MINIMUM_DIM_CONTRAST, "{}: {low}", theme.id);
            // Solarized sets its own body text at 4.2:1, under any visible
            // mark; everywhere else the mark stays below the text beside it.
            let ceiling = text.max(6.0);
            assert!(high < ceiling, "{}: {high} >= {ceiling}", theme.id);
            // Equal lightness is nearly equal luminance: no hue stands out.
            assert!(high / low < 1.12, "{}: {low}..{high}", theme.id);
        }
    }

    #[test]
    fn wheel_hues_are_separated_and_avoid_the_reserved_inks() {
        // Danger, clay, attention, fresh: `diri_ui::Ink` and `Palette`.
        let reserved = [0xf5453a_u32, 0xd97757, 0xf5a623, 0x34c759]
            .map(|ink| loudness(crate::theme::hex(ink), crate::theme::hex(0)));
        for (index, hue) in WHEEL.iter().enumerate() {
            for other in &WHEEL[index + 1..] {
                let apart = hue_delta(hue.to_radians(), other.to_radians())
                    .to_degrees()
                    .abs();
                assert!(apart >= 44.9, "{hue} and {other} are {apart} apart");
            }
            for ink in reserved {
                let apart = hue_delta(hue.to_radians(), ink.hue.to_radians())
                    .to_degrees()
                    .abs();
                assert!(apart >= 20.0, "{hue} is {apart} from a status ink");
                assert!(ink.chroma > CHROMA * 1.25);
            }
        }
    }

    #[test]
    fn a_light_dark_crossfade_carries_the_mark_without_a_jump() {
        let (dark, light) = (TermTheme::TOKYO_NIGHT, TermTheme::GITHUB_LIGHT);
        for hue in WHEEL {
            let mut last = mark(dark.background, hue);
            for step in 1..=60 {
                let theme = dark.mix(&light, step as f32 / 60.0);
                let next = mark(theme.background, hue);
                let jump = (next.r - last.r)
                    .abs()
                    .max((next.g - last.g).abs())
                    .max((next.b - last.b).abs());
                assert!(jump < 0.06, "{hue} jumped {jump} at step {step}");
                last = next;
            }
            assert_eq!(last, mark(light.background, hue));
        }
    }

    /// Prints the numbers the pull request quotes, and the colors the contact
    /// sheet is drawn from: `cargo test -p diri-term identity_hue -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "prints a table"]
    fn print_mark_table() {
        for theme in TermTheme::CATALOG {
            let surface = sidebar_surface(&theme);
            let byte = |channel: f32| (channel * 255.0).round() as u32;
            let hex = |color: Rgba| {
                format!(
                    "{:02x}{:02x}{:02x}",
                    byte(color.r),
                    byte(color.g),
                    byte(color.b)
                )
            };
            let marks = WHEEL.map(|hue| mark(theme.background, hue));
            let ratios = marks.map(|color| contrast_ratio(color, surface));
            println!(
                "{} {} {} {} text={:.2} mark={:.2}..{:.2}",
                theme.id,
                hex(theme.background),
                hex(surface),
                marks.map(hex).join(","),
                contrast_ratio(theme.foreground, surface),
                ratios.iter().copied().fold(f32::MAX, f32::min),
                ratios.iter().copied().fold(0.0, f32::max),
            );
        }
    }
}
