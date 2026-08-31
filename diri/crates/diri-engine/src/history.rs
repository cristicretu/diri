//! Past conversations, read from the agents' own transcript stores.
//!
//! This is what makes resume work across an app quit or a daemon restart: the
//! `.jsonl` files outlive both, so a conversation can be recovered even when no
//! session record survived.
//!
//! - **Claude Code** — `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`. The
//!   filename stem is the id `claude --resume` takes. The authoritative `cwd`
//!   and title come from the transcript's own lines, because the
//!   directory-name encoding is lossy.
//! - **Codex** — `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`,
//!   whose first line is a `session_meta` record with the id and cwd.
//!
//! Every read here is bounded. Transcripts run to many megabytes — one with a
//! pasted image can be enormous — and the scan happens while a user waits.
//!
//! Ported from the Swift `HistoryScanner`.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// Defensive cap on returned entries.
const MAX_ENTRIES: usize = 500;
/// How far into a Claude transcript to read looking for the first line with a
/// `cwd`. That is the first user message, which can be large.
const CLAUDE_HEAD_CAP: usize = 8 << 20;
/// Tail window scanned for the newest Claude `ai-title`.
const CLAUDE_TAIL_BYTES: usize = 16 << 10;
/// Tail window for Cursor's last jsonl object (turn working/idle).
const CURSOR_TAIL_BYTES: usize = 64 << 10;
/// Cap on a Codex `session_meta` first line.
const CODEX_FIRST_LINE_CAP: usize = 512 << 10;
const CHUNK: usize = 64 << 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryKind {
    ClaudeCode,
    Codex,
}

impl HistoryKind {
    pub fn id(&self) -> &'static str {
        match self {
            HistoryKind::ClaudeCode => "claude-code",
            HistoryKind::Codex => "codex",
        }
    }
}

/// One resumable past conversation.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    /// The agent-side conversation id, which is what `--resume` takes.
    pub id: String,
    pub kind: HistoryKind,
    pub cwd: String,
    pub title: Option<String>,
    pub transcript_path: String,
    pub last_active_at: SystemTime,
    pub created_at: Option<SystemTime>,
    /// False when the directory the conversation ran in is gone — resuming
    /// there would fail, and the UI should say so rather than offer it.
    pub cwd_exists: bool,
}

/// Scans both stores under `home`, newest first, skipping ids already tracked.
pub fn scan(home: &Path, tracked: &[String]) -> Vec<HistoryEntry> {
    scan_roots(
        &home.join(".claude/projects"),
        &home.join(".codex/sessions"),
        tracked,
    )
}

/// Root-injectable core, so tests can point at fixture trees.
pub fn scan_roots(claude_root: &Path, codex_root: &Path, tracked: &[String]) -> Vec<HistoryEntry> {
    let mut entries = scan_claude(claude_root);
    entries.extend(scan_codex(codex_root));

    let mut seen: std::collections::HashSet<String> = tracked.iter().cloned().collect();
    let mut deduped = Vec::new();
    for entry in entries {
        if seen.insert(entry.id.clone()) {
            deduped.push(entry);
        }
    }
    deduped.sort_by_key(|entry| std::cmp::Reverse(entry.last_active_at));
    deduped.truncate(MAX_ENTRIES);
    deduped
}

// MARK: Claude

fn scan_claude(root: &Path) -> Vec<HistoryEntry> {
    let Ok(project_dirs) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for project in project_dirs.filter_map(Result::ok) {
        if !project.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.filter_map(Result::ok) {
            let path = file.path();
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(entry) = claude_entry(&path)
            {
                result.push(entry);
            }
        }
    }
    result
}

