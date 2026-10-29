//! The one interface through which [`HeadlessScreen`] (and its inline-image
//! placement) reads and writes the terminal emulator: geometry, the cursor,
//! cells, modes, colors, damage, history management, keyboard stacks and the
//! cell writes a checkpoint restore makes.
//!
//! The emulator in use is chosen once, by [`Emu`]. Everything is static
//! dispatch: cell reads return the emulator's own cells by reference, and the
//! helpers that read them are generic over [`CellView`] and [`RowView`], so
//! they monomorphize to the same code as reading the cells directly.
//!
//! [`HeadlessScreen`]: crate::HeadlessScreen

use alacritty_terminal::grid::{Dimensions, Row};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::keyboard::KeyboardSnapshot;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorStyle, Handler};
use diri_proto::grid::GridCell;

use crate::{Collector, Geometry};

/// The emulator behind every [`crate::HeadlessScreen`].
pub(crate) type Emu = Term<Collector>;

/// An OSC 8 hyperlink as a cell holds it. Two links are equal when they are
/// the same link (target and id), as consecutive cells of one link are.
pub(crate) trait LinkView: PartialEq + Clone {
    /// The link's target URI.
    fn target(&self) -> &str;
}

/// Read access to one emulator cell.
pub(crate) trait CellView {
    type Link: LinkView;

    /// The base character; `'\0'` or `' '` when blank.
    fn ch(&self) -> char;
    fn fg_color(&self) -> Color;
    fn bg_color(&self) -> Color;
    fn cell_flags(&self) -> Flags;
    /// Whether the cell carries anything beyond its character, colors and
    /// flags: a link, combining marks or an underline color. A cell without
    /// it has none of them.
    fn has_extra(&self) -> bool;
    /// Zero-width characters combined with the base character.
    fn combining(&self) -> Option<&[char]>;
    /// The SGR 58 underline color.
    fn underline(&self) -> Option<Color>;
    fn link(&self) -> Option<Self::Link>;
}

/// The cell writes a checkpoint restore and history annotations make.
pub(crate) trait CellWrite: CellView {
    fn insert_flags(&mut self, flags: Flags);
    fn remove_flags(&mut self, flags: Flags);
    /// A new link to `uri` with no explicit id. Every call makes a distinct
    /// link; clone one to give cells the same link.
    fn new_link(uri: String) -> Self::Link;
    fn attach_link(&mut self, link: Self::Link);
    fn append_zerowidth(&mut self, ch: char);
    fn put_underline_color(&mut self, color: Option<Color>);
}

/// Read access to one row of cells.
pub(crate) trait RowView {
    type Cell: CellView;

    /// Every cell of the row, left to right.
    fn cells(&self) -> &[Self::Cell];
    /// The number of leading cells up to the last one that is not a default
    /// cell; every cell after it is default.
    fn content_width(&self) -> usize;
}

pub(crate) trait Emulator: Handler + Sized {
    type Cell: CellWrite;
    type Row: RowView<Cell = Self::Cell>;
    /// The attributes new characters are written with, saved and restored
    /// around writes that must not change them.
    type Pen;

    fn create(config: Config, geometry: &Geometry, events: Collector) -> Self;

    // Geometry.

    fn cols(&self) -> usize;
    fn screen_rows(&self) -> usize;
    /// Resizes both screens, reflowing the primary screen and its history.
    fn resize_screen(&mut self, geometry: Geometry);

    // Options.

    fn options(&self) -> &Config;
    /// Replaces the options, applying the history limit to the primary
    /// screen, and re-announces the title.
    fn replace_options(&mut self, options: Config);

    // Cursor.

    fn cursor_point(&self) -> Point;
    /// Whether the next printed character wraps first (the cursor sits past
    /// the last column).
    fn cursor_pending_wrap(&self) -> bool;
    fn place_cursor(&mut self, point: Point, pending_wrap: bool);
    fn pen(&self) -> Self::Pen;
    fn restore_pen(&mut self, pen: Self::Pen);
    /// Sets a default pen, linked to `uri` when given.
    fn set_link_pen(&mut self, uri: Option<String>);

