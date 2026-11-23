//! First-launch conversation import: find the handful of recent Claude Code
//! and Codex conversations worth bringing into Diri, and turn the chosen ones
//! into resumable records that start nothing.
//!
//! Discovery is strictly read-only and bounded. It lists transcripts by
//! modification time first (a `stat` each), then opens only the newest until
//! it has enough real conversations, so a machine with thousands of
//! transcripts costs the same as one with fifty. Each opened transcript gets
//! a bounded head read for identity and a bounded tail read for the title
//! and the last thing said.
//!
//! What is left out, and why:
//! - subagent transcripts: a sidechain or child thread is not something the
//!   person ever typed into, and resuming one alone makes no sense;
//! - empty conversations: no Agent reply means nothing to pick up;
//! - scratch folders (`/tmp`, `/private/tmp`, `/var/folders`, anything named
//!   `scratchpad`): throwaway work that would clutter the sidebar;
//! - deleted folders: resuming there could only fail;
//! - conversations Diri already tracks, so importing again is a no-op.
//!
//! Import builds records in [`ExitReason::Imported`] state: no Holder, PTY or
//! Agent process exists until the person opens one, which resumes it exactly.
//!
//! The selection and idempotence rules are modelled in
//! `diri/formal/OnboardingImport.lean`.
//!
//! [`ExitReason::Imported`]: diri_proto::ExitReason::Imported

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use diri_proto::{AgentKind, DateMillis, ExitInfo, Resumability, SessionStatus, TitleSource};
use serde_json::Value;

use crate::history::{self, HistoryEntry, HistoryKind};

/// Suggestions shown pre-checked on first launch. Never more.
pub const SUGGESTED: usize = 5;
/// Further valid conversations offered behind "Browse more".
pub const MAX_MORE: usize = 120;
/// Transcripts opened per scan, newest first, junk included.
const MAX_OPENED: usize = 400;
/// Directory entries listed per provider before sorting by time.
const MAX_LISTED: usize = 8_192;
/// Head window read for sidechain and first-prompt facts.
const HEAD_BYTES: u64 = 64 << 10;
/// Tail window read for the last message and whether the Agent replied.
const TAIL_BYTES: u64 = 64 << 10;
/// Preview characters kept: one line in the onboarding row.
const PREVIEW_CHARS: usize = 140;

/// One conversation worth offering.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub entry: HistoryEntry,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    /// Newest first, at most [`SUGGESTED`].
    pub suggested: Vec<Candidate>,
    /// The rest of the valid conversations the bounded scan found.
    pub more: Vec<Candidate>,
    /// The scan stopped at a bound before running out of transcripts.
    pub truncated: bool,
}

/// Why a transcript was not offered. Tests assert on these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    Unreadable,
    Subagent,
    Empty,
    Scratch,
    MissingFolder,
    Tracked,
}

/// Scan `home`'s Claude Code and Codex stores.
pub fn discover(home: &Path, tracked: &HashSet<String>) -> Discovery {
    let mut listed = list_claude(&home.join(".claude/projects"));
    listed.extend(list_codex(&home.join(".codex/sessions")));
    // Newest first; the path breaks ties so equal mtimes order stably.
    listed.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let window = &listed[..listed.len().min(MAX_OPENED)];

    let mut seen = tracked.clone();
    let mut valid = Vec::new();
    let mut opened = 0;
    // Opening transcripts is the cost; a batch at a time runs in parallel
    // and stops as soon as enough real conversations are in hand.
    for batch in window.chunks(BATCH) {
        if valid.len() >= SUGGESTED + MAX_MORE {
            break;
        }
        opened += batch.len();
        for result in inspect_batch(home, batch) {
            let Ok(candidate) = result else { continue };
            // Already tracked, or a second transcript for one conversation
            // (the newer one came first and wins).
            if !seen.insert(candidate.entry.id.clone()) {
                continue;
            }
            if valid.len() < SUGGESTED + MAX_MORE {
                valid.push(candidate);
            }
        }
    }
    let truncated = opened < listed.len();
    apply_codex_titles(home, &mut valid);

    let more = valid.split_off(valid.len().min(SUGGESTED));
    Discovery {
        suggested: valid,
        more,
        truncated,
    }
}

