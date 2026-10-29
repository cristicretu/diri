//! Migration step 4c (`docs/core-v2.md`): core v2 behind the emulator
//! interface `HeadlessScreen` reads and writes (`crate::emulator`), selected
//! by the `core-v2-screen` feature.
//!
//! Readers get rows and cells in the current core's cell type, converted
//! from pages as they are read, so every snapshot, diff, scrollback and
//! checkpoint encoder runs unchanged. Writers (checkpoint restore, history
//! annotations) go through the same cell type and are converted back.
//!
//! - **Damage.** Rows carry `RowMeta::dirty`, set by every write. A frame is
//!   fully damaged when rows moved since the last clear (a row was appended,
//!   history was cleared, the screens were swapped, or the options changed);
//!   otherwise the dirty rows plus the cursor rows (then and now, as the
//!   current core adds them) are damaged. Moved rows are not tracked
//!   ([`Emulator::moved_line_sources`] is `None`).
//! - **History.** It lives in the primary screen's pages, bounded by rows
//!   and by the live-byte budget, packed page by page. A packed page that a
//!   checkpoint annotates is thawed for the edit and packed again before
//!   rows move or history is bounded.
//! - **Resize** needs reflow (step 5). Until then a resize that changes the
//!   geometry panics instead of guessing.

use std::ops::Deref;

use alacritty_terminal::event::Event;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell as GridCell, Flags, Hyperlink};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::keyboard::KeyboardSnapshot;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{CursorStyle, KeyboardModes};
use diri_proto::grid::GridCell as WireCell;

use super::Core;
use super::cell::{
    CP, Cell, HYPERLINK, LEADING, MARKS_SHIFT, SPACER, STYLE_FLAGS, STYLE_SHIFT, Style, WIDE, WRAP,
};
use super::check::flags_with;
use super::page::Link;
use crate::emulator::{Emulator, RowView, RowWrite};
use crate::{Collector, Geometry};

/// The OSC 133 marks, in the order of the cell's MARKS bits.
const MARK_FLAGS: [Flags; 5] = [
    Flags::PROMPT_START,
    Flags::INPUT_START,
    Flags::OUTPUT_START,
    Flags::COMMAND_SUCCEEDED,
    Flags::COMMAND_FAILED,
];

/// The id a link read from a page gets when the program gave it none. A
/// link's cells in one row read as one link, as they do in the current core.
/// A parsed OSC 8 id never holds a NUL.
const ANONYMOUS_LINK: &str = "\0core-v2";

/// What rows look like at the last [`Emulator::clear_damage`]: when any of
/// it changes, rows moved and the whole screen is damaged.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Layout {
    total: u64,
    swaps: u64,
    history: usize,
}

/// Core v2 as `HeadlessScreen`'s emulator.
pub(crate) struct Screen {
    core: Core,
    /// The packed page thawed by [`Emulator::edit_row`], if any.
    thawed: Option<usize>,
    layout: Layout,
    cursor_line: usize,
    /// Everything is damaged (options changed).
    full_damage: bool,
}

/// One row read from a [`Screen`].
pub(crate) struct ScreenRow {
    cells: Vec<GridCell>,
}

impl RowView for ScreenRow {
    type Cell = GridCell;

    #[inline]
    fn cells(&self) -> &[GridCell] {
        &self.cells
    }

    fn content_width(&self) -> usize {
        self.cells
            .iter()
            .rposition(|cell| !cell.is_default())
            .map_or(0, |i| i + 1)
    }
}

/// One cell read from a [`Screen`].
pub(crate) struct ScreenCell(GridCell);

impl Deref for ScreenCell {
    type Target = GridCell;

    #[inline]
    fn deref(&self) -> &GridCell {
        &self.0
    }
}

/// One row being written by [`Emulator::edit_row`]: the cells it read, and
/// which of them the edit may have changed.
pub(crate) struct RowEdit {
    cells: Vec<GridCell>,
    touched: Vec<bool>,
}

impl RowWrite for RowEdit {
    type Cell = GridCell;

    fn cell(&self, column: Column) -> &GridCell {
        &self.cells[column.0]
    }

    fn cell_mut(&mut self, column: Column) -> &mut GridCell {
        self.touched[column.0] = true;
        &mut self.cells[column.0]
    }
}

