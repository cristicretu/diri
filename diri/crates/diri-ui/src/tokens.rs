use gpui::{FontWeight, Rgba, WindowAppearance};

/// Constructs a GPUI color from normalized channel values.
pub const fn rgba_f32(r: f32, g: f32, b: f32, a: f32) -> Rgba {
    Rgba { r, g, b, a }
}

/// Continuous-corner radii from the Swift design system.
///
/// GPUI's rounded rectangles are circular, not continuous squircles. Consumers
/// should use these exact radii with GPUI's `rounded` API until native
/// continuous corners are available.
pub struct Radius;

impl Radius {
    pub const CHIP: f32 = 5.0;
    pub const BADGE: f32 = 6.0;
    pub const ROW: f32 = 7.0;
    pub const CARD: f32 = 10.0;
    pub const PANEL: f32 = 12.0;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    Meta,
    SectionHeader,
    Row,
    RowEmphasized,
    Title,
    DisplayTitle,
    MetaMono,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeStyle {
    pub size: f32,
    pub weight: FontWeight,
    pub monospaced: bool,
}

pub struct Typo;

impl Typo {
    pub const META: TypeStyle = TypeStyle::new(11.0, FontWeight::MEDIUM, false);
    pub const SECTION_HEADER: TypeStyle = TypeStyle::new(11.0, FontWeight::SEMIBOLD, false);
    pub const ROW: TypeStyle = TypeStyle::new(13.0, FontWeight::NORMAL, false);
    pub const ROW_EMPHASIZED: TypeStyle = TypeStyle::new(13.0, FontWeight::MEDIUM, false);
    pub const TITLE: TypeStyle = TypeStyle::new(13.0, FontWeight::SEMIBOLD, false);
    pub const DISPLAY_TITLE: TypeStyle = TypeStyle::new(15.0, FontWeight::SEMIBOLD, false);
    pub const META_MONO: TypeStyle = TypeStyle::new(11.0, FontWeight::MEDIUM, true);

    pub const ALL: [(TextRole, TypeStyle); 7] = [
        (TextRole::Meta, Self::META),
        (TextRole::SectionHeader, Self::SECTION_HEADER),
        (TextRole::Row, Self::ROW),
        (TextRole::RowEmphasized, Self::ROW_EMPHASIZED),
        (TextRole::Title, Self::TITLE),
        (TextRole::DisplayTitle, Self::DISPLAY_TITLE),
        (TextRole::MetaMono, Self::META_MONO),
    ];
}

impl TypeStyle {
    pub const fn new(size: f32, weight: FontWeight, monospaced: bool) -> Self {
        Self {
            size,
            weight,
            monospaced,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub fn from_window(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextTone {
    Selected,
    Unselected,
    Label,
}

/// Light/dark-aware semantic colors corresponding to SwiftUI's label colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SemanticColors {
    pub appearance: Appearance,
    pub primary: Rgba,
    pub secondary: Rgba,
    pub tertiary: Rgba,
    pub background: Rgba,
    sidebar_surface: Rgba,
    floating_surface: Rgba,
}

impl SemanticColors {
    pub const fn light() -> Self {
        let foreground = rgba_f32(0.0, 0.0, 0.0, 1.0);
        Self {
            appearance: Appearance::Light,
            primary: foreground,
            secondary: rgba_f32(0.0, 0.0, 0.0, 0.60),
            tertiary: rgba_f32(0.0, 0.0, 0.0, 0.30),
            background: rgba_f32(1.0, 1.0, 1.0, 1.0),
            sidebar_surface: rgba_f32(0.949, 0.953, 0.941, 0.89),
            floating_surface: rgba_f32(0.949, 0.953, 0.941, 1.0),
        }
    }

    pub const fn dark() -> Self {
        let foreground = rgba_f32(1.0, 1.0, 1.0, 1.0);
        Self {
            appearance: Appearance::Dark,
            primary: foreground,
            secondary: rgba_f32(1.0, 1.0, 1.0, 0.60),
            tertiary: rgba_f32(1.0, 1.0, 1.0, 0.30),
            background: rgba_f32(0.071, 0.075, 0.094, 1.0),
            sidebar_surface: rgba_f32(0.141, 0.161, 0.196, 0.89),
            floating_surface: rgba_f32(0.141, 0.161, 0.196, 1.0),
        }
    }

    pub const fn new(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light => Self::light(),
            Appearance::Dark => Self::dark(),
        }
    }

    /// Sidebar materials sit over live desktop content, so stock label
    /// opacities lose more perceived contrast than they do on an opaque
    /// surface. Keep the same neutral family with firmer supporting tones.
    pub const fn sidebar(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light => Self {
                secondary: rgba_f32(0.0, 0.0, 0.0, 0.68),
                tertiary: rgba_f32(0.0, 0.0, 0.0, 0.42),
                ..Self::light()
            },
            Appearance::Dark => Self {
                secondary: rgba_f32(1.0, 1.0, 1.0, 0.70),
                tertiary: rgba_f32(1.0, 1.0, 1.0, 0.44),
                ..Self::dark()
            },
        }
    }

    /// Builds the semantic application palette from a concrete product theme.
    ///
    /// Surface colors are explicit so application chrome and terminal content
    /// can use the same light or dark palette without coupling `diri-ui` to the
    /// terminal crate.
    pub const fn themed(
        appearance: Appearance,
        background: Rgba,
        foreground: Rgba,
        sidebar_surface: Rgba,
        floating_surface: Rgba,
        sidebar_tones: bool,
    ) -> Self {
        let secondary_alpha = if sidebar_tones { 0.70 } else { 0.60 };
        let tertiary_alpha = if sidebar_tones { 0.44 } else { 0.30 };
        Self {
            appearance,
            primary: foreground,
            secondary: rgba_f32(foreground.r, foreground.g, foreground.b, secondary_alpha),
            tertiary: rgba_f32(foreground.r, foreground.g, foreground.b, tertiary_alpha),
            background,
            sidebar_surface,
            floating_surface,
        }
    }

    pub fn text(self, tone: TextTone) -> Rgba {
        let alpha = match tone {
            TextTone::Selected => 1.0,
            TextTone::Unselected => 0.75,
            TextTone::Label => 0.85,
        };
        self.primary.alpha(alpha)
    }

    pub const fn floating_stroke(self) -> Rgba {
        match self.appearance {
            Appearance::Dark => rgba_f32(1.0, 1.0, 1.0, 0.08),
            Appearance::Light => rgba_f32(0.0, 0.0, 0.0, 0.10),
        }
    }

    /// Denser material for transient UI layered over live content.
    ///
    /// Menus, popovers, and dialogs need stronger separation than persistent
    /// sidebars so text and controls never compete with the content beneath.
    pub const fn floating_surface(self) -> Rgba {
        self.floating_surface
    }

    /// Shared translucent material for the leading and trailing sidebars.
    pub const fn sidebar_surface(self) -> Rgba {
        self.sidebar_surface
    }
}

pub struct Palette;

impl Palette {
    pub const CLAY: Rgba = rgba_f32(0.851, 0.467, 0.341, 1.0);
    pub const GEMINI_BLUE: Rgba = rgba_f32(0.306, 0.510, 0.933, 1.0);
}

pub struct Ink;

impl Ink {
    pub const ATTENTION: Rgba = rgba_f32(0.961, 0.651, 0.137, 1.0);
    pub const DANGER: Rgba = rgba_f32(0.961, 0.271, 0.227, 1.0);
    pub const FRESH: Rgba = rgba_f32(0.204, 0.780, 0.349, 1.0);
    pub const GENERIC_WORKING: Rgba = rgba_f32(0.541, 0.561, 0.596, 1.0);

