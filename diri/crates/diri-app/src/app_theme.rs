use diri_term::theme::{TermTheme, ThemeAppearance};
use diri_ui::{Appearance, Material, SemanticColors};
use gpui::Rgba;

use crate::store::{Prefs, SessionStore};

/// Resolves persisted theme ids in one place for both terminal and app chrome.
pub(crate) fn terminal_theme(id: &str) -> TermTheme {
    TermTheme::CATALOG
        .into_iter()
        .find(|theme| theme.id == id)
        .unwrap_or_default()
}

/// Opaque application palette for a theme id. Previews, tests, and surfaces
/// that never sit on the blurred window use this; live chrome goes through
/// [`colors_for`] so the window material travels with the palette.
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

/// Application palette carrying the user's window material.
pub(crate) fn colors_for(prefs: &Prefs) -> SemanticColors {
    colors_with(&prefs.terminal_theme, prefs.window_material.to_ui())
}

pub(crate) fn sidebar_colors_for(prefs: &Prefs) -> SemanticColors {
    sidebar_colors_with(&prefs.terminal_theme, prefs.window_material.to_ui())
}

/// Live palette for a store: the previewed theme, if any, under the user's
/// window material.
pub(crate) fn colors_in(store: &SessionStore) -> SemanticColors {
    colors_with(
        store.theme_id(),
        store.preferences().window_material.to_ui(),
    )
}

pub(crate) fn sidebar_colors_in(store: &SessionStore) -> SemanticColors {
    sidebar_colors_with(
        store.theme_id(),
        store.preferences().window_material.to_ui(),
    )
}

fn semantic_colors(theme: TermTheme, sidebar_tones: bool) -> SemanticColors {
    // A small foreground tint keeps chrome neutral and legible while making
    // each terminal theme visibly continuous across the whole application.
    // Dark themes can carry a little more translucency without losing label
    // contrast; light themes keep a denser tint so desktop highlights do not
    // wash the navigation out.
    let sidebar_alpha = match theme.appearance {
        ThemeAppearance::Dark => 0.86,
        ThemeAppearance::Light => 0.90,
    };
    let sidebar_surface = mix(theme.background, theme.foreground, 0.08, sidebar_alpha);
    let floating_surface = mix(theme.background, theme.foreground, 0.13, 1.0);
    SemanticColors::themed(
        match theme.appearance {
            ThemeAppearance::Dark => Appearance::Dark,
            ThemeAppearance::Light => Appearance::Light,
        },
        theme.background,
        theme.foreground,
        sidebar_surface,
        floating_surface,
        sidebar_tones,
    )
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
