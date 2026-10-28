//! How the window's chrome reads when the window does not have the keyboard:
//! the sidebar's selected row, the horizontal strip's tab pill and the title
//! row. Terminal panes recede separately (`pane_fade`).
//!
//! A key window keeps the shipped look exactly. A window that is not key
//! steps its chrome back the way a native window does: the selection loses
//! its lift, hover stops lighting rows, and the title row's text and glyphs
//! fade. How far is one table per interface style and density, so a design
//! pass edits numbers, not view code.
//!
//! Cost: nothing while key status holds. GPUI refreshes the whole window when
//! it becomes or stops being key (`Window::refresh` in the activation
//! handler), which re-renders every cached view, the sidebar and its rows
//! included, on `main` already. The look is read during that render from
//! `Window::is_window_active`, so it adds no render, timer, subscription or
//! state, and a key change paints the same number of frames as before.
use diri_ui::{Appearance, Glass, SemanticColors};
use gpui::{Rgba, Styled, Window};

use crate::store::{InterfaceStyle, PaneDensity, SessionStore};

/// What a selected pill looks like.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Pill {
    /// The shipped lifted glass pill: fill plus hairline.
    Lifted,
    /// The lifted pill with its fill and hairline alphas scaled down; it
    /// still reads as a raised surface, only quieter.
    Dimmed { fill: f32, stroke: f32 },
    /// No lift at all: a flat wash of the text color at this alpha, the way a
    /// native inactive list selection turns gray.
    Flat(f32),
}

/// The chrome's look for one window state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChromeLook {
    pub(crate) key: bool,
    pub(crate) pill: Pill,
    /// Whether pointer hover lights rows and tabs.
    pub(crate) hover: bool,
    /// Opacity of the title row's text and glyphs.
    pub(crate) title: f32,
}

impl ChromeLook {
    /// A key window: the shipped look.
    pub(crate) const KEY: Self = Self {
        key: true,
        pill: Pill::Lifted,
        hover: true,
        title: 1.0,
    };

    /// The selected pill's fill.
    pub(crate) fn pill_fill(self, colors: SemanticColors) -> Rgba {
        let lifted = Glass::fill(colors);
        match self.pill {
            Pill::Lifted => lifted,
            Pill::Dimmed { fill, .. } => lifted.alpha(lifted.a * fill),
            Pill::Flat(alpha) => colors.primary.alpha(alpha),
        }
    }

    /// The selected pill's hairline.
    pub(crate) fn pill_stroke(self, colors: SemanticColors) -> Rgba {
        let lifted = Glass::stroke(colors);
        match self.pill {
            Pill::Lifted => lifted,
            Pill::Dimmed { stroke, .. } => lifted.alpha(lifted.a * stroke),
            Pill::Flat(_) => lifted.alpha(0.0),
        }
    }

    /// `color` where hover lights a row, clear where it does not.
    pub(crate) fn hover(self, color: Rgba) -> Rgba {
        if self.hover { color } else { color.alpha(0.0) }
    }
}

/// The look for `key`, under a style, density and appearance.
pub(crate) const fn look(
    style: InterfaceStyle,
    density: PaneDensity,
    appearance: Appearance,
    key: bool,
) -> ChromeLook {
    if key {
        return ChromeLook::KEY;
    }
    let compact = matches!(density, PaneDensity::Compact);
    let dark = matches!(appearance, Appearance::Dark);
    let pill = match style {
        // Glass styles keep a quieter lift: the pill is a pane of the same
        // glass, so it stays glass. Compact chrome has no islands framing
        // it, so it steps back further (the reference tables drop the tab
        // container wash from 5 % to 3 % comfortable and 2 % compact).
        InterfaceStyle::Modern | InterfaceStyle::Blended => {
            let scale = if compact { 0.4 } else { 0.6 };
            Pill::Dimmed {
                fill: scale,
                stroke: scale,
            }
        }
        // Native grays: the white-on-gray lift becomes a gray wash, as an
        // inactive macOS source list does.
        InterfaceStyle::System => Pill::Flat(if dark { 0.05 } else { 0.03 }),
        // Theme hues, opaque: dark keeps a hairline-free raised fill, light
        // becomes a faint wash of the theme's text.
        InterfaceStyle::Themed => {
            if dark {
                Pill::Dimmed {
                    fill: if compact { 0.45 } else { 0.6 },
                    stroke: 0.0,
                }
            } else {
                Pill::Flat(0.05)
            }
        }
    };
    ChromeLook {
        key: false,
        pill,
        hover: false,
        title: match style {
            // The native style dims its title row like a native window.
            InterfaceStyle::System => 0.5,
            _ => 0.6,
        },
    }
}

