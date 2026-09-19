//! Selection and find highlights derived from a theme's own colors.
//!
//! A highlight is a translucent overlay painted between cell backgrounds and
//! glyphs. One fixed yellow at fixed alphas lands at a different perceived
//! strength on every background and ignores whether the text beneath it
//! stays readable. Here each tint is instead defined by where its composite
//! over the theme background sits in Oklab: a fixed lightness step toward the
//! foreground plus a fixed chroma offset, so every theme gets the same
//! hierarchy, and the step shrinks only when the theme's default text would
//! stop being readable on top of it.
//!
//! The catalog in `theme.rs` stays `const`: it holds the derived values as
//! `0xRRGGBBAA` literals, and `catalog_literals_match_the_derivation` fails
//! with the exact lines to paste whenever a palette or a constant changes.
//! Eight bits per channel is what the literals carry, so [`derive`] returns
//! colors already rounded to that grid.

use std::ops::RangeInclusive;

use gpui::Rgba;

use crate::contrast::{
    ACCENT_REACH_DEGREES, MINIMUM_TEXT_CONTRAST, NEUTRAL_CHROMA, Oklch, composite, contrast_ratio,
    hue_delta, in_gamut, linear, oklch, rgba,
};

/// Oklab lightness the selection composite moves from the background. A
/// selection covers whole lines, so a calm step is enough to read as a band.
const SELECTION_STEP: f32 = 0.10;
/// Step for find matches other than the current one. The same lightness as a
/// selection; a match is a few cells wide and is told apart by its chroma.
const FIND_STEP: f32 = 0.10;
/// Step for the current find match: a little over twice a plain match, so
/// "which one am I on" reads before the hue does. It is also the largest
/// step default text survives at 4.5:1 on a theme with 9:1 body text.
const CURRENT_STEP: f32 = 0.22;

/// Steps a low-contrast theme is never squeezed below. Under these a tint
/// stops reading as a highlight at all, which is worse than soft text.
const MINIMUM_SELECTION_STEP: f32 = 0.05;
const MINIMUM_FIND_STEP: f32 = 0.04;
const MINIMUM_CURRENT_STEP: f32 = 0.09;

/// Chroma the find tints add to the background's own, as an Oklab offset
/// toward the theme's yellow. Dark composites reach less: a translucent
/// overlay cannot pull a channel below the background's share of it.
const FIND_CHROMA: f32 = 0.09;
const CURRENT_CHROMA: f32 = 0.15;

/// The densest each overlay may be. The composite over the theme background
/// is fixed by the steps above; opacity decides what happens over every other
/// cell background. A sheer layer of an extreme ink moves any cell by a
/// bounded amount, so a match on an inverse-video status bar keeps the bar's
/// own text readable. A dense layer of a moderate ink repaints the cell,
/// which a selection needs: it has no chroma to show itself with, and a
/// sheer gray disappears on most palette backgrounds.
const SELECTION_ALPHA: f32 = 0.5;
const FIND_ALPHA: f32 = 0.4;
const CURRENT_ALPHA: f32 = 0.7;
/// Below this an overlay disappears on cells that are already near its ink.
const SHEEREST_ALPHA: f32 = 0.2;
const ALPHA_STEP: f32 = 0.05;
/// An overlay is made as sheer as it can be while it still carries this
/// share of the chroma its densest form would. Lightening needs little
/// opacity; taking blue out of a light background to make yellow needs more.
const CHROMA_SHARE: f32 = 0.85;

/// Headroom over the contrast floor, so rounding to 8-bit literals cannot
/// land a composite just under it.
const FLOOR_MARGIN: f32 = 0.03;

/// Hues, in degrees, between which a tint of this lightness reads as yellow.
/// Palette yellows are text colors: a light theme darkens its yellow toward
/// orange to keep it legible, and a pale tint of that hue is peach; a dark
/// tint of a lemon yellow is olive.
const DARK_TINT_HUES: (f32, f32) = (70.0, 95.0);
const LIGHT_TINT_HUES: (f32, f32) = (88.0, 100.0);