/// Reads row `line` (negative: history) of `core`'s shown screen into `out`
/// as current-core cells.
fn read_row(core: &Core, line: i32, out: &mut Vec<GridCell>) {
    let cols = core.cols();
    let pages = &core.pages;
    let (p, r) = pages.locate_line(line);
    let page = pages.page(p);
    let meta = pages.meta(p, r);
    let occ = meta.occ as usize;
    let mut linked = Vec::new();
    out.clear();
    pages.read(p, |_, cells, clusters| {
        for x in 0..cols {
            let cell = if x < occ {
                cells[r * cols + x]
            } else {
                Cell::blank(meta.fill)
            };
            if cell.has(HYPERLINK) {
                linked.push(x);
            }
            out.push(grid_cell(
                cell,
                &page.styles.get(cell.style()),
                clusters.of(cell),
                x + 1 == cols && meta.wrapped,
            ));
        }
    });
    let mut previous: Option<(Link, Hyperlink)> = None;
    for x in linked {
        let Some(link) = pages.read_link(p, r * cols + x) else {
            continue;
        };
        let hyperlink = match &previous {
            Some((last, hyperlink)) if *last == link => hyperlink.clone(),
            _ => {
                let hyperlink = Hyperlink::new(
                    Some(link.id.as_deref().unwrap_or(ANONYMOUS_LINK)),
                    link.uri.clone(),
                );
                previous = Some((link, hyperlink.clone()));
                hyperlink
            }
        };
        out[x].set_hyperlink(Some(hyperlink));
    }
}

/// The current core's cell for a v2 cell in `style`.
#[inline]
fn grid_cell(cell: Cell, style: &Style, zerowidth: &[char], row_wrap: bool) -> GridCell {
    let mut out = GridCell {
        c: cell.ch(),
        fg: style.fg,
        bg: style.bg,
        flags: flags_with(cell, style, row_wrap),
        extra: None,
    };
    if style.underline.is_some() {
        out.set_underline_color(style.underline);
    }
    for &c in zerowidth {
        out.push_zerowidth(c);
    }
    out
}

impl Screen {
    /// Packs the page [`Emulator::edit_row`] thawed, if any.
    fn refreeze(&mut self) {
        if let Some(p) = self.thawed.take() {
            self.core.pages.refreeze(p);
        }
    }

    fn layout(&self) -> Layout {
        Layout {
            total: self.core.pages.total_rows(),
            swaps: self.core.screen_swaps(),
            history: self.core.pages.history_size(),
        }
    }

    /// Page and row of `line`, made writable.
    fn writable(&mut self, line: i32) -> (usize, usize) {
        let (p, r) = self.core.pages.locate_line(line);
        if self.core.pages.page(p).is_packed() {
            self.refreeze();
            self.core.pages.thaw(p);
            self.thawed = Some(p);
        }
        (p, r)
    }

    /// Writes `cell` (a current-core cell) at column `x` of row `r` of page
    /// `p`, which is writable: its character, style, width and wrap bits,
    /// marks, combining characters and hyperlink.
    fn store(&mut self, p: usize, r: usize, x: usize, cell: &GridCell) {
        let cols = self.core.cols();
        let page = self.core.pages.page_mut(p);
        let index = r * cols + x;
        let meta = &mut page.meta[r];
        let occ = meta.occ as usize;
        if occ <= x {
            let blank = Cell::blank(meta.fill);
            page.cells[r * cols + occ..index].fill(blank);
            meta.occ = x as u16 + 1;
        }
        meta.dirty = true;
        meta.simple = false;
        meta.lead = meta.lead.min(x as u16);
        let flags = cell.flags;
        if x + 1 == cols {
            meta.wrapped = flags.contains(Flags::WRAPLINE);
        }
        page.text.invalidate(r);
        let style = Style {
            fg: cell.fg,
            bg: cell.bg,
            underline: cell.underline_color(),
            flags: flags.bits() & STYLE_FLAGS,
        };
        let id = page.styles.intern(&style);
        let mut bits = (cell.c as u64 & CP) | (id as u64) << STYLE_SHIFT;
        for (flag, bit) in [
            (Flags::WIDE_CHAR, WIDE),
            (Flags::WIDE_CHAR_SPACER, SPACER),
            (Flags::LEADING_WIDE_CHAR_SPACER, LEADING),
        ] {
            if flags.contains(flag) {
                bits |= bit;
            }
        }
        if x + 1 != cols && flags.contains(Flags::WRAPLINE) {
            bits |= WRAP;
        }
        for (i, flag) in MARK_FLAGS.into_iter().enumerate() {
            if flags.contains(flag) {
                bits |= 1 << (MARKS_SHIFT + i as u32);
            }
        }
        page.cells[index] = Cell(bits);
        for &c in cell.zerowidth().unwrap_or(&[]) {
            page.push_zerowidth(index, c);
        }
        if let Some(hyperlink) = cell.hyperlink() {
            let link = Link {
                id: (hyperlink.id() != ANONYMOUS_LINK).then(|| hyperlink.id().to_owned()),
                uri: hyperlink.uri().to_owned(),
            };
            let id = page.link_id(&link);
            page.set_link(index, id);
            page.cells[index].0 |= HYPERLINK;
        }
    }

