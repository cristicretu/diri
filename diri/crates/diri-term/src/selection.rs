//! Scroll-invariant terminal selection over a composed history/live window.

use diri_proto::grid::{GridCell, TermStyle};

use crate::buffer::GridBuffer;
use crate::scrollback::ScrollbackViewport;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SelectionPoint {
    pub row: i64,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub start: SelectionPoint,
    /// Reading-order exclusive endpoint.
    pub end: SelectionPoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionSpan {
    pub row: usize,
    pub start_col: usize,
    pub end_col_exclusive: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WordClass {
    Word,
    Whitespace,
    Punctuation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Granularity {
    #[default]
    Cell,
    Word,
    Line,
}

#[derive(Clone, Debug, Default)]
pub struct TerminalSelection {
    anchor: Option<SelectionPoint>,
    head: Option<SelectionPoint>,
    granularity: Granularity,
    initial: Option<SelectionRange>,
    rectangle: bool,
    /// The cell [`Self::begin`] was given, for keyboard selection by cells.
    anchor_cell: Option<SelectionPoint>,
}

impl TerminalSelection {
    #[must_use]
    pub const fn anchor(&self) -> Option<SelectionPoint> {
        self.anchor
    }

    #[must_use]
    pub const fn head(&self) -> Option<SelectionPoint> {
        self.head
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn begin(&mut self, point: SelectionPoint) {
        self.clear();
        self.anchor = Some(point);
        self.head = Some(point);
        self.anchor_cell = Some(point);
    }

    /// Selects from the cell given to [`Self::begin`] through `cell`, both
    /// included, in either direction: keyboard copy mode moves a cursor over
    /// cells rather than a pointer over boundaries.
    pub fn drag_cells_to(&mut self, cell: SelectionPoint) {
        let Some(anchor) = self.anchor_cell else {
            return;
        };
        let after = |point: SelectionPoint| SelectionPoint {
            row: point.row,
            col: point.col + 1,
        };
        if cell >= anchor {
            self.anchor = Some(anchor);
            self.head = Some(after(cell));
        } else {
            self.anchor = Some(after(anchor));
            self.head = Some(cell);
        }
    }

    pub fn drag_to(&mut self, point: SelectionPoint) {
        if self.anchor.is_some() {
            self.head = Some(point);
        }
    }

    pub fn begin_rectangle(&mut self, point: SelectionPoint) {
        self.begin(point);
        self.rectangle = true;
    }

    pub fn drag_in_view(
        &mut self,
        viewport: &ScrollbackViewport,
        buffer: &GridBuffer,
        row: usize,
        col: usize,
    ) {
        self.drag_in_view_to_edge(viewport, buffer, row, col, col);
    }

    /// [`Self::drag_in_view`] for a pointer: a character drag ends at `edge`,
    /// the cell boundary nearest the pointer (0 to `cols` inclusive), so a
    /// cell joins the selection once the pointer passes its middle and the
    /// last column can be reached. Word and line drags extend by the cell
    /// under the pointer, `col`.
    pub fn drag_in_view_to_edge(
        &mut self,
        viewport: &ScrollbackViewport,
        buffer: &GridBuffer,
        row: usize,
        col: usize,
        edge: usize,
    ) {
        let point = SelectionPoint {
            row: viewport.absolute_row(row),
            col,
        };
        let edge_point = SelectionPoint {
            row: point.row,
            col: edge,
        };
        let Some(initial) = self.initial else {
            self.drag_to(edge_point);
            return;
        };
        let mut target = Self::default();
        match self.granularity {
            Granularity::Cell => {
                self.drag_to(edge_point);
                return;
            }
            Granularity::Word => target.select_word(viewport, buffer, row, col),
            Granularity::Line => target.select_line(viewport, buffer, row),
        }
        let Some(range) = target.range() else {
            return;
        };
        if point < initial.start {
            self.anchor = Some(initial.end);
            self.head = Some(range.start);
        } else {
            self.anchor = Some(initial.start);
            self.head = Some(initial.end.max(range.end));
        }
    }

    fn columns(&self, range: SelectionRange, row: i64, cols: usize) -> (usize, usize) {
        if self.rectangle {
            // Both ends are cell boundaries, like a character selection's.
            let a = self.anchor.unwrap().col;
            let b = self.head.unwrap().col;
            (a.min(b).min(cols), a.max(b).min(cols))
        } else {
            columns_for_row(range, row, cols)
        }
    }

    pub fn set_from_window(
        &mut self,
        viewport: &ScrollbackViewport,
        anchor_col: usize,
        anchor_row: usize,
        head_col: usize,
        head_row: usize,
    ) {
        self.clear();
        self.anchor = Some(SelectionPoint {
            row: viewport.absolute_row(anchor_row),
            col: anchor_col,
        });
        self.head = Some(SelectionPoint {
            row: viewport.absolute_row(head_row),
            col: head_col,
        });
    }

    /// Expands a double-click to a run of word, whitespace, or punctuation
    /// cells. The run follows soft wraps, so a URL or path longer than the
    /// pane is one word, and a double-width glyph's spacer belongs to its
    /// glyph rather than breaking the word. The resulting endpoint is
    /// exclusive.
    pub fn select_word(
        &mut self,
        viewport: &ScrollbackViewport,
        buffer: &GridBuffer,
        window_row: usize,
        col: usize,
    ) {
        self.clear();
        let cols = usize::from(buffer.cols);
        if cols == 0 {
            return;
        }
        let line = LogicalLine::around(viewport, buffer, viewport.absolute_row(window_row));
        let Some(at) = line.index(viewport.absolute_row(window_row), col.min(cols - 1)) else {
            return;
        };
        let cells = &line.cells;
        let (start, end) = url_span_at(cells, at).unwrap_or_else(|| {
            let classes = word_classes(cells);
            let class = classes[at];
            let mut start = at;
            while start > 0 && classes[start - 1] == class {
                start -= 1;
            }
            let mut end = at + 1;
            while end < cells.len() && classes[end] == class {
                end += 1;
            }
            (start, end)
        });
        self.anchor = Some(line.start_point(start));
        self.head = Some(line.end_point(end));
        self.granularity = Granularity::Word;
        self.initial = self.range();
    }

    /// Expands a triple-click to the whole line the program wrote: every row
    /// joined to this one by soft wraps. Terminal padding is retained in the
    /// range so dragging can continue naturally, while copy still trims the
    /// padding through [`Self::selected_text`].
    pub fn select_line(
        &mut self,
        viewport: &ScrollbackViewport,
        buffer: &GridBuffer,
        window_row: usize,
    ) {
        self.clear();
        if buffer.cols == 0 {
            return;
        }
        let line = LogicalLine::around(viewport, buffer, viewport.absolute_row(window_row));
        if line.cells.is_empty() {
            return;
        }
        self.anchor = Some(line.start_point(0));
        self.head = Some(line.end_point(line.cells.len()));
        self.granularity = Granularity::Line;
        self.initial = self.range();
    }

    #[must_use]
    pub fn range(&self) -> Option<SelectionRange> {
        let anchor = self.anchor?;
        let head = self.head?;
        if anchor == head {
            return None;
        }
        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        Some(SelectionRange { start, end })
    }

    /// Whether a full-row content repaint would replace any selected cell.
    ///
    /// Selection endpoints are exclusive, so an endpoint at column zero does
    /// not make that final row overlap. Keeping that distinction prevents a
    /// repaint immediately below a selection from dismissing it.
    #[must_use]
    pub(crate) fn overlaps_row(&self, absolute_row: i64, cols: usize) -> bool {
        let Some(range) = self.range() else {
            return false;
        };
        if absolute_row < range.start.row || absolute_row > range.end.row {
            return false;
        }
        let (start, end) = self.columns(range, absolute_row, cols);
        start < end
    }

    #[must_use]
    pub fn visible_spans(
        &self,
        viewport: &ScrollbackViewport,
        visible_rows: usize,
        cols: usize,
    ) -> Vec<SelectionSpan> {
        let Some(range) = self.range() else {
            return Vec::new();
        };
        if cols == 0 {
            return Vec::new();
        }
        let window_top = viewport.absolute_row(0);
        let window_bottom =
            window_top.saturating_add(i64::try_from(visible_rows).unwrap_or(i64::MAX));
        let first = range.start.row.max(window_top);
        let last = range.end.row.min(window_bottom.saturating_sub(1));
        if first > last {
            return Vec::new();
        }
        (first..=last)
            .filter_map(|absolute_row| {
                let (start_col, end_col) = self.columns(range, absolute_row, cols);
                (start_col < end_col).then_some(SelectionSpan {
                    row: usize::try_from(absolute_row - window_top).ok()?,
                    start_col,
                    end_col_exclusive: end_col,
                })
            })
            .collect()
    }

    /// Extracts selected cells, trimming terminal padding at each row end and
    /// joining logical rows with newlines for Cmd-C.
    #[must_use]
    pub fn selected_text(&self, viewport: &ScrollbackViewport, buffer: &GridBuffer) -> String {
        let Some(range) = self.range() else {
            return String::new();
        };
        let cols = usize::from(buffer.cols);
        let mut text = String::new();
        for absolute_row in range.start.row..=range.end.row {
            let (start_col, end_col) = self.columns(range, absolute_row, cols);
            let row = viewport.row_at_absolute(buffer, absolute_row);
            let metadata = viewport.row_metadata(buffer, absolute_row);
            let mut line = String::new();
            for (col, cell) in row.iter().enumerate().take(end_col).skip(start_col) {
                if cell.scalar == 0 || cell.style.contains(TermStyle::WIDE_SPACER) {
                    continue;
                }
                line.push(cell_char(*cell));
                if let Some((_, extra)) = metadata
                    .graphemes
                    .iter()
                    .find(|(x, _)| usize::from(*x) == col)
                {
                    line.push_str(extra);
                }
            }
            // A soft wrap joins this row to the next only when the selection
            // carries on there; the selection's own last row is trimmed.
            let wraps = !self.rectangle
                && absolute_row < range.end.row
                && end_col == cols
                && row
                    .last()
                    .is_some_and(|cell| cell.style.contains(TermStyle::SOFT_WRAP));
            text.push_str(if wraps {
                &line
            } else {
                line.trim_end_matches(' ')
            });
            if absolute_row < range.end.row && !wraps {
                text.push('\n');
            }
        }
        text
    }
}

fn columns_for_row(range: SelectionRange, row: i64, cols: usize) -> (usize, usize) {
    let start = if row == range.start.row {
        range.start.col
    } else {
        0
    };
    let end = if row == range.end.row {
        range.end.col
    } else {
        cols
    };
    (start.min(cols), end.min(cols))
}

/// Rows a double or triple click may follow through soft wraps in each
/// direction. Ample for any URL or command; bounds a minified file's line.
const MAX_WRAPPED_ROWS: i64 = 1_000;

/// The rows one program line occupies after the terminal soft-wrapped it,
/// as one run of cells. Rows the view has not fetched end the walk.
struct LogicalLine {
    cells: Vec<GridCell>,
    first_row: i64,
    cols: usize,
}

impl LogicalLine {
    fn around(viewport: &ScrollbackViewport, buffer: &GridBuffer, absolute_row: i64) -> Self {
        let cols = usize::from(buffer.cols);
        let wraps = |row: &[GridCell]| {
            row.len() >= cols && row[cols - 1].style.contains(TermStyle::SOFT_WRAP)
        };
        let mut first_row = absolute_row;
        while absolute_row - first_row < MAX_WRAPPED_ROWS
            && viewport
                .stored_row(buffer, first_row - 1)
                .is_some_and(wraps)
        {
            first_row -= 1;
        }
        let mut cells = Vec::with_capacity(cols);
        let mut row = first_row;
        loop {
            let cells_of_row = viewport.row_at_absolute(buffer, row);
            let continues = wraps(&cells_of_row)
                && row - absolute_row < MAX_WRAPPED_ROWS
                && viewport.stored_row(buffer, row + 1).is_some();
            cells.extend(cells_of_row);
            if !continues {
                break;
            }
            row += 1;
        }
        Self {
            cells,
            first_row,
            cols,
        }
    }

    fn index(&self, row: i64, col: usize) -> Option<usize> {
        let offset = usize::try_from(row - self.first_row).ok()?;
        let index = offset * self.cols + col;
        (index < self.cells.len()).then_some(index)
    }

    fn point(&self, index: usize) -> SelectionPoint {
        SelectionPoint {
            row: self.first_row + (index / self.cols) as i64,
            col: index % self.cols,
        }
    }

    fn start_point(&self, index: usize) -> SelectionPoint {
        self.point(index)
    }

    /// An exclusive end stays on the row of the last selected cell, so a word
    /// ending at a wrap does not reach onto the next row.
    fn end_point(&self, index: usize) -> SelectionPoint {
        if index == 0 {
            return self.point(0);
        }
        let last = self.point(index - 1);
        SelectionPoint {
            row: last.row,
            col: last.col + 1,
        }
    }
}

fn is_spacer(cell: GridCell) -> bool {
    cell.style.contains(TermStyle::WIDE_SPACER)
}

/// Each cell's class, a double-width glyph's spacer taking its glyph's.
fn word_classes(cells: &[GridCell]) -> Vec<WordClass> {
    let mut classes = Vec::with_capacity(cells.len());
    for (index, cell) in cells.iter().enumerate() {
        let class = if index > 0 && is_spacer(*cell) {
            classes[index - 1]
        } else {
            word_class(*cell)
        };
        classes.push(class);
    }
    classes
}

fn word_class(cell: GridCell) -> WordClass {
    let ch = cell_char(cell);
    if ch.is_alphanumeric() || is_terminal_word_punctuation(ch) {
        WordClass::Word
    } else if ch.is_whitespace() {
        WordClass::Whitespace
    } else {
        WordClass::Punctuation
    }
}

/// Punctuation people expect to travel with paths, URLs, flags, and common
/// identifiers when double-clicking in a terminal. Shell separators and quote
/// delimiters deliberately remain punctuation boundaries.
fn is_terminal_word_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '_' | '-' | '.' | '/' | '~' | ':' | '@' | '%' | '+' | '=' | '?'
    )
}

/// Returns the exact URL/reference span containing `col`, if the surrounding
/// whitespace-delimited run begins with a scheme people commonly encounter in
/// terminal output. `&` and `#` are shell boundaries everywhere else, but are
/// meaningful query/fragment separators inside these recognized references.
fn url_span_at(row: &[GridCell], col: usize) -> Option<(usize, usize)> {
    let blank = |cell: GridCell| !is_spacer(cell) && cell_char(cell).is_whitespace();
    let mut run_start = col;
    while run_start > 0 && !blank(row[run_start - 1]) {
        run_start -= 1;
    }
    let mut run_end = col + 1;
    while run_end < row.len() && !blank(row[run_end]) {
        run_end += 1;
    }

    let mut start = run_start;
    while start < run_end && matches!(cell_char(row[start]), '(' | '[' | '{' | '<' | '\'' | '"') {
        start += 1;
    }
    let mut end = run_end;
    while end > start
        && matches!(
            cell_char(row[end - 1]),
            '.' | ',' | ';' | ':' | ')' | ']' | '}' | '\'' | '"' | '>' | '!' | '?'
        )
    {
        end -= 1;
    }
    if col < start || col >= end {
        return None;
    }

    let candidate: String = row[start..end]
        .iter()
        .copied()
        .filter(|cell| !is_spacer(*cell))
        .map(cell_char)
        .collect();
    let recognized = candidate.starts_with("http://")
        || candidate.starts_with("https://")
        || candidate.starts_with("file://")
        || candidate
            .strip_prefix("www.")
            .is_some_and(|rest| rest.contains('.'));
    recognized.then_some((start, end))
}

/// The text a cell stands for. The parser marks the cell where a tab began
/// with `\t`, but the tab moved the cursor over blank cells that copy as
/// spaces, as in xterm and Ghostty; keeping the `\t` as well would paste a
/// tab followed by the spaces it already stood for.
pub(crate) fn cell_char(cell: GridCell) -> char {
    if cell.scalar == 0 {
        return ' ';
    }
    char::from_u32(cell.scalar)
        .filter(|ch| !matches!(ch, '\n' | '\r' | '\t'))
        .unwrap_or(' ')
}

#[cfg(test)]
mod tests {
    use diri_proto::grid::{TermColor, TermStyle};

    use super::*;

    fn cell(ch: char) -> GridCell {
        GridCell::new(
            u32::from(ch),
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    }

    fn row(text: &str, cols: usize) -> Vec<GridCell> {
        let mut cells: Vec<_> = text.chars().map(cell).collect();
        cells.resize(cols, GridCell::BLANK);
        cells
    }

    #[test]
    fn extracts_selection_crossing_history_live_seam() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![row("history ", 8)], 9, 10, 10, 1, 3);
        viewport.set_view_offset(1, 3);
        let mut buffer = GridBuffer::new(8, 3);
        buffer.cells = [row("live one", 8), row("live two", 8), row("prompt  ", 8)].concat();
        let mut selection = TerminalSelection::default();
        selection.set_from_window(&viewport, 2, 0, 4, 2);

        assert_eq!(
            selection.selected_text(&viewport, &buffer),
            "story\nlive one\nlive"
        );
        assert_eq!(
            selection.visible_spans(&viewport, 3, 8),
            vec![
                SelectionSpan {
                    row: 0,
                    start_col: 2,
                    end_col_exclusive: 8,
                },
                SelectionSpan {
                    row: 1,
                    start_col: 0,
                    end_col_exclusive: 8,
                },
                SelectionSpan {
                    row: 2,
                    start_col: 0,
                    end_col_exclusive: 4,
                },
            ]
        );
    }

    #[test]
    fn reverse_drag_normalizes_in_reading_order() {
        let viewport = ScrollbackViewport::default();
        let buffer = GridBuffer::new(6, 2);
        let mut selection = TerminalSelection::default();
        selection.set_from_window(&viewport, 4, 1, 1, 0);
        assert_eq!(
            selection.range(),
            Some(SelectionRange {
                start: SelectionPoint { row: 0, col: 1 },
                end: SelectionPoint { row: 1, col: 4 },
            })
        );
        assert_eq!(selection.selected_text(&viewport, &buffer), "\n");
    }

    #[test]
    fn double_click_selects_word() {
        let viewport = ScrollbackViewport::default();
        let mut buffer = GridBuffer::new(14, 1);
        buffer.cells = row("one two_three!", 14);
        let mut selection = TerminalSelection::default();
        selection.select_word(&viewport, &buffer, 0, 6);

        assert_eq!(selection.selected_text(&viewport, &buffer), "two_three");
    }

    #[test]
    fn double_click_keeps_paths_urls_and_flags_together() {
        let viewport = ScrollbackViewport::default();
        let text = "open ./crates/diri-app:42 https://diri.dev/a?q=1&v=2#install, --all | next";
        let mut buffer = GridBuffer::new(text.len() as u16, 1);
        buffer.cells = row(text, text.len());
        let mut selection = TerminalSelection::default();

        selection.select_word(&viewport, &buffer, 0, text.find("diri-app").expect("path"));
        assert_eq!(
            selection.selected_text(&viewport, &buffer),
            "./crates/diri-app:42"
        );

        selection.select_word(&viewport, &buffer, 0, text.find("diri.dev").expect("url"));
        assert_eq!(
            selection.selected_text(&viewport, &buffer),
            "https://diri.dev/a?q=1&v=2#install"
        );

        selection.select_word(&viewport, &buffer, 0, text.find("--all").expect("flag"));
        assert_eq!(selection.selected_text(&viewport, &buffer), "--all");

        selection.select_word(&viewport, &buffer, 0, text.find('|').expect("pipe"));
        assert_eq!(selection.selected_text(&viewport, &buffer), "|");
    }

    #[test]
    fn double_click_keeps_shell_operators_and_comments_as_boundaries() {
        let viewport = ScrollbackViewport::default();
        let text = "true&&false value#comment";
        let mut buffer = GridBuffer::new(text.len() as u16, 1);
        buffer.cells = row(text, text.len());
        let mut selection = TerminalSelection::default();

        selection.select_word(&viewport, &buffer, 0, text.find("true").expect("true"));
        assert_eq!(selection.selected_text(&viewport, &buffer), "true");
        selection.select_word(&viewport, &buffer, 0, text.find("false").expect("false"));
        assert_eq!(selection.selected_text(&viewport, &buffer), "false");
        selection.select_word(&viewport, &buffer, 0, text.find("value").expect("value"));
        assert_eq!(selection.selected_text(&viewport, &buffer), "value");
        selection.select_word(
            &viewport,
            &buffer,
            0,
            text.find("comment").expect("comment"),
        );
        assert_eq!(selection.selected_text(&viewport, &buffer), "comment");
    }

    #[test]
    fn triple_click_selects_the_visual_line_without_terminal_padding() {
        let viewport = ScrollbackViewport::default();
        let mut buffer = GridBuffer::new(16, 2);
        buffer.cells = [row("first", 16), row("whole line", 16)].concat();
        let mut selection = TerminalSelection::default();

        selection.select_line(&viewport, &buffer, 1);

        assert_eq!(selection.selected_text(&viewport, &buffer), "whole line");
        assert_eq!(
            selection.range(),
            Some(SelectionRange {
                start: SelectionPoint { row: 1, col: 0 },
                end: SelectionPoint { row: 1, col: 16 },
            })
        );
    }

    #[test]
    fn row_overlap_respects_the_exclusive_endpoint() {
        let viewport = ScrollbackViewport::default();
        let mut selection = TerminalSelection::default();
        selection.set_from_window(&viewport, 2, 0, 0, 2);

        assert!(selection.overlaps_row(0, 8));
        assert!(selection.overlaps_row(1, 8));
        assert!(!selection.overlaps_row(2, 8));
    }
}

#[cfg(test)]
mod qol_tests {
    use super::*;
    fn buffer(lines: &[&str], cols: u16) -> GridBuffer {
        let mut buffer = GridBuffer::new(cols, lines.len() as u16);
        for (y, line) in lines.iter().enumerate() {
            for (x, ch) in line.chars().enumerate().take(cols as usize) {
                buffer.cells[y * cols as usize + x].scalar = ch as u32;
            }
        }
        buffer
    }
    #[test]
    fn double_click_drag_keeps_whole_words_in_both_directions() {
        let grid = buffer(&["one two three"], 16);
        let view = ScrollbackViewport::default();
        let mut selection = TerminalSelection::default();
        selection.select_word(&view, &grid, 0, 5);
        selection.drag_in_view(&view, &grid, 0, 10);
        assert_eq!(selection.selected_text(&view, &grid), "two three");
        selection.drag_in_view(&view, &grid, 0, 1);
        assert_eq!(selection.selected_text(&view, &grid), "one two");
    }
    #[test]
    fn triple_click_drag_keeps_whole_lines() {
        let grid = buffer(&["one", "two", "three"], 8);
        let view = ScrollbackViewport::default();
        let mut selection = TerminalSelection::default();
        selection.select_line(&view, &grid, 1);
        selection.drag_in_view(&view, &grid, 2, 2);
        assert_eq!(selection.selected_text(&view, &grid), "two\nthree");
        selection.drag_in_view(&view, &grid, 0, 2);
        assert_eq!(selection.selected_text(&view, &grid), "one\ntwo");
    }
    #[test]
    fn rectangle_copies_only_requested_columns_in_reverse_drag() {
        let grid = buffer(&["a 11 x", "b 22 y", "c 33 z"], 8);
        let view = ScrollbackViewport::default();
        let mut selection = TerminalSelection::default();
        // Corners are cell boundaries: columns 2 and 3 lie between 2 and 4.
        selection.begin_rectangle(SelectionPoint { row: 2, col: 4 });
        selection.drag_to(SelectionPoint { row: 0, col: 2 });
        assert_eq!(selection.selected_text(&view, &grid), "11\n22\n33");
    }
    #[test]
    fn copying_unwraps_only_soft_lines_and_preserves_graphemes() {
        let mut grid = buffer(&["abcdef", "ghi", "界 e"], 6);
        let view = ScrollbackViewport::default();
        let mut selection = TerminalSelection::default();
        grid.cells[5].style |= TermStyle::SOFT_WRAP;
        grid.cells[13].scalar = 0;
        grid.cells[13].style |= TermStyle::WIDE_SPACER;
        grid.annotations[2].graphemes.push((2, "\u{301}".into()));
        selection.set_from_window(&view, 0, 0, 3, 2);
        assert_eq!(
            selection.selected_text(&view, &grid),
            "abcdefghi\n界e\u{301}"
        );
    }
}
