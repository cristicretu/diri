//! The GUI Lens's client-side thread state, free of GPUI.
//!
//! [`ThreadModel::ingest`] merges `session.thread_read` pages by item key
//! (existing items win), so a replay from any older cursor, an Engine
//! restart or a reconnect never duplicates or drops a turn
//! (`DualLens.replay_idempotent`, `synced_reconnect`).
//!
//! Rows are built incrementally: transcript records are immutable once
//! written, so appended items only extend the tail row (or open new rows),
//! and every assistant turn is parsed to Markdown exactly once.
//! An out-of-order insert or a reset rebuilds from the retained items and
//! reuses the parsed documents, so nothing on screen changes shape.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use diri_proto::thread::{
    CacheWarmth, ItemKey, SegmentCause, ThreadBody, ThreadCursor, ThreadImage, ThreadItem,
    ThreadReadResult, ThreadSegment, ToolDetail, TurnUsage, WorkKind, cache_warmth, same_model,
    segment_chain,
};

use crate::i18n::{t, tf};
use crate::markdown::MarkdownDocument;

/// One tool call and, once it arrives, its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub key: ItemKey,
    pub call_id: String,
    pub name: String,
    pub detail: ToolDetail,
    pub at_ms: Option<i64>,
    pub result: Option<CallResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResult {
    pub is_error: bool,
    pub preview: String,
    pub images: Vec<ThreadImage>,
    pub at_ms: Option<i64>,
}

/// Consecutive tool calls and the thinking between them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkStrip {
    pub key: ItemKey,
    pub calls: Vec<Call>,
    pub thoughts: usize,
}

impl WorkStrip {
    /// `Explored 8 files · Edited 2 files · Ran 2 checks`.
    pub fn summary(&self) -> String {
        let mut explored = std::collections::BTreeSet::new();
        let mut searches = 0usize;
        let mut edited = std::collections::BTreeSet::new();
        let mut runs = 0usize;
        let mut delegated = 0usize;
        let mut other = 0usize;
        for call in &self.calls {
            match &call.detail {
                ToolDetail::Read { path } => {
                    explored.insert(path.as_str());
                }
                ToolDetail::Search { .. } => searches += 1,
                ToolDetail::Edit { path, .. } | ToolDetail::Write { path, .. } => {
                    edited.insert(path.as_str());
                }
                ToolDetail::Patch { patch } => {
                    let paths = patch_paths(patch);
                    if paths.is_empty() {
                        other += 1;
                    }
                    for path in paths {
                        edited.insert(path);
                    }
                }
                ToolDetail::Command { .. } => runs += 1,
                ToolDetail::Task { .. } => delegated += 1,
                ToolDetail::Other { .. } => other += 1,
            }
        }
        let count = |n: usize, one: &'static str, many: &'static str| {
            if n == 1 {
                t(one).to_owned()
            } else {
                tf(many, &[("count", &n)])
            }
        };
        let mut parts = Vec::new();
        if !explored.is_empty() {
            let mut text = count(
                explored.len(),
                "thread_lens.work.explored_file",
                "thread_lens.work.explored_files",
            );
            if searches > 0 {
                text.push_str(", ");
                text.push_str(&count(searches, "thread_lens.work.search", "thread_lens.work.searches"));
            }
            parts.push(text);
        } else if searches > 0 {
            parts.push(count(searches, "thread_lens.work.search", "thread_lens.work.searches"));
        }
        if !edited.is_empty() {
            parts.push(count(
                edited.len(),
                "thread_lens.work.edited_file",
                "thread_lens.work.edited_files",
            ));
        }
        if runs > 0 {
            parts.push(count(runs, "thread_lens.work.ran_command", "thread_lens.work.ran_commands"));
        }
        if delegated > 0 {
            parts.push(tf("thread_lens.work.delegated", &[("count", &delegated)]));
        }
        if other > 0 {
            parts.push(count(other, "thread_lens.work.used_tool", "thread_lens.work.used_tools"));
        }
        if parts.is_empty() {
            parts.push(t("thread_lens.work.thinking").to_owned());
        }
        parts.join(" · ")
    }