    pub fn working(kind: crate::AgentKind, semantic: SemanticColors) -> Rgba {
        match kind {
            crate::AgentKind::ClaudeCode => Palette::CLAY,
            crate::AgentKind::Codex | crate::AgentKind::Cursor => semantic.primary.alpha(0.82),
            crate::AgentKind::Gemini => Palette::GEMINI_BLUE,
            crate::AgentKind::Shell | crate::AgentKind::Generic => Self::GENERIC_WORKING,
        }
    }

    pub const fn overprint(kind: crate::AgentKind) -> Rgba {
        match kind {
            crate::AgentKind::ClaudeCode => rgba_f32(1.0, 0.435, 0.380, 1.0),
            crate::AgentKind::Codex => rgba_f32(0.180, 0.800, 0.741, 1.0),
            crate::AgentKind::Cursor => rgba_f32(0.62, 0.45, 0.95, 1.0),
            crate::AgentKind::Gemini => rgba_f32(0.85, 0.40, 0.55, 1.0),
            crate::AgentKind::Shell | crate::AgentKind::Generic => rgba_f32(0.62, 0.64, 0.68, 1.0),
        }
    }
}

pub struct Fill;

impl Fill {
    pub const HOVER_OPACITY: f32 = 0.06;
    pub const MULTI_SELECTED_OPACITY: f32 = 0.08;
    pub const SELECTED_OPACITY: f32 = 0.10;
    pub const SUBTLE_OPACITY: f32 = 0.06;

    pub fn hover(colors: SemanticColors, on: bool) -> Rgba {
        colors
            .primary
            .alpha(if on { Self::HOVER_OPACITY } else { 0.0 })
    }

    pub fn selected(colors: SemanticColors, on: bool) -> Rgba {
        colors
            .primary
            .alpha(if on { Self::SELECTED_OPACITY } else { 0.0 })
    }

    pub fn multi_selected(colors: SemanticColors) -> Rgba {
        colors.primary.alpha(Self::MULTI_SELECTED_OPACITY)
    }