fn claude_entry(path: &Path) -> Option<HistoryEntry> {
    // The filename stem is the id `claude --resume` resolves against.
    let uuid = path.file_stem()?.to_string_lossy().to_string();
    if uuid.len() < 32 {
        return None;
    }

    let (modified, created) = timestamps(path);

    let mut cwd = None;
    let mut first_prompt = None;
    for line in read_claude_head(path) {
        let Ok(object) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if first_prompt.is_none() {
            first_prompt = claude_user_text(&object);
        }
        if let Some(found) = object.get("cwd").and_then(Value::as_str)
            && !found.is_empty()
        {
            cwd = Some(found.to_string());
            break;
        }
    }
    // No cwd means nothing to resume into.
    let cwd = cwd?;

    let title = latest_claude_ai_title(path).or_else(|| first_prompt.map(|text| title_from(&text)));

    Some(HistoryEntry {
        id: uuid,
        kind: HistoryKind::ClaudeCode,
        cwd_exists: Path::new(&cwd).exists(),
        cwd,
        title,
        transcript_path: path.to_string_lossy().to_string(),
        last_active_at: modified,
        created_at: created,
    })
}

/// The user prompt text from a Claude `user` line, if it is one.
fn claude_user_text(object: &Value) -> Option<String> {
    if object.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = object.get("message")?.get("content")?;
    if let Some(text) = content.as_str()
        && !text.is_empty()
    {
        return Some(text.to_string());
    }
    // Content can also be a list of typed parts.
    content.as_array()?.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| item.get("text").and_then(Value::as_str))
            .flatten()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

/// Reads until the first line containing `"cwd"` is complete, or the cap is
/// reached. Stopping early matters: the alternative is reading megabytes of
/// transcript to learn one field.
fn read_claude_head(path: &Path) -> Vec<String> {
    let Ok(mut handle) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut data: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; CHUNK];

    while data.len() < CLAUDE_HEAD_CAP {
        let want = CHUNK.min(CLAUDE_HEAD_CAP - data.len());
        match handle.read(&mut buffer[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buffer[..n]),
        }
        if let Some(position) = find(&data, b"\"cwd\"")
            && data[position..].contains(&b'\n')
        {
            break; // the cwd-bearing line is complete
        }
    }
    String::from_utf8_lossy(&data)
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The newest `ai-title` in the tail — the title Claude generated for the
/// conversation.
pub(crate) fn latest_claude_ai_title(path: &Path) -> Option<String> {
    let mut handle = std::fs::File::open(path).ok()?;
    let end = handle.seek(SeekFrom::End(0)).ok()?;
    let start = end.saturating_sub(CLAUDE_TAIL_BYTES as u64);
    handle.seek(SeekFrom::Start(start)).ok()?;

    let mut data = Vec::new();
    handle
        .take((CLAUDE_TAIL_BYTES + (4 << 10)) as u64)
        .read_to_end(&mut data)
        .ok()?;

    let mut newest = None;
    for line in String::from_utf8_lossy(&data).split('\n') {
        if !line.contains("\"ai-title\"") {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("ai-title")
            && let Some(title) = object.get("aiTitle").and_then(Value::as_str)
            && !title.is_empty()
        {
            newest = Some(title.to_string());
        }
    }
    newest
}

/// Cursor conversation identity plus the generated title Cursor writes to
/// `~/.cursor/chats/<workspace>/<id>/meta.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CursorConversation {
    pub id: String,
    pub title: Option<String>,
    pub transcript_path: Option<String>,
}

/// Last complete jsonl object in a Cursor transcript: in-flight or done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorTranscriptTurn {
    Working,
    Idle,
}

/// Cursor writes `tool_use` while a turn is in flight and a text-only
/// assistant line (or `turn_ended`) when it is done. `turn_ended` is the
/// whole conversation, not each turn, so the last object is the signal.
pub(crate) fn cursor_transcript_turn(path: &Path) -> Option<CursorTranscriptTurn> {
    classify_cursor_transcript_object(&last_jsonl_object(path)?)
}

