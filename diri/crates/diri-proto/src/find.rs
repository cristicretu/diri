//! Terminal Find: the one definition of what a query matches.
//!
//! The app's capture search and the Engine's history search both call
//! [`FindNeedle::for_each_match`], so a match the reading view highlights is
//! exactly a match the Engine reports, wherever in history it sits.
//!
//! Matching is literal, by `char`, and smart-case: a query with an uppercase
//! letter matches case exactly; an all-lowercase query folds case (Unicode
//! lowercase, per character). Matches on a line do not overlap.

use serde::{Deserialize, Serialize};

use crate::SessionId;

/// A compiled query. Building it once per search keeps the per-line scan
/// allocation-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindNeedle {
    chars: Vec<char>,
    case_sensitive: bool,
}

impl FindNeedle {
    /// `None` for an empty query, which matches nothing.
    #[must_use]
    pub fn new(query: &str) -> Option<Self> {
        let chars: Vec<char> = query.chars().collect();
        if chars.is_empty() {
            return None;
        }
        let case_sensitive = chars.iter().any(|ch| ch.is_uppercase());
        Some(Self {
            chars,
            case_sensitive,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    #[must_use]
    pub const fn is_case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// Calls `found(start, end_exclusive)` with `char` indices into
    /// `haystack` for every non-overlapping match, left to right.
    pub fn for_each_match(&self, haystack: &[char], mut found: impl FnMut(usize, usize)) {
        let needle = self.chars.as_slice();
        if haystack.len() < needle.len() {
            return;
        }
        let mut index = 0;
        while index + needle.len() <= haystack.len() {
            if chars_equal(
                &haystack[index..index + needle.len()],
                needle,
                self.case_sensitive,
            ) {
                found(index, index + needle.len());
                index += needle.len();
            } else {
                index += 1;
            }
        }
    }
}

fn chars_equal(haystack: &[char], needle: &[char], case_sensitive: bool) -> bool {
    haystack.iter().zip(needle).all(|(left, right)| {
        // Exact match first skips the fold on the overwhelmingly common
        // path, including every mismatching position the scan visits.
        left == right
            || (!case_sensitive
                && if left.is_ascii() && right.is_ascii() {
                    // Same answer as the Unicode fold below, ~10x cheaper.
                    left.eq_ignore_ascii_case(right)
                } else {
                    // Not only for two non-ASCII chars: KELVIN SIGN folds to
                    // an ASCII 'k'.
                    left.to_lowercase().eq(right.to_lowercase())
                })
    })
}

/// `session.find_history`: one bounded page of a search over everything a
/// local terminal retains, walking from `before_row` toward older output.
///
/// Rows are *stable*: `history_origin + absolute row`, where an absolute row
/// counts from the oldest row the terminal still holds. Output that evicts
/// old history shifts absolute rows but not stable ones, so a client can
/// resume a search, or jump to a match, after the terminal has moved on.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindHistoryParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub query: String,
    /// Stable row to search below (exclusive). Absent: from the newest row,
    /// the visible screen included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_row: Option<i64>,
    /// Width the client's earlier pages were searched at. A different width
    /// means history reflowed, and the request fails with
    /// `find_history_resized` instead of mixing two layouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<i64>,
    /// The page ends at the first row boundary at or past this many matches.
    pub max_matches: u32,
    /// The page ends after about this much scanning (milliseconds).
    pub budget_ms: u32,
}

/// One match: stable row, first column, end column (exclusive).
pub type FindHistoryMatch = [i64; 3];

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindHistoryResult {
    /// Stable row of absolute row 0 when the page ended.
    pub history_origin: i64,
    pub cols: i64,
    pub live_start_row: i64,
    pub total_rows: i64,
    pub content_seq: u64,
    /// Newest first: rows descending, columns ascending within a row.
    pub matches: Vec<FindHistoryMatch>,
    /// Where the next page starts (stable, exclusive); absent once the
    /// oldest retained row has been searched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_row: Option<i64>,
    pub scanned_rows: i64,
}

/// Rows the Engine scans per hold of a terminal's screen lock. Output and
/// input wait at most one chunk for a search in progress.
pub const FIND_HISTORY_CHUNK_ROWS: usize = 128;
/// Most matches one page may carry.
pub const FIND_HISTORY_MAX_PAGE_MATCHES: u32 = 4096;
/// Longest scan one page may ask for.
pub const FIND_HISTORY_MAX_BUDGET_MS: u32 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(query: &str, line: &str) -> Vec<(usize, usize)> {
        let haystack: Vec<char> = line.chars().collect();
        let mut out = Vec::new();
        FindNeedle::new(query)
            .unwrap()
            .for_each_match(&haystack, |start, end| out.push((start, end)));
        out
    }

    #[test]
    fn smart_case_and_non_overlapping() {
        assert_eq!(matches("Error", "Error error ERROR"), vec![(0, 5)]);
        assert_eq!(
            matches("error", "Error error ERROR"),
            vec![(0, 5), (6, 11), (12, 17)]
        );
        assert_eq!(matches("échec", "Échec échec"), vec![(0, 5), (6, 11)]);
        assert_eq!(matches("aa", "aaaa"), vec![(0, 2), (2, 4)]);
        assert_eq!(matches("k", "\u{212A}K"), vec![(0, 1), (1, 2)]);
        assert!(FindNeedle::new("").is_none());
    }

    #[test]
    fn page_wire_shape_is_compact_and_additive() {
        let result = FindHistoryResult {
            history_origin: 7,
            cols: 80,
            live_start_row: 100,
            total_rows: 124,
            content_seq: 3,
            matches: vec![[120, 0, 5]],
            next_before_row: None,
            scanned_rows: 124,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["matches"], serde_json::json!([[120, 0, 5]]));
        assert!(value.get("nextBeforeRow").is_none());
        let params: FindHistoryParams = serde_json::from_value(serde_json::json!({
            "sessionID": "s", "query": "x", "maxMatches": 10, "budgetMs": 5
        }))
        .unwrap();
        assert_eq!(params.before_row, None);
        assert_eq!(params.cols, None);
    }
}