    /// Replaces screen line 0 with `cells`, then appends a row so that line
    /// becomes the newest history row.
    fn push_history_row(&mut self, cells: impl Iterator<Item = GridCell>) {
        let (p, r) = self.core.pages.locate(0);
        for (x, cell) in cells.enumerate() {
            self.store(p, r, x, &cell);
        }
        let fill = self.core.pen.erased();
        self.core.pages.push_row(&fill);
    }

    /// Copies `source`'s cells, history and cursor (the verification bridge,
    /// `HeadlessScreen::from_core_v2`). History is kept whole: no budget
    /// applies until the screen is next settled.
    pub(crate) fn copy_from(&mut self, source: &Core) {
        let history = source.history_size();
        self.core.pages.set_budget(None);
        let limit = self.core.pages.history_limit.max(history);
        self.core.pages.set_history_limit(limit);
        let mut cells = Vec::new();
        for line in -(history as i32)..0 {
            read_row(source, line, &mut cells);
            self.push_history_row(cells.drain(..));
        }
        for line in 0..source.lines() {
            read_row(source, line as i32, &mut cells);
            let (p, r) = self.core.pages.locate(line);
            for (x, cell) in cells.iter().enumerate() {
                self.store(p, r, x, cell);
            }
        }
        assert_eq!(
            self.core.history_size(),
            history,
            "the screen keeps the core's history"
        );
        self.core.line = source.line;
        self.core.col = source.col;
        self.core.wrap_pending = source.wrap_pending;
        source.release_read_cache();
    }
}

impl Emulator for Screen {
    type Handler = Core;
    type Cell = GridCell;
    type RowRef<'a> = ScreenRow;
    type CellRef<'a> = ScreenCell;
    type RowMut = RowEdit;
    type Pen = (Style, Option<Link>);

    fn create(config: Config, geometry: &Geometry, events: Collector) -> Self {
        let mut core = Core::with_budget(
            geometry.rows,
            geometry.cols,
            config.scrolling_history,
            Some(crate::HISTORY_STORAGE_BUDGET_BYTES),
        );
        core.set_events(config, Box::new(events));
        let mut screen = Screen {
            core,
            thawed: None,
            layout: Layout::default(),
            cursor_line: 0,
            full_damage: true,
        };
        screen.layout = screen.layout();
        screen
    }

    #[inline]
    fn handler(&mut self) -> &mut Core {
        self.refreeze();
        &mut self.core
    }

    #[inline]
    fn cols(&self) -> usize {
        self.core.cols()
    }

    #[inline]
    fn screen_rows(&self) -> usize {
        self.core.lines()
    }

    fn resize_screen(&mut self, geometry: Geometry) {
        if geometry.cols == self.core.cols() && geometry.rows == self.core.lines() {
            return;
        }
        panic!("core-v2: resize needs reflow, step 5");
    }

    #[inline]
    fn options(&self) -> &Config {
        &self.core.events.config
    }

    /// `Term::set_options`.
    fn replace_options(&mut self, options: Config) {
        self.refreeze();
        let old = std::mem::replace(&mut self.core.events.config, options);
        let title = match &self.core.events.title {
            Some(title) => Event::Title(title.clone()),
            None => Event::ResetTitle,
        };
        self.core.send(title);
        let limit = self.core.events.config.scrolling_history;
        self.core.primary_pages_mut().set_history_limit(limit);
        if self.core.events.config.kitty_keyboard != old.kitty_keyboard {
            self.core.events.keyboard_stack = Vec::new();
            self.core.events.inactive_keyboard_stack = Vec::new();
            self.core.mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
        }
        self.full_damage = true;
    }

