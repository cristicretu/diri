//! Shell commands read from OSC 133 marks the parser keeps on cells.
//!
//! A command is the span from its prompt (A) to its end (D). Its output runs
//! from the output mark (C) to the end. Shells emit D and the next A at the
//! same position, so the end on a prompt's own cell belongs to the command
//! before it. Everything here is a client interpretation of retained cells:
//! rows that are not available end a search instead of being guessed.

use diri_proto::grid::{CommandEnd, GridCell, RowMetadata, TermStyle};

use crate::selection::SelectionPoint;

const MARKS: TermStyle = TermStyle::from_bits_retain(
    TermStyle::PROMPT_START.bits() | TermStyle::OUTPUT_START.bits() | TermStyle::COMMAND_END.bits(),
);

#[inline]
fn marked(cell: &GridCell) -> bool {
    cell.style.bits() & MARKS.bits() != 0
}

/// Finished commands whose prompt starts on rows `first..first + count`, with
/// how they ended. Ends are looked for up to `lookahead` rows past the
/// window; a command still running, or ending beyond that, has no entry.
pub fn finished_prompts<'a>(
    first: i64,
    count: usize,
    lookahead: usize,
    row: impl Fn(i64) -> Option<&'a [GridCell]>,
) -> Vec<(i64, CommandEnd)> {
    let mut finished = Vec::new();
    let window_end = first.saturating_add(count as i64);
    let scan_end = window_end.saturating_add(lookahead as i64);
    let mut open: Option<i64> = None;
    let mut absolute = first;
    while absolute < scan_end {
        if absolute >= window_end && open.is_none() {
            break;
        }
        let Some(cells) = row(absolute) else {
            break;
        };
        for cell in cells.iter().filter(|cell| marked(cell)) {
            if let Some(end) = cell.style.command_end()
                && let Some(prompt) = open.take()
            {
                finished.push((prompt, end));
            }
            if cell.style.contains(TermStyle::PROMPT_START) {
                open = (absolute < window_end).then_some(absolute);
            }
        }
        absolute += 1;
    }
    finished
}

/// The output of one finished command, as a reading-order range whose end is
/// exclusive, like a selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub status: CommandEnd,
}

/// The output of the finished command whose prompt, command line or output
/// holds `at`. Looks at most `limit` rows either way. `None` for a command
/// still running, one without an output mark, or one that printed nothing.
pub fn command_output_at<'a>(
    at: SelectionPoint,
    limit: usize,
    row: impl Fn(i64) -> Option<&'a [GridCell]>,
) -> Option<CommandOutput> {
    // Back to the prompt that starts the command holding `at`.
    let mut prompt = None;
    let lowest = at.row.saturating_sub(limit as i64);
    let mut absolute = at.row;
    'back: while absolute >= lowest {
        let cells = row(absolute)?;
        let last = if absolute == at.row {
            at.col.min(cells.len().saturating_sub(1))
        } else {
            cells.len().saturating_sub(1)
        };
        for col in (0..cells.len().min(last + 1)).rev() {
            if cells[col].style.contains(TermStyle::PROMPT_START) {
                prompt = Some(SelectionPoint { row: absolute, col });
                break 'back;
            }
        }
        absolute -= 1;
    }
    let prompt = prompt?;
    // Forward to its output and end, stopping at the next prompt.
    let mut output = None;
    let highest = prompt.row.saturating_add(limit as i64);
    let mut absolute = prompt.row;
    while absolute <= highest {
        let cells = row(absolute)?;
        for (col, cell) in cells.iter().enumerate() {
            let point = SelectionPoint { row: absolute, col };
            if point <= prompt || !marked(cell) {
                continue;
            }
            if let Some(status) = cell.style.command_end() {
                let start = output?;
                return (start < point).then_some(CommandOutput {
                    start,
                    end: point,
                    status,
                });
            }
            if cell.style.contains(TermStyle::PROMPT_START) {
                return None;
            }
            if cell.style.contains(TermStyle::OUTPUT_START) {
                output.get_or_insert(point);
            }
        }
        absolute += 1;
    }
    None
}

