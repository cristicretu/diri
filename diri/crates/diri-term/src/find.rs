//! Debounced, capped find over daemon history plus the authoritative live grid.

use std::time::Duration;

use diri_proto::methods::ReadScrollbackResult;

use crate::buffer::GridBuffer;
use crate::scrollback::ScrollbackViewport;

mod scheduler;
mod search;

pub use scheduler::{FindSearchScheduler, ReadCompletion, ScanCompletion};
pub use search::{SearchJob, SearchResult};

pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
pub const OUTPUT_RESCAN_DELAY: Duration = Duration::from_millis(100);
pub const MATCH_CAP: usize = 500;
pub const HISTORY_ANCHOR: f32 = 0.33;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FindMatch {
    pub absolute_row: i64,
    pub start_col: usize,
    pub end_col_exclusive: usize,
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
        if self.query.is_empty() {
            return false;
        }
        // Content is part of a search generation. Any job that captured the
        // previous live grid must not overwrite a newer screen when it returns.
        self.generation = self.generation.wrapping_add(1);
        // A not-yet-started debounced query search will capture the newest
        // generation, so arming a second earlier rescan would only defeat the
        // query debounce and duplicate its work.
        if self.search_due.is_some() || self.rescan_due.is_some() {
            return false;
        }
        self.rescan_due = Some(now.saturating_add(OUTPUT_RESCAN_DELAY));
        true
    }

    /// Pulls one due request. The app asynchronously reads a scrollback text
    /// snapshot and submits it through [`Self::prepare_search`].
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

    /// Validates an asynchronously-read history snapshot and captures the live
    /// grid with a short clone. [`SearchJob::run`] owns the expensive scan and
    /// must be dispatched to a background executor.
    #[must_use]
    pub fn prepare_search(
        &self,
        request: &SearchRequest,
        snapshot: FindSnapshot,
        live: &GridBuffer,
    ) -> Option<SearchJob> {
        self.is_current(request)
            .then(|| SearchJob::new(request.clone(), snapshot, live.clone()))
    }

    /// Discards stale background results and preserves the current index only
    /// for a same-geometry/content rescan.
    pub fn apply_result(
        &mut self,
        result: SearchResult,
        viewport: &mut ScrollbackViewport,
    ) -> bool {
        if !self.is_current(&result.request) {
            return false;
        }
        let sequence_changed = self.cached_content_seq != Some(result.content_seq)
            || self.cached_cols != Some(result.cols);
        self.matches = result.matches;
        self.is_alt_screen = result.is_alt_screen;
        self.cached_visible_start_row = Some(result.visible_start_row);
        self.cached_rows = usize::try_from(result.rows.max(0)).unwrap_or(usize::MAX);
        self.cached_content_seq = Some(result.content_seq);
        self.cached_cols = Some(result.cols);
        viewport.apply_geometry(
            result.visible_start_row,
            result.visible_start_row.max(0),
            result.content_seq,
            self.cached_rows,
        );

        if result.request.is_rescan && !sequence_changed {
            self.current_index = self.current_index.min(self.matches.len().saturating_sub(1));
        } else {
            let window_top = result
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

    fn is_current(&self, request: &SearchRequest) -> bool {
        request.generation == self.generation && request.query == self.query
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
        let job = model.prepare_search(&request, snapshot, live).unwrap();
        assert!(model.apply_result(job.run(), viewport));
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
        assert_eq!(model.matches().first().unwrap().absolute_row, 101);
        assert_eq!(model.matches().last().unwrap().absolute_row, 600);

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
    fn matching_is_utf8_case_insensitive_and_non_overlapping() {
        let mut viewport = ScrollbackViewport::default();
        let live = live_buffer(&["CAFÉ BaNaNa", "", ""], 16);
        let mut model = TerminalFindModel::default();
        search(
            &mut model,
            "café",
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
            (0, 4)
        );

        model.set_query("", Duration::from_secs(1));
        search(
            &mut model,
            "ana",
            snapshot(Vec::new(), false),
            &live,
            &mut viewport,
        );
        assert_eq!(model.matches().len(), 1);
        assert_eq!(model.matches()[0].start_col, 6);
    }

    #[test]
    fn output_invalidates_an_in_flight_search() {
        let mut model = TerminalFindModel::default();
        model.set_query("needle", Duration::ZERO);
        let request = model.take_due_search(SEARCH_DEBOUNCE).unwrap();
        let old_live = live_buffer(&["old needle", "", ""], 20);
        let old_job = model
            .prepare_search(&request, snapshot(Vec::new(), false), &old_live)
            .unwrap();
        assert!(model.on_output(Duration::from_secs(1)));

        let new_request = model
            .take_due_search(Duration::from_secs(1) + OUTPUT_RESCAN_DELAY)
            .unwrap();
        let new_live = live_buffer(&["needle", "", ""], 20);
        let new_job = model
            .prepare_search(
                &new_request,
                FindSnapshot {
                    content_seq: 2,
                    ..snapshot(Vec::new(), false)
                },
                &new_live,
            )
            .unwrap();
        let mut viewport = ScrollbackViewport::default();
        assert!(model.apply_result(new_job.run(), &mut viewport));
        assert_eq!(model.matches()[0].start_col, 0);

        assert!(!model.apply_result(old_job.run(), &mut viewport));
        assert_eq!(model.matches()[0].start_col, 0);
    }

    #[test]
    fn stale_query_result_cannot_overwrite_the_current_result() {
        let mut model = TerminalFindModel::default();
        let live = live_buffer(&["old fresh", "", ""], 20);

        model.set_query("old", Duration::ZERO);
        let old_request = model.take_due_search(SEARCH_DEBOUNCE).unwrap();
        let old_job = model
            .prepare_search(&old_request, snapshot(Vec::new(), false), &live)
            .unwrap();

        model.set_query("fresh", Duration::from_secs(1));
        let fresh_request = model
            .take_due_search(Duration::from_secs(1) + SEARCH_DEBOUNCE)
            .unwrap();
        let fresh_job = model
            .prepare_search(&fresh_request, snapshot(Vec::new(), false), &live)
            .unwrap();
        let mut viewport = ScrollbackViewport::default();
        assert!(model.apply_result(fresh_job.run(), &mut viewport));
        assert_eq!(model.matches()[0].start_col, 4);

        assert!(!model.apply_result(old_job.run(), &mut viewport));
        assert_eq!(model.matches()[0].start_col, 4);
    }
}