/// Transcripts inspected together.
const BATCH: usize = 32;
/// Threads per batch: transcript reads are small and mostly wait on disk.
const WORKERS: usize = 8;

/// Inspect a batch on scoped threads, results in input order.
fn inspect_batch(home: &Path, batch: &[Listed]) -> Vec<Result<Candidate, Rejection>> {
    let mut results: Vec<Option<Result<Candidate, Rejection>>> = vec![None; batch.len()];
    let workers = WORKERS.min(batch.len()).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                scope.spawn(move || {
                    batch
                        .iter()
                        .enumerate()
                        .skip(worker)
                        .step_by(workers)
                        .map(|(index, file)| (index, inspect(home, file)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            for (index, result) in handle.join().unwrap_or_default() {
                results[index] = Some(result);
            }
        }
    });
    results
        .into_iter()
        .map(|result| result.unwrap_or(Err(Rejection::Unreadable)))
        .collect()
}

struct Listed {
    modified: SystemTime,
    created: Option<SystemTime>,
    path: PathBuf,
    kind: HistoryKind,
}

/// Top-level `<project>/<uuid>.jsonl` only: Claude keeps subagent
/// transcripts in nested `<uuid>/subagents/` folders, which this skips.
fn list_claude(root: &Path) -> Vec<Listed> {
    let mut listed = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return listed;
    };
    for project in projects.filter_map(Result::ok) {
        if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.filter_map(Result::ok) {
            if listed.len() >= MAX_LISTED {
                return listed;
            }
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            listed.push(Listed {
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                created: meta.created().ok(),
                path,
                kind: HistoryKind::ClaudeCode,
            });
        }
    }
    listed
}

/// `YYYY/MM/DD/rollout-*.jsonl`, walked newest day first. Once a whole day
/// brings the listing past what one scan opens, older days cannot reach the
/// newest window, so the walk stops there.
fn list_codex(root: &Path) -> Vec<Listed> {
    let mut listed = Vec::new();
    for year in sorted_dirs_desc(root) {
        for month in sorted_dirs_desc(&year) {
            for day in sorted_dirs_desc(&month) {
                if listed.len() >= MAX_OPENED {
                    return listed;
                }
                let Ok(files) = std::fs::read_dir(&day) else {
                    continue;
                };
                for file in files.filter_map(Result::ok) {
                    if listed.len() >= MAX_LISTED {
                        return listed;
                    }
                    let path = file.path();
                    let is_rollout =
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with("rollout-") && name.ends_with(".jsonl")
                            });
                    if !is_rollout {
                        continue;
                    }
                    let Ok(meta) = file.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    listed.push(Listed {
                        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        created: meta.created().ok(),
                        path,
                        kind: HistoryKind::Codex,
                    });
                }
            }
        }
    }
    listed
}

fn sorted_dirs_desc(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort_by(|a, b| b.cmp(a));
    dirs
}

