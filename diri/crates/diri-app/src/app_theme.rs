use diri_term::theme::{TermTheme, ThemeAppearance};
use diri_ui::{Appearance, SemanticColors};
use gpui::Rgba;

use crate::store::{InterfaceStyle, SessionStore};

pub(crate) mod live;

/// Resolves persisted theme ids in one place for both terminal and app chrome.
/// While the application is fading to `id`, this is the fade's current frame.
pub(crate) fn terminal_theme(id: &str) -> TermTheme {
    live::resolve(catalog_theme(id))
}

fn catalog_theme(id: &str) -> TermTheme {
    diri_term::themes::find(id).unwrap_or_default()
}

/// Whether a theme reads as light or dark, as an Agent's first-run "text
/// style" question needs to know.
pub(crate) fn spawn_appearance(id: &str) -> diri_proto::TerminalAppearance {
    match catalog_theme(id).appearance {
        ThemeAppearance::Light => diri_proto::TerminalAppearance::Light,
        ThemeAppearance::Dark => diri_proto::TerminalAppearance::Dark,
    }
}

/// The main window calls this as it renders. It follows the store's theme and
/// keeps every window repainting for exactly as long as a fade is running.
pub(crate) fn follow(store: &SessionStore, window: &mut gpui::Window, cx: &mut gpui::App) {
    let theme = catalog_theme(store.theme_id());
    live::follow(theme, window, cx);
    // The phone's terminal view paints with the same colours. The settled
    // theme, not a fade frame, so a fade does not churn it; the call is a
    // compare when nothing changed.
    diri_web::set_terminal_palette(phone_palette(&styled_terminal(
        theme,
        store.interface_style(),
    )));
}

fn phone_palette(theme: &TermTheme) -> diri_web::TerminalPalette {
    fn rgb(color: Rgba) -> [u8; 3] {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        [channel(color.r), channel(color.g), channel(color.b)]
    }
    diri_web::TerminalPalette {
        foreground: rgb(theme.foreground),
        background: rgb(theme.background),
        ansi: theme.ansi.map(rgb),
    }
}

/// The palette the Engine answers local terminals' color queries from: the
/// saved theme under the saved interface style, never a preview or a fade
/// frame, so programs see what the terminal settles on.
pub(crate) fn engine_palette(
    theme_id: &str,
    style: InterfaceStyle,
) -> diri_proto::TerminalPalette {
    fn rgb(color: Rgba) -> [u8; 3] {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        [channel(color.r), channel(color.g), channel(color.b)]
    }
    let theme = styled_terminal(catalog_theme(theme_id), style);
    diri_proto::TerminalPalette {
        appearance: match theme.appearance {
            ThemeAppearance::Light => diri_proto::TerminalAppearance::Light,
            ThemeAppearance::Dark => diri_proto::TerminalAppearance::Dark,
        },
        foreground: rgb(theme.foreground),
        background: rgb(theme.background),
        cursor: rgb(theme.cursor),
        ansi: theme.ansi.map(rgb),
    }
}

/// The terminal theme a store is showing: the previewed one, if any.
pub(crate) fn terminal_theme_in(store: &SessionStore) -> TermTheme {
    let theme = catalog_theme(store.theme_id());
    live::head_for(theme);
    styled_terminal(live::resolve(theme), store.interface_style())
}

/// The terminal under an interface style. Neutral styles paint the grid on a
/// system gray, so the theme keeps its text and ANSI colors while contrast
/// correction measures against the background actually on screen.
fn styled_terminal(mut theme: TermTheme, style: InterfaceStyle) -> TermTheme {
    if style.neutral() {
        theme.background = system_background(theme.appearance);
    }
    theme
}

/// The window background native macOS apps paint: `#212121` dark, white
/// light, matching the system-palette style measured from Superlogical.
const fn system_background(appearance: ThemeAppearance) -> Rgba {
    match appearance {
        ThemeAppearance::Dark => Rgba {
            r: 0.129,
            g: 0.129,
            b: 0.129,
            a: 1.0,
        },
        ThemeAppearance::Light => Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        },
    }
}

/// Opaque application palette for a theme id. Previews, tests, and surfaces
/// that never sit on the blurred window use this; live chrome goes through
/// [`colors_in`] so the window material travels with the palette.
#[cfg(test)]
pub(crate) fn colors(id: &str) -> SemanticColors {
    semantic_colors(terminal_theme(id), false)
}