    pub fn failures(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| call.result.as_ref().is_some_and(|result| result.is_error))
            .count()
    }

    pub fn pending(&self) -> bool {
        self.calls.iter().any(|call| call.result.is_none())
    }

    /// From the first call to the last result, when both were timestamped.
    pub fn duration_ms(&self) -> Option<i64> {
        let start = self.calls.iter().filter_map(|call| call.at_ms).min()?;
        let end = self
            .calls
            .iter()
            .filter_map(|call| call.result.as_ref().and_then(|r| r.at_ms).or(call.at_ms))
            .max()?;
        (end >= start).then_some(end - start)
    }

    pub fn kinds(&self) -> impl Iterator<Item = WorkKind> + '_ {
        self.calls.iter().map(|call| call.detail.work())
    }

    pub fn edits(&self) -> impl Iterator<Item = &Call> {
        self.calls
            .iter()
            .filter(|call| matches!(call.detail.work(), WorkKind::Edit))
    }
}

pub fn patch_paths(patch: &str) -> Vec<&str> {
    patch
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Update File: ")
                .or_else(|| line.strip_prefix("*** Add File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .or_else(|| line.strip_prefix("+++ b/"))
                .map(str::trim)
        })
        .collect()
}

#[derive(Clone, Debug)]
pub enum Row {
    User {
        key: ItemKey,
        text: Arc<str>,
        images: Arc<[ThreadImage]>,
    },
    Command {
        key: ItemKey,
        name: Arc<str>,
        args: Arc<str>,
    },
    Assistant {
        key: ItemKey,
        text: Arc<str>,
        document: Arc<MarkdownDocument>,
    },
    Work(WorkStrip),
    Notice {
        key: ItemKey,
        text: Arc<str>,
    },
    Divider(ThreadSegment),
}

impl Row {
    /// Stable identity for element ids, expansion state and list splices.
    pub fn key(&self) -> ItemKey {
        match self {
            Row::User { key, .. }
            | Row::Command { key, .. }
            | Row::Assistant { key, .. }
            | Row::Notice { key, .. } => *key,
            Row::Work(strip) => strip.key,
            Row::Divider(segment) => ItemKey {
                offset: segment.start_offset,
                part: u16::MAX,
            },
        }
    }
}

/// What changed in `rows` after an ingest, for `ListState::splice`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowChange {
    None,
    /// Rows from `from` on were replaced (the tail strip grew, rows were
    /// appended). `old_len` is the row count before.
    Tail { from: usize, old_len: usize },
    Rebuilt,
}

#[derive(Default)]
pub struct ThreadModel {
    items: Vec<ThreadItem>,
    pub cursor: ThreadCursor,
    stored_segments: Vec<ThreadSegment>,
    pub structured: Option<bool>,
    pub reason: Option<String>,
    pub rows: Vec<Row>,
    /// Requests seen so far, with Codex's model-less token counts attributed
    /// to the model its latest turn context named.
    usages: Vec<TurnUsage>,
    builder: Builder,
    markdown: HashMap<(ItemKey, u64), Arc<MarkdownDocument>>,
    pub revision: u64,
}

/// Incremental row construction state, so an append continues where the
/// last one stopped.
#[derive(Clone, Default)]
struct Builder {
    /// Items consumed into rows.
    consumed: usize,
    next_stored: usize,
    model: Option<String>,
    calls: HashMap<String, (usize, usize)>,
}

impl ThreadModel {
    pub fn items(&self) -> &[ThreadItem] {
        &self.items
    }

    pub fn segments(&self) -> Vec<ThreadSegment> {
        segment_chain(&self.stored_segments, &self.items)
    }

    /// The model the next request goes to, as last observed.
    pub fn current_model(&self) -> Option<&str> {
        self.builder.model.as_deref()
    }

    pub fn warmth(&self, now_ms: i64) -> Option<(String, CacheWarmth)> {
        let model = self.current_model()?;
        Some((model.to_owned(), cache_warmth(self.usages.iter(), model, now_ms)))
    }

    pub fn warmth_for(&self, model: &str, now_ms: i64) -> CacheWarmth {
        cache_warmth(self.usages.iter(), model, now_ms)
    }

    pub fn last_usage(&self) -> Option<&TurnUsage> {
        self.usages.last()
    }