/// Open one transcript and decide whether it is worth offering. Identity
/// checks against what Diri tracks happen in the caller, in time order.
fn inspect(home: &Path, file: &Listed) -> Result<Candidate, Rejection> {
    let (mut entry, facts) = match file.kind {
        HistoryKind::ClaudeCode => {
            let facts = transcript_facts(&file.path, Format::Claude)?;
            // The filename stem is the id `claude --resume` resolves.
            let id = file
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| stem.len() >= 32)
                .ok_or(Rejection::Unreadable)?
                .to_owned();
            // A first line longer than the head window: the shared reader
            // looks further, bounded, for the folder and the first prompt.
            let fallback = || history::claude_entry(&file.path);
            let (cwd, prompt_title) = match (&facts.cwd, &facts.first_prompt) {
                (Some(cwd), Some(prompt)) => (cwd.clone(), Some(history::title_from(prompt))),
                _ => {
                    let found = fallback().ok_or(Rejection::Unreadable)?;
                    (found.cwd, found.title)
                }
            };
            let title = facts
                .title
                .clone()
                .or(prompt_title)
                .filter(|title| !title.starts_with('<'));
            let entry = HistoryEntry {
                id,
                kind: HistoryKind::ClaudeCode,
                cwd_exists: true,
                cwd,
                title,
                transcript_path: file.path.to_string_lossy().into_owned(),
                last_active_at: file.modified,
                created_at: file.created,
            };
            (entry, facts)
        }
        HistoryKind::Codex => {
            // `codex_entry` already drops subagent threads.
            let mut entry = history::codex_entry(&file.path).ok_or(Rejection::Subagent)?;
            let facts = transcript_facts(&file.path, Format::Codex)?;
            entry.title = facts.first_prompt.as_deref().map(history::title_from);
            (entry, facts)
        }
    };
    check_entry(home, &entry.id, &entry.cwd, &HashSet::new())?;
    if facts.sidechain {
        return Err(Rejection::Subagent);
    }
    // No Agent reply: nothing to pick up.
    if !facts.replied {
        return Err(Rejection::Empty);
    }
    entry.cwd_exists = true;
    Ok(Candidate {
        entry,
        preview: facts.last_message,
    })
}

/// The filters shared by discovery and import, in the order the Lean model
/// states them: junk first, then identity.
pub(crate) fn check_entry(
    home: &Path,
    id: &str,
    cwd: &str,
    seen: &HashSet<String>,
) -> Result<(), Rejection> {
    if !history::safe_agent_id(id) {
        return Err(Rejection::Unreadable);
    }
    if is_scratch_path(home, cwd) {
        return Err(Rejection::Scratch);
    }
    if !Path::new(cwd).is_dir() {
        return Err(Rejection::MissingFolder);
    }
    if seen.contains(id) {
        return Err(Rejection::Tracked);
    }
    Ok(())
}

/// Throwaway locations: system temp folders and agent scratchpads. A folder
/// inside `home` is never a system temp folder, whatever its parents are
/// called; only its own components are checked for a scratchpad.
pub fn is_scratch_path(home: &Path, cwd: &str) -> bool {
    const TEMP_ROOTS: [&str; 4] = [
        "/tmp",
        "/private/tmp",
        "/var/folders",
        "/private/var/folders",
    ];
    let path = Path::new(cwd);
    let inside_home = path.strip_prefix(home).ok();
    if inside_home.is_none() {
        let temp = std::env::var("TMPDIR").unwrap_or_default();
        if TEMP_ROOTS.iter().any(|root| path.starts_with(root))
            || (!temp.is_empty() && path.starts_with(&temp))
        {
            return true;
        }
    }
    inside_home.unwrap_or(path).components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("scratchpad"))
    })
}

#[derive(Clone, Copy)]
enum Format {
    Claude,
    Codex,
}

#[derive(Default)]
struct Facts {
    /// Claude: the first line naming a folder.
    cwd: Option<String>,
    /// Claude: newest `/rename`, else newest generated title, in the tail.
    title: Option<String>,
    sidechain: bool,
    replied: bool,
    first_prompt: Option<String>,
    last_message: Option<String>,
}

