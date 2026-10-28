//! The prototype terminal: the print path (ASCII and non-ASCII runs,
//! grapheme clusters, wrap), linefeed into page history, tabs, SGR, and
//! the cursor moves and line erase the reference CSI workload uses.
//!
//! Semantics follow the current core (`alacritty_terminal::Term` with Diri's
//! patches) cell for cell; `examples/core_v2_bench.rs` checks the final
//! state of every run against it. Anything the prototype does not implement
//! is counted in [`Core::unsupported`], so a payload that needs it fails
//! the check instead of reporting a number.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::grapheme::{self, BreakState, Class, Cluster, Info};
use alacritty_terminal::vte::ansi::{
    Attr, ClearMode, Handler, LineClearMode, Mode, NamedMode, NamedPrivateMode, PrivateMode,
};

use super::cell::{CLUSTER, Cell, GRAPHEME, LEADING, MARKS, SPACER, STYLE_SHIFT, Style, WIDE};
use super::page::{Pages, RowMeta};
use super::text::{Tables, tables};

pub struct Core {
    pub pages: Pages,
    lines: usize,
    cols: usize,
    pub line: usize,
    pub col: usize,
    pub wrap_pending: bool,
    pen: Style,
    /// (page serial, interned id) of `pen`.
    pen_cache: (u64, u16),
    tabs: Vec<bool>,
    clustering: bool,
    insert: bool,
    line_wrap: bool,
    tables: &'static Tables,
    /// Sequences the prototype does not implement.
    pub unsupported: u64,
    /// Decode ground-state text from raw bytes (`Handler::input_bytes`)
    /// instead of the parser's validated runs. On by default.
    pub raw_text: bool,
}

impl Core {
    pub fn new(lines: usize, cols: usize, history_rows: usize) -> Core {
        Core::with_budget(lines, cols, history_rows, None)
    }

    /// A core whose history is bounded by `budget` live bytes (cold pages
    /// packed) as well as `history_rows` rows.
    pub fn with_budget(
        lines: usize,
        cols: usize,
        history_rows: usize,
        budget: Option<usize>,
    ) -> Core {
        Core::with_layout(lines, cols, history_rows, budget, None)
    }

    /// [`Core::with_budget`] with `page_rows` rows per page (a power of
    /// two), so tests can seal and evict pages with little output.
    #[doc(hidden)]
    pub fn with_layout(
        lines: usize,
        cols: usize,
        history_rows: usize,
        budget: Option<usize>,
        page_rows: Option<usize>,
    ) -> Core {
        Core {
            pages: Pages::new(lines, cols, history_rows, budget, page_rows),
            lines,
            cols,
            line: 0,
            col: 0,
            wrap_pending: false,
            pen: Style::default(),
            pen_cache: (0, 0),
            tabs: (0..cols).map(|c| c % 8 == 0).collect(),
            clustering: false,
            insert: false,
            line_wrap: true,
            tables: tables(),
            unsupported: 0,
            raw_text: true,
        }
    }

