use std::sync::OnceLock;

use gpui::{
    App, Background, IntoElement, PathBuilder, Pixels, RenderOnce, Window, canvas, div, point,
    prelude::*, px,
};

use crate::{Fill, Icon, IconName, Palette, PathCommand, SemanticColors, SvgPath};

pub const CLAUDE_PATH: &str = "m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z";
pub const OPENAI_PATH: &str = "M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z";
pub const CURSOR_PATH: &str = "M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23";
pub const GEMINI_PATH: &str = "M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58 12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96 2.19.93 3.81 2.55t2.55 3.81";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Cursor,
    Gemini,
    Shell,
    Generic,
}

impl AgentKind {
    pub const ALL: [Self; 6] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Cursor,
        Self::Gemini,
        Self::Shell,
        Self::Generic,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini",
            Self::Shell => "Shell",
            Self::Generic => "Generic",
        }
    }

    pub const fn brand_mark(self) -> Option<BrandMarkKind> {
        match self {
            Self::ClaudeCode => Some(BrandMarkKind::Claude),
            Self::Codex => Some(BrandMarkKind::OpenAi),
            Self::Cursor => Some(BrandMarkKind::Cursor),
            Self::Gemini => Some(BrandMarkKind::Gemini),
            Self::Shell | Self::Generic => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrandMarkKind {
    Claude,
    OpenAi,
    Cursor,
    Gemini,
}

impl BrandMarkKind {
    pub const ALL: [Self; 4] = [Self::Claude, Self::OpenAi, Self::Cursor, Self::Gemini];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude sunburst",
            Self::OpenAi => "OpenAI blossom",
            Self::Cursor => "Cursor cube",
            Self::Gemini => "Gemini sparkle",
        }
    }

    fn data(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_PATH,
            Self::OpenAi => OPENAI_PATH,
            Self::Cursor => CURSOR_PATH,
            Self::Gemini => GEMINI_PATH,
        }
    }

    /// Parsed 24x24 view-box path commands for platform rasterizers.
    pub fn path_commands(self) -> &'static [PathCommand] {
        self.parsed().commands()
    }

    fn parsed(self) -> &'static SvgPath {
        static CLAUDE: OnceLock<SvgPath> = OnceLock::new();
        static OPENAI: OnceLock<SvgPath> = OnceLock::new();
        static CURSOR: OnceLock<SvgPath> = OnceLock::new();
        static GEMINI: OnceLock<SvgPath> = OnceLock::new();
        let cell = match self {
            Self::Claude => &CLAUDE,
            Self::OpenAi => &OPENAI,
            Self::Cursor => &CURSOR,
            Self::Gemini => &GEMINI,
        };
        cell.get_or_init(|| SvgPath::parse(self.data()).expect("bundled brand SVG must be valid"))
    }
}

/// Optional platform hook producing a crisp raster for a solid-color mark.
/// GPUI's GPU path antialiasing is visibly soft at row-icon sizes; macOS
/// installs a CoreGraphics rasterizer at startup. Vector rendering remains
/// authoritative for gradient fills and animated scaling.
pub type MarkRasterizer = fn(BrandMarkKind, f32, f32, gpui::Rgba) -> Option<gpui::AnyElement>;
static MARK_RASTERIZER: OnceLock<MarkRasterizer> = OnceLock::new();

pub fn set_mark_rasterizer(rasterizer: MarkRasterizer) {
    let _ = MARK_RASTERIZER.set(rasterizer);
}

/// A tintable vector mark authored in a 24×24 view box.
#[derive(IntoElement)]
pub struct BrandMark {
    kind: BrandMarkKind,
    size: f32,
    fill: Background,
    solid: Option<gpui::Rgba>,
    inset_ratio: f32,
    visual_scale: f32,
}

impl BrandMark {
    pub fn new(kind: BrandMarkKind, size: f32, fill: impl Into<Background>) -> Self {
        Self {
            kind,
            size,
            fill: fill.into(),
            solid: None,
            inset_ratio: 0.0,
            visual_scale: 1.0,
        }
    }

    /// A solid-color mark, eligible for the platform rasterizer.
    pub fn solid(kind: BrandMarkKind, size: f32, color: gpui::Rgba) -> Self {
        Self {
            kind,
            size,
            fill: color.into(),
            solid: Some(color),
            inset_ratio: 0.0,
            visual_scale: 1.0,
        }
    }

    pub fn inset(mut self, ratio: f32) -> Self {
        self.inset_ratio = ratio;
        self
    }

