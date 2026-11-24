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
    /// What the strip did, by real tool and target: `Read view.rs · Ran
    /// cargo test`, `Read 3 files · Edited model.rs`, `Used Computer, Browser`.
    pub fn summary(&self) -> String {
        let mut read: Vec<&str> = Vec::new();
        let mut searched: Vec<&str> = Vec::new();
        let mut edited: Vec<&str> = Vec::new();
        let mut ran: Vec<String> = Vec::new();
        let mut delegated: Vec<&str> = Vec::new();
        let mut used: Vec<String> = Vec::new();
        for call in &self.calls {
            match &call.detail {
                ToolDetail::Read { path } => {
                    if !read.contains(&path.as_str()) {
                        read.push(path);
                    }
                }
                ToolDetail::Search { pattern, .. } => searched.push(pattern),
                ToolDetail::Edit { path, .. } | ToolDetail::Write { path, .. } => {
                    if !edited.contains(&path.as_str()) {
                        edited.push(path);
                    }
                }
                ToolDetail::Patch { patch } => {
                    for path in patch_paths(patch) {
                        if !edited.contains(&path) {
                            edited.push(path);
                        }
                    }
                }
                ToolDetail::Command { command } => ran.push(short_command(command)),
                ToolDetail::Task { description } => delegated.push(description),
                ToolDetail::Other { .. } => {
                    let label = tool_label(&call.name);
                    if !used.contains(&label) {
                        used.push(label);
                    }
                }
            }
        }
        let mut parts = Vec::new();
        let one_or_many = |items: usize, target: String, one: &'static str, many: &'static str| {
            if items == 1 {
                tf(one, &[("target", &target)])
            } else {
                tf(many, &[("count", &items)])
            }
        };
        if !read.is_empty() {
            parts.push(one_or_many(read.len(), file_name(read[0]), "thread_lens.work.read_one", "thread_lens.work.read_many"));
        }
        if !searched.is_empty() {
            parts.push(one_or_many(searched.len(), clip(searched[0], 32), "thread_lens.work.searched_one", "thread_lens.work.searched_many"));
        }
        if !edited.is_empty() {
            parts.push(one_or_many(edited.len(), file_name(edited[0]), "thread_lens.work.edited_one", "thread_lens.work.edited_many"));
        }
        if !ran.is_empty() {
            parts.push(one_or_many(ran.len(), ran[0].clone(), "thread_lens.work.ran_one", "thread_lens.work.ran_many"));
        }
        if !delegated.is_empty() {
            parts.push(one_or_many(delegated.len(), clip(delegated[0], 40), "thread_lens.work.delegated_one", "thread_lens.work.delegated_many"));
        }
        if !used.is_empty() {
            let mut tools = used.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            if used.len() > 3 {
                tools.push_str(&format!(" +{}", used.len() - 3));
            }
            parts.push(tf("thread_lens.work.used", &[("tools", &tools)]));
        }
        if parts.is_empty() {
            parts.push(t("thread_lens.work.thinking").to_owned());
        }
        parts.join(" · ")
    }

    /// While the strip runs: the tool working right now and its target,
    /// `Reading view.rs`, `Running cargo test`, `Using Computer · Trash`.
    pub fn activity(&self) -> String {
        let Some(call) = self
            .calls
            .iter()
            .rev()
            .find(|call| call.result.is_none())
        else {
            return t("thread_lens.work.thinking").to_owned();
        };
        let (key, target): (&'static str, String) = match &call.detail {
            ToolDetail::Read { path } => ("thread_lens.work.reading", file_name(path)),
            ToolDetail::Search { pattern, .. } => ("thread_lens.work.searching", clip(pattern, 40)),
            ToolDetail::Edit { path, .. } | ToolDetail::Write { path, .. } => ("thread_lens.work.editing", file_name(path)),
            ToolDetail::Patch { patch } => (
                "thread_lens.work.editing",
                patch_paths(patch).first().map_or_else(String::new, |path| file_name(path)),
            ),
            ToolDetail::Command { command } => ("thread_lens.work.running", short_command(command)),
            ToolDetail::Task { description } => ("thread_lens.work.delegating", clip(description, 40)),
            ToolDetail::Other { input } => {
                let label = tool_label(&call.name);
                let target = match tool_subject(input) {
                    Some(subject) => format!("{label} · {subject}"),
                    None => label,
                };
                ("thread_lens.work.using", target)
            }
        };
        tf(key, &[("target", &target)])
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

/// The last path component: what a reader recognises a file by.
pub fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn clip(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= limit {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `cd x && FOO=1 cargo test -p diri-engine --lib` → `cargo test -p diri-engine`.
pub fn short_command(command: &str) -> String {
    let line = command.lines().next().unwrap_or_default().trim();
    let line = line.rsplit("&& ").next().unwrap_or(line).trim();
    let mut out = String::new();
    for (index, token) in line
        .split_whitespace()
        .skip_while(|token| token.contains('=') && !token.starts_with('-'))
        .enumerate()
    {
        if index >= 2 && (token.starts_with("--") || out.len() + token.len() >= 28) {
            break;
        }
        if index > 0 {
            out.push(' ');
        }
        out.push_str(token);
        if index >= 3 {
            break;
        }
    }
    clip(&out, 32)
}

/// `mcp__dirijor__computer` → `Computer`, `WebFetch` → `Web fetch`.
pub fn tool_label(name: &str) -> String {
    let base = name.rsplit("__").next().unwrap_or(name);
    let mut words = String::new();
    for (index, c) in base.chars().enumerate() {
        if c == '_' || c == '-' {
            words.push(' ');
        } else if c.is_ascii_uppercase() && index > 0 && !words.ends_with(' ') {
            words.push(' ');
            words.push(c.to_ascii_lowercase());
        } else {
            words.push(c);
        }
    }
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => name.to_owned(),
    }
}

/// The short noun a tool call acts on, from its JSON input.
pub fn tool_subject(input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    ["title", "target", "query", "url", "app", "element", "path", "file_path", "name", "description", "action"]
        .iter()
        .find_map(|key| value.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(|subject| clip(subject, 36))
}

/// Claude and OpenAI model ids as people say them: `claude-opus-5-5` → `Opus 5.5`.
pub fn model_display(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    for family in ["opus", "sonnet", "haiku", "fable"] {
        if let Some(rest) = lower.split(&format!("{family}-")).nth(1) {
            let version: Vec<&str> = rest
                .split(['-', '['])
                .take_while(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit()))
                .collect();
            let mut name = String::from(&family[..1]).to_ascii_uppercase() + &family[1..];
            if !version.is_empty() {
                name.push(' ');
                name.push_str(&version.join("."));
            }
            return name;
        }
    }
    model.to_owned()
}

/// `Set model to \`Opus 5.5\` and saved…` → `Opus 5.5`.
fn announced_model(output: &str) -> Option<String> {
    let rest = output.trim().strip_prefix("Set model to")?;
    let name = match rest.split('`').nth(1) {
        Some(quoted) => quoted.to_owned(),
        None => rest.trim().split(" and ").next()?.trim_end_matches('.').to_owned(),
    };
    (!name.is_empty()).then(|| model_display(&name))
}

/// `Set effort level to medium (saved as your default…)` → `medium`.
fn announced_effort(output: &str) -> Option<String> {
    let rest = output.trim().strip_prefix("Set effort level to ")?;
    let level: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!level.is_empty()).then_some(level)
}

/// Whitespace-insensitive text identity, for matching a sent message to the
/// transcript record it becomes.
fn same_text(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

#[derive(Clone, Debug)]
pub enum Row {
    User {
        key: ItemKey,
        text: Arc<str>,
        images: Arc<[ThreadImage]>,
    },
    /// A CLI-local command and what it printed; `name` is empty for output
    /// whose command line was not recorded.
    Command {
        key: ItemKey,
        name: Arc<str>,
        args: Arc<str>,
        output: Option<(Arc<str>, bool)>,
    },
    /// One block of an assistant turn (a paragraph, heading, list, quote,
    /// code block or diagram). Long turns virtualize block by block, so a
    /// 200-line reply lays out only the blocks on screen.
    Assistant {
        key: ItemKey,
        /// Index into `document.blocks`.
        block: u16,
        /// The turn's last block, which carries its Copy control.
        last: bool,
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
    /// Stable identity of the row itself: its item key and, for an
    /// assistant block, which block.
    pub fn id(&self) -> (ItemKey, u16) {
        match self {
            Row::Assistant { key, block, .. } => (*key, *block),
            row => (row.key(), u16::MAX),
        }
    }

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

/// A message sent from the composer that the transcript has not recorded
/// yet, drawn at once and dropped when its record lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub text: Arc<str>,
    pub sent_ms: i64,
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
    pub pending: Vec<Pending>,
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
    /// A `/model` ran and its result has not printed yet.
    model_command: bool,
    /// The CLI's permission mode, as last recorded.
    mode: Option<String>,
    /// When the latest user message was recorded.
    last_user_at: Option<i64>,
    /// The model the CLI last confirmed (`/model` output or a request), as
    /// people say it.
    selected: Option<String>,
    /// The effort the CLI last confirmed (`/effort` output).
    effort: Option<String>,
    /// The model a `/model` divider already announced, so the next request's
    /// usage record does not draw a second divider for the same switch.
    announced: Option<String>,
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

    /// When the latest user message was recorded, or a pending one sent.
    pub fn last_user_at(&self) -> Option<i64> {
        let pending = self.pending.iter().map(|p| p.sent_ms).max();
        pending.max(self.builder.last_user_at)
    }

    /// The thread ends on the user's message: the Agent owes a reply.
    pub fn awaiting_reply(&self) -> bool {
        !self.pending.is_empty() || matches!(self.rows.last(), Some(Row::User { .. }))
    }

    /// The model the CLI last confirmed, as people say it (`Haiku 5.5`).
    pub fn selected_model(&self) -> Option<&str> {
        self.builder.selected.as_deref()
    }

    /// The effort the CLI last confirmed.
    pub fn effort(&self) -> Option<&str> {
        self.builder.effort.as_deref()
    }

    /// The permission mode the transcript last recorded.
    pub fn mode(&self) -> Option<&str> {
        self.builder.mode.as_deref()
    }

    /// How full the context window was at the last request: tokens sent
    /// (fresh, cached and cache-written input plus the reply) and the
    /// window. Codex reports its window; Claude's is 200k, or 1M for a
    /// `[1m]` model or a request already past 200k.
    pub fn context(&self) -> Option<(u64, u64)> {
        let usage = self.usages.iter().rev().find(|u| u.input + u.cache_read + u.cache_write > 0)?;
        let used = usage.input + usage.cache_read + usage.cache_write + usage.output;
        let window = usage.context_window.unwrap_or_else(|| {
            let long = usage.model.as_deref().is_some_and(|m| m.contains("[1m]"));
            if long || used > 200_000 { 1_000_000 } else { 200_000 }
        });
        Some((used.min(window), window))
    }

    /// Every model this thread's requests went to, in first-use order.
    pub fn usage_models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for model in self.usages.iter().filter_map(|u| u.model.as_deref()) {
            if !out.iter().any(|m| same_model(m, model)) {
                out.push(model.to_owned());
            }
        }
        out
    }

    pub fn last_usage(&self) -> Option<&TurnUsage> {
        self.usages.last()
    }

    /// Shows `text` as sent before the transcript has it.
    pub fn push_pending(&mut self, text: &str, sent_ms: i64) {
        self.pending.push(Pending {
            text: Arc::from(text),
            sent_ms,
        });
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
                self.builder.last_user_at = item.at_ms.or(self.builder.last_user_at);
                if let Some(index) = self.pending.iter().position(|p| same_text(&p.text, text)) {
                    self.pending.remove(index);
                } else if let Some(at_ms) = item.at_ms {
                    // A message the CLI rewrote (expanded a paste, trimmed a
                    // mention) never matches; one older than this record is done.
                    self.pending.retain(|p| p.sent_ms > at_ms);
                }
                self.rows.push(Row::User {
                    key: item.key,
                    text: Arc::from(text.as_str()),
                    images: Arc::from(images.as_slice()),
                });
                at
            }
            ThreadBody::Command { name, args } => {
                if name == "/model" {
                    // The switch shows as one divider once the CLI confirms it.
                    self.builder.model_command = true;
                    return at;
                }
                self.builder.model_command = false;
                self.rows.push(Row::Command {
                    key: item.key,
                    name: Arc::from(name.as_str()),
                    args: Arc::from(args.as_str()),
                    output: None,
                });
                at
            }
            ThreadBody::Mode { mode } => {
                self.builder.mode = Some(mode.clone());
                at
            }
            ThreadBody::CommandOutput { text, is_error } => {
                if let Some(effort) = announced_effort(text) {
                    self.builder.effort = Some(effort);
                }
                if let Some(model) = announced_model(text) {
                    self.builder.selected = Some(model.clone());
                    self.builder.model_command = false;
                    let segment = self.model_segment(item.key.offset, item.at_ms.unwrap_or_default(), model.clone());
                    self.builder.announced = Some(model);
                    // Switching twice with nothing between reads as one switch.
                    if let Some(Row::Divider(last)) = self.rows.last_mut()
                        && last.cause == SegmentCause::Model
                    {
                        *last = segment;
                        return at.saturating_sub(1);
                    }
                    self.rows.push(Row::Divider(segment));
                    return at;
                }
                if std::mem::take(&mut self.builder.model_command) {
                    // "Kept model as …": nothing changed.
                    return at;
                }
                if let Some(Row::Command { output: output @ None, .. }) = self.rows.last_mut() {
                    *output = Some((Arc::from(text.as_str()), *is_error));
                    return at.saturating_sub(1);
                }
                self.rows.push(Row::Command {
                    key: item.key,
                    name: Arc::from(""),
                    args: Arc::from(""),
                    output: Some((Arc::from(text.as_str()), *is_error)),
                });
                at
            }
            ThreadBody::Assistant { text } => {
                let document = self.document(item.key, text);
                let text: Arc<str> = Arc::from(text.as_str());
                let blocks = document.blocks.len().min(usize::from(u16::MAX));
                for block in 0..blocks {
                    self.rows.push(Row::Assistant {
                        key: item.key,
                        block: block as u16,
                        last: block + 1 == blocks,
                        text: Arc::clone(&text),
                        document: Arc::clone(&document),
                    });
                }
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
                        self.builder.selected = Some(model_display(model));
                        if let Some(previous) = self.builder.model.as_deref()
                            && !same_model(previous, model)
                            && self.builder.announced.as_deref() != Some(model_display(model).as_str())
                        {
                            let segment = self.model_segment(item.key.offset, usage.at_ms, model.clone());
                            self.rows.push(Row::Divider(segment));
                        }
                        self.builder.announced = None;
                        self.builder.model = Some(model.clone());
                    }
                    None => usage.model = self.builder.model.clone(),
                }
                self.usages.push(usage);
                at
            }
        }
    }

    fn model_segment(&self, start_offset: u64, started_at_ms: i64, model: String) -> ThreadSegment {
        let current = self.stored_segments.get(self.builder.next_stored.saturating_sub(1));
        ThreadSegment {
            index: 0,
            kind: current
                .map(|s| s.kind.clone())
                .unwrap_or(diri_proto::AgentKind::CLAUDE_CODE),
            session_id: current
                .map(|s| s.session_id.clone())
                .unwrap_or_else(|| diri_proto::SessionId(String::new())),
            model: Some(model),
            account: current.and_then(|s| s.account.clone()),
            start_offset,
            started_at_ms,
            cause: SegmentCause::Model,
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
            // A turn's blocks read as one turn here.
            .filter(|row| !matches!(row, Row::Assistant { block, .. } if *block > 0))
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
        assert_eq!(strip.summary(), "Read a.rs · Edited a.rs · Ran cargo test");
        assert_eq!(strip.activity(), "Editing a.rs", "the edit is the call still running");
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

    /// The pill's `/model opus`, `/model sonnet`, `/model opus` from the
    /// user's screenshot: no chips, one divider for the switch that stuck,
    /// and no second divider when the next request confirms it.
    #[test]
    fn model_commands_collapse_into_one_divider() {
        let command = |offset, args: &str| item(offset, ThreadBody::Command { name: "/model".into(), args: args.into() });
        let output = |offset, model: &str| item(offset, ThreadBody::CommandOutput {
            text: format!("Set model to `{model}` and saved as your default for new sessions"),
            is_error: false,
        });
        let mut items = conversation();
        items.extend([
            command(30, "opus"), output(31, "Opus 5.5"),
            command(32, "sonnet"), output(33, "Sonnet 5.5"),
            command(34, "opus"), output(35, "Opus 5.5"),
            command(36, "haiku"), output(37, "Haiku 5.5"),
            item(40, ThreadBody::User { text: "hi there".into(), images: vec![] }),
            item(41, usage("claude-haiku-5-5", 4_100)),
            item(42, ThreadBody::Assistant { text: "Hi!".into() }),
        ]);
        let mut model = ThreadModel::default();
        model.ingest(page(items.clone(), 50));
        assert_eq!(shape(&model), vec!["user", "work:3", "assistant", "divider:Model", "user", "assistant"]);
        let Row::Divider(segment) = &model.rows[3] else { unreachable!() };
        assert_eq!(segment.model.as_deref(), Some("Haiku 5.5"));
        // Incremental arrival draws the same rows.
        let mut stepwise = ThreadModel::default();
        for one in items {
            stepwise.ingest(page(vec![one], 50));
        }
        assert_eq!(shape(&stepwise), shape(&model));
    }

    #[test]
    fn local_command_output_stays_with_its_command() {
        let mut model = ThreadModel::default();
        model.ingest(page(vec![
            item(0, ThreadBody::Command { name: "/cost".into(), args: String::new() }),
            item(1, ThreadBody::CommandOutput { text: "Total cost: $0.42".into(), is_error: false }),
            item(2, ThreadBody::Command { name: "/model".into(), args: String::new() }),
            item(3, ThreadBody::CommandOutput { text: "Kept model as Opus 5.5".into(), is_error: false }),
        ], 4));
        assert_eq!(shape(&model), vec!["command"]);
        let Row::Command { output: Some((text, false)), .. } = &model.rows[0] else { unreachable!() };
        assert_eq!(&**text, "Total cost: $0.42");
    }

    /// Optimistic send: drawn at 0 ms, dropped exactly when its record lands.
    #[test]
    fn a_sent_message_is_pending_until_its_record_lands() {
        let mut model = ThreadModel::default();
        model.ingest(page(conversation(), 30));
        model.push_pending("cool  it's fine.\n", 10_000);
        model.push_pending("and another", 10_001);
        assert_eq!(model.pending.len(), 2);
        model.ingest(page(vec![item(40, ThreadBody::User { text: "cool it's fine.".into(), images: vec![] })], 41));
        assert_eq!(model.pending.len(), 1);
        assert_eq!(&*model.pending[0].text, "and another");
        // A replay of the same record does not resurrect or eat anything.
        model.ingest(page(conversation(), 41));
        assert_eq!(model.pending.len(), 1);
    }

    #[test]
    fn tool_names_read_the_way_people_say_them() {
        assert_eq!(tool_label("mcp__dirijor__computer"), "Computer");
        assert_eq!(tool_label("WebFetch"), "Web fetch");
        assert_eq!(tool_label("mcp__browser__take_screenshot"), "Take screenshot");
        assert_eq!(tool_subject(r#"{"action":"click","title":"Trash"}"#).as_deref(), Some("Trash"));
        assert_eq!(short_command("cd /r && RUST_LOG=1 cargo test -p diri-engine --lib"), "cargo test -p diri-engine");
        assert_eq!(short_command("git push origin feat/dual-lens-gui"), "git push origin");
        let strip = WorkStrip {
            key: key(0),
            thoughts: 0,
            calls: ["mcp__dirijor__computer", "mcp__dirijor__computer", "mcp__browser__navigate"]
                .iter()
                .enumerate()
                .map(|(n, name)| Call {
                    key: key(n as u64),
                    call_id: n.to_string(),
                    name: (*name).into(),
                    detail: ToolDetail::Other { input: r#"{"title":"Trash"}"#.into() },
                    at_ms: None,
                    result: None,
                })
                .collect(),
        };
        assert_eq!(strip.summary(), "Used Computer, Navigate");
        assert_eq!(strip.activity(), "Using Navigate · Trash");
    }

    #[test]
    fn a_long_turn_is_one_row_per_block() {
        let mut model = ThreadModel::default();
        let text = "# Plan\n\nFirst.\n\n```rust\nfn a() {}\n```\n\n- one\n- two\n\nDone.";
        model.ingest(page(vec![item(0, ThreadBody::Assistant { text: text.into() })], 1));
        let blocks: Vec<(u16, bool)> = model
            .rows
            .iter()
            .map(|row| match row {
                Row::Assistant { block, last, .. } => (*block, *last),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(blocks, vec![(0, false), (1, false), (2, false), (3, false), (4, true)]);
        assert_eq!(model.parsed_documents(), 1, "one parse for all its rows");
    }

    #[test]
    fn mode_and_context_follow_the_latest_records() {
        let mut model = ThreadModel::default();
        let mut items = conversation();
        items.push(item(30, ThreadBody::Mode { mode: "plan".into() }));
        model.ingest(page(items, 31));
        assert_eq!(model.mode(), Some("plan"));
        // conversation(): cache_write 10 on the last request.
        assert_eq!(model.context(), Some((10, 200_000)));
        model.ingest(page(vec![item(40, ThreadBody::Usage(TurnUsage {
            input: 1_000, cache_read: 99_000, context_window: Some(272_000), ..TurnUsage::default()
        }))], 41));
        assert_eq!(model.context(), Some((100_000, 272_000)));
    }

    /// The user's live run: `/effort medium`, then `/model haiku`, with no
    /// request yet. The pill must say Haiku 5.5 · medium, not the startup
    /// default.
    #[test]
    fn the_cli_confirmation_names_the_model_and_effort_before_any_request() {
        let mut model = ThreadModel::default();
        model.ingest(page(vec![
            item(0, ThreadBody::Command { name: "/effort".into(), args: "medium".into() }),
            item(1, ThreadBody::CommandOutput { text: "Set effort level to medium (saved as your default for new sessions): Balanced approach".into(), is_error: false }),
            item(2, ThreadBody::Command { name: "/model".into(), args: "haiku".into() }),
            item(3, ThreadBody::CommandOutput { text: "Set model to `Haiku 5.5` and saved as your default for new sessions".into(), is_error: false }),
        ], 4));
        assert_eq!(model.selected_model(), Some("Haiku 5.5"));
        assert_eq!(model.effort(), Some("medium"));
        assert_eq!(model.current_model(), None, "no request has gone to it yet");
        model.ingest(page(vec![item(10, usage("claude-haiku-5-5", 1_000))], 11));
        assert_eq!(model.selected_model(), Some("Haiku 5.5"));
    }
}
