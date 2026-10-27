//! Matches older than the retained capture, found by the Engine.
//!
//! The capture holds the newest few thousand rows with their styled cells, so
//! its matches can be painted from it. Everything older is searched by the
//! Engine (`session.find_history`) one bounded page at a time, newest first,
//! and kept here as positions only. Rows are stable (`history_origin +
//! absolute row`), so output that evicts old history between pages, or while
//! the reader is looking, does not move a match.

use diri_proto::SessionId;
use diri_proto::find::{FindHistoryParams, FindHistoryResult};

use super::FindMatch;

/// Most older matches kept; past it the search stops and says it is partial.
pub const OLDER_MATCH_CAP: usize = 10_000;
/// The first page is short so the first older matches arrive quickly.
const FIRST_PAGE_BUDGET_MS: u32 = 8;
const FIRST_PAGE_MATCHES: u32 = 1;
const PAGE_BUDGET_MS: u32 = 40;
const PAGE_MATCHES: u32 = 2048;

/// One page to ask the Engine for. It answers the search it was taken from
/// only: a changed query or a new search generation discards the reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OlderRequest {
    pub(super) generation: u64,
    pub(super) query: String,
    pub(super) before_row: i64,
    /// Lower bound (stable, inclusive) of a gap scan; see [`OlderSearch::rebase`].
    pub(super) gap_floor: Option<i64>,
    pub(super) cols: i64,
    first: bool,
    rebases: u64,
}

