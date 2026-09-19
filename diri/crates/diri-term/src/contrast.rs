//! Automatic contrast correction for terminal text.
//!
//! Terminal programs are overwhelmingly designed against dark backgrounds:
//! white emphasis, pastel accents, and neon truecolor all vanish into a light
//! theme. When a cell's foreground is unreadable on the background it will be
//! painted over, the foreground is moved through Oklab, where lightness can
//! change without the hue drifting, until it reaches a WCAG contrast ratio.
//! The further a color has to travel, the more its hue and chroma are pulled
//! toward the theme's own rendition of that color, so a corrected
//! screen reads as the theme rather than as a darkened copy of another one.
//!
//! Correction is decided per color pair, never per program, and a readable
//! pair is returned bit-for-bit unchanged. Solved pairs live in a bounded
//! cache because the renderer resolves every cell several times per paint.

use std::cell::{Cell, RefCell};

use diri_proto::grid::{GridCell, TermColor, TermStyle};
use gpui::Rgba;

use crate::theme::TermTheme;

mod faint;

/// WCAG AA for body text.
pub(crate) const MINIMUM_TEXT_CONTRAST: f32 = 4.5;
/// SGR faint text is meant to recede, so it is held to the WCAG floor for
/// large text and interface components.
pub(crate) const MINIMUM_DIM_CONTRAST: f32 = 3.0;
/// The half-opacity blend SGR faint text used to be, and still the measure of
/// how far faint text recedes; see [`faint`].
const DIM_OPACITY: f32 = 0.5;

/// Below this Oklch chroma a color has no hue worth preserving.
pub(crate) const NEUTRAL_CHROMA: f32 = 0.02;
/// At and above this chroma a color is treated as fully chromatic.
const CHROMATIC_CHROMA: f32 = 0.08;
/// Lightness travel below which a color keeps its authored hue and chroma.
const HARMONIZE_FROM: f32 = 0.03;
/// Lightness travel at which harmonization reaches full strength.
const HARMONIZE_FULL: f32 = 0.30;
/// Share of the rotation onto the theme's palette applied at full strength.
const HUE_PULL: f32 = 0.75;
/// Share of excess chroma over the theme accent shed at full strength.
const CHROMA_PULL: f32 = 0.5;
/// A palette slot further than this from its primary's hue was repurposed by
/// the theme and does not steer anything.
pub(crate) const ACCENT_REACH_DEGREES: f32 = 60.0;
/// The chromatic ANSI slots with the primaries they name, in hue order.
const PRIMARIES: [(usize, LinearRgb); 6] = [
    (1, [1.0, 0.0, 0.0]),
    (3, [1.0, 1.0, 0.0]),
    (2, [0.0, 1.0, 0.0]),
    (6, [0.0, 1.0, 1.0]),
    (4, [0.0, 0.0, 1.0]),
    (5, [1.0, 0.0, 1.0]),
];

/// Whether the theme, rather than the program, is answerable for this cell's
/// legibility.
///
/// A pair the program authored outright (truecolor or the fixed 256-color
/// cube on both sides) looks the same under every theme and is left alone:
/// diff panels, images, and gradients keep their colors. A pair made only of
/// theme defaults is the theme's own choice. Everything in between mixes a
/// color the program picked for a dark terminal with one the theme supplied.
/// Powerline separators are exempt because they continue the background of
/// the neighboring cell and must match it exactly.
pub(crate) fn applies(cell: GridCell) -> bool {
    let defaults = is_default(cell.fg) && is_default(cell.bg);
    let authored = !is_theme_owned(cell.fg) && !is_theme_owned(cell.bg);
    !defaults
        && !authored
        && !cell.style.contains(TermStyle::INVISIBLE)
        && !is_powerline(cell.scalar)
}

