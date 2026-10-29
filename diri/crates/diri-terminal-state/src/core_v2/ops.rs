//! Grid operations beyond the print path: scroll regions, line and
//! character insertion and deletion, erase, the alternate screen, cursor
//! save and restore, modes, resets, marks and hyperlinks.
//!
//! Each is a port of the current core's `Term` / `Grid` method of the same
//! job (named in its doc), with the same results cell for cell; the
//! differential tests in `tests.rs` compare them family by family.

use std::collections::HashMap;

use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{
    ClearMode, Mode, NamedMode, NamedPrivateMode, PrivateMode, ShellCommandMark, StandardCharset,
};

use super::Core;
use super::cell::{
    CLUSTER, CP, Cell, GRAPHEME, HYPERLINK, MARKS_SHIFT, STYLE, STYLE_SHIFT, Style, WRAP,
};
use super::page::{Link, Pages, RowMeta};

/// OSC 133 marks in the cell's MARKS bits.
pub const MARK_PROMPT: u64 = 1 << MARKS_SHIFT;
pub const MARK_INPUT: u64 = 1 << (MARKS_SHIFT + 1);
pub const MARK_OUTPUT: u64 = 1 << (MARKS_SHIFT + 2);
pub const MARK_SUCCEEDED: u64 = 1 << (MARKS_SHIFT + 3);
pub const MARK_FAILED: u64 = 1 << (MARKS_SHIFT + 4);

/// The cursor as DECSC saves it: position, pending wrap, pen (style and
/// hyperlink) and character sets.
#[derive(Clone, Debug, Default)]
pub struct Saved {
    pub line: usize,
    pub col: usize,
    pub wrap_pending: bool,
    pub pen: Style,
    pub link: Option<Link>,
    pub charsets: [StandardCharset; 4],
}

/// The screen not shown: its pages, cursor and saved cursor.
pub struct Inactive {
    pages: Pages,
    cursor: Saved,
    saved: Saved,
    /// Screen swaps so far, for damage tracking.
    swaps: u64,
}

/// `AltScreen` of the current core: what entering or leaving does to the
/// cursor and the alternate screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AltScreen {
    /// 1049: save the cursor, clear the alternate screen on entry.
    SaveCursorAndClear,
    /// 47: one cursor, nothing cleared.
    Shared,
    /// 1047: one cursor, the alternate screen cleared on exit.
    ClearOnExit,
}

impl Inactive {
    pub(super) fn pages_heap_bytes(&self) -> usize {
        self.pages.heap_bytes()
    }
}

impl Core {
    /// The primary screen's pages (history lives there), whichever screen
    /// is shown.
    pub(super) fn primary_pages(&self) -> &Pages {
        match &self.other {
            Some(other) if self.mode.contains(TermMode::ALT_SCREEN) => &other.pages,
            _ => &self.pages,
        }
    }

    /// The primary screen's pages, whichever screen is shown.
    pub(super) fn primary_pages_mut(&mut self) -> &mut Pages {
        match &mut self.other {
            Some(other) if self.mode.contains(TermMode::ALT_SCREEN) => &mut other.pages,
            _ => &mut self.pages,
        }
    }

    /// How many times the shown screen was swapped with the other one.
    pub(super) fn screen_swaps(&self) -> u64 {
        self.other.as_ref().map_or(0, |other| other.swaps)
    }

    /// Keeps the print path's copies of mode bits and its slow-text gate
    /// in step.
    pub(super) fn sync_mode(&mut self) {
        self.insert = self.mode.contains(TermMode::INSERT);
        self.line_wrap = self.mode.contains(TermMode::LINE_WRAP);
        self.clustering = self.mode.contains(TermMode::GRAPHEME_CLUSTERING);
    }

    pub(super) fn update_slow_text(&mut self) {
        self.slow_text =
            self.link.is_some() || self.charsets[self.active_charset] != StandardCharset::Ascii;
    }

    pub(super) fn cursor_state(&self) -> Saved {
        Saved {
            line: self.line,
            col: self.col,
            wrap_pending: self.wrap_pending,
            pen: self.pen,
            link: self.link.clone(),
            charsets: self.charsets,
        }
    }

    pub(super) fn set_cursor_state(&mut self, state: Saved) {
        self.line = state.line.min(self.lines - 1);
        self.col = state.col.min(self.cols - 1);
        self.wrap_pending = state.wrap_pending;
        self.pen = state.pen;
        self.pen_cache = (0, 0);
        self.link = state.link;
        self.charsets = state.charsets;
        self.update_slow_text();
    }

