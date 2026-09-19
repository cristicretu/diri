//! SGR faint as an opaque color.
//!
//! Faint used to be the foreground at half opacity, which has two problems.
//! The glyph is translucent, so whatever is behind the cell ends up in the
//! text: under the glass window material that includes the desktop. And the
//! blend runs per sRGB channel, which only approximates a perceptual fade.
//! It is close for the muted colors of most palettes, but a saturated color
//! loses more than its share (pure blue kept 42% of its lightness distance
//! from a dark background where default text kept 54%), and how much a color
//! keeps depends on its hue.
//!
//! Faint text is instead the foreground moved a fixed share of the way to the
//! background it is painted on, along a straight line in Oklab. Lightness and
//! chroma shrink together and the hue holds, so every color keeps exactly the
//! same share of its perceived distance from the background.

use gpui::Rgba;

use super::{DIM_OPACITY, Oklch, composite, in_gamut, linear, oklch, rgba};
use crate::theme::TermTheme;

/// `foreground` as SGR faint paints it on `background`: opaque, and as much
/// closer to the background as the theme's own faint text is.
///
/// Over a translucent terminal background the true backdrop is unknown. The
/// theme's background color is what the surface tint is derived from, so it
/// stands in, and the glyph itself no longer lets the backdrop through.
pub(super) fn faded(theme: &TermTheme, foreground: Rgba, background: Rgba) -> Rgba {
    let step = step(theme);
    let [ink_l, ink_a, ink_b] = oklab(foreground);
    let [paper_l, paper_a, paper_b] = oklab(background);
    let toward = |ink: f32, paper: f32| ink + (paper - ink) * step;
    let (a, b) = (toward(ink_a, paper_a), toward(ink_b, paper_b));
    let mixed = Oklch {
        lightness: toward(ink_l, paper_l),
        chroma: a.hypot(b),
        hue: b.atan2(a),
    };
    rgba(in_gamut(mixed), foreground.a)
}

/// The share of the way to the background that faint text travels.
///
/// Calibrated per theme so that default text on the default background lands
/// on the lightness the half-opacity blend gave it: faint default text, by
/// far the most common kind, looks as it always has, and every other color
/// takes the same perceptual step. The blend happened in gamma space, which
/// is why the share is not simply the opacity (it is near 0.46 on dark themes
/// and 0.53 on light ones).
fn step(theme: &TermTheme) -> f32 {
    let lightness = |color: Rgba| oklch(linear(color)).lightness;
    let ink = lightness(theme.foreground);
    let paper = lightness(theme.background);
    let blended = lightness(composite(
        theme.foreground.opacity(DIM_OPACITY),
        theme.background,
    ));
    if (ink - paper).abs() < 0.01 {
        // No lightness separates this theme's ink from its paper, so there is
        // nothing to calibrate against.
        return DIM_OPACITY;
    }
    ((ink - blended) / (ink - paper)).clamp(0.0, 1.0)
}

/// Oklab as `[L, a, b]`. The solver works in the polar form; mixing two
/// colors needs the rectangular one, where a gray has no hue to get wrong.
fn oklab(color: Rgba) -> [f32; 3] {
    let Oklch {
        lightness,
        chroma,
        hue,
    } = oklch(linear(color));
    [lightness, chroma * hue.cos(), chroma * hue.sin()]
}

#[cfg(test)]
mod tests {
    use diri_proto::grid::{GridCell, TermColor, TermStyle};

    use super::*;
    use crate::theme::ThemeAppearance;

    fn cell(fg: TermColor, bg: TermColor, style: TermStyle) -> GridCell {
        GridCell::new(u32::from('x'), fg, bg, style)
    }

    fn lightness(color: Rgba) -> f32 {
        oklch(linear(color)).lightness
    }

    /// Share of its lightness difference from the background that `faint`
    /// kept, relative to `normal`.
    fn kept(normal: Rgba, faint: Rgba, background: Rgba) -> f32 {
        (lightness(faint) - lightness(background)) / (lightness(normal) - lightness(background))
    }

    /// Foregrounds an agent screen actually fades: the default, the ANSI
    /// palette, and truecolor accents.
    fn foregrounds() -> Vec<TermColor> {
        let mut colors = vec![TermColor::Default];
        colors.extend((0..16).map(TermColor::Ansi));
        colors.extend([
            TermColor::Rgb(215, 119, 87),
            TermColor::Rgb(177, 185, 249),
            TermColor::Rgb(255, 255, 0),
            TermColor::Rgb(0, 0, 255),
            TermColor::Rgb(255, 0, 0),
            TermColor::Rgb(0, 255, 255),
        ]);
        colors
    }