/// The readable foreground for `cell`, given its resolved colors.
///
/// `foreground` and `background` are the colors after inverse video swapped
/// them. `own` is whether the cell is corrected against its own background
/// (see [`applies`]); without it only SGR faint changes the color. The result
/// is the color as painted, SGR faint included.
pub(crate) fn painted_foreground(
    theme: &TermTheme,
    cell: GridCell,
    foreground: Rgba,
    background: Rgba,
    own: bool,
) -> Rgba {
    let inverse = cell.style.contains(TermStyle::INVERSE);
    let dim = cell.style.contains(TermStyle::DIM);
    let key = CacheKey {
        colors: (u64::from(cell.fg.packed()) << 32) | u64::from(cell.bg.packed()),
        theme: theme_key(theme),
        flags: u8::from(inverse) | (u8::from(dim) << 1) | (u8::from(own) << 2),
    };
    // Text comes in runs of one style, and the reading view resolves every
    // visible cell on every frame: answering a repeat of the previous key
    // without touching the table is what keeps faint rows at their old cost.
    if let Some((cached, color)) = LAST.get()
        && cached == key
    {
        return color;
    }
    let color = CACHE.with_borrow_mut(|cache| {
        let set = &mut cache[key.set()];
        if let Some(way) = set
            .iter()
            .position(|entry| entry.is_some_and(|(cached, _)| cached == key))
        {
            // Most recently used first, so the oldest way is the one evicted.
            set[..=way].rotate_right(1);
            return set[0].map_or(foreground, |(_, color)| color);
        }
        // What the cell paints when nothing corrects it: SGR faint already
        // faded. A theme fading toward light blends from here.
        let authored = if dim {
            faint::faded(theme, foreground, background)
        } else {
            foreground
        };
        let mut color = authored;
        if own {
            let on_paper = !inverse && is_default(cell.bg);
            let corrected = correct(theme, authored, background, on_paper, dim);
            color = phased_in(theme, authored, corrected);
        }
        set.rotate_right(1);
        set[0] = Some((key, color));
        color
    });
    LAST.set(Some((key, color)));
    color
}

/// Paper lightness, in Oklab, below which a light theme corrects nothing and
/// at which it corrects in full. Below the lower bound white still reads on
/// the paper, so the solver may answer on either side of it and its answer
/// can swap sides from one frame to the next; above it every answer is darker
/// than the paper and moves continuously. Every catalog light theme sits far
/// above the upper bound and is unaffected.
const PHASE_IN_FROM: f32 = 0.65;
const PHASE_IN_FULL: f32 = 0.85;

/// A theme fading between dark and light is light for the whole fade (see
/// `TermTheme::mix`) while its paper is anywhere in between. Correction
/// arrives with the paper's lightness instead of with the flag, so the dark
/// end of a fade paints exactly what the dark theme paints and nothing pops
/// on the first or last frame. Only a cache miss pays for this.
#[cold]
#[inline(never)]
fn phased_in(theme: &TermTheme, authored: Rgba, corrected: Rgba) -> Rgba {
    let paper = oklch(linear(theme.background)).lightness;
    if paper >= PHASE_IN_FULL {
        return corrected;
    }
    crate::crossfade::mix_color(
        authored,
        corrected,
        ramp(paper, PHASE_IN_FROM, PHASE_IN_FULL),
    )
}

pub(crate) fn contrast_ratio(left: Rgba, right: Rgba) -> f32 {
    ratio(
        relative_luminance(linear(left)),
        relative_luminance(linear(right)),
    )
}

const fn is_default(color: TermColor) -> bool {
    matches!(color, TermColor::Default | TermColor::DefaultInverted)
}

const fn is_theme_owned(color: TermColor) -> bool {
    matches!(
        color,
        TermColor::Default | TermColor::DefaultInverted | TermColor::Ansi(0..=15)
    )
}

const fn is_powerline(scalar: u32) -> bool {
    matches!(scalar, 0xe0b0..=0xe0d7)
}

