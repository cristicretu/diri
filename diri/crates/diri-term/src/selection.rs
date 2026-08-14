//! Scroll-invariant terminal selection over a composed history/live window.

use diri_proto::grid::GridCell;

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

#[derive(Clone, Debug, Default)]
pub struct TerminalSelection {
    anchor: Option<SelectionPoint>,
    head: Option<SelectionPoint>,
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
        self.anchor = None;
        self.head = None;
    }

    pub fn begin(&mut self, point: SelectionPoint) {
        self.anchor = Some(point);
        self.head = Some(point);
    }

    pub fn drag_to(&mut self, point: SelectionPoint) {
        if self.anchor.is_some() {
            self.head = Some(point);
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
    /// cells. The resulting endpoint is exclusive.
    pub fn select_word(
        &mut self,
        viewport: &ScrollbackViewport,
        buffer: &GridBuffer,
        window_row: usize,
        col: usize,
    ) {
        let row = viewport.window_row(buffer, window_row);
        if row.is_empty() {
            self.clear();
            return;
        }
        let col = col.min(row.len() - 1);
        let class = word_class(row[col]);
        let mut start = col;
        while start > 0 && word_class(row[start - 1]) == class {
            start -= 1;
        }
        let mut end = col + 1;
        while end < row.len() && word_class(row[end]) == class {
            end += 1;
        }
        let absolute_row = viewport.absolute_row(window_row);
        self.anchor = Some(SelectionPoint {
            row: absolute_row,
            col: start,
        });
        self.head = Some(SelectionPoint {
            row: absolute_row,
            col: end,
        });
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
        let (start, end) = columns_for_row(range, absolute_row, cols);
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
                let (start_col, end_col) = columns_for_row(range, absolute_row, cols);
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
        let mut lines = Vec::new();
        for absolute_row in range.start.row..=range.end.row {
            let (start_col, end_col) = columns_for_row(range, absolute_row, cols);
            if start_col >= end_col {
                lines.push(String::new());
                continue;
            }
            let row = viewport.row_at_absolute(buffer, absolute_row);
            let mut line = String::new();
            for cell in &row[start_col..end_col] {
                line.push(cell_char(*cell));
            }
            lines.push(line.trim_end_matches(' ').to_owned());
        }
        lines.join("\n")
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

fn word_class(cell: GridCell) -> WordClass {
    let ch = cell_char(cell);
    if ch.is_alphanumeric() || ch == '_' {
        WordClass::Word
    } else if ch.is_whitespace() {
        WordClass::Whitespace
    } else {
        WordClass::Punctuation
    }
}

pub(crate) fn cell_char(cell: GridCell) -> char {
    if cell.scalar == 0 {
        return ' ';
    }
    char::from_u32(cell.scalar)
        .filter(|ch| *ch != '\n' && *ch != '\r')
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
    fn row_overlap_respects_the_exclusive_endpoint() {
        let viewport = ScrollbackViewport::default();
        let mut selection = TerminalSelection::default();
        selection.set_from_window(&viewport, 2, 0, 0, 2);

        assert!(selection.overlaps_row(0, 8));
        assert!(selection.overlaps_row(1, 8));
        assert!(!selection.overlaps_row(2, 8));
    }
}