fn transcript_facts(path: &Path, format: Format) -> Result<Facts, Rejection> {
    let mut file = history::open_regular_readonly(path).ok_or(Rejection::Unreadable)?;
    let len = file.metadata().map_err(|_| Rejection::Unreadable)?.len();
    let mut facts = Facts::default();

    let head = read_window(&mut file, 0, HEAD_BYTES.min(len))?;
    for line in complete_lines(&head, true) {
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if matches!(format, Format::Claude) && facts.cwd.is_none() {
            facts.cwd = object
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_owned);
        }
        match message(&object, format) {
            Some(Message::Sidechain) => facts.sidechain = true,
            Some(Message::User(text)) if facts.first_prompt.is_none() => {
                facts.last_message = Some(text.clone());
                facts.first_prompt = Some(text);
            }
            Some(Message::Assistant(text)) => {
                facts.replied = true;
                if !text.trim().is_empty() {
                    facts.last_message = Some(text);
                }
            }
            Some(Message::User(text)) => facts.last_message = Some(text),
            None => {}
        }
    }

    let start = len.saturating_sub(TAIL_BYTES);
    let tail = if start == 0 {
        head
    } else {
        read_window(&mut file, start, len - start)?
    };
    let (mut custom, mut generated) = (None, None);
    for line in complete_lines(&tail, start == 0) {
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if matches!(format, Format::Claude) {
            let text = |key| {
                object
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
            };
            match object.get("type").and_then(Value::as_str) {
                Some("custom-title") => custom = text("customTitle").or(custom),
                Some("ai-title") => generated = text("aiTitle").or(generated),
                _ => {}
            }
        }
        match message(&object, format) {
            Some(Message::User(text)) => facts.last_message = Some(text),
            Some(Message::Assistant(text)) => {
                facts.replied = true;
                if !text.trim().is_empty() {
                    facts.last_message = Some(text);
                }
            }
            _ => {}
        }
    }
    facts.title = custom.or(generated);
    facts.last_message = facts.last_message.as_deref().map(preview_line);
    Ok(facts)
}

fn read_window(file: &mut File, start: u64, len: u64) -> Result<Vec<u8>, Rejection> {
    file.seek(SeekFrom::Start(start))
        .map_err(|_| Rejection::Unreadable)?;
    let mut data = Vec::with_capacity(len as usize);
    file.by_ref()
        .take(len)
        .read_to_end(&mut data)
        .map_err(|_| Rejection::Unreadable)?;
    Ok(data)
}

/// Lines wholly inside the window: a window that starts mid-file drops its
/// first partial line, and the last line is kept only if it parses.
fn complete_lines(data: &[u8], from_start: bool) -> impl Iterator<Item = &str> {
    let text = std::str::from_utf8(data).unwrap_or_else(|error| {
        // A window can cut a multi-byte character; keep the valid prefix.
        std::str::from_utf8(&data[..error.valid_up_to()]).unwrap_or_default()
    });
    text.split('\n')
        .skip(usize::from(!from_start))
        .filter(|line| !line.trim().is_empty())
}

enum Message {
    User(String),
    Assistant(String),
    Sidechain,
}

fn message(object: &Value, format: Format) -> Option<Message> {
    match format {
        Format::Claude => claude_message(object),
        Format::Codex => codex_message(object),
    }
}

fn claude_message(object: &Value) -> Option<Message> {
    if object.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Some(Message::Sidechain);
    }
    if object.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match object.get("type").and_then(Value::as_str)? {
        "user" => history::claude_user_text(object)
            .filter(|text| is_human_text(text))
            .map(Message::User),
        "assistant" => {
            let content = object.get("message")?.get("content")?;
            let text = match content {
                Value::String(text) => Some(text.clone()),
                Value::Array(items) => items.iter().find_map(|item| {
                    (item.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| item.get("text").and_then(Value::as_str))
                        .flatten()
                        .map(str::to_owned)
                }),
                _ => None,
            };
            // A tool-only reply still counts as a reply; it just has no
            // line worth previewing.
            Some(Message::Assistant(text.unwrap_or_default()))
        }
        _ => None,
    }
}

