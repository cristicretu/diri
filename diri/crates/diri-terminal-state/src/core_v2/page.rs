//! Pages: fixed-size blocks of rows holding both the screen and history.
//!
//! The screen is the newest `lines` rows of the page list. A linefeed at the
//! bottom of a full-screen scroll region appends one row (no cell is
//! touched) and the oldest screen row becomes history by definition, so
//! scrolling into history costs nothing.
//!
//! With a history budget, a page whose rows are all older than the newest
//! history row is packed into a compact byte stream (see [`Page::pack`]) and
//! its cell buffer is reused for the next tail page. History is then bounded
//! by live bytes: packed pages plus the dense pages that still hold history
//! rows, counted at their full size. Both sealing and eviction are driven by
//! the row count alone, so the history kept is the same whatever the read
//! sizes. Without a budget, whole pages past the row limit are recycled as
//! the next tail page.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use alacritty_terminal::term::grapheme::Info;

use super::cell::{
    CLUSTER, CLUSTER_SHIFT, CP, Cell, GRAPHEME, HYPERLINK, LEADING, SPACER, STYLE_SHIFT, Style,
    WIDE,
};

/// Per-row facts. Cells at or past `occ` were never written since the row
/// was appended or wholly erased: they read as blanks of style `fill`, and
/// whatever bytes they hold are garbage from a previous use of the slot.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub occ: u16,
    pub fill: u16,
    /// Cells below `lead` are blanks of `fill` (a lower bound; 0 is always
    /// true).
    pub lead: u16,
    pub wrapped: bool,
    pub dirty: bool,
    /// Every cell below `occ` was written by the text paths in the fill
    /// style: blanks, tabs, printable ASCII, characters with their own
    /// width (two-column ones followed by their spacer) and leading
    /// spacers, with no marks or clusters. Such a row packs as text.
    pub simple: bool,
}

#[derive(Default)]
pub struct StyleTable {
    table: Vec<Style>,
    /// Boxed: packed pages drop it, and a page's size counts in the budget
    /// (a bare `HashMap` adds 40 bytes to every packed page).
    #[allow(clippy::box_collection)]
    index: Option<Box<HashMap<Style, u16>>>,
}

impl StyleTable {
    #[inline(always)]
    pub fn intern(&mut self, style: &Style) -> u16 {
        debug_assert_eq!(style.is_default(), *style == Style::default());
        if style.is_default() {
            return 0;
        }
        self.intern_other(style)
    }

    #[inline(never)]
    fn intern_other(&mut self, style: &Style) -> u16 {
        if self.table.is_empty() {
            self.table.push(Style::default());
        }
        let index = self.index.get_or_insert_with(Default::default);
        if let Some(&id) = index.get(style) {
            return id;
        }
        // Callers intern only for a cell or row fill they write, so a page
        // holds at most rows * (cols + 1) styles, far below 65,535.
        let id = self.table.len() as u16;
        self.table.push(*style);
        index.insert(*style, id);
        id
    }

    #[inline]
    pub fn get(&self, id: u16) -> Style {
        if id == 0 {
            Style::default()
        } else {
            self.table[id as usize]
        }
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    fn clear(&mut self) {
        self.table.clear();
        if let Some(index) = &mut self.index {
            index.clear();
        }
    }

    /// A packed page never interns again: keep only the table.
    fn freeze(&mut self) {
        self.index = None;
        self.table.shrink_to_fit();
    }

    fn heap_bytes(&self) -> usize {
        self.table.capacity() * std::mem::size_of::<Style>()
    }
}

/// The extra codepoints of each cluster in a page: a cell with GRAPHEME
/// names its span by the index in its CLUSTER bits. Overwritten clusters
/// leave dead spans; the arena is compacted when it fills.
#[derive(Default)]
pub struct Clusters {
    spans: Vec<(u32, u32)>,
    chars: Vec<char>,
}

impl Clusters {
    #[inline]
    pub fn of(&self, cell: Cell) -> &[char] {
        if !cell.has(GRAPHEME) {
            return &[];
        }
        let (start, len) = self.spans[(cell.0 >> CLUSTER_SHIFT) as usize];
        &self.chars[start as usize..(start + len) as usize]
    }

    fn clear(&mut self) {
        self.spans.clear();
        self.chars.clear();
    }

    /// Empties the arena for the next page, keeping a small one's
    /// capacity (a page of marks takes a few KiB) and freeing a large one.
    fn reuse(&mut self) {
        const KEEP: usize = 16 << 10;
        if self.spans.capacity() * 8 + self.chars.capacity() * 4 > KEEP {
            *self = Clusters::default();
        } else {
            self.clear();
        }
    }

    /// A new cluster holding `chars`; returns its index.
    pub fn add(&mut self, chars: &[char]) -> u64 {
        let index = self.spans.len() as u64;
        self.spans
            .push((self.chars.len() as u32, chars.len() as u32));
        self.chars.extend_from_slice(chars);
        index
    }

    /// Appends `c` to cluster `index`, moving it to the end of the arena
    /// unless it is already there.
    fn push(&mut self, index: usize, c: char) {
        let (start, len) = self.spans[index];
        if (start + len) as usize != self.chars.len() {
            let moved = self.chars.len() as u32;
            self.chars
                .extend_from_within(start as usize..(start + len) as usize);
            self.spans[index].0 = moved;
        }
        self.chars.push(c);
        self.spans[index].1 += 1;
    }

    pub fn full(&self) -> bool {
        self.spans.len() >= 0xFFFF || self.chars.len() >= 1 << 20
    }
}

/// Text-row tokens (see [`pack_text_row`]) of the rows the text paths are
/// writing left to right, kept as they write so that packing a page copies
/// them instead of reading its cells back. A row's tokens cover its cells
/// from `lead` to `occ`; any other write invalidates them, and the row is
/// then packed from its cells.
pub struct TextLog {
    pub on: bool,
    bytes: Vec<u8>,
    /// Per row: start and length in `bytes`; start `INVALID` when the row
    /// has no tokens.
    spans: Box<[(u32, u32)]>,
}

const INVALID: u32 = u32::MAX;

impl TextLog {
    fn new(rows: usize, on: bool) -> TextLog {
        TextLog {
            on,
            bytes: Vec::new(),
            spans: vec![(INVALID, 0); rows].into_boxed_slice(),
        }
    }

    /// Row `row` starts empty (appended, or wholly erased).
    #[inline]
    pub fn reset(&mut self, row: usize) {
        if !self.on {
            return;
        }
        // A row rewritten in place forever (a progress line) must not grow
        // the arena without bound: past a limit, every row reverts to
        // packing from cells.
        if self.bytes.len() > self.spans.len() * 1024 {
            self.bytes.clear();
            self.spans.fill((INVALID, 0));
        }
        self.spans[row] = (self.bytes.len() as u32, 0);
    }

    /// Whether `row` can take more tokens: valid and last in the arena.
    #[inline(always)]
    pub fn open(&self, row: usize) -> bool {
        let (start, len) = self.spans[row];
        self.on && start != INVALID && (start + len) as usize == self.bytes.len()
    }