    pub fn lines(&self) -> usize {
        self.lines
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn history_size(&self) -> usize {
        self.pages.history_size()
    }

    /// Live bytes of history (packed pages plus dense pages holding history).
    pub fn history_bytes(&self) -> usize {
        self.pages.history_bytes()
    }

    /// Heap bytes held by pages.
    pub fn heap_bytes(&self) -> usize {
        self.pages.heap_bytes()
    }

    /// The pen's style id in page `page`.
    #[inline(always)]
    fn pen_id(&mut self, page: usize) -> u16 {
        let p = self.pages.page_mut(page);
        if p.serial != self.pen_cache.0 {
            self.pen_cache = (p.serial, p.styles.intern(&self.pen));
        }
        self.pen_cache.1
    }

    /// The cell at `line` (negative: history) and `col`, the erase fill
    /// past the row's occupancy. Packed history is unpacked to read it.
    pub fn cell(&self, line: i32, col: usize) -> Cell {
        let (p, r) = self.pages.locate_line(line);
        let cols = self.cols;
        self.pages.read(p, |meta, cells, _| {
            let meta = meta[r];
            if col >= meta.occ as usize {
                Cell::blank(meta.fill)
            } else {
                cells[r * cols + col]
            }
        })
    }

    pub fn style_of(&self, line: i32, cell: Cell) -> Style {
        let (p, _) = self.pages.locate_line(line);
        self.pages.page(p).styles.get(cell.style())
    }

    pub fn wrapped(&self, line: i32) -> bool {
        let (p, r) = self.pages.locate_line(line);
        self.pages.meta(p, r).wrapped
    }

    /// The extra codepoints of the cluster at `line`, `col`.
    pub fn zerowidth(&self, line: i32, col: usize) -> Vec<char> {
        let (p, r) = self.pages.locate_line(line);
        let cols = self.cols;
        self.pages.read(p, |meta, cells, clusters| {
            if col >= meta[r].occ as usize {
                return Vec::new();
            }
            clusters.of(cells[r * cols + col]).to_vec()
        })
    }

    /// The cluster at screen `line`, `col` (always a writable page).
    fn zerowidth_hot(&self, line: usize, col: usize) -> &[char] {
        let cell = self.get(line, col);
        let (p, _) = self.pages.locate(line);
        self.pages.page(p).clusters.of(cell)
    }

    /// Releases the read cache of packed history.
    pub fn release_read_cache(&self) {
        self.pages.release_read_cache();
    }

    /// Makes cells below `upto` real on screen line `line`.
    #[inline(always)]
    fn materialize(&mut self, p: usize, r: usize, upto: usize) {
        let cols = self.cols;
        let page = self.pages.page_mut(p);
        let meta = &mut page.meta[r];
        let occ = meta.occ as usize;
        if occ < upto {
            let blank = Cell::blank(meta.fill);
            page.cells[r * cols + occ..r * cols + upto].fill(blank);
            meta.occ = upto as u16;
        }
    }

    /// Writes one cell on screen line `line` (the general path).
    fn set(&mut self, line: usize, col: usize, f: impl FnOnce(Cell) -> Cell) {
        let (p, r) = self.pages.locate(line);
        self.set_at(p, r, col, f);
    }

    /// The general cell write: the row no longer packs as text.
    fn set_at(&mut self, p: usize, r: usize, col: usize, f: impl FnOnce(Cell) -> Cell) {
        self.materialize(p, r, col + 1);
        let cols = self.cols;
        let page = self.pages.page_mut(p);
        let cell = &mut page.cells[r * cols + col];
        *cell = f(*cell);
        let meta = &mut page.meta[r];
        meta.dirty = true;
        meta.simple = false;
        meta.lead = meta.lead.min(col as u16);
        page.text.invalidate(r);
    }

    /// Records a text write starting at `col` on a row whose occupancy was
    /// `occ`, in style `style`, for the row's text packing.
    #[inline(always)]
    fn note_text(meta: &mut RowMeta, occ: usize, col: usize, style: u16) {
        if occ == 0 || (col as u16) < meta.lead {
            meta.lead = col as u16;
        }
        if style != meta.fill {
            meta.simple = false;
        }
    }

    /// A cell of screen line `line` (always a writable page).
    #[inline]
    fn get(&self, line: usize, col: usize) -> Cell {
        let (p, r) = self.pages.locate(line);
        let page = self.pages.page(p);
        let meta = page.meta[r];
        if col >= meta.occ as usize {
            Cell::blank(meta.fill)
        } else {
            page.cells[r * self.cols + col]
        }
    }

    // ---- the general print path, ported from `Term::input` ----

    fn wrapline(&mut self) {
        if !self.line_wrap {
            return;
        }
        let (p, r) = self.pages.locate(self.line);
        self.pages.page_mut(p).meta[r].wrapped = true;
        self.linefeed_inner();
        self.col = 0;
        self.wrap_pending = false;
    }

    /// `Term::write_at_cursor`: `extra` is WIDE, SPACER or LEADING.
    fn write_at_cursor(&mut self, c: char, extra: u64) {
        let (line, col) = (self.line, self.col);
        let (p, r) = self.pages.locate(line);
        let id = self.pen_id(p);
        let style = (id as u64) << STYLE_SHIFT;
        let old = self.get(line, col);
        if old.has(WIDE | SPACER) {
            if old.has(WIDE) && col + 1 < self.cols {
                self.set(line, col + 1, |cell| Cell(cell.0 & !SPACER));
            } else if col > 0 {
                // `clear_wide`: no longer wide, no cluster, a space.
                self.set(line, col - 1, |cell| {
                    Cell((cell.0 & !(WIDE | GRAPHEME | CLUSTER | super::cell::CP)) | ' ' as u64)
                });
            }
            if col <= 1 && (line as i32) > -(self.history_size() as i32) {
                let (pp, pr) = self.pages.locate_line(line as i32 - 1);
                let last = self.cols - 1;
                self.set_at(pp, pr, last, |cell| Cell(cell.0 & !LEADING));
            }
            let marks = self.get(line, col).0 & MARKS;
            self.set_at(p, r, col, |_| Cell(c as u64 | extra | style | marks));
        } else {
            // One cell, as the text paths write it: the row keeps packing
            // as text when the cell is plain in the fill style.
            let occ = self.pages.page(p).meta[r].occ as usize;
            self.materialize(p, r, col + 1);
            let cols = self.cols;
            let page = self.pages.page_mut(p);
            let meta = &mut page.meta[r];
            Self::note_text(meta, occ, col, id);
            let marks = old.0 & MARKS;
            if marks != 0 {
                meta.simple = false;
            }
            meta.dirty = true;
            if page.text.on {
                let fill = meta.fill;
                let fresh = col >= occ && id == fill && page.text.begin(r, occ, col);
                let logged = fresh
                    && match extra {
                        0 if c >= '\u{A0}' => {
                            let mut buf = [0u8; 4];
                            page.text.blanks(r, if occ == 0 { 0 } else { col - occ });
                            page.text
                                .char(r, c as u64, c.encode_utf8(&mut buf).as_bytes());
                            true
                        }
                        0 if (' '..'\x7f').contains(&c) => {
                            page.text.blanks(r, if occ == 0 { 0 } else { col - occ });
                            page.text.push(r, c as u8);
                            true
                        }
                        WIDE => {
                            let mut buf = [0u8; 4];
                            page.text.blanks(r, if occ == 0 { 0 } else { col - occ });
                            page.text
                                .char(r, c as u64, c.encode_utf8(&mut buf).as_bytes());
                            true
                        }
                        // The spacer right after its character is implied.
                        SPACER => {
                            c == ' '
                                && col == occ
                                && col > 0
                                && page.cells[r * cols + col - 1].0 & !super::cell::CP
                                    == WIDE | style
                        }
                        LEADING if c == ' ' => {
                            page.text.blanks(r, if occ == 0 { 0 } else { col - occ });
                            page.text.leading(r);
                            true
                        }
                        _ => false,
                    };
                if !logged {
                    page.text.invalidate(r);
                }
            }
            page.cells[r * cols + col] = Cell(c as u64 | extra | style | marks);
        }
        if col + 1 == self.cols {
            self.pages.page_mut(p).meta[r].wrapped = false;
        }
    }

    fn push_zerowidth(&mut self, line: usize, col: usize, c: char) {
        let (p, r) = self.pages.locate(line);
        self.materialize(p, r, col + 1);
        let cols = self.cols;
        let page = self.pages.page_mut(p);
        page.push_zerowidth(r * cols + col, c);
        page.meta[r].dirty = true;
        page.meta[r].simple = false;
        page.text.invalidate(r);
    }

    fn input_general(&mut self, c: char) {
        if c < ' ' || ('\u{7F}'..'\u{A0}').contains(&c) {
            return;
        }
        let info = Info::of(c);
        let width = info.width();
        // A zero-width mark after a cell that is no cluster of its own
        // making: `continue_cluster` would decline (it joins such a mark
        // only to a lone regional indicator, keycap or emoji modifier), so
        // the mark goes straight onto that cell.
        let mark_only = width == 0 && !grapheme::is_presentation_mark(c) && {
            let mut col = self.col;
            if !self.wrap_pending {
                col = col.saturating_sub(1);
            }
            if self.get(self.line, col).has(SPACER) {
                col = col.saturating_sub(1);
            }
            let cell = self.get(self.line, col);
            cell.has(GRAPHEME) || {
                let base = cell.ch();
                !Cluster::new(base, Info::of(base)).presentation()
            }
        };
        if !mark_only && grapheme::may_join(c, info) && self.continue_cluster(c, info) {
            return;
        }
        if width == 0 {
            let mut col = self.col;
            if !self.wrap_pending {
                col = col.saturating_sub(1);
            }
            if self.get(self.line, col).has(SPACER) {
                col = col.saturating_sub(1);
            }
            self.push_zerowidth(self.line, col, c);
            return;
        }
        if self.wrap_pending {
            self.wrapline();
        }
        let cols = self.cols;
        if self.insert && self.col + width < cols {
            self.unsupported += 1;
        }
        if width == 1 {
            self.write_at_cursor(c, 0);
        } else {
            if self.col + 1 >= cols {
                if self.line_wrap {
                    self.write_at_cursor(' ', LEADING);
                    self.wrapline();
                } else {
                    self.wrap_pending = true;
                    return;
                }
            }
            self.write_at_cursor(c, WIDE);
            self.col += 1;
            self.write_at_cursor(' ', SPACER);
        }
        if self.col + 1 < cols {
            self.col += 1;
        } else {
            self.wrap_pending = true;
        }
    }

    /// `Term::continue_cluster`, on this grid.
    fn continue_cluster(&mut self, c: char, info: Info) -> bool {
        const MAX_CLUSTER_CHARS: usize = 32;
        let line = self.line;
        let mut col = self.col;
        if !self.wrap_pending {
            if col == 0 {
                return false;
            }
            col -= 1;
        }
        if self.get(line, col).has(SPACER) {
            if col == 0 {
                return false;
            }
            col -= 1;
        }
        let cell = self.get(line, col);
        if cell.has(SPACER | LEADING) {
            return false;
        }
        let mut buffer = ['\0'; MAX_CLUSTER_CHARS];
        let zerowidth = {
            let stored = self.zerowidth_hot(line, col);
            if stored.len() >= MAX_CLUSTER_CHARS {
                return false;
            }
            buffer[..stored.len()].copy_from_slice(stored);
            &buffer[..stored.len()]
        };
        let clustering = self.clustering;
        let base_char = cell.ch();
        let first = Info::of(base_char);
        let base = first.class();
        if !grapheme::is_presentation_mark(c) {
            let alone_presents = Cluster::new(base_char, first).presentation();
            if info.width() == 0 {
                if !(zerowidth.is_empty() && alone_presents) {
                    return false;
                }
            } else if !clustering
                && !matches!(
                    info.class(),
                    Class::RegionalIndicator
                        | Class::EmojiModifier
                        | Class::ExtendedPictographic
                        | Class::EmojiModifierBase
                )
                && !alone_presents
                && !zerowidth.iter().any(|&z| grapheme::sets_presentation(z))
            {
                return false;
            }
        }
        match info.class() {
            Class::ExtendedPictographic | Class::EmojiModifierBase | Class::IndicConsonant
                if zerowidth.is_empty() =>
            {
                return false;
            }
            Class::RegionalIndicator
                if !zerowidth.is_empty() || base != Class::RegionalIndicator =>
            {
                return false;
            }
            _ => (),
        }
        let mut cluster = Cluster::new(base_char, first);
        let mut previous = base;
        let mut state = BreakState::Default;
        for &z in zerowidth {
            let z_info = Info::of(z);
            if grapheme::breaks(previous, z_info.class(), &mut state) {
                return false;
            }
            cluster.add(z, z_info);
            previous = z_info.class();
        }
        if grapheme::breaks(previous, info.class(), &mut state) {
            return false;
        }
        let before = cluster.width(clustering);
        cluster.add(c, info);
        let after = cluster.width(clustering);
        if !clustering && info.width() > 0 && !cluster.presentation() {
            return false;
        }
        let cell_width = if cell.has(WIDE) { 2 } else { 1 };
        self.push_zerowidth(line, col, c);
        if after != before && before == cell_width {
            match after {
                2 => self.widen_cluster(line, col),
                1 => self.narrow_cluster(line, col),
                _ => (),
            }
        }
        true
    }

    fn widen_cluster(&mut self, line: usize, col: usize) {
        let cols = self.cols;
        if cols < 2 {
            return;
        }
        if col + 1 >= cols {
            if !self.line_wrap {
                return;
            }
            let cluster = self.get(line, col);
            let cluster_style = self.style_of(line as i32, cluster);
            let zerowidth = self.zerowidth_hot(line, col).to_vec();
            self.wrap_pending = false;
            self.col = col;
            self.write_at_cursor(' ', LEADING);
            self.wrapline();
            self.write_at_cursor(cluster.ch(), WIDE);
            let marks = self.get(self.line, self.col).0 & MARKS;
            let (l, c) = (self.line, self.col);
            // The cluster may move to another page: re-intern its style.
            let (p, _) = self.pages.locate(l);
            let style =
                (self.pages.page_mut(p).styles.intern(&cluster_style) as u64) << STYLE_SHIFT;
            let keep = cluster.0 & (super::cell::CP | super::cell::HYPERLINK);
            self.set(l, c, |_| Cell(keep | style | WIDE | marks));
            for z in zerowidth {
                self.push_zerowidth(l, c, z);
            }
        } else {
            self.set(line, col, |cell| Cell(cell.0 | WIDE));
            self.wrap_pending = false;
            self.col = col;
        }
        self.col += 1;
        if self.insert && self.col + 1 < cols {
            self.unsupported += 1;
        }
        self.write_at_cursor(' ', SPACER);
        if self.col + 1 < cols {
            self.col += 1;
        } else {
            self.wrap_pending = true;
        }
    }

    fn narrow_cluster(&mut self, line: usize, col: usize) {
        let spacer = col + 1;
        if spacer >= self.cols || !self.get(line, spacer).has(SPACER) {
            return;
        }
        self.set(line, col, |cell| Cell(cell.0 & !WIDE));
        self.set(line, spacer, |cell| {
            Cell((cell.0 & !(SPACER | super::cell::CP)) | ' ' as u64)
        });
        if self.wrap_pending && self.col == spacer {
            self.wrap_pending = false;
        } else {
            self.col = spacer;
        }
    }

    // ---- fast paths ----

    /// Printable ASCII from `col` on the cursor row; returns how many were
    /// written (0 when the first cell is part of a wide character).
    #[inline(always)]
    fn write_ascii_segment(&mut self, seg: &[u8]) -> usize {
        let x = self.col;
        let n = seg.len();
        let cols = self.cols;
        let (p, r) = self.pages.locate(self.line);
        let style = (self.pen_id(p) as u64) << STYLE_SHIFT;
        let page = self.pages.page_mut(p);
        let meta = &mut page.meta[r];
        let row = &mut page.cells[r * cols..(r + 1) * cols];
        let occ = meta.occ as usize;
        if x > occ {
            row[occ..x].fill(Cell::blank(meta.fill));
        }
        let overlap = occ.clamp(x, x + n) - x;
        let mut written = n;
        if overlap > 0 {
            let olds = &row[x..x + overlap];
            let any_wide = olds.iter().fold(0, |acc, cell| acc | cell.0) & (WIDE | SPACER);
            if any_wide != 0 {
                written = olds
                    .iter()
                    .position(|cell| cell.has(WIDE | SPACER))
                    .unwrap_or(overlap);
            }
            let keep = written.min(overlap);
            for (cell, &byte) in row[x..x + keep].iter_mut().zip(seg) {
                cell.0 = byte as u64 | style | (cell.0 & MARKS);
            }
        }
        if written == n {
            for (cell, &byte) in row[x + overlap..x + n].iter_mut().zip(&seg[overlap..]) {
                cell.0 = byte as u64 | style;
            }
        }
        if written > 0 {
            let id = (style >> STYLE_SHIFT) as u16;
            Self::note_text(meta, occ, x, id);
            let text = &mut page.text;
            if text.on {
                if x >= occ && id == meta.fill && text.begin(r, occ, x) {
                    text.blanks(r, if occ == 0 { 0 } else { x - occ });
                    text.extend(r, &seg[..written]);
                } else {
                    text.invalidate(r);
                }
            }
        }
        meta.occ = occ.max(x + written) as u16;
        meta.dirty = true;
        if x + written == cols {
            meta.wrapped = false;
        }
        written
    }

    /// Non-ASCII characters that start their own cluster and fit on the
    /// cursor row, from `bytes` (validated here); returns `(bytes
    /// consumed, columns)`. Stops at ASCII, invalid UTF-8, a character the
    /// general path must place, or a wide character in the way.
    #[inline(always)]
    fn write_text_segment(&mut self, bytes: &[u8]) -> (usize, usize) {
        let x0 = self.col;
        let cols = self.cols;
        let tables = self.tables;
        let (p, r) = self.pages.locate(self.line);
        let style = (self.pen_id(p) as u64) << STYLE_SHIFT;
        let page = self.pages.page_mut(p);
        let meta = &mut page.meta[r];
        let row = &mut page.cells[r * cols..(r + 1) * cols];
        let occ = meta.occ as usize;
        if x0 > occ {
            row[occ..x0].fill(Cell::blank(meta.fill));
        }
        let text = &mut page.text;
        let id = (style >> STYLE_SHIFT) as u16;
        // Fresh cells in the fill style extend the row's text tokens.
        let log = x0 >= occ && id == meta.fill && text.begin(r, occ, x0);
        if log && occ > 0 {
            text.blanks(r, x0 - occ);
        }
        let mut x = x0;
        let mut i = 0;
        let mut cps = [0u32; 16];
        'row: while i < bytes.len() && x < cols {
            if bytes.len() - i >= 48 && cols - x >= 4 {
                let found = tables.wide_run16(&bytes[i..], &mut cps);
                let fits = found.min((cols - x) / 2);
                if fits > 0 && x >= occ {
                    // Fresh cells: nothing to keep or clear.
                    let (glyph, spacer) = (WIDE | style, ' ' as u64 | SPACER | style);
                    for (pair, &cp) in row[x..x + 2 * fits].chunks_exact_mut(2).zip(&cps[..fits]) {
                        pair[0].0 = cp as u64 | glyph;
                        pair[1].0 = spacer;
                    }
                    if log {
                        text.chars3(r, &cps[..fits], &bytes[i..i + 3 * fits]);
                    }
                    x += 2 * fits;
                    i += 3 * fits;
                    continue;
                }
                if fits > 0 {
                    for &cp in &cps[..fits] {
                        let (glyph, spacer) = if x + 1 < occ {
                            let (a, b) = (row[x].0, row[x + 1].0);
                            if (a | b) & (WIDE | SPACER) != 0 {
                                break 'row;
                            }
                            (a & MARKS, b & MARKS)
                        } else if x < occ {
                            let a = row[x].0;
                            if a & (WIDE | SPACER) != 0 {
                                break 'row;
                            }
                            (a & MARKS, 0)
                        } else {
                            (0, 0)
                        };
                        row[x].0 = cp as u64 | WIDE | style | glyph;
                        row[x + 1].0 = ' ' as u64 | SPACER | style | spacer;
                        x += 2;
                        i += 3;
                    }
                    continue;
                }
            }
            let Some((cp, len, width)) = tables.decode(&bytes[i..]) else {
                break;
            };
            let width = width as usize;
            if width == 0 || x + width > cols {
                break;
            }
            let old = if x < occ { row[x].0 } else { 0 };
            let old2 = if width == 2 && x + 1 < occ {
                row[x + 1].0
            } else {
                0
            };
            if (old | old2) & (WIDE | SPACER) != 0 {
                break;
            }
            if width == 1 {
                row[x].0 = cp as u64 | style | (old & MARKS);
            } else {
                row[x].0 = cp as u64 | WIDE | style | (old & MARKS);
                row[x + 1].0 = ' ' as u64 | SPACER | style | (old2 & MARKS);
            }
            if log {
                text.char(r, cp as u64, &bytes[i..i + len]);
            }
            x += width;
            i += len;
        }
        if x > x0 || x0 > occ {
            // Written cells, or at least the blanks of the gap before x0.
            Self::note_text(meta, occ, x0, id);
            if text.on && !log {
                text.invalidate(r);
            }
        }
        meta.occ = occ.max(x) as u16;
        if x > x0 {
            meta.dirty = true;
            if x == cols {
                meta.wrapped = false;
            }
        }
        (i, x - x0)
    }

