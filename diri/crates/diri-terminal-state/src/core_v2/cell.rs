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
    #[cfg(target_arch = "aarch64")]
    {
        // Sixteen cells per step, the last step overlapping the one before
        // (the overlap is written twice with the same values); eight at a
        // time below sixteen.
        if n >= 16 {
            let mut k = 0;
            while k + 16 <= n {
                // SAFETY: `k + 16 <= n`.
                unsafe { neon::fill16(cells.as_mut_ptr().add(k), src.as_ptr().add(k), high) };
                k += 16;
            }
            if k < n {
                // SAFETY: `n >= 16`.
                unsafe {
                    neon::fill16(
                        cells.as_mut_ptr().add(n - 16),
                        src.as_ptr().add(n - 16),
                        high,
                    )
                };
            }
            return;
        }
        if n >= 8 {
            // SAFETY: `8 <= n`, so both windows are in bounds.
            unsafe {
                neon::fill8(cells.as_mut_ptr(), src.as_ptr(), high);
                neon::fill8(cells.as_mut_ptr().add(n - 8), src.as_ptr().add(n - 8), high);
            }
            return;
        }
    }
    for (cell, &byte) in cells[..n].iter_mut().zip(&src[..n]) {
        cell.0 = byte as u64 | high;
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::Cell;
    use std::arch::aarch64::*;

    /// Table indices that spread two source bytes into two u64 lanes (an
    /// index past the table reads as zero).
    const SPREAD: [[u8; 16]; 8] = {
        let mut t = [[0xFFu8; 16]; 8];
        let mut j = 0;
        while j < 8 {
            t[j][0] = 2 * j as u8;
            t[j][8] = 2 * j as u8 + 1;
            j += 1;
        }
        t
    };

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
            for (j, spread) in SPREAD.iter().enumerate() {
                let lanes = vreinterpretq_u64_u8(vqtbl1q_u8(v, vld1q_u8(spread.as_ptr())));
                vst1q_u64(dst.add(2 * j), vorrq_u64(lanes, fill));
            }
        }
    }

    /// Eight cells from eight bytes.
    ///
    /// # Safety
    /// `dst` has room for eight cells and `src` holds eight bytes.
    #[inline(always)]
    pub unsafe fn fill8(dst: *mut Cell, src: *const u8, high: u64) {
        // SAFETY: as `fill16`, over eight bytes.
        unsafe {
            let v = vcombine_u8(vld1_u8(src), vdup_n_u8(0));
            let fill = vdupq_n_u64(high);
            let dst = dst as *mut u64;
            for (j, spread) in SPREAD[..4].iter().enumerate() {
                let lanes = vreinterpretq_u64_u8(vqtbl1q_u8(v, vld1q_u8(spread.as_ptr())));
                vst1q_u64(dst.add(2 * j), vorrq_u64(lanes, fill));
            }
        }
    }
}

/// `cells.fill(cell)`, sixteen bytes per store.
#[inline(always)]
pub fn fill_cells(cells: &mut [Cell], cell: Cell) {
    let mut pairs = cells.chunks_exact_mut(2);
    for pair in &mut pairs {
        // SAFETY: two cells are sixteen bytes in bounds; `Cell` is a
        // transparent u64 with no invalid bit patterns.
        unsafe { (pair.as_mut_ptr() as *mut [u64; 2]).write_unaligned([cell.0, cell.0]) };
    }
    for c in pairs.into_remainder() {
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
