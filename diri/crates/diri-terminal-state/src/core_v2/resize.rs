//! Migration step 5 (`docs/core-v2.md`): resize with reflow, a port of the
//! current core's `Term::resize` and `Grid::resize` (`grid/resize.rs`).
//!
//! Each screen's rows (history oldest first, then the screen) are streamed
//! through the current core's algorithm as current-core rows (`Row<Cell>`,
//! the vendored type, so `Row::shrink`, `is_clear` and the cell's
//! `is_empty` are the very functions the current core runs), and written
//! into new pages of the new geometry. Packed history is unpacked a page at
//! a time by the read cache and the new pages pack as they fill, so a resize
//! holds the screen and a handful of rows decoded, never the whole history.
//!
//! - **Lines** first (`grow_lines` / `shrink_lines`): growing pulls rows back
//!   from history, shrinking pushes the rows above the cursor into history
//!   and drops the rows below it, with the row limit applied as the current
//!   core's scroll does.
//! - **Columns** then (`grow_columns` / `shrink_columns`), with reflow on
//!   the shown primary screen and without it on the alternate screen (the
//!   current core passes `!is_alt` and `is_alt` the same way).
//! - The cursor, its pending wrap and the saved cursor of both screens move
//!   as the current core moves them; tab stops grow or shrink, and the
//!   scroll region is reset.
//! - The history origin counts the rows the current core counts as evicted
//!   (see [`Pages::history_origin`]).
//!
//! The display offset is always zero here: `HeadlessScreen` never scrolls
//! the display, so the offset bookkeeping of `Grid::resize` is left out.

use std::cmp::{max, min};
use std::collections::VecDeque;

use alacritty_terminal::grid::{Dimensions, Row};
use alacritty_terminal::index::{Boundary, Column, Line, Point};
use alacritty_terminal::term::cell::{Cell as GridCell, Flags, Hyperlink};

use super::Core;
use super::cell::{
    CP, Cell, HYPERLINK, LEADING, MARKS_SHIFT, SPACER, STYLE_FLAGS, STYLE_SHIFT, Style, WIDE, WRAP,
};
use super::check::flags_with;
use super::ops::Saved;
use super::page::{Link, Pages};

/// The OSC 133 marks, in the order of the cell's MARKS bits.
pub(super) const MARK_FLAGS: [Flags; 5] = [
    Flags::PROMPT_START,
    Flags::INPUT_START,
    Flags::OUTPUT_START,
    Flags::COMMAND_SUCCEEDED,
    Flags::COMMAND_FAILED,
];

/// The id a link read from a page gets when the program gave it none. A
/// link's cells in one row read as one link, as they do in the current core.
/// A parsed OSC 8 id never holds a NUL.
pub(super) const ANONYMOUS_LINK: &str = "\0core-v2";

