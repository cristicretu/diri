//! How far a terminal pane's content recedes, from two facts: whether its
//! window is key, and whether it is the pane typing goes to.
//!
//! The strengths are one table per appearance, so a design pass edits
//! numbers, not layout code. A pane recedes by laying its own surface colour
//! over its content at `1 - opacity`: the background stays put and the text,
//! cursor and images fade toward it.
//!
//! The values are read during render from `Window::is_window_active` and the
//! focused pane. Every view that paints a scrim already re-renders when the
//! window's key status changes (GPUI refreshes the window), and pane focus
//! changes re-render the workbench, so the fade costs no extra frames and no
//! state while nothing changes. It switches instantly, so there is no motion
//! to reduce.
use diri_ui::{Appearance, SemanticColors};
use gpui::{Div, Hsla, InteractiveElement, ParentElement, Styled, div, px};

/// Content opacity, indexed `[window key][pane active]`.
///
/// * Key window: the active pane is at full strength; the others recede
///   enough to show where typing goes, and stay readable for watching an
///   agent work beside it.
/// * Non-key window: every pane recedes, so a glance at the screen tells which
///   window has the keyboard. The active pane keeps its lead over its
///   neighbours, so the split's focus survives the trip to another app.
type Table = [[f32; 2]; 2];

const DARK: Table = [
    // Window not key: [inactive pane, active pane].
    [0.52, 0.8],
    // Window key.
    [0.72, 1.0],
];

/// Fading dark ink toward a light page loses perceived lightness faster than
/// fading light ink toward a dark one, so light themes lay a thinner scrim.
/// Each value gives the same CIELAB L* step as its dark counterpart on the
/// default themes' ink and paper (within 0.01).
const LIGHT: Table = [[0.6, 0.85], [0.78, 1.0]];

/// How opaque a pane's content is.
pub(crate) fn content_opacity(appearance: Appearance, window_key: bool, pane_active: bool) -> f32 {
    let table = match appearance {
        Appearance::Dark => &DARK,
        Appearance::Light => &LIGHT,
    };
    table[usize::from(window_key)][usize::from(pane_active)]
}

/// The scrim colour that paints content at `opacity` over `surface`, or
/// `None` at full strength so a focused pane in a key window paints nothing.
pub(crate) fn scrim_color(surface: impl Into<Hsla>, opacity: f32) -> Option<Hsla> {
    (opacity < 1.0).then(|| {
        let mut color: Hsla = surface.into();
        color.a = 1.0 - opacity;
        color
    })
}

/// A full-size overlay that fades a pane, for a `relative` pane container;
/// the caller rounds it to the pane's corners. Absolute and listener-free,
/// so it never takes a click or a hover.
pub(crate) fn scrim(colors: SemanticColors, window_key: bool, pane_active: bool) -> Option<Div> {
    let opacity = content_opacity(colors.appearance, window_key, pane_active);
    let color = scrim_color(colors.terminal_surface(), opacity)?;
    Some(
        div()
            .debug_selector(move || {
                format!(
                    "pane-fade:{}-{}",
                    if window_key { "key" } else { "nonkey" },
                    if pane_active { "active" } else { "inactive" },
                )
            })
            .absolute()
            .inset_0()
            .bg(color),
    )
}

/// Adds [`scrim`] as the last child of `container`, rounded to `radius`,
/// when the pane recedes.
pub(crate) fn faded<E: ParentElement>(
    container: E,
    colors: SemanticColors,
    window_key: bool,
    pane_active: bool,
    radius: Option<f32>,
) -> E {
    match scrim(colors, window_key, pane_active) {
        Some(scrim) => container.child(match radius {
            Some(radius) => scrim.rounded(px(radius)),
            None => scrim,
        }),
        None => container,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;

    const APPEARANCES: [Appearance; 2] = [Appearance::Dark, Appearance::Light];

    #[test]
    fn only_the_active_pane_of_a_key_window_is_at_full_strength() {
        for appearance in APPEARANCES {
            assert_eq!(content_opacity(appearance, true, true), 1.0);
            for (key, active) in [(true, false), (false, true), (false, false)] {
                let opacity = content_opacity(appearance, key, active);
                assert!(opacity < 1.0, "{appearance:?} {key} {active}");
                assert!(scrim_color(rgb(0x101010), opacity).is_some());
            }
        }
        assert!(scrim_color(rgb(0x101010), 1.0).is_none());
    }

    #[test]
    fn focus_order_holds_inside_and_across_windows() {
        for appearance in APPEARANCES {
            let o = |key, active| content_opacity(appearance, key, active);
            // The active pane leads its neighbours in both windows, and
            // leaving the window fades both by a step.
            assert!(o(true, true) > o(true, false));
            assert!(o(false, true) > o(false, false));
            assert!(o(true, true) > o(false, true));
            assert!(o(true, false) > o(false, false));
            // The active pane of a background window still reads stronger
            // than a neighbour in the key window.
            assert!(o(false, true) > o(true, false));
        }
    }

    /// CIELAB L* of an sRGB colour.
    fn lightness(rgb: [f32; 3]) -> f32 {
        let linear = |v: f32| {
            let v = v / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let y = 0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2]);
        if y > 216.0 / 24389.0 {
            116.0 * y.cbrt() - 16.0
        } else {
            y * 24389.0 / 27.0
        }
    }

    /// The share of the ink-to-paper lightness step left after the fade.
    fn lightness_kept(paper: [f32; 3], ink: [f32; 3], opacity: f32) -> f32 {
        let faded = [0, 1, 2].map(|i| paper[i] + opacity * (ink[i] - paper[i]));
        (lightness(faded) - lightness(paper)) / (lightness(ink) - lightness(paper))
    }

    /// Light and dark themes recede by the same perceived amount. Ink and
    /// paper are the default dark and light themes' as rendered.
    #[test]
    fn light_and_dark_themes_recede_by_the_same_perceived_step() {
        let dark = ([18.0, 19.0, 24.0], [229.0, 229.0, 229.0]);
        let light = ([247.0, 245.0, 240.0], [37.0, 34.0, 29.0]);
        for (key, active) in [(true, false), (false, true), (false, false)] {
            let dark = lightness_kept(
                dark.0,
                dark.1,
                content_opacity(Appearance::Dark, key, active),
            );
            let light = lightness_kept(
                light.0,
                light.1,
                content_opacity(Appearance::Light, key, active),
            );
            assert!(
                (dark - light).abs() < 0.015,
                "{key} {active}: dark keeps {dark}, light keeps {light}"
            );
        }
    }

    #[test]
    fn the_scrim_is_the_surface_at_the_missing_opacity() {
        let surface: Hsla = rgb(0xfafafa).into();
        let scrim = scrim_color(surface, 0.72).unwrap();
        assert_eq!((scrim.h, scrim.s, scrim.l), (surface.h, surface.s, surface.l));
        assert!((scrim.a - 0.28).abs() < 1e-6);
    }
}