    /// Whether a write at `col` on `row` (which holds `occ` cells, so
    /// `col >= occ`) can extend its tokens. An empty row starts over at the
    /// arena's end with its text-row header, whose lead is `col`.
    #[inline(always)]
    pub fn begin(&mut self, row: usize, occ: usize, col: usize) -> bool {
        if !self.on {
            return false;
        }
        if occ == 0 {
            let start = self.bytes.len() as u32;
            if col < 255 {
                self.bytes.extend_from_slice(&[T_TEXT_ROW, col as u8]);
                self.spans[row] = (start, 2);
            } else {
                let lead = (col as u16).to_le_bytes();
                self.bytes
                    .extend_from_slice(&[T_TEXT_ROW, 255, lead[0], lead[1]]);
                self.spans[row] = (start, 4);
            }
            return true;
        }
        self.open(row)
    }

    /// Whether the rows with cells hold valid tokens that tile the arena
    /// in row order.
    fn contiguous(&self, meta: &[RowMeta]) -> bool {
        let mut at = 0u32;
        for (row, meta) in meta.iter().enumerate() {
            if meta.occ == 0 {
                continue;
            }
            let (start, len) = self.spans[row];
            if start != at {
                return false;
            }
            at += len;
        }
        at as usize == self.bytes.len()
    }

    #[inline(always)]
    pub fn invalidate(&mut self, row: usize) {
        self.spans[row].0 = INVALID;
    }

    #[inline(always)]
    pub fn push(&mut self, row: usize, byte: u8) {
        self.bytes.push(byte);
        self.spans[row].1 += 1;
    }

    #[inline(always)]
    pub fn extend(&mut self, row: usize, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        self.spans[row].1 += bytes.len() as u32;
    }

    /// `n` blank cells.
    #[inline]
    pub fn blanks(&mut self, row: usize, mut n: usize) {
        while n >= 3 {
            let run = n.min(255);
            self.extend(row, &[T_BLANKS, run as u8]);
            n -= run;
        }
        for _ in 0..n {
            self.push(row, 0);
        }
    }

    /// A character's token: its two-byte CJK code, or `utf8` (its UTF-8).
    #[inline(always)]
    pub fn char(&mut self, row: usize, cp: u64, utf8: &[u8]) {
        if (CJK_BASE..CJK_END).contains(&cp) {
            let index = cp - CJK_BASE;
            self.extend(row, &[CJK_LEADS[(index >> 8) as usize], index as u8]);
        } else {
            self.small(row, utf8);
        }
    }

    /// One to four bytes, without a call to `memcpy`.
    #[inline(always)]
    fn small(&mut self, row: usize, bytes: &[u8]) {
        debug_assert!((1..=4).contains(&bytes.len()));
        self.bytes.reserve(4);
        let len = self.bytes.len();
        // SAFETY: four bytes are reserved past `len`; only `bytes.len()` of
        // them become part of the vector.
        let mut word = 0u32;
        for (k, &b) in bytes.iter().enumerate() {
            word |= (b as u32) << (8 * k);
        }
        unsafe {
            (self.bytes.as_mut_ptr().add(len) as *mut u32).write_unaligned(word.to_le());
            self.bytes.set_len(len + bytes.len());
        }
        self.spans[row].1 += bytes.len() as u32;
    }

    // ---- the text path's writers ----
    //
    // `write_text_segment` reserves room for every cell it may still write
    // ([`TextLog::reserve_cells`]), writes tokens without capacity checks,
    // and adds the bytes to the row once ([`TextLog::add_to_row`]). A
    // character takes at most four bytes per cell; a mark reserves again.

    /// Room for the tokens of `cells` more cells (four bytes each).
    #[inline(always)]
    pub fn reserve_cells(&mut self, cells: usize) {
        self.bytes.reserve(4 * cells + 32);
    }

    /// Bytes in the arena.
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the arena is empty.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Counts the `bytes` written since the arena held `from` towards `row`.
    #[inline(always)]
    pub fn add_to_row(&mut self, row: usize, from: usize) {
        self.spans[row].1 += (self.bytes.len() - from) as u32;
    }

    /// A character's token (see [`TextLog::char`]) for the character
    /// whose UTF-8 starts `utf8` (which may run on past it), covering one
    /// or two cells.
    ///
    /// # Safety
    /// [`TextLog::reserve_cells`] covered the cells written since.
    #[inline(always)]
    pub unsafe fn put_char(&mut self, cp: u64, utf8: &[u8]) {
        debug_assert!(self.bytes.capacity() - self.bytes.len() >= 4);
        let len = self.bytes.len();
        let index = cp.wrapping_sub(CJK_BASE);
        let (word, n) = if index < CJK_END - CJK_BASE {
            (
                CJK_LEADS[(index >> 8) as usize] as u32 | (index as u32 & 0xFF) << 8,
                2,
            )
        } else if let Some(&word) = utf8.first_chunk::<4>() {
            // The bytes after the character are written past it and
            // overwritten later (or left past the vector's end).
            (u32::from_le_bytes(word), len_of(utf8[0]))
        } else {
            let n = len_of(utf8[0]);
            let mut word = 0u32;
            for (k, &b) in utf8[..n].iter().enumerate() {
                word |= (b as u32) << (8 * k);
            }
            (word, n)
        };
        // SAFETY: four bytes of room (the caller's reservation); `n` of
        // them join the vector.
        unsafe {
            (self.bytes.as_mut_ptr().add(len) as *mut u32).write_unaligned(word.to_le());
            self.bytes.set_len(len + n);
        }
    }

    /// Tokens of `cps`, two-column three-byte characters whose UTF-8 is
    /// `utf8`.
    ///
    /// # Safety
    /// As [`TextLog::put_char`]: their `2 * cps.len()` cells are reserved.
    #[inline(always)]
    pub unsafe fn put_chars3(&mut self, cps: &[u32; 16], n: usize, utf8: &[u8]) {
        debug_assert!(n <= 16 && utf8.len() >= 3 * n);
        let len = self.bytes.len();
        debug_assert!(self.bytes.capacity() - len >= 8 * n);
        // SAFETY: 8 * n bytes of room; at most 3 * n are written, or 32
        // when n >= 4.
        let ptr = unsafe { self.bytes.as_mut_ptr().add(len) };
        #[cfg(target_arch = "aarch64")]
        if n >= 4
            && let Some(()) = unsafe { cjk_codes16(cps, ptr) }
        {
            // SAFETY: 2 * n of the 32 bytes written are tokens.
            unsafe { self.bytes.set_len(len + 2 * n) };
            return;
        }
        let mut w = 0;
        for (k, &cp) in cps[..n].iter().enumerate() {
            let index = (cp as u64).wrapping_sub(CJK_BASE);
            unsafe {
                if index < CJK_END - CJK_BASE {
                    ptr.add(w).write(CJK_LEADS[(index >> 8) as usize]);
                    ptr.add(w + 1).write(index as u8);
                    w += 2;
                } else {
                    std::ptr::copy_nonoverlapping(utf8.as_ptr().add(3 * k), ptr.add(w), 3);
                    w += 3;
                }
            }
        }
        // SAFETY: `w` bytes were written within the reserved capacity.
        unsafe { self.bytes.set_len(len + w) };
    }

    /// A zero-width mark (its UTF-8) on the cell before; keeps room for
    /// `cells` more cells.
    #[inline]
    pub fn put_mark(&mut self, utf8: &[u8], cells: usize) {
        self.bytes.push(T_MARK);
        self.bytes.extend_from_slice(utf8);
        self.reserve_cells(cells);
    }

    pub fn leading(&mut self, row: usize) {
        self.push(row, T_LEADING);
    }