    // Cells. Lines below zero are history, oldest at `-history_rows()`.

    fn row(&self, line: Line) -> &Self::Row;
    fn cell(&self, line: Line, column: Column) -> &Self::Cell {
        &self.row(line).cells()[column.0]
    }
    fn cell_mut(&mut self, line: Line, column: Column) -> &mut Self::Cell;
    /// Replaces a cell with the emulator form of a wire cell.
    fn write_cell(&mut self, line: Line, column: Column, cell: GridCell);

    // Modes, colors and the cursor style.

    fn modes(&self) -> TermMode;
    /// Colors the program redefined (OSC 4/10/11/12), by index.
    fn program_colors(&self) -> &Colors;
    /// Advances whenever [`Self::program_colors`] changes.
    fn program_colors_generation(&self) -> u64;
    /// The cursor the program asked for (DECSCUSR, mode 12).
    fn program_cursor_style(&self) -> CursorStyle;

    // Damage since the last [`Self::clear_damage`].

    /// True when the whole screen is damaged; otherwise calls `on_line` with
    /// every damaged screen line.
    fn collect_damage(&mut self, on_line: impl FnMut(usize)) -> bool;
    /// For each screen line, the line whose cells it held at the last
    /// [`Self::clear_damage`] when scrolling only moved them there, or
    /// `alacritty_terminal::term::CONTENT_CHANGED`; `None` while unknown.
    fn moved_line_sources(&self) -> Option<&[u32]>;
    fn clear_damage(&mut self);

    // History.

    /// Rows above the visible screen on the active screen.
    fn history_rows(&self) -> usize;
    /// Rows that have left the active screen's history from its oldest end,
    /// so `history_origin() + row` names a history row stably while output
    /// evicts older ones.
    fn history_origin(&self) -> u64;
    /// History rows of the primary screen, also while the alternate one shows.
    #[cfg(feature = "compact-history")]
    fn primary_history_rows(&self) -> usize;
    /// The primary history's row limit; a lower one drops the oldest rows.
    #[cfg(feature = "compact-history")]
    fn set_history_limit(&mut self, limit: usize);
    /// Scrolls the whole screen up by `rows`, pushing them into history.
    fn push_screen_into_history(&mut self, rows: usize);
    /// Memory held per history row of `cols` cells while it is decoded.
    #[cfg(feature = "compact-history")]
    fn decoded_row_bytes(cols: usize) -> usize;
    #[cfg(feature = "compact-history")]
    fn history_storage_bytes(&self) -> usize;
    #[cfg(feature = "compact-history")]
    fn bound_history_storage(&mut self, budget: usize);
    #[cfg(feature = "compact-history")]
    fn keep_history_spare_rows(&mut self, keep: bool);
    #[cfg(feature = "compact-history")]
    fn release_history_spare_rows(&mut self);
    #[cfg(feature = "compact-history")]
    fn release_idle_history_memory(&mut self);
    #[cfg(feature = "compact-history")]
    fn prepare_history_read(&mut self);
    #[cfg(feature = "compact-history")]
    fn release_history_read_cache(&mut self);
    /// Pages idle primary history out to `store`; see
    /// [`crate::HeadlessScreen::page_out_history`].
    #[cfg(feature = "compact-history")]
    fn page_out_history(
        &mut self,
        store: &std::sync::Arc<dyn crate::ColdStore>,
        keep_rows: usize,
        min_bytes: usize,
    ) -> std::io::Result<crate::PageOut>;
    #[cfg(feature = "compact-history")]
    fn history_residency(&self) -> crate::PayloadResidency;

    // Keyboard protocol.

    fn kitty_keyboard_enabled(&self) -> bool;
    fn keyboard_stacks(&self) -> KeyboardSnapshot;
    fn can_restore_keyboard_stacks(&self, snapshot: &KeyboardSnapshot) -> bool;
    fn restore_keyboard_stacks(&mut self, snapshot: &KeyboardSnapshot) -> bool;
}