    pub fn subtle(colors: SemanticColors) -> Rgba {
        colors.primary.alpha(Self::SUBTLE_OPACITY)
    }
}

/// Shared geometry for the compact state chips (Zzz, Ended, host, …).
///
/// GPUI draws them via [`crate::StateChip`]; AppKit menubar rows must use the
/// same numbers so the two surfaces stay optically identical.
pub struct Chip;

impl Chip {
    pub const PAD_X: f32 = 5.0;
    pub const PAD_Y: f32 = 1.0;
    /// The chip pins its own line box rather than inheriting GPUI's default for
    /// the font size. AppKit has no line box at all, so leaving this implicit
    /// meant the menubar had to guess at a text engine it does not run — and it
    /// guessed with a magic `+ 4.0` on top of this token.
    pub const LINE_H: f32 = 15.0;

    pub const fn font_size() -> f32 {
        Typo::META.size
    }

    /// Full painted height of the pill. Both surfaces size from this.
    pub const fn height() -> f32 {
        Self::LINE_H + Self::PAD_Y * 2.0
    }
}

pub struct Space;

impl Space {
    pub const INDENT: f32 = 12.0;
    pub const ROW_H: f32 = 8.0;
    pub const INSET: f32 = 10.0;
}

pub struct Metrics;

impl Metrics {
    pub const TITLE_BAR: f32 = 42.0;
    pub const TOOLBAR_EDGE_INSET: f32 = 12.0;
    pub const TOOLBAR_TRAFFIC_LIGHT_LANE: f32 = 66.0;
    pub const TOOLBAR_ITEM_GAP: f32 = 8.0;
    pub const TOOLBAR_COMPACT_GAP: f32 = 4.0;
    pub const TOOLBAR_CONTROL_SIZE: f32 = 26.0;
    pub const TOOLBAR_CHIP_HEIGHT: f32 = 24.0;
    pub const ROW_HEIGHT: f32 = 28.0;
    pub const NEW_AGENT_FOOTER: f32 = 32.0;
    pub const TRAFFIC_LIGHT_X_OFFSET: f32 = 12.0;
    pub const TRAFFIC_LIGHT_Y_OFFSET: f32 = 6.0;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub response: f32,
    pub damping_fraction: f32,
}

impl Spring {
    pub const fn new(response: f32, damping_fraction: f32) -> Self {
        Self {
            response,
            damping_fraction,
        }
    }
}

pub struct Motion;

impl Motion {
    pub const SNAP: Spring = Spring::new(0.32, 0.74);
    pub const POP: Spring = Spring::new(0.40, 0.60);
    pub const SETTLE: Spring = Spring::new(0.55, 0.82);
    pub const FOOTER_PIN: Spring = Spring::new(0.32, 0.82);
    pub const ROW_SELECT: f32 = 0.16;
    pub const OVERLAY_FADE: f32 = 0.12;
    /// Sidebar and inspector open/close. Longer than the fades above because
    /// the seam moves a whole panel width and pushes the workbench with it.
    pub const SEAM_SLIDE_MS: u64 = 260;
    pub const BREATHE: f64 = 2.6;
    pub const SWEEP_REV: f64 = 2.4;
    pub const PING_PERIOD: f64 = 1.8;
    pub const PING_PERIOD_RISK: f64 = 1.2;
    pub const SHELL_BLINK: f64 = 1.6;
    pub const TICK_HZ: u64 = 10;
}

pub struct MemoryFormat;

impl MemoryFormat {
    pub const SOFT_BYTES: u64 = 2 * 1_073_741_824;
    pub const LOUD_BYTES: u64 = 6 * 1_073_741_824;

    pub fn gb(bytes: u64) -> String {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    }

    pub fn badge(bytes: Option<u64>) -> Option<String> {
        bytes
            .filter(|bytes| *bytes > Self::SOFT_BYTES)
            .map(Self::gb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_opacities_match_swift_conventions() {
        let colors = SemanticColors::dark();
        assert_eq!(colors.text(TextTone::Selected).a, 1.0);
        assert_eq!(colors.text(TextTone::Unselected).a, 0.75);
        assert_eq!(colors.text(TextTone::Label).a, 0.85);
        assert_eq!(Fill::hover(colors, true).a, 0.06);
        assert_eq!(Fill::selected(colors, true).a, 0.10);
    }

    #[test]
    fn sidebar_supporting_tones_survive_translucent_materials() {
        let base = SemanticColors::dark();
        let sidebar = SemanticColors::sidebar(Appearance::Dark);
        assert!(sidebar.secondary.a > base.secondary.a);
        assert!(sidebar.tertiary.a > base.tertiary.a);
        assert_eq!(sidebar.primary, base.primary);
        assert_eq!(sidebar.background, base.background);
    }

    #[test]
    fn floating_material_is_denser_than_sidebar_material() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let colors = SemanticColors::new(appearance);
            assert!(colors.floating_surface().a > colors.sidebar_surface().a);
            assert_eq!(colors.floating_surface().a, 1.0);
        }
    }

    #[test]
    fn memory_badge_uses_strict_soft_threshold() {
        assert_eq!(MemoryFormat::badge(Some(MemoryFormat::SOFT_BYTES)), None);
        assert_eq!(
            MemoryFormat::badge(Some(MemoryFormat::SOFT_BYTES + 1)),
            Some("2.0 GB".to_owned())
        );
    }
}
