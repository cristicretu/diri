//! Pure, blocking terminal-history search.
//!
//! [`SearchJob`] owns an immutable history/grid snapshot, so callers can move
//! the whole scan onto a background executor without locks or callbacks. The
//! result is capped while retaining the newest matches; presentation state and
//! stale-generation decisions stay in the parent find model.

use std::collections::VecDeque;

use crate::buffer::GridBuffer;

use super::{FindMatch, FindSnapshot, MATCH_CAP, SearchRequest};

/// An immutable search input prepared with a short clone of the live grid.
///
/// `run` is deliberately the only operation: all scanning and Unicode case
/// comparison lives behind this seam. It is CPU-bound and must be called from
/// a background executor.
pub struct SearchJob {
    request: SearchRequest,
    snapshot: FindSnapshot,
    live: Option<GridBuffer>,
}

/// A completed pure search plus the geometry it was computed against.
pub struct SearchResult {
    pub(super) request: SearchRequest,
    pub(super) error: Option<String>,
    pub(super) source: Option<std::sync::Arc<super::RetainedFindSnapshot>>,
    pub(super) matches: Vec<FindMatch>,
    pub(super) visible_start_row: i64,
    pub(super) rows: i64,
    pub(super) cols: i64,
    pub(super) content_seq: u64,
    pub(super) is_alt_screen: bool,
}

impl SearchJob {
    pub(super) fn new(
        request: SearchRequest,
        snapshot: FindSnapshot,
        live: Option<GridBuffer>,
    ) -> Self {
        Self {
            request,
            snapshot,
            live,
        }
    }

    #[must_use]
    pub fn run(self) -> SearchResult {
        let matches = build_matches(
            &self.request.query,
            &self.snapshot,
            self.live.as_ref().unwrap_or(&GridBuffer::default()),
        );
        SearchResult {
            request: self.request,
            error: self.snapshot.error,
            matches,
            source: self.snapshot.retained,
            visible_start_row: self.snapshot.visible_start_row,
            rows: self.snapshot.rows,
            cols: self.snapshot.cols,
            content_seq: self.snapshot.content_seq,
            is_alt_screen: self.snapshot.is_alt_screen,
        }
    }
}

fn build_matches(query: &str, snapshot: &FindSnapshot, live: &GridBuffer) -> Vec<FindMatch> {
    let needle: Vec<char> = query.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }

    // Search still walks every retained line, but memory stays bounded and
    // each newer hit displaces the oldest. This keeps the live screen and the
    // newest history discoverable even when old output alone exceeds the cap.
    let mut matches = VecDeque::with_capacity(MATCH_CAP);
    // One char scratch reused across every scanned line: a fresh Vec per line
    // measurably dominates scans of large histories.
    let mut scratch = Vec::new();

    if let Some(source) = &snapshot.retained {
        for index in 0..source.row_count() {
            let absolute = source.first_row + index as i64;
            if source.is_alt_screen && absolute < source.live_start_row {
                continue;
            }
            if let Some((line, columns)) = source.row_text(absolute) {
                append_matches(
                    &line,
                    Some(&columns),
                    absolute,
                    &needle,
                    &mut scratch,
                    &mut matches,
                );
            }
        }
        return matches.into();
    }

    if !snapshot.is_alt_screen {
        for (index, line) in snapshot.lines.iter().enumerate() {
            let absolute_row = snapshot
                .first_row
                .saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
            if absolute_row >= snapshot.visible_start_row {
                continue;
            }
            append_matches(
                line,
                snapshot.text_cells.get(&index).map(Vec::as_slice),
                absolute_row,
                &needle,
                &mut scratch,
                &mut matches,
            );
        }
    }

    let live_rows = usize::try_from(snapshot.rows.max(0))
        .unwrap_or(usize::MAX)
        .min(usize::from(live.rows));
    for row in 0..live_rows {
        let Some((line, columns)) = live.row_text_with_cell_ranges(row) else {
            continue;
        };
        append_matches(
            &line,
            Some(&columns),
            snapshot
                .visible_start_row
                .saturating_add(i64::try_from(row).unwrap_or(i64::MAX)),
            &needle,
            &mut scratch,
            &mut matches,
        );
    }

    matches.into()
}

fn append_matches(
    line: &str,
    columns: Option<&[[u16; 2]]>,
    absolute_row: i64,
    needle: &[char],
    scratch: &mut Vec<char>,
    output: &mut VecDeque<FindMatch>,
) {
    scratch.clear();
    scratch.extend(line.chars());
    let haystack: &[char] = scratch;
    if haystack.len() < needle.len() {
        return;
    }

    let case_sensitive = needle.iter().any(|ch| ch.is_uppercase());
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if chars_equal(
            &haystack[index..index + needle.len()],
            needle,
            case_sensitive,
        ) {
            if output.len() == MATCH_CAP {
                output.pop_front();
            }
            output.push_back(FindMatch {
                absolute_row,
                start_col: columns
                    .and_then(|ranges| ranges.get(index))
                    .map_or(index, |range| usize::from(range[0])),
                end_col_exclusive: columns
                    .and_then(|ranges| ranges.get(index + needle.len() - 1))
                    .map_or(index + needle.len(), |range| usize::from(range[1])),
            });
            // Preserve the existing non-overlapping navigation semantics.
            index += needle.len();
        } else {
            index += 1;
        }
    }
}

fn chars_equal(haystack: &[char], needle: &[char], case_sensitive: bool) -> bool {
    haystack.iter().zip(needle).all(|(left, right)| {
        // Exact match first skips allocation on the overwhelmingly common
        // path, including every mismatching position the scan visits.
        left == right || (!case_sensitive && left.to_lowercase().eq(right.to_lowercase()))
    })
}

#[cfg(test)]
mod qol_tests {
    use super::*;

    #[test]
    fn unicode_matches_cover_complete_cells_in_live_grid_and_history() {
        let mut terminal = diri_terminal_state::HeadlessScreen::new(8, 2);
        terminal.feed("<界> e\u{301}\r\nplain\r\n<界> e\u{301}".as_bytes());
        let mut live = GridBuffer::default();
        live.apply(terminal.full_snapshot());
        let snapshot = FindSnapshot::from(terminal.scrollback());
        for (query, start, end) in [
            ("界", 1, 3),
            ("<界>", 0, 4),
            ("e", 5, 6),
            ("e\u{301}", 5, 6),
            ("\u{301}", 5, 6),
        ] {
            assert_eq!(
                build_matches(query, &snapshot, &live),
                vec![
                    FindMatch {
                        absolute_row: 0,
                        start_col: start,
                        end_col_exclusive: end
                    },
                    FindMatch {
                        absolute_row: 2,
                        start_col: start,
                        end_col_exclusive: end
                    },
                ],
                "query {query:?}"
            );
        }
    }
    #[test]
    fn uppercase_query_is_exact_and_lowercase_folds_unicode() {
        let mut result = VecDeque::new();
        append_matches(
            "Error error ERROR",
            None,
            0,
            &"Error".chars().collect::<Vec<_>>(),
            &mut Vec::new(),
            &mut result,
        );
        assert_eq!(result.len(), 1);
        result.clear();
        append_matches(
            "Échec échec",
            None,
            0,
            &"échec".chars().collect::<Vec<_>>(),
            &mut Vec::new(),
            &mut result,
        );
        assert_eq!(result.len(), 2);
    }
}