/// The look for the window being drawn, under the store's style and density.
/// Pass the main window, never a floating panel's: panels are never key.
pub(crate) fn look_in(store: &SessionStore, appearance: Appearance, window: &Window) -> ChromeLook {
    look(
        store.interface_style(),
        store.preferences().pane_density,
        appearance,
        window.is_window_active(),
    )
}

/// Styles a tab-like control as the selected pill under `look` while `on`.
/// Callers keep a transparent `border_1` on the off state so toggling never
/// shifts layout.
pub(crate) trait LookPill: Styled + Sized {
    fn look_pill(self, colors: SemanticColors, look: ChromeLook, on: bool) -> Self {
        if on {
            self.bg(look.pill_fill(colors))
                .border_1()
                .border_color(look.pill_stroke(colors))
                .shadow(Glass::shadows(colors))
        } else {
            self
        }
    }
}

impl<T: Styled> LookPill for T {}

#[cfg(test)]
mod tests {
    use super::*;

    const APPEARANCES: [Appearance; 2] = [Appearance::Dark, Appearance::Light];

    fn every_state() -> impl Iterator<Item = (InterfaceStyle, PaneDensity, Appearance)> {
        InterfaceStyle::ALL.into_iter().flat_map(|style| {
            PaneDensity::ALL.into_iter().flat_map(move |density| {
                APPEARANCES
                    .into_iter()
                    .map(move |appearance| (style, density, appearance))
            })
        })
    }

    fn palette(appearance: Appearance) -> SemanticColors {
        SemanticColors::new(appearance)
    }

    #[test]
    fn a_key_window_keeps_the_shipped_look() {
        for (style, density, appearance) in every_state() {
            let look = look(style, density, appearance, true);
            assert_eq!(look, ChromeLook::KEY);
            let colors = palette(appearance);
            assert_eq!(look.pill_fill(colors), Glass::fill(colors));
            assert_eq!(look.pill_stroke(colors), Glass::stroke(colors));
            let hover = colors.primary.alpha(0.06);
            assert_eq!(look.hover(hover), hover);
        }
    }

    #[test]
    fn a_window_without_the_keyboard_steps_every_chrome_cue_back() {
        for (style, density, appearance) in every_state() {
            let key = look(style, density, appearance, true);
            let other = look(style, density, appearance, false);
            let colors = palette(appearance);
            let what = format!("{style:?} {density:?} {appearance:?}");
            assert!(!other.key);
            assert!(
                other.pill_fill(colors).a < key.pill_fill(colors).a,
                "{what}: pill fill"
            );
            assert!(other.pill_fill(colors).a > 0.0, "{what}: selection vanished");
            assert!(
                other.pill_stroke(colors).a < key.pill_stroke(colors).a,
                "{what}: hairline"
            );
            assert_eq!(other.hover(colors.primary.alpha(0.06)).a, 0.0, "{what}");
            assert!(other.title < 1.0 && other.title >= 0.5, "{what}: title");
        }
    }

    #[test]
    fn compact_steps_back_at_least_as_far_as_comfortable() {
        for style in InterfaceStyle::ALL {
            for appearance in APPEARANCES {
                let colors = palette(appearance);
                let fill = |density| look(style, density, appearance, false).pill_fill(colors).a;
                assert!(
                    fill(PaneDensity::Compact) <= fill(PaneDensity::Comfortable),
                    "{style:?} {appearance:?}"
                );
            }
        }
    }
}
