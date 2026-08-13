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
    live: GridBuffer,
}

/// A completed pure search plus the geometry it was computed against.
pub struct SearchResult {
    pub(super) request: SearchRequest,
    pub(super) matches: Vec<FindMatch>,
    pub(super) visible_start_row: i64,
    pub(super) rows: i64,
    pub(super) cols: i64,
    pub(super) content_seq: u64,
    pub(super) is_alt_screen: bool,
}

impl SearchJob {
    pub(super) fn new(request: SearchRequest, snapshot: FindSnapshot, live: GridBuffer) -> Self {
        Self {
            request,
            snapshot,
            live,
        }
    }

    #[must_use]
    pub fn run(self) -> SearchResult {
        let matches = build_matches(&self.request.query, &self.snapshot, &self.live);
        SearchResult {
            request: self.request,
            matches,
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
                None,
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
        let Some((line, columns)) = live.row_text_with_columns(row) else {
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
    columns: Option<&[usize]>,
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

    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if chars_equal_ci(&haystack[index..index + needle.len()], needle) {
            if output.len() == MATCH_CAP {
                output.pop_front();
            }
            output.push_back(FindMatch {
                absolute_row,
                start_col: column_for(index, columns),
                end_col_exclusive: column_past_end(index + needle.len(), columns),
            });
            // Preserve the existing non-overlapping navigation semantics.
            index += needle.len();
        } else {
            index += 1;
        }
    }
}

fn chars_equal_ci(haystack: &[char], needle: &[char]) -> bool {
    haystack.iter().zip(needle).all(|(left, right)| {
        // Exact match first skips allocation on the overwhelmingly common
        // path, including every mismatching position the scan visits.
        left == right || left.to_lowercase().eq(right.to_lowercase())
    })
}

fn column_for(index: usize, columns: Option<&[usize]>) -> usize {
    columns.map_or(index, |columns| {
        columns
            .get(index)
            .copied()
            .or_else(|| columns.last().map(|last| last + 1))
            .unwrap_or(index)
    })
}

fn column_past_end(index: usize, columns: Option<&[usize]>) -> usize {
    column_for(index, columns)
}
