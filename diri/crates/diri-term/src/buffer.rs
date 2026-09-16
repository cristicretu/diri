use diri_proto::grid::{GridCell, GridUpdate, RowMetadata};
use unicode_width::UnicodeWidthChar;

/// Cursor state carried by every daemon grid update.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorState {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
}

/// A compact summary of damage caused by applying a grid update.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ApplySummary {
    pub changed: bool,
    pub size_changed: bool,
    pub cursor_changed: bool,
    pub dirty_row_count: usize,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedRenderRow {
    pub row: usize,
    pub generation: u64,
    pub cells: Vec<GridCell>,
    pub graphemes: Vec<(u16, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderDamageSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorState,
    pub changed_rows: Vec<ChangedRenderRow>,
}

/// Row-major terminal cells plus damage and cursor bookkeeping.
#[derive(Clone, Debug, Default)]
pub struct GridBuffer {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<GridCell>,
    pub annotations: Vec<RowMetadata>,
    pub cursor: CursorState,
    generation: u64,
    row_generations: Vec<u64>,
    dirty_rows: Vec<bool>,
}

impl GridBuffer {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let row_count = usize::from(rows);
        Self {
            cols,
            rows,
            cells: vec![GridCell::BLANK; usize::from(cols) * row_count],
            annotations: vec![RowMetadata::default(); row_count],
            cursor: CursorState::default(),
            generation: 0,
            row_generations: vec![0; row_count],
            dirty_rows: vec![true; row_count],
        }
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// True when no cell carries a printable glyph. Distinguishes a screen the
    /// daemon still holds (an exited agent's last frame, worth showing) from
    /// one that was never painted.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.cells
            .iter()
            .all(|cell| cell.scalar == 0 || cell.scalar == u32::from(' '))
    }

    #[must_use]
    pub fn row(&self, row: usize) -> Option<&[GridCell]> {
        if row >= usize::from(self.rows) {
            return None;
        }
        let cols = usize::from(self.cols);
        let start = row * cols;
        Some(&self.cells[start..start + cols])
    }

    /// Plain text plus the source cell column for every emitted character.
    /// Zero-scalar wide-glyph continuation cells carry no character and are
    /// skipped, preserving exact find-highlight columns.
    #[must_use]
    pub fn row_text_with_columns(&self, row: usize) -> Option<(String, Vec<usize>)> {
        self.row_text_with_cell_ranges(row).map(|(text, ranges)| {
            (
                text,
                ranges
                    .into_iter()
                    .map(|range| usize::from(range[0]))
                    .collect(),
            )
        })
    }

    /// Each scalar maps to its complete source cell span. Combining marks share
    /// their base's span, so searching either part still highlights that cell.
    #[must_use]
    pub fn row_text_with_cell_ranges(&self, row: usize) -> Option<(String, Vec<[u16; 2]>)> {
        Some(Self::text_with_cell_ranges(
            self.row(row)?,
            self.annotations.get(row),
        ))
    }

    pub(crate) fn text_with_cell_ranges(
        cells: &[GridCell],
        metadata: Option<&RowMetadata>,
    ) -> (String, Vec<[u16; 2]>) {
        let mut text = String::with_capacity(cells.len());
        let mut columns = Vec::with_capacity(cells.len());
        let mut graphemes = metadata
            .into_iter()
            .flat_map(|metadata| &metadata.graphemes)
            .peekable();
        for (column, cell) in cells.iter().enumerate() {
            if cell.scalar == 0 {
                continue;
            }
            let ch = char::from_u32(cell.scalar)
                .filter(|ch| *ch != '\n' && *ch != '\r')
                .unwrap_or(' ');
            // Match the shared parser's width rules, including older Helpers
            // whose wire cells do not distinguish wide bases from other glyphs.
            let width = ch.width().unwrap_or(1).max(1);
            let range = [column as u16, (column + width).min(cells.len()) as u16];
            text.push(ch);
            columns.push(range);
            while graphemes
                .peek()
                .is_some_and(|(col, _)| usize::from(*col) < column)
            {
                graphemes.next();
            }
            if let Some((_, combining)) = graphemes.next_if(|(col, _)| usize::from(*col) == column)
            {
                for ch in combining.chars() {
                    text.push(ch);
                    columns.push(range);
                }
            }
        }
        (text, columns)
    }

    #[must_use]
    pub fn row_generation(&self, row: usize) -> Option<u64> {
        self.row_generations.get(row).copied()
    }

    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.dirty_rows
            .iter()
            .enumerate()
            .filter_map(|(row, dirty)| dirty.then_some(row))
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_rows.fill(false);
    }

    /// Copies only rows whose generation changed since `known_generations`.
    /// The renderer retains prepared rows for everything else, so a cursor or
    /// prompt update no longer clones the entire screen on every GPUI frame.
    #[must_use]
    pub fn snapshot_damage(
        &self,
        known_generations: &mut Vec<u64>,
        visible_rows: usize,
        visible_cols: usize,
        force: bool,
    ) -> RenderDamageSnapshot {
        let row_count = visible_rows.min(usize::from(self.rows));
        let col_count = visible_cols.min(usize::from(self.cols));
        known_generations.resize(row_count, u64::MAX);
        let mut changed_rows = Vec::new();
        for (row, known) in known_generations.iter_mut().enumerate() {
            let generation = self.row_generations.get(row).copied().unwrap_or_default();
            if force || *known != generation {
                let cells = self
                    .row(row)
                    .map_or_else(Vec::new, |cells| cells[..col_count].to_vec());
                changed_rows.push(ChangedRenderRow {
                    row,
                    generation,
                    cells,
                    graphemes: self.annotations.get(row).map_or_else(Vec::new, |metadata| {
                        metadata
                            .graphemes
                            .iter()
                            .take_while(|(col, _)| usize::from(*col) < col_count)
                            .cloned()
                            .collect()
                    }),
                });
                *known = generation;
            }
        }
        RenderDamageSnapshot {
            cols: self.cols,
            rows: self.rows,
            cursor: self.cursor,
            changed_rows,
        }
    }

    /// Apply a full snapshot or patch rows from a diff.
    ///
    /// Full snapshots always replace storage. A geometry-changing diff is
    /// treated as a re-seed as a defensive measure, matching the protocol
    /// helper's blank-fill behavior. Short rows are padded and long rows are
    /// truncated to the daemon-provided column count.
    pub fn apply(&mut self, update: GridUpdate) -> ApplySummary {
        let new_cols = usize::from(update.cols);
        let new_rows = usize::from(update.rows);
        let size_changed = self.cols != update.cols || self.rows != update.rows;
        let replace = update.is_full_snapshot || size_changed;

        if replace {
            self.cols = update.cols;
            self.rows = update.rows;
            // Reuse the allocation: a live resize re-seeds this buffer on every
            // step of the drag, and dropping the old Vec each time hands the
            // allocator a screen's worth of churn for nothing.
            self.annotations = vec![RowMetadata::default(); new_rows];
            self.cells.clear();
            self.cells.resize(new_cols * new_rows, GridCell::BLANK);
            self.row_generations.resize(new_rows, 0);
            self.dirty_rows.clear();
            self.dirty_rows.resize(new_rows, true);
        } else {
            // Damage belongs to one update/frame. Leaving old bits set caused
            // every later generation to mark the whole screen as changed.
            self.dirty_rows.fill(false);
            if self.dirty_rows.len() != new_rows {
                self.dirty_rows.resize(new_rows, false);
                self.row_generations.resize(new_rows, 0);
            }
        }

        let mut changed = replace;
        for changed_row in update.changed_rows {
            let row = usize::from(changed_row.y);
            if row >= new_rows {
                continue;
            }

            let start = row * new_cols;
            let end = start + new_cols;
            let target = &mut self.cells[start..end];
            let copied = changed_row.cells.len().min(new_cols);
            let differs = self.annotations[row] != changed_row.metadata
                || target[..copied] != changed_row.cells[..copied]
                || target[copied..].iter().any(|cell| *cell != GridCell::BLANK);
            if differs {
                self.annotations[row] = changed_row.metadata;
                target[..copied].copy_from_slice(&changed_row.cells[..copied]);
                target[copied..].fill(GridCell::BLANK);
                self.dirty_rows[row] = true;
                changed = true;
            }
        }

        let new_cursor = CursorState {
            col: update.cursor_col,
            row: update.cursor_row,
            visible: update.cursor_visible,
        };
        let cursor_changed = self.cursor != new_cursor;
        if cursor_changed {
            self.mark_dirty_row(usize::from(self.cursor.row));
            self.mark_dirty_row(usize::from(new_cursor.row));
            self.cursor = new_cursor;
            changed = true;
        }

        if changed {
            self.generation = self.generation.wrapping_add(1);
            for (row, dirty) in self.dirty_rows.iter().copied().enumerate() {
                if dirty {
                    self.row_generations[row] = self.generation;
                }
            }
        }

        ApplySummary {
            changed,
            size_changed,
            cursor_changed,
            dirty_row_count: self.dirty_rows().count(),
            generation: self.generation,
        }
    }

    fn mark_dirty_row(&mut self, row: usize) {
        if let Some(dirty) = self.dirty_rows.get_mut(row) {
            *dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use diri_proto::grid::{ChangedRow, TermColor, TermStyle};

    use super::*;

    fn cell(ch: char) -> GridCell {
        GridCell::new(
            u32::from(ch),
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    }

    fn update(full: bool, changed_rows: Vec<ChangedRow>) -> GridUpdate {
        GridUpdate {
            cols: 4,
            rows: 3,
            cursor_col: 1,
            cursor_row: 1,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows,
        }
    }

    #[test]
    fn full_snapshot_reallocates_and_pads_rows() {
        let mut buffer = GridBuffer::new(1, 1);
        buffer.clear_dirty();
        let result = buffer.apply(update(
            true,
            vec![ChangedRow::new(1, vec![cell('a'), cell('b')])],
        ));

        assert!(result.changed);
        assert!(result.size_changed);
        assert_eq!((buffer.cols, buffer.rows), (4, 3));
        assert_eq!(buffer.cells.len(), 12);
        assert_eq!(
            buffer.row(1).unwrap(),
            &[cell('a'), cell('b'), GridCell::BLANK, GridCell::BLANK]
        );
        assert_eq!(buffer.dirty_rows().collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn diff_only_marks_changed_and_cursor_rows() {
        let mut buffer = GridBuffer::default();
        buffer.apply(update(true, vec![]));
        buffer.clear_dirty();

        let mut diff = update(false, vec![ChangedRow::new(2, vec![cell('x'); 4])]);
        diff.cursor_row = 0;
        let result = buffer.apply(diff);

        assert!(result.cursor_changed);
        assert_eq!(buffer.dirty_rows().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(buffer.row(2).unwrap(), &[cell('x'); 4]);
    }

    #[test]
    fn identical_diff_does_not_advance_generation() {
        let mut buffer = GridBuffer::default();
        buffer.apply(update(true, vec![ChangedRow::new(0, vec![cell('x'); 4])]));
        buffer.clear_dirty();
        let generation = buffer.generation();

        let result = buffer.apply(update(false, vec![ChangedRow::new(0, vec![cell('x'); 4])]));

        assert!(!result.changed);
        assert_eq!(buffer.generation(), generation);
        assert_eq!(buffer.dirty_rows().count(), 0);
    }

    #[test]
    fn successive_diffs_do_not_keep_old_rows_dirty() {
        let mut buffer = GridBuffer::default();
        buffer.apply(update(true, vec![]));

        let result = buffer.apply(update(false, vec![ChangedRow::new(2, vec![cell('x'); 4])]));

        assert_eq!(result.dirty_row_count, 1);
        assert_eq!(buffer.dirty_rows().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn render_snapshot_clones_only_rows_changed_since_the_previous_frame() {
        let mut buffer = GridBuffer::default();
        buffer.apply(update(true, vec![]));
        let mut generations = Vec::new();
        assert_eq!(
            buffer
                .snapshot_damage(&mut generations, 3, 4, true)
                .changed_rows
                .len(),
            3
        );

        buffer.apply(update(false, vec![ChangedRow::new(2, vec![cell('x'); 4])]));
        let snapshot = buffer.snapshot_damage(&mut generations, 3, 4, false);

        assert_eq!(snapshot.changed_rows.len(), 1);
        assert_eq!(snapshot.changed_rows[0].row, 2);
        assert_eq!(snapshot.changed_rows[0].cells, vec![cell('x'); 4]);
    }

    #[test]
    fn out_of_range_rows_are_ignored() {
        let mut buffer = GridBuffer::default();
        buffer.apply(update(true, vec![]));
        buffer.clear_dirty();
        let generation = buffer.generation();

        let result = buffer.apply(update(false, vec![ChangedRow::new(99, vec![cell('x'); 4])]));

        assert!(!result.changed);
        assert_eq!(buffer.generation(), generation);
    }

    #[test]
    fn blankness_ignores_spaces_and_wide_glyph_continuations() {
        let mut buffer = GridBuffer::new(3, 1);
        assert!(buffer.is_blank());

        buffer.cells = vec![cell(' '), GridCell::BLANK, cell(' ')];
        assert!(buffer.is_blank());

        buffer.cells[1] = cell('x');
        assert!(!buffer.is_blank());
    }

    #[test]
    fn row_text_skips_wide_glyph_continuations() {
        let mut buffer = GridBuffer::new(3, 1);
        buffer.cells = vec![cell('界'), GridCell::BLANK, cell('x')];
        buffer.cells[1].scalar = 0;

        assert_eq!(
            buffer.row_text_with_columns(0),
            Some(("界x".to_owned(), vec![0, 2]))
        );
    }

    #[test]
    fn row_text_maps_combining_marks_and_wide_cells_without_absorbing_wrap_padding() {
        let mut buffer = GridBuffer::new(6, 1);
        buffer.cells = vec![
            cell('界'),
            cell('\0'),
            cell('e'),
            cell('x'),
            cell(' '),
            cell('\0'),
        ];
        buffer.annotations[0].graphemes.push((2, "\u{301}".into()));
        assert_eq!(
            buffer.row_text_with_cell_ranges(0),
            Some((
                "界e\u{301}x ".into(),
                vec![[0, 2], [2, 3], [2, 3], [3, 4], [4, 5]]
            ))
        );
    }
}