impl LinkView for Hyperlink {
    #[inline]
    fn target(&self) -> &str {
        self.uri()
    }
}

impl CellView for Cell {
    type Link = Hyperlink;

    #[inline(always)]
    fn ch(&self) -> char {
        self.c
    }

    #[inline(always)]
    fn fg_color(&self) -> Color {
        self.fg
    }

    #[inline(always)]
    fn bg_color(&self) -> Color {
        self.bg
    }

    #[inline(always)]
    fn cell_flags(&self) -> Flags {
        self.flags
    }

    #[inline(always)]
    fn has_extra(&self) -> bool {
        self.extra.is_some()
    }

    #[inline]
    fn combining(&self) -> Option<&[char]> {
        self.zerowidth()
    }

    #[inline]
    fn underline(&self) -> Option<Color> {
        self.underline_color()
    }

    #[inline]
    fn link(&self) -> Option<Hyperlink> {
        self.hyperlink()
    }
}

impl CellWrite for Cell {
    #[inline]
    fn insert_flags(&mut self, flags: Flags) {
        self.flags.insert(flags);
    }

    #[inline]
    fn remove_flags(&mut self, flags: Flags) {
        self.flags.remove(flags);
    }

    fn new_link(uri: String) -> Hyperlink {
        Hyperlink::new(None::<String>, uri)
    }

    fn attach_link(&mut self, link: Hyperlink) {
        self.set_hyperlink(Some(link));
    }

    fn append_zerowidth(&mut self, ch: char) {
        self.push_zerowidth(ch);
    }

    fn put_underline_color(&mut self, color: Option<Color>) {
        self.set_underline_color(color);
    }
}

impl RowView for Row<Cell> {
    type Cell = Cell;

    #[inline(always)]
    fn cells(&self) -> &[Cell] {
        &self[..]
    }

    #[inline]
    fn content_width(&self) -> usize {
        self.content_len(Cell::is_default)
    }
}

impl Emulator for Term<Collector> {
    type Cell = Cell;
    type Row = Row<Cell>;
    type Pen = Cell;

    fn create(config: Config, geometry: &Geometry, events: Collector) -> Self {
        let term = Term::new(config, geometry, events);
        #[cfg(feature = "compact-history")]
        let term = {
            let mut term = term;
            term.grid_mut().enable_compact_history();
            term
        };
        term
    }

    #[inline]
    fn cols(&self) -> usize {
        Dimensions::columns(self)
    }

    #[inline]
    fn screen_rows(&self) -> usize {
        Dimensions::screen_lines(self)
    }

    fn resize_screen(&mut self, geometry: Geometry) {
        self.resize(geometry);
    }

    #[inline]
    fn options(&self) -> &Config {
        self.config()
    }

    fn replace_options(&mut self, options: Config) {
        self.set_options(options);
    }

    #[inline]
    fn cursor_point(&self) -> Point {
        self.grid().cursor.point
    }

    #[inline]
    fn cursor_pending_wrap(&self) -> bool {
        self.grid().cursor.input_needs_wrap
    }

    #[inline]
    fn place_cursor(&mut self, point: Point, pending_wrap: bool) {
        let cursor = &mut self.grid_mut().cursor;
        cursor.point = point;
        cursor.input_needs_wrap = pending_wrap;
    }

    fn pen(&self) -> Cell {
        self.grid().cursor.template.clone()
    }

    fn restore_pen(&mut self, pen: Cell) {
        self.grid_mut().cursor.template = pen;
    }

    fn set_link_pen(&mut self, uri: Option<String>) {
        let mut template = Cell::default();
        if let Some(uri) = uri {
            template.set_hyperlink(Some(Hyperlink::new(None::<&str>, uri)));
        }
        self.grid_mut().cursor.template = template;
    }

    #[inline(always)]
    fn row(&self, line: Line) -> &Row<Cell> {
        &self.grid()[line]
    }

    #[inline]
    fn cell_mut(&mut self, line: Line, column: Column) -> &mut Cell {
        &mut self.grid_mut()[line][column]
    }

