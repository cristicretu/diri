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

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

use diri_proto::AgentKind;
use serde_json::Value;

/// Defensive cap on returned entries.
const MAX_ENTRIES: usize = 500;
/// How far into a Claude transcript to read looking for the first line with a
/// `cwd`. That is the first user message, which can be large.
const CLAUDE_HEAD_CAP: usize = 8 << 20;
/// Tail window scanned for the newest Claude `ai-title`.
const CLAUDE_TAIL_BYTES: usize = 16 << 10;
/// Cap on a Codex `session_meta` first line.
const CODEX_FIRST_LINE_CAP: usize = 512 << 10;
const CHUNK: usize = 64 << 10;
const CODEX_ASSOCIATION_DATE_DIRS: usize = 8;
const CODEX_ASSOCIATION_ENTRIES: usize = 1_024;
const CODEX_ASSOCIATION_DIRECTORY_ENTRIES: usize = 1_024;

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

/// A provider transcript whose containment, ownership, file type, and identity
/// were established against the same descriptor consumers must read from.
pub(crate) struct TrustedTranscript {
    path: PathBuf,
    file: File,
}

impl TrustedTranscript {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn latest_claude_ai_title(&mut self) -> Option<String> {
        latest_claude_ai_title_from(&mut self.file)
    }
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

/// Resolve the provider-owned transcript for a live Codex thread. Codex puts
/// the thread id in both the rollout filename and its first `session_meta`
/// record. We inspect only the newest bounded set of strict YYYY/MM/DD
/// directories and require both identities plus the launch cwd to match.
pub(crate) fn find_live_codex_transcript(
    home: &Path,
    agent_id: &str,
    cwd: &str,
) -> Option<TrustedTranscript> {
    if !safe_agent_id(agent_id) {
        return None;
    }
    let root = home.join(".codex/sessions");
    let suffix = format!("-{agent_id}.jsonl");
    let mut seen = 0usize;
    let mut matches = Vec::new();
    for date in newest_codex_date_dirs(&root) {
        let Ok(entries) = std::fs::read_dir(date) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if seen >= CODEX_ASSOCIATION_ENTRIES {
                break;
            }
            seen += 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("rollout-") || !name.ends_with(&suffix) {
                continue;
            }
            let path = entry.path();
            if let Some(transcript) =
                validate_transcript_path(home, &AgentKind::CODEX, agent_id, cwd, &path)
            {
                let modified = transcript
                    .file
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                matches.push((modified, transcript));
            }
        }
        if seen >= CODEX_ASSOCIATION_ENTRIES {
            break;
        }
    }
    matches.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    matches.into_iter().next().map(|(_, path)| path)
}

/// Validate an agent-reported transcript path before it enters SessionRecord.
/// The path must resolve inside the provider's transcript root, must not be a
/// symlink or non-regular file, must belong to this user, and must be tied to
/// the provider conversation identity.
pub(crate) fn validate_transcript_path(
    home: &Path,
    kind: &AgentKind,
    agent_id: &str,
    cwd: &str,
    path: &Path,
) -> Option<TrustedTranscript> {
    if !safe_agent_id(agent_id) {
        return None;
    }
    let root = match kind.id() {
        AgentKind::CLAUDE_CODE_ID => home.join(".claude/projects"),
        AgentKind::CODEX_ID => home.join(".codex/sessions"),
        _ => return None,
    };
    let mut file = open_trusted_regular_file(&root, path)?;
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => {
            let expected = format!("{agent_id}.jsonl");
            (path.file_name()?.to_str()? == expected).then(|| TrustedTranscript {
                path: path.to_path_buf(),
                file,
            })
        }
        AgentKind::CODEX_ID => {
            let (found_id, found_cwd) = codex_identity_from(&mut file)?;
            (found_id == agent_id && found_cwd == cwd).then(|| TrustedTranscript {
                path: path.to_path_buf(),
                file,
            })
        }
        _ => None,
    }
}

fn safe_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn open_trusted_regular_file(root: &Path, path: &Path) -> Option<File> {
    let canonical_root = root.canonicalize().ok()?;
    let file = open_regular_readonly(path)?;
    let opened_metadata = file.metadata().ok()?;
    let link_metadata = std::fs::symlink_metadata(path).ok()?;
    let canonical_path = path.canonicalize().ok()?;
    if link_metadata.file_type().is_symlink()
        || !link_metadata.is_file()
        || !canonical_path.starts_with(&canonical_root)
    {
        return None;
    }
    #[cfg(unix)]
    {
        if opened_metadata.dev() != link_metadata.dev()
            || opened_metadata.ino() != link_metadata.ino()
        {
            return None;
        }
    }
    Some(file)
}

fn newest_codex_date_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dates = Vec::new();
    for year in numeric_child_dirs(root, 4, 1970, 9999, CODEX_ASSOCIATION_DATE_DIRS) {
        for month in numeric_child_dirs(&year, 2, 1, 12, 12) {
            for day in numeric_child_dirs(&month, 2, 1, 31, 31) {
                dates.push(day);
            }
        }
    }
    dates.sort_by(|left, right| right.cmp(left));
    dates.truncate(CODEX_ASSOCIATION_DATE_DIRS);
    dates
}