    #[inline]
    fn cursor_point(&self) -> Point {
        Point::new(Line(self.core.line as i32), Column(self.core.col))
    }

    #[inline]
    fn cursor_pending_wrap(&self) -> bool {
        self.core.wrap_pending
    }

    fn place_cursor(&mut self, point: Point, pending_wrap: bool) {
        self.core.line = point.line.0.clamp(0, self.core.lines() as i32 - 1) as usize;
        self.core.col = point.column.0.min(self.core.cols() - 1);
        self.core.wrap_pending = pending_wrap;
    }

    fn pen(&self) -> (Style, Option<Link>) {
        (self.core.pen, self.core.link.clone())
    }

    fn restore_pen(&mut self, (pen, link): (Style, Option<Link>)) {
        self.core.pen = pen;
        self.core.pen_cache = (0, 0);
        self.core.link = link;
        self.core.update_slow_text();
    }

    fn set_link_pen(&mut self, uri: Option<String>) {
        self.restore_pen((Style::default(), uri.map(|uri| Link { id: None, uri })));
    }

    fn row(&self, line: Line) -> ScreenRow {
        let mut cells = Vec::with_capacity(self.core.cols());
        read_row(&self.core, line.0, &mut cells);
        ScreenRow { cells }
    }

    fn cell(&self, line: Line, column: Column) -> ScreenCell {
        let core = &self.core;
        let (line, col) = (line.0, column.0);
        let cell = core.cell(line, col);
        let row_wrap = col + 1 == core.cols() && core.wrapped(line);
        let mut out = grid_cell(
            cell,
            &core.style_of(line, cell),
            &core.zerowidth(line, col),
            row_wrap,
        );
        if let Some(link) = core.link_at(line, col) {
            let id = link.id.as_deref().unwrap_or(ANONYMOUS_LINK);
            out.set_hyperlink(Some(Hyperlink::new(Some(id), link.uri)));
        }
        ScreenCell(out)
    }

    fn edit_row<R>(&mut self, line: Line, edit: impl FnOnce(&mut RowEdit) -> R) -> R {
        let (p, r) = self.writable(line.0);
        let mut row = RowEdit {
            cells: Vec::with_capacity(self.core.cols()),
            touched: vec![false; self.core.cols()],
        };
        read_row(&self.core, line.0, &mut row.cells);
        let result = edit(&mut row);
        for (x, cell) in row.cells.iter().enumerate() {
            if row.touched[x] {
                self.store(p, r, x, cell);
            }
        }
        result
    }

    #[inline]
    fn modes(&self) -> TermMode {
        self.core.mode
    }

    #[inline]
    fn program_colors(&self) -> &Colors {
        self.core.colors()
    }

    #[inline]
    fn program_colors_generation(&self) -> u64 {
        self.core.colors_generation()
    }

    #[inline]
    fn program_cursor_style(&self) -> CursorStyle {
        self.core.cursor_style()
    }

    fn collect_damage(&mut self, mut on_line: impl FnMut(usize)) -> bool {
        if self.full_damage || self.layout() != self.layout {
            return true;
        }
        let pages = &self.core.pages;
        for y in 0..self.core.lines() {
            let (p, r) = pages.locate(y);
            if pages.page(p).meta[r].dirty {
                on_line(y);
            }
        }
        // As the current core does, the rows the cursor was on and is on:
        // the print path changes them.
        on_line(self.cursor_line);
        on_line(self.core.line);
        false
    }

    #[inline]
    fn moved_line_sources(&self) -> Option<&[u32]> {
        None
    }

    fn clear_damage(&mut self) {
        for y in 0..self.core.lines() {
            let (p, r) = self.core.pages.locate(y);
            self.core.pages.page_mut(p).meta[r].dirty = false;
        }
        self.layout = self.layout();
        self.cursor_line = self.core.line;
        self.full_damage = false;
    }

    #[inline]
    fn history_rows(&self) -> usize {
        self.core.pages.history_size()
    }

