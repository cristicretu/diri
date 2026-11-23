//! Structured thread observation for the Dual-Lens GUI.
//!
//! One PTY is the truth. A structured Agent (Claude Code, Codex) also writes
//! its own JSONL transcript; the Engine tails that file by byte offset and
//! returns normalized [`ThreadItem`]s. The GUI Lens renders them while the
//! TUI Lens keeps painting the same live Holder, so flipping lenses never
//! reconnects or replays anything.
//!
//! Identity is positional: an item's [`ItemKey`] is the byte offset of the
//! JSONL record that produced it plus its index inside that record. Reading
//! the same bytes twice therefore yields the same keys, and a client that
//! merges by key is idempotent under reconnect, Engine restart and replay
//! (`diri/formal/DualLens.lean` proves the model).
//!
//! Nothing here is written to an Agent. Observation adds zero bytes to any
//! prompt, so it cannot move a provider's prompt-cache prefix.

use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, SessionId};

/// Major version of the thread item stream. Additive fields keep it; a
/// renamed or removed field bumps it and clients fall back to the TUI Lens.
pub const THREAD_WIRE_VERSION: u16 = 1;

/// Bytes the Engine reads per `session.thread_read` unless the caller asks
/// for fewer. A larger backlog is paged with `more`.
pub const THREAD_READ_DEFAULT_BYTES: u32 = 2 * 1024 * 1024;
/// Hard cap on one read's window.
pub const THREAD_READ_MAX_BYTES: u32 = 8 * 1024 * 1024;
/// Inline image bytes one read may carry. Larger images are elided with
/// their size so the composer can still show a placeholder.
pub const THREAD_IMAGE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Where an item came from: the record's first byte, then its index among
/// the items that record produced.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ItemKey {
    pub offset: u64,
    pub part: u16,
}

/// A position in one transcript file. `generation` names the file's
/// identity (device, inode and path); a different generation means the
/// offset belongs to another file and the reader starts over.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadCursor {
    #[serde(default)]
    pub generation: String,
    #[serde(default)]
    pub offset: u64,
}

/// Token accounting the provider recorded for one request. Never summed
/// across requests here; the warmth indicator only needs the latest.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TurnUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Milliseconds since the Unix epoch at which the provider answered.
    #[serde(default)]
    pub at_ms: i64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// The part of `cache_write` written with a one-hour TTL.
    #[serde(default)]
    pub cache_write_1h: u64,
    /// The model's context window, when the provider reports it (Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// A picture attached by the user or returned by a tool.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadImage {
    pub media_type: String,
    /// Decoded size in bytes.
    pub bytes: u64,
    /// Base64 payload, absent when the image exceeded the per-read budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

/// One replaced span inside a file edit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EditHunk {
    pub old: String,
    pub new: String,
}

/// What a tool call did, in the few shapes the GUI renders specially.
/// Everything else is `Other` with a bounded preview of its input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ToolDetail {
    Command {
        command: String,
    },
    Read {
        path: String,
    },
    Search {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Edit {
        path: String,
        hunks: Vec<EditHunk>,
    },
    Write {
        path: String,
        content: String,
    },
    /// A unified or Codex `apply_patch` body.
    Patch {
        patch: String,
    },
    /// A subagent or delegated task.
    Task {
        description: String,
    },
    Other {
        input: String,
    },
}