/// A screen shows tens of color pairs, a gradient a few thousand. Four ways
/// per set keep a handful of colliding pairs from evicting each other on
/// every cell, which a direct-mapped table of any practical size does.
const CACHE_SETS: usize = 1024;
const CACHE_WAYS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    colors: u64,
    theme: u64,
    flags: u8,
}

impl CacheKey {
    fn set(self) -> usize {
        let mixed = (self.colors ^ self.theme.rotate_left(17) ^ u64::from(self.flags))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (mixed >> 40) as usize % CACHE_SETS
    }
}

type CacheSet = [Option<(CacheKey, Rgba)>; CACHE_WAYS];

thread_local! {
    static LAST: Cell<Option<(CacheKey, Rgba)>> = const { Cell::new(None) };
    static CACHE: RefCell<Vec<CacheSet>> = RefCell::new(vec![[None; CACHE_WAYS]; CACHE_SETS]);
}

/// Identifies everything in a theme the solver reads, cheaply enough to
/// compute per cell. Catalog themes are distinguished by their static id;
/// the default colors guard a theme value rebuilt under the same id. A theme
/// fading into another keeps one id while its palette moves every frame, and
/// two frames can share default colors, so `TermTheme::mix` stamps each
/// palette it produces and the stamp stands in for the accents here.
fn theme_key(theme: &TermTheme) -> u64 {
    let mut key = theme.id.as_ptr() as u64 ^ theme.blend;
    for color in [theme.background, theme.foreground] {
        for channel in [color.r, color.g, color.b] {
            key = key.rotate_left(11) ^ u64::from(channel.to_bits());
        }
    }
    key
}

pub(crate) type LinearRgb = [f32; 3];

#[derive(Clone, Copy)]
pub(crate) struct Oklch {
    pub(crate) lightness: f32,
    pub(crate) chroma: f32,
    /// Radians.
    pub(crate) hue: f32,
}

/// The background a foreground is judged against.
struct Surface {
    luminance: f32,
    target: f32,
}

impl Surface {
    fn contrast(&self, color: LinearRgb) -> f32 {
        ratio(relative_luminance(color), self.luminance)
    }

    fn reads(&self, color: LinearRgb) -> bool {
        self.contrast(color) >= self.target
    }

    /// The lightness nearest `from` at which `chroma` and `hue` become
    /// readable, preferring the side of the background the color is already
    /// on. `None` when neither black nor white is readable here.
    fn readable_lightness(&self, from: Oklch, chroma: f32, hue: f32) -> Option<f32> {
        let lighter = relative_luminance(in_gamut(from)) >= self.luminance;
        let extremes = if lighter { [1.0, 0.0] } else { [0.0, 1.0] };
        let extreme = extremes.into_iter().find(|&lightness| {
            self.reads(in_gamut(Oklch {
                lightness,
                chroma,
                hue,
            }))
        })?;
        let mut unreadable = from.lightness;
        let mut readable = extreme;
        for _ in 0..16 {
            let lightness = (unreadable + readable) * 0.5;
            if self.reads(in_gamut(Oklch {
                lightness,
                chroma,
                hue,
            })) {
                readable = lightness;
            } else {
                unreadable = lightness;
            }
        }
        Some(readable)
    }
}