/// The current core's cell for a v2 cell in `style`.
#[inline]
pub(super) fn grid_cell(cell: Cell, style: &Style, zerowidth: &[char], row_wrap: bool) -> GridCell {
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

/// Reads row `line` (negative: history) of `pages` into `out` as
/// current-core cells.
pub(super) fn read_row(pages: &Pages, line: i32, out: &mut Vec<GridCell>) {
    let cols = pages.cols;
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

/// Writes `cell` (a current-core cell) at column `x` of row `r` of page
/// `p`, which is writable: its character, style, width and wrap bits,
/// marks, combining characters and hyperlink.
pub(super) fn store(pages: &mut Pages, p: usize, r: usize, x: usize, cell: &GridCell) {
    let cols = pages.cols;
    let page = pages.page_mut(p);
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

/// Writes `cells` over the whole of screen line `line`: every cell up to the
/// last one that is not the default cell; the rest read as default blanks.
fn store_row(pages: &mut Pages, line: usize, cells: &[GridCell]) {
    let (p, r) = pages.locate(line);
    let default = GridCell::default();
    let end = cells
        .iter()
        .rposition(|cell| *cell != default)
        .map_or(0, |i| i + 1);
    let page = pages.page_mut(p);
    let meta = &mut page.meta[r];
    meta.occ = 0;
    meta.lead = 0;
    meta.wrapped = false;
    meta.dirty = true;
    meta.fill = page.styles.intern(&Style::default());
    page.text.reset(r);
    for (x, cell) in cells[..end].iter().enumerate() {
        store(pages, p, r, x, cell);
    }
}

/// `Grid` dimensions for the cursor arithmetic of the current core.
struct Dims {
    lines: usize,
    cols: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// One screen's cursor state through a resize, as the current core's
/// `Grid` keeps it.
struct Cursors {
    lines: usize,
    point: Point,
    input_needs_wrap: bool,
    saved: Point,
}

impl Cursors {
    fn of(lines: usize, cursor: &Saved, saved: &Saved) -> Cursors {
        Cursors {
            lines,
            point: Point::new(Line(cursor.line as i32), Column(cursor.col)),
            input_needs_wrap: cursor.wrap_pending,
            saved: Point::new(Line(saved.line as i32), Column(saved.col)),
        }
    }

    fn apply(&self, cursor: &mut Saved, saved: &mut Saved) {
        cursor.line = self.point.line.0 as usize;
        cursor.col = self.point.column.0;
        cursor.wrap_pending = self.input_needs_wrap;
        saved.line = self.saved.line.0 as usize;
        saved.col = self.saved.column.0;
    }

    /// The cursor's distance from the newest row, as `Grid::resize` names it.
    fn buffer_line(&self) -> usize {
        self.lines - self.point.line.0 as usize - 1
    }
}

/// The rows a column change reads, oldest first: old rows `next..end`
/// (negative: history), then `blanks` default rows of the old width.
struct Source<'a> {
    pages: &'a Pages,
    next: i32,
    end: i32,
    blanks: usize,
}

impl Source<'_> {
    fn len(&self) -> usize {
        (self.end - self.next) as usize + self.blanks
    }

    fn next_row(&mut self) -> Row<GridCell> {
        if self.next < self.end {
            let mut cells = Vec::with_capacity(self.pages.cols);
            read_row(self.pages, self.next, &mut cells);
            self.next += 1;
            let len = cells.len();
            Row::from_vec(cells, len)
        } else {
            debug_assert!(self.blanks > 0, "rows remain");
            self.blanks -= 1;
            Row::new(self.pages.cols)
        }
    }
}

/// The rows a column change writes, oldest first. The newest `keep` stay
/// decoded (the reflow still edits the newest, and the screen is read back);
/// older ones are written to the new pages as history.
struct Sink {
    pages: Pages,
    tail: VecDeque<Row<GridCell>>,
    keep: usize,
    /// Rows written to `pages` as history.
    written: usize,
}

impl Sink {
    fn new(pages: Pages) -> Sink {
        let keep = pages.lines + 2;
        Sink {
            pages,
            tail: VecDeque::with_capacity(keep + 1),
            keep,
            written: 0,
        }
    }

    fn len(&self) -> usize {
        self.written + self.tail.len()
    }

    fn last_mut(&mut self) -> Option<&mut Row<GridCell>> {
        self.tail.back_mut()
    }

    fn push(&mut self, row: Row<GridCell>) {
        self.tail.push_back(row);
        if self.tail.len() > self.keep {
            let row = self.tail.pop_front().expect("a row");
            self.write_history(row);
        }
    }

    /// Drops the `count` newest rows.
    fn truncate_newest(&mut self, count: usize) {
        assert!(count <= self.tail.len(), "only decoded rows are dropped");
        self.tail.truncate(self.tail.len() - count);
    }

    /// Screen line `line` once the rows are complete.
    fn screen_row(&self, line: usize) -> &Row<GridCell> {
        &self.tail[self.tail.len() - self.pages.lines + line]
    }

    /// Writes `row` as the newest history row: onto screen line 0, which
    /// then scrolls into history.
    fn write_history(&mut self, mut row: Row<GridCell>) {
        let cols = self.pages.cols;
        row.grow(cols);
        let cells: Vec<GridCell> = (0..cols).map(|x| row[Column(x)].clone()).collect();
        store_row(&mut self.pages, 0, &cells);
        self.pages.push_row(&Style::default());
        self.written += 1;
    }

    /// Writes the remaining rows, the newest `lines` of them as the screen;
    /// returns the pages and the history rows written.
    fn finish(mut self) -> (Pages, usize) {
        let lines = self.pages.lines;
        assert!(self.tail.len() >= lines, "the screen is filled");
        while self.tail.len() > lines {
            let row = self.tail.pop_front().expect("a row");
            self.write_history(row);
        }
        let cols = self.pages.cols;
        for (line, mut row) in self.tail.drain(..).enumerate() {
            row.grow(cols);
            let cells: Vec<GridCell> = (0..cols).map(|x| row[Column(x)].clone()).collect();
            store_row(&mut self.pages, line, &cells);
        }
        (self.pages, self.written)
    }
}

/// The new pages of one screen, and what its history origin gains.
///
/// The current core counts the history rows its row limit drops while the
/// line count changes (the scroll it runs counts them), but not the rows
/// past the limit that narrowing re-wraps history into (`shrink_columns`
/// truncates its rows before `update_history` measures them).
struct Resized {
    pages: Pages,
    origin: i64,
}

/// `Grid::resize` of one screen: `pages` with `cursors`, to `lines` ×
/// `cols`, reflowing when `reflow`.
fn resize_grid(
    pages: &Pages,
    cursors: &mut Cursors,
    reflow: bool,
    lines: usize,
    cols: usize,
) -> Resized {
    let old_lines = pages.lines;
    let history = pages.history_size();
    let limit = pages.history_limit;
    let mut source = Source {
        pages,
        next: -(history as i32),
        end: old_lines as i32,
        blanks: 0,
    };
    let mut evicted = 0u64;
    cursors.lines = lines;
    if lines > old_lines {
        // `grow_lines`: rows come back from history; any more are blank
        // rows at the bottom, after the scroll that the current core runs
        // through history (which can count rows in its origin).
        let added = lines - old_lines;
        let from_history = min(history, added);
        if from_history != added {
            let delta = added - from_history;
            let created = min(delta, limit - history);
            evicted += min(delta - created, history) as u64;
            source.blanks = delta;
        }
        cursors.saved.line += from_history;
        cursors.point.line += from_history;
    } else if lines < old_lines {
        // `shrink_lines`: the rows above the cursor that no longer fit go
        // to history (under the row limit); the rows below it are dropped.
        let required = (cursors.point.line.0 as usize + 1).saturating_sub(lines);
        if required > 0 {
            let created = min(required, limit - history);
            let overflow = required - created;
            evicted += min(overflow, history) as u64;
            source.next += overflow as i32;
            cursors.point.line = min(cursors.point.line, Line(lines as i32 - 1));
        }
        cursors.saved.line = min(cursors.saved.line, Line(lines as i32 - 1));
        source.end = (lines + required) as i32;
    }
    let mut sink = Sink::new(pages.resized(lines, cols));
    match pages.cols.cmp(&cols) {
        std::cmp::Ordering::Less => grow_columns(cursors, reflow, cols, &mut source, &mut sink),
        std::cmp::Ordering::Greater => {
            shrink_columns(cursors, reflow, cols, &mut source, &mut sink)
        }
        std::cmp::Ordering::Equal => {
            for _ in 0..source.len() {
                let row = source.next_row();
                sink.push(row);
            }
        }
    }
    pages.release_read_cache();
    let (pages, written) = sink.finish();
    let uncounted = written.saturating_sub(limit) as i64;
    Resized {
        pages,
        origin: evicted as i64 - uncounted,
    }
}

/// `Grid::grow_columns`.
fn grow_columns(
    cursors: &mut Cursors,
    reflow: bool,
    columns: usize,
    source: &mut Source,
    sink: &mut Sink,
) {
    let should_reflow = |row: &Row<GridCell>| -> bool {
        let len = row.len();
        reflow && len > 0 && len < columns && row[Column(len - 1)].flags.contains(Flags::WRAPLINE)
    };
    let dims = Dims {
        lines: cursors.lines,
        cols: columns,
    };
    let mut cursor_line_delta = 0;

    // Remove the linewrap special case, by moving the cursor outside of the grid.
    if cursors.input_needs_wrap && reflow {
        cursors.input_needs_wrap = false;
        cursors.point.column += 1;
    }

    let total = source.len();
    for k in 0..total {
        let i = total - 1 - k;
        let mut row = source.next_row();
        let last_row = match sink.last_mut() {
            Some(last_row) if should_reflow(last_row) => last_row,
            _ => {
                sink.push(row);
                continue;
            }
        };

        // Remove wrap flag before appending additional cells.
        if let Some(cell) = last_row.last_mut() {
            cell.flags.remove(Flags::WRAPLINE);
        }

        // Remove leading spacers when reflowing wide char to the previous line.
        let mut last_len = last_row.len();
        if last_len >= 1
            && last_row[Column(last_len - 1)]
                .flags
                .contains(Flags::LEADING_WIDE_CHAR_SPACER)
        {
            last_row.shrink(last_len - 1);
            last_len -= 1;
        }

        // Don't try to pull more cells from the next line than available.
        let mut num_wrapped = columns - last_len;
        let len = min(row.len(), num_wrapped);

        // Insert leading spacer when there's not enough room for reflowing wide char.
        let mut cells = if row[Column(len - 1)].flags.contains(Flags::WIDE_CHAR) {
            num_wrapped -= 1;

            let mut cells = row.front_split_off(len - 1);

            let mut spacer = GridCell::default();
            spacer.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
            cells.push(spacer);

            cells
        } else {
            row.front_split_off(len)
        };

        // Add removed cells to previous row and reflow content.
        last_row.append(&mut cells);

        let cursor_buffer_line = cursors.buffer_line();

        if i == cursor_buffer_line && reflow {
            // Resize cursor's line and reflow the cursor if necessary.
            let mut target = cursors.point.sub(&dims, Boundary::Cursor, num_wrapped);

            // Clamp to the last column, if no content was reflown with the cursor.
            if target.column.0 == 0 && row.is_clear() {
                cursors.input_needs_wrap = true;
                target = target.sub(&dims, Boundary::Cursor, 1);
            }
            cursors.point.column = target.column;

            // Get required cursor line changes. Since `num_wrapped` is smaller than `columns`
            // this will always be either `0` or `1`.
            let line_delta = cursors.point.line - target.line;

            if line_delta != 0 && row.is_clear() {
                continue;
            }

            cursor_line_delta += line_delta.0 as usize;
        } else if row.is_clear() {
            // Rotate cursor down if content below them was pulled from history.
            if i < cursor_buffer_line {
                cursors.point.line += 1;
            }

            // Don't push line into the new buffer.
            continue;
        }

        if let Some(cell) = last_row.last_mut() {
            // Set wrap flag if next line still has cells.
            cell.flags.insert(Flags::WRAPLINE);
        }

        sink.push(row);
    }

    // Make sure we have at least the viewport filled.
    let lines = cursors.lines;
    if sink.len() < lines {
        let delta = (lines - sink.len()) as i32;
        cursors.point.line = max(cursors.point.line - delta, Line(0));
        while sink.len() < lines {
            sink.push(Row::new(columns));
        }
    }

    // Pull content down to put cursor in correct position, or move cursor up if there's no
    // more lines to delete below the cursor.
    if cursor_line_delta != 0 {
        let cursor_buffer_line = cursors.buffer_line();
        let available = min(cursor_buffer_line, sink.len() - lines);
        let overflow = cursor_line_delta.saturating_sub(available);
        sink.truncate_newest(cursor_line_delta - overflow);
        cursors.point.line = max(cursors.point.line - overflow, Line(0));
    }
}

/// `Grid::shrink_columns`.
fn shrink_columns(
    cursors: &mut Cursors,
    reflow: bool,
    columns: usize,
    source: &mut Source,
    sink: &mut Sink,
) {
    // Remove the linewrap special case, by moving the cursor outside of the grid.
    if cursors.input_needs_wrap && reflow {
        cursors.input_needs_wrap = false;
        cursors.point.column += 1;
    }

    let mut buffered: Option<Vec<GridCell>> = None;

    let total = source.len();
    for k in 0..total {
        let i = total - 1 - k;
        let mut row = source.next_row();
        // Append lines left over from the previous row.
        if let Some(buffered) = buffered.take() {
            // Add a column for every cell added before the cursor, if it goes beyond the new
            // width it is then later reflown.
            if i == cursors.buffer_line() {
                cursors.point.column += buffered.len();
            }

            row.append_front(buffered);
        }

        loop {
            // Remove all cells which require reflowing.
            let mut wrapped = match row.shrink(columns) {
                Some(wrapped) if reflow => wrapped,
                _ => {
                    if reflow
                        && i == cursors.buffer_line()
                        && cursors.point.column > Column(columns)
                    {
                        // If there are empty cells before the cursor, we assume it is explicit
                        // whitespace and need to wrap it like normal content.
                        Vec::new()
                    } else {
                        // Since it fits, just push the existing line without any reflow.
                        sink.push(row);
                        break;
                    }
                }
            };

            // Insert spacer if a wide char would be wrapped into the last column.
            //
            // Not at one column: there the current core moves the wide
            // character to the next row, where it is in the last column
            // again, and never finishes (each round pushes a row). Here it
            // stays, and its spacer wraps to the next row as any cell does.
            if columns > 1
                && row.len() >= columns
                && row[Column(columns - 1)].flags.contains(Flags::WIDE_CHAR)
            {
                let mut spacer = GridCell::default();
                spacer.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);

                let wide_char = std::mem::replace(&mut row[Column(columns - 1)], spacer);
                wrapped.insert(0, wide_char);
            }

            // Remove wide char spacer before shrinking.
            let len = wrapped.len();
            if len > 0
                && wrapped[len - 1]
                    .flags
                    .contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                if len == 1 {
                    row[Column(columns - 1)].flags.insert(Flags::WRAPLINE);
                    sink.push(row);
                    break;
                } else {
                    // Remove the leading spacer from the end of the wrapped row.
                    wrapped[len - 2].flags.insert(Flags::WRAPLINE);
                    wrapped.truncate(len - 1);
                }
            }

            sink.push(row);

            // Set line as wrapped if cells got removed.
            if let Some(cell) = sink.last_mut().and_then(|r| r.last_mut()) {
                cell.flags.insert(Flags::WRAPLINE);
            }

            if wrapped
                .last()
                .map(|c| c.flags.contains(Flags::WRAPLINE) && i >= 1)
                .unwrap_or(false)
                && wrapped.len() < columns
            {
                // Make sure previous wrap flag doesn't linger around.
                if let Some(cell) = wrapped.last_mut() {
                    cell.flags.remove(Flags::WRAPLINE);
                }

                // Add removed cells to start of next row.
                buffered = Some(wrapped);
                break;
            } else {
                // Reflow cursor if a line below it is deleted.
                let cursor_buffer_line = cursors.buffer_line();
                if (i == cursor_buffer_line && cursors.point.column < Column(columns))
                    || i < cursor_buffer_line
                {
                    cursors.point.line = max(cursors.point.line - 1, Line(0));
                }

                // Reflow the cursor if it is on this line beyond the width.
                if i == cursor_buffer_line && cursors.point.column >= Column(columns) {
                    // Since only a single new line is created, we subtract only `columns`
                    // from the cursor instead of reflowing it completely.
                    cursors.point.column -= columns;
                }

                // Make sure new row is at least as long as new width.
                let occ = wrapped.len();
                if occ < columns {
                    wrapped.resize_with(columns, GridCell::default);
                }
                row = Row::from_vec(wrapped, occ);
            }
        }
    }

    // Reflow the primary cursor, or clamp it if reflow is disabled.
    let dims = Dims {
        lines: cursors.lines,
        cols: columns,
    };
    if !reflow {
        cursors.point.column = min(cursors.point.column, Column(columns - 1));
    } else if cursors.point.column == Column(columns)
        && !sink.screen_row(cursors.point.line.0 as usize)[Column(columns - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    {
        cursors.input_needs_wrap = true;
        cursors.point.column -= 1;
    } else {
        cursors.point = cursors.point.grid_clamp(&dims, Boundary::Cursor);
    }

    // Clamp the saved cursor to the grid.
    cursors.saved.column = min(cursors.saved.column, Column(columns - 1));
}

impl Core {
    /// `Term::resize`: both screens to `lines` × `cols`, the shown primary
    /// screen with reflow and the alternate screen without; tab stops
    /// resized, the scroll region reset.
    pub fn resize(&mut self, lines: usize, cols: usize) {
        assert!(lines >= 1 && cols >= 1, "a screen has a cell");
        let (old_lines, old_cols) = (self.lines, self.cols);
        if old_lines == lines && old_cols == cols {
            return;
        }
        let is_alt = self
            .mode
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);

        // The shown screen.
        let mut cursor = self.cursor_state();
        let mut saved = self.saved.clone();
        let mut cursors = Cursors::of(old_lines, &cursor, &saved);
        let resized = resize_grid(&self.pages, &mut cursors, !is_alt, lines, cols);
        let origin = self.pages.history_origin() as i64 + resized.origin;
        self.pages = resized.pages;
        self.pages.origin_bias = origin;
        cursors.apply(&mut cursor, &mut saved);
        self.line = cursor.line;
        self.col = cursor.col;
        self.wrap_pending = cursor.wrap_pending;
        self.saved = saved;

        // The other screen.
        if let Some(other) = self.other.as_mut() {
            let mut cursors = Cursors::of(old_lines, &other.cursor, &other.saved);
            let resized = resize_grid(&other.pages, &mut cursors, is_alt, lines, cols);
            let origin = other.pages.history_origin() as i64 + resized.origin;
            other.pages = resized.pages;
            other.pages.origin_bias = origin;
            cursors.apply(&mut other.cursor, &mut other.saved);
        }

        self.lines = lines;
        self.cols = cols;
        // Style ids are per page and the pages are new.
        self.pen_cache = (0, 0);
        if old_cols != cols {
            let mut index = self.tabs.len();
            self.tabs.resize_with(cols, || {
                let stop = index.is_multiple_of(8);
                index += 1;
                stop
            });
        }
        self.region = (0, lines);
    }
}
