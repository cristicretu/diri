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
/// | 31     | soft-wrap mark moved off the last column                 |
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
/// A soft-wrap mark that left the last column (DCH moved the cell); the
/// last column's mark is `RowMeta::wrapped`.
pub const WRAP: u64 = 1 << 31;
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
    /// `*self == Style::default()`, without comparing whole colors.
    #[inline(always)]
    pub fn is_default(&self) -> bool {
        matches!(self.fg, Color::Named(NamedColor::Foreground))
            && matches!(self.bg, Color::Named(NamedColor::Background))
            && self.underline.is_none()
            && self.flags == 0
    }

    /// The style of a cell erased under this pen: background only.
    pub fn erased(&self) -> Style {
        Style {
            bg: self.bg,
            ..Style::default()
        }
    }
}

/// Writes `cells[k] = src[k] | high` for every `k` (printable ASCII into
/// fresh cells; `high` carries the style). `cells` and `src` have the same
/// length.
#[inline(always)]
pub fn fill_bytes(cells: &mut [Cell], src: &[u8], high: u64) {
    debug_assert_eq!(cells.len(), src.len());
    let n = src.len().min(cells.len());
    // Whole sixteens with the vectorised loop; a remainder after at least
    // sixteen as one overlapping NEON window (the overlap is written twice
    // with the same values) instead of a scalar tail.
    let body = if cfg!(target_arch = "aarch64") && n > 16 {
        n & !15
    } else {
        n
    };
    for (cell, &byte) in cells[..body].iter_mut().zip(&src[..body]) {
        cell.0 = byte as u64 | high;
    }
    #[cfg(target_arch = "aarch64")]
    if body < n {
        // SAFETY: `n > 16`, so the window `n - 16..n` is in bounds.
        unsafe {
            neon::fill16(
                cells.as_mut_ptr().add(n - 16),
                src.as_ptr().add(n - 16),
                high,
            )
        };
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::Cell;
    use std::arch::aarch64::*;

    /// Sixteen cells from sixteen bytes.
    ///
    /// # Safety
    /// `dst` has room for sixteen cells and `src` holds sixteen bytes.
    #[inline(always)]
    pub unsafe fn fill16(dst: *mut Cell, src: *const u8, high: u64) {
        // SAFETY: NEON is baseline on aarch64; the caller bounds the
        // pointers.
        unsafe {
            let v = vld1q_u8(src);
            let fill = vdupq_n_u64(high);
            let dst = dst as *mut u64;
            // Widen u8 -> u16 -> u32 -> u64 by zipping with zero.
            let z8 = vdupq_n_u8(0);
            let (h0, h1) = (
                vreinterpretq_u16_u8(vzip1q_u8(v, z8)),
                vreinterpretq_u16_u8(vzip2q_u8(v, z8)),
            );
            let z16 = vdupq_n_u16(0);
            let words = [
                vreinterpretq_u32_u16(vzip1q_u16(h0, z16)),
                vreinterpretq_u32_u16(vzip2q_u16(h0, z16)),
                vreinterpretq_u32_u16(vzip1q_u16(h1, z16)),
                vreinterpretq_u32_u16(vzip2q_u16(h1, z16)),
            ];
            let z32 = vdupq_n_u32(0);
            for (j, w) in words.into_iter().enumerate() {
                let lo = vorrq_u64(vreinterpretq_u64_u32(vzip1q_u32(w, z32)), fill);
                let hi = vorrq_u64(vreinterpretq_u64_u32(vzip2q_u32(w, z32)), fill);
                vst1q_u64(dst.add(4 * j), lo);
                vst1q_u64(dst.add(4 * j + 2), hi);
            }
        }
    }
}

/// `cells.fill(cell)`, sixteen bytes per store.
#[inline(always)]
pub fn fill_cells(cells: &mut [Cell], cell: Cell) {
    let (pairs, rest) = cells.as_chunks_mut::<2>();
    for pair in pairs {
        // SAFETY: two cells are sixteen bytes in bounds; `Cell` is a
        // transparent u64 with no invalid bit patterns.
        unsafe { (pair.as_mut_ptr() as *mut [u64; 2]).write_unaligned([cell.0, cell.0]) };
    }
    for c in rest {
        *c = cell;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_bytes_writes_every_length() {
        let src: Vec<u8> = (0..64u8).map(|b| b' ' + b).collect();
        for n in 0..=40 {
            for start in 0..3 {
                let mut cells = vec![Cell(0xDEAD); n + 6];
                fill_bytes(&mut cells[start..start + n], &src[..n], 7 << STYLE_SHIFT);
                for (k, cell) in cells.iter().enumerate() {
                    let want = if (start..start + n).contains(&k) {
                        src[k - start] as u64 | 7 << STYLE_SHIFT
                    } else {
                        0xDEAD
                    };
                    assert_eq!(cell.0, want, "n {n} start {start} cell {k}");
                }
            }
        }
    }

    #[test]
    fn fill_cells_writes_every_length() {
        for n in 0..20 {
            let mut cells = vec![Cell(1); n + 2];
            fill_cells(&mut cells[1..1 + n], Cell(9));
            assert_eq!(cells[0], Cell(1));
            assert!(cells[1..1 + n].iter().all(|c| *c == Cell(9)));
            assert_eq!(cells[n + 1], Cell(1));
        }
    }
}