fn classify_cursor_transcript_object(object: &Value) -> Option<CursorTranscriptTurn> {
    if object.get("type").and_then(Value::as_str) == Some("turn_ended") {
        return Some(CursorTranscriptTurn::Idle);
    }
    match object.get("role").and_then(Value::as_str) {
        Some("user") => Some(CursorTranscriptTurn::Working),
        Some("assistant") => Some(if assistant_uses_tool(object) {
            CursorTranscriptTurn::Working
        } else {
            CursorTranscriptTurn::Idle
        }),
        _ => None,
    }
}

fn assistant_uses_tool(object: &Value) -> bool {
    object
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_use"))
        })
}

fn last_jsonl_object(path: &Path) -> Option<Value> {
    let mut handle = std::fs::File::open(path).ok()?;
    let end = handle.seek(SeekFrom::End(0)).ok()?;
    if end == 0 {
        return None;
    }
    let start = end.saturating_sub(CURSOR_TAIL_BYTES as u64);
    handle.seek(SeekFrom::Start(start)).ok()?;
    let mut data = Vec::new();
    handle
        .take((CURSOR_TAIL_BYTES + (4 << 10)) as u64)
        .read_to_end(&mut data)
        .ok()?;
    let text = String::from_utf8_lossy(&data);
    let mut lines = text.split('\n').filter(|line| !line.is_empty());
    if start > 0 {
        lines.next();
    }
    let mut newest = None;
    for line in lines {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            newest = Some(value);
        }
    }
    newest
}

/// `~/.cursor/projects/<slug>/` slug for a working directory: leading slash
/// stripped, remaining `/` replaced by `-`.
pub(crate) fn cursor_project_slug(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .trim_start_matches('/')
        .replace('/', "-")
}

/// Resolve a Cursor conversation and its generated title.
///
/// `conversation_id` wins when the caller already knows it (hook payload).
/// Otherwise the newest unclaimed transcript under the cwd's project whose
/// `createdAtMs` is at or after `created_after_ms` is chosen.
pub(crate) fn cursor_conversation(
    home: &Path,
    cwd: &str,
    conversation_id: Option<&str>,
    created_after_ms: f64,
    claimed: &HashSet<String>,
) -> Option<CursorConversation> {
    if let Some(id) = conversation_id.filter(|id| is_cursor_conversation_id(id)) {
        return Some(read_cursor_conversation(home, cwd, id));
    }
    discover_cursor_conversation(home, cwd, created_after_ms, claimed)
}

fn is_cursor_conversation_id(id: &str) -> bool {
    (32..80).contains(&id.len())
        && id
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-')
}

fn read_cursor_conversation(home: &Path, cwd: &str, id: &str) -> CursorConversation {
    let transcript = cursor_transcript_path(home, cwd, id).filter(|path| path.is_file());
    CursorConversation {
        title: cursor_meta_title(home, id),
        transcript_path: transcript.map(|path| path.to_string_lossy().into_owned()),
        id: id.to_owned(),
    }
}

fn discover_cursor_conversation(
    home: &Path,
    cwd: &str,
    created_after_ms: f64,
    claimed: &HashSet<String>,
) -> Option<CursorConversation> {
    let transcripts = cursor_transcripts_dir(home, cwd)?;
    let Ok(entries) = std::fs::read_dir(&transcripts) else {
        return None;
    };
    let floor = created_after_ms - 2_000.0;
    let mut best: Option<(f64, CursorConversation)> = None;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if claimed.contains(&id) || !is_cursor_conversation_id(&id) {
            continue;
        }
        let conversation = read_cursor_conversation(home, cwd, &id);
        let created = cursor_meta_created_ms(home, &id).unwrap_or_else(|| {
            std::fs::metadata(&path)
                .and_then(|meta| meta.created().or_else(|_| meta.modified()))
                .ok()
                .map(system_time_ms)
                .unwrap_or(0.0)
        });
        if created < floor {
            continue;
        }
        let better = best.as_ref().is_none_or(|(best_created, _)| {
            (created - created_after_ms).abs() < (best_created - created_after_ms).abs()
        });
        if better {
            best = Some((created, conversation));
        }
    }
    best.map(|(_, conversation)| conversation)
}