    #[test]
    fn faint_is_opaque_in_every_theme() {
        for theme in TermTheme::CATALOG {
            for fg in foregrounds() {
                for bg in [TermColor::Default, TermColor::Rgb(34, 92, 43)] {
                    for style in [TermStyle::DIM, TermStyle::DIM | TermStyle::INVERSE] {
                        let resolved = theme.resolve_cell(cell(fg, bg, style));
                        assert_eq!(resolved.foreground.a, 1.0, "{fg:?} on {}", theme.id);
                    }
                }
            }
        }
    }

    #[test]
    fn faint_default_text_stays_where_half_opacity_put_it() {
        for theme in TermTheme::CATALOG {
            let resolved =
                theme.resolve_cell(cell(TermColor::Default, TermColor::Default, TermStyle::DIM));
            let before = composite(theme.foreground.opacity(DIM_OPACITY), theme.background);
            let after = resolved.foreground;
            for (left, right) in [
                (before.r, after.r),
                (before.g, after.g),
                (before.b, after.b),
            ] {
                // Not exact: a tinted ink on tinted paper (Solarized) takes a
                // slightly different path through Oklab than through sRGB.
                assert!(
                    (left - right).abs() <= 4.0 / 255.0,
                    "{}: {before:?} became {after:?}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn every_hue_recedes_by_the_same_share() {
        for theme in TermTheme::CATALOG
            .into_iter()
            .filter(|theme| theme.appearance == ThemeAppearance::Dark)
        {
            let expected = 1.0 - step(&theme);
            for fg in foregrounds() {
                let normal = theme.resolve_color(fg, false);
                if (lightness(normal) - lightness(theme.background)).abs() < 0.1 {
                    // Too close to the background for a ratio to mean much.
                    continue;
                }
                let faint = theme
                    .resolve_cell(cell(fg, TermColor::Default, TermStyle::DIM))
                    .foreground;
                let kept = kept(normal, faint, theme.background);
                assert!(
                    (kept - expected).abs() < 0.02,
                    "{fg:?} on {} kept {kept}, default text keeps {expected}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn half_opacity_shortchanged_saturated_colors() {
        // The defect this module removes, pinned so the claim stays honest.
        let theme = TermTheme::DIRIJOR_DARK;
        let blend = |color: Rgba| {
            kept(
                color,
                composite(color.opacity(DIM_OPACITY), theme.background),
                theme.background,
            )
        };
        let ink = blend(theme.foreground);
        let blue = blend(theme.resolve_color(TermColor::Rgb(0, 0, 255), false));
        assert!(ink - blue > 0.1, "default text kept {ink}, blue {blue}");
    }

    #[test]
    fn faint_fades_toward_the_background_it_is_painted_on() {
        let theme = TermTheme::DIRIJOR_DARK;
        let panel = TermColor::Rgb(34, 92, 43);
        let on_panel = theme
            .resolve_cell(cell(TermColor::Default, panel, TermStyle::DIM))
            .foreground;
        let on_paper = theme
            .resolve_cell(cell(TermColor::Default, TermColor::Default, TermStyle::DIM))
            .foreground;
        // Toward a green panel the text picks up green, not the paper's blue.
        assert!(on_panel.g > on_panel.r && on_panel.g > on_panel.b);
        assert_ne!(on_panel, on_paper);

        // Inverse video swaps the roles first: the painted foreground is the
        // theme background, fading toward the theme foreground.
        let inverse = theme.resolve_cell(cell(
            TermColor::Default,
            TermColor::Default,
            TermStyle::DIM | TermStyle::INVERSE,
        ));
        assert_eq!(inverse.background, theme.foreground);
        assert!(lightness(inverse.foreground) > lightness(theme.background));
        assert!(lightness(inverse.foreground) < lightness(theme.foreground));
    }

    #[test]
    fn hue_survives_the_fade() {
        let theme = TermTheme::DIRIJOR_DARK;
        for slot in [1, 2, 3, 4, 5, 6] {
            let normal = oklch(linear(theme.ansi[slot]));
            let faint = oklch(linear(faded(&theme, theme.ansi[slot], theme.background)));
            let drift = super::super::hue_delta(normal.hue, faint.hue).abs();
            assert!(
                drift.to_degrees() < 12.0,
                "ansi {slot} drifted {} degrees",
                drift.to_degrees()
            );
            assert!(faint.chroma > 0.03, "ansi {slot} lost its color");
        }
    }

    #[test]
    fn invisible_still_wins() {
        let theme = TermTheme::DIRIJOR_DARK;
        let resolved = theme.resolve_cell(cell(
            TermColor::Ansi(3),
            TermColor::Default,
            TermStyle::DIM | TermStyle::INVISIBLE,
        ));
        assert_eq!(resolved.foreground.a, 0.0);
    }

    #[test]
    fn a_theme_without_lightness_separation_does_not_divide_by_zero() {
        let mut theme = TermTheme::DIRIJOR_DARK;
        theme.foreground = theme.background;
        let color = faded(&theme, theme.ansi[3], theme.background);
        assert!(color.r.is_finite() && color.g.is_finite() && color.b.is_finite());
    }
}