    fn get(&self, row: usize) -> Option<&[u8]> {
        let (start, len) = self.spans[row];
        (self.on && start != INVALID).then(|| &self.bytes[start as usize..(start + len) as usize])
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.spans.fill((INVALID, 0));
    }
}

/// The length of the UTF-8 sequence led by valid lead byte `b0`.
#[inline(always)]
fn len_of(b0: u8) -> usize {
    1 + (b0 >= 0xC0) as usize + (b0 >= 0xE0) as usize + (b0 >= 0xF0) as usize
}

/// Writes the two-byte codes of sixteen characters, all of them from
/// U+4E00 on (see [`CJK_LEADS`]), as 32 bytes at `out`; `None` (writing
/// nothing) when any is outside that range.
///
/// # Safety
/// `out` has room for 32 bytes.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn cjk_codes16(cps: &[u32; 16], out: *mut u8) -> Option<()> {
    use std::arch::aarch64::*;
    // SAFETY: NEON is baseline on aarch64; the loads read `cps` and the
    // lead table, the store writes the caller's 32 bytes.
    unsafe {
        let base = vdupq_n_u32(CJK_BASE as u32);
        let span = vdupq_n_u32((CJK_END - CJK_BASE) as u32);
        let p = cps.as_ptr();
        let (a, b, c, d) = (
            vsubq_u32(vld1q_u32(p), base),
            vsubq_u32(vld1q_u32(p.add(4)), base),
            vsubq_u32(vld1q_u32(p.add(8)), base),
            vsubq_u32(vld1q_u32(p.add(12)), base),
        );
        let inside = vandq_u32(
            vandq_u32(vcltq_u32(a, span), vcltq_u32(b, span)),
            vandq_u32(vcltq_u32(c, span), vcltq_u32(d, span)),
        );
        if vminvq_u32(inside) == 0 {
            return None;
        }
        let narrow = |x: uint32x4_t, y: uint32x4_t| vcombine_u16(vmovn_u32(x), vmovn_u32(y));
        let (lo16, hi16) = (narrow(a, b), narrow(c, d));
        let low = vcombine_u8(vmovn_u16(lo16), vmovn_u16(hi16));
        let high = vcombine_u8(vshrn_n_u16::<8>(lo16), vshrn_n_u16::<8>(hi16));
        let t = CJK_LEADS_PADDED.as_ptr();
        let first = uint8x16x4_t(
            vld1q_u8(t),
            vld1q_u8(t.add(16)),
            vld1q_u8(t.add(32)),
            vld1q_u8(t.add(48)),
        );
        let second = uint8x16x3_t(
            vld1q_u8(t.add(64)),
            vld1q_u8(t.add(80)),
            vld1q_u8(t.add(96)),
        );
        // Indices 0..64 read the first table, 64..100 the second (an
        // index past a table reads as zero).
        let lead = vorrq_u8(
            vqtbl4q_u8(first, high),
            vqtbl3q_u8(second, vsubq_u8(high, vdupq_n_u8(64))),
        );
        vst2q_u8(out, uint8x16x2_t(lead, low));
        Some(())
    }
}

/// [`CJK_LEADS`] padded to 112 bytes for table lookups.
#[cfg(target_arch = "aarch64")]
const CJK_LEADS_PADDED: [u8; 112] = {
    let mut t = [0u8; 112];
    let mut i = 0;
    while i < 100 {
        t[i] = CJK_LEADS[i];
        i += 1;
    }
    t
};

/// An OSC 8 hyperlink: an explicit id, if the program gave one, and the URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub id: Option<String>,
    pub uri: String,
}

/// The link of each cell with HYPERLINK, by `row * cols + col`, as an index
/// into `table`. Entries of overwritten cells are stale and ignored (the
/// bit decides).
#[derive(Default)]
struct Links {
    map: HashMap<u32, u32>,
    table: Vec<Link>,
}

pub struct Page {
    /// Changes whenever the page is (re)initialised, so a pen can cache
    /// its interned id per page.
    pub serial: u64,
    pub len: usize,
    /// Empty once the page is packed.
    pub cells: Box<[Cell]>,
    /// Empty once the page is packed: the packed bytes start with each
    /// row's metadata (see [`Page::pack`]); read it with [`Pages::meta`].
    pub meta: Box<[RowMeta]>,
    pub styles: StyleTable,
    pub clusters: Clusters,
    pub text: TextLog,
    /// Hyperlinks, for pages that have any (boxed: a page's size counts in
    /// the history budget).
    links: Option<Box<Links>>,
    packed: Option<Box<[u8]>>,
}

const T_LITERAL: u8 = 1;
const T_STYLE: u8 = 2;
const T_CLUSTER: u8 = 3;
const T_BLANKS: u8 = 4;
/// A leading spacer: a blank with LEADING, before a wide character that
/// wrapped to the next row.
const T_LEADING: u8 = 5;
/// In a text row: a zero-width mark (its UTF-8 follows) joining the cluster
/// of the cell before (its character, when that is a spacer). 0xC1 starts
/// no other token, in text or general rows (never UTF-8, not a CJK lead).
const T_MARK: u8 = 0xC1;
/// The next cell has a hyperlink: u16 index into the page's link table.
/// (0xC0 never starts UTF-8.)
const T_LINK: u8 = 0xC0;
/// A row packed as text (see [`Page::pack_text_row`]).
const T_TEXT_ROW: u8 = 0x7F;
const FLAGS: u64 = 0x7FF << 21;
const STYLE_BITS: u64 = 0xFFFF << STYLE_SHIFT;

/// Two-byte codes for U+4E00.. (CJK ideographs, Yi, most Hangul
/// syllables): a first byte that cannot start any other token (UTF-8
/// continuation bytes, bytes past 0xF4, unused controls) gives the high
/// byte of `cp - CJK_BASE`, the second byte the low byte.
const CJK_BASE: u64 = 0x4E00;
const CJK_LEADS: [u8; 100] = {
    let mut leads = [0u8; 100];
    let mut n = 0;
    let mut b = 0x06u8;
    while n < 100 {
        let free = matches!(b, 0x06..=0x08 | 0x0A..=0x1F | 0x80..=0xBF | 0xF5..=0xFF);
        if free {
            leads[n] = b;
            n += 1;
        }
        b = b.wrapping_add(1);
    }
    leads
};
const CJK_HIGH: [u8; 256] = {
    let mut high = [0xFFu8; 256];
    let mut i = 0;
    while i < 100 {
        high[CJK_LEADS[i] as usize] = i as u8;
        i += 1;
    }
    high
};
const CJK_END: u64 = CJK_BASE + 100 * 256;

#[inline]
fn wide(cp: u64) -> bool {
    char::from_u32(cp as u32).is_some_and(|c| Info::of(c).width() == 2)
}

#[inline]
fn push_char(out: &mut Vec<u8>, cp: u64) {
    if (CJK_BASE..CJK_END).contains(&cp) {
        let index = cp - CJK_BASE;
        out.push(CJK_LEADS[(index >> 8) as usize]);
        out.push(index as u8);
        return;
    }
    push_utf8(out, cp);
}