fn cursor_transcripts_dir(home: &Path, cwd: &str) -> Option<PathBuf> {
    let dir = home
        .join(".cursor/projects")
        .join(cursor_project_slug(cwd))
        .join("agent-transcripts");
    confined_dir(home.join(".cursor"), &dir)
}

fn cursor_transcript_path(home: &Path, cwd: &str, id: &str) -> Option<PathBuf> {
    let path = cursor_transcripts_dir(home, cwd)?
        .join(id)
        .join(format!("{id}.jsonl"));
    confined_file(home.join(".cursor"), &path)
}

fn cursor_meta_title(home: &Path, id: &str) -> Option<String> {
    cursor_meta(home, id)?
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

fn cursor_meta_created_ms(home: &Path, id: &str) -> Option<f64> {
    cursor_meta(home, id)?
        .get("createdAtMs")
        .and_then(Value::as_f64)
}

fn cursor_meta(home: &Path, id: &str) -> Option<Value> {
    let chats = home.join(".cursor/chats");
    let Ok(workspaces) = std::fs::read_dir(&chats) else {
        return None;
    };
    for workspace in workspaces.filter_map(Result::ok) {
        let meta = workspace.path().join(id).join("meta.json");
        let Some(meta) = confined_file(&chats, &meta) else {
            continue;
        };
        let bytes = std::fs::read(&meta).ok()?;
        return serde_json::from_slice(&bytes).ok();
    }
    None
}

fn confined_dir(root: impl AsRef<Path>, path: &Path) -> Option<PathBuf> {
    let root = root.as_ref().canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

fn confined_file(root: impl AsRef<Path>, path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    confined_dir(root, path)
}

fn system_time_ms(time: SystemTime) -> f64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// MARK: Codex

fn scan_codex(root: &Path) -> Vec<HistoryEntry> {
    let mut result = Vec::new();
    // A bounded YYYY/MM/DD walk, not a general recursive one.
    for year in child_dirs(root) {
        for month in child_dirs(&year) {
            for day in child_dirs(&month) {
                let Ok(files) = std::fs::read_dir(&day) else {
                    continue;
                };
                for file in files.filter_map(Result::ok) {
                    let path = file.path();
                    let is_rollout = path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("rollout-"));
                    if is_rollout
                        && path.extension().is_some_and(|ext| ext == "jsonl")
                        && let Some(entry) = codex_entry(&path)
                    {
                        result.push(entry);
                    }
                }
            }
        }
    }
    result
}

fn codex_entry(path: &Path) -> Option<HistoryEntry> {
    let first = read_first_line(path, CODEX_FIRST_LINE_CAP)?;
    let object: Value = serde_json::from_str(&first).ok()?;
    if object.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = object.get("payload")?;
    let id = payload.get("id").and_then(Value::as_str)?.to_string();
    let cwd = payload.get("cwd").and_then(Value::as_str)?;
    if cwd.is_empty() {
        return None;
    }

    let (modified, created) = timestamps(path);
    Some(HistoryEntry {
        id,
        kind: HistoryKind::Codex,
        cwd_exists: Path::new(cwd).exists(),
        cwd: cwd.to_string(),
        // Codex has no ai-title record; the caller can fall back to a
        // placeholder built from cwd and date.
        title: None,
        transcript_path: path.to_string_lossy().to_string(),
        last_active_at: modified,
        created_at: created,
    })
}

// MARK: Shared

fn child_dirs(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn read_first_line(path: &Path, cap: usize) -> Option<String> {
    let mut handle = std::fs::File::open(path).ok()?;
    let mut data: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; CHUNK];

    while data.len() < cap {
        let want = CHUNK.min(cap - data.len());
        match handle.read(&mut buffer[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buffer[..n]),
        }
        if let Some(newline) = data.iter().position(|&byte| byte == b'\n') {
            return Some(String::from_utf8_lossy(&data[..newline]).to_string());
        }
    }
    (!data.is_empty()).then(|| String::from_utf8_lossy(&data).to_string())
}

fn timestamps(path: &Path) -> (SystemTime, Option<SystemTime>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return (SystemTime::UNIX_EPOCH, None);
    };
    (
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        metadata.created().ok(),
    )
}

