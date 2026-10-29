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
//!   current core adds them) are damaged. Rows moved by appends report
//!   where they were ([`Emulator::moved_line_sources`]), so their
//!   fingerprints are reused; a dirty or new row reports `CONTENT_CHANGED`.
//! - **History.** It lives in the primary screen's pages, bounded by rows
//!   and by the live-byte budget, packed page by page. A packed page that a
//!   checkpoint annotates is thawed for the edit and packed again before
//!   rows move or history is bounded.
//! - **Resize** reflows both screens as the current core does (step 5,
//!   `super::resize`), and damages everything.

use std::ops::Deref;

use alacritty_terminal::event::Event;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell as GridCell, Hyperlink};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::keyboard::KeyboardSnapshot;
use alacritty_terminal::term::{CONTENT_CHANGED, Config, TermMode};
use alacritty_terminal::vte::ansi::{CursorStyle, KeyboardModes};
use diri_proto::grid::GridCell as WireCell;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::Core;
use super::cell::{Cell, GRAPHEME, HYPERLINK, LEADING, MARKS, SPACER, STYLE, Style, WIDE, WRAP};
use super::check::flags_with;
use super::page::{Clusters, Link, style_in};
use super::resize::{self, ANONYMOUS_LINK, grid_cell};
use crate::emulator::{CellView, Emulator, LinkView, RowView, RowWrite};
use crate::{Collector, Geometry};
use crate::{RowHasher, SPAN_FLAGS, style_word, wire_cell};

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
    /// The program colors' generation at the last clear.
    colors_seen: u64,
    /// Everything is damaged (options changed).
    full_damage: bool,
    /// Per screen line, where its cells were at the last
    /// [`Emulator::clear_damage`] (see [`Emulator::moved_line_sources`]);
    /// valid while `moved_known`.
    moved: Vec<u32>,
    moved_known: bool,
    /// The dirty screen lines [`Emulator::collect_damage`] found, valid
    /// while `dirty_listed`.
    dirty: Vec<u32>,
    dirty_listed: bool,
    /// `0..lines`: the moved-line sources when nothing moved.
    identity: Vec<u32>,
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

impl LinkView for &Link {
    #[inline]
    fn target(&self) -> &str {
        &self.uri
    }
}

/// A page cell as the current core's cell reads, without building one: the
/// view [`Emulator::fingerprint_row`] and [`Emulator::wire_row`] read pages
/// through, so the shared fold and wire mapping run on it. It
/// reads exactly as `resize::read_row`'s cell for the same column, links
/// compared as the ids `read_row` gives them compare.
#[derive(Clone, Copy)]
struct PageCell<'a> {
    cell: Cell,
    style: Style,
    zerowidth: &'a [char],
    row_wrap: bool,
    link: Option<&'a Link>,
}

/// Cell bits that [`flags_with`] turns into flags beyond the style's.
const FLAG_BITS: u64 = WIDE | SPACER | LEADING | WRAP | MARKS;

impl<'a> PageCell<'a> {
    /// Column `x` of a row of `cols` cells whose first `occ` are `cells`
    /// (the rest blanks of style `fill`), its link not yet resolved.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn at(
        cells: &[Cell],
        occ: usize,
        fill: u16,
        wrapped: bool,
        cols: usize,
        x: usize,
        styles: &[Style],
        clusters: &'a Clusters,
    ) -> Self {
        let cell = if x < occ { cells[x] } else { Cell::blank(fill) };
        PageCell {
            cell,
            style: style_in(styles, cell.style()),
            zerowidth: clusters.of(cell),
            row_wrap: wrapped && x + 1 == cols,
            link: None,
        }
    }

    /// `GridCell::is_default` of the cell this view reads as.
    #[inline(always)]
    fn is_default(&self) -> bool {
        self.ch() == ' ' && self.cell_flags().is_empty() && !self.has_extra() && {
            let style = &self.style;
            matches!(style.fg, Color::Named(NamedColor::Foreground))
                && matches!(style.bg, Color::Named(NamedColor::Background))
        }
    }
}

