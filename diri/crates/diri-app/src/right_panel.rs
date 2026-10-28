//! Chrome for the right sidebar (the workbench inspector) and the one control
//! that opens and closes it.
//!
//! The panel sits beside the terminal rather than beside the navigation, so it
//! is painted with the terminal's background and reads as part of the work
//! surface; the left sidebar keeps its own tinted material. Text keeps the
//! sidebar palette's stronger supporting tones, which stay legible on either.
//!
//! Every surface inside the panel should take its colors from here --
//! [`panel_colors_in`] and [`panel_background`] -- so a theme change, a live
//! theme fade, or a window material switch repaints the whole panel at once.
//!
//! Tab strip styling adapted from Ely GPUI Components (MIT OR Apache-2.0),
//! https://github.com/ZacharyZhang-NY/Ely-GPUI-Components (`navigation/editor_tabs.rs`):
//! compact tabs whose close slot is always reserved, so a tab never changes
//! width when its close button appears on hover.

use diri_ui::{Fill, Metrics, Radius, SemanticColors};
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Rgba, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::commands::CommandId;
use crate::icons::sf_symbol;
use crate::store::SessionStore;
use crate::tooltip_warmth::WarmTooltip;

/// The toggle's glyph, mirroring the left sidebar's `sidebar.left`.
pub(crate) const TOGGLE_SYMBOL: &str = "sidebar.right";

/// Height of one workspace tab inside the 42pt title bar.
pub(crate) const TAB_HEIGHT: f32 = 26.0;
/// Widest a tab grows before its title truncates.
pub(crate) const TAB_MAX_WIDTH: f32 = 168.0;
/// The reserved close slot at the trailing edge of every tab.
pub(crate) const TAB_CLOSE_SIZE: f32 = 16.0;

/// The palette everything inside the right panel paints with.
pub(crate) fn panel_colors_in(store: &SessionStore) -> SemanticColors {
    crate::app_theme::sidebar_colors_in(store)
}

/// The panel's fill: exactly the terminal's, under either window material.
/// Opaque windows paint the theme background; glass windows paint the same
/// translucent terminal tint the session pane does, over the same window
/// fill, so the seam between them disappears into one surface.
pub(crate) fn panel_background(colors: SemanticColors) -> Rgba {
    colors.terminal_surface()
}

/// The hairline between the terminal card and the panel, and under the
/// panel's title bar. Both surfaces share a fill, so this line is the only
/// thing separating them; it is a touch stronger than the sidebar's.
pub(crate) fn panel_divider(colors: SemanticColors) -> Rgba {
    colors.primary.alpha(0.09)
}

/// Fill of the selected workspace tab.
pub(crate) fn tab_active_fill(colors: SemanticColors) -> Rgba {
    colors.primary.alpha(0.085)
}

/// Fill of a hovered, unselected workspace tab.
pub(crate) fn tab_hover_fill(colors: SemanticColors) -> Rgba {
    colors.primary.alpha(0.045)
}

/// What the toggle says it will do.
pub(crate) fn toggle_label(open: bool) -> &'static str {
    if open {
        crate::i18n::t("panel.hide_right_sidebar")
    } else {
        crate::i18n::t("panel.show_right_sidebar")
    }
}


/// The right sidebar toggle. It is drawn by whichever title bar ends at the
/// window's trailing edge -- the session pane's while the panel is closed, the
/// panel's own while it is open -- so at rest it is always the last control in
/// the title bar, and it rides in and out with the panel the way the left
/// sidebar's toggle and the pane's reveal control trade places.
///
/// `selector` is the debug selector and held-hint key; the two title bars use
/// different ones because both are painted for the length of a slide.
pub(crate) fn toggle_button(
    selector: &'static str,
    open: bool,
    colors: SemanticColors,
    held_hint: f32,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let button = div()
        .id(selector)
        .debug_selector(move || selector.into())
        .role(gpui::Role::Button)
        .aria_label(toggle_label(open))
        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .cursor_pointer()
        .hover(move |button| button.bg(Fill::subtle(colors)))
        .child(sf_symbol(TOGGLE_SYMBOL, 15.0, colors.secondary))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |event, window, cx| {
            on_click(event, window, cx);
            cx.stop_propagation();
        })
        .shortcut_tooltip(toggle_label(open), Some(CommandId::ToggleInspector), colors)
        .into_any_element();
    crate::held_hints::below(
        button,
        selector,
        crate::held_hints::label(CommandId::ToggleInspector),
        held_hint,
        colors,
    )
}