/// One line, trimmed, for a sidebar row.
fn title_from(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    first_line
        .chars()
        .take(60)
        .collect::<String>()
        .trim()
        .to_string()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let mut file = std::fs::File::create(path).expect("create");
        file.write_all(contents.as_bytes()).expect("write");
    }

    fn claude_transcript(cwd: &str, prompt: &str, ai_title: Option<&str>) -> String {
        let mut lines = vec![
            format!(r#"{{"type":"user","cwd":"{cwd}","message":{{"content":"{prompt}"}}}}"#),
            r#"{"type":"assistant","message":{"content":"working"}}"#.to_string(),
        ];
        if let Some(title) = ai_title {
            lines.push(format!(r#"{{"type":"ai-title","aiTitle":"{title}"}}"#));
        }
        lines.join("\n") + "\n"
    }

    #[test]
    fn a_claude_transcript_yields_a_resumable_entry() {
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        let uuid = "0199f2c4-1a2b-4c3d-8e9f-000000000001";
        write(
            &claude.join(format!("-Users-someone-project/{uuid}.jsonl")),
            &claude_transcript("/tmp", "Fix the login bug", None),
        );

        let entries = scan_roots(&claude, &temp.path().join("codex"), &[]);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(
            entry.id, uuid,
            "the id is the filename stem, what --resume takes"
        );
        assert_eq!(entry.kind, HistoryKind::ClaudeCode);
        assert_eq!(
            entry.cwd, "/tmp",
            "cwd comes from the transcript, not the lossy directory name"
        );
        assert_eq!(entry.title.as_deref(), Some("Fix the login bug"));
        assert!(entry.cwd_exists);
    }

    #[test]
    fn an_ai_title_wins_over_the_first_prompt() {
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        write(
            &claude.join("-p/0199f2c4-1a2b-4c3d-8e9f-000000000002.jsonl"),
            &claude_transcript("/tmp", "vague first prompt", Some("Refactor the parser")),
        );

        let entries = scan_roots(&claude, &temp.path().join("codex"), &[]);
        assert_eq!(entries[0].title.as_deref(), Some("Refactor the parser"));
    }

    #[test]
    fn a_transcript_without_a_cwd_is_not_resumable() {
        // There is nowhere to resume it into, so offering it would only fail.
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        write(
            &claude.join("-p/0199f2c4-1a2b-4c3d-8e9f-000000000003.jsonl"),
            "{\"type\":\"assistant\",\"message\":{\"content\":\"hi\"}}\n",
        );

        assert!(scan_roots(&claude, &temp.path().join("codex"), &[]).is_empty());
    }

    #[test]
    fn a_missing_cwd_directory_is_flagged_rather_than_hidden() {
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        write(
            &claude.join("-p/0199f2c4-1a2b-4c3d-8e9f-000000000004.jsonl"),
            &claude_transcript("/no/such/directory", "hello", None),
        );

        let entries = scan_roots(&claude, &temp.path().join("codex"), &[]);
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].cwd_exists,
            "the UI needs to know resuming here would fail"
        );
    }

    #[test]
    fn a_short_filename_is_not_a_session_id() {
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        write(
            &claude.join("-p/notes.jsonl"),
            &claude_transcript("/tmp", "hello", None),
        );
        assert!(scan_roots(&claude, &temp.path().join("codex"), &[]).is_empty());
    }

    #[test]
    fn a_codex_rollout_yields_an_entry_from_its_session_meta() {
        let temp = tempfile::tempdir().expect("temp");
        let codex = temp.path().join("codex");
        write(
            &codex.join("2026/08/05/rollout-2026-08-05T01-00-00-thread-9.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-9\",\"cwd\":\"/tmp\"}}\n\
             {\"type\":\"user_message\",\"payload\":{\"text\":\"do the thing\"}}\n",
        );

        let entries = scan_roots(&temp.path().join("claude"), &codex, &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "thread-9");
        assert_eq!(entries[0].kind, HistoryKind::Codex);
        assert_eq!(entries[0].cwd, "/tmp");
    }

    #[test]
    fn a_file_that_is_not_a_rollout_is_ignored() {
        let temp = tempfile::tempdir().expect("temp");
        let codex = temp.path().join("codex");
        write(
            &codex.join("2026/08/05/notes.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"cwd\":\"/tmp\"}}\n",
        );
        assert!(scan_roots(&temp.path().join("claude"), &codex, &[]).is_empty());
    }

    #[test]
    fn already_tracked_conversations_are_excluded() {
        // A conversation the daemon is already running must not also appear as
        // something to resume.
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        let uuid = "0199f2c4-1a2b-4c3d-8e9f-000000000005";
        write(
            &claude.join(format!("-p/{uuid}.jsonl")),
            &claude_transcript("/tmp", "live one", None),
        );

        let entries = scan_roots(&claude, &temp.path().join("codex"), &[uuid.to_string()]);
        assert!(entries.is_empty());
    }

    #[test]
    fn entries_come_back_newest_first() {
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        for (index, uuid) in [
            "0199f2c4-1a2b-4c3d-8e9f-00000000000a",
            "0199f2c4-1a2b-4c3d-8e9f-00000000000b",
        ]
        .iter()
        .enumerate()
        {
            let path = claude.join(format!("-p/{uuid}.jsonl"));
            write(&path, &claude_transcript("/tmp", "hello", None));
            // Stagger modification times.
            let when =
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + index as u64 * 60);
            let times = std::fs::FileTimes::new().set_modified(when);
            let file = std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open");
            file.set_times(times).expect("set times");
        }

        let entries = scan_roots(&claude, &temp.path().join("codex"), &[]);
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].last_active_at > entries[1].last_active_at,
            "newest first"
        );
    }

    /// Scans the machine's real transcript stores. Read-only, but ignored by
    /// default because it depends on what happens to be on disk.
    ///
    /// ```sh
    /// DIRI_INTEROP_HISTORY=1 cargo test -p diri-engine -- --ignored
    /// ```
    #[test]
    #[ignore = "set DIRI_INTEROP_HISTORY=1 to scan the real transcript stores"]
    fn reads_the_real_transcript_stores() {
        if std::env::var("DIRI_INTEROP_HISTORY").is_err() {
            eprintln!("skipped: DIRI_INTEROP_HISTORY is not set");
            return;
        }
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let started = std::time::Instant::now();
        let entries = scan(&home, &[]);
        let elapsed = started.elapsed();

        eprintln!(
            "scanned {} conversations in {elapsed:?} ({} claude, {} codex)",
            entries.len(),
            entries
                .iter()
                .filter(|e| e.kind == HistoryKind::ClaudeCode)
                .count(),
            entries
                .iter()
                .filter(|e| e.kind == HistoryKind::Codex)
                .count(),
        );
        assert!(!entries.is_empty(), "expected some history on this machine");
        for entry in entries.iter().take(3) {
            eprintln!("  {} {:?} {}", entry.id, entry.title, entry.cwd);
            assert!(!entry.cwd.is_empty());
            assert!(Path::new(&entry.transcript_path).exists());
        }
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "a scan a user waits on should not take {elapsed:?}"
        );
    }

    #[test]
    fn missing_roots_are_not_an_error() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(
            scan_roots(
                &temp.path().join("nope"),
                &temp.path().join("also-nope"),
                &[]
            )
            .is_empty()
        );
    }

    #[test]
    fn the_head_read_stops_at_the_cwd_line_rather_than_reading_everything() {
        // A transcript with a huge tail must not cost megabytes of reading to
        // learn its cwd.
        let temp = tempfile::tempdir().expect("temp");
        let claude = temp.path().join("claude");
        let path = claude.join("-p/0199f2c4-1a2b-4c3d-8e9f-00000000000c.jsonl");
        let mut contents = claude_transcript("/tmp", "small head", None);
        contents.push_str(&"{\"type\":\"filler\",\"data\":\"x\"}\n".repeat(50_000));
        write(&path, &contents);

        let lines = read_claude_head(&path);
        assert!(
            lines.len() < 5_000,
            "stopped early rather than reading the whole file: {} lines",
            lines.len()
        );
        let entries = scan_roots(&claude, &temp.path().join("codex"), &[]);
        assert_eq!(entries[0].cwd, "/tmp");
    }

    #[test]
    fn a_cursor_meta_title_is_the_generated_conversation_name() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path();
        let id = "11111fcb-7655-4342-8b2f-88068c650200";
        let cwd = "/Users/alex/GitHub/diri";
        write_cursor_chat(home, cwd, id, "Cursor Integration Fix", 1_788_110_437_802.0);

        let conversation =
            cursor_conversation(home, cwd, Some(id), 0.0, &HashSet::new()).expect("found");
        assert_eq!(conversation.id, id);
        assert_eq!(
            conversation.title.as_deref(),
            Some("Cursor Integration Fix")
        );
    }

    #[test]
    fn an_unclaimed_cursor_transcript_is_matched_to_the_session_cwd() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path();
        let cwd = "/Users/alex/GitHub/diri";
        let claimed = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".to_owned();
        write_cursor_chat(home, cwd, &claimed, "Old chat", 1_000.0);
        write_cursor_chat(
            home,
            cwd,
            "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb",
            "Cursor Integration Fix",
            5_000.0,
        );

        let conversation = cursor_conversation(home, cwd, None, 4_500.0, &HashSet::from([claimed]))
            .expect("found");
        assert_eq!(conversation.id, "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb");
        assert_eq!(
            conversation.title.as_deref(),
            Some("Cursor Integration Fix")
        );
    }

    #[test]
    fn cursor_transcript_last_object_is_working_or_idle() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("chat.jsonl");

        write(
            &path,
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}
"#,
        );
        assert_eq!(
            cursor_transcript_turn(&path),
            Some(CursorTranscriptTurn::Working)
        );

        write(
            &path,
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}
{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Grep"}]}}
"#,
        );
        assert_eq!(
            cursor_transcript_turn(&path),
            Some(CursorTranscriptTurn::Working)
        );

        write(
            &path,
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Grep"}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"done"}]}}
"#,
        );
        assert_eq!(
            cursor_transcript_turn(&path),
            Some(CursorTranscriptTurn::Idle)
        );

        write(
            &path,
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"done"}]}}
{"type":"turn_ended","status":"success"}
"#,
        );
        assert_eq!(
            cursor_transcript_turn(&path),
            Some(CursorTranscriptTurn::Idle)
        );
    }

    fn write_cursor_chat(home: &Path, cwd: &str, id: &str, title: &str, created_at_ms: f64) {
        let transcripts = home
            .join(".cursor/projects")
            .join(cursor_project_slug(cwd))
            .join("agent-transcripts")
            .join(id);
        std::fs::create_dir_all(&transcripts).expect("transcripts");
        std::fs::write(transcripts.join(format!("{id}.jsonl")), "{}\n").expect("jsonl");
        let meta_dir = home.join(".cursor/chats/workspace").join(id);
        std::fs::create_dir_all(&meta_dir).expect("meta");
        std::fs::write(
            meta_dir.join("meta.json"),
            format!(
                r#"{{"schemaVersion":1,"createdAtMs":{created_at_ms},"title":"{title}","cwd":"{cwd}"}}"#
            ),
        )
        .expect("meta");
    }
}