/// The published selection color of each palette theme. Its cast is the
/// theme's voice and is kept; how far toward it a selection went, set by the
/// alpha it was painted at, is what made selections range from faint to loud
/// and is replaced by [`SELECTION_STEP`]. Only the golden test reads these:
/// the catalog holds what they derive to.
#[cfg(test)]
const SELECTION_SEEDS: [(&str, u32); 15] = [
    ("solarized-dark", 0x586e75),
    ("dracula", 0x44475a),
    ("one-dark", 0x3e4451),
    ("gruvbox-dark", 0x504945),
    ("tokyo-night", 0x33467c),
    ("catppuccin-mocha", 0x585b70),
    ("nord", 0x4c566a),
    ("rose-pine", 0x403d52),
    ("kanagawa-wave", 0x2d4f67),
    ("everforest-dark", 0x475258),
    ("dirijor-light", 0xb8d1e8),
    ("solarized-light", 0xeee8d5),
    ("github-light", 0xbddfff),
    ("gruvbox-light", 0xd5c4a1),
    ("catppuccin-latte", 0xacb0be),
];

/// The three interaction overlays of a theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tints {
    pub selection: Rgba,
    pub find_match: Rgba,
    pub find_match_current: Rgba,
}

/// Overlays for a palette, solved against its default background and text.
///
/// Priority when a theme cannot have everything: displayable colors first,
/// then the minimum steps and their order, then the contrast floor. The
/// floor is 4.5:1 for default text over each composite, or the theme's own
/// body contrast when that is lower. Only a theme whose body text is already
/// near 4.5:1 (Solarized) gives any of it up, and then by the minimum steps
/// and no more.
#[must_use]
pub fn derive(
    background: Rgba,
    foreground: Rgba,
    selection_seed: Rgba,
    ansi: &[Rgba; 16],
) -> Tints {
    let paper = Paper::new(background, foreground);

    let find_hue = paper.find_hue(ansi);
    let current = paper.solve(
        CURRENT_STEP,
        MINIMUM_CURRENT_STEP,
        SHEEREST_ALPHA..=CURRENT_ALPHA,
        &|_| (CURRENT_CHROMA, find_hue),
    );
    // A plain match keeps its proportion to the current one when that one had
    // to shrink, so the pair never closes up.
    let find_step = (current.step * FIND_STEP / CURRENT_STEP).max(MINIMUM_FIND_STEP);
    let find = paper.solve(find_step, find_step, SHEEREST_ALPHA..=FIND_ALPHA, &|_| {
        (FIND_CHROMA, find_hue)
    });

    // The selection walks from the background toward the theme's published
    // selection color and stops at the step, so it takes on that color's
    // cast in proportion and never more of it than the theme itself shows.
    let seed = oklch(linear(selection_seed));
    let seed_a = seed.chroma * seed.hue.cos() - paper.a;
    let seed_b = seed.chroma * seed.hue.sin() - paper.b;
    let seed_travel = (seed.lightness - paper.lightness).abs().max(f32::EPSILON);
    let selection = paper.solve(
        SELECTION_STEP,
        MINIMUM_SELECTION_STEP,
        SELECTION_ALPHA..=SELECTION_ALPHA,
        &|step| {
            let along = (step / seed_travel).min(1.0);
            let (a, b) = (seed_a * along, seed_b * along);
            (a.hypot(b), b.atan2(a))
        },
    );

    Tints {
        selection: selection.overlay,
        find_match: find.overlay,
        find_match_current: current.overlay,
    }
}

struct Solved {
    overlay: Rgba,
    step: f32,
}

struct Paper {
    background: Rgba,
    foreground: Rgba,
    lightness: f32,
    a: f32,
    b: f32,
    /// +1 when text is lighter than the background, -1 when darker.
    toward_text: f32,
    floor: f32,
}

impl Paper {
    fn new(background: Rgba, foreground: Rgba) -> Self {
        let paper = oklch(linear(background));
        let ink = oklch(linear(foreground));
        Self {
            background,
            foreground,
            lightness: paper.lightness,
            a: paper.chroma * paper.hue.cos(),
            b: paper.chroma * paper.hue.sin(),
            toward_text: if ink.lightness >= paper.lightness {
                1.0
            } else {
                -1.0
            },
            floor: contrast_ratio(foreground, background).min(MINIMUM_TEXT_CONTRAST),
        }
    }

