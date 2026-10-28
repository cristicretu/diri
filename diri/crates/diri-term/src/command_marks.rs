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

/// What a search for the nearest prompt mark found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptSearch {
    /// The absolute row of the nearest prompt that way.
    Found(i64),
    /// Every row that way was read and none starts a prompt.
    Absent,
    /// A row that way is not held here, so a fuller copy must answer.
    Unknown,
}

/// Whether a row holds the start of a shell prompt.
#[inline]
#[must_use]
pub fn starts_prompt(cells: &[GridCell]) -> bool {
    cells
        .iter()
        .any(|cell| cell.style.contains(TermStyle::PROMPT_START))
}

/// The prompt nearest to `from`, strictly above it (`next == false`) or
/// strictly below it, among rows `first..end`. Reads outward one row at a
/// time and stops at the first prompt, so its cost is the distance to that
/// prompt, not the length of the history. A row `row` cannot supply makes the
/// answer [`PromptSearch::Unknown`] instead of a guess.
pub fn nearest_prompt<'a>(
    from: i64,
    next: bool,
    first: i64,
    end: i64,
    row: impl Fn(i64) -> Option<&'a [GridCell]>,
) -> PromptSearch {
    let step = if next { 1 } else { -1 };
    let mut absolute = from.saturating_add(step);
    while absolute >= first && absolute < end {
        let Some(cells) = row(absolute) else {
            return PromptSearch::Unknown;
        };
        if starts_prompt(cells) {
            return PromptSearch::Found(absolute);
        }
        absolute += step;
    }
    PromptSearch::Absent
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

/// What the line under the cursor is, read from the OSC 133 marks on the
/// screen. Decides whether typing a command there runs it in the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptLine {
    /// A shell prompt whose input start (`B`) is marked and has nothing
    /// typed before the cursor.
    Empty,
    /// A marked prompt with text typed after its input start.
    Typed,
    /// A shell prompt (`A`, or the last command's `D`) with no input-start
    /// mark, so typed text cannot be told from the prompt (fish).
    Unknown,
    /// A command line is running (`C` with no `D` since).
    Busy,
    /// No marks on screen before the cursor.
    Unmarked,
}