/// The rows of a command's output, cut to the output's first and last cells.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutputRows {
    pub rows: Vec<(Vec<GridCell>, Option<RowMetadata>)>,
    pub status: Option<CommandEnd>,
}

/// Finds the newest finished command output in rows fed in reading order,
/// holding only the rows of the output being read.
#[derive(Default)]
pub struct LastCommandOutput {
    capturing: Option<Vec<(Vec<GridCell>, Option<RowMetadata>)>>,
    /// Cells in `capturing`.
    captured_cells: usize,
    last: Option<OutputRows>,
}

impl LastCommandOutput {
    pub fn push_row(&mut self, cells: &[GridCell], metadata: Option<&RowMetadata>) {
        // Columns where this row's part of an output starts and stops.
        let mut from = self.capturing.as_ref().map(|_| 0);
        for (col, cell) in cells.iter().enumerate().filter(|(_, cell)| marked(cell)) {
            if let Some(status) = cell.style.command_end()
                && let Some(mut rows) = self.capturing.take()
            {
                if let Some(start) = from.take()
                    && col > start
                {
                    rows.push(cut(cells, metadata, start, col));
                }
                self.captured_cells = 0;
                self.last = Some(OutputRows {
                    rows,
                    status: Some(status),
                });
            }
            if cell.style.contains(TermStyle::PROMPT_START) {
                // A new prompt before an end abandons that output.
                self.capturing = None;
                self.captured_cells = 0;
                from = None;
            }
            if cell.style.contains(TermStyle::OUTPUT_START) {
                self.capturing = Some(Vec::new());
                self.captured_cells = 0;
                from = Some(col);
            }
        }
        if let (Some(rows), Some(start)) = (self.capturing.as_mut(), from) {
            rows.push(cut(cells, metadata, start, cells.len()));
            self.captured_cells += cells.len();
        }
    }

    /// Cells held for the output still being read, to bound the read. A
    /// finished output passed the same bound while it was read.
    #[must_use]
    pub fn held_cells(&self) -> usize {
        self.captured_cells
    }

    #[must_use]
    pub fn finish(self) -> Option<OutputRows> {
        self.last.filter(|last| !last.rows.is_empty())
    }
}