impl ToolDetail {
    /// The class a Work Strip counts this call under.
    pub fn work(&self) -> WorkKind {
        match self {
            Self::Read { .. } | Self::Search { .. } => WorkKind::Explore,
            Self::Command { .. } => WorkKind::Run,
            Self::Edit { .. } | Self::Write { .. } | Self::Patch { .. } => WorkKind::Edit,
            Self::Task { .. } => WorkKind::Delegate,
            Self::Other { .. } => WorkKind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkKind {
    Explore,
    Edit,
    Run,
    Delegate,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ThreadBody {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ThreadImage>,
    },
    /// A slash command the user ran (`/model`, `/compact`), shown as a chip.
    Command {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        args: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        detail: ToolDetail,
    },
    ToolResult {
        call_id: String,
        #[serde(default)]
        is_error: bool,
        preview: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ThreadImage>,
    },
    /// What a CLI-local command (`/model`, `/cost`, `/status`) printed for
    /// the user. Claude records it beside the command; the model never saw it.
    CommandOutput {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// The CLI's permission mode changed (Claude: `default`, `acceptEdits`,
    /// `plan`, `auto`, `bypassPermissions`).
    Mode {
        mode: String,
    },
    /// Provider accounting for one request; folded into warmth, not drawn.
    Usage(TurnUsage),
    /// Context compaction, an interrupted turn, a model change.
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadItem {
    pub key: ItemKey,
    /// Milliseconds since the Unix epoch, when the provider recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<i64>,
    pub body: ThreadBody,
}

/// One stretch of a Diri thread run by one Agent, model and account.
///
/// A thread is an ordered chain of segments. Tier 1 (a model switch inside
/// the CLI) and Tier 2 (same Agent, another account) keep the session and
/// the transcript bytes, so their segments differ only in `model` or
/// `account` and start at the byte offset where the change happened.
/// Tier 3 (another Agent) adds a segment with its own `session_id` and
/// transcript; the previous segment's Holder can stay parked so returning
/// to it reuses its prompt-cache prefix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadSegment {
    pub index: u32,
    pub kind: AgentKind,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Account profile display name; never a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// First transcript byte this segment owns.
    pub start_offset: u64,
    #[serde(default)]
    pub started_at_ms: i64,
    #[serde(default)]
    pub cause: SegmentCause,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SegmentCause {
    #[default]
    Start,
    /// Tier 1: the CLI switched model in place.
    Model,
    /// Tier 2: the same Agent continued under another account.
    Account,
    /// Tier 3: another Agent continued from a Handoff Capsule.
    Handoff,
}

/// `session.thread_read`: the structured items after `from`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadReadParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(default)]
    pub from: ThreadCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
    /// Keep a file watch on the transcript so appends publish
    /// `session.thread_appended`. Renewed by every read that asks for it.
    #[serde(default)]
    pub watch: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThreadReadResult {
    pub version: u16,
    /// The session's Agent writes a transcript Diri can read. When false the
    /// client stays in the TUI Lens and `reason` says why.
    pub structured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The caller's cursor named another file or a position that is no
    /// longer a record boundary; `items` start from the beginning and the
    /// client must drop what it had.
    #[serde(default)]
    pub reset: bool,
    /// Bytes remain after `next`; read again without waiting.
    #[serde(default)]
    pub more: bool,
    pub next: ThreadCursor,
    pub items: Vec<ThreadItem>,
    pub segments: Vec<ThreadSegment>,
}

/// `session.thread_appended` event params.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadAppendedEvent {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub length: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BlockerChoice {
    Approve,
    Deny,
}

/// `session.answer_blocker`: answer the permission prompt the caller saw.
///
/// The Engine writes the Agent manifest's approve or deny keys only while
/// the session still shows that very prompt (`occurredAt` matches the live
/// `needsInput`). A prompt that changed or cleared answers
/// `blocker_changed` and nothing reaches the PTY.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AnswerBlockerParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub occurred_at: f64,
    pub choice: BlockerChoice,
}

/// `session.handoff_capsule`: the deterministic Handoff Capsule another
/// Agent would start from (Tier 3). Reading it changes nothing: the source
/// session keeps running, or stays parked, with its prompt cache intact.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HandoffCapsuleParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// A pinned goal or note text to lead the capsule with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HandoffCapsuleResult {
    /// Markdown for the receiving Agent's first prompt.
    pub capsule: String,
    pub touched_files: Vec<String>,
}

/// How long a prompt-cache entry written with `tier` stays readable.
pub const CACHE_TTL_5M_MS: i64 = 5 * 60 * 1000;
pub const CACHE_TTL_1H_MS: i64 = 60 * 60 * 1000;

/// What the next request to `model` should expect from the provider cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheWarmth {
    /// A request to the same model within the TTL reads the prefix.
    Warm { remaining_ms: i64, ttl_ms: i64 },
    /// The prefix expired, or this model has not been asked in this thread:
    /// the next request writes the cache from scratch.
    Cold { ever: bool },
}

/// Warmth for the next request to `model`, from the usage records seen so
/// far (oldest first). Provider caches are per model, so a switch to a
/// model that last ran an hour ago is cold even when the thread is busy.
///
/// The TTL follows the last write tier observed for that model: a one-hour
/// write anywhere in its latest request means the hour TTL; otherwise the
/// default five minutes. Every read or write refreshes the TTL.
pub fn cache_warmth<'a>(
    usages: impl DoubleEndedIterator<Item = &'a TurnUsage>,
    model: &str,
    now_ms: i64,
) -> CacheWarmth {
    let Some(last) = usages
        .rev()
        .find(|usage| usage.model.as_deref().is_some_and(|m| same_model(m, model)))
    else {
        return CacheWarmth::Cold { ever: false };
    };
    let ttl_ms = if last.cache_write_1h > 0 {
        CACHE_TTL_1H_MS
    } else if last.cache_write > 0 || last.cache_read > 0 {
        CACHE_TTL_5M_MS
    } else {
        return CacheWarmth::Cold { ever: true };
    };
    let remaining_ms = last.at_ms.saturating_add(ttl_ms).saturating_sub(now_ms);
    if remaining_ms > 0 {
        CacheWarmth::Warm {
            remaining_ms: remaining_ms.min(ttl_ms),
            ttl_ms,
        }
    } else {
        CacheWarmth::Cold { ever: true }
    }
}