pub(crate) fn sidebar_colors(id: &str) -> SemanticColors {
    semantic_colors(terminal_theme(id), true)
}

/// Live palette for a store: the previewed theme, if any, under the user's
/// window material.
pub(crate) fn colors_in(store: &SessionStore) -> SemanticColors {
    styled_colors(store, false)
}

pub(crate) fn sidebar_colors_in(store: &SessionStore) -> SemanticColors {
    styled_colors(store, true)
}

/// The application palette under the store's interface style (its palette
/// preview included): hue source, window material and glass density.
fn styled_colors(store: &SessionStore, sidebar_tones: bool) -> SemanticColors {
    style_colors(
        terminal_theme_in(store),
        store.interface_style(),
        store.preferences().window_transparency,
        sidebar_tones,
    )
}

pub(crate) fn style_colors(
    theme: TermTheme,
    style: InterfaceStyle,
    transparency: f32,
    sidebar_tones: bool,
) -> SemanticColors {
    let colors = if style.neutral() {
        neutral_colors(theme.appearance, sidebar_tones)
    } else {
        semantic_colors(theme, sidebar_tones)
    };
    colors
        .with_material(style.material().to_ui())
        .with_clear_glass(style.clear_glass())
        .with_transparency(transparency)
}

/// Chrome in system grays: no theme hue reaches the sidebar, menus or
/// window fill. Labels use the system's black or white.
fn neutral_colors(appearance: ThemeAppearance, sidebar_tones: bool) -> SemanticColors {
    let gray = |value: f32, alpha: f32| Rgba {
        r: value,
        g: value,
        b: value,
        a: alpha,
    };
    match appearance {
        ThemeAppearance::Dark => SemanticColors::themed(
            Appearance::Dark,
            system_background(appearance),
            gray(1.0, 1.0),
            gray(0.106, 0.86),
            gray(0.176, 1.0),
            sidebar_tones,
        ),
        ThemeAppearance::Light => SemanticColors::themed(
            Appearance::Light,
            system_background(appearance),
            gray(0.0, 1.0),
            gray(0.937, 0.90),
            gray(0.965, 1.0),
            sidebar_tones,
        ),
    }
}

fn semantic_colors(theme: TermTheme, sidebar_tones: bool) -> SemanticColors {
    // A small foreground tint keeps chrome neutral and legible while making
    // each terminal theme visibly continuous across the whole application.
    // Dark themes can carry a little more translucency without losing label
    // contrast; light themes keep a denser tint so desktop highlights do not
    // wash the navigation out.
    let lightness = chrome_lightness(theme);
    let sidebar_alpha = match theme.appearance {
        ThemeAppearance::Dark => 0.86,
        ThemeAppearance::Light if lightness >= 1.0 => 0.90,
        ThemeAppearance::Light => 0.86 + 0.04 * lightness,
    };
    let sidebar_surface = mix(theme.background, theme.foreground, 0.08, sidebar_alpha);
    let floating_surface = mix(theme.background, theme.foreground, 0.13, 1.0);
    let colors = SemanticColors::themed(
        match theme.appearance {
            ThemeAppearance::Dark => Appearance::Dark,
            ThemeAppearance::Light => Appearance::Light,
        },
        theme.background,
        theme.foreground,
        sidebar_surface,
        floating_surface,
        sidebar_tones,
    );
    if lightness < 1.0 && theme.appearance == ThemeAppearance::Light {
        colors.with_lightness(lightness)
    } else {
        colors
    }
}

/// Where chrome stands between its dark and its light constants: 0 or 1 for
/// every catalog theme. A theme fading between dark and light is light
/// throughout (see `TermTheme::mix`) while its background is anywhere in
/// between, so hairlines, selection pills and glass densities follow the
/// background actually painted instead of switching sides on one frame.
fn chrome_lightness(theme: TermTheme) -> f32 {
    match theme.appearance {
        ThemeAppearance::Dark => 0.0,
        ThemeAppearance::Light => {
            let Rgba { r, g, b, .. } = theme.background;
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            ((luma - 0.15) / (0.80 - 0.15)).clamp(0.0, 1.0)
        }
    }
}