    fn history_origin(&self) -> u64 {
        // Rows that entered the shown screen's history and left it from the
        // oldest end. With no history allowed nothing ever enters it, as in
        // the current core (the alternate screen).
        let pages = &self.core.pages;
        if pages.history_limit == 0 {
            return 0;
        }
        let entered = pages.total_rows() - self.core.lines() as u64;
        entered - pages.history_size() as u64
    }

    #[cfg(feature = "compact-history")]
    fn primary_history_rows(&self) -> usize {
        self.core.primary_pages().history_size()
    }

    /// `Term::set_scrolling_history`.
    #[cfg(feature = "compact-history")]
    fn set_history_limit(&mut self, limit: usize) {
        self.refreeze();
        self.core.events.config.scrolling_history = limit;
        self.core.primary_pages_mut().set_history_limit(limit);
    }

    fn restore_history(&mut self, history: &[Vec<WireCell>]) -> bool {
        self.refreeze();
        for row in history {
            self.push_history_row(row.iter().map(|cell| crate::emulator_cell(*cell)));
        }
        self.core.pages.history_size() == history.len()
    }

    #[cfg(feature = "compact-history")]
    fn decoded_row_bytes(cols: usize) -> usize {
        cols.saturating_mul(std::mem::size_of::<GridCell>())
            .saturating_add(std::mem::size_of::<Vec<GridCell>>())
    }

    /// Live bytes of the shown screen's history (none on the alternate
    /// screen, which keeps no history).
    #[cfg(feature = "compact-history")]
    fn history_storage_bytes(&self) -> usize {
        let pages = &self.core.pages;
        if pages.history_size() == 0 {
            0
        } else {
            pages.history_bytes()
        }
    }

    #[cfg(feature = "compact-history")]
    fn bound_history_storage(&mut self, budget: usize) {
        self.refreeze();
        self.core.primary_pages_mut().set_budget(Some(budget));
    }

    #[cfg(feature = "compact-history")]
    fn keep_history_spare_rows(&mut self, _keep: bool) {}

    #[cfg(feature = "compact-history")]
    fn release_history_spare_rows(&mut self) {}

    #[cfg(feature = "compact-history")]
    fn release_idle_history_memory(&mut self) {
        self.refreeze();
        self.core.release_read_cache();
    }

    #[cfg(feature = "compact-history")]
    fn prepare_history_read(&mut self) {}

    #[cfg(feature = "compact-history")]
    fn release_history_read_cache(&mut self) {
        self.core.release_read_cache();
    }

    /// v2 keeps its packed history in memory; paging it out to the cold
    /// store is not ported yet, so nothing is written and every byte stays.
    #[cfg(feature = "compact-history")]
    fn page_out_history(
        &mut self,
        _store: &std::sync::Arc<dyn crate::ColdStore>,
        _keep_rows: usize,
        _min_bytes: usize,
    ) -> std::io::Result<crate::PageOut> {
        Ok(crate::PageOut {
            payloads: 0,
            bytes: 0,
            already_stored: 0,
            kept_bytes: self.core.history_bytes(),
        })
    }

    #[cfg(feature = "compact-history")]
    fn history_residency(&self) -> crate::PayloadResidency {
        crate::PayloadResidency {
            resident: self.core.primary_pages().page_count(),
            resident_bytes: self.core.history_bytes(),
            stored: 0,
            stored_bytes: 0,
        }
    }

    fn kitty_keyboard_enabled(&self) -> bool {
        self.core.events.config.kitty_keyboard
    }

    /// `Term::keyboard_snapshot`, built through the snapshot's own encoding.
    fn keyboard_stacks(&self) -> KeyboardSnapshot {
        let events = &self.core.events;
        let (active, inactive) = (&events.keyboard_stack, &events.inactive_keyboard_stack);
        let mut bytes = Vec::with_capacity(6 + active.len() + inactive.len());
        bytes.extend([1, KeyboardModes::from(self.core.mode).bits()]);
        bytes.extend((active.len() as u16).to_le_bytes());
        bytes.extend((inactive.len() as u16).to_le_bytes());
        bytes.extend(active.iter().map(|flags| flags.bits()));
        bytes.extend(inactive.iter().map(|flags| flags.bits()));
        KeyboardSnapshot::decode(&bytes)
            .expect("the keyboard stacks are bounded and the active flags are their top")
    }