fn correct(
    theme: &TermTheme,
    foreground: Rgba,
    background: Rgba,
    on_paper: bool,
    dim: bool,
) -> Rgba {
    // Faint text arrives already faded, as the opaque color it paints, and
    // is held to the lower floor for text that is meant to recede.
    let target = if dim {
        MINIMUM_DIM_CONTRAST
    } else {
        MINIMUM_TEXT_CONTRAST
    };
    let surface = Surface {
        luminance: relative_luminance(linear(background)),
        target,
    };
    if surface.reads(linear(foreground)) {
        return foreground;
    }

    let authored = oklch(linear(foreground));
    let Some(minimal) = surface.readable_lightness(authored, authored.chroma, authored.hue) else {
        // A mid-tone background neither black nor white clears: take the
        // better of the two rather than give up.
        let black = [0.0; 3];
        let white = [1.0; 3];
        let best = if surface.contrast(black) >= surface.contrast(white) {
            black
        } else {
            white
        };
        return rgba(best, foreground.a);
    };

    let travel = (minimal - authored.lightness).abs();
    let strength = ramp(travel, HARMONIZE_FROM, HARMONIZE_FULL);
    let neutrality = 1.0 - ramp(authored.chroma, NEUTRAL_CHROMA, CHROMATIC_CHROMA);
    let (chroma, hue) = harmonized(theme, authored, strength, neutrality);
    let lightness = surface
        .readable_lightness(authored, chroma, hue)
        .unwrap_or(minimal);

    // A gray carries nothing but emphasis, and a dark-terminal program spends
    // lightness on it: white is its strongest text. Stopping every gray at the
    // same minimal contrast would flatten that hierarchy, so on the theme's
    // own background grays take their place between paper and ink instead.
    let mut solved = Oklch {
        lightness,
        chroma,
        hue,
    };
    if on_paper && neutrality > 0.0 {
        let paper = oklch(linear(theme.background)).lightness;
        // Faint grays settle against faint default text, not full ink.
        let ink = if dim {
            faint::faded(theme, theme.foreground, theme.background)
        } else {
            theme.foreground
        };
        let ink = oklch(linear(ink)).lightness;
        let emphasis = if paper >= ink {
            authored.lightness
        } else {
            1.0 - authored.lightness
        };
        let role = paper + (ink - paper) * emphasis;
        let further = if lightness <= authored.lightness {
            role.min(lightness)
        } else {
            role.max(lightness)
        };
        let placed = Oklch {
            lightness: lightness + (further - lightness) * neutrality,
            ..solved
        };
        if surface.reads(in_gamut(placed)) {
            solved = placed;
        }
    }
    rgba(in_gamut(solved), foreground.a)
}

/// Chroma and hue after the pull toward the theme. Neutrals take on the tint
/// of the theme's ink. Chromatic colors are read the way a palette would name
/// them: the hue circle is warped so that pure red, yellow, green, cyan, blue,
/// and magenta land on the theme's own, and every hue in between moves with
/// its neighbors. Naming by primary rather than by nearest accent matters for
/// yellow, which sits closer to most themes' green than to their ochre.
fn harmonized(theme: &TermTheme, authored: Oklch, strength: f32, neutrality: f32) -> (f32, f32) {
    use std::f32::consts::TAU;

    if authored.chroma < NEUTRAL_CHROMA {
        let ink = oklch(linear(theme.foreground));
        let chroma = authored.chroma + (ink.chroma - authored.chroma) * strength;
        return (chroma, ink.hue);
    }

    // (hue of the primary, rotation onto the theme's slot, that slot's chroma)
    let reach = ACCENT_REACH_DEGREES.to_radians();
    let anchors = PRIMARIES.map(|(slot, primary)| {
        let primary = oklch(primary).hue.rem_euclid(TAU);
        let accent = oklch(linear(theme.ansi[slot]));
        let rotation = hue_delta(primary, accent.hue);
        if accent.chroma >= NEUTRAL_CHROMA && rotation.abs() <= reach {
            (primary, rotation, accent.chroma)
        } else {
            // The theme repurposed this slot; it says nothing about the hue.
            (primary, 0.0, f32::INFINITY)
        }
    });

    let hue = authored.hue.rem_euclid(TAU);
    let after = anchors
        .iter()
        .position(|(primary, ..)| *primary > hue)
        .unwrap_or(0);
    let (from, from_rotation, from_chroma) = anchors[(after + anchors.len() - 1) % anchors.len()];
    let (to, to_rotation, to_chroma) = anchors[after];
    let along = (hue - from).rem_euclid(TAU) / (to - from).rem_euclid(TAU);
    let rotation = from_rotation + (to_rotation - from_rotation) * along;
    let accent_chroma = if along < 0.5 { from_chroma } else { to_chroma };

    let pull = strength * (1.0 - neutrality);
    let excess = (authored.chroma - accent_chroma).max(0.0);
    (
        authored.chroma - excess * pull * CHROMA_PULL,
        authored.hue + rotation * pull * HUE_PULL,
    )
}