impl OlderRequest {
    #[must_use]
    pub fn params(&self, session_id: SessionId) -> FindHistoryParams {
        FindHistoryParams {
            session_id,
            query: self.query.clone(),
            before_row: Some(self.before_row),
            cols: Some(self.cols),
            max_matches: if self.first {
                FIRST_PAGE_MATCHES
            } else {
                PAGE_MATCHES
            },
            budget_ms: if self.first {
                FIRST_PAGE_BUDGET_MS
            } else {
                PAGE_BUDGET_MS
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Rows from `cursor` down still need searching.
    Searching,
    /// Every retained row older than the capture has been searched.
    Done,
    /// Stopped at [`OLDER_MATCH_CAP`], or the Engine cannot search history.
    Stopped,
}

#[derive(Clone, Debug)]
pub(super) struct OlderSearch {
    /// Ascending stable rows; every one below `top`.
    pub(super) matches: Vec<FindMatch>,
    /// Stable row of the capture's first row: rows below it, down to
    /// `cursor`, have been searched.
    top: i64,
    cursor: i64,
    /// Rows `[floor, ceiling)` that left the capture when a newer one
    /// replaced it, searched before the main scan continues.
    gap: Option<(i64, i64)>,
    phase: Phase,
    cols: i64,
    in_flight: bool,
    first_sent: bool,
    /// Bumped by [`Self::rebase`]: a gap page in flight across one searched
    /// a gap that has since grown, so its reply is dropped and the gap asked
    /// for again whole.
    rebases: u64,
    /// From the latest page: maps stable rows to the Engine's current
    /// absolute rows, and places a reading view on them.
    pub(super) origin: i64,
    pub(super) live_start_row: i64,
    pub(super) total_rows: i64,
    pub(super) content_seq: u64,
}

impl OlderSearch {
    /// A search below a capture whose first row is the stable row `top`.
    pub(super) fn new(top: i64, cols: i64, origin: i64) -> Self {
        Self {
            matches: Vec::new(),
            top,
            cursor: top,
            gap: None,
            phase: if top > origin {
                Phase::Searching
            } else {
                Phase::Done
            },
            cols,
            in_flight: false,
            first_sent: false,
            rebases: 0,
            origin,
            live_start_row: 0,
            total_rows: 0,
            content_seq: 0,
        }
    }

    pub(super) fn is_complete(&self) -> bool {
        self.phase == Phase::Done && self.gap.is_none()
    }

    pub(super) fn cols(&self) -> i64 {
        self.cols
    }

    pub(super) fn release(&mut self) {
        self.in_flight = false;
    }

    pub(super) fn stop(&mut self) {
        self.phase = Phase::Stopped;
        self.gap = None;
        self.in_flight = false;
    }

    /// A newer capture of the same width replaced the one this search sits
    /// under. Keeps what was found and searches only the rows between the
    /// two captures. Returns how many matches were dropped from the front
    /// (evicted) so the caller can keep its selection.
    pub(super) fn rebase(&mut self, top: i64, origin: i64) -> usize {
        let evicted = self
            .matches
            .partition_point(|item| item.absolute_row < origin);
        self.matches.drain(..evicted);
        if top > self.top {
            let floor = self.gap.map_or(self.top, |(floor, _)| floor);
            self.gap = Some((floor, top));
        } else if top < self.top {
            // The new capture reaches further back: what it holds is its own.
            let keep = self.matches.partition_point(|item| item.absolute_row < top);
            self.matches.truncate(keep);
            self.cursor = self.cursor.min(top);
        }
        self.top = top;
        self.origin = self.origin.max(origin);
        self.rebases += 1;
        evicted
    }

    /// The next page to ask for, marking it in flight; none while one is.
    pub(super) fn next_request(&mut self, generation: u64, query: &str) -> Option<OlderRequest> {
        if self.in_flight || self.matches.len() >= OLDER_MATCH_CAP {
            return None;
        }
        let (before_row, gap_floor) = match (self.gap, self.phase) {
            (Some((floor, ceiling)), _) => (ceiling, Some(floor)),
            (None, Phase::Searching) => (self.cursor, None),
            _ => return None,
        };
        let first = !self.first_sent;
        self.first_sent = true;
        self.in_flight = true;
        Some(OlderRequest {
            generation,
            query: query.to_owned(),
            before_row,
            gap_floor,
            cols: self.cols,
            first,
            rebases: self.rebases,
        })
    }

    /// Takes a page in. Returns where its matches were inserted and how many,
    /// so the caller can keep its selection on the same match.
    pub(super) fn apply(
        &mut self,
        request: &OlderRequest,
        page: Result<FindHistoryResult, ()>,
    ) -> (usize, usize) {
        self.in_flight = false;
        let Ok(page) = page else {
            self.stop();
            return (0, 0);
        };
        if page.cols != self.cols {
            self.stop();
            return (0, 0);
        }
        self.origin = page.history_origin;
        self.live_start_row = page.live_start_row;
        self.total_rows = page.total_rows;
        self.content_seq = page.content_seq;
        if request.gap_floor.is_some() && request.rebases != self.rebases {
            return (0, 0);
        }
        let floor = request.gap_floor.unwrap_or(i64::MIN);
        let top = self.top;
        // Newest first from the Engine; kept ascending here.
        let found: Vec<FindMatch> = page
            .matches
            .iter()
            .rev()
            .filter(|[row, ..]| *row >= floor && *row < top)
            .map(|&[row, start, end]| FindMatch {
                absolute_row: row,
                start_col: usize::try_from(start).unwrap_or_default(),
                end_col_exclusive: usize::try_from(end).unwrap_or_default(),
            })
            .collect();
        let next = page.next_before_row;
        let (at, count);
        if let Some(gap_floor) = request.gap_floor {
            // Gap rows sit above everything found before them.
            at = self
                .matches
                .partition_point(|item| item.absolute_row < gap_floor);
            count = found.len();
            self.matches.splice(at..at, found);
            match next {
                Some(next) if next > gap_floor => {
                    self.gap = self.gap.map(|(floor, _)| (floor, next));
                }
                _ => self.gap = None,
            }
        } else {
            at = 0;
            count = found.len();
            self.matches.splice(0..0, found);
            match next {
                Some(next) => self.cursor = next.min(self.cursor),
                None if self.phase == Phase::Searching => self.phase = Phase::Done,
                None => {}
            }
        }
        if self.matches.len() >= OLDER_MATCH_CAP {
            // Keep the newest: the ones nearest what the reader can see.
            let excess = self.matches.len() - OLDER_MATCH_CAP;
            self.matches.drain(..excess);
            self.phase = Phase::Stopped;
            return (at.saturating_sub(excess), count.saturating_sub(excess));
        }
        (at, count)
    }

    /// The Engine's current absolute row for a stable row.
    pub(super) fn absolute(&self, stable: i64) -> i64 {
        stable - self.origin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(origin: i64, rows: &[i64], next: Option<i64>) -> Result<FindHistoryResult, ()> {
        Ok(FindHistoryResult {
            history_origin: origin,
            cols: 80,
            live_start_row: 1000,
            total_rows: 1024,
            content_seq: 1,
            matches: rows.iter().map(|row| [*row, 0, 1]).collect(),
            next_before_row: next,
            scanned_rows: 0,
        })
    }

    fn rows(search: &OlderSearch) -> Vec<i64> {
        search
            .matches
            .iter()
            .map(|item| item.absolute_row)
            .collect()
    }

    #[test]
    fn pages_prepend_older_matches_until_the_oldest_row() {
        let mut search = OlderSearch::new(500, 80, 0);
        let first = search.next_request(1, "x").unwrap();
        assert_eq!(first.before_row, 500);
        assert!(first.first);
        assert!(search.next_request(1, "x").is_none(), "one page in flight");
        assert_eq!(
            search.apply(&first, page(0, &[450, 400], Some(300))),
            (0, 2)
        );
        let second = search.next_request(1, "x").unwrap();
        assert_eq!((second.before_row, second.first), (300, false));
        assert_eq!(search.apply(&second, page(0, &[10], None)), (0, 1));
        assert_eq!(rows(&search), vec![10, 400, 450]);
        assert!(search.is_complete());
        assert!(search.next_request(1, "x").is_none());
    }

    #[test]
    fn a_newer_capture_searches_only_the_rows_between_the_two() {
        let mut search = OlderSearch::new(500, 80, 0);
        let request = search.next_request(1, "x").unwrap();
        search.apply(&request, page(0, &[450, 100], None));
        // Output moved the capture 200 rows newer and evicted 50 rows.
        assert_eq!(search.rebase(700, 50), 0);
        let gap = search.next_request(2, "x").unwrap();
        assert_eq!((gap.before_row, gap.gap_floor), (700, Some(500)));
        // Rows below the floor were searched already and are not added twice.
        assert_eq!(search.apply(&gap, page(50, &[650, 520, 450], None)), (2, 2));
        assert_eq!(rows(&search), vec![100, 450, 520, 650]);
        assert!(search.is_complete());
        // A gap page in flight while the capture moves again is asked again.
        assert_eq!(search.rebase(800, 50), 0);
        let stale = search.next_request(3, "x").unwrap();
        assert_eq!((stale.before_row, stale.gap_floor), (800, Some(700)));
        search.rebase(900, 50);
        assert_eq!(search.apply(&stale, page(50, &[750], None)), (0, 0));
        let whole = search.next_request(3, "x").unwrap();
        assert_eq!((whole.before_row, whole.gap_floor), (900, Some(700)));
        search.apply(&whole, page(50, &[850, 750], None));
        assert_eq!(rows(&search), vec![100, 450, 520, 650, 750, 850]);
        // Further eviction drops matches that are gone.
        assert_eq!(search.rebase(900, 200), 1);
        assert_eq!(rows(&search), vec![450, 520, 650, 750, 850]);
    }

    #[test]
    fn a_failed_or_resized_page_stops_the_search() {
        let mut search = OlderSearch::new(500, 80, 0);
        let request = search.next_request(1, "x").unwrap();
        search.apply(&request, Err(()));
        assert!(!search.is_complete());
        assert!(search.next_request(1, "x").is_none());

        let mut search = OlderSearch::new(500, 80, 0);
        let request = search.next_request(1, "x").unwrap();
        let mut resized = page(0, &[1], None).unwrap();
        resized.cols = 100;
        search.apply(&request, Ok(resized));
        assert!(search.matches.is_empty());
        assert!(!search.is_complete());
    }

    #[test]
    fn the_cap_keeps_the_newest_older_matches() {
        let mut search = OlderSearch::new(1_000_000, 80, 0);
        let request = search.next_request(1, "x").unwrap();
        let many: Vec<i64> = (0..OLDER_MATCH_CAP as i64 + 5).rev().collect();
        search.apply(&request, page(0, &many, Some(1)));
        assert_eq!(search.matches.len(), OLDER_MATCH_CAP);
        assert_eq!(search.matches[0].absolute_row, 5);
        assert!(search.next_request(1, "x").is_none());
        assert!(!search.is_complete());
    }

    #[test]
    fn nothing_older_than_the_capture_is_complete_at_once() {
        let mut search = OlderSearch::new(0, 80, 0);
        assert!(search.is_complete());
        assert!(search.next_request(1, "x").is_none());
    }
}