fn push_utf8(out: &mut Vec<u8>, cp: u64) {
    let c = char::from_u32(cp as u32).expect("cells hold scalars");
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn read_utf8(data: &[u8], at: &mut usize) -> u64 {
    let len = match data[*at] {
        0..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    let c = std::str::from_utf8(&data[*at..*at + len])
        .expect("packed by pack()")
        .chars()
        .next()
        .expect("one char");
    *at += len;
    c as u64
}

/// The low bytes of eight cells when all are plain cells (no flags, no
/// cluster) below U+0080 in style `current` (blanks give 0), with a byte
/// mask of the printable ones (tab, space..tilde) and of the blanks.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn narrow8(cells: &[Cell], current: u64) -> Option<(u64, u64, u64)> {
    use std::arch::aarch64::*;
    debug_assert!(cells.len() >= 8);
    // SAFETY: NEON is baseline on aarch64; eight cells are in bounds.
    unsafe {
        let p = cells.as_ptr() as *const u64;
        let mask = vdupq_n_u64(!0x7F);
        let want = vdupq_n_u64(current);
        let (a, b, c, d) = (
            vld1q_u64(p),
            vld1q_u64(p.add(2)),
            vld1q_u64(p.add(4)),
            vld1q_u64(p.add(6)),
        );
        let same = vandq_u64(
            vandq_u64(
                vceqq_u64(vandq_u64(a, mask), want),
                vceqq_u64(vandq_u64(b, mask), want),
            ),
            vandq_u64(
                vceqq_u64(vandq_u64(c, mask), want),
                vceqq_u64(vandq_u64(d, mask), want),
            ),
        );
        if vminvq_u32(vreinterpretq_u32_u64(same)) == 0 {
            return None;
        }
        let low = vcombine_u32(vmovn_u64(a), vmovn_u64(b));
        let high = vcombine_u32(vmovn_u64(c), vmovn_u64(d));
        let bytes = vmovn_u16(vcombine_u16(vmovn_u32(low), vmovn_u32(high)));
        let plain = vorr_u8(
            vceq_u8(bytes, vdup_n_u8(9)),
            vand_u8(
                vcge_u8(bytes, vdup_n_u8(0x20)),
                vclt_u8(bytes, vdup_n_u8(0x7F)),
            ),
        );
        let blank = vceq_u8(bytes, vdup_n_u8(0));
        let word = |v: uint8x8_t| vget_lane_u64::<0>(vreinterpret_u64_u8(v));
        Some((word(bytes), word(plain), word(blank)))
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn narrow8(cells: &[Cell], current: u64) -> Option<(u64, u64, u64)> {
    let (mut out, mut plain, mut blank) = (0u64, 0u64, 0u64);
    for (i, c) in cells[..8].iter().enumerate() {
        if c.0 & !0x7F != current {
            return None;
        }
        let b = c.0 & 0x7F;
        out |= b << (8 * i);
        if b == 9 || (0x20..0x7F).contains(&b) {
            plain |= 0xFF << (8 * i);
        }
        if b == 0 {
            blank |= 0xFF << (8 * i);
        }
    }
    Some((out, plain, blank))
}

/// Writes `blanks` pending blank cells of the current style.
#[inline]
fn flush_blanks(out: &mut Vec<u8>, blanks: &mut usize) {
    while *blanks >= 3 {
        let n = (*blanks).min(255);
        out.push(T_BLANKS);
        out.push(n as u8);
        *blanks -= n;
    }
    for _ in 0..*blanks {
        out.push(0);
    }
    *blanks = 0;
}

/// Packs a row the text paths wrote (`RowMeta::simple`, a hint that this
/// checks) as `T_TEXT_ROW`, `lead` (one byte, or 255 and a u16), then from
/// `lead` on: `T_BLANKS n` or 0 for blanks, a tab or printable ASCII byte,
/// `T_LEADING`, or a character (two-byte CJK code or UTF-8), two columns
/// when its width is two. Every cell must be in the fill style with no
/// marks or clusters, and a two-column character must be followed by its
/// spacer; otherwise this writes nothing and returns false, and the row
/// takes the general tokens.
#[inline]
fn pack_text_row(cells: &[Cell], meta: &RowMeta, out: &mut Vec<u8>) -> bool {
    let fill = (meta.fill as u64) << STYLE_SHIFT;
    let occ = cells.len();
    let lead = (meta.lead as usize).min(occ);
    // At most four bytes per cell (UTF-8), plus the header and a blank run.
    out.reserve(8 + (occ - lead) * 4);
    let start = out.len();
    let mut w = Writer {
        // SAFETY: `Writer` stays within the capacity reserved above: each
        // cell adds at most four bytes and a blank run at most two.
        ptr: unsafe { out.as_mut_ptr().add(start) },
        len: 0,
        blanks: 0,
    };
    w.put(T_TEXT_ROW);
    if lead < 255 {
        w.put(lead as u8);
    } else {
        w.put(255);
        w.put(lead as u8);
        w.put((lead >> 8) as u8);
    }
    let spacer = 0x20 | SPACER | fill;
    let mut x = lead;
    while x < occ {
        if x + 8 <= occ
            && let Some((word, plain, blank)) = narrow8(&cells[x..x + 8], fill)
        {
            if plain == u64::MAX {
                w.flush();
                w.put8(word);
                x += 8;
                continue;
            }
            if blank == u64::MAX {
                w.blanks += 8;
                x += 8;
                continue;
            }
            if plain | blank == u64::MAX {
                for byte in word.to_le_bytes() {
                    if byte == 0 {
                        w.blanks += 1;
                    } else {
                        w.flush();
                        w.put(byte);
                    }
                }
                x += 8;
                continue;
            }
        }
        let c = cells[x].0;
        let cp = c & CP;
        if c & !CP == WIDE | fill {
            // A run of two-column CJK characters with their spacers.
            let glyph = WIDE | fill;
            let mut y = x;
            while y + 1 < occ {
                let (c, next) = (cells[y].0, cells[y + 1].0);
                let index = (c & CP).wrapping_sub(CJK_BASE);
                if c & !CP != glyph || next != spacer || index >= CJK_END - CJK_BASE {
                    break;
                }
                if y == x {
                    w.flush();
                }
                w.put(CJK_LEADS[(index >> 8) as usize]);
                w.put(index as u8);
                y += 2;
            }
            if y > x {
                x = y;
                continue;
            }
        }
        if c == fill {
            w.blanks += 1;
            x += 1;
            continue;
        }
        if c & !(CP | WIDE | LEADING) != fill {
            break;
        }
        w.flush();
        if c & LEADING != 0 {
            if c != 0x20 | LEADING | fill {
                break;
            }
            w.put(T_LEADING);
            x += 1;
        } else if c & WIDE != 0 {
            if cp < 0x80 || x + 1 >= occ || cells[x + 1].0 != spacer {
                break;
            }
            w.put_char(cp);
            x += 2;
        } else if cp == 9 || (0x20..0x7F).contains(&cp) {
            w.put(cp as u8);
            x += 1;
        } else if cp >= 0xA0 {
            w.put_char(cp);
            x += 1;
        } else {
            break;
        }
    }
    if x < occ {
        return false;
    }
    w.flush();
    // SAFETY: `w.len` bytes were written after `start`, within capacity.
    unsafe { out.set_len(start + w.len) };
    true
}

/// Decodes text-row tokens at `data[*at..]` into `cells`, the occupied
/// cells of a row whose first `lead` are blanks of style `fill`, adding
/// marks to `clusters`.
fn decode_text_row(
    data: &[u8],
    at: &mut usize,
    fill: u16,
    lead: usize,
    cells: &mut [Cell],
    clusters: &mut Clusters,
) {
    let fill = (fill as u64) << STYLE_SHIFT;
    let occ = cells.len();
    cells.fill(Cell(fill));
    let mut x = lead;
    // A mark may follow the row's last cell.
    while x < occ || data.get(*at) == Some(&T_MARK) {
        let byte = data[*at];
        *at += 1;
        let cp = match CJK_HIGH[byte as usize] {
            _ if byte == T_LEADING => {
                cells[x] = Cell(0x20 | LEADING | fill);
                x += 1;
                continue;
            }
            _ if byte == T_BLANKS => {
                let n = data[*at] as usize;
                *at += 1;
                x += n;
                continue;
            }
            _ if byte == T_MARK => {
                let c = char::from_u32(read_utf8(data, at) as u32).expect("a mark");
                let mut base = x - 1;
                if cells[base].has(SPACER) {
                    base -= 1;
                }
                push_mark(cells, clusters, base, c);
                continue;
            }
            0xFF if byte < 0x80 => byte as u64,
            0xFF => {
                *at -= 1;
                read_utf8(data, at)
            }
            high => {
                let cp = CJK_BASE + ((high as u64) << 8 | data[*at] as u64);
                *at += 1;
                cp
            }
        };
        if cp >= 0x80 && wide(cp) {
            cells[x] = Cell(cp | WIDE | fill);
            cells[x + 1] = Cell(0x20 | SPACER | fill);
            x += 2;
        } else {
            cells[x] = Cell(cp | fill);
            x += 1;
        }
    }
}

/// Adds `c` to the cluster of `cells[index]`, starting one when the cell
/// has none. The caller makes sure the arena is not [`Clusters::full`].
#[inline]
pub fn push_mark(cells: &mut [Cell], clusters: &mut Clusters, index: usize, c: char) {
    let cell = cells[index];
    if cell.has(GRAPHEME) {
        clusters.push((cell.0 >> CLUSTER_SHIFT) as usize, c);
    } else {
        let k = clusters.add(&[c]);
        cells[index] = Cell((cell.0 & !CLUSTER) | GRAPHEME | k << CLUSTER_SHIFT);
    }
}

/// Unchecked byte output into reserved capacity, with pending blanks.
struct Writer {
    ptr: *mut u8,
    len: usize,
    blanks: usize,
}

impl Writer {
    #[inline(always)]
    fn put(&mut self, byte: u8) {
        // SAFETY: the caller reserved room (see `pack_text_row`).
        unsafe { self.ptr.add(self.len).write(byte) };
        self.len += 1;
    }

    #[inline(always)]
    fn put8(&mut self, word: u64) {
        // SAFETY: as `put`; the destination may be unaligned.
        unsafe { (self.ptr.add(self.len) as *mut u64).write_unaligned(word.to_le()) };
        self.len += 8;
    }

    #[inline(always)]
    fn flush(&mut self) {
        if self.blanks == 0 {
            return;
        }
        while self.blanks >= 3 {
            let n = self.blanks.min(255);
            self.put(T_BLANKS);
            self.put(n as u8);
            self.blanks -= n;
        }
        for _ in 0..self.blanks {
            self.put(0);
        }
        self.blanks = 0;
    }

    #[inline]
    fn put_char(&mut self, cp: u64) {
        if (CJK_BASE..CJK_END).contains(&cp) {
            let index = cp - CJK_BASE;
            self.put(CJK_LEADS[(index >> 8) as usize]);
            self.put(index as u8);
            return;
        }
        let c = char::from_u32(cp as u32).expect("cells hold scalars");
        let mut buf = [0u8; 4];
        for &byte in c.encode_utf8(&mut buf).as_bytes() {
            self.put(byte);
        }
    }
}

#[inline]
fn push_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[inline]
fn read_varint(data: &[u8], at: &mut usize) -> usize {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let byte = data[*at];
        *at += 1;
        value |= ((byte & 0x7F) as usize) << shift;
        if byte < 0x80 {
            return value;
        }
        shift += 7;
    }
}

/// The metadata a packed row keeps, as two varints: `occ`, then
/// `fill << 1 | wrapped` (usually two bytes, against a ten-byte
/// [`RowMeta`]). `lead` is in a text row's header; `dirty` and `simple`
/// mean nothing once a row is packed.
fn pack_meta(meta: &[RowMeta], out: &mut Vec<u8>) {
    for meta in meta {
        push_varint(out, meta.occ as usize);
        push_varint(out, (meta.fill as usize) << 1 | meta.wrapped as usize);
    }
}

/// Decodes `metas.len()` rows of [`pack_meta`] output; returns where the
/// row tokens start.
fn unpack_meta(data: &[u8], metas: &mut [RowMeta]) -> usize {
    let mut at = 0;
    for meta in metas {
        let occ = read_varint(data, &mut at) as u16;
        let fill = read_varint(data, &mut at);
        *meta = RowMeta {
            occ,
            fill: (fill >> 1) as u16,
            lead: 0,
            wrapped: fill & 1 != 0,
            dirty: false,
            simple: false,
        };
    }
    at
}

/// Dense buffers a packed page leaves behind, reused by the next tail page.
type DenseBuffers = (Box<[Cell]>, Box<[RowMeta]>, TextLog, Clusters);

impl Page {
    fn new(
        rows: usize,
        cols: usize,
        serial: u64,
        dense: Option<DenseBuffers>,
        logging: bool,
    ) -> Page {
        let (cells, meta, text, clusters) = dense.unwrap_or_else(|| {
            (
                vec![Cell::default(); rows * cols].into_boxed_slice(),
                vec![RowMeta::default(); rows].into_boxed_slice(),
                TextLog::new(rows, logging),
                Clusters::default(),
            )
        });
        Page {
            serial,
            len: 0,
            cells,
            meta,
            styles: StyleTable::default(),
            clusters,
            text,
            links: None,
            packed: None,
        }
    }

    fn recycle(&mut self, serial: u64) {
        // Cells and metadata are not cleared: rows are appended with
        // occ = 0, so stale cells are never read.
        self.serial = serial;
        self.len = 0;
        self.styles.clear();
        self.clusters.clear();
        self.text.clear();
        self.links = None;
    }

    /// The index of `link` in this page's table.
    pub fn link_id(&mut self, link: &Link) -> u32 {
        let links = self.links.get_or_insert_with(Default::default);
        match links.table.iter().position(|l| l == link) {
            Some(i) => i as u32,
            None => {
                links.table.push(link.clone());
                (links.table.len() - 1) as u32
            }
        }
    }

    /// Cell `index` (`row * cols + col`) has link `id` of this page.
    pub fn set_link(&mut self, index: usize, id: u32) {
        self.links
            .get_or_insert_with(Default::default)
            .map
            .insert(index as u32, id);
    }

    /// The link id recorded for cell `index` (meaningful with HYPERLINK).
    pub fn link_of(&self, index: usize) -> Option<u32> {
        self.links.as_ref()?.map.get(&(index as u32)).copied()
    }

    fn link(&self, id: u32) -> Option<Link> {
        self.links.as_ref()?.table.get(id as usize).cloned()
    }

    pub fn is_packed(&self) -> bool {
        self.packed.is_some()
    }

    /// Adds `c` to the cluster of the cell at `index` (a hot page).
    pub fn push_zerowidth(&mut self, index: usize, c: char) {
        if self.clusters.full() {
            self.compact_clusters();
        }
        push_mark(&mut self.cells, &mut self.clusters, index, c);
    }

    /// Rebuilds the arena from the clusters cells still name.
    fn compact_clusters(&mut self) {
        let old = std::mem::take(&mut self.clusters);
        for cell in self.cells.iter_mut() {
            if cell.has(GRAPHEME) {
                let k = self.clusters.add(old.of(*cell));
                cell.0 = (cell.0 & !CLUSTER) | k << CLUSTER_SHIFT;
            }
        }
    }

    /// Packs the page; returns its dense buffers for reuse.
    ///
    /// The packed bytes are every row's metadata (see [`pack_meta`]), then
    /// per row, the cells below `occ` as tokens, with a current style:
    /// `0x02 id(u16)` sets the style; `0x03 n chars` makes the next cell a
    /// cluster with those extra codepoints; `0x04 n` is n blanks; `0x01` and
    /// eight bytes is a literal cell; `0x05` is a leading spacer; a blank,
    /// tab or printable ASCII cell is its byte; a character is its UTF-8
    /// sequence, or two bytes from U+4E00 on (see [`CJK_LEADS`]), and is
    /// two columns (with its spacer) when its width is two. Everything else about a cell (marks,
    /// spacers that do not follow their character, leading spacers) is a
    /// literal, so packing is lossless.
    fn pack(&mut self, cols: usize, out: &mut Vec<u8>) -> DenseBuffers {
        out.clear();
        pack_meta(&self.meta[..self.len], out);
        // Every written row logged, in order, with nothing between them:
        // the log is the row tokens.
        if self.text.on && self.text.contiguous(&self.meta[..self.len]) {
            out.extend_from_slice(&self.text.bytes);
            return self.finish_pack(cols, out);
        }
        let mut current = 0u64;
        for row in 0..self.len {
            let occ = self.meta[row].occ as usize;
            let cells = &self.cells[row * cols..row * cols + occ];
            if occ > 0
                && let Some(tokens) = self.text.get(row)
            {
                out.extend_from_slice(tokens);
                continue;
            }
            if occ > 0 && self.meta[row].simple && pack_text_row(cells, &self.meta[row], out) {
                continue;
            }
            let mut x = 0;
            let mut blanks = 0;
            while x < occ {
                let raw = cells[x].0;
                let cell = raw & !(GRAPHEME | CLUSTER | HYPERLINK);
                let style = cell & STYLE_BITS;
                // Plain ASCII and blanks in the current style, eight at a
                // time: printable bytes and tabs as themselves, blanks
                // gathered into runs.
                if x + 8 <= occ
                    && let Some((word, plain, blank)) = narrow8(&cells[x..x + 8], current)
                {
                    if plain == u64::MAX {
                        flush_blanks(out, &mut blanks);
                        out.extend_from_slice(&word.to_le_bytes());
                        x += 8;
                        continue;
                    }
                    if blank == u64::MAX {
                        blanks += 8;
                        x += 8;
                        continue;
                    }
                    if plain | blank == u64::MAX {
                        for b in word.to_le_bytes() {
                            if b == 0 {
                                blanks += 1;
                            } else {
                                flush_blanks(out, &mut blanks);
                                out.push(b);
                            }
                        }
                        x += 8;
                        continue;
                    }
                }
                flush_blanks(out, &mut blanks);
                if raw & HYPERLINK != 0 {
                    let index = row * cols + x;
                    let link = self.link_of(index).expect("linked cell");
                    out.push(T_LINK);
                    out.extend_from_slice(&(link as u16).to_le_bytes());
                }
                if raw & GRAPHEME != 0 {
                    let extra = self.clusters.of(cells[x]);
                    out.push(T_CLUSTER);
                    out.push(extra.len() as u8);
                    for &c in extra {
                        push_utf8(out, c as u64);
                    }
                }
                let cp = cell & CP;
                let flags = cell & FLAGS;
                let token = |out: &mut Vec<u8>, current: &mut u64| {
                    if style != *current {
                        out.push(T_STYLE);
                        out.extend_from_slice(&((style >> STYLE_SHIFT) as u16).to_le_bytes());
                        *current = style;
                    }
                };
                if flags == 0 && (cp == 0 || cp == 9 || (0x20..0x7F).contains(&cp)) {
                    token(out, &mut current);
                    if cp == 0 && raw & GRAPHEME == 0 {
                        let run = cells[x..]
                            .iter()
                            .take(255)
                            .take_while(|c| c.0 == cell)
                            .count();
                        if run >= 3 {
                            out.push(T_BLANKS);
                            out.push(run as u8);
                            x += run;
                            continue;
                        }
                    }
                    out.push(cp as u8);
                    x += 1;
                    continue;
                }
                if cell == 0x20 | LEADING | style {
                    token(out, &mut current);
                    out.push(T_LEADING);
                    x += 1;
                    continue;
                }
                if cp >= 0x80 {
                    let wide = wide(cp);
                    if flags == 0 && !wide {
                        token(out, &mut current);
                        push_char(out, cp);
                        x += 1;
                        continue;
                    }
                    if flags == WIDE
                        && wide
                        && x + 1 < occ
                        && cells[x + 1].0 == 0x20 | SPACER | style
                    {
                        token(out, &mut current);
                        push_char(out, cp);
                        x += 2;
                        continue;
                    }
                }
                out.push(T_LITERAL);
                out.extend_from_slice(&cell.to_le_bytes());
                x += 1;
            }
            flush_blanks(out, &mut blanks);
        }
        self.finish_pack(cols, out)
    }

    /// Keeps `packed` as the page and hands back its dense buffers.
    fn finish_pack(&mut self, cols: usize, packed: &[u8]) -> DenseBuffers {
        self.packed = Some(Box::from(packed));
        // Debug builds decode the page and compare it with the cells.
        #[cfg(debug_assertions)]
        {
            let mut check = vec![Cell::default(); self.len * cols];
            let mut metas = vec![RowMeta::default(); self.len];
            let mut clusters = Clusters::default();
            let mut links = HashMap::new();
            self.unpack(cols, &mut metas, &mut check, &mut clusters, &mut links);
            for (row, (&got, &want)) in metas.iter().zip(&self.meta[..self.len]).enumerate() {
                assert_eq!(
                    (got.occ, got.fill, got.wrapped),
                    (want.occ, want.fill, want.wrapped),
                    "packed metadata of row {row}"
                );
                let span = row * cols..row * cols + want.occ as usize;
                for (&a, &b) in check[span.clone()].iter().zip(&self.cells[span]) {
                    assert_eq!(a.0 & !CLUSTER, b.0 & !CLUSTER, "packed row {row}");
                    assert_eq!(clusters.of(a), self.clusters.of(b), "cluster in row {row}");
                }
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = cols;
        self.styles.freeze();
        if let Some(links) = &mut self.links {
            links.map = HashMap::new();
        }
        let text = std::mem::replace(&mut self.text, TextLog::new(0, false));
        let cells = std::mem::take(&mut self.cells);
        let meta = std::mem::take(&mut self.meta);
        let mut clusters = std::mem::take(&mut self.clusters);
        clusters.reuse();
        (cells, meta, text, clusters)
    }

    /// Decodes a packed page into `metas` (its rows), `cells` (rows *
    /// cols), `clusters` and `links`.
    fn unpack(
        &self,
        cols: usize,
        metas: &mut [RowMeta],
        cells: &mut [Cell],
        clusters: &mut Clusters,
        links: &mut HashMap<u32, u32>,
    ) {
        let data = self.packed.as_deref().expect("packed");
        clusters.clear();
        links.clear();
        let mut pending_link: Option<u32> = None;
        let mut at = unpack_meta(data, &mut metas[..self.len]);
        let mut current = 0u64;
        let mut pending: Option<u64> = None;
        let mut chars = Vec::new();
        for row in 0..self.len {
            let occ = metas[row].occ as usize;
            let cells = &mut cells[row * cols..row * cols + occ];
            if occ > 0 && data[at] == T_TEXT_ROW {
                let mut lead = data[at + 1] as usize;
                at += 2;
                if lead == 255 {
                    lead = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
                    at += 2;
                }
                metas[row].lead = lead as u16;
                decode_text_row(data, &mut at, metas[row].fill, lead, cells, clusters);
                continue;
            }
            let mut x = 0;
            while x < occ {
                let byte = data[at];
                at += 1;
                let first = x;
                match byte {
                    T_LITERAL => {
                        let bytes: [u8; 8] = data[at..at + 8].try_into().expect("eight bytes");
                        cells[x] = Cell(u64::from_le_bytes(bytes));
                        at += 8;
                        x += 1;
                    }
                    T_STYLE => {
                        current =
                            (u16::from_le_bytes([data[at], data[at + 1]]) as u64) << STYLE_SHIFT;
                        at += 2;
                        continue;
                    }
                    T_LINK => {
                        pending_link = Some(u16::from_le_bytes([data[at], data[at + 1]]) as u32);
                        at += 2;
                        continue;
                    }
                    T_CLUSTER => {
                        let n = data[at] as usize;
                        at += 1;
                        chars.clear();
                        for _ in 0..n {
                            chars.push(
                                char::from_u32(read_utf8(data, &mut at) as u32).expect("char"),
                            );
                        }
                        pending = Some(clusters.add(&chars));
                        continue;
                    }
                    T_BLANKS => {
                        let n = data[at] as usize;
                        at += 1;
                        cells[x..x + n].fill(Cell(current));
                        x += n;
                    }
                    T_LEADING => {
                        cells[x] = Cell(0x20 | LEADING | current);
                        x += 1;
                    }
                    _ => {
                        let cp = match CJK_HIGH[byte as usize] {
                            0xFF if byte < 0x80 => byte as u64,
                            0xFF => {
                                at -= 1;
                                read_utf8(data, &mut at)
                            }
                            high => {
                                let cp = CJK_BASE + ((high as u64) << 8 | data[at] as u64);
                                at += 1;
                                cp
                            }
                        };
                        if cp >= 0x80 && wide(cp) {
                            cells[x] = Cell(cp | WIDE | current);
                            cells[x + 1] = Cell(0x20 | SPACER | current);
                            x += 2;
                        } else {
                            cells[x] = Cell(cp | current);
                            x += 1;
                        }
                    }
                }
                if let Some(k) = pending.take() {
                    cells[first].0 |= GRAPHEME | k << CLUSTER_SHIFT;
                }
                if let Some(link) = pending_link.take() {
                    cells[first].0 |= HYPERLINK;
                    links.insert((row * cols + first) as u32, link);
                }
            }
        }
        debug_assert_eq!(at, data.len());
    }

    /// Live bytes of a packed page.
    fn stored_bytes(&self) -> usize {
        self.packed.as_ref().map_or(0, |p| p.len())
            + std::mem::size_of_val(&*self.meta)
            + self.styles.heap_bytes()
            + self.links.as_ref().map_or(0, |links| {
                links
                    .table
                    .iter()
                    .map(|l| l.uri.len() + l.id.as_ref().map_or(0, String::len))
                    .sum::<usize>()
            })
            + std::mem::size_of::<Page>()
    }
}

/// The most recently unpacked page, for reads of packed history.
struct Unpacked {
    serial: u64,
    meta: Box<[RowMeta]>,
    cells: Box<[Cell]>,
    clusters: Clusters,
    links: HashMap<u32, u32>,
}

pub struct Pages {
    pub cols: usize,
    pub lines: usize,
    shift: u32,
    pub history_limit: usize,
    /// Live-byte bound on history; packs cold pages when set.
    budget: Option<usize>,
    pages: VecDeque<Page>,
    /// Absolute index of the first row of `pages[0]`.
    base: u64,
    /// Absolute index one past the newest row.
    total: u64,
    /// Rows below this absolute index are not history (cleared).
    floor: u64,
    /// Pages at the front that are packed.
    sealed: usize,
    packed_bytes: usize,
    spare: Option<Page>,
    spare_dense: Vec<DenseBuffers>,
    /// Reused output buffer of `Page::pack`.
    scratch: Vec<u8>,
    next_serial: u64,
    cache: RefCell<Option<Unpacked>>,
}

/// Rows per page: a power of two near 64 KiB of cells, at least 16.
fn rows_shift(cols: usize) -> u32 {
    let rows = (65536 / (cols.max(1) * 8)).max(16);
    usize::BITS - 1 - rows.leading_zeros()
}

impl Pages {
    /// `page_rows` overrides the rows per page (a power of two), for tests.
    pub fn new(
        lines: usize,
        cols: usize,
        history_limit: usize,
        budget: Option<usize>,
        page_rows: Option<usize>,
    ) -> Pages {
        let shift = page_rows.map_or_else(
            || rows_shift(cols),
            |rows| {
                assert!(rows.is_power_of_two(), "rows per page");
                rows.trailing_zeros()
            },
        );
        let mut pages = Pages {
            cols,
            lines,
            shift,
            history_limit,
            budget,
            pages: VecDeque::new(),
            base: 0,
            total: 0,
            floor: 0,
            sealed: 0,
            packed_bytes: 0,
            spare: None,
            spare_dense: Vec::new(),
            scratch: Vec::new(),
            next_serial: 1,
            cache: RefCell::new(None),
        };
        for _ in 0..lines {
            pages.push_row(&Style::default());
        }
        pages
    }

    #[inline(always)]
    pub fn rows_per_page(&self) -> usize {
        1 << self.shift
    }

    /// Page index and row within it of absolute row `abs`.
    #[inline(always)]
    fn locate_abs(&self, abs: u64) -> (usize, usize) {
        let off = (abs - self.base) as usize;
        (off >> self.shift, off & ((1 << self.shift) - 1))
    }

    /// Page index and row of screen line `y`.
    #[inline(always)]
    pub fn locate(&self, y: usize) -> (usize, usize) {
        self.locate_abs(self.total - self.lines as u64 + y as u64)
    }

    #[inline(always)]
    pub fn page(&self, index: usize) -> &Page {
        &self.pages[index]
    }

    /// A page that may be written: never a packed one.
    #[inline(always)]
    pub fn page_mut(&mut self, index: usize) -> &mut Page {
        debug_assert!(
            !self.pages[index].is_packed(),
            "writes stay out of packed pages"
        );
        &mut self.pages[index]
    }

    pub fn history_size(&self) -> usize {
        let behind = (self.total - self.base.max(self.floor)) as usize - self.lines;
        behind.min(self.history_limit)
    }

    /// Drops every history row (ED 3, reset): rows below the screen are
    /// hidden at once, and whole pages of them are released.
    pub fn clear_history(&mut self) {
        self.floor = self.total - self.lines as u64;
        let rows = self.rows_per_page() as u64;
        while self.pages.len() > 1 && self.base + rows <= self.floor {
            self.pop_front_page();
        }
    }

    /// Page and row of line `line`, where negative lines are history
    /// (`-1` is the newest history row), as alacritty numbers them.
    pub fn locate_line(&self, line: i32) -> (usize, usize) {
        let abs = (self.total as i64 - self.lines as i64 + line as i64) as u64;
        self.locate_abs(abs)
    }

    /// Runs `f` over page `p`'s cells and clusters, unpacking it first
    /// (into a one-page cache) when it is packed.
    pub fn read<R>(&self, p: usize, f: impl FnOnce(&[RowMeta], &[Cell], &Clusters) -> R) -> R {
        let page = &self.pages[p];
        if !page.is_packed() {
            return f(&page.meta, &page.cells, &page.clusters);
        }
        let mut cache = self.cache.borrow_mut();
        if cache.as_ref().is_none_or(|c| c.serial != page.serial) {
            let mut unpacked = cache.take().unwrap_or_else(|| Unpacked {
                serial: 0,
                meta: vec![RowMeta::default(); self.rows_per_page()].into_boxed_slice(),
                cells: vec![Cell::default(); self.rows_per_page() * self.cols].into_boxed_slice(),
                clusters: Clusters::default(),
                links: HashMap::new(),
            });
            page.unpack(
                self.cols,
                &mut unpacked.meta,
                &mut unpacked.cells,
                &mut unpacked.clusters,
                &mut unpacked.links,
            );
            unpacked.serial = page.serial;
            *cache = Some(unpacked);
        }
        let unpacked = cache.as_ref().expect("filled");
        f(&unpacked.meta, &unpacked.cells, &unpacked.clusters)
    }

    /// The metadata of row `r` of page `p`, packed or not.
    #[inline]
    pub fn meta(&self, p: usize, r: usize) -> RowMeta {
        let page = &self.pages[p];
        if !page.is_packed() {
            return page.meta[r];
        }
        self.read(p, |meta, _, _| meta[r])
    }

    /// The link of cell `index` (`row * cols + col`) of page `p`, which has
    /// the HYPERLINK bit.
    pub fn read_link(&self, p: usize, index: usize) -> Option<Link> {
        let page = &self.pages[p];
        let id = if page.is_packed() {
            self.read(p, |_, _, _| ());
            let cache = self.cache.borrow();
            cache.as_ref()?.links.get(&(index as u32)).copied()
        } else {
            page.link_of(index)
        }?;
        page.link(id)
    }

    /// Drops the read cache (readers call this after a batch).
    pub fn release_read_cache(&self) {
        *self.cache.borrow_mut() = None;
    }

    fn new_page(&mut self) -> Page {
        let serial = self.next_serial;
        self.next_serial += 1;
        if let Some(mut page) = self.spare.take() {
            page.recycle(serial);
            return page;
        }
        let logging = self.budget.is_some();
        Page::new(
            self.rows_per_page(),
            self.cols,
            serial,
            self.spare_dense.pop(),
            logging,
        )
    }

    /// Appends a blank row whose erase fill is `fill`; the oldest screen
    /// row becomes history.
    #[inline]
    pub fn push_row(&mut self, fill: &Style) {
        let rows = self.rows_per_page();
        if self.pages.back().is_none_or(|page| page.len == rows) {
            let page = self.new_page();
            self.pages.push_back(page);
        }
        let page = self.pages.back_mut().expect("a tail page");
        let fill = page.styles.intern(fill);
        let row = page.len;
        page.meta[row] = RowMeta {
            occ: 0,
            fill,
            lead: 0,
            wrapped: false,
            dirty: true,
            simple: true,
        };
        page.text.reset(row);
        page.len += 1;
        self.total += 1;
        // Sealing, the hot-history count and page eviction change only when
        // the newest row, the newest history row or the oldest row the row
        // limit keeps crosses a page boundary.
        let mask = self.rows_per_page() as u64 - 1;
        let keep = (self.lines + self.history_limit) as u64;
        if self.total & mask == 0
            || self.total.wrapping_sub(self.lines as u64) & mask == 0
            || self.total.wrapping_sub(self.lines as u64 + 1) & mask == 0
            || self.total.wrapping_sub(keep) & mask == 0
        {
            self.seal();
            self.trim();
        }
    }

    /// Packs every page whose rows are all older than the newest history
    /// row (which writes at the top of the screen may still touch).
    #[inline]
    fn seal(&mut self) {
        if self.budget.is_none() {
            return;
        }
        let rows = self.rows_per_page() as u64;
        let top = self.total.saturating_sub(self.lines as u64);
        while self.sealed < self.pages.len() {
            let first = self.base + self.sealed as u64 * rows;
            if first + rows >= top {
                break;
            }
            let cols = self.cols;
            let page = &mut self.pages[self.sealed];
            let (cells, meta, mut text, clusters) = page.pack(cols, &mut self.scratch);
            self.packed_bytes += page.stored_bytes();
            if self.spare_dense.len() < 2 {
                text.clear();
                self.spare_dense.push((cells, meta, text, clusters));
            }
            self.sealed += 1;
        }
    }

    /// Live history bytes: packed pages plus the dense pages that hold any
    /// history row, at full size.
    pub fn history_bytes(&self) -> usize {
        let rows = self.rows_per_page() as u64;
        let top = self.total.saturating_sub(self.lines as u64);
        let first_hot = self.base + self.sealed as u64 * rows;
        let hot_history = if first_hot >= top {
            0
        } else {
            (top - first_hot).div_ceil(rows) as usize
        };
        let dense = self.rows_per_page()
            * (self.cols * std::mem::size_of::<Cell>() + std::mem::size_of::<RowMeta>())
            + std::mem::size_of::<Page>();
        self.packed_bytes + hot_history.min(self.pages.len() - self.sealed) * dense
    }

    fn pop_front_page(&mut self) {
        let page = self.pages.pop_front().expect("a page");
        self.base += self.rows_per_page() as u64;
        if page.is_packed() {
            self.sealed -= 1;
            self.packed_bytes -= page.stored_bytes();
        } else if self.spare.is_none() {
            self.spare = Some(page);
        }
    }

    /// Drops whole pages past the row limit or, oldest first, while history
    /// exceeds the budget.
    #[inline]
    fn trim(&mut self) {
        let keep = (self.lines + self.history_limit) as u64;
        let rows = self.rows_per_page() as u64;
        while self.pages.len() > 1 && self.total - (self.base + rows) >= keep {
            self.pop_front_page();
        }
        if let Some(budget) = self.budget {
            while self.sealed > 0 && self.history_bytes() > budget {
                self.pop_front_page();
            }
        }
    }

    /// Heap bytes held by pages, for footprint.
    pub fn heap_bytes(&self) -> usize {
        let dense = self.rows_per_page()
            * (self.cols * std::mem::size_of::<Cell>() + std::mem::size_of::<RowMeta>());
        self.packed_bytes
            + (self.pages.len() - self.sealed + usize::from(self.spare.is_some())) * dense
            + self.spare_dense.len() * self.rows_per_page() * self.cols * 8
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
}
