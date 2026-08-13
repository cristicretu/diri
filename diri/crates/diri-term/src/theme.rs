use diri_proto::grid::{GridCell, TermColor, TermStyle};
use gpui::Rgba;

const fn rgba_f32(r: f32, g: f32, b: f32, a: f32) -> Rgba {
    Rgba { r, g, b, a }
}

const fn hex(value: u32) -> Rgba {
    Rgba {
        r: ((value >> 16) & 0xff) as f32 / 255.0,
        g: ((value >> 8) & 0xff) as f32 / 255.0,
        b: (value & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..color }
}

const MINIMUM_TEXT_CONTRAST: f32 = 4.5;

fn relative_luminance(color: Rgba) -> f32 {
    fn linear(channel: f32) -> f32 {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

fn contrast_ratio(left: Rgba, right: Rgba) -> f32 {
    let left = relative_luminance(left);
    let right = relative_luminance(right);
    (left.max(right) + 0.05) / (left.min(right) + 0.05)
}

fn mix(left: Rgba, right: Rgba, amount: f32) -> Rgba {
    Rgba {
        r: left.r + (right.r - left.r) * amount,
        g: left.g + (right.g - left.g) * amount,
        b: left.b + (right.b - left.b) * amount,
        a: left.a + (right.a - left.a) * amount,
    }
}

/// Pull an explicit dark-oriented terminal color toward the theme foreground
/// only as far as needed to remain readable on a light default background.
///
/// Programs such as Claude Code emit truecolor syntax palettes chosen by their
/// own theme. A light Diri theme cannot replace those colors through its ANSI
/// palette, so white and neon tokens otherwise disappear into the terminal's
/// light background. Explicit block backgrounds are intentionally excluded by
/// the caller: a program's own diff/error panels keep their authored colors.
fn readable_explicit_foreground(foreground: Rgba, background: Rgba, fallback: Rgba) -> Rgba {
    if contrast_ratio(foreground, background) >= MINIMUM_TEXT_CONTRAST {
        return foreground;
    }
    let fallback = if contrast_ratio(fallback, background) >= MINIMUM_TEXT_CONTRAST {
        fallback
    } else {
        let black = rgba_f32(0.0, 0.0, 0.0, 1.0);
        let white = rgba_f32(1.0, 1.0, 1.0, 1.0);
        if contrast_ratio(black, background) >= contrast_ratio(white, background) {
            black
        } else {
            white
        }
    };

    let mut unreadable = 0.0;
    let mut readable = 1.0;
    for _ in 0..12 {
        let amount = (unreadable + readable) * 0.5;
        if contrast_ratio(mix(foreground, fallback, amount), background) >= MINIMUM_TEXT_CONTRAST {
            readable = amount;
        } else {
            unreadable = amount;
        }
    }
    mix(foreground, fallback, readable)
}

/// Concrete rendering attributes after terminal colors and SGR flags resolve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedCellStyle {
    pub foreground: Rgba,
    pub background: Rgba,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub visible: bool,
}

/// Broad appearance used by application chrome and theme pickers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeAppearance {
    Dark,
    Light,
}

impl ThemeAppearance {
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

/// Renderer-facing terminal color theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TermTheme {
    pub id: &'static str,
    pub name: &'static str,
    pub appearance: ThemeAppearance,
    pub background: Rgba,
    pub foreground: Rgba,
    pub cursor: Rgba,
    pub cursor_text: Rgba,
    pub selection: Rgba,
    pub find_match: Rgba,
    pub find_match_current: Rgba,
    pub ansi: [Rgba; 16],
}

impl TermTheme {
    pub const DIRIJOR_DARK: Self = Self {
        id: "dirijor-dark",
        name: "Dirijor Dark",
        appearance: ThemeAppearance::Dark,
        background: rgba_f32(0.071, 0.075, 0.094, 1.0),
        foreground: rgba_f32(0.90, 0.90, 0.90, 1.0),
        cursor: rgba_f32(0.90, 0.90, 0.90, 0.85),
        cursor_text: rgba_f32(0.05, 0.05, 0.05, 1.0),
        selection: rgba_f32(0.28, 0.42, 0.62, 0.35),
        find_match: rgba_f32(1.0, 0.8, 0.0, 0.35),
        find_match_current: rgba_f32(1.0, 0.8, 0.0, 0.65),
        ansi: [
            hex(0x000000),
            hex(0xcd3131),
            hex(0x0dbc79),
            hex(0xe5e510),
            hex(0x2472c8),
            hex(0xbc3fbc),
            hex(0x11a8cd),
            hex(0xe5e5e5),
            hex(0x666666),
            hex(0xf14c4c),
            hex(0x23d18b),
            hex(0xf5f543),
            hex(0x3b8eea),
            hex(0xd670d6),
            hex(0x29b8db),
            hex(0xffffff),
        ],
    };

    pub const SOLARIZED_DARK: Self = dark_theme(
        "solarized-dark",
        "Solarized Dark",
        0x002b36,
        0x839496,
        0x586e75,
        [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
    );

    pub const DRACULA: Self = dark_theme(
        "dracula",
        "Dracula",
        0x282a36,
        0xf8f8f2,
        0x44475a,
        [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    );

    pub const ONE_DARK: Self = dark_theme(
        "one-dark",
        "One Dark",
        0x282c34,
        0xabb2bf,
        0x3e4451,
        [
            0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
            0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
        ],
    );

    pub const GRUVBOX_DARK: Self = dark_theme(
        "gruvbox-dark",
        "Gruvbox Dark",
        0x282828,
        0xebdbb2,
        0x504945,
        [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    );

    pub const TOKYO_NIGHT: Self = dark_theme(
        "tokyo-night",
        "Tokyo Night",
        0x1a1b26,
        0xc0caf5,
        0x33467c,
        [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
    );

    pub const CATPPUCCIN_MOCHA: Self = dark_theme(
        "catppuccin-mocha",
        "Catppuccin Mocha",
        0x1e1e2e,
        0xcdd6f4,
        0x585b70,
        [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
            0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
        ],
    );

    /// Vesper's official terminal palette. The ANSI colors come from the
    /// upstream Warp theme; cursor and selection colors come from its iTerm
    /// profile so the theme keeps Vesper's warm accent throughout the UI.
    pub const VESPER: Self = Self {
        id: "vesper",
        name: "Vesper",
        appearance: ThemeAppearance::Dark,
        background: hex(0x101010),
        foreground: hex(0xffffff),
        cursor: hex(0xffc799),
        cursor_text: hex(0x101010),
        selection: with_alpha(hex(0xf8feff), 0.247_058_82),
        find_match: with_alpha(hex(0xffc799), 0.35),
        find_match_current: with_alpha(hex(0xffc799), 0.65),
        ansi: [
            hex(0x101010),
            hex(0xff8080),
            hex(0x99ffe4),
            hex(0xffc799),
            hex(0xa0a0a0),
            hex(0xff7300),
            hex(0x99ffe4),
            hex(0xffffff),
            hex(0x505050),
            hex(0xff8080),
            hex(0x99ffe4),
            hex(0xffcfa8),
            hex(0xa0a0a0),
            hex(0xff8080),
            hex(0x99ffe4),
            hex(0xffffff),
        ],
    };

    pub const NORD: Self = dark_theme(
        "nord",
        "Nord",
        0x2e3440,
        0xd8dee9,
        0x4c566a,
        [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
    );

    pub const ROSE_PINE: Self = dark_theme(
        "rose-pine",
        "Rosé Pine",
        0x191724,
        0xe0def4,
        0x403d52,
        [
            0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xe0def4, 0xe0def4,
            0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xe0def4, 0xffffff,
        ],
    );

    pub const KANAGAWA_WAVE: Self = dark_theme(
        "kanagawa-wave",
        "Kanagawa Wave",
        0x1f1f28,
        0xdcd7ba,
        0x2d4f67,
        [
            0x16161d, 0xc34043, 0x76946a, 0xc0a36e, 0x7e9cd8, 0x957fb8, 0x6a9589, 0xc8c093,
            0x727169, 0xe82424, 0x98bb6c, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xdcd7ba,
        ],
    );

    pub const EVERFOREST_DARK: Self = dark_theme(
        "everforest-dark",
        "Everforest Dark",
        0x2d353b,
        0xd3c6aa,
        0x475258,
        [
            0x475258, 0xe67e80, 0xa7c080, 0xdbbc7f, 0x7fbbb3, 0xd699b6, 0x83c092, 0xd3c6aa,
            0x859289, 0xe67e80, 0xa7c080, 0xdbbc7f, 0x7fbbb3, 0xd699b6, 0x83c092, 0xffffff,
        ],
    );

    pub const DIRIJOR_LIGHT: Self = light_theme(
        "dirijor-light",
        "Dirijor Light",
        0xf7f5f0,
        0x25221d,
        0xb8d1e8,
        [
            0x2b2a27, 0xb34234, 0x4d7c3f, 0x9b6a18, 0x356c9a, 0x8a4f8d, 0x2f7d79, 0xe8e4dc,
            0x6d6860, 0xd05a47, 0x669955, 0xc18426, 0x4b83b5, 0xa467a6, 0x449793, 0xffffff,
        ],
    );

    pub const SOLARIZED_LIGHT: Self = light_theme(
        "solarized-light",
        "Solarized Light",
        0xfdf6e3,
        0x657b83,
        0xeee8d5,
        [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
    );

    pub const GITHUB_LIGHT: Self = light_theme(
        "github-light",
        "GitHub Light",
        0xffffff,
        0x24292f,
        0xbddfff,
        [
            0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
            0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0xffffff,
        ],
    );

    pub const GRUVBOX_LIGHT: Self = light_theme(
        "gruvbox-light",
        "Gruvbox Light",
        0xfbf1c7,
        0x3c3836,
        0xd5c4a1,
        [
            0x3c3836, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64,
            0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0xffffff,
        ],
    );

    pub const CATPPUCCIN_LATTE: Self = light_theme(
        "catppuccin-latte",
        "Catppuccin Latte",
        0xeff1f5,
        0x4c4f69,
        0xacb0be,
        [
            0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0x8839ef, 0x179299, 0xacb0be,
            0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xffffff,
        ],
    );

    pub const CATALOG: [Self; 17] = [
        Self::DIRIJOR_DARK,
        Self::SOLARIZED_DARK,
        Self::DRACULA,
        Self::ONE_DARK,
        Self::GRUVBOX_DARK,
        Self::TOKYO_NIGHT,
        Self::CATPPUCCIN_MOCHA,
        Self::VESPER,
        Self::NORD,
        Self::ROSE_PINE,
        Self::KANAGAWA_WAVE,
        Self::EVERFOREST_DARK,
        Self::DIRIJOR_LIGHT,
        Self::SOLARIZED_LIGHT,
        Self::GITHUB_LIGHT,
        Self::GRUVBOX_LIGHT,
        Self::CATPPUCCIN_LATTE,
    ];

    #[must_use]
    pub fn resolve_color(&self, color: TermColor, is_background: bool) -> Rgba {
        match color {
            TermColor::Default | TermColor::DefaultInverted => {
                if is_background {
                    self.background
                } else {
                    self.foreground
                }
            }
            TermColor::Ansi(index @ 0..=15) => self.ansi[usize::from(index)],
            TermColor::Ansi(index) => xterm_extended(index),
            TermColor::Rgb(red, green, blue) => rgba_f32(
                f32::from(red) / 255.0,
                f32::from(green) / 255.0,
                f32::from(blue) / 255.0,
                1.0,
            ),
        }
    }

    /// Resolve cell colors and SGR flags exactly as the Swift grid painter does.
    /// Bold selects a bold face; it intentionally does not brighten ANSI 0–7.
    #[must_use]
    pub fn resolve_cell(&self, cell: GridCell) -> ResolvedCellStyle {
        let inverse = cell.style.contains(TermStyle::INVERSE);
        let mut foreground = if inverse {
            self.resolve_color(cell.bg, true)
        } else {
            self.resolve_color(cell.fg, false)
        };
        let background = if inverse {
            self.resolve_color(cell.fg, false)
        } else {
            self.resolve_color(cell.bg, true)
        };
        if self.appearance == ThemeAppearance::Light
            && !inverse
            && is_default_background(cell.bg)
            && !matches!(cell.fg, TermColor::Default | TermColor::DefaultInverted)
        {
            foreground = readable_explicit_foreground(foreground, background, self.foreground);
        }
        let visible = !cell.style.contains(TermStyle::INVISIBLE);
        if cell.style.contains(TermStyle::DIM) {
            foreground = foreground.opacity(0.5);
        }
        if !visible {
            foreground = foreground.alpha(0.0);
        }

        ResolvedCellStyle {
            foreground,
            background,
            bold: cell.style.contains(TermStyle::BOLD),
            italic: cell.style.contains(TermStyle::ITALIC),
            underline: visible && cell.style.contains(TermStyle::UNDERLINE),
            strikethrough: visible && cell.style.contains(TermStyle::CROSSED_OUT),
            visible,
        }
    }

    #[must_use]
    pub fn signature(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for color in [
            self.background,
            self.foreground,
            self.cursor,
            self.cursor_text,
            self.selection,
            self.find_match,
            self.find_match_current,
        ]
        .into_iter()
        .chain(self.ansi)
        {
            for component in [color.r, color.g, color.b, color.a] {
                hash ^= u64::from(component.to_bits());
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }
}

impl Default for TermTheme {
    fn default() -> Self {
        Self::DIRIJOR_DARK
    }
}

#[must_use]
pub const fn is_default_background(color: TermColor) -> bool {
    matches!(color, TermColor::Default | TermColor::DefaultInverted)
}

const fn dark_theme(
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    selection: u32,
    ansi_values: [u32; 16],
) -> TermTheme {
    palette_theme(
        ThemeAppearance::Dark,
        id,
        name,
        background,
        foreground,
        selection,
        ansi_values,
    )
}

const fn light_theme(
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    selection: u32,
    ansi_values: [u32; 16],
) -> TermTheme {
    palette_theme(
        ThemeAppearance::Light,
        id,
        name,
        background,
        foreground,
        selection,
        ansi_values,
    )
}

const fn palette_theme(
    appearance: ThemeAppearance,
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    selection: u32,
    ansi_values: [u32; 16],
) -> TermTheme {
    let fg = hex(foreground);
    TermTheme {
        id,
        name,
        appearance,
        background: hex(background),
        foreground: fg,
        cursor: with_alpha(fg, 0.85),
        cursor_text: hex(background),
        selection: with_alpha(hex(selection), 0.60),
        find_match: rgba_f32(1.0, 0.8, 0.0, 0.35),
        find_match_current: rgba_f32(1.0, 0.8, 0.0, 0.65),
        ansi: [
            hex(ansi_values[0]),
            hex(ansi_values[1]),
            hex(ansi_values[2]),
            hex(ansi_values[3]),
            hex(ansi_values[4]),
            hex(ansi_values[5]),
            hex(ansi_values[6]),
            hex(ansi_values[7]),
            hex(ansi_values[8]),
            hex(ansi_values[9]),
            hex(ansi_values[10]),
            hex(ansi_values[11]),
            hex(ansi_values[12]),
            hex(ansi_values[13]),
            hex(ansi_values[14]),
            hex(ansi_values[15]),
        ],
    }
}

fn xterm_extended(index: u8) -> Rgba {
    if index < 232 {
        let cube = index - 16;
        let red = cube / 36;
        let green = (cube % 36) / 6;
        let blue = cube % 6;
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let red = STEPS[usize::from(red)];
        let green = STEPS[usize::from(green)];
        let blue = STEPS[usize::from(blue)];
        rgba_f32(
            f32::from(red) / 255.0,
            f32::from(green) / 255.0,
            f32::from(blue) / 255.0,
            1.0,
        )
    } else {
        let value = 8 + (index - 232) * 10;
        let component = f32::from(value) / 255.0;
        rgba_f32(component, component, component, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rgba(actual: Rgba, expected: Rgba) {
        assert!((actual.r - expected.r).abs() < 0.000_01);
        assert!((actual.g - expected.g).abs() < 0.000_01);
        assert!((actual.b - expected.b).abs() < 0.000_01);
        assert!((actual.a - expected.a).abs() < 0.000_01);
    }

    #[test]
    fn catalog_keeps_original_themes_and_adds_light_and_dark_choices() {
        assert_eq!(TermTheme::CATALOG.len(), 17);
        assert_eq!(TermTheme::CATALOG[0].id, "dirijor-dark");
        assert!(
            TermTheme::CATALOG
                .iter()
                .any(|theme| theme.id == "catppuccin-mocha")
        );
        assert!(
            TermTheme::CATALOG
                .iter()
                .any(|theme| theme.appearance == ThemeAppearance::Light)
        );
        assert!(
            TermTheme::CATALOG
                .iter()
                .any(|theme| theme.appearance == ThemeAppearance::Dark)
        );
        let unique_ids = TermTheme::CATALOG
            .iter()
            .map(|theme| theme.id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique_ids.len(), TermTheme::CATALOG.len());
        assert_rgba(
            TermTheme::DIRIJOR_DARK.background,
            rgba_f32(0.071, 0.075, 0.094, 1.0),
        );
        assert_eq!(TermTheme::VESPER.appearance, ThemeAppearance::Dark);
        assert_rgba(TermTheme::VESPER.background, hex(0x101010));
        assert_rgba(TermTheme::VESPER.cursor, hex(0xffc799));
        assert_rgba(TermTheme::VESPER.ansi[5], hex(0xff7300));
        assert_rgba(TermTheme::GITHUB_LIGHT.background, hex(0xffffff));
    }

    #[test]
    fn resolves_base_extended_and_truecolor() {
        let theme = TermTheme::default();
        assert_rgba(
            theme.resolve_color(TermColor::Ansi(1), false),
            hex(0xcd3131),
        );
        assert_rgba(
            theme.resolve_color(TermColor::Ansi(16), false),
            hex(0x000000),
        );
        assert_rgba(
            theme.resolve_color(TermColor::Ansi(231), false),
            hex(0xffffff),
        );
        assert_rgba(
            theme.resolve_color(TermColor::Ansi(232), false),
            hex(0x080808),
        );
        assert_rgba(
            theme.resolve_color(TermColor::Rgb(12, 34, 56), false),
            hex(0x0c2238),
        );
    }

    #[test]
    fn light_theme_keeps_dark_oriented_truecolor_readable_on_its_default_background() {
        for theme in TermTheme::CATALOG
            .iter()
            .filter(|theme| theme.appearance == ThemeAppearance::Light)
        {
            for foreground in [TermColor::Rgb(255, 255, 255), TermColor::Ansi(15)] {
                let resolved = theme.resolve_cell(GridCell::new(
                    u32::from('x'),
                    foreground,
                    TermColor::Default,
                    TermStyle::empty(),
                ));

                assert!(
                    contrast_ratio(resolved.foreground, resolved.background) >= 4.5,
                    "a dark-oriented TUI's bright text must remain readable with {}",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn light_theme_preserves_truecolor_inside_an_explicit_dark_panel() {
        let theme = TermTheme::DIRIJOR_LIGHT;
        let foreground = TermColor::Rgb(255, 255, 255);
        let resolved = theme.resolve_cell(GridCell::new(
            u32::from('x'),
            foreground,
            TermColor::Rgb(64, 0, 0),
            TermStyle::empty(),
        ));

        assert_rgba(resolved.foreground, theme.resolve_color(foreground, false));
    }

    #[test]
    fn dark_theme_preserves_truecolor_on_its_default_background() {
        let theme = TermTheme::DIRIJOR_DARK;
        let foreground = TermColor::Rgb(32, 32, 32);
        let resolved = theme.resolve_cell(GridCell::new(
            u32::from('x'),
            foreground,
            TermColor::Default,
            TermStyle::empty(),
        ));

        assert_rgba(resolved.foreground, theme.resolve_color(foreground, false));
    }

    #[test]
    fn inverse_swaps_roles_and_dim_only_fades_foreground() {
        let theme = TermTheme::default();
        let cell = GridCell::new(
            u32::from('x'),
            TermColor::Ansi(1),
            TermColor::Ansi(4),
            TermStyle::INVERSE | TermStyle::DIM,
        );
        let resolved = theme.resolve_cell(cell);
        assert_rgba(resolved.foreground, theme.ansi[4].opacity(0.5));
        assert_rgba(resolved.background, theme.ansi[1]);
    }

    #[test]
    fn bold_does_not_remap_ansi_and_invisible_hides_decorations() {
        let theme = TermTheme::default();
        let bold = theme.resolve_cell(GridCell::new(
            u32::from('x'),
            TermColor::Ansi(2),
            TermColor::Default,
            TermStyle::BOLD,
        ));
        assert_rgba(bold.foreground, theme.ansi[2]);
        assert!(bold.bold);

        let invisible = theme.resolve_cell(GridCell::new(
            u32::from('x'),
            TermColor::Default,
            TermColor::Default,
            TermStyle::INVISIBLE | TermStyle::UNDERLINE | TermStyle::CROSSED_OUT,
        ));
        assert!(!invisible.visible);
        assert_eq!(invisible.foreground.a, 0.0);
        assert!(!invisible.underline);
        assert!(!invisible.strikethrough);
    }
}
