use diri_term::theme::{TermTheme, ThemeAppearance};
use diri_ui::{Appearance, Material, SemanticColors};
use gpui::Rgba;

use crate::store::SessionStore;

pub(crate) mod live;

/// Resolves persisted theme ids in one place for both terminal and app chrome.
/// While the application is fading to `id`, this is the fade's current frame.
pub(crate) fn terminal_theme(id: &str) -> TermTheme {
    live::resolve(catalog_theme(id))
}

fn catalog_theme(id: &str) -> TermTheme {
    TermTheme::CATALOG
        .into_iter()
        .find(|theme| theme.id == id)
        .unwrap_or_default()
}

/// The main window calls this as it renders. It follows the store's theme and
/// keeps every window repainting for exactly as long as a fade is running.
pub(crate) fn follow(store: &SessionStore, window: &mut gpui::Window, cx: &mut gpui::App) {
    live::follow(catalog_theme(store.theme_id()), window, cx);
}

/// The terminal theme a store is showing: the previewed one, if any.
pub(crate) fn terminal_theme_in(store: &SessionStore) -> TermTheme {
    let theme = catalog_theme(store.theme_id());
    live::head_for(theme);
    live::resolve(theme)
}

/// Opaque application palette for a theme id. Previews, tests, and surfaces
/// that never sit on the blurred window use this; live chrome goes through
/// [`colors_in`] so the window material travels with the palette.
pub(crate) fn colors(id: &str) -> SemanticColors {
    semantic_colors(terminal_theme(id), false)
}

pub(crate) fn sidebar_colors(id: &str) -> SemanticColors {
    semantic_colors(terminal_theme(id), true)
}

pub(crate) fn colors_with(id: &str, material: Material) -> SemanticColors {
    colors(id).with_material(material)
}

pub(crate) fn sidebar_colors_with(id: &str, material: Material) -> SemanticColors {
    sidebar_colors(id).with_material(material)
}

/// Live palette for a store: the previewed theme, if any, under the user's
/// window material.
pub(crate) fn colors_in(store: &SessionStore) -> SemanticColors {
    semantic_colors(terminal_theme_in(store), false)
        .with_material(store.preferences().window_material.to_ui())
}

pub(crate) fn sidebar_colors_in(store: &SessionStore) -> SemanticColors {
    semantic_colors(terminal_theme_in(store), true)
        .with_material(store.preferences().window_material.to_ui())
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
