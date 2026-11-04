//! `search.global`: one search over every session the Engine holds — the
//! text on each terminal's screen and in its retained history, and the
//! messages of each live Claude Code or Codex conversation.
//!
//! The Engine keeps no search index. A request scans the grids it already
//! holds, one bounded run of rows per screen-lock hold (as
//! `session.find_history` does), every session advancing newest row first
//! in turns, and stops at its time budget. What it did not reach comes back
//! as [`SearchCursor`]s: the client streams the rest by sending them as
//! `resume`, and cancels by not asking. Transcripts are read through a small
//! per-file cache keyed by length and mtime. Every bound below is enforced by
//! the Engine whatever a client asks for.
//!
//! A row matches when every whitespace-separated word of the query is on it,
//! each matched as Find matches it: literal and smart-case
//! ([`crate::find::FindNeedle`]). A conversation message matches the same way.

use serde::{Deserialize, Serialize};

use crate::SessionId;

/// Most sessions one reply carries screen hits for.
pub const SEARCH_MAX_SESSIONS: u32 = 64;
/// Most matching lines one session's screen hit carries.
pub const SEARCH_MAX_LINES_PER_SESSION: u32 = 8;
/// Longest scan one request may ask for.
pub const SEARCH_MAX_BUDGET_MS: u32 = 250;
/// Rows of context around a screen hit's newest line, each side.
pub const SEARCH_CONTEXT_ROWS: usize = 2;
/// Longest line or message excerpt a reply carries, in `char`s.
pub const SEARCH_MAX_EXCERPT_CHARS: usize = 240;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GlobalSearchParams {
    /// Whitespace-separated words, all of which must match. An empty query
    /// searches nothing but still warms the transcript cache when
    /// `transcripts` is set, so the first real query does not pay for it.
    pub query: String,
    /// Sessions with screen hits to return (most recent output first).
    pub max_sessions: u32,
    /// Matching lines per session, newest first.
    pub max_lines_per_session: u32,
    /// The scan stops after about this long; what it did not reach is
    /// reported in [`GlobalSearchResult::incomplete`].
    pub budget_ms: u32,
    /// Search terminal screens and history. Off for a request that only
    /// re-reads conversations once the transcript cache has caught up.
    #[serde(default = "default_true")]
    pub screens: bool,
    /// Also search Claude Code and Codex conversations. Ignored with
    /// `resume`: a continuation covers screens only.
    #[serde(default)]
    pub transcripts: bool,
    /// Continue an earlier reply's search: only these sessions, each below
    /// its row. Empty: a new search from every session's newest row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume: Vec<SearchCursor>,
}

const fn default_true() -> bool {
    true
}

/// Where a session's scan stopped: its next row to search is below
/// `before_row` (a stable row, exclusive).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchCursor {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub before_row: i64,
}

/// One matching line, with `char` ranges of the matched words.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchLine {
    /// Stable row (`history_origin + absolute row`, as in
    /// `session.find_history`); absent for context lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<i64>,
    pub text: String,
    /// `[start, end)` in `char`s of `text`, ascending, non-overlapping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<[u32; 2]>,
}

/// One session's matching screen and history lines.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScreenHit {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// Matching lines, newest first.
    pub lines: Vec<SearchLine>,
    /// Lines around the newest match this reply found, oldest first; the
    /// match is the one with ranges.
    pub context: Vec<SearchLine>,
    /// More matching lines exist than `lines` carries.
    pub more: bool,
    /// Only the mirrored visible screen was searched: a remote session,
    /// whose history lives on its host.
    #[serde(default)]
    pub screen_only: bool,
}

/// One conversation's best matching message.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConversationHit {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// `user` or `agent`.
    pub role: String,
    /// An excerpt of the message around the first match.
    pub excerpt: SearchLine,
    /// Messages after this one in the conversation: 0 is the latest.
    pub messages_ago: u32,
    /// Further messages in this conversation that also match.
    pub more: u32,
}

/// What a reply did not cover.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchIncomplete {
    /// Sessions whose older history the time budget left unsearched, and
    /// where to continue (`GlobalSearchParams::resume`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume: Vec<SearchCursor>,
    /// Conversations not yet read into the cache; asking again shortly
    /// finds them.
    #[serde(default)]
    pub transcripts_pending: u32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GlobalSearchResult {
    pub screens: Vec<ScreenHit>,
    pub conversations: Vec<ConversationHit>,
    pub incomplete: SearchIncomplete,
    /// Terminals searched and the rows scanned in them.
    pub sessions_searched: u32,
    pub rows_scanned: u64,
    pub elapsed_ms: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_use_camel_case_and_default_transcripts_off() {
        let params: GlobalSearchParams = serde_json::from_value(serde_json::json!({
            "query": "panic",
            "maxSessions": 10,
            "maxLinesPerSession": 3,
            "budgetMs": 80,
        }))
        .unwrap();
        assert!(!params.transcripts);
        assert!(params.screens);
        assert!(params.resume.is_empty());
        let hit = serde_json::to_value(ScreenHit {
            session_id: SessionId("s".into()),
            lines: vec![SearchLine {
                row: Some(4),
                text: "panic".into(),
                ranges: vec![[0, 5]],
            }],
            context: Vec::new(),
            more: false,
            screen_only: false,
        })
        .unwrap();
        assert_eq!(hit["sessionID"], "s");
        assert_eq!(hit["lines"][0]["ranges"][0][1], 5);
    }
}