    // ---- rows ----

    /// `Row::reset` with the cursor's template: every cell a blank of the
    /// pen's background, no soft wrap.
    pub(super) fn reset_row(&mut self, line: usize) {
        self.erase_cells(line, 0, self.cols);
    }

    /// Cells `left..right` of screen `line` become blanks of the pen's
    /// background (`bg.into()`), as erase in line, ECH and ED do.
    pub(super) fn erase_cells(&mut self, line: usize, left: usize, right: usize) {
        if left >= right {
            return;
        }
        let (p, r) = self.pages.locate(line);
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
            self.materialize_row(p, r, right);
            let page = self.pages.page_mut(p);
            page.cells[r * cols + left..r * cols + right].fill(Cell::blank(id));
        }
        let meta = &mut self.pages.page_mut(p).meta[r];
        meta.dirty = true;
        if right == cols {
            meta.wrapped = false;
        }
    }

    fn materialize_row(&mut self, p: usize, r: usize, upto: usize) {
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

    /// Runs `f` over the whole of screen `line`'s cells (made real), each
    /// with its hyperlink index, with the row's soft-wrap mark carried as
    /// the WRAP bit of its last cell, so that cells move exactly as the
    /// current core moves them (links and WRAPLINE included).
    fn with_row_cells(&mut self, line: usize, f: impl FnOnce(&mut [(Cell, Option<u32>)])) {
        let (p, r) = self.pages.locate(line);
        let cols = self.cols;
        self.materialize_row(p, r, cols);
        let page = self.pages.page_mut(p);
        page.text.invalidate(r);
        let meta = &mut page.meta[r];
        meta.simple = false;
        meta.lead = 0;
        meta.dirty = true;
        let wrapped = std::mem::take(&mut meta.wrapped);
        let mut row: Vec<(Cell, Option<u32>)> = (0..cols)
            .map(|x| {
                let cell = page.cells[r * cols + x];
                let link = cell
                    .has(HYPERLINK)
                    .then(|| page.link_of(r * cols + x))
                    .flatten();
                (cell, link)
            })
            .collect();
        if wrapped {
            row[cols - 1].0.0 |= WRAP;
        }
        f(&mut row);
        if row[cols - 1].0.0 & WRAP != 0 {
            row[cols - 1].0.0 &= !WRAP;
            page.meta[r].wrapped = true;
        }
        for (x, (cell, link)) in row.into_iter().enumerate() {
            page.cells[r * cols + x] = cell;
            if let Some(link) = link {
                page.set_link(r * cols + x, link);
            }
        }
    }

    /// The current core's insert shift (`Term::input` in insert mode, and
    /// `insert_blank` before it clears): `count` cells from `col` move right
    /// by swaps, the last ones landing at `col`.
    pub(super) fn shift_right(&mut self, line: usize, col: usize, count: usize) {
        self.shift_right_inner(line, col, count, false);
    }

    /// `Term::insert_blank`: the shift, then blanks of the pen's background
    /// over the cells it opened.
    pub(super) fn insert_blank_cells(&mut self, count: usize) {
        let count = count.min(self.cols - self.col);
        self.shift_right_inner(self.line, self.col, count, true);
    }

    fn shift_right_inner(&mut self, line: usize, col: usize, count: usize, blank: bool) {
        let cols = self.cols;
        let (p, _) = self.pages.locate(line);
        let bg = if blank {
            let erased = self.pen.erased();
            Some(Cell::blank(self.pages.page_mut(p).styles.intern(&erased)))
        } else {
            None
        };
        self.with_row_cells(line, |row| {
            if count == 0 {
                return;
            }
            let dest = col + count;
            if dest < cols {
                for offset in (0..cols - dest).rev() {
                    row.swap(dest + offset, col + offset);
                }
            }
            if let Some(bg) = bg {
                row[col..dest.min(cols)].fill((bg, None));
            }
        });
    }

    /// `Term::delete_chars`.
    pub(super) fn delete_chars_inner(&mut self, count: usize) {
        let cols = self.cols;
        let count = count.min(cols);
        let start = self.col;
        let end = (start + count).min(cols - 1);
        let (p, _) = self.pages.locate(self.line);
        let erased = self.pen.erased();
        let bg = Cell::blank(self.pages.page_mut(p).styles.intern(&erased));
        self.with_row_cells(self.line, |row| {
            for offset in 0..cols - end {
                row.swap(start + offset, end + offset);
            }
            row[cols - count..].fill((bg, None));
        });
    }

    /// Copies screen row `src` onto `dst` (cells, row facts, and the
    /// clusters, links and styles they name, re-interned when the rows are
    /// in different pages).
    fn copy_row(&mut self, src: usize, dst: usize) {
        if src == dst {
            return;
        }
        let cols = self.cols;
        let (sp, sr) = self.pages.locate(src);
        let (dp, dr) = self.pages.locate(dst);
        let source = self.pages.page(sp);
        let meta: RowMeta = source.meta[sr];
        let occ = meta.occ as usize;
        let cells: Vec<Cell> = source.cells[sr * cols..sr * cols + occ].to_vec();
        let fill = source.styles.get(meta.fill);
        let mut styles: HashMap<u16, Style> = HashMap::new();
        let mut clusters = Vec::new();
        let mut links = Vec::new();
        for (x, cell) in cells.iter().enumerate() {
            styles
                .entry(cell.style())
                .or_insert_with(|| source.styles.get(cell.style()));
            if cell.has(GRAPHEME) {
                clusters.push((x, source.clusters.of(*cell).to_vec()));
            }
            if cell.has(HYPERLINK)
                && let Some(link) = self.pages.read_link(sp, sr * cols + x)
            {
                links.push((x, link));
            }
        }
        let target = self.pages.page_mut(dp);
        let ids: HashMap<u16, u16> = styles
            .into_iter()
            .map(|(id, style)| (id, target.styles.intern(&style)))
            .collect();
        let fill = target.styles.intern(&fill);
        for (x, cell) in cells.iter().enumerate() {
            let bits = cell.0 & !(STYLE | CLUSTER | GRAPHEME | HYPERLINK);
            target.cells[dr * cols + x] = Cell(bits | (ids[&cell.style()] as u64) << STYLE_SHIFT);
        }
        target.meta[dr] = RowMeta {
            fill,
            dirty: true,
            ..meta
        };
        target.text.invalidate(dr);
        for (x, chars) in clusters {
            for c in chars {
                target.push_zerowidth(dr * cols + x, c);
            }
        }
        for (x, link) in links {
            let id = target.link_id(&link);
            target.set_link(dr * cols + x, id);
            target.cells[dr * cols + x].0 |= HYPERLINK;
        }
    }

    /// `Term::scroll_up_relative` and `Grid::scroll_up`: lines from
    /// `origin` to the region's end move up by `n`; with `origin` at the
    /// top of the screen the lines leaving go to history.
    pub(super) fn scroll_up_from(&mut self, origin: usize, n: usize) {
        let end = self.region.1;
        let n = n.min(self.region.1 - self.region.0);
        if n == 0 {
            return;
        }
        if end - origin <= n && origin != 0 {
            for y in origin..end {
                self.reset_row(y);
            }
            return;
        }
        if origin == 0 {
            let fill = self.pen.erased();
            for _ in 0..n {
                self.pages.push_row(&fill);
            }
            if end < self.lines {
                // The lines below the region stay where they were.
                for y in (end..self.lines).rev() {
                    self.copy_row(y - n, y);
                }
                for y in end - n..end {
                    self.reset_row(y);
                }
            }
            return;
        }
        for y in origin..end - n {
            self.copy_row(y + n, y);
        }
        for y in end - n..end {
            self.reset_row(y);
        }
    }

    /// `Term::scroll_down_relative` and `Grid::scroll_down`.
    pub(super) fn scroll_down_from(&mut self, origin: usize, n: usize) {
        let end = self.region.1;
        let n = n.min(self.region.1 - self.region.0).min(end - origin);
        if n == 0 {
            return;
        }
        if end - origin <= n {
            for y in origin..end {
                self.reset_row(y);
            }
            return;
        }
        for y in (origin + n..end).rev() {
            self.copy_row(y - n, y);
        }
        for y in origin..origin + n {
            self.reset_row(y);
        }
    }

    // ---- erase ----

    /// `Cell::is_empty` for the cell at screen `line`, `col`.
    fn is_empty_cell(&self, line: usize, col: usize) -> bool {
        let cell = self.cell(line as i32, col);
        let cp = cell.0 & CP;
        if !(cp == 0 || cp == ' ' as u64 || cp == '\t' as u64) {
            return false;
        }
        if cell.has(super::cell::SPACER | super::cell::LEADING | GRAPHEME | WRAP) {
            return false;
        }
        if col + 1 == self.cols && self.wrapped(line as i32) {
            return false;
        }
        let style = self.style_of(line as i32, cell);
        let blocking = alacritty_terminal::term::cell::Flags::INVERSE
            | alacritty_terminal::term::cell::Flags::ALL_UNDERLINES
            | alacritty_terminal::term::cell::Flags::STRIKEOUT;
        style.fg == Style::default().fg
            && style.bg == Style::default().bg
            && style.flags & blocking.bits() == 0
    }

    /// `Grid::clear_viewport` (ED 2 on the primary screen): the lines up to
    /// the last non-empty one scroll into history, the screen is blank.
    fn clear_viewport(&mut self) {
        // With no history the current core's backward search ends on the
        // first cell instead of above it: an empty screen still scrolls one
        // line.
        let mut positions = usize::from(self.history_size() == 0);
        'find: for line in (0..self.lines).rev() {
            for col in (0..self.cols).rev() {
                if !self.is_empty_cell(line, col) {
                    positions = line + 1;
                    break 'find;
                }
            }
        }
        let fill = self.pen.erased();
        for _ in 0..positions {
            self.pages.push_row(&fill);
        }
        for y in 0..self.lines - positions {
            self.reset_row(y);
        }
    }

    /// `Term::clear_screen`.
    pub(super) fn clear_screen_inner(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::Above => {
                if self.line > 1 {
                    for y in 0..self.line {
                        self.reset_row(y);
                    }
                }
                let end = (self.col + 1).min(self.cols);
                self.erase_cells(self.line, 0, end);
            }
            ClearMode::Below => {
                self.erase_cells(self.line, self.col, self.cols);
                for y in self.line + 1..self.lines {
                    self.reset_row(y);
                }
            }
            ClearMode::All => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    for y in 0..self.lines {
                        self.reset_row(y);
                    }
                } else {
                    self.clear_viewport();
                }
            }
            ClearMode::Saved => {
                if self.history_size() > 0 {
                    self.pages.clear_history();
                }
            }
        }
    }

    /// `Term::set_scrolling_region`.
    pub(super) fn set_scrolling_region_inner(&mut self, top: usize, bottom: Option<usize>) {
        let bottom = bottom.unwrap_or(self.lines);
        if top >= bottom {
            return;
        }
        let start = (top as i64 - 1).max(0) as usize;
        self.region = (start.min(self.lines), bottom.min(self.lines));
        self.goto_inner(0, 0);
    }

    /// `Term::decaln`: every cell an `E` in the default style.
    pub(super) fn decaln_inner(&mut self) {
        let cols = self.cols;
        for line in 0..self.lines {
            let (p, r) = self.pages.locate(line);
            let page = self.pages.page_mut(p);
            page.cells[r * cols..(r + 1) * cols].fill(Cell('E' as u64));
            page.text.invalidate(r);
            let meta = &mut page.meta[r];
            meta.occ = cols as u16;
            meta.lead = 0;
            meta.wrapped = false;
            meta.simple = false;
            meta.dirty = true;
        }
    }

    // ---- marks and links ----

    /// `Term::mark_prompt`: OSC 133 A on the cursor cell, not on the
    /// alternate screen.
    pub(super) fn mark_prompt_inner(&mut self) {
        self.set_marks(|marks| marks | MARK_PROMPT);
    }

    /// `Term::mark_shell_command`.
    pub(super) fn mark_command(&mut self, mark: ShellCommandMark) {
        match mark {
            ShellCommandMark::InputStart => self.set_marks(|m| m | MARK_INPUT),
            ShellCommandMark::OutputStart => self.set_marks(|m| m | MARK_OUTPUT),
            ShellCommandMark::End(status) => self.set_marks(|m| {
                (m & !(MARK_SUCCEEDED | MARK_FAILED))
                    | match status {
                        Some(0) => MARK_SUCCEEDED,
                        Some(_) => MARK_FAILED,
                        None => MARK_SUCCEEDED | MARK_FAILED,
                    }
            }),
        }
    }

    fn set_marks(&mut self, f: impl FnOnce(u64) -> u64) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            return;
        }
        let (line, col) = (self.line, self.col);
        self.set(line, col, |cell| {
            let marks = cell.0 & super::cell::MARKS;
            Cell((cell.0 & !super::cell::MARKS) | f(marks))
        });
    }

    /// Gives the cell just written at screen row `r` of page `p`, column
    /// `col`, the pen's hyperlink.
    pub(super) fn link_cell(&mut self, p: usize, r: usize, col: usize) {
        let Some(link) = self.link.clone() else {
            return;
        };
        let cols = self.cols;
        let page = self.pages.page_mut(p);
        let id = page.link_id(&link);
        page.set_link(r * cols + col, id);
        page.cells[r * cols + col].0 |= HYPERLINK;
        page.text.invalidate(r);
        page.meta[r].simple = false;
    }

    /// The hyperlink of the cell at `line` (negative: history), `col`.
    pub fn link_at(&self, line: i32, col: usize) -> Option<Link> {
        if !self.cell(line, col).has(HYPERLINK) {
            return None;
        }
        let (p, r) = self.pages.locate_line(line);
        self.pages.read_link(p, r * self.cols + col)
    }

    // ---- modes and screens ----

    /// `Term::set_mode` / `unset_mode`.
    pub(super) fn set_mode_inner(&mut self, mode: Mode, on: bool) {
        let Mode::Named(mode) = mode else {
            return;
        };
        let bit = match mode {
            NamedMode::Insert => TermMode::INSERT,
            NamedMode::LineFeedNewLine => TermMode::LINE_FEED_NEW_LINE,
        };
        self.mode.set(bit, on);
        self.sync_mode();
    }

    /// `Term::set_private_mode` / `unset_private_mode`, for the modes that
    /// change the grid or its cursor; the rest only flip their bit.
    pub(super) fn set_private_mode_inner(&mut self, mode: PrivateMode, on: bool) {
        let PrivateMode::Named(mode) = mode else {
            return;
        };
        let alt = self.mode.contains(TermMode::ALT_SCREEN);
        match mode {
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if on != alt {
                    self.swap_alt_screen(AltScreen::SaveCursorAndClear);
                }
            }
            NamedPrivateMode::LegacyAltScreen => {
                if on != alt {
                    self.swap_alt_screen(AltScreen::Shared);
                }
            }
            NamedPrivateMode::ClearingAltScreen => {
                if on != alt {
                    self.swap_alt_screen(AltScreen::ClearOnExit);
                }
            }
            NamedPrivateMode::Origin => {
                self.mode.set(TermMode::ORIGIN, on);
                if on {
                    self.goto_inner(0, 0);
                }
            }
            NamedPrivateMode::ColumnMode => {
                self.set_scrolling_region_inner(1, None);
                for y in 0..self.lines {
                    self.reset_row(y);
                }
            }
            NamedPrivateMode::ReportMouseClicks
            | NamedPrivateMode::ReportCellMouseMotion
            | NamedPrivateMode::ReportAllMouseMotion => {
                let bit = match mode {
                    NamedPrivateMode::ReportMouseClicks => TermMode::MOUSE_REPORT_CLICK,
                    NamedPrivateMode::ReportCellMouseMotion => TermMode::MOUSE_DRAG,
                    _ => TermMode::MOUSE_MOTION,
                };
                if on {
                    self.mode.remove(TermMode::MOUSE_MODE);
                }
                self.mode.set(bit, on);
            }
            // Mouse encodings are mutually exclusive.
            NamedPrivateMode::SgrMouse
            | NamedPrivateMode::Utf8Mouse
            | NamedPrivateMode::SgrPixelMouse => {
                let bit = match mode {
                    NamedPrivateMode::SgrMouse => TermMode::SGR_MOUSE,
                    NamedPrivateMode::Utf8Mouse => TermMode::UTF8_MOUSE,
                    _ => TermMode::SGR_PIXEL_MOUSE,
                };
                if on {
                    self.mode.remove(
                        TermMode::SGR_MOUSE | TermMode::UTF8_MOUSE | TermMode::SGR_PIXEL_MOUSE,
                    );
                }
                self.mode.set(bit, on);
            }
            NamedPrivateMode::CursorKeys => self.mode.set(TermMode::APP_CURSOR, on),
            NamedPrivateMode::LineWrap => self.mode.set(TermMode::LINE_WRAP, on),
            NamedPrivateMode::ShowCursor => self.mode.set(TermMode::SHOW_CURSOR, on),
            NamedPrivateMode::ReportFocusInOut => self.mode.set(TermMode::FOCUS_IN_OUT, on),
            NamedPrivateMode::BracketedPaste => self.mode.set(TermMode::BRACKETED_PASTE, on),
            NamedPrivateMode::AlternateScroll => self.mode.set(TermMode::ALTERNATE_SCROLL, on),
            NamedPrivateMode::UrgencyHints => self.mode.set(TermMode::URGENCY_HINTS, on),
            NamedPrivateMode::GraphemeClustering => {
                self.mode.set(TermMode::GRAPHEME_CLUSTERING, on)
            }
            NamedPrivateMode::ColorSchemeUpdates => {
                self.mode.set(TermMode::COLOR_SCHEME_UPDATES, on)
            }
            NamedPrivateMode::InBandResize => self.mode.set(TermMode::IN_BAND_RESIZE, on),
            NamedPrivateMode::BlinkingCursor
            | NamedPrivateMode::SyncUpdate
            | NamedPrivateMode::LeftRightMargins => {}
        }
        self.sync_mode();
        self.private_mode_events(mode, on);
    }

    /// Swaps the shown screen with the other one: pages, cursor and saved
    /// cursor.
    fn swap_screens(&mut self) {
        let lines = self.lines;
        let cols = self.cols;
        let other = self.other.get_or_insert_with(|| {
            Box::new(Inactive {
                pages: Pages::new(lines, cols, 0, None, None),
                cursor: Saved::default(),
                saved: Saved::default(),
                swaps: 0,
            })
        });
        let mut other = std::mem::replace(
            other,
            Box::new(Inactive {
                pages: Pages::new(1, 1, 0, None, None),
                cursor: Saved::default(),
                saved: Saved::default(),
                swaps: 0,
            }),
        );
        other.swaps += 1;
        std::mem::swap(&mut self.pages, &mut other.pages);
        std::mem::swap(&mut self.saved, &mut other.saved);
        let shown = std::mem::replace(&mut other.cursor, self.cursor_state());
        self.set_cursor_state(shown);
        self.other = Some(other);
        self.mode.toggle(TermMode::ALT_SCREEN);
    }

    /// `Term::swap_alt_screen`.
    fn swap_alt_screen(&mut self, kind: AltScreen) {
        let entering = !self.mode.contains(TermMode::ALT_SCREEN);
        let shared = (kind != AltScreen::SaveCursorAndClear).then(|| self.cursor_state());
        if !entering && kind == AltScreen::ClearOnExit {
            for y in 0..self.lines {
                self.reset_row(y);
            }
        }
        self.swap_keyboard_stacks();
        let clear_entered = entering && kind == AltScreen::SaveCursorAndClear;
        if clear_entered {
            // The alternate screen starts with this cursor, and this is
            // the primary screen's saved cursor from now on.
            self.saved = self.cursor_state();
            let cursor = self.cursor_state();
            self.swap_screens();
            self.set_cursor_state(cursor);
            for y in 0..self.lines {
                self.reset_row(y);
            }
        } else {
            self.swap_screens();
        }
        if let Some(cursor) = shared {
            self.set_cursor_state(cursor);
        }
    }

    /// `Grid::reset` of the shown screen: no history, default cursors,
    /// every line blank.
    fn reset_grid(&mut self) {
        self.pages.clear_history();
        self.saved = Saved::default();
        self.set_cursor_state(Saved::default());
        for y in 0..self.lines {
            self.reset_row(y);
        }
    }

    /// `Term::reset_state` (RIS), for the grid and its modes.
    pub(super) fn reset_state_inner(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.swap_screens();
        }
        self.active_charset = 0;
        self.events.cursor_style = None;
        self.reset_grid();
        if self.other.is_some() {
            self.swap_screens();
            self.reset_grid();
            self.swap_screens();
        }
        self.region = (0, self.lines);
        self.tabs = (0..self.cols).map(|c| c % 8 == 0).collect();
        self.reset_events();
        self.mode = TermMode::default();
        self.sync_mode();
        self.update_slow_text();
        self.send(alacritty_terminal::event::Event::CursorBlinkingChange);
    }

    /// `Term::soft_reset` (DECSTR).
    pub(super) fn soft_reset_inner(&mut self) {
        self.mode
            .insert(TermMode::SHOW_CURSOR | TermMode::LINE_WRAP);
        self.mode.remove(
            TermMode::INSERT | TermMode::ORIGIN | TermMode::APP_CURSOR | TermMode::APP_KEYPAD,
        );
        self.region = (0, self.lines);
        self.active_charset = 0;
        self.pen = Style::default();
        self.pen_cache = (0, 0);
        self.link = None;
        self.charsets = Default::default();
        let mut saved = self.cursor_state();
        saved.line = 0;
        saved.col = 0;
        saved.wrap_pending = false;
        self.saved = saved;
        self.sync_mode();
        self.update_slow_text();
        if self.events.cursor_style.take().is_some() {
            self.send(alacritty_terminal::event::Event::CursorBlinkingChange);
        }
    }
}