    /// The theme's yellow, from its palette when the slot still holds one,
    /// held inside the band that reads as yellow at a tint's lightness.
    fn find_hue(&self, ansi: &[Rgba; 16]) -> f32 {
        use std::f32::consts::TAU;
        let yellow = oklch([1.0, 1.0, 0.0]).hue;
        let hue = [3, 11]
            .into_iter()
            .map(|slot| oklch(linear(ansi[slot])))
            .find(|accent| {
                accent.chroma >= NEUTRAL_CHROMA
                    && hue_delta(yellow, accent.hue).abs() <= ACCENT_REACH_DEGREES.to_radians()
            })
            .map_or(yellow, |accent| accent.hue);
        let (from, to) = if self.toward_text > 0.0 {
            DARK_TINT_HUES
        } else {
            LIGHT_TINT_HUES
        };
        hue.rem_euclid(TAU)
            .clamp(from.to_radians(), to.to_radians())
    }

    /// The largest step up to `step` whose composite keeps default text at
    /// the floor, but never less than `minimum`.
    fn solve(
        &self,
        step: f32,
        minimum: f32,
        alpha: RangeInclusive<f32>,
        tone: &dyn Fn(f32) -> (f32, f32),
    ) -> Solved {
        let reads = |step: f32| {
            let overlay = self.overlay(step, tone(step), &alpha);
            contrast_ratio(self.foreground, composite(overlay, self.background))
                >= self.floor + FLOOR_MARGIN
        };
        let mut solved = step;
        if !reads(step) {
            let mut readable = 0.0;
            let mut unreadable = step;
            for _ in 0..20 {
                let middle = (readable + unreadable) * 0.5;
                if reads(middle) {
                    readable = middle;
                } else {
                    unreadable = middle;
                }
            }
            solved = readable.max(minimum);
        }
        Solved {
            overlay: self.overlay(solved, tone(solved), &alpha),
            step: solved,
        }
    }

    /// The overlay whose composite over the background sits `step` away in
    /// lightness with as much of `chroma` as a translucent layer can carry.
    fn overlay(&self, step: f32, (chroma, hue): (f32, f32), alpha: &RangeInclusive<f32>) -> Rgba {
        let carried = |alpha: f32| {
            let ink = |chroma: f32| self.ink(self.target(step, chroma, hue), alpha);
            if ink(chroma).is_some() {
                return Some(chroma);
            }
            ink(0.0)?;
            let mut inside = 0.0;
            let mut outside = chroma;
            for _ in 0..16 {
                let middle = (inside + outside) * 0.5;
                if ink(middle).is_some() {
                    inside = middle;
                } else {
                    outside = middle;
                }
            }
            Some(inside)
        };

        // Even a gray this far from the background may need a denser layer
        // than the role asks for.
        let mut densest = *alpha.end();
        let most = loop {
            match carried(densest) {
                Some(chroma) => break chroma,
                None if densest >= 1.0 => break 0.0,
                None => densest = (densest + ALPHA_STEP).min(1.0),
            }
        };
        let mut alpha = alpha.start().min(densest);
        let chroma = loop {
            match carried(alpha) {
                Some(chroma) if chroma >= most * CHROMA_SHARE => break chroma,
                _ if alpha + ALPHA_STEP > densest => {
                    alpha = densest;
                    break most;
                }
                _ => alpha += ALPHA_STEP,
            }
        };
        let target = self.target(step, chroma, hue);
        quantized(self.ink(target, alpha).unwrap_or(Rgba { a: 1.0, ..target }))
    }

    /// `chroma` toward `hue` is an offset from the background's own a/b, so
    /// the perceived distance from the background is the same on tinted
    /// paper as on gray.
    fn target(&self, step: f32, chroma: f32, hue: f32) -> Rgba {
        let a = self.a + chroma * hue.cos();
        let b = self.b + chroma * hue.sin();
        rgba(
            in_gamut(Oklch {
                lightness: self.lightness + self.toward_text * step,
                chroma: a.hypot(b),
                hue: b.atan2(a),
            }),
            1.0,
        )
    }

    /// The color that composites to `target` at `alpha`, if it is a color.
    /// GPUI blends in gamma space, as `composite` does.
    fn ink(&self, target: Rgba, alpha: f32) -> Option<Rgba> {
        let solve = |target: f32, paper: f32| paper + (target - paper) / alpha;
        let ink = [
            solve(target.r, self.background.r),
            solve(target.g, self.background.g),
            solve(target.b, self.background.b),
        ];
        ink.iter()
            .all(|channel| (-0.000_5..=1.000_5).contains(channel))
            .then(|| Rgba {
                r: ink[0].clamp(0.0, 1.0),
                g: ink[1].clamp(0.0, 1.0),
                b: ink[2].clamp(0.0, 1.0),
                a: alpha,
            })
    }
}