impl<'a> CellView for PageCell<'a> {
    type Link = &'a Link;

    #[inline(always)]
    fn ch(&self) -> char {
        self.cell.ch()
    }

    #[inline(always)]
    fn fg_color(&self) -> Color {
        self.style.fg
    }

    #[inline(always)]
    fn bg_color(&self) -> Color {
        self.style.bg
    }

    #[inline(always)]
    fn cell_flags(&self) -> Flags {
        if self.cell.0 & FLAG_BITS == 0 && !self.row_wrap {
            // The style carries none of the bits flags_with sets.
            Flags::from_bits_retain(self.style.flags)
        } else {
            flags_with(self.cell, &self.style, self.row_wrap)
        }
    }

    #[inline(always)]
    fn has_extra(&self) -> bool {
        self.style.underline.is_some() || !self.zerowidth.is_empty() || self.link.is_some()
    }

    #[inline]
    fn combining(&self) -> Option<&[char]> {
        self.has_extra().then_some(self.zerowidth)
    }

    #[inline]
    fn underline(&self) -> Option<Color> {
        self.style.underline
    }

    #[inline]
    fn link(&self) -> Option<&'a Link> {
        self.link
    }
}

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
    resize::read_row(&core.pages, line, out);
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
    /// `p`, which is writable.
    fn store(&mut self, p: usize, r: usize, x: usize, cell: &GridCell) {
        resize::store(&mut self.core.pages, p, r, x, cell);
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
            colors_seen: 0,
            full_damage: true,
            moved: Vec::new(),
            moved_known: false,
            dirty: Vec::new(),
            dirty_listed: false,
            identity: Vec::new(),
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

    /// `Term::resize`, with reflow (`core_v2::resize`).
    fn resize_screen(&mut self, geometry: Geometry) {
        if geometry.cols == self.core.cols() && geometry.rows == self.core.lines() {
            return;
        }
        self.refreeze();
        self.core.resize(geometry.rows, geometry.cols);
        self.full_damage = true;
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

    /// Reads link targets where the cells' link bit is set, without
    /// converting the row to current-core cells.
    fn each_link_target(&self, line: Line, f: &mut dyn FnMut(usize, &str)) {
        let pages = &self.core.pages;
        let cols = self.core.cols();
        let (p, r) = pages.locate_line(line.0);
        let occ = (pages.meta(p, r).occ as usize).min(cols);
        let mut linked = [0u16; 64];
        let mut columns = Vec::new();
        let mut n = 0;
        pages.read(p, |_, cells, _| {
            for (x, cell) in cells[r * cols..r * cols + occ].iter().enumerate() {
                if cell.has(HYPERLINK) {
                    if n < linked.len() {
                        linked[n] = x as u16;
                    } else {
                        columns.push(x as u16);
                    }
                    n += 1;
                }
            }
        });
        for &x in linked[..n.min(linked.len())].iter().chain(&columns) {
            if let Some(link) = pages.read_link(p, r * cols + x as usize) {
                f(x as usize, &link.uri);
            }
        }
    }

    fn row(&self, line: Line) -> ScreenRow {
        let mut cells = Vec::with_capacity(self.core.cols());
        read_row(&self.core, line.0, &mut cells);
        ScreenRow { cells }
    }

    /// `fingerprint_cells` of `row(line)`, read from the page through
    /// [`PageCell`] views: no row of current-core cells is built.
    fn fingerprint_row(&self, line: Line, cols: usize) -> (u64, usize) {
        let pages = &self.core.pages;
        let (p, r) = pages.locate_line(line.0);
        let page = pages.page(p);
        if page.is_packed() {
            // Screen rows are never packed; history rows read as before.
            return crate::fingerprint_cells(&self.row(line), cols);
        }
        let row_cols = pages.cols;
        let meta = page.meta[r];
        let base = r * row_cols;
        let occ = (meta.occ as usize).min(row_cols);
        let cells = &page.cells[base..base + occ];
        let styles = page.styles.as_slice();
        let clusters = &page.clusters;
        let view = |x: usize| {
            let mut cell = PageCell::at(
                cells,
                occ,
                meta.fill,
                meta.wrapped,
                row_cols,
                x,
                styles,
                clusters,
            );
            if cell.cell.has(HYPERLINK) {
                cell.link = page.link_at(base + x);
            }
            cell
        };
        let width = cols.min(row_cols);
        // `content_width` of the whole row: past `occ` every cell is the
        // fill blank (the last one marked when the row wrapped).
        let content =
            if occ < row_cols && (!view(row_cols - 1).is_default() || !view(occ).is_default()) {
                row_cols
            } else {
                (0..occ)
                    .rposition(|x| !view(x).is_default())
                    .map_or(0, |x| x + 1)
            }
            .min(width);
        // Consecutive cells mostly share a style and flag bits: the style
        // word is derived once per run of them, as `RowHashState` caches
        // it per raw style. Cells with a link, combining marks or an
        // underline color take the shared fold through their view.
        let mut hasher = RowHasher::new();
        let mut key = u64::MAX;
        let mut style_hash = 0;
        let mut underlined = false;
        let fill = std::iter::repeat(Cell::blank(meta.fill));
        for (x, cell) in cells.iter().copied().chain(fill).take(content).enumerate() {
            let row_wrap = meta.wrapped && x + 1 == row_cols;
            if cell.has(GRAPHEME | HYPERLINK) {
                hasher.cell(&view(x));
                continue;
            }
            let k = cell.0 & (FLAG_BITS | STYLE) | if row_wrap { WRAP } else { 0 };
            if k != key {
                key = k;
                let style = style_in(styles, cell.style());
                underlined = style.underline.is_some();
                style_hash = style_word(flags_with(cell, &style, row_wrap), style.fg, style.bg);
            }
            if underlined {
                hasher.cell(&view(x));
            } else {
                hasher.plain(cell.ch(), style_hash);
            }
        }
        hasher.finish(width, content)
    }

    /// The wire cells `row(line)` maps to, read from the page through
    /// [`PageCell`] views. Links are not resolved (the wire cell has none):
    /// a HYPERLINK bit reports the row annotated.
    fn wire_row(
        &self,
        line: Line,
        out: &mut [WireCell],
        mut each: impl FnMut(&mut WireCell, WireCell),
    ) -> bool {
        let pages = &self.core.pages;
        let (p, r) = pages.locate_line(line.0);
        let row_cols = pages.cols;
        pages.read_styled(p, |metas, cells, clusters, styles| {
            let meta = metas[r];
            let base = r * row_cols;
            let occ = (meta.occ as usize).min(row_cols);
            let cells = &cells[base..base + occ];
            // A wire cell is its character over what the style and flag
            // bits give, so those are mapped once per run of equal bits.
            let mut annotated = false;
            let mut key = u64::MAX;
            let mut template = WireCell::BLANK;
            let mut spacer = false;
            for (x, slot) in out.iter_mut().take(row_cols).enumerate() {
                let cell = if x < occ {
                    cells[x]
                } else {
                    Cell::blank(meta.fill)
                };
                let row_wrap = meta.wrapped && x + 1 == row_cols;
                let k = cell.0 & (FLAG_BITS | STYLE) | if row_wrap { WRAP } else { 0 };
                if k != key {
                    key = k;
                    let view = PageCell::at(
                        cells,
                        occ,
                        meta.fill,
                        meta.wrapped,
                        row_cols,
                        x,
                        styles,
                        clusters,
                    );
                    template = wire_cell(&view);
                    spacer = cell.has(SPACER | LEADING);
                    annotated |=
                        view.style.underline.is_some() | view.cell_flags().intersects(SPAN_FLAGS);
                }
                // A link or a cluster may annotate the row (over-reported
                // for an empty cluster, whose annotations read as none).
                annotated |= cell.has(HYPERLINK | GRAPHEME);
                let mut wire = template;
                if !spacer {
                    wire.scalar = u32::from(cell.ch());
                }
                each(slot, wire);
            }
            annotated
        })
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

    /// Every write marks its row dirty (the print path included), so the
    /// dirty rows are the damage. The current core also reports the rows the
    /// cursor was on and is on, which re-hashes them on every read; here
    /// that happens only after the program colors changed, the one change
    /// to those rows' shown colors that writes no cell (and which the
    /// current core shows through them). A read that changes nothing
    /// reports nothing, and settling it hashes no row.
    ///
    /// One walk over the screen finds the dirty rows (which
    /// [`Emulator::clear_damage`] then clears alone) and where each line's
    /// cells were at the last clear, so moved rows keep their fingerprints
    /// (as the current core reports them). Rows only move by being appended
    /// at the bottom (the screen is the newest rows of the page list), so
    /// line `y` holds what line `y + appended` held, unless a write since
    /// marked it dirty or it is new. That is unknown across a screen swap
    /// or an options change.
    fn collect_damage(&mut self, mut on_line: impl FnMut(usize)) -> bool {
        let now = self.layout();
        self.moved_known =
            !self.full_damage && now.swaps == self.layout.swaps && now.total >= self.layout.total;
        let full = self.full_damage || now != self.layout;
        let appended = now.total.wrapping_sub(self.layout.total);
        let lines = self.core.lines();
        let pages = &self.core.pages;
        self.moved.clear();
        self.dirty.clear();
        self.dirty_listed = true;
        if !full {
            // Nothing moved: each line is its own source, and settling
            // reads sources only of lines it was not given (none of them
            // dirty), so the identity serves without a per-line write.
            if self.identity.len() != lines {
                self.identity = (0..lines as u32).collect();
            }
            let rows = pages.rows_per_page();
            let (mut p, mut r) = pages.locate(0);
            let mut y = 0;
            while y < lines {
                let take = (rows - r).min(lines - y);
                for (k, meta) in pages.page(p).meta[r..r + take].iter().enumerate() {
                    if meta.dirty {
                        self.dirty.push((y + k) as u32);
                    }
                }
                y += take;
                p += 1;
                r = 0;
            }
            for &y in &self.dirty {
                on_line(y as usize);
            }
            if self.core.colors_generation() != self.colors_seen {
                on_line(self.cursor_line);
                on_line(self.core.line);
            }
            return false;
        }
        for y in 0..lines {
            let (p, r) = pages.locate(y);
            let dirty = pages.page(p).meta[r].dirty;
            if dirty {
                self.dirty.push(y as u32);
            }
            if self.moved_known {
                let source = y as u64 + appended;
                self.moved.push(if dirty || source >= lines as u64 {
                    CONTENT_CHANGED
                } else {
                    source as u32
                });
            }
        }
        true
    }

    #[inline]
    fn moved_line_sources(&self) -> Option<&[u32]> {
        if !self.moved_known {
            return None;
        }
        Some(if self.moved.is_empty() {
            &self.identity
        } else {
            &self.moved
        })
    }

    fn clear_damage(&mut self) {
        let lines = self.core.lines();
        let pages = &mut self.core.pages;
        if std::mem::take(&mut self.dirty_listed) {
            for &y in &self.dirty {
                let (p, r) = pages.locate(y as usize);
                pages.page_mut(p).meta[r].dirty = false;
            }
        } else {
            for y in 0..lines {
                let (p, r) = pages.locate(y);
                pages.page_mut(p).meta[r].dirty = false;
            }
        }
        self.layout = self.layout();
        self.cursor_line = self.core.line;
        self.colors_seen = self.core.colors_generation();
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
        pages.history_origin()
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

    /// Packed pages older than the newest `keep_rows` history rows move to
    /// the cold store (#958), as the current core's blocks do.
    #[cfg(feature = "compact-history")]
    fn page_out_history(
        &mut self,
        store: &std::sync::Arc<dyn crate::ColdStore>,
        keep_rows: usize,
        min_bytes: usize,
    ) -> std::io::Result<crate::PageOut> {
        self.core.release_read_cache();
        self.core
            .primary_pages_mut()
            .page_out(store, keep_rows, min_bytes)
    }

    #[cfg(feature = "compact-history")]
    fn history_residency(&self) -> crate::PayloadResidency {
        let (resident, resident_bytes, stored, stored_bytes) =
            self.core.primary_pages().residency();
        crate::PayloadResidency {
            resident,
            resident_bytes,
            stored,
            stored_bytes,
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

    /// The page views behind `fingerprint_row` and `wire_row` read exactly
    /// as the converted rows the default implementations read: the same
    /// fingerprint and fill count at every width, the same wire cells, and
    /// an annotation report whenever the converted row has one. Covers
    /// links (with and without ids), wide and combining characters,
    /// underline colors, decorations, shell marks, soft wraps, colored
    /// erase fills, and packed history rows.
    #[test]
    fn page_views_read_as_converted_rows() {
        use alacritty_terminal::index::Line;
        use diri_proto::grid::GridCell as WireCell;

        use crate::emulator::{CellView, Emulator, RowView};

        let inputs: &[&[u8]] = &[
            b"plain text that wraps past the edge of the row\r\n",
            b"\x1b]8;;https://a.invalid\x07ab\x1b]8;;\x07 \x1b]8;id=x;https://a.invalid\x07cd\x1b]8;;\x07\r\n",
            "wide \u{754c}\u{1f642} e\u{301} x\u{200d}y\r\n".as_bytes(),
            b"\x1b[4:3m\x1b[58;5;1mcurl\x1b[0m \x1b[21mdouble\x1b[0m \x1b[53mover\x1b[0m\r\n",
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\x1b]133;D;0\x07\r\n",
            b"\x1b[44m\x1b[K\x1b[0mblue fill\r\n",
            b"\x1b[41m  \x1b[0m\x1b[1;7mbold inverse\x1b[0m\r\n",
            b"0123456789abcdefghijklmnop\x1b[2D\x1b[3P\r\n",
            b"\x1b[38;2;1;2;3m\x1b[48;5;200mtruecolor\x1b[0m\t\ttabs\r\n",
            b"\x1b[2;3H\x1b[1Xerased\x1b[999;1H\r\n",
        ];
        let mut screen = HeadlessScreen::new(20, 6);
        let mut generic_cells = Vec::new();
        let mut native = Vec::new();
        for (step, bytes) in inputs.iter().cycle().take(4_000).enumerate() {
            screen.feed(bytes);
            if step % 37 != 0 && step < 3_900 {
                continue;
            }
            let term = &screen.term;
            let cols = term.cols();
            let history = term.history_rows() as i32;
            let lines = (-history..term.screen_rows() as i32).filter(|y| *y >= -40 || y % 97 == 0);
            for y in lines {
                let line = Line(y);
                let row = term.row(line);
                for width in [cols, cols - 3, 1] {
                    assert_eq!(
                        term.fingerprint_row(line, width),
                        crate::fingerprint_cells(&row, width),
                        "fingerprint of line {y} at width {width}, step {step}"
                    );
                }
                generic_cells.clear();
                generic_cells.extend(row.cells().iter().map(crate::wire_cell));
                native.clear();
                native.resize(cols, WireCell::BLANK);
                let annotated = term.wire_row(line, &mut native, |slot, cell| *slot = cell);
                assert_eq!(native, generic_cells, "wire cells of line {y}, step {step}");
                let generic_annotated = row.cells().iter().any(|cell| {
                    cell.has_extra() || cell.cell_flags().intersects(crate::SPAN_FLAGS)
                });
                assert!(
                    annotated || !generic_annotated,
                    "annotations of line {y}, step {step}"
                );
            }
        }
        assert!(
            screen.term.core.pages.history_size() > 1_000,
            "history reached packed pages"
        );
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
