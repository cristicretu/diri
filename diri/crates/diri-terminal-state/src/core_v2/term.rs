//! The prototype terminal: the print path (ASCII and non-ASCII runs,
//! grapheme clusters, wrap), linefeed into page history, tabs, SGR, and
//! the cursor moves and line erase the reference CSI workload uses.
//!
//! Semantics follow the current core (`alacritty_terminal::Term` with Diri's
//! patches) cell for cell; `examples/core_v2_bench.rs` checks the final
//! state of every run against it. Anything the prototype does not implement
//! is counted in [`Core::unsupported`], so a payload that needs it fails
//! the check instead of reporting a number.

use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::grapheme::{self, BreakState, Class, Cluster, Info};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, PrivateMode, Rgb, ShellCommandMark,
    StandardCharset, TabulationClearMode,
};

use super::cell::{
    CLUSTER, Cell, GRAPHEME, LEADING, MARKS, SPACER, STYLE_SHIFT, Style, WIDE, fill_bytes,
    fill_cells,
};
use super::ops::{Inactive, Saved};
use super::page::{Clusters, Link, Pages, RowMeta, TextLog, push_mark};
use super::text::{Tables, tables};

pub struct Core {
    pub pages: Pages,
    pub(super) lines: usize,
    pub(super) cols: usize,
    pub line: usize,
    pub col: usize,
    pub wrap_pending: bool,
    pub(super) pen: Style,
    /// (page serial, interned id) of `pen`.
    pub(super) pen_cache: (u64, u16),
    pub(super) tabs: Vec<bool>,
    pub(super) clustering: bool,
    pub(super) insert: bool,
    pub(super) line_wrap: bool,
    tables: &'static Tables,
    /// Every mode, as the current core keeps them; `clustering`, `insert`
    /// and `line_wrap` mirror their bits for the print path.
    pub mode: TermMode,
    /// Scroll region: first line and one past the last.
    pub(super) region: (usize, usize),
    /// The pen's hyperlink (OSC 8), part of the cursor like its style.
    pub(super) link: Option<Link>,
    /// Character sets G0..G3, part of the cursor; and the active one.
    pub(super) charsets: [StandardCharset; 4],
    pub(super) active_charset: usize,
    /// Text must take the general path: a hyperlink or a character set
    /// that maps ASCII is active.
    pub(super) slow_text: bool,
    /// DECSC state of this screen.
    pub(super) saved: Saved,
    /// The other screen (alternate while on the primary, and the reverse),
    /// created on first use.
    pub(super) other: Option<Box<Inactive>>,
    /// Titles, colors, cursor style, keyboard protocol, options and the
    /// event listener.
    pub(super) events: super::events::Events,
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
            mode: TermMode::default(),
            region: (0, lines),
            link: None,
            charsets: [StandardCharset::Ascii; 4],
            active_charset: 0,
            slow_text: false,
            saved: Saved::default(),
            other: None,
            events: Default::default(),
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
        self.primary_pages().history_bytes()
    }

    /// Heap bytes held by pages, both screens.
    pub fn heap_bytes(&self) -> usize {
        self.pages.heap_bytes() + self.other.as_ref().map_or(0, |o| o.pages_heap_bytes())
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
    pub(super) fn set(&mut self, line: usize, col: usize, f: impl FnOnce(Cell) -> Cell) {
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
        if self.col + 1 == self.cols {
            let meta = &mut self.pages.page_mut(p).meta[r];
            meta.wrapped = true;
            meta.dirty = true;
        } else {
            // A restored cursor can wait to wrap before the last column;
            // the mark goes on the cursor cell, as in the current core.
            let (line, col) = (self.line, self.col);
            self.set(line, col, |cell| Cell(cell.0 | super::cell::WRAP));
        }
        if self.line + 1 >= self.region.1 {
            self.linefeed_inner();
        } else {
            self.line += 1;
        }
        self.col = 0;
        self.wrap_pending = false;
    }

    /// `Term::write_at_cursor`: `extra` is WIDE, SPACER or LEADING.
    fn write_at_cursor(&mut self, c: char, extra: u64) {
        let c = self.charsets[self.active_charset].map(c);
        let (line, col) = (self.line, self.col);
        let (p, r) = self.pages.locate(line);
        let id = self.pen_id(p);
        let style = (id as u64) << STYLE_SHIFT;
        self.write_at_cursor_inner(c, extra, p, r, id, style);
        if self.link.is_some() {
            self.link_cell(p, r, col);
        }
    }

    fn write_at_cursor_inner(
        &mut self,
        c: char,
        extra: u64,
        p: usize,
        r: usize,
        id: u16,
        style: u64,
    ) {
        let (line, col) = (self.line, self.col);
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
                let fresh = col >= occ
                    && id == fill
                    && self.link.is_none()
                    && (c < '\u{80}' || extra != 0 || Info::of(c).width() != 2)
                    && page.text.begin(r, occ, col);
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
            self.shift_right(self.line, self.col, width);
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
            // Read before the leading spacer overwrites the cell and the
            // wrap scrolls `line` away.
            let link = cluster
                .has(super::cell::HYPERLINK)
                .then(|| self.link_at(line as i32, col))
                .flatten();
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
            if let Some(link) = link {
                let (p, r) = self.pages.locate(l);
                let page = self.pages.page_mut(p);
                let id = page.link_id(&link);
                page.set_link(r * cols + c, id);
            }
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
            self.shift_right(self.line, self.col, 1);
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
            fill_cells(&mut row[occ..x], Cell::blank(meta.fill));
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
            fill_bytes(&mut row[x + overlap..x + n], &seg[overlap..], style);
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
    #[inline(never)]
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
            fill_cells(&mut row[occ..x0], Cell::blank(meta.fill));
        }
        let text = &mut page.text;
        let id = (style >> STYLE_SHIFT) as u16;
        // Fresh cells in the fill style extend the row's text tokens.
        let log = x0 >= occ && id == meta.fill && text.begin(r, occ, x0);
        if log && occ > 0 {
            text.blanks(r, x0 - occ);
        }
        // Tokens are written unchecked into room for every cell left on
        // the row, and counted towards the row once at the end.
        let log_from = text.len();
        if log {
            text.reserve_cells(cols - x0);
        }
        // SAFETY (every use of `cur`): the cursor stays within the room
        // reserved for the row's remaining cells, and is committed before
        // anything else touches the arena.
        let mut cur = text.cursor();
        let mut x = x0;
        let mut i = 0;
        let mut marked = false;
        let mut cps = [0u32; 16];
        // After a vector run that stopped at a character it rejected, that
        // character goes to the scalar decode directly.
        let mut rejected = false;
        'row: while i < bytes.len() && x < cols {
            if !std::mem::take(&mut rejected)
                && bytes.len() - i >= 48
                && cols - x >= 4
                && bytes[i] & 0xF0 == 0xE0
            {
                let found = tables.wide_run16(&bytes[i..], &mut cps);
                let fits = found.min((cols - x) / 2);
                rejected = found > 0 && fits == found && found < 16;
                if fits > 0 && x >= occ {
                    // Fresh cells: nothing to keep or clear.
                    let (glyph, spacer) = (WIDE | style, ' ' as u64 | SPACER | style);
                    for (pair, &cp) in row[x..x + 2 * fits]
                        .as_chunks_mut::<2>()
                        .0
                        .iter_mut()
                        .zip(&cps[..fits])
                    {
                        pair[0].0 = cp as u64 | glyph;
                        pair[1].0 = spacer;
                    }
                    if log {
                        // SAFETY: room for the row's cells was reserved.
                        cur = unsafe { TextLog::put_chars3(cur, &cps, fits, &bytes[i..]) };
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
            let Some((cp, len, class)) = tables.decode(&bytes[i..]) else {
                break;
            };
            let width = if class != 0 {
                class as usize
            } else {
                match classify_rare(cp, row, x, &page.clusters) {
                    Rare::Width(width) => width,
                    Rare::Mark(base, c) => {
                        push_mark(row, &mut page.clusters, base, c);
                        marked = true;
                        if log {
                            unsafe { text.commit(cur) };
                            text.put_mark(&bytes[i..i + len], cols - x);
                            cur = text.cursor();
                        }
                        i += len;
                        continue;
                    }
                    Rare::General => break,
                }
            };
            if x + width > cols {
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
                // SAFETY: room for the row's cells was reserved.
                cur = unsafe { TextLog::put_char(cur, cp as u64, &bytes[i..]) };
            }
            x += width;
            i += len;
        }
        if log {
            unsafe { text.commit(cur) };
            text.add_to_row(r, log_from);
        }
        if x > x0 || x0 > occ || marked {
            // Written cells, or at least the blanks of the gap before x0.
            Self::note_text(meta, occ, x0, id);
            if text.on && !log {
                text.invalidate(r);
            }
        }
        meta.occ = occ.max(x) as u16;
        if marked {
            meta.dirty = true;
            meta.simple = false;
        }
        if x > x0 {
            meta.dirty = true;
            if x == cols {
                meta.wrapped = false;
            }
        }
        (i, x - x0)
    }

    /// Printable ASCII bytes.
    #[inline(never)]
    fn ascii_run(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if self.wrap_pending && !self.insert && !self.slow_text && self.line_wrap {
                // What `input_general` does first for a character that
                // starts its own cluster, as printable ASCII always does.
                self.wrapline();
            }
            if self.wrap_pending || self.insert || self.slow_text {
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
        if next == self.region.1 {
            if self.region == (0, self.lines) {
                let fill = self.pen.erased();
                self.pages.push_row(&fill);
            } else {
                self.scroll_up_from(self.region.0, 1);
            }
        } else if next < self.lines {
            self.line = next;
        }
    }

    pub(super) fn goto_inner(&mut self, line: i32, col: usize) {
        let (offset, max) = if self.mode.contains(TermMode::ORIGIN) {
            (self.region.0 as i32, self.region.1 as i32 - 1)
        } else {
            (0, self.lines as i32 - 1)
        };
        self.line = (line + offset).min(max).max(0) as usize;
        self.col = col.min(self.cols - 1);
        self.wrap_pending = false;
    }
}

impl Handler for Core {
    /// REP of printable ASCII: row segments of the repeated byte.
    fn input_repeat(&mut self, c: char, count: usize) {
        if self.slow_text || !(' '..'\x7f').contains(&c) {
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
        if self.slow_text {
            self.input_general(c);
            return;
        }
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
            if !self.wrap_pending && !self.insert && !self.slow_text {
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
        if !self.raw_text || self.slow_text {
            return (0, None);
        }
        let mut i = 0;
        let mut last = None;
        // The end of the last text segment when it printed last: decoded
        // once, on the way out.
        let mut last_text = None;
        while i < bytes.len() {
            let b = bytes[i];
            if (0x20..0x7F).contains(&b) {
                let end = printable_ascii_end(bytes, i + 1);
                self.ascii_run(&bytes[i..end]);
                last = Some(bytes[end - 1] as char);
                last_text = None;
                i = end;
                continue;
            }
            if b >= 0x80 {
                // The table's classes 1 and 2 start their own cluster.
                let own = || matches!(self.tables.decode(&bytes[i..]), Some((_, _, 1 | 2)));
                if self.wrap_pending && !self.insert && self.line_wrap && own() {
                    self.wrapline();
                }
                if !self.wrap_pending && !self.insert {
                    let (used, columns) = self.write_text_segment(&bytes[i..]);
                    if used > 0 {
                        last_text = Some(i + used);
                        i += used;
                        self.advance_cursor(self.col + columns);
                        continue;
                    }
                    // A two-column character in the last column: a leading
                    // spacer and a wrap, as `input_general` does, then the
                    // character goes through the text path on the next row.
                    if self.line_wrap
                        && self.col + 1 == self.cols
                        && self.cols >= 2
                        && matches!(self.tables.decode(&bytes[i..]), Some((_, _, 2)))
                    {
                        self.write_at_cursor(' ', LEADING);
                        self.wrapline();
                        continue;
                    }
                }
                match self.tables.decode(&bytes[i..]) {
                    Some((cp, len, _)) if cp >= 0xA0 => {
                        let c = char::from_u32(cp).expect("validated");
                        self.input_general(c);
                        last = Some(c);
                        last_text = None;
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
        if let Some(end) = last_text {
            last = last_char(&bytes[..end]);
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
        if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            self.carriage_return();
        }
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
        self.set_mode_inner(mode, true);
    }

    fn unset_mode(&mut self, mode: Mode) {
        self.set_mode_inner(mode, false);
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        self.set_private_mode_inner(mode, true);
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        self.set_private_mode_inner(mode, false);
    }

    fn set_keypad_application_mode(&mut self) {
        self.mode.insert(TermMode::APP_KEYPAD);
    }

    fn unset_keypad_application_mode(&mut self) {
        self.mode.remove(TermMode::APP_KEYPAD);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        self.clear_screen_inner(mode);
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.set_scrolling_region_inner(top, bottom);
    }

    fn scroll_up(&mut self, lines: usize) {
        self.scroll_up_from(self.region.0, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll_down_from(self.region.0, lines);
    }

    fn insert_blank_lines(&mut self, lines: usize) {
        let origin = self.line;
        if (self.region.0..self.region.1).contains(&origin) {
            self.scroll_down_from(origin, lines);
        }
    }

    fn delete_lines(&mut self, lines: usize) {
        let origin = self.line;
        let lines = lines.min(self.lines - origin);
        if lines > 0 && (self.region.0..self.region.1).contains(&origin) {
            self.scroll_up_from(origin, lines);
        }
    }

    fn erase_chars(&mut self, count: usize) {
        let end = (self.col + count).min(self.cols);
        self.erase_cells(self.line, self.col, end);
    }

    fn delete_chars(&mut self, count: usize) {
        self.delete_chars_inner(count);
    }

    fn insert_blank(&mut self, count: usize) {
        self.insert_blank_cells(count);
    }

    fn reverse_index(&mut self) {
        if self.line == self.region.0 {
            self.scroll_down_from(self.region.0, 1);
        } else {
            self.line = self.line.saturating_sub(1);
        }
    }

    fn set_horizontal_tabstop(&mut self) {
        self.tabs[self.col] = true;
    }

    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        match mode {
            TabulationClearMode::Current => self.tabs[self.col] = false,
            TabulationClearMode::All => self.tabs.fill(false),
        }
    }

    fn move_forward_tabs(&mut self, count: u16) {
        for _ in 0..count {
            let mut col = self.col;
            if col == self.cols - 1 {
                break;
            }
            for i in col + 1..self.cols {
                col = i;
                if self.tabs[col] {
                    break;
                }
            }
            self.col = col;
        }
    }

    fn move_backward_tabs(&mut self, count: u16) {
        for _ in 0..count {
            let mut col = self.col;
            if col == 0 {
                break;
            }
            for i in (0..col).rev() {
                if self.tabs[i] {
                    col = i;
                    break;
                }
            }
            self.col = col;
        }
    }

    fn save_cursor_position(&mut self) {
        self.saved = self.cursor_state();
    }

    fn restore_cursor_position(&mut self) {
        let saved = self.saved.clone();
        self.set_cursor_state(saved);
    }

    fn reset_state(&mut self) {
        self.reset_state_inner();
    }

    fn soft_reset(&mut self) {
        self.soft_reset_inner();
    }

    fn decaln(&mut self) {
        self.decaln_inner();
    }

    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        self.charsets[index as usize] = charset;
        self.update_slow_text();
    }

    fn set_active_charset(&mut self, index: CharsetIndex) {
        self.active_charset = index as usize;
        self.update_slow_text();
    }

    fn set_hyperlink(&mut self, link: Option<Hyperlink>) {
        self.link = link.map(|link| Link {
            id: link.id,
            uri: link.uri,
        });
        self.update_slow_text();
    }

    fn mark_prompt(&mut self) {
        self.mark_prompt_inner();
    }

    fn mark_shell_command(&mut self, mark: ShellCommandMark) {
        self.mark_command(mark);
    }

    fn bell(&mut self) {
        self.send(alacritty_terminal::event::Event::Bell);
    }

    fn identify_terminal(&mut self, intermediate: Option<char>) {
        self.identify_terminal_inner(intermediate);
    }

    fn report_keyboard_mode(&mut self) {
        self.report_keyboard_mode_inner();
    }

    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        self.push_keyboard_mode_inner(mode);
    }

    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        self.pop_keyboard_modes_inner(to_pop);
    }

    fn set_keyboard_mode(&mut self, mode: KeyboardModes, apply: KeyboardModesApplyBehavior) {
        self.set_keyboard_mode_inner(mode, apply);
    }

    fn device_status(&mut self, arg: usize) {
        self.device_status_inner(arg);
    }

    fn private_device_status(&mut self, arg: usize) {
        self.private_device_status_inner(arg);
    }

    fn set_color(&mut self, index: usize, color: Rgb) {
        self.set_color_inner(index, color);
    }

    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        self.dynamic_color_sequence_inner(prefix, index, terminator);
    }

    fn reset_color(&mut self, index: usize) {
        self.reset_color_inner(index);
    }

    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        self.clipboard_store_inner(clipboard, base64);
    }

    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        self.clipboard_load_inner(clipboard, terminator);
    }

    fn report_mode(&mut self, mode: Mode) {
        self.report_mode_inner(mode);
    }

    fn report_private_mode(&mut self, mode: PrivateMode) {
        self.report_private_mode_inner(mode);
    }

    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        self.set_cursor_style_inner(style);
    }

    fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.set_cursor_shape_inner(shape);
    }

    fn set_title(&mut self, title: Option<String>) {
        self.set_title_inner(title);
    }

    fn push_title(&mut self) {
        self.push_title_inner();
    }

    fn pop_title(&mut self) {
        self.pop_title_inner();
    }

    fn text_area_size_pixels(&mut self) {
        self.text_area_size_pixels_inner();
    }

    fn cell_size_pixels(&mut self) {
        self.cell_size_pixels_inner();
    }

    fn text_area_size_chars(&mut self) {
        self.text_area_size_chars_inner();
    }

    fn screen_size_chars(&mut self) {
        self.screen_size_chars_inner();
    }

    fn report_version(&mut self) {
        self.report_version_inner();
    }

    fn request_termcap(&mut self, names: &[u8]) {
        self.request_termcap_inner(names);
    }

    fn request_setting(&mut self, setting: &[u8]) {
        self.request_setting_inner(setting);
    }
}