    fn write_cell(&mut self, line: Line, column: Column, cell: GridCell) {
        self.grid_mut()[line][column] = crate::emulator_cell(cell);
    }

    #[inline]
    fn modes(&self) -> TermMode {
        *self.mode()
    }

    #[inline]
    fn program_colors(&self) -> &Colors {
        self.colors()
    }

    #[inline]
    fn program_colors_generation(&self) -> u64 {
        self.colors_generation()
    }

    #[inline]
    fn program_cursor_style(&self) -> CursorStyle {
        self.cursor_style()
    }

    fn collect_damage(&mut self, mut on_line: impl FnMut(usize)) -> bool {
        match self.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(lines) => {
                for damage in lines {
                    on_line(damage.line);
                }
                false
            }
        }
    }

    #[inline]
    fn moved_line_sources(&self) -> Option<&[u32]> {
        self.damage_content_sources()
    }

    fn clear_damage(&mut self) {
        self.reset_damage();
    }

    #[inline]
    fn history_rows(&self) -> usize {
        self.grid().history_size()
    }

    #[inline]
    fn history_origin(&self) -> u64 {
        self.grid().history_origin()
    }

    #[cfg(feature = "compact-history")]
    fn primary_history_rows(&self) -> usize {
        self.primary_history_size()
    }

    #[cfg(feature = "compact-history")]
    fn set_history_limit(&mut self, limit: usize) {
        self.set_scrolling_history(limit);
    }

    fn push_screen_into_history(&mut self, rows: usize) {
        let lines = self.grid().screen_lines() as i32;
        self.grid_mut().scroll_up(&(Line(0)..Line(lines)), rows);
    }

    #[cfg(feature = "compact-history")]
    fn decoded_row_bytes(cols: usize) -> usize {
        cols.saturating_mul(std::mem::size_of::<Cell>())
            .saturating_add(std::mem::size_of::<Row<Cell>>())
    }

    #[cfg(feature = "compact-history")]
    fn history_storage_bytes(&self) -> usize {
        self.grid().history_storage_bytes()
    }

    #[cfg(feature = "compact-history")]
    fn bound_history_storage(&mut self, budget: usize) {
        self.bound_primary_history_storage(budget);
    }

    #[cfg(feature = "compact-history")]
    fn keep_history_spare_rows(&mut self, keep: bool) {
        self.keep_primary_history_spare_rows(keep);
    }

    #[cfg(feature = "compact-history")]
    fn release_history_spare_rows(&mut self) {
        self.release_primary_history_spare_rows();
    }

    #[cfg(feature = "compact-history")]
    fn release_idle_history_memory(&mut self) {
        self.release_idle_history();
    }

    #[cfg(feature = "compact-history")]
    fn prepare_history_read(&mut self) {
        self.grid_mut().prepare_history_read();
    }

    #[cfg(feature = "compact-history")]
    fn release_history_read_cache(&mut self) {
        self.grid_mut().release_history_read_cache();
    }

    #[cfg(feature = "compact-history")]
    fn page_out_history(
        &mut self,
        store: &std::sync::Arc<dyn crate::ColdStore>,
        keep_rows: usize,
        min_bytes: usize,
    ) -> std::io::Result<crate::PageOut> {
        self.page_out_primary_history(store, keep_rows, min_bytes)
    }

    #[cfg(feature = "compact-history")]
    fn history_residency(&self) -> crate::PayloadResidency {
        self.primary_history_residency()
    }

    fn kitty_keyboard_enabled(&self) -> bool {
        self.keyboard_enhancements_enabled()
    }

    fn keyboard_stacks(&self) -> KeyboardSnapshot {
        self.keyboard_snapshot()
    }

    fn can_restore_keyboard_stacks(&self, snapshot: &KeyboardSnapshot) -> bool {
        self.can_restore_keyboard_snapshot(snapshot)
    }

    fn restore_keyboard_stacks(&mut self, snapshot: &KeyboardSnapshot) -> bool {
        self.restore_keyboard_snapshot(snapshot)
    }
}
