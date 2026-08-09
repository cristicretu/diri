//! Debounced, capped find over daemon history plus the authoritative live grid.

use std::time::Duration;

use diri_proto::methods::ReadScrollbackResult;

use crate::buffer::GridBuffer;
use crate::scrollback::ScrollbackViewport;

pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
pub const OUTPUT_RESCAN_DELAY: Duration = Duration::from_millis(100);
pub const MATCH_CAP: usize = 500;
pub const HISTORY_ANCHOR: f32 = 0.33;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FindMatch {
    pub absolute_row: i64,
    pub start_col: usize,
    pub end_col_exclusive: usize,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FindSpan {
    pub row: usize,
    pub start_col: usize,
    pub end_col_exclusive: usize,
    pub is_current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FindSnapshot {
    pub lines: Vec<String>,
    pub first_row: i64,
    pub visible_start_row: i64,
    pub cols: i64,
    pub rows: i64,
    pub content_seq: u64,
    pub is_alt_screen: bool,
}

impl From<ReadScrollbackResult> for FindSnapshot {
    fn from(result: ReadScrollbackResult) -> Self {
        Self {
            lines: result.lines,
            first_row: result.first_row,
            visible_start_row: result.visible_start_row,
            cols: result.cols,
            rows: result.rows,
            content_seq: result.content_seq,
            is_alt_screen: result.is_alt_screen,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchRequest {
    pub query: String,
    pub is_rescan: bool,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavigationTarget {
    Live,
    History { absolute_row: i64, anchor: f32 },
}

#[derive(Clone, Debug, Default)]
pub struct TerminalFindModel {
    query: String,
    matches: Vec<FindMatch>,
    current_index: usize,
    is_alt_screen: bool,
    cached_visible_start_row: Option<i64>,
    cached_rows: usize,
    cached_content_seq: Option<u64>,
    cached_cols: Option<i64>,
    generation: u64,
    search_due: Option<Duration>,
    rescan_due: Option<Duration>,
}

impl TerminalFindModel {
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub fn matches(&self) -> &[FindMatch] {
        &self.matches
    }

    #[must_use]
    pub const fn current_index(&self) -> usize {
        self.current_index
    }

    #[must_use]
    pub const fn is_alt_screen(&self) -> bool {
        self.is_alt_screen
    }

    pub fn set_query(&mut self, query: impl Into<String>, now: Duration) {
        let query = query.into();
        if query == self.query {
            return;
        }
        self.query = query;
        self.generation = self.generation.wrapping_add(1);
        self.rescan_due = None;
        if self.query.is_empty() {
            self.matches.clear();
            self.current_index = 0;
            self.search_due = None;
        } else {
            self.search_due = Some(now.saturating_add(SEARCH_DEBOUNCE));
        }
    }

    /// Coalesces a busy output stream to at most one pending 100 ms rescan.
    /// Returns true only when this call armed the rescan, so the host schedules
    /// exactly one follow-up timer per burst.
    pub fn on_output(&mut self, now: Duration) -> bool {
        if self.query.is_empty() || self.rescan_due.is_some() {
            return false;
        }
        self.rescan_due = Some(now.saturating_add(OUTPUT_RESCAN_DELAY));
        true
    }

    /// Pulls one due request. The app asynchronously reads a scrollback text
    /// snapshot, then returns it through [`Self::apply_snapshot`].
    pub fn take_due_search(&mut self, now: Duration) -> Option<SearchRequest> {
        let is_rescan = if self.search_due.is_some_and(|due| due <= now) {
            self.search_due = None;
            false
        } else if self.rescan_due.is_some_and(|due| due <= now) {
            self.rescan_due = None;
            true
        } else {
            return None;
        };
        Some(SearchRequest {
            query: self.query.clone(),
            is_rescan,
            generation: self.generation,
        })
    }

    /// Discards stale async responses, builds sorted matches, and preserves the
    /// current index only for a same-geometry/content rescan.
    pub fn apply_snapshot(
        &mut self,
        request: &SearchRequest,
        snapshot: FindSnapshot,
        live: &GridBuffer,
        viewport: &mut ScrollbackViewport,
    ) -> bool {
        if request.generation != self.generation || request.query != self.query {
            return false;
        }
        let sequence_changed = self.cached_content_seq != Some(snapshot.content_seq)
            || self.cached_cols != Some(snapshot.cols);
        self.matches = build_matches(&request.query, &snapshot, live);
        self.is_alt_screen = snapshot.is_alt_screen;
        self.cached_visible_start_row = Some(snapshot.visible_start_row);
        self.cached_rows = usize::try_from(snapshot.rows.max(0)).unwrap_or(usize::MAX);
        self.cached_content_seq = Some(snapshot.content_seq);
        self.cached_cols = Some(snapshot.cols);
        viewport.apply_geometry(
            snapshot.visible_start_row,
            snapshot.visible_start_row.max(0),
            snapshot.content_seq,
            self.cached_rows,
        );

        if request.is_rescan && !sequence_changed {
            self.current_index = self.current_index.min(self.matches.len().saturating_sub(1));
        } else {
            let window_top = snapshot
                .visible_start_row
                .saturating_sub(viewport.view_offset());
            self.current_index = self
                .matches
                .iter()
                .position(|item| item.absolute_row >= window_top)
                .unwrap_or(0);
        }
        true
    }

    #[must_use]
    pub fn visible_spans(&self, viewport: &ScrollbackViewport) -> Vec<FindSpan> {
        let Some(_) = self.cached_visible_start_row else {
            return Vec::new();
        };
        let window_top = self
            .cached_visible_start_row
            .unwrap_or_default()
            .saturating_sub(viewport.view_offset());
        self.matches
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let row = item.absolute_row.checked_sub(window_top)?;
                let row = usize::try_from(row).ok()?;
                (row < self.cached_rows).then_some(FindSpan {
                    row,
                    start_col: item.start_col,
                    end_col_exclusive: item.end_col_exclusive,
                    is_current: index == self.current_index,
                })
            })
            .collect()
    }

    pub fn next(&mut self, viewport: &mut ScrollbackViewport) -> Option<NavigationTarget> {
        self.advance(1, viewport)
    }

    pub fn previous(&mut self, viewport: &mut ScrollbackViewport) -> Option<NavigationTarget> {
        self.advance(-1, viewport)
    }

    fn advance(
        &mut self,
        direction: isize,
        viewport: &mut ScrollbackViewport,
    ) -> Option<NavigationTarget> {
        if self.matches.is_empty() {
            return None;
        }
        self.current_index = self
            .current_index
            .wrapping_add_signed(direction)
            .rem_euclid(self.matches.len());
        let item = &self.matches[self.current_index];
        let visible_start = self.cached_visible_start_row?;
        let target = if item.absolute_row >= visible_start {
            viewport.scroll_to_live(self.cached_rows);
            NavigationTarget::Live
        } else {
            viewport.scroll_to_absolute(item.absolute_row, HISTORY_ANCHOR, self.cached_rows);
            NavigationTarget::History {
                absolute_row: item.absolute_row,
                anchor: HISTORY_ANCHOR,
            }
        };
        Some(target)
    }
}

fn build_matches(query: &str, snapshot: &FindSnapshot, live: &GridBuffer) -> Vec<FindMatch> {
    let needle: Vec<char> = query.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    // One char scratch reused across every scanned line: a rescan walks the
    // whole scrollback, and a fresh Vec per line dominated the scan.
    let mut scratch: Vec<char> = Vec::new();

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
            if matches.len() >= MATCH_CAP {
                matches.truncate(MATCH_CAP);
                return matches;
            }
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
        if matches.len() >= MATCH_CAP {
            matches.truncate(MATCH_CAP);
            return matches;
        }
    }
    matches.sort_by_key(|item| (item.absolute_row, item.start_col));
    matches.truncate(MATCH_CAP);
    matches
}

fn append_matches(
    line: &str,
    columns: Option<&[usize]>,
    absolute_row: i64,
    needle: &[char],
    scratch: &mut Vec<char>,
    output: &mut Vec<FindMatch>,
) {
    scratch.clear();
    scratch.extend(line.chars());
    let haystack: &[char] = scratch;
    if haystack.len() < needle.len() {
        return;
    }
    let mut index = 0;
    while index + needle.len() <= haystack.len() && output.len() < MATCH_CAP {
        if chars_equal_ci(&haystack[index..index + needle.len()], needle) {
            let start_col = column_for(index, columns);
            let end_col_exclusive = column_past_end(index + needle.len(), columns);
            output.push(FindMatch {
                absolute_row,
                start_col,
                end_col_exclusive,
                text: line.to_owned(),
            });
            index += needle.len();
        } else {
            index += 1;
        }
    }
}

fn chars_equal_ci(haystack: &[char], needle: &[char]) -> bool {
    haystack.iter().zip(needle).all(|(left, right)| {
        // Exact match first: it skips the case-fold on the overwhelmingly
        // common path, including every mismatching position the scan visits.
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

#[cfg(test)]
mod tests {
    use diri_proto::grid::{GridCell, TermColor, TermStyle};

    use super::*;

    fn cell(ch: char) -> GridCell {
        GridCell::new(
            u32::from(ch),
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    }

    fn live_buffer(lines: &[&str], cols: usize) -> GridBuffer {
        let mut buffer = GridBuffer::new(cols as u16, lines.len() as u16);
        for (row_index, line) in lines.iter().enumerate() {
            for (col, ch) in line.chars().take(cols).enumerate() {
                buffer.cells[row_index * cols + col] = cell(ch);
            }
        }
        buffer
    }

    fn snapshot(lines: Vec<String>, alt: bool) -> FindSnapshot {
        FindSnapshot {
            lines,
            first_row: 0,
            visible_start_row: 10,
            cols: 20,
            rows: 3,
            content_seq: 1,
            is_alt_screen: alt,
        }
    }

    fn search(
        model: &mut TerminalFindModel,
        query: &str,
        snapshot: FindSnapshot,
        live: &GridBuffer,
        viewport: &mut ScrollbackViewport,
    ) {
        model.set_query(query, Duration::ZERO);
        let request = model.take_due_search(SEARCH_DEBOUNCE).unwrap();
        assert!(model.apply_snapshot(&request, snapshot, live, viewport));
    }

    #[test]
    fn search_and_rescan_deadlines_are_debounced_and_coalesced() {
        let mut model = TerminalFindModel::default();
        model.set_query("needle", Duration::from_millis(10));
        assert!(model.take_due_search(Duration::from_millis(209)).is_none());
        assert!(
            !model
                .take_due_search(Duration::from_millis(210))
                .unwrap()
                .is_rescan
        );

        model.on_output(Duration::from_millis(300));
        model.on_output(Duration::from_millis(350));
        assert!(model.take_due_search(Duration::from_millis(399)).is_none());
        assert!(
            model
                .take_due_search(Duration::from_millis(400))
                .unwrap()
                .is_rescan
        );
        assert!(model.take_due_search(Duration::from_secs(1)).is_none());
    }

    #[test]
    fn matches_wrap_and_anchor_history_one_third_down() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![], 0, 10, 10, 1, 3);
        let live = live_buffer(&["live needle", "", ""], 20);
        let mut model = TerminalFindModel::default();
        search(
            &mut model,
            "needle",
            snapshot(
                vec![
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    "history needle".to_owned(),
                ],
                false,
            ),
            &live,
            &mut viewport,
        );
        assert_eq!(model.matches().len(), 2);

        assert_eq!(
            model.next(&mut viewport),
            Some(NavigationTarget::History {
                absolute_row: 5,
                anchor: HISTORY_ANCHOR,
            })
        );
        assert_eq!(viewport.view_offset(), 6);
        assert_eq!(viewport.window_row_for_absolute(5), Some(1));
        assert_eq!(model.previous(&mut viewport), Some(NavigationTarget::Live));
        assert_eq!(viewport.view_offset(), 0);
    }

    #[test]
    fn live_match_snaps_to_bottom_and_highlights_use_cell_columns() {
        let mut viewport = ScrollbackViewport::default();
        viewport.apply_rows(vec![], 0, 10, 10, 1, 3);
        viewport.set_view_offset(5, 3);
        let mut live = live_buffer(&["ab", "", ""], 3);
        live.cells[1].scalar = 0;
        live.cells[2] = cell('b');
        let mut model = TerminalFindModel::default();
        search(
            &mut model,
            "b",
            snapshot(Vec::new(), false),
            &live,
            &mut viewport,
        );
        assert_eq!(model.matches()[0].start_col, 2);
        assert_eq!(model.previous(&mut viewport), Some(NavigationTarget::Live));
        assert_eq!(viewport.view_offset(), 0);
        assert_eq!(model.visible_spans(&viewport)[0].row, 0);
    }

    #[test]
    fn match_count_is_capped_and_alt_screen_ignores_history() {
        let lines = (0..600).map(|_| "a".to_owned()).collect::<Vec<_>>();
        let mut viewport = ScrollbackViewport::default();
        let live = live_buffer(&["a", "", ""], 2);
        let mut model = TerminalFindModel::default();
        search(
            &mut model,
            "a",
            FindSnapshot {
                visible_start_row: 600,
                ..snapshot(lines.clone(), false)
            },
            &live,
            &mut viewport,
        );
        assert_eq!(model.matches().len(), MATCH_CAP);

        model.set_query("", Duration::from_secs(1));
        search(
            &mut model,
            "a",
            FindSnapshot {
                visible_start_row: 600,
                ..snapshot(lines, true)
            },
            &live,
            &mut viewport,
        );
        assert_eq!(model.matches().len(), 1);
        assert!(model.is_alt_screen());
    }

    #[test]
    fn matching_is_case_insensitive_and_non_overlapping() {
        let mut viewport = ScrollbackViewport::default();
        let live = live_buffer(&["BaNaNa", "", ""], 8);
        let mut model = TerminalFindModel::default();
        search(
            &mut model,
            "ana",
            snapshot(Vec::new(), false),
            &live,
            &mut viewport,
        );
        assert_eq!(model.matches().len(), 1);
        assert_eq!(
            (
                model.matches()[0].start_col,
                model.matches()[0].end_col_exclusive
            ),
            (1, 4)
        );
    }
}