fn codex_message(object: &Value) -> Option<Message> {
    if object.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = object.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let text = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .find(|text| !text.trim().is_empty())
        .map(str::to_owned);
    match payload.get("role").and_then(Value::as_str)? {
        "user" => text.filter(|text| is_human_text(text)).map(Message::User),
        "assistant" => Some(Message::Assistant(text.unwrap_or_default())),
        _ => None,
    }
}

/// Text a person typed, not an injected context block or a command echo.
fn is_human_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.is_empty()
        && !trimmed.starts_with('<')
        && !trimmed.starts_with("# AGENTS.md")
        && !trimmed.starts_with("Caveat:")
}

/// One calm line: whitespace collapsed, cut at a character boundary.
fn preview_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_CHARS {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(PREVIEW_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// Codex keeps titles outside the rollout (its index and state database);
/// one batched lookup covers every Codex candidate.
fn apply_codex_titles(home: &Path, valid: &mut [Candidate]) {
    let ids: Vec<String> = valid
        .iter()
        .filter(|candidate| candidate.entry.kind == HistoryKind::Codex)
        .map(|candidate| candidate.entry.id.clone())
        .collect();
    if ids.is_empty() {
        return;
    }
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let titles = history::profile_codex_title_details(None, home, &refs);
    for candidate in valid {
        if let Some(title) = titles.get(&candidate.entry.id) {
            candidate.entry.title = Some(title.title.clone());
        }
    }
}

/// A resumable record for one imported conversation. It names no process:
/// the status is `Exited(Imported)` and the sidebar resumes it on open.
pub(crate) fn imported_record(
    id: &str,
    entry: &diri_proto::HistoryEntry,
    project_id: diri_proto::ProjectId,
) -> diri_proto::SessionRecord {
    let mut record = crate::control::new_record(id, entry.kind.id(), &entry.cwd);
    record.project_id = project_id;
    record.agent_session_id = Some(entry.id.clone());
    record.transcript_path = Some(entry.transcript_path.clone());
    if let Some(title) = entry
        .title
        .as_ref()
        .filter(|title| !title.trim().is_empty())
    {
        record.title.clone_from(title);
        record.title_source = TitleSource::FirstPrompt;
    }
    record.status = SessionStatus::Exited(ExitInfo::imported());
    record.resumability = Resumability::Resumable;
    record.created_at = entry.created_at.unwrap_or(entry.last_active_at);
    record.updated_at = entry.last_active_at;
    record.last_turn_completed_at = Some(entry.last_active_at);
    // Seen: an import is not news, so it must not light up as unread.
    record.last_seen_at = Some(DateMillis::from(SystemTime::now()));
    record
}

/// Re-derive the facts import depends on from the Engine's own reads: the
/// request names a conversation, but its folder, transcript and identity are
/// checked here, never trusted from the client.
pub(crate) fn validate_import(
    home: &Path,
    entry: &diri_proto::HistoryEntry,
    seen: &HashSet<String>,
) -> Result<(), Rejection> {
    let format = match entry.kind.id() {
        AgentKind::CLAUDE_CODE_ID => Format::Claude,
        AgentKind::CODEX_ID => Format::Codex,
        _ => return Err(Rejection::Unreadable),
    };
    check_entry(home, &entry.id, &entry.cwd, seen)?;
    let transcript = Path::new(&entry.transcript_path);
    history::validate_profile_transcript_path(
        None,
        home,
        &entry.kind,
        &entry.id,
        &entry.cwd,
        transcript,
    )
    .ok_or(Rejection::Unreadable)?;
    if transcript_facts(transcript, format)?.sidechain {
        return Err(Rejection::Subagent);
    }
    // A Claude transcript names its folder inside; a request may not move
    // it somewhere else.
    if format_is_claude(format)
        && history::claude_entry(transcript).is_none_or(|found| found.cwd != entry.cwd)
    {
        return Err(Rejection::Unreadable);
    }
    Ok(())
}

fn format_is_claude(format: Format) -> bool {
    matches!(format, Format::Claude)
}

#[cfg(test)]
pub(crate) mod tests;