    fn can_restore_keyboard_stacks(&self, snapshot: &KeyboardSnapshot) -> bool {
        self.core.events.config.kitty_keyboard || !snapshot.has_enhancements()
    }

    /// `Term::restore_keyboard_snapshot`.
    fn restore_keyboard_stacks(&mut self, snapshot: &KeyboardSnapshot) -> bool {
        if !self.can_restore_keyboard_stacks(snapshot) {
            return false;
        }
        let bytes = snapshot.encode();
        let active = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
        let stack = |bytes: &[u8]| -> Vec<KeyboardModes> {
            bytes
                .iter()
                .map(|b| KeyboardModes::from_bits_retain(*b))
                .collect()
        };
        self.core.events.keyboard_stack = stack(&bytes[6..6 + active]);
        self.core.events.inactive_keyboard_stack = stack(&bytes[6 + active..]);
        self.core.mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
        self.core
            .mode
            .insert(KeyboardModes::from_bits_retain(snapshot.current()).into());
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::HeadlessScreen;

    /// Diffs built from v2 damage, applied in order, always equal a fresh
    /// full snapshot: no changed row is ever missed, across scrolls, region
    /// scrolls, screen swaps, erases, inserts, links and wraps, with
    /// several feeds (split byte by byte) per publication.
    #[test]
    fn diffs_from_damage_match_fresh_snapshots() {
        let mut screen = HeadlessScreen::new(24, 8);
        let mut mirror = Vec::new();
        screen.grid_update(true).apply(&mut mirror);
        let operations: &[&[u8]] = &[
            b"hello\r\nworld",
            b"\x1b[1;31mcolored\x1b[0m",
            b"\x1b[H",
            b"\x1b[C\x1b[D",
            b"\x1b[2J",
            "wide: \u{754c}\u{1f642}".as_bytes(),
            b"\x1b[?1049h",
            b"\x1b[3;5Halternate",
            b"\x1b[?1049l",
            b"\x1b[2;6r\x1b[6;1H\n\n\x1b[r",
            b"\x1b[2;1H\x1b[L",
            b"\x1b[M",
            b"\x1b[?2026hheld\x1b[?2026l",
            b"\x1b[?25l",
            b"0123456789012345678901234",
            b"\x1b[8;24Hx",
            b"y\r\n",
            b"\x1b[4h\x1b[1;3Hins\x1b[4l",
            b"\x1b[2;4H\x1b[3P\x1b[2X",
            b"\x1b]8;;https://example.invalid\x07link\x1b]8;;\x07",
            b"\x1bM\x1bM\x1bM",
            b"\x1b[?47h\x1b[?47l",
            b"\x1b[3J",
            b"\x1b#8",
            b"\ttab\x1b[2K",
            b"e\xcc\x81 \x1b[4:3m\x1b[58;5;1mcurl\x1b[0m",
            b"\x1b[5;20H\x1b[1J",
            b"\x1b[3S\x1b[2T",
        ];
        for round in 0..200 {
            for bytes in operations.iter().cycle().skip(round * 3 % 29).take(4) {
                if round % 2 == 0 {
                    for byte in bytes.iter() {
                        screen.feed(std::slice::from_ref(byte));
                    }
                } else {
                    screen.feed(bytes);
                }
            }
            let update = screen.grid_update(round % 17 == 0);
            update.apply(&mut mirror);
            let mut expected = Vec::new();
            screen.full_snapshot().apply(&mut expected);
            assert_eq!(mirror, expected, "round {round}");
        }
    }

    /// A link without an id reads as one link across its cells, and two
    /// links are two.
    #[test]
    fn anonymous_links_keep_their_spans() {
        let mut screen = HeadlessScreen::new(40, 4);
        screen.feed(b"\x1b]8;;https://a.invalid\x07abc\x1b]8;;\x07 \x1b]8;;https://b.invalid\x07de\x1b]8;;\x07");
        let links = &screen.full_snapshot().changed_rows[0].metadata.links;
        assert_eq!(links.len(), 2);
        assert_eq!((links[0].start, links[0].end), (0, 3));
        assert_eq!((links[1].start, links[1].end), (4, 6));
    }
}