fn quantized(color: Rgba) -> Rgba {
    let round = |channel: f32| (channel * 255.0).round() / 255.0;
    Rgba {
        r: round(color.r),
        g: round(color.g),
        b: round(color.b),
        a: round(color.a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::TermTheme;

    /// Oklab distance below which two flat areas stop reading as different.
    const DISTINCT: f32 = 0.04;
    /// The current match must stand out from the background at least this
    /// many times as far as a plain match does.
    const CURRENT_TO_FIND: f32 = 1.4;
    /// What a theme with sub-4.5 body text may lose to the minimum steps.
    const LOW_CONTRAST_SHARE: f32 = 0.7;

    fn lab(color: Rgba) -> [f32; 3] {
        let color = oklch(linear(color));
        [
            color.lightness,
            color.chroma * color.hue.cos(),
            color.chroma * color.hue.sin(),
        ]
    }

    fn distance(left: Rgba, right: Rgba) -> f32 {
        let (left, right) = (lab(left), lab(right));
        (0..3)
            .map(|axis| (left[axis] - right[axis]).powi(2))
            .sum::<f32>()
            .sqrt()
    }

    fn seed(id: &str) -> Option<Rgba> {
        SELECTION_SEEDS
            .iter()
            .find(|(seeded, _)| *seeded == id)
            .map(|&(_, value)| crate::theme::hex(value))
    }

    fn literal(color: Rgba) -> String {
        let byte = |channel: f32| (channel * 255.0).round() as u32;
        format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            byte(color.r),
            byte(color.g),
            byte(color.b),
            byte(color.a)
        )
    }

    #[test]
    fn catalog_literals_match_the_derivation() {
        let mut stale = false;
        let mut expected = String::new();
        for theme in TermTheme::CATALOG {
            let Some(seed) = seed(theme.id) else {
                continue;
            };
            let tints = derive(theme.background, theme.foreground, seed, &theme.ansi);
            let pairs = [
                (theme.selection, tints.selection),
                (theme.find_match, tints.find_match),
                (theme.find_match_current, tints.find_match_current),
            ];
            // One step of the 8-bit grid absorbs libm differences between
            // platforms; anything further is a stale literal.
            stale |= pairs.iter().any(|(actual, derived)| {
                [
                    actual.r - derived.r,
                    actual.g - derived.g,
                    actual.b - derived.b,
                    actual.a - derived.a,
                ]
                .iter()
                .any(|delta| delta.abs() > 1.01 / 255.0)
            });
            expected.push_str(&format!(
                "{:<18} [{}, {}, {}],\n",
                theme.id,
                literal(tints.selection),
                literal(tints.find_match),
                literal(tints.find_match_current)
            ));
        }
        assert!(
            !stale,
            "theme.rs tint literals are stale; paste these:\n{expected}"
        );
    }

    #[test]
    fn every_palette_theme_has_a_selection_seed() {
        let hand_authored = ["dirijor-dark", "vesper"];
        for theme in TermTheme::CATALOG {
            assert_eq!(
                seed(theme.id).is_none(),
                hand_authored.contains(&theme.id),
                "{}",
                theme.id
            );
        }
    }

    #[test]
    fn default_text_stays_readable_over_every_tint() {
        for theme in TermTheme::CATALOG {
            let own = contrast_ratio(theme.foreground, theme.background);
            let floor = if own < MINIMUM_TEXT_CONTRAST + 0.5 {
                // Body text this close to the floor leaves no room for a
                // visible step; see the priority order on `derive`.
                own * LOW_CONTRAST_SHARE
            } else {
                MINIMUM_TEXT_CONTRAST
            };
            for tint in [theme.selection, theme.find_match, theme.find_match_current] {
                let over = contrast_ratio(theme.foreground, composite(tint, theme.background));
                assert!(over >= floor, "{}: {over} < {floor}", theme.id);
            }
        }
    }

    #[test]
    fn only_low_contrast_themes_give_up_any_contrast_floor() {
        let relaxed = TermTheme::CATALOG
            .iter()
            .filter(|theme| {
                contrast_ratio(theme.foreground, theme.background) < MINIMUM_TEXT_CONTRAST + 0.5
            })
            .map(|theme| theme.id)
            .collect::<Vec<_>>();
        assert_eq!(relaxed, ["solarized-dark", "solarized-light"]);
    }

    #[test]
    fn every_tint_is_distinguishable_from_the_bare_background() {
        for theme in TermTheme::CATALOG {
            for tint in [theme.selection, theme.find_match, theme.find_match_current] {
                let over = composite(tint, theme.background);
                assert!(
                    distance(over, theme.background) >= DISTINCT,
                    "{}: {}",
                    theme.id,
                    distance(over, theme.background)
                );
            }
        }
    }

    #[test]
    fn the_hierarchy_is_ordered_including_where_tints_overlap() {
        for theme in TermTheme::CATALOG {
            let paper = theme.background;
            let selection = composite(theme.selection, paper);
            let find = composite(theme.find_match, paper);
            let current = composite(theme.find_match_current, paper);
            assert!(
                distance(current, paper) >= distance(find, paper) * CURRENT_TO_FIND,
                "{}: current {} vs find {}",
                theme.id,
                distance(current, paper),
                distance(find, paper)
            );
            // Find overlays are painted over the selection.
            let selected_find = composite(theme.find_match, selection);
            let selected_current = composite(theme.find_match_current, selection);
            for (what, left, right) in [
                ("find vs current", find, current),
                ("selection vs find", selection, find),
                ("selection vs current", selection, current),
                ("find inside selection", selected_find, selection),
                ("current inside selection", selected_current, selection),
                ("matches inside selection", selected_find, selected_current),
            ] {
                assert!(
                    distance(left, right) >= DISTINCT,
                    "{}: {what} {}",
                    theme.id,
                    distance(left, right)
                );
            }
        }
    }

    #[test]
    fn tints_are_displayable_translucent_colors() {
        for theme in TermTheme::CATALOG {
            for tint in [theme.selection, theme.find_match, theme.find_match_current] {
                for channel in [tint.r, tint.g, tint.b] {
                    assert!((0.0..=1.0).contains(&channel), "{}", theme.id);
                }
                assert!((0.19..=0.71).contains(&tint.a), "{}: {}", theme.id, tint.a);
            }
        }
    }

    #[test]
    fn themes_with_room_share_one_ladder() {
        for theme in TermTheme::CATALOG {
            if seed(theme.id).is_none() || contrast_ratio(theme.foreground, theme.background) < 10.0
            {
                continue;
            }
            let base = lab(theme.background)[0];
            for (tint, step) in [
                (theme.selection, SELECTION_STEP),
                (theme.find_match, FIND_STEP),
                (theme.find_match_current, CURRENT_STEP),
            ] {
                let moved = (lab(composite(tint, theme.background))[0] - base).abs();
                assert!((moved - step).abs() < 0.01, "{}: {moved}", theme.id);
            }
        }
    }

    #[test]
    fn tints_stay_visible_over_most_palette_backgrounds() {
        // Derived themes only: Vesper's published selection is a quarter-opaque
        // white, which its mostly pastel palette swallows by design.
        for theme in TermTheme::CATALOG
            .iter()
            .filter(|theme| seed(theme.id).is_some())
        {
            for tint in [theme.selection, theme.find_match, theme.find_match_current] {
                let visible = theme
                    .ansi
                    .iter()
                    .filter(|&&cell| distance(composite(tint, cell), cell) >= DISTINCT)
                    .count();
                // A cell painted in the overlay's own color hides it; that
                // is inherent to a translucent layer.
                assert!(visible >= 10, "{}: {visible}/16", theme.id);
            }
        }
    }

    #[test]
    fn a_repurposed_yellow_slot_falls_back_to_yellow() {
        let theme = TermTheme::NORD;
        let mut ansi = theme.ansi;
        ansi[3] = theme.ansi[4];
        ansi[11] = theme.ansi[8];
        let paper = Paper::new(theme.background, theme.foreground);
        let hue = paper.find_hue(&ansi).to_degrees();
        assert!(
            (DARK_TINT_HUES.0..=DARK_TINT_HUES.1).contains(&hue),
            "{hue}"
        );
    }
}
