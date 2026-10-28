//! Plain-text view of a terminal's visible rows for assistive technology.
//!
//! The platform bridge asks for this only while an accessibility client is
//! reading the terminal, so building it never runs on an ordinary frame.
//! Every index is in UTF-16 code units, the unit `NSRange` and the macOS
//! accessibility API count in.

use std::ops::Range;

use diri_proto::grid::{GridCell, RowMetadata};

use crate::buffer::GridBuffer;

/// One visible row: where its text starts in the whole value and which grid
/// columns each UTF-16 unit of it came from.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Line {
    start: usize,
    /// Units of this row's text, excluding the line break after it.
    len: usize,
    /// Source cell span for every UTF-16 unit, `[first, end)` columns.
    cells: Vec<[u16; 2]>,
}

/// The visible terminal as one string plus the maps an accessibility client
/// needs: line for index, range for line, cell rectangle for a range, and
/// index for a cell.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessibilityText {
    text: String,
    lines: Vec<Line>,
    utf16_len: usize,
    cursor: Option<usize>,
    selection: Option<Range<usize>>,
}

/// Grid cells a range covers on one row: `row`, columns `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellSpan {
    pub row: usize,
    pub start_col: u16,
    pub end_col: u16,
}

/// A selection or caret in visible-row coordinates. `end` is exclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisiblePoint {
    pub row: usize,
    pub col: usize,
}