    pub fn visual_scale(mut self, scale: f32) -> Self {
        self.visual_scale = scale;
        self
    }
}

impl RenderOnce for BrandMark {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        if let (Some(color), Some(rasterize)) = (self.solid, MARK_RASTERIZER.get().copied())
            && (self.visual_scale - 1.0).abs() < 0.001
            && let Some(element) = rasterize(self.kind, self.size, self.inset_ratio, color)
        {
            return element;
        }
        let commands = self.kind.parsed().commands().to_vec();
        let fill = self.fill;
        let inset = self.inset_ratio;
        let visual_scale = self.visual_scale;
        canvas(
            move |bounds, _, _| build_path(&commands, bounds, inset, visual_scale),
            move |_, path, window, _| {
                if let Some(path) = path {
                    window.paint_path(path, fill);
                }
            },
        )
        .size(px(self.size))
        .into_any_element()
    }
}

fn build_path(
    commands: &[PathCommand],
    bounds: gpui::Bounds<Pixels>,
    inset_ratio: f32,
    visual_scale: f32,
) -> Option<gpui::Path<Pixels>> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let available_width = width * (1.0 - 2.0 * inset_ratio).max(0.0);
    let available_height = height * (1.0 - 2.0 * inset_ratio).max(0.0);
    let scale = (available_width.min(available_height) / 24.0) * visual_scale;
    let drawn_size = 24.0 * scale;
    let origin_x = f32::from(bounds.origin.x) + (width - drawn_size) / 2.0;
    let origin_y = f32::from(bounds.origin.y) + (height - drawn_size) / 2.0;
    let map = |source: (f32, f32)| {
        point(
            px(origin_x + source.0 * scale),
            px(origin_y + source.1 * scale),
        )
    };

    let mut builder = PathBuilder::fill();
    for command in commands {
        match *command {
            PathCommand::MoveTo(x, y) => builder.move_to(map((x, y))),
            PathCommand::LineTo(x, y) => builder.line_to(map((x, y))),
            PathCommand::QuadTo { control, to } => builder.curve_to(map(to), map(control)),
            PathCommand::CubicTo {
                control_a,
                control_b,
                to,
            } => builder.cubic_bezier_to(map(to), map(control_a), map(control_b)),
            PathCommand::Close => builder.close(),
        }
    }
    builder.build().ok()
}

/// A regularized agent mark seated in an optional rounded badge.
#[derive(IntoElement)]
pub struct AgentLogo {
    kind: AgentKind,
    size: f32,
    badged: bool,
    colors: SemanticColors,
}

impl AgentLogo {
    pub fn new(kind: AgentKind, size: f32, colors: SemanticColors) -> Self {
        Self {
            kind,
            size,
            badged: true,
            colors,
        }
    }

    pub fn badged(mut self, badged: bool) -> Self {
        self.badged = badged;
        self
    }
}

impl RenderOnce for AgentLogo {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mark_size = self.size * (1.0 - 2.0 * 0.28);
        let fill = match self.kind {
            AgentKind::ClaudeCode => Palette::CLAY,
            AgentKind::Codex | AgentKind::Cursor => self.colors.primary.alpha(0.82),
            AgentKind::Gemini => Palette::GEMINI_BLUE,
            AgentKind::Shell | AgentKind::Generic => self.colors.secondary,
        };
        let badge_fill = match self.kind {
            AgentKind::ClaudeCode => Palette::CLAY.alpha(0.14),
            _ => Fill::subtle(self.colors),
        };

        let mut container = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(self.size))
            .rounded(px(self.size * 0.27));
        if self.badged {
            container = container.bg(badge_fill);
        }

        if let Some(mark) = self.kind.brand_mark() {
            container.child(BrandMark::solid(mark, mark_size, fill))
        } else {
            container.child(Icon::new(IconName::Terminal, self.size * 0.54, fill))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_marks_parse() {
        for mark in BrandMarkKind::ALL {
            assert!(mark.parsed().commands().len() > 3, "{}", mark.label());
        }
    }

    #[test]
    fn brand_mapping_matches_swift() {
        assert_eq!(
            AgentKind::ClaudeCode.brand_mark(),
            Some(BrandMarkKind::Claude)
        );
        assert_eq!(AgentKind::Codex.brand_mark(), Some(BrandMarkKind::OpenAi));
        assert_eq!(AgentKind::Shell.brand_mark(), None);
    }
}
