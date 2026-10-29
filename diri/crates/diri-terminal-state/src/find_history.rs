//! Find over everything a terminal retains, a bounded run of rows at a time.
//!
//! History rows are decoded one at a time from their compact blocks and
//! matched as text; nothing is kept but the matches. The caller holds the
//! screen for one call and releases it between calls, so a search over
//! 100,000+ rows never holds the terminal longer than one run.

use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Flags;
use diri_proto::find::FindNeedle;

use crate::HeadlessScreen;
use crate::emulator::{CellView, Emulator, RowView};

/// A compiled query and the row scratch it reuses across rows and runs.
#[derive(Debug)]
pub struct HistorySearch {
    needle: FindNeedle,
    text: Vec<char>,
    columns: Vec<[u16; 2]>,
}

impl HistorySearch {
    /// `None` for an empty query.
    #[must_use]
    pub fn new(query: &str) -> Option<Self> {
        Some(Self {
            needle: FindNeedle::new(query)?,
            text: Vec::new(),
            columns: Vec::new(),
        })
    }
}

impl HeadlessScreen {
    /// Stable row of absolute row 0 (the oldest retained row): how many rows
    /// have left the active screen's history from its oldest end.
    #[must_use]
    pub fn history_origin(&self) -> i64 {
        i64::try_from(self.term.history_origin()).unwrap_or(i64::MAX)
    }

