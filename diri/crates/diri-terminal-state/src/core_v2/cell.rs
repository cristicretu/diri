//! The 8-byte cell and the style it points at.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// One grid cell in 8 bytes.
///
/// | bits   | field                                                    |
/// |--------|----------------------------------------------------------|
/// | 0..21  | codepoint; 0 is a blank that was never written           |
/// | 21..26 | WIDE, SPACER, LEADING_SPACER, GRAPHEME, HYPERLINK        |
/// | 26..31 | shell marks (OSC 133 A/B/C/D-ok/D-failed)                |
/// | 32..48 | style id in the owning page's style table (0 = default) |
/// | 48..64 | cluster index in the page's arena, with GRAPHEME         |
///
/// Row-level facts (soft wrap, occupancy, the erase fill) live in the row's
/// [`RowMeta`](super::page::RowMeta), not in cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct Cell(pub u64);

pub const CP: u64 = 0x1F_FFFF;
pub const WIDE: u64 = 1 << 21;
pub const SPACER: u64 = 1 << 22;
pub const LEADING: u64 = 1 << 23;
pub const GRAPHEME: u64 = 1 << 24;
pub const HYPERLINK: u64 = 1 << 25;
pub const MARKS_SHIFT: u32 = 26;
pub const MARKS: u64 = 0x1F << MARKS_SHIFT;
pub const STYLE_SHIFT: u32 = 32;
pub const STYLE: u64 = 0xFFFF << STYLE_SHIFT;
/// Index of the cell's cluster in the page's cluster arena (with GRAPHEME).
pub const CLUSTER_SHIFT: u32 = 48;
pub const CLUSTER: u64 = 0xFFFF << CLUSTER_SHIFT;

impl Cell {
    #[inline(always)]
    pub fn blank(style: u16) -> Self {
        Cell((style as u64) << STYLE_SHIFT)
    }

    #[inline(always)]
    pub fn ch(self) -> char {
        match (self.0 & CP) as u32 {
            0 => ' ',
            // Only valid scalars are ever stored.
            cp => unsafe { char::from_u32_unchecked(cp) },
        }
    }

    #[inline(always)]
    pub fn style(self) -> u16 {
        (self.0 >> STYLE_SHIFT) as u16
    }

    #[inline(always)]
    pub fn has(self, bits: u64) -> bool {
        self.0 & bits != 0
    }
}

/// Everything about a cell's look that is not its character: the
/// alacritty `Flags` style bits, colors and underline color. Interned per
/// page so a cell carries a 16-bit id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline: Option<Color>,
    /// `Flags` bits that are style (never WRAPLINE, WIDE*, SPACER or marks).
    pub flags: u32,
}

/// The `Flags` bits a [`Style`] carries.
pub const STYLE_FLAGS: u32 = Flags::INVERSE.bits()
    | Flags::BOLD.bits()
    | Flags::ITALIC.bits()
    | Flags::DIM.bits()
    | Flags::HIDDEN.bits()
    | Flags::STRIKEOUT.bits()
    | Flags::ALL_UNDERLINES.bits()
    | Flags::BLINK.bits()
    | Flags::OVERLINE.bits();

impl Default for Style {
    fn default() -> Self {
        Style {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            underline: None,
            flags: 0,
        }
    }
}

fn color_key(color: Color) -> u32 {
    match color {
        Color::Named(named) => 1 << 24 | named as u32,
        Color::Spec(rgb) => 2 << 24 | (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32,
        Color::Indexed(index) => 3 << 24 | index as u32,
    }
}

impl std::hash::Hash for Style {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (
            color_key(self.fg),
            color_key(self.bg),
            self.underline.map(color_key),
            self.flags,
        )
            .hash(state);
    }
}

impl Style {
    /// The style of a cell erased under this pen: background only.
    pub fn erased(&self) -> Style {
        Style {
            bg: self.bg,
            ..Style::default()
        }
    }
}