    /// Merges one page. Returns how the rows changed.
    pub fn ingest(&mut self, page: ThreadReadResult) -> RowChange {
        self.structured = Some(page.structured);
        self.reason = page.reason;
        let segments_changed = page.segments != self.stored_segments;
        self.stored_segments = page.segments;
        let mut in_order = true;
        if page.reset {
            // The file changed identity (account handoff copies it). Item
            // keys of identical bytes are identical, so parsed documents are
            // reused; only the sequence is rebuilt.
            self.items.clear();
            in_order = false;
        }
        let before = self.items.len();
        for item in page.items {
            match self.items.last() {
                Some(last) if last.key >= item.key => {
                    if let Err(at) = self.items.binary_search_by_key(&item.key, |i| i.key) {
                        self.items.insert(at, item);
                        in_order = false;
                    }
                }
                _ => self.items.push(item),
            }
        }
        self.cursor = page.next;
        let grew = self.items.len() != before;
        if !in_order || segments_changed {
            self.rebuild();
            self.revision += 1;
            return RowChange::Rebuilt;
        }
        if !grew {
            return RowChange::None;
        }
        let change = self.extend();
        self.revision += 1;
        change
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        self.usages.clear();
        self.builder = Builder::default();
        if let Some(first) = self.stored_segments.first()
            && first.cause != SegmentCause::Start
        {
            self.rows.push(Row::Divider(first.clone()));
        }
        if !self.stored_segments.is_empty() {
            self.builder.next_stored = 1;
        }
        self.extend();
    }

    /// Consumes items past `builder.consumed`. The tail Work strip may grow,
    /// so the change starts at the first row this call touched.
    fn extend(&mut self) -> RowChange {
        let old_len = self.rows.len();
        let mut first_touched = old_len;
        let items = std::mem::take(&mut self.items);
        for item in &items[self.builder.consumed..] {
            while let Some(segment) = self.stored_segments.get(self.builder.next_stored)
                && segment.start_offset <= item.key.offset
            {
                let mut segment = segment.clone();
                if segment.model.is_none() {
                    segment.model = self.builder.model.clone();
                }
                self.rows.push(Row::Divider(segment));
                self.builder.next_stored += 1;
            }
            let touched = self.push_item(item);
            first_touched = first_touched.min(touched);
        }
        self.builder.consumed = items.len();
        self.items = items;
        if first_touched >= old_len && self.rows.len() == old_len {
            RowChange::None
        } else {
            RowChange::Tail {
                from: first_touched.min(old_len),
                old_len,
            }
        }
    }

    /// Adds one item, returning the index of the first row it changed.
    fn push_item(&mut self, item: &ThreadItem) -> usize {
        let at = self.rows.len();
        match &item.body {
            ThreadBody::User { text, images } => {
                self.rows.push(Row::User {
                    key: item.key,
                    text: Arc::from(text.as_str()),
                    images: Arc::from(images.as_slice()),
                });
                at
            }
            ThreadBody::Command { name, args } => {
                self.rows.push(Row::Command {
                    key: item.key,
                    name: Arc::from(name.as_str()),
                    args: Arc::from(args.as_str()),
                });
                at
            }
            ThreadBody::Assistant { text } => {
                let document = self.document(item.key, text);
                self.rows.push(Row::Assistant {
                    key: item.key,
                    text: Arc::from(text.as_str()),
                    document,
                });
                at
            }
            ThreadBody::Notice { text } => {
                self.rows.push(Row::Notice {
                    key: item.key,
                    text: Arc::from(text.as_str()),
                });
                at
            }
            ThreadBody::Thinking { .. } => {
                let row = self.tail_strip(item.key);
                if let Row::Work(strip) = &mut self.rows[row] {
                    strip.thoughts += 1;
                }
                row
            }
            ThreadBody::ToolCall {
                call_id,
                name,
                detail,
            } => {
                let row = self.tail_strip(item.key);
                if let Row::Work(strip) = &mut self.rows[row] {
                    if !call_id.is_empty() {
                        self.builder
                            .calls
                            .insert(call_id.clone(), (row, strip.calls.len()));
                    }
                    strip.calls.push(Call {
                        key: item.key,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        detail: detail.clone(),
                        at_ms: item.at_ms,
                        result: None,
                    });
                }
                row
            }
            ThreadBody::ToolResult {
                call_id,
                is_error,
                preview,
                images,
            } => {
                let Some(&(row, call)) = self.builder.calls.get(call_id) else {
                    return at;
                };
                if let Some(Row::Work(strip)) = self.rows.get_mut(row)
                    && let Some(call) = strip.calls.get_mut(call)
                {
                    call.result = Some(CallResult {
                        is_error: *is_error,
                        preview: preview.clone(),
                        images: images.clone(),
                        at_ms: item.at_ms,
                    });
                }
                row
            }
            ThreadBody::Usage(usage) => {
                let mut usage = usage.clone();
                match &usage.model {
                    Some(model) => {
                        if let Some(previous) = self.builder.model.as_deref()
                            && !same_model(previous, model)
                        {
                            let segment = ThreadSegment {
                                index: 0,
                                kind: self
                                    .stored_segments
                                    .last()
                                    .map(|s| s.kind.clone())
                                    .unwrap_or(diri_proto::AgentKind::CLAUDE_CODE),
                                session_id: self
                                    .stored_segments
                                    .last()
                                    .map(|s| s.session_id.clone())
                                    .unwrap_or_else(|| diri_proto::SessionId(String::new())),
                                model: Some(model.clone()),
                                account: self
                                    .stored_segments
                                    .get(self.builder.next_stored.saturating_sub(1))
                                    .and_then(|s| s.account.clone()),
                                start_offset: item.key.offset,
                                started_at_ms: usage.at_ms,
                                cause: SegmentCause::Model,
                            };
                            self.rows.push(Row::Divider(segment));
                        }
                        self.builder.model = Some(model.clone());
                    }
                    None => usage.model = self.builder.model.clone(),
                }
                self.usages.push(usage);
                at
            }
        }
    }