/// Signed shortest rotation from `from` to `to`, in radians.
pub(crate) fn hue_delta(from: f32, to: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (to - from + PI).rem_euclid(TAU) - PI
}

pub(crate) fn composite(foreground: Rgba, background: Rgba) -> Rgba {
    let blend = |ink: f32, paper: f32| paper + (ink - paper) * foreground.a;
    Rgba {
        r: blend(foreground.r, background.r),
        g: blend(foreground.g, background.g),
        b: blend(foreground.b, background.b),
        a: 1.0,
    }
}

fn ramp(value: f32, from: f32, to: f32) -> f32 {
    ((value - from) / (to - from)).clamp(0.0, 1.0)
}

fn ratio(left: f32, right: f32) -> f32 {
    (left.max(right) + 0.05) / (left.min(right) + 0.05)
}

fn relative_luminance([r, g, b]: LinearRgb) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn decode(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn encode(channel: f32) -> f32 {
    if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

pub(crate) fn linear(color: Rgba) -> LinearRgb {
    [decode(color.r), decode(color.g), decode(color.b)]
}

pub(crate) fn rgba(color: LinearRgb, alpha: f32) -> Rgba {
    Rgba {
        r: encode(color[0]),
        g: encode(color[1]),
        b: encode(color[2]),
        a: alpha,
    }
}

// Björn Ottosson's Oklab, https://bottosson.github.io/posts/oklab/.
pub(crate) fn oklch([r, g, b]: LinearRgb) -> Oklch {
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let b = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
    Oklch {
        lightness: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        chroma: a.hypot(b),
        hue: b.atan2(a),
    }
}

fn unclipped(color: Oklch) -> LinearRgb {
    let a = color.chroma * color.hue.cos();
    let b = color.chroma * color.hue.sin();
    let l = (color.lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (color.lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (color.lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

/// `color` as displayable linear sRGB. Out-of-gamut colors shed chroma at
/// constant lightness and hue, which is what keeps a darkened yellow yellow
/// instead of clipping a channel and sliding toward green.
pub(crate) fn in_gamut(color: Oklch) -> LinearRgb {
    fn displayable(rgb: LinearRgb) -> bool {
        rgb.iter()
            .all(|channel| (-0.000_5..=1.000_5).contains(channel))
    }
    fn clamped(rgb: LinearRgb) -> LinearRgb {
        rgb.map(|channel| channel.clamp(0.0, 1.0))
    }

    let color = Oklch {
        lightness: color.lightness.clamp(0.0, 1.0),
        ..color
    };
    let rgb = unclipped(color);
    if displayable(rgb) {
        return clamped(rgb);
    }
    let mut inside = 0.0;
    let mut outside = color.chroma;
    for _ in 0..12 {
        let chroma = (inside + outside) * 0.5;
        if displayable(unclipped(Oklch { chroma, ..color })) {
            inside = chroma;
        } else {
            outside = chroma;
        }
    }
    clamped(unclipped(Oklch {
        chroma: inside,
        ..color
    }))
}

/// `color` moved by `delta` in Oklab lightness and scaled in chroma, keeping
/// hue and alpha. Used for highlights derived from a theme color.
pub(crate) fn relit(color: Rgba, delta: f32, chroma_scale: f32) -> Rgba {
    let authored = oklch(linear(color));
    rgba(
        in_gamut(Oklch {
            lightness: authored.lightness + delta,
            chroma: authored.chroma * chroma_scale,
            ..authored
        }),
        color.a,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeAppearance;

    fn light_themes() -> impl Iterator<Item = TermTheme> {
        TermTheme::CATALOG
            .into_iter()
            .filter(|theme| theme.appearance == ThemeAppearance::Light)
    }

    fn cell(fg: TermColor, bg: TermColor, style: TermStyle) -> GridCell {
        GridCell::new(u32::from('x'), fg, bg, style)
    }

    fn hue_of(color: Rgba) -> f32 {
        oklch(linear(color)).hue
    }

    /// Every program-chosen foreground a terminal can express compactly: the
    /// 256-color cube plus a truecolor lattice.
    fn program_foregrounds() -> Vec<TermColor> {
        let lattice = [0_u8, 64, 128, 192, 255];
        let mut colors: Vec<_> = (0..=255).map(TermColor::Ansi).collect();
        for red in lattice {
            for green in lattice {
                for blue in lattice {
                    colors.push(TermColor::Rgb(red, green, blue));
                }
            }
        }
        colors
    }

    #[test]
    fn oklab_round_trips_srgb() {
        for value in [0x000000, 0xffffff, 0xff0000, 0x00ff00, 0x0000ff, 0x9b6a18] {
            let color = Rgba {
                r: ((value >> 16) & 0xff) as f32 / 255.0,
                g: ((value >> 8) & 0xff) as f32 / 255.0,
                b: (value & 0xff) as f32 / 255.0,
                a: 1.0,
            };
            let back = rgba(in_gamut(oklch(linear(color))), 1.0);
            for (left, right) in [(color.r, back.r), (color.g, back.g), (color.b, back.b)] {
                assert!((left - right).abs() < 0.002, "{value:06x} round trip");
            }
        }
    }

    #[test]
    fn every_program_foreground_is_readable_on_every_light_theme() {
        for theme in light_themes() {
            for fg in program_foregrounds() {
                let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
                let contrast = contrast_ratio(resolved.foreground, resolved.background);
                assert!(
                    contrast >= MINIMUM_TEXT_CONTRAST - 0.01,
                    "{fg:?} on {} reads at {contrast}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn faint_text_stays_readable() {
        for theme in light_themes() {
            for fg in program_foregrounds() {
                let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::DIM));
                assert_eq!(resolved.foreground.a, 1.0, "faint paints opaque");
                let contrast = contrast_ratio(resolved.foreground, resolved.background);
                assert!(
                    contrast >= MINIMUM_DIM_CONTRAST - 0.01,
                    "faint {fg:?} on {} reads at {contrast}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn readable_colors_are_returned_untouched() {
        let theme = TermTheme::DIRIJOR_LIGHT;
        for fg in program_foregrounds() {
            let authored = theme.resolve_color(fg, false);
            if contrast_ratio(authored, theme.background) < MINIMUM_TEXT_CONTRAST {
                continue;
            }
            let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
            assert_eq!(resolved.foreground, authored, "{fg:?} was already readable");
        }
    }

    #[test]
    fn correction_keeps_a_color_recognizably_itself() {
        let theme = TermTheme::GITHUB_LIGHT;
        for (name, fg) in [
            ("neon green", TermColor::Rgb(0, 255, 0)),
            ("cyan", TermColor::Rgb(0, 255, 255)),
            ("hot pink", TermColor::Rgb(255, 105, 180)),
            ("orange", TermColor::Rgb(255, 165, 0)),
        ] {
            let authored = theme.resolve_color(fg, false);
            let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
            let drift = hue_delta(hue_of(authored), hue_of(resolved.foreground)).abs();
            assert!(
                drift <= ACCENT_REACH_DEGREES.to_radians() * HUE_PULL + 0.02,
                "{name} drifted {} degrees",
                drift.to_degrees()
            );
            assert!(
                oklch(linear(resolved.foreground)).chroma > CHROMATIC_CHROMA,
                "{name} lost its color"
            );
        }
    }

    #[test]
    fn heavily_corrected_colors_rotate_toward_the_theme_accent() {
        let theme = TermTheme::DIRIJOR_LIGHT;
        let fg = TermColor::Rgb(255, 255, 0);
        let accent = hue_of(theme.ansi[3]);
        let authored = theme.resolve_color(fg, false);
        let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));

        let before = hue_delta(hue_of(authored), accent).abs();
        let after = hue_delta(hue_of(resolved.foreground), accent).abs();
        assert!(
            after < before * 0.5,
            "yellow should land near the theme's yellow: {} -> {} degrees away",
            before.to_degrees(),
            after.to_degrees()
        );
    }

    #[test]
    fn grays_keep_their_order_of_emphasis() {
        for theme in light_themes() {
            let lightness = |value: u8| {
                let fg = TermColor::Rgb(value, value, value);
                let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
                oklch(linear(resolved.foreground)).lightness
            };
            let white = lightness(255);
            let silver = lightness(200);
            let gray = lightness(150);
            assert!(
                white < silver && silver < gray,
                "{}: white {white}, silver {silver}, gray {gray}",
                theme.id
            );
        }
    }

    #[test]
    fn faint_white_recedes_like_faint_default_text() {
        for theme in light_themes() {
            let lightness = |fg, style| {
                let resolved = theme.resolve_cell(cell(fg, TermColor::Default, style));
                oklch(linear(resolved.foreground)).lightness
            };
            let faint_white = lightness(TermColor::Rgb(255, 255, 255), TermStyle::DIM);
            let white = lightness(TermColor::Rgb(255, 255, 255), TermStyle::empty());
            let faint_default = lightness(TermColor::Default, TermStyle::DIM);
            assert!(faint_white > white, "{}: faint must recede", theme.id);
            // Where the theme's own faint text clears the floor, faint white
            // joins it; where it does not, the floor decides.
            let faded = faint::faded(&theme, theme.foreground, theme.background);
            if contrast_ratio(faded, theme.background) >= MINIMUM_DIM_CONTRAST {
                assert!(
                    (faint_white - faint_default).abs() < 0.05,
                    "{}: faint white {faint_white} vs faint default {faint_default}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn program_authored_pairs_are_left_alone() {
        let theme = TermTheme::DIRIJOR_LIGHT;
        for (fg, bg) in [
            (TermColor::Rgb(90, 90, 90), TermColor::Rgb(64, 64, 64)),
            (TermColor::Ansi(240), TermColor::Ansi(236)),
            (TermColor::Rgb(250, 250, 250), TermColor::Ansi(255)),
        ] {
            assert!(!applies(cell(fg, bg, TermStyle::empty())));
            let resolved = theme.resolve_cell(cell(fg, bg, TermStyle::empty()));
            assert_eq!(resolved.foreground, theme.resolve_color(fg, false));
        }
    }

    #[test]
    fn theme_supplied_text_is_corrected_on_a_program_background() {
        // A status bar that sets only a dark background expects light text.
        let theme = TermTheme::DIRIJOR_LIGHT;
        let resolved = theme.resolve_cell(cell(
            TermColor::Default,
            TermColor::Rgb(40, 40, 48),
            TermStyle::empty(),
        ));
        assert!(contrast_ratio(resolved.foreground, resolved.background) >= 4.49);
    }

    #[test]
    fn selected_rows_on_a_palette_background_are_corrected() {
        for theme in light_themes() {
            for fg in [TermColor::Default, TermColor::Ansi(4), TermColor::Ansi(6)] {
                let resolved = theme.resolve_cell(cell(fg, TermColor::Ansi(4), TermStyle::empty()));
                let contrast = contrast_ratio(resolved.foreground, resolved.background);
                let ceiling = contrast_ratio(theme.ansi[4], Rgba::default())
                    .max(contrast_ratio(theme.ansi[4], gpui::white().into()));
                assert!(
                    contrast >= MINIMUM_TEXT_CONTRAST.min(ceiling) - 0.01,
                    "{fg:?} on {}'s blue reads at {contrast}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn inverse_video_is_judged_on_the_colors_it_paints() {
        let theme = TermTheme::DIRIJOR_LIGHT;
        let resolved = theme.resolve_cell(cell(
            TermColor::Rgb(255, 255, 160),
            TermColor::Default,
            TermStyle::INVERSE,
        ));
        assert_eq!(
            resolved.background,
            theme.resolve_color(TermColor::Rgb(255, 255, 160), false)
        );
        assert!(contrast_ratio(resolved.foreground, resolved.background) >= 4.49);
    }

    #[test]
    fn powerline_separators_keep_the_neighboring_background() {
        let theme = TermTheme::DIRIJOR_LIGHT;
        let fg = TermColor::Rgb(255, 230, 120);
        let separator = GridCell::new(0xe0b0, fg, TermColor::Default, TermStyle::empty());
        assert_eq!(
            theme.resolve_cell(separator).foreground,
            theme.resolve_color(fg, false)
        );
    }

    #[test]
    fn dark_themes_are_untouched() {
        for theme in TermTheme::CATALOG
            .into_iter()
            .filter(|theme| theme.appearance == ThemeAppearance::Dark)
        {
            for fg in [TermColor::Rgb(32, 32, 32), TermColor::Ansi(0)] {
                let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
                assert_eq!(resolved.foreground, theme.resolve_color(fg, false));
            }
        }
    }

    #[test]
    fn cached_answers_match_fresh_ones_across_themes() {
        let fg = TermColor::Rgb(255, 255, 0);
        let probe = cell(fg, TermColor::Default, TermStyle::empty());
        for _ in 0..2 {
            for theme in light_themes() {
                let fresh = correct(
                    &theme,
                    theme.resolve_color(fg, false),
                    theme.background,
                    true,
                    false,
                );
                assert_eq!(theme.resolve_cell(probe).foreground, fresh, "{}", theme.id);
            }
        }
    }

    #[test]
    fn a_theme_in_transit_never_answers_from_another_palette() {
        // Two frames of one fade: the same id and, because the endpoints
        // agree on them, the same default colors, under different accents.
        let to = TermTheme::DIRIJOR_LIGHT;
        let mut from = to;
        from.ansi[3] = to.ansi[5];
        let (early, late) = (from.mix(&to, 0.2), from.mix(&to, 0.8));
        assert_eq!(
            (early.id, early.background, early.foreground),
            (late.id, late.background, late.foreground)
        );
        assert_ne!(theme_key(&early), theme_key(&late));

        // A saturated yellow travels far enough to be pulled onto the accent.
        let yellow = cell(
            TermColor::Rgb(255, 255, 0),
            TermColor::Default,
            TermStyle::empty(),
        );
        let first = early.resolve_cell(yellow).foreground;
        assert_ne!(late.resolve_cell(yellow).foreground, first);
        assert_eq!(early.resolve_cell(yellow).foreground, first);
    }

    #[test]
    fn a_flooded_cache_still_answers_correctly() {
        let theme = TermTheme::GITHUB_LIGHT;
        let gradient = || {
            (0..=255_u8)
                .flat_map(|red| (0..=255_u8).step_by(5).map(move |green| (red, green)))
                .map(|(red, green)| TermColor::Rgb(red, green, 255))
        };
        assert!(gradient().count() > CACHE_SETS * CACHE_WAYS);
        for fg in gradient() {
            let _ = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
        }
        for fg in gradient().step_by(97) {
            let fresh = correct(
                &theme,
                theme.resolve_color(fg, false),
                theme.background,
                true,
                false,
            );
            let resolved = theme.resolve_cell(cell(fg, TermColor::Default, TermStyle::empty()));
            assert_eq!(resolved.foreground, fresh, "{fg:?}");
        }
    }
}