    /// Printable ASCII bytes.
    #[inline(always)]
    fn ascii_run(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if self.wrap_pending || self.insert {
                self.input_general(bytes[i] as char);
                i += 1;
                continue;
            }
            let n = (self.cols - self.col).min(bytes.len() - i);
            let written = self.write_ascii_segment(&bytes[i..i + n]);
            if written == 0 {
                self.input_general(bytes[i] as char);
                i += 1;
                continue;
            }
            i += written;
            self.advance_cursor(self.col + written);
        }
    }

    fn advance_cursor(&mut self, end: usize) {
        if end < self.cols {
            self.col = end;
        } else {
            self.col = self.cols - 1;
            self.wrap_pending = true;
        }
    }

    fn linefeed_inner(&mut self) {
        let next = self.line + 1;
        if next == self.lines {
            let fill = self.pen.erased();
            self.pages.push_row(&fill);
        } else if next < self.lines {
            self.line = next;
        }
    }

    fn goto_inner(&mut self, line: i32, col: usize) {
        self.line = line.clamp(0, self.lines as i32 - 1) as usize;
        self.col = col.min(self.cols - 1);
        self.wrap_pending = false;
    }
}

impl Handler for Core {
    /// REP of printable ASCII: row segments of the repeated byte.
    fn input_repeat(&mut self, c: char, count: usize) {
        if !(' '..'\x7f').contains(&c) {
            for _ in 0..count {
                self.input(c);
            }
            return;
        }
        let run = [c as u8; 256];
        let mut left = count;
        while left > 0 {
            let n = left.min(run.len());
            self.ascii_run(&run[..n]);
            left -= n;
        }
    }