    /// The Work strip at the tail, opening one if the last row is not.
    fn tail_strip(&mut self, key: ItemKey) -> usize {
        if matches!(self.rows.last(), Some(Row::Work(_))) {
            return self.rows.len() - 1;
        }
        self.rows.push(Row::Work(WorkStrip {
            key,
            calls: Vec::new(),
            thoughts: 0,
        }));
        self.rows.len() - 1
    }

    fn document(&mut self, key: ItemKey, text: &str) -> Arc<MarkdownDocument> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        Arc::clone(
            self.markdown
                .entry((key, hash))
                .or_insert_with(|| Arc::new(MarkdownDocument::parse(text))),
        )
    }

    /// Markdown parses performed so far (for the incremental-parse test and
    /// the benchmark).
    pub fn parsed_documents(&self) -> usize {
        self.markdown.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::thread::{EditHunk, THREAD_WIRE_VERSION};
    use diri_proto::{AgentKind, SessionId};

    fn key(offset: u64) -> ItemKey {
        ItemKey { offset, part: 0 }
    }

    fn item(offset: u64, body: ThreadBody) -> ThreadItem {
        ThreadItem {
            key: key(offset),
            at_ms: Some(offset as i64 * 100),
            body,
        }
    }

    fn page(items: Vec<ThreadItem>, next: u64) -> ThreadReadResult {
        ThreadReadResult {
            version: THREAD_WIRE_VERSION,
            structured: true,
            next: ThreadCursor {
                generation: "g".into(),
                offset: next,
            },
            items,
            segments: vec![start()],
            ..ThreadReadResult::default()
        }
    }

    fn start() -> ThreadSegment {
        ThreadSegment {
            index: 0,
            kind: AgentKind::CLAUDE_CODE,
            session_id: SessionId("s".into()),
            model: None,
            account: None,
            start_offset: 0,
            started_at_ms: 0,
            cause: SegmentCause::Start,
        }
    }

    fn usage(model: &str, at_ms: i64) -> ThreadBody {
        ThreadBody::Usage(TurnUsage {
            model: Some(model.into()),
            at_ms,
            cache_write: 10,
            cache_write_1h: 10,
            ..TurnUsage::default()
        })
    }

    fn conversation() -> Vec<ThreadItem> {
        vec![
            item(0, ThreadBody::User { text: "Fix the parser".into(), images: vec![] }),
            item(10, ThreadBody::Thinking { text: "plan".into() }),
            item(11, ThreadBody::ToolCall { call_id: "r".into(), name: "Read".into(), detail: ToolDetail::Read { path: "src/a.rs".into() } }),
            item(12, ThreadBody::ToolResult { call_id: "r".into(), is_error: false, preview: "fn a()".into(), images: vec![] }),
            item(13, ThreadBody::ToolCall { call_id: "e".into(), name: "Edit".into(), detail: ToolDetail::Edit { path: "src/a.rs".into(), hunks: vec![EditHunk { old: "a".into(), new: "b".into() }] } }),
            item(14, ThreadBody::ToolCall { call_id: "t".into(), name: "Bash".into(), detail: ToolDetail::Command { command: "cargo test".into() } }),
            item(15, ThreadBody::ToolResult { call_id: "t".into(), is_error: true, preview: "1 failed".into(), images: vec![] }),
            item(16, usage("claude-opus-5-5", 1_600)),
            item(20, ThreadBody::Assistant { text: "One **test** fails.".into() }),
        ]
    }

    fn shape(model: &ThreadModel) -> Vec<String> {
        model
            .rows
            .iter()
            .map(|row| match row {
                Row::User { .. } => "user".into(),
                Row::Command { .. } => "command".into(),
                Row::Assistant { .. } => "assistant".into(),
                Row::Work(strip) => format!("work:{}", strip.calls.len()),
                Row::Notice { .. } => "notice".into(),
                Row::Divider(segment) => format!("divider:{:?}", segment.cause),
            })
            .collect()
    }

    #[test]
    fn consecutive_tools_fold_into_one_strip_with_a_summary() {
        let mut model = ThreadModel::default();
        model.ingest(page(conversation(), 30));
        assert_eq!(shape(&model), vec!["user", "work:3", "assistant"]);
        let Row::Work(strip) = &model.rows[1] else { unreachable!() };
        assert_eq!(strip.summary(), "Explored 1 file · Edited 1 file · Ran 1 command");
        assert_eq!(strip.failures(), 1);
        assert!(strip.pending(), "the edit has no result yet");
        assert_eq!(strip.duration_ms(), Some(400));
        assert_eq!(model.current_model(), Some("claude-opus-5-5"));
    }

    /// Lean `replay_idempotent` / `synced_reconnect`: any chunking and any
    /// stale replay converge on the single-read rows.
    #[test]
    fn chunked_and_replayed_ingest_matches_one_read() {
        let all = conversation();
        let mut whole = ThreadModel::default();
        whole.ingest(page(all.clone(), 30));
        for split in 0..=all.len() {
            let mut model = ThreadModel::default();
            model.ingest(page(all[..split].to_vec(), 1));
            model.ingest(page(all[split..].to_vec(), 30));
            // Reconnect from zero: the whole stream again.
            assert_eq!(model.ingest(page(all.clone(), 30)), RowChange::None);
            assert_eq!(shape(&model), shape(&whole), "split {split}");
            assert_eq!(model.items().len(), all.len());
        }
    }

    #[test]
    fn appends_splice_only_the_tail_and_parse_each_turn_once() {
        let all = conversation();
        let mut model = ThreadModel::default();
        model.ingest(page(all[..3].to_vec(), 12));
        assert_eq!(shape(&model), vec!["user", "work:1"]);
        let change = model.ingest(page(all[3..6].to_vec(), 15));
        assert_eq!(change, RowChange::Tail { from: 1, old_len: 2 });
        model.ingest(page(all[6..].to_vec(), 30));
        assert_eq!(model.parsed_documents(), 1);
        let reset = ThreadReadResult {
            reset: true,
            ..page(all.clone(), 30)
        };
        assert_eq!(model.ingest(reset), RowChange::Rebuilt);
        assert_eq!(model.parsed_documents(), 1, "documents survive a reset");
    }

    #[test]
    fn model_changes_and_stored_handoffs_draw_dividers() {
        let mut items = conversation();
        items.push(item(40, usage("claude-sonnet-5-5", 4_000)));
        items.push(item(50, ThreadBody::Assistant { text: "Done.".into() }));
        let mut model = ThreadModel::default();
        let mut first = page(items, 60);
        first.segments.push(ThreadSegment {
            account: Some("Work".into()),
            start_offset: 45,
            cause: SegmentCause::Account,
            ..start()
        });
        model.ingest(first);
        assert_eq!(
            shape(&model),
            vec!["user", "work:3", "assistant", "divider:Model", "divider:Account", "assistant"]
        );
        let causes: Vec<_> = model.segments().iter().map(|s| s.cause).collect();
        assert_eq!(causes, vec![SegmentCause::Start, SegmentCause::Model, SegmentCause::Account]);
        assert!(matches!(
            model.warmth(4_000 + 60_000),
            Some((_, CacheWarmth::Warm { .. }))
        ));
        assert!(matches!(
            model.warmth_for("claude-haiku-5-5", 0),
            CacheWarmth::Cold { ever: false }
        ));
    }
}