/// Model ids differ by date suffix and context tag (`[1m]`); the cache
/// cannot be shared across families, but those spellings name one model.
pub fn same_model(a: &str, b: &str) -> bool {
    fn base(id: &str) -> &str {
        let id = id.split('[').next().unwrap_or(id);
        // claude-sonnet-5-5-20261001 → claude-sonnet-5-5
        match id.rsplit_once('-') {
            Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => {
                head
            }
            _ => id,
        }
    }
    base(a).eq_ignore_ascii_case(base(b))
}

/// The rendered segment chain: the Engine's stored boundaries (start,
/// account and Agent handoffs) plus a Tier 1 boundary wherever the model of
/// consecutive requests changes within a stored segment. Pure and
/// deterministic, so every client draws the same dividers.
pub fn segment_chain(stored: &[ThreadSegment], items: &[ThreadItem]) -> Vec<ThreadSegment> {
    let mut chain: Vec<ThreadSegment> = Vec::with_capacity(stored.len() + 2);
    let mut stored_iter = stored.iter().peekable();
    let mut current_model: Option<String> = None;
    let push_stored =
        |chain: &mut Vec<ThreadSegment>, segment: &ThreadSegment, model: &Option<String>| {
            let mut segment = segment.clone();
            if segment.model.is_none() {
                segment.model = model.clone();
            }
            chain.push(segment);
        };
    if let Some(first) = stored_iter.next() {
        push_stored(&mut chain, first, &None);
    }
    for item in items {
        while let Some(next) = stored_iter.peek()
            && next.start_offset <= item.key.offset
        {
            push_stored(&mut chain, next, &current_model);
            stored_iter.next();
        }
        let ThreadBody::Usage(usage) = &item.body else {
            continue;
        };
        let Some(model) = usage.model.as_deref() else {
            continue;
        };
        let Some(last) = chain.last_mut() else {
            continue;
        };
        match last.model.as_deref() {
            None => last.model = Some(model.to_owned()),
            Some(previous) if same_model(previous, model) => {}
            Some(_) => {
                let mut next = last.clone();
                next.model = Some(model.to_owned());
                next.start_offset = item.key.offset;
                next.started_at_ms = usage.at_ms;
                next.cause = SegmentCause::Model;
                chain.push(next);
            }
        }
        current_model = Some(model.to_owned());
    }
    for rest in stored_iter {
        push_stored(&mut chain, rest, &current_model);
    }
    for (index, segment) in chain.iter_mut().enumerate() {
        segment.index = u32::try_from(index).unwrap_or(u32::MAX);
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(model: &str, at_ms: i64, write: u64, write_1h: u64, read: u64) -> TurnUsage {
        TurnUsage {
            model: Some(model.into()),
            at_ms,
            cache_write: write,
            cache_write_1h: write_1h,
            cache_read: read,
            ..TurnUsage::default()
        }
    }

    #[test]
    fn warmth_follows_the_latest_request_to_the_same_model() {
        let usages = [
            usage("claude-opus-5-5", 0, 40_000, 40_000, 0),
            usage("claude-sonnet-5-5", 60_000, 9_000, 0, 0),
        ];
        assert_eq!(
            cache_warmth(usages.iter(), "claude-opus-5-5", 252_000),
            CacheWarmth::Warm {
                remaining_ms: CACHE_TTL_1H_MS - 252_000,
                ttl_ms: CACHE_TTL_1H_MS
            }
        );
        assert_eq!(
            cache_warmth(usages.iter(), "claude-sonnet-5-5", 60_000 + CACHE_TTL_5M_MS),
            CacheWarmth::Cold { ever: true }
        );
        assert_eq!(
            cache_warmth(usages.iter(), "claude-haiku-5-5", 0),
            CacheWarmth::Cold { ever: false }
        );
    }

    #[test]
    fn a_request_without_cache_traffic_is_cold() {
        let usages = [usage("gpt-5.5", 0, 0, 0, 0)];
        assert_eq!(
            cache_warmth(usages.iter(), "gpt-5.5", 1),
            CacheWarmth::Cold { ever: true }
        );
    }

    #[test]
    fn model_spellings_collapse_dates_and_context_tags() {
        assert!(same_model(
            "claude-sonnet-5-5-20261001",
            "claude-sonnet-5-5[1m]"
        ));
        assert!(!same_model("claude-sonnet-5-5", "claude-opus-5-5"));
    }

    #[test]
    fn segment_chain_adds_model_boundaries_inside_stored_segments() {
        let start = ThreadSegment {
            index: 0,
            kind: AgentKind::CLAUDE_CODE,
            session_id: SessionId("s".into()),
            model: None,
            account: Some("Personal".into()),
            start_offset: 0,
            started_at_ms: 0,
            cause: SegmentCause::Start,
        };
        let account = ThreadSegment {
            account: Some("Work".into()),
            start_offset: 300,
            cause: SegmentCause::Account,
            ..start.clone()
        };
        let usage_at = |offset: u64, model: &str| ThreadItem {
            key: ItemKey { offset, part: 0 },
            at_ms: None,
            body: ThreadBody::Usage(usage(model, offset as i64, 1, 0, 0)),
        };
        let items = [
            usage_at(10, "claude-opus-5-5"),
            usage_at(20, "claude-opus-5-5"),
            usage_at(100, "claude-sonnet-5-5"),
            usage_at(400, "claude-sonnet-5-5"),
        ];
        let chain = segment_chain(&[start, account], &items);
        let summary: Vec<_> = chain
            .iter()
            .map(|s| (s.cause, s.start_offset, s.model.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            summary,
            vec![
                (SegmentCause::Start, 0, "claude-opus-5-5"),
                (SegmentCause::Model, 100, "claude-sonnet-5-5"),
                (SegmentCause::Account, 300, "claude-sonnet-5-5"),
            ]
        );
        assert_eq!(chain[2].index, 2);
    }

    #[test]
    fn item_bodies_round_trip_with_stable_tags() {
        let item = ThreadItem {
            key: ItemKey {
                offset: 42,
                part: 1,
            },
            at_ms: Some(7),
            body: ThreadBody::ToolCall {
                call_id: "toolu_1".into(),
                name: "Edit".into(),
                detail: ToolDetail::Edit {
                    path: "src/main.rs".into(),
                    hunks: vec![EditHunk {
                        old: "a".into(),
                        new: "b".into(),
                    }],
                },
            },
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["body"]["type"], "toolCall");
        assert_eq!(json["body"]["detail"]["kind"], "edit");
        assert_eq!(serde_json::from_value::<ThreadItem>(json).unwrap(), item);
    }
}