/// The toggle as the session pane draws it: it asks the root to toggle, which
/// owns the panel's visibility and its slide.
pub(crate) fn dispatching_toggle(
    selector: &'static str,
    open: bool,
    colors: SemanticColors,
    held_hint: f32,
) -> AnyElement {
    toggle_button(selector, open, colors, held_hint, |_, window, cx| {
        window.dispatch_action(Box::new(crate::commands::ToggleInspector), cx);
    })
}

/// Whether a tab's close control is drawn. The slot is always reserved, so
/// this only decides visibility: the selected tab always offers it, the others
/// only under the pointer.
pub(crate) fn tab_close_visible(active: bool, hovered: bool) -> bool {
    active || hovered
}

/// The surface a freshly opened panel shows when it has no tabs left: the
/// user's last details destination, so reopening lands on real content rather
/// than on an empty chooser.
pub(crate) fn default_surface(
    preferred: crate::store::InspectorTab,
) -> crate::inspector::WorkspaceSurface {
    use crate::inspector::WorkspaceSurface;
    use crate::store::InspectorTab;
    match preferred {
        InspectorTab::Changes => WorkspaceSurface::Review,
        InspectorTab::Code => WorkspaceSurface::Files,
        InspectorTab::Info | InspectorTab::Artifacts => WorkspaceSurface::Details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::WorkspaceSurface;
    use crate::store::InspectorTab;

    fn luminance(color: Rgba) -> f32 {
        let channel = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    fn over(top: Rgba, bottom: Rgba) -> Rgba {
        let mix = |a: f32, b: f32| a * top.a + b * (1.0 - top.a);
        Rgba {
            r: mix(top.r, bottom.r),
            g: mix(top.g, bottom.g),
            b: mix(top.b, bottom.b),
            a: 1.0,
        }
    }

    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn the_panel_paints_the_terminal_background_not_the_sidebar_material() {
        for id in ["tokyo-night", "dirijor-light", "dracula", "solarized-dark"] {
            let terminal = crate::app_theme::colors(id);
            let panel = crate::app_theme::sidebar_colors(id);
            assert_eq!(
                panel_background(panel),
                terminal.terminal_surface(),
                "{id}: the right panel must match the terminal"
            );
            assert_ne!(
                panel_background(panel),
                panel.sidebar_surface(),
                "{id}: the right panel must not wear the left sidebar's material"
            );
        }
    }

    #[test]
    fn panel_chrome_stays_legible_in_light_and_dark_themes() {
        for id in ["tokyo-night", "dirijor-light", "gruvbox-light", "dracula"] {
            let colors = crate::app_theme::sidebar_colors(id);
            let background = panel_background(colors);
            // Labels on the selected tab and secondary glyphs.
            let active = over(tab_active_fill(colors), background);
            assert!(
                contrast(over(colors.primary, active), active) >= 4.5,
                "{id}: selected tab label"
            );
            assert!(
                contrast(over(colors.secondary, background), background) >= 3.0,
                "{id}: toolbar glyphs"
            );
            // The selected tab is distinguishable from its neighbours, and
            // hover sits between rest and selection.
            assert!(tab_active_fill(colors).a > tab_hover_fill(colors).a);
            assert!(
                contrast(active, background) > 1.02,
                "{id}: selected tab fill"
            );
            // The divider is a visible hairline, never a heavy rule.
            let divider = over(panel_divider(colors), background);
            let ratio = contrast(divider, background);
            assert!((1.03..1.6).contains(&ratio), "{id}: divider {ratio}");
        }
    }

    #[test]
    fn the_close_slot_shows_on_the_selected_tab_and_under_the_pointer() {
        assert!(tab_close_visible(true, false));
        assert!(tab_close_visible(false, true));
        assert!(!tab_close_visible(false, false));
    }

    #[test]
    fn an_emptied_panel_reopens_on_the_last_details_destination() {
        assert_eq!(
            default_surface(InspectorTab::Changes),
            WorkspaceSurface::Review
        );
        assert_eq!(default_surface(InspectorTab::Code), WorkspaceSurface::Files);
        assert_eq!(
            default_surface(InspectorTab::Info),
            WorkspaceSurface::Details
        );
        assert_eq!(
            default_surface(InspectorTab::Artifacts),
            WorkspaceSurface::Details
        );
    }

    #[test]
    fn the_toggle_names_what_it_will_do() {
        assert_eq!(toggle_label(true), "Hide right sidebar");
        assert_eq!(toggle_label(false), "Show right sidebar");
    }
}