    /// One character (REP and other single prints): the row writers when
    /// it starts its own cluster, else the general path.
    #[inline]
    fn input(&mut self, c: char) {
        if (' '..'\x7f').contains(&c) {
            self.ascii_run(&[c as u8]);
            return;
        }
        if !self.wrap_pending && !self.insert && c >= '\u{A0}' {
            let mut buf = [0u8; 4];
            let (used, columns) = self.write_text_segment(c.encode_utf8(&mut buf).as_bytes());
            if used > 0 {
                self.advance_cursor(self.col + columns);
                return;
            }
        }
        self.input_general(c);
    }

    #[inline(never)]
    fn input_ascii(&mut self, text: &str) {
        self.ascii_run(text.as_bytes());
    }

    #[inline(never)]
    fn input_text(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if !self.wrap_pending && !self.insert {
                let (used, columns) = self.write_text_segment(&bytes[i..]);
                if used > 0 {
                    i += used;
                    self.advance_cursor(self.col + columns);
                    continue;
                }
            }
            let c = text[i..].chars().next().expect("a character");
            self.input_general(c);
            i += c.len_utf8();
        }
    }

    /// Ground-state text straight from the read: printable ASCII, UTF-8
    /// validated and measured in one pass, and the C0 controls a print run
    /// is interleaved with. Stops where the parser must take over.
    #[inline(never)]
    fn input_bytes(&mut self, bytes: &[u8]) -> (usize, Option<char>) {
        if !self.raw_text {
            return (0, None);
        }
        let mut i = 0;
        let mut last = None;
        while i < bytes.len() {
            let b = bytes[i];
            if (0x20..0x7F).contains(&b) {
                let end = printable_ascii_end(bytes, i + 1);
                self.ascii_run(&bytes[i..end]);
                last = Some(bytes[end - 1] as char);
                i = end;
                continue;
            }
            if b >= 0x80 {
                if !self.wrap_pending && !self.insert {
                    let (used, columns) = self.write_text_segment(&bytes[i..]);
                    if used > 0 {
                        last = last_char(&bytes[i..i + used]);
                        i += used;
                        self.advance_cursor(self.col + columns);
                        continue;
                    }
                }
                match self.tables.decode(&bytes[i..]) {
                    Some((cp, len, _)) if cp >= 0xA0 => {
                        let c = char::from_u32(cp).expect("validated");
                        self.input_general(c);
                        last = Some(c);
                        i += len;
                        continue;
                    }
                    // C1 controls, invalid or incomplete: the parser's.
                    _ => break,
                }
            }
            match b {
                b'\n' | 0x0B | 0x0C => self.linefeed_inner(),
                b'\r' => self.carriage_return(),
                b'\t' => self.put_tab(1),
                0x08 => self.backspace(),
                _ => break,
            }
            i += 1;
        }
        (i, last)
    }

    fn put_tab(&mut self, mut count: u16) {
        if self.wrap_pending {
            self.wrapline();
            return;
        }
        while self.col < self.cols && count != 0 {
            count -= 1;
            let (line, col) = (self.line, self.col);
            let cell = self.get(line, col);
            if cell.ch() == ' ' {
                let (p, r) = self.pages.locate(line);
                let occ = self.pages.page(p).meta[r].occ as usize;
                self.materialize(p, r, col + 1);
                let cols = self.cols;
                let page = self.pages.page_mut(p);
                let meta = &mut page.meta[r];
                Self::note_text(meta, occ, col, cell.style());
                if cell.0 & !(super::cell::CP | super::cell::STYLE) != 0 {
                    meta.simple = false;
                }
                if page.text.on {
                    if col >= occ && cell.style() == meta.fill && page.text.begin(r, occ, col) {
                        page.text.blanks(r, if occ == 0 { 0 } else { col - occ });
                        page.text.push(r, b'\t');
                    } else {
                        page.text.invalidate(r);
                    }
                }
                meta.dirty = true;
                let target = &mut page.cells[r * cols + col];
                target.0 = (target.0 & !super::cell::CP) | '\t' as u64;
            }
            loop {
                if self.col + 1 == self.cols {
                    break;
                }
                self.col += 1;
                if self.tabs[self.col] {
                    break;
                }
            }
        }
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.wrap_pending = false;
        }
    }

    fn carriage_return(&mut self) {
        self.col = 0;
        self.wrap_pending = false;
    }

    fn linefeed(&mut self) {
        self.linefeed_inner();
    }

    fn newline(&mut self) {
        self.linefeed_inner();
    }

    fn goto(&mut self, line: i32, col: usize) {
        self.goto_inner(line, col);
    }

    fn goto_line(&mut self, line: i32) {
        self.goto_inner(line, self.col);
    }

    fn goto_col(&mut self, col: usize) {
        self.goto_inner(self.line as i32, col);
    }

    fn move_up(&mut self, lines: usize) {
        self.goto_inner(self.line as i32 - lines as i32, self.col);
    }

    fn move_down(&mut self, lines: usize) {
        self.goto_inner((self.line + lines) as i32, self.col);
    }

    fn move_forward(&mut self, cols: usize) {
        self.col = (self.col + cols).min(self.cols - 1);
        self.wrap_pending = false;
    }

    fn move_backward(&mut self, cols: usize) {
        self.col = self.col.saturating_sub(cols);
        self.wrap_pending = false;
    }

    fn move_down_and_cr(&mut self, lines: usize) {
        self.goto_inner((self.line + lines) as i32, 0);
    }

    fn move_up_and_cr(&mut self, lines: usize) {
        self.goto_inner(self.line as i32 - lines as i32, 0);
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        let col = self.col;
        let (left, right) = match mode {
            LineClearMode::Right => (col, self.cols),
            LineClearMode::Left => (0, col + 1),
            LineClearMode::All => (0, self.cols),
        };
        let (p, r) = self.pages.locate(self.line);
        let erased = self.pen.erased();
        let cols = self.cols;
        let page = self.pages.page_mut(p);
        let id = page.styles.intern(&erased);
        let meta = &mut page.meta[r];
        if right == cols && (left == 0 || meta.fill == id) {
            if left == 0 {
                meta.fill = id;
                meta.simple = true;
                meta.lead = 0;
                page.text.reset(r);
            } else if (left as u16) < meta.occ {
                page.text.invalidate(r);
            }
            meta.occ = meta.occ.min(left as u16);
            meta.lead = meta.lead.min(meta.occ);
        } else {
            page.text.invalidate(r);
            if meta.fill != id {
                meta.simple = false;
            }
            meta.lead = meta.lead.min(left as u16);
            self.materialize(p, r, right);
            let page = self.pages.page_mut(p);
            page.cells[r * cols + left..r * cols + right].fill(Cell::blank(id));
        }
        let meta = &mut self.pages.page_mut(p).meta[r];
        meta.dirty = true;
        if right == cols {
            meta.wrapped = false;
        }
        self.wrap_pending = false;
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        let pen = &mut self.pen;
        let mut flags = Flags::from_bits_retain(pen.flags);
        match attr {
            Attr::Foreground(color) => pen.fg = color,
            Attr::Background(color) => pen.bg = color,
            Attr::UnderlineColor(color) => pen.underline = color,
            Attr::Reset => {
                *pen = Style::default();
                flags = Flags::empty();
            }
            Attr::Reverse => flags.insert(Flags::INVERSE),
            Attr::CancelReverse => flags.remove(Flags::INVERSE),
            Attr::Bold => flags.insert(Flags::BOLD),
            Attr::CancelBold => flags.remove(Flags::BOLD),
            Attr::Dim => flags.insert(Flags::DIM),
            Attr::CancelBoldDim => flags.remove(Flags::BOLD | Flags::DIM),
            Attr::Italic => flags.insert(Flags::ITALIC),
            Attr::CancelItalic => flags.remove(Flags::ITALIC),
            Attr::Underline => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(Flags::UNDERLINE);
            }
            Attr::DoubleUnderline => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(Flags::DOUBLE_UNDERLINE);
            }
            Attr::Undercurl => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(Flags::UNDERCURL);
            }
            Attr::DottedUnderline => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(Flags::DOTTED_UNDERLINE);
            }
            Attr::DashedUnderline => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(Flags::DASHED_UNDERLINE);
            }
            Attr::CancelUnderline => flags.remove(Flags::ALL_UNDERLINES),
            Attr::Hidden => flags.insert(Flags::HIDDEN),
            Attr::CancelHidden => flags.remove(Flags::HIDDEN),
            Attr::Strike => flags.insert(Flags::STRIKEOUT),
            Attr::CancelStrike => flags.remove(Flags::STRIKEOUT),
            Attr::BlinkSlow | Attr::BlinkFast => flags.insert(Flags::BLINK),
            Attr::CancelBlink => flags.remove(Flags::BLINK),
            Attr::Overline => flags.insert(Flags::OVERLINE),
            Attr::CancelOverline => flags.remove(Flags::OVERLINE),
        }
        pen.flags = flags.bits();
        // Any change re-interns lazily on the next write.
        self.pen_cache.0 = 0;
    }

    fn set_mode(&mut self, mode: Mode) {
        match mode {
            Mode::Named(NamedMode::Insert) => self.insert = true,
            Mode::Named(NamedMode::LineFeedNewLine) => self.unsupported += 1,
            _ => {}
        }
    }

    fn unset_mode(&mut self, mode: Mode) {
        if let Mode::Named(NamedMode::Insert) = mode {
            self.insert = false;
        }
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(NamedPrivateMode::LineWrap) => self.line_wrap = true,
            PrivateMode::Named(NamedPrivateMode::CursorKeys) => {}
            PrivateMode::Named(NamedPrivateMode::GraphemeClustering) => self.clustering = true,
            _ => self.unsupported += 1,
        }
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(NamedPrivateMode::LineWrap) => self.line_wrap = false,
            PrivateMode::Named(NamedPrivateMode::CursorKeys) => {}
            PrivateMode::Named(NamedPrivateMode::GraphemeClustering) => self.clustering = false,
            _ => self.unsupported += 1,
        }
    }

    // Not in the prototype: counted so the check fails loudly.
    fn clear_screen(&mut self, _: ClearMode) {
        self.unsupported += 1;
    }
    fn set_scrolling_region(&mut self, _: usize, _: Option<usize>) {
        self.unsupported += 1;
    }
    fn scroll_up(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn scroll_down(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn insert_blank_lines(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn delete_lines(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn erase_chars(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn delete_chars(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn insert_blank(&mut self, _: usize) {
        self.unsupported += 1;
    }
    fn reverse_index(&mut self) {
        self.unsupported += 1;
    }
    fn save_cursor_position(&mut self) {
        self.unsupported += 1;
    }
    fn restore_cursor_position(&mut self) {
        self.unsupported += 1;
    }
    fn reset_state(&mut self) {
        self.unsupported += 1;
    }
    fn decaln(&mut self) {
        self.unsupported += 1;
    }
    fn configure_charset(
        &mut self,
        _: alacritty_terminal::vte::ansi::CharsetIndex,
        _: alacritty_terminal::vte::ansi::StandardCharset,
    ) {
        self.unsupported += 1;
    }
    fn set_hyperlink(&mut self, _: Option<alacritty_terminal::vte::ansi::Hyperlink>) {
        self.unsupported += 1;
    }
    fn mark_prompt(&mut self) {
        self.unsupported += 1;
    }
}

/// The last character of valid UTF-8 `bytes`.
fn last_char(bytes: &[u8]) -> Option<char> {
    let start = bytes.iter().rposition(|&b| b & 0xC0 != 0x80)?;
    std::str::from_utf8(&bytes[start..]).ok()?.chars().next()
}

/// The end of the printable ASCII run that continues at `from`, sixteen
/// bytes at a time.
#[inline(always)]
fn printable_ascii_end(bytes: &[u8], mut from: usize) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        while from + 16 <= bytes.len() {
            // SAFETY: NEON is baseline on aarch64; 16 bytes are in bounds.
            let mask = unsafe {
                let v = vld1q_u8(bytes.as_ptr().add(from));
                let bad = vorrq_u8(vcltq_u8(v, vdupq_n_u8(0x20)), vcgtq_u8(v, vdupq_n_u8(0x7E)));
                vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(
                    bad,
                ))))
            };
            if mask != 0 {
                return from + (mask.trailing_zeros() / 4) as usize;
            }
            from += 16;
        }
    }
    while from < bytes.len() && (0x20..0x7F).contains(&bytes[from]) {
        from += 1;
    }
    from
}