    /// Rows the active screen holds: history and the visible grid.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.term.history_rows() + self.geometry.rows
    }

    /// Searches up to `max_rows` absolute rows below `before` (exclusive),
    /// newest first, appending `[row, start_col, end_col)` matches in that
    /// order (columns ascending within a row). Stops early at the first row
    /// boundary at which `out` holds `stop_at` matches. Returns the lowest
    /// row searched, which is where the next run starts.
    pub fn find_rows_before(
        &mut self,
        before: usize,
        max_rows: usize,
        search: &mut HistorySearch,
        stop_at: usize,
        out: &mut Vec<[i64; 3]>,
    ) -> usize {
        let history = self.term.history_rows();
        let before = before.min(history + self.geometry.rows);
        let start = before.saturating_sub(max_rows);
        let mut next = before;
        let mut searched = 0;
        while next > start {
            if out.len() >= stop_at {
                break;
            }
            next -= 1;
            self.search_row(next, history, search, out);
            searched += 1;
            self.finish_history_read_batch(searched, next == start || out.len() >= stop_at);
        }
        if searched > 0 {
            // Every exit leaves no decoded block behind.
            self.finish_history_read_batch(searched, true);
        }
        next
    }

    fn search_row(
        &self,
        index: usize,
        history: usize,
        search: &mut HistorySearch,
        out: &mut Vec<[i64; 3]>,
    ) {
        let cols = self.geometry.cols;
        let line = Line(index as i32 - history as i32);
        let row = self.term.row(line);
        let cells = row.cells();
        let HistorySearch {
            needle,
            text,
            columns,
        } = search;
        text.clear();
        columns.clear();
        for (x, cell) in cells.iter().enumerate().take(cols) {
            let flags = cell.cell_flags();
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let width = if flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let range = [x as u16, (x + width).min(cols) as u16];
            let c = cell.ch();
            text.push(if c < ' ' && c != '\t' { ' ' } else { c });
            columns.push(range);
            if let Some(combining) = cell.combining() {
                for &ch in combining {
                    text.push(ch);
                    columns.push(range);
                }
            }
        }
        let row = index as i64;
        needle.for_each_match(text, |start, end| {
            out.push([
                row,
                i64::from(columns[start][0]),
                i64::from(columns[end - 1][1]),
            ]);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_with(lines: usize, needle_every: usize) -> HeadlessScreen {
        let mut screen = HeadlessScreen::new(80, 24);
        let mut bytes = Vec::new();
        for line in 0..lines {
            if line % needle_every == 0 {
                bytes.extend_from_slice(format!("line {line} has the Needle\r\n").as_bytes());
            } else {
                bytes.extend_from_slice(format!("line {line} plain output\r\n").as_bytes());
            }
        }
        screen.feed(&bytes);
        screen
    }

    fn search_all(screen: &mut HeadlessScreen, query: &str) -> Vec<[i64; 3]> {
        let mut search = HistorySearch::new(query).unwrap();
        let mut out = Vec::new();
        let mut before = screen.total_rows();
        while before > 0 {
            before = screen.find_rows_before(before, 1024, &mut search, usize::MAX, &mut out);
        }
        out
    }

    #[test]
    fn finds_every_match_in_deep_history_newest_first() {
        // Past the 10,000 rows exports carry and the ~2,000 a capture holds.
        let mut screen = screen_with(60_000, 1_000);
        assert!(screen.history_rows() > 50_000, "{}", screen.history_rows());
        let found = search_all(&mut screen, "needle");
        assert_eq!(found.len(), 60);
        assert!(found.windows(2).all(|pair| pair[0][0] > pair[1][0]));
        // The oldest match is the first line ever printed, at row 0, and its
        // columns are where the text is.
        assert_eq!(found.last().unwrap(), &[0, 15, 21]);
        // Smart case: an uppercase query is exact.
        assert_eq!(search_all(&mut screen, "NEEDLE").len(), 0);
        assert_eq!(search_all(&mut screen, "Needle").len(), 60);
    }

    #[test]
    fn stops_at_a_row_boundary_and_resumes_without_gaps() {
        let mut screen = HeadlessScreen::new(40, 4);
        screen.feed(b"a a a\r\nb\r\na a\r\nb\r\na\r\n");
        let total = screen.total_rows();
        let mut search = HistorySearch::new("a").unwrap();
        let mut out = Vec::new();
        let next = screen.find_rows_before(total, usize::MAX, &mut search, 1, &mut out);
        // Row 4 holds the newest "a"; the run stops right after that row.
        assert_eq!(out, vec![[4, 0, 1]]);
        assert_eq!(next, 4);
        let next = screen.find_rows_before(next, usize::MAX, &mut search, 3, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(next, 2);
        screen.find_rows_before(next, usize::MAX, &mut search, usize::MAX, &mut out);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn wide_and_combining_text_matches_whole_cells() {
        let mut screen = HeadlessScreen::new(20, 2);
        screen.feed("<界> e\u{301}\r\nplain\r\n".as_bytes());
        for (query, start, end) in [("界", 1, 3), ("<界>", 0, 4), ("e\u{301}", 5, 6)] {
            let found = search_all(&mut screen, query);
            assert_eq!(found, vec![[0, start, end]], "query {query:?}");
        }
    }

    #[test]
    fn origin_counts_rows_evicted_from_the_oldest_end() {
        // Byte-bounded history evicts; the stable row of a line survives it.
        let mut screen = HeadlessScreen::new(80, 4);
        assert_eq!(screen.history_origin(), 0);
        screen.feed(b"first marker\r\n");
        let mut noise = Vec::new();
        for i in 0..200_000 {
            noise.extend_from_slice(format!("{i:x} {}\r\n", "q".repeat(i % 60)).as_bytes());
        }
        screen.feed(&noise);
        screen.feed(b"second marker\r\n");
        let origin = screen.history_origin();
        assert!(origin > 0, "history evicted");
        let found = search_all(&mut screen, "second marker");
        assert_eq!(found.len(), 1);
        let stable = origin + found[0][0];
        // More output evicts more rows: the absolute row moves, the stable
        // one does not.
        screen.feed(&noise[..noise.len() / 4]);
        let found = search_all(&mut screen, "second marker");
        assert_eq!(screen.history_origin() + found[0][0], stable);
        assert!(screen.history_origin() > origin);
        assert!(search_all(&mut screen, "first marker").is_empty());
    }
}