fn mix(background: Rgba, foreground: Rgba, amount: f32, alpha: f32) -> Rgba {
    let inverse = 1.0 - amount;
    Rgba {
        r: background.r * inverse + foreground.r * amount,
        g: background.g * inverse + foreground.g * amount,
        b: background.b * inverse + foreground.b * amount,
        a: alpha,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_terminal_theme_drives_application_semantics() {
        let dracula = terminal_theme("dracula");
        let dracula_app = colors("dracula");
        let solarized_app = colors("solarized-dark");

        assert_eq!(dracula_app.background, dracula.background);
        assert_eq!(dracula_app.primary, dracula.foreground);
        assert_ne!(dracula_app.background, solarized_app.background);
        assert_ne!(
            dracula_app.sidebar_surface(),
            solarized_app.sidebar_surface()
        );
        assert_ne!(
            dracula_app.floating_surface(),
            solarized_app.floating_surface()
        );
    }

    #[test]
    fn chrome_crosses_a_light_dark_flip_without_switching_sides_on_one_frame() {
        let (dark, light) = (TermTheme::TOKYO_NIGHT, TermTheme::GRUVBOX_LIGHT);
        let pill = |theme: TermTheme| diri_ui::Glass::fill(semantic_colors(theme, true)).a;
        let hairline = |theme: TermTheme| semantic_colors(theme, false).floating_stroke();
        let mut last = (pill(dark), hairline(dark));
        for step in 1..=60 {
            let theme = dark.mix(&light, step as f32 / 60.0);
            let next = (pill(theme), hairline(theme));
            assert!((next.0 - last.0).abs() < 0.04, "pill jumped at step {step}");
            assert!(
                (next.1.r - last.1.r).abs() < 0.08,
                "hairline jumped at step {step}"
            );
            last = next;
        }
        assert_eq!(last, (pill(light), hairline(light)));
        // Settled themes stand exactly at an end.
        for theme in TermTheme::CATALOG {
            let expected = match theme.appearance {
                ThemeAppearance::Dark => 0.0,
                ThemeAppearance::Light => 1.0,
            };
            assert_eq!(chrome_lightness(theme), expected, "{}", theme.id);
        }
    }

    #[test]
    fn neutral_styles_drop_the_theme_hue_but_keep_its_text_colors() {
        let dracula = terminal_theme("dracula");
        for style in [InterfaceStyle::Modern, InterfaceStyle::System] {
            let colors = style_colors(dracula, style, 1.0, false);
            let terminal = styled_terminal(dracula, style);
            assert_eq!(colors.background, system_background(dracula.appearance));
            assert_eq!(terminal.background, colors.background);
            assert_eq!(terminal.ansi, dracula.ansi);
            assert_eq!(terminal.foreground, dracula.foreground);
            let surface = colors.sidebar_surface();
            assert!((surface.r - surface.g).abs() < 1e-6 && (surface.g - surface.b).abs() < 1e-6);
        }
        for style in [InterfaceStyle::Themed, InterfaceStyle::Blended] {
            assert_eq!(
                style_colors(dracula, style, 1.0, false).background,
                dracula.background
            );
            assert_eq!(styled_terminal(dracula, style), dracula);
        }
    }

    #[test]
    fn modern_glass_lets_more_desktop_through_than_blended() {
        let theme = terminal_theme("dirijor-dark");
        let modern = style_colors(theme, InterfaceStyle::Modern, 1.0, false);
        let blended = style_colors(theme, InterfaceStyle::Blended, 1.0, false);
        assert!(modern.terminal_surface().a < blended.terminal_surface().a);
        assert!(modern.sidebar_surface().a < blended.sidebar_surface().a);
        assert!(
            modern.terminal_surface().a > 0.4,
            "text needs a tint under it"
        );
        for style in [InterfaceStyle::System, InterfaceStyle::Themed] {
            assert_eq!(
                style_colors(theme, style, 1.0, false).terminal_surface().a,
                1.0
            );
        }
    }

    #[test]
    fn sidebar_palette_keeps_stronger_supporting_text() {
        let base = colors("tokyo-night");
        let sidebar = sidebar_colors("tokyo-night");
        assert!(sidebar.secondary.a > base.secondary.a);
        assert!(sidebar.tertiary.a > base.tertiary.a);
    }

    #[test]
    fn light_terminal_themes_produce_light_application_semantics() {
        let theme = terminal_theme("dirijor-light");
        let app = colors(theme.id);

        assert_eq!(app.appearance, Appearance::Light);
        assert_eq!(app.background, theme.background);
        assert_eq!(app.primary, theme.foreground);
        assert_eq!(app.floating_stroke().r, 0.0);
        assert_eq!(app.floating_stroke().a, 0.10);
    }
}