/// Reads [`PromptLine`] backwards from the cursor through the screen rows.
/// The cursor's own cell counts for its marks, not its text: an empty
/// prompt parks the cursor on the cell its `B` was written to. A prompt
/// drawn inside a running command line (fish's `read`: `C`, then `A` with
/// no `D`) is that command asking for input, so it reads as busy.
pub fn prompt_line<'a>(
    cursor_row: usize,
    cursor_col: usize,
    row: impl Fn(usize) -> Option<&'a [GridCell]>,
) -> PromptLine {
    let mut typed = false;
    // The prompt's verdict, once its `B` or `A` is found, and whether its
    // `A` has been passed yet.
    let mut verdict: Option<PromptLine> = None;
    let mut passed_prompt_start = false;
    for line in (0..=cursor_row).rev() {
        let Some(cells) = row(line) else {
            break;
        };
        let end = if line == cursor_row {
            (cursor_col + 1).min(cells.len())
        } else {
            cells.len()
        };
        for (col, cell) in cells[..end].iter().enumerate().rev() {
            let style = cell.style;
            let ended = style.command_end().is_some();
            let prompt = style.contains(TermStyle::PROMPT_START);
            let output = style.contains(TermStyle::OUTPUT_START);
            let Some(found) = verdict else {
                let before_cursor = line < cursor_row || col < cursor_col;
                if before_cursor && cell.scalar != 0 && cell.scalar != u32::from(' ') {
                    typed = true;
                }
                if style.contains(TermStyle::INPUT_START) {
                    verdict = Some(if typed {
                        PromptLine::Typed
                    } else {
                        PromptLine::Empty
                    });
                } else if prompt || ended {
                    verdict = Some(PromptLine::Unknown);
                    passed_prompt_start = true;
                } else if output {
                    return PromptLine::Busy;
                }
                if verdict.is_some() && ended {
                    return verdict.unwrap_or(PromptLine::Unmarked);
                }
                continue;
            };
            if !passed_prompt_start {
                if prompt {
                    passed_prompt_start = true;
                    if ended {
                        return found;
                    }
                }
                continue;
            }
            if output && !ended && !prompt {
                return PromptLine::Busy;
            }
            if output || ended || prompt {
                return found;
            }
        }
    }
    verdict.unwrap_or(PromptLine::Unmarked)
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
                            'B' => TermStyle::INPUT_START,
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

    fn prompt_at(lines: &[&str], row: usize, col: usize) -> PromptLine {
        let rows = rows(lines);
        prompt_line(row, col, |line| rows.get(line).map(Vec::as_slice))
    }

    #[test]
    fn a_marked_empty_prompt_is_ready_for_a_command() {
        // bash/zsh with shell integration: A, prompt text, B where input
        // starts; the cursor sits on the B cell.
        assert_eq!(prompt_at(&["{A}~/w $ {B}"], 0, 8), PromptLine::Empty);
        // A finished command above does not matter; the new prompt does.
        let screen = ["{A}$ {B}ls", "{C}a b", "{0}{A}$ {B}"];
        assert_eq!(prompt_at(&screen, 2, 2), PromptLine::Empty);
        // A two-line prompt: A on the line above, B on the cursor's line.
        assert_eq!(
            prompt_at(&["{A}~/w main", "> {B}"], 1, 2),
            PromptLine::Empty
        );
    }

    #[test]
    fn typed_text_at_a_marked_prompt_is_not_overwritten() {
        assert_eq!(prompt_at(&["{A}$ {B}git st"], 0, 8), PromptLine::Typed);
        // Text after the cursor alone (an autosuggestion) is not typed.
        assert_eq!(prompt_at(&["{A}$ {B}git status"], 0, 2), PromptLine::Empty);
    }

    #[test]
    fn fish_prompts_without_an_input_mark_are_unknown() {
        assert_eq!(prompt_at(&["{0}{A}~/w> "], 0, 5), PromptLine::Unknown);
        assert_eq!(prompt_at(&["{A}~/w> echo"], 0, 9), PromptLine::Unknown);
    }

    #[test]
    fn a_running_command_or_unmarked_screen_is_refused() {
        let running = ["{A}$ {B}vim", "{C}~", "~"];
        assert_eq!(prompt_at(&running, 2, 1), PromptLine::Busy);
        // An agent started from the shell is a running command line.
        assert_eq!(
            prompt_at(&["{A}$ {B}claude", "{C}> "], 1, 2),
            PromptLine::Busy
        );
        assert_eq!(
            prompt_at(&["plain output", "$ "], 1, 2),
            PromptLine::Unmarked
        );
        // fish's `read` inside a running command: `A` after `C`, no `D`.
        let read = ["{A}~> {C}read x", "{A}read> "];
        assert_eq!(prompt_at(&read, 1, 6), PromptLine::Busy);
        // The same prompt after the command ended is the shell's own.
        let idle = ["{A}~> {C}true", "{0}{A}~> "];
        assert_eq!(prompt_at(&idle, 1, 3), PromptLine::Unknown);
    }

    #[test]
    fn the_nearest_prompt_is_read_outward_and_never_guessed() {
        let session = session();
        let all = lookup(&session);
        assert_eq!(nearest_prompt(5, false, 0, 6, &all), PromptSearch::Found(4));
        assert_eq!(nearest_prompt(4, false, 0, 6, &all), PromptSearch::Found(3));
        assert_eq!(nearest_prompt(3, false, 0, 6, &all), PromptSearch::Found(0));
        assert_eq!(nearest_prompt(0, false, 0, 6, &all), PromptSearch::Absent);
        assert_eq!(nearest_prompt(0, true, 0, 6, &all), PromptSearch::Found(3));
        assert_eq!(nearest_prompt(4, true, 0, 6, &all), PromptSearch::Absent);
        // A gap in the held rows stops the search before it guesses past it.
        let held = |row: i64| (row != 2).then(|| session[row as usize].as_slice());
        assert_eq!(nearest_prompt(3, false, 0, 6, held), PromptSearch::Unknown);
        assert_eq!(nearest_prompt(1, true, 0, 6, held), PromptSearch::Unknown);
        assert_eq!(nearest_prompt(4, false, 0, 6, held), PromptSearch::Found(3));
        // Rows outside `first..end` are never asked for.
        assert_eq!(
            nearest_prompt(2, false, 1, 6, |row| {
                assert!(row >= 1, "asked for row {row}");
                Some(session[row as usize].as_slice())
            }),
            PromptSearch::Absent
        );
    }
}