impl AccessibilityText {
    /// Builds the text from rows that are already composed for the window
    /// (live grid or a reading view into history). Trailing blanks are
    /// dropped, except on the cursor row up to the cursor, so the caret's
    /// index lands where it is drawn.
    #[must_use]
    pub fn from_rows<'a>(
        rows: impl IntoIterator<Item = (&'a [GridCell], Option<&'a RowMetadata>)>,
        cursor: Option<VisiblePoint>,
        selection: Option<(VisiblePoint, VisiblePoint)>,
    ) -> Self {
        let mut text = String::new();
        let mut lines = Vec::new();
        let mut offset = 0usize;
        for (row, (cells, metadata)) in rows.into_iter().enumerate() {
            if row > 0 {
                text.push('\n');
                offset += 1;
            }
            let (row_text, ranges) = GridBuffer::text_with_cell_ranges(cells, metadata);
            let keep_through = cursor
                .filter(|cursor| cursor.row == row)
                .map_or(0, |cursor| cursor.col);
            let mut chars: Vec<(char, [u16; 2])> = row_text.chars().zip(ranges).collect();
            while chars.last().is_some_and(|(ch, range)| {
                ch.is_whitespace() && usize::from(range[0]) >= keep_through
            }) {
                chars.pop();
            }
            let mut units = Vec::with_capacity(chars.len());
            for (ch, range) in chars {
                text.push(ch);
                for _ in 0..ch.len_utf16() {
                    units.push(range);
                }
            }
            let len = units.len();
            lines.push(Line {
                start: offset,
                len,
                cells: units,
            });
            offset += len;
        }
        let mut model = Self {
            text,
            lines,
            utf16_len: offset,
            cursor: None,
            selection: None,
        };
        model.cursor = cursor.map(|point| model.index_for_cell(point.row, point.col));
        model.selection = selection
            .map(|(start, end)| {
                let a = model.index_for_cell(start.row, start.col);
                let b = model.index_for_cell(end.row, end.col);
                a.min(b)..a.max(b)
            })
            .filter(|range| !range.is_empty());
        model
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Length in UTF-16 units.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.utf16_len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.utf16_len == 0
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len().max(1)
    }

    /// The selection if there is one, otherwise the caret as an empty range.
    #[must_use]
    pub fn selected_range(&self) -> Range<usize> {
        self.selection.clone().unwrap_or_else(|| {
            let caret = self.cursor.unwrap_or(0);
            caret..caret
        })
    }

    #[must_use]
    pub fn selected_text(&self) -> String {
        self.selection
            .clone()
            .map(|range| self.string_for_range(range))
            .unwrap_or_default()
    }

    /// The row of the caret, or of the start of the selection.
    #[must_use]
    pub fn insertion_line(&self) -> usize {
        self.line_for_index(self.selected_range().start)
    }

    /// The row holding `index`. A line break belongs to the row it ends;
    /// indices past the end clamp to the last row.
    #[must_use]
    pub fn line_for_index(&self, index: usize) -> usize {
        match self.lines.binary_search_by(|line| line.start.cmp(&index)) {
            Ok(row) => row,
            Err(0) => 0,
            Err(next) => next - 1,
        }
    }

    /// The row's text including its trailing line break, the way text views
    /// report lines.
    #[must_use]
    pub fn range_for_line(&self, row: usize) -> Range<usize> {
        let Some(line) = self.lines.get(row) else {
            return self.utf16_len..self.utf16_len;
        };
        let end = line.start + line.len + usize::from(row + 1 < self.lines.len());
        line.start..end
    }

    /// The composed character at `index`: one unit, or both halves of a
    /// surrogate pair.
    #[must_use]
    pub fn range_for_index(&self, index: usize) -> Range<usize> {
        if index >= self.utf16_len {
            return self.utf16_len..self.utf16_len;
        }
        let mut units = 0;
        for ch in self.text.chars() {
            let width = ch.len_utf16();
            if index < units + width {
                return units..units + width;
            }
            units += width;
        }
        self.utf16_len..self.utf16_len
    }

    /// Text in `range`, clamped to the value. Half a surrogate pair at
    /// either end widens to the whole character.
    #[must_use]
    pub fn string_for_range(&self, range: Range<usize>) -> String {
        let start = range.start.min(self.utf16_len);
        let end = range.end.clamp(start, self.utf16_len);
        let mut units = 0;
        let mut out = String::new();
        for ch in self.text.chars() {
            let width = ch.len_utf16();
            if units + width > start && units < end {
                out.push(ch);
            }
            units += width;
            if units >= end {
                break;
            }
        }
        out
    }

    /// Cells covered by `range`, one span per row it touches. An empty range
    /// gives the one cell the caret would sit in.
    #[must_use]
    pub fn cell_spans(&self, range: Range<usize>) -> Vec<CellSpan> {
        let start = range.start.min(self.utf16_len);
        let end = range.end.clamp(start, self.utf16_len);
        if start == end {
            let row = self.line_for_index(start);
            let col = self.col_for_index(row, start);
            return vec![CellSpan {
                row,
                start_col: col,
                end_col: col.saturating_add(1),
            }];
        }
        let mut spans = Vec::new();
        for row in self.line_for_index(start)..=self.line_for_index(end - 1) {
            let Some(line) = self.lines.get(row) else {
                break;
            };
            let from = start.max(line.start) - line.start;
            let to = end.min(line.start + line.len).saturating_sub(line.start);
            if from >= to {
                continue;
            }
            spans.push(CellSpan {
                row,
                start_col: line.cells[from][0],
                end_col: line.cells[to - 1][1],
            });
        }
        spans
    }

    /// The index of the character drawn in the cell at `row`, `col`. A cell
    /// right of the row's text maps to the end of the row.
    #[must_use]
    pub fn index_for_cell(&self, row: usize, col: usize) -> usize {
        let Some(line) = self.lines.get(row) else {
            return self.utf16_len;
        };
        let unit = line
            .cells
            .iter()
            .position(|range| col < usize::from(range[1]))
            .unwrap_or(line.len);
        line.start + unit
    }

    fn col_for_index(&self, row: usize, index: usize) -> u16 {
        let Some(line) = self.lines.get(row) else {
            return 0;
        };
        let unit = index.saturating_sub(line.start);
        line.cells.get(unit).map_or_else(
            || line.cells.last().map_or(0, |range| range[1]),
            |range| range[0],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::grid::TermStyle;

    fn row(text: &str, cols: usize) -> Vec<GridCell> {
        let mut cells = vec![GridCell::BLANK; cols];
        let mut col = 0;
        for ch in text.chars() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            cells[col] = GridCell {
                scalar: u32::from(ch),
                ..GridCell::BLANK
            };
            if width == 2 {
                cells[col + 1] = GridCell {
                    scalar: 0,
                    style: TermStyle::WIDE_SPACER,
                    ..GridCell::BLANK
                };
            }
            col += width;
        }
        cells
    }

    fn model(rows: &[&str], cursor: Option<(usize, usize)>) -> AccessibilityText {
        let rows: Vec<Vec<GridCell>> = rows.iter().map(|text| row(text, 20)).collect();
        AccessibilityText::from_rows(
            rows.iter().map(|cells| (cells.as_slice(), None)),
            cursor.map(|(row, col)| VisiblePoint { row, col }),
            None,
        )
    }

    #[test]
    fn rows_join_with_line_breaks_and_drop_trailing_blanks() {
        let text = model(&["$ ls", "Cargo.toml  src", ""], None);
        assert_eq!(text.text(), "$ ls\nCargo.toml  src\n");
        assert_eq!(text.len(), 21);
        assert_eq!(text.line_count(), 3);
    }

    #[test]
    fn caret_keeps_the_blank_it_sits_after() {
        // A prompt `$ ` with the caret after the space: the space stays so
        // the caret index is where the caret is drawn.
        let text = model(&["$ "], Some((0, 2)));
        assert_eq!(text.text(), "$ ");
        assert_eq!(text.selected_range(), 2..2);
        assert_eq!(text.insertion_line(), 0);
    }

    #[test]
    fn lines_and_indices_round_trip() {
        let text = model(&["one", "two", "three"], Some((2, 1)));
        assert_eq!(text.line_for_index(0), 0);
        assert_eq!(text.line_for_index(3), 0, "the break belongs to its row");
        assert_eq!(text.line_for_index(4), 1);
        assert_eq!(text.line_for_index(8), 2);
        assert_eq!(text.line_for_index(999), 2);
        assert_eq!(text.range_for_line(0), 0..4);
        assert_eq!(text.range_for_line(2), 8..13);
        assert_eq!(text.range_for_line(9), 13..13);
        assert_eq!(text.string_for_range(text.range_for_line(1)), "two\n");
        assert_eq!(text.selected_range(), 9..9);
        assert_eq!(text.insertion_line(), 2);
    }

    #[test]
    fn wide_glyphs_map_to_both_cells_and_astral_glyphs_to_two_units() {
        let text = model(&["a界b😀c"], None);
        assert_eq!(text.text(), "a界b😀c");
        // 😀 is one char, two UTF-16 units.
        assert_eq!(text.len(), 6);
        assert_eq!(text.range_for_index(3), 3..5);
        assert_eq!(text.range_for_index(4), 3..5);
        assert_eq!(text.string_for_range(4..5), "😀");
        // 界 occupies columns 1-2; clicking either finds it.
        assert_eq!(text.index_for_cell(0, 1), 1);
        assert_eq!(text.index_for_cell(0, 2), 1);
        assert_eq!(text.index_for_cell(0, 3), 2);
        assert_eq!(
            text.cell_spans(1..2),
            vec![CellSpan {
                row: 0,
                start_col: 1,
                end_col: 3
            }]
        );
        assert_eq!(
            text.index_for_cell(0, 19),
            6,
            "right of the text is its end"
        );
    }

    #[test]
    fn spans_cover_each_row_a_range_touches() {
        let text = model(&["hello", "world"], None);
        assert_eq!(
            text.cell_spans(3..8),
            vec![
                CellSpan {
                    row: 0,
                    start_col: 3,
                    end_col: 5
                },
                CellSpan {
                    row: 1,
                    start_col: 0,
                    end_col: 2
                },
            ]
        );
        assert_eq!(
            text.cell_spans(11..11),
            vec![CellSpan {
                row: 1,
                start_col: 5,
                end_col: 6
            }],
            "an empty range at the end is the cell after the text"
        );
    }

    #[test]
    fn selection_maps_to_a_text_range() {
        let rows: Vec<Vec<GridCell>> = ["alpha", "beta"].iter().map(|t| row(t, 10)).collect();
        let text = AccessibilityText::from_rows(
            rows.iter().map(|cells| (cells.as_slice(), None)),
            Some(VisiblePoint { row: 1, col: 4 }),
            Some((
                VisiblePoint { row: 1, col: 2 },
                VisiblePoint { row: 0, col: 1 },
            )),
        );
        assert_eq!(text.selected_range(), 1..8);
        assert_eq!(text.selected_text(), "lpha\nbe");
        assert_eq!(text.insertion_line(), 0);
    }

    #[test]
    fn element_handle_reads_the_live_grid_and_lets_go() {
        use crate::buffer::CursorState;
        use crate::element::TerminalElement;
        use crate::selection::SelectionPoint;

        let mut buffer = GridBuffer::new(10, 2);
        buffer.cells[..10].copy_from_slice(&row("hello", 10));
        buffer.cursor = CursorState {
            col: 5,
            row: 0,
            visible: true,
            ..CursorState::default()
        };
        let element = TerminalElement::with_buffer(buffer);
        let handle = element.accessibility();
        let text = handle.snapshot().expect("view alive");
        assert_eq!(text.text(), "hello\n");
        assert_eq!(text.selected_range(), 5..5);

        let before = handle.revision();
        element.select_range(
            SelectionPoint { row: 0, col: 1 },
            SelectionPoint { row: 0, col: 4 },
        );
        assert_ne!(handle.revision(), before, "a selection is a new revision");
        assert_eq!(handle.snapshot().expect("alive").selected_text(), "ell");

        drop(element);
        assert!(
            handle.snapshot().is_none(),
            "the handle keeps nothing alive"
        );
        assert!(handle.revision().is_none());
    }
}