/// Columns `start..end` of a row. Cells before `start` become empty cells
/// (scalar 0, which text export skips) so grapheme columns in the metadata
/// stay valid.
fn cut(
    cells: &[GridCell],
    metadata: Option<&RowMetadata>,
    start: usize,
    end: usize,
) -> (Vec<GridCell>, Option<RowMetadata>) {
    let mut row = cells[..end].to_vec();
    for cell in &mut row[..start] {
        *cell = GridCell {
            scalar: 0,
            ..GridCell::BLANK
        };
    }
    if end < cells.len()
        && let Some(last) = row.last_mut()
    {
        // A cut row never continues onto the next one.
        last.style = TermStyle::from_bits_retain(last.style.bits() & !TermStyle::SOFT_WRAP.bits());
    }
    (row, metadata.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::grid::TermColor;

    fn cell(c: char, style: TermStyle) -> GridCell {
        GridCell::new(u32::from(c), TermColor::Default, TermColor::Default, style)
    }

    /// Rows from text, with marks written as `{A}`, `{C}`, `{0}` (D;0),
    /// `{1}` (D;1) and `{?}` (D without status) before the cell they mark.
    fn rows(lines: &[&str]) -> Vec<Vec<GridCell>> {
        lines
            .iter()
            .map(|line| {
                let mut cells = Vec::new();
                let mut pending = TermStyle::empty();
                let mut chars = line.chars();
                while let Some(c) = chars.next() {
                    if c == '{' {
                        let mark = chars.next().unwrap();
                        assert_eq!(chars.next(), Some('}'));
                        pending |= match mark {
                            'A' => TermStyle::PROMPT_START,
                            'C' => TermStyle::OUTPUT_START,
                            '0' => TermStyle::COMMAND_SUCCEEDED,
                            '1' => TermStyle::COMMAND_FAILED,
                            '?' => TermStyle::COMMAND_END,
                            _ => panic!("unknown mark {mark}"),
                        };
                        continue;
                    }
                    cells.push(cell(c, pending));
                    pending = TermStyle::empty();
                }
                cells.resize(12, cell(' ', pending));
                cells
            })
            .collect()
    }

    fn lookup<'a>(rows: &'a [Vec<GridCell>]) -> impl Fn(i64) -> Option<&'a [GridCell]> {
        move |row| {
            usize::try_from(row)
                .ok()
                .and_then(|row| rows.get(row))
                .map(Vec::as_slice)
        }
    }

    fn session() -> Vec<Vec<GridCell>> {
        rows(&[
            "{A}$ ls",
            "{C}a",
            "b",
            "{0}{A}$ false",
            "{C}{1}{A}$ make",
            "{C}building",
        ])
    }

    #[test]
    fn a_prompt_shows_how_its_own_command_ended() {
        let session = session();
        assert_eq!(
            finished_prompts(0, 6, 0, lookup(&session)),
            [(0, CommandEnd::Succeeded), (3, CommandEnd::Failed)],
            "the running `make` has no end yet"
        );
        // The end of a prompt above the window is not drawn, and an end past
        // the window is found within the lookahead only.
        assert_eq!(
            finished_prompts(1, 3, 1, lookup(&session)),
            [(3, CommandEnd::Failed)]
        );
        assert_eq!(finished_prompts(0, 1, 2, lookup(&session)), []);
        assert_eq!(
            finished_prompts(0, 1, 3, lookup(&session)),
            [(0, CommandEnd::Succeeded)]
        );
        let unknown = rows(&["{A}$ x", "{C}y", "{?}{A}$"]);
        assert_eq!(
            finished_prompts(0, 3, 0, lookup(&unknown)),
            [(0, CommandEnd::Unknown)]
        );
    }

    #[test]
    fn command_output_runs_from_its_output_mark_to_its_end() {
        let session = session();
        let point = |row, col| SelectionPoint { row, col };
        let ls = Some(CommandOutput {
            start: point(1, 0),
            end: point(3, 0),
            status: CommandEnd::Succeeded,
        });
        // From the prompt, the command line and the output alike.
        for at in [point(0, 0), point(0, 4), point(1, 0), point(2, 5)] {
            assert_eq!(command_output_at(at, 100, lookup(&session)), ls, "{at:?}");
        }
        // The cell holding `false`'s prompt also holds `ls`'s end, and
        // belongs to `false`, which printed nothing.
        assert_eq!(command_output_at(point(3, 0), 100, lookup(&session)), None);
        // Still running.
        assert_eq!(command_output_at(point(5, 3), 100, lookup(&session)), None);
        // Rows that are not available end the search.
        assert_eq!(command_output_at(point(2, 0), 1, lookup(&session)), None);
    }

    #[test]
    fn the_last_finished_output_is_kept_while_reading_forward() {
        let mut last = LastCommandOutput::default();
        for row in rows(&[
            "{A}$ ls",
            "{C}a",
            "b",
            "{0}{A}$ echo -n x",
            "{C}x{1}{A}$ sleep",
            "{C}z",
        ]) {
            last.push_row(&row, None);
        }
        let output = last.finish().unwrap();
        assert_eq!(output.status, Some(CommandEnd::Failed));
        let text: Vec<String> = output
            .rows
            .iter()
            .map(|(cells, _)| {
                cells
                    .iter()
                    .map(|cell| char::from_u32(cell.scalar).unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(text, ["x"], "`sleep` has not finished");

        let mut last = LastCommandOutput::default();
        for row in rows(&["{A}$ ls", "{C}a", "b", "{0}{A}$"]) {
            last.push_row(&row, None);
        }
        let output = last.finish().unwrap();
        assert_eq!(output.rows.len(), 2, "the end's own row holds no output");
        assert_eq!(output.rows[0].0[0].scalar, u32::from('a'));
    }
}