/// How the text path places a character the class table sends to the
/// general path (class 0).
enum Rare {
    /// It starts its own cluster: write it like any other character.
    Width(usize),
    /// A zero-width mark that goes straight onto the cluster of the cell
    /// at this index, as `input_general` would put it.
    Mark(usize, char),
    /// Anything else: the general path.
    General,
}

/// Places a class-0 character at `x` on a row the text path is writing
/// (no wrap pending, no insert mode). Every cell before `x` is real.
/// Mirrors `input_general` and `continue_cluster` for the cases it accepts:
/// - characters past the BMP that never join (the table covers only the
///   BMP);
/// - emoji and Indic consonants, which `continue_cluster` joins only to a
///   cell that already holds a cluster;
/// - zero-width marks that are not presentation marks, onto a cell
///   `continue_cluster` would not extend (`mark_only`), or any mark that
///   cannot join (both go straight onto the cell).
#[inline]
fn classify_rare(cp: u32, row: &[Cell], x: usize, clusters: &Clusters) -> Rare {
    if cp < 0xA0 {
        return Rare::General;
    }
    // SAFETY: `Tables::decode` returns only scalar values.
    let c = unsafe { char::from_u32_unchecked(cp) };
    let info = Info::of(c);
    let width = info.width();
    let joins = grapheme::may_join(c, info);
    // The cell `continue_cluster` looks at: the one before `x`, or the
    // character of a spacer there.
    let base = || -> Option<usize> {
        let mut base = x.checked_sub(1)?;
        if row[base].has(SPACER) {
            base = base.checked_sub(1)?;
        }
        Some(base)
    };
    if width > 0 {
        if !joins {
            return Rare::Width(width);
        }
        if !matches!(
            info.class(),
            Class::ExtendedPictographic | Class::EmojiModifierBase | Class::IndicConsonant
        ) {
            return Rare::General;
        }
        // At column 0, or after a spacer at column 0, it cannot join; a
        // spacer or leading spacer as the base declines too; otherwise it
        // joins only a cell whose cluster has extra codepoints.
        return match base() {
            Some(base) if row[base].has(GRAPHEME) => Rare::General,
            _ => Rare::Width(width),
        };
    }
    if grapheme::is_presentation_mark(c) || clusters.full() {
        return Rare::General;
    }
    let Some(base) = base() else {
        return Rare::General;
    };
    let cell = row[base];
    let mark_only = cell.has(GRAPHEME) || {
        let b = cell.ch();
        !Cluster::new(b, Info::of(b)).presentation()
    };
    if mark_only || !joins {
        Rare::Mark(base, c)
    } else {
        Rare::General
    }
}

/// The last character of valid UTF-8 `bytes`.
fn last_char(bytes: &[u8]) -> Option<char> {
    let start = bytes.iter().rposition(|&b| b & 0xC0 != 0x80)?;
    let (cp, len, _) = tables().decode(&bytes[start..])?;
    debug_assert_eq!(start + len, bytes.len());
    char::from_u32(cp)
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