fn numeric_child_dirs(
    root: &Path,
    width: usize,
    minimum: u16,
    maximum: u16,
    keep: usize,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut children = entries
        .take(CODEX_ASSOCIATION_DIRECTORY_ENTRIES)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let number = name.parse::<u16>().ok()?;
            (name.len() == width && (minimum..=maximum).contains(&number))
                .then(|| (number, entry.path()))
        })
        .collect::<Vec<_>>();
    children.sort_by_key(|(number, _)| std::cmp::Reverse(*number));
    children
        .into_iter()
        .take(keep)
        .map(|(_, path)| path)
        .collect()
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
    let Some(mut handle) = open_regular_readonly(path) else {
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
    let mut handle = open_regular_readonly(path)?;
    latest_claude_ai_title_from(&mut handle)
}

fn latest_claude_ai_title_from(handle: &mut File) -> Option<String> {
    let end = handle.seek(SeekFrom::End(0)).ok()?;
    let start = end.saturating_sub(CLAUDE_TAIL_BYTES as u64);
    handle.seek(SeekFrom::Start(start)).ok()?;

    let mut data = Vec::new();
    handle
        .by_ref()
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
    let mut handle = open_regular_readonly(path)?;
    let (id, cwd) = codex_identity_from(&mut handle)?;
    let metadata = handle.metadata().ok()?;

    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let created = metadata.created().ok();
    Some(HistoryEntry {
        id,
        kind: HistoryKind::Codex,
        cwd_exists: Path::new(&cwd).exists(),
        cwd,
        // Codex has no ai-title record; the caller can fall back to a
        // placeholder built from cwd and date.
        title: None,
        transcript_path: path.to_string_lossy().to_string(),
        last_active_at: modified,
        created_at: created,
    })
}

fn codex_identity_from(handle: &mut File) -> Option<(String, String)> {
    handle.seek(SeekFrom::Start(0)).ok()?;
    let first = read_first_line_from(handle, CODEX_FIRST_LINE_CAP)?;
    let object: Value = serde_json::from_str(&first).ok()?;
    if object.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = object.get("payload")?;
    let id = payload.get("id").and_then(Value::as_str)?.to_string();
    let cwd = payload.get("cwd").and_then(Value::as_str)?.to_string();
    if cwd.is_empty() {
        return None;
    }
    Some((id, cwd))
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

fn read_first_line_from(handle: &mut File, cap: usize) -> Option<String> {
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

/// Provider history is local user data. A nonblocking, no-follow open keeps a
/// malformed or raced path from turning any history/metadata read into a FIFO
/// wait or symlink traversal; metadata and bytes come from the same handle.
fn open_regular_readonly(path: &Path) -> Option<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    Some(file)
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
    fn live_codex_transcript_resolution_is_bounded_and_identity_associated() {
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join(".codex/sessions");
        write(
            &root.join("2026/08/12/rollout-old-thread-9.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\",\"cwd\":\"/tmp\"}}\n",
        );
        let expected = root.join("2026/08/13/rollout-now-thread-9.jsonl");
        write(
            &expected,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-9\",\"cwd\":\"/work/app\"}}\n",
        );

        assert_eq!(
            find_live_codex_transcript(home.path(), "thread-9", "/work/app")
                .map(|transcript| transcript.path().to_path_buf()),
            Some(expected)
        );
        assert!(find_live_codex_transcript(home.path(), "thread-9", "/wrong").is_none());
        assert!(find_live_codex_transcript(home.path(), "../escape", "/work/app").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn transcript_validation_rejects_symlinks_and_paths_outside_provider_roots() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        let outside = home.path().join("outside.jsonl");
        write(
            &outside,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-9\",\"cwd\":\"/tmp\"}}\n",
        );
        let link = home
            .path()
            .join(".codex/sessions/2026/08/13/rollout-now-thread-9.jsonl");
        std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        symlink(&outside, &link).expect("symlink");

        assert!(
            validate_transcript_path(home.path(), &AgentKind::CODEX, "thread-9", "/tmp", &link,)
                .is_none()
        );
        assert!(
            validate_transcript_path(home.path(), &AgentKind::CODEX, "thread-9", "/tmp", &outside,)
                .is_none()
        );

        let fifo = link.with_file_name("rollout-fifo-thread-9.jsonl");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("fifo path");
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(
            validate_transcript_path(home.path(), &AgentKind::CODEX, "thread-9", "/tmp", &fifo,)
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn trusted_transcript_reads_the_validated_inode_after_path_replacement() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        let agent_id = "0199f2c4-1a2b-4c3d-8e9f-000000000099";
        let path = home
            .path()
            .join(".claude/projects/-tmp")
            .join(format!("{agent_id}.jsonl"));
        write(
            &path,
            "{\"type\":\"ai-title\",\"aiTitle\":\"validated inode\"}\n",
        );
        let mut trusted = validate_transcript_path(
            home.path(),
            &AgentKind::CLAUDE_CODE,
            agent_id,
            "/tmp",
            &path,
        )
        .expect("trusted transcript");

        let moved = path.with_extension("validated");
        std::fs::rename(&path, &moved).expect("move validated inode");
        let replacement = home.path().join("replacement.jsonl");
        write(
            &replacement,
            "{\"type\":\"ai-title\",\"aiTitle\":\"swapped path\"}\n",
        );
        symlink(&replacement, &path).expect("swap final component");

        assert_eq!(
            trusted.latest_claude_ai_title().as_deref(),
            Some("validated inode")
        );
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
}
