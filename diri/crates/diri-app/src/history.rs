//! Read-only discovery of Claude and Codex transcripts.
//!
//! Local history stays available while the Engine connects. Streaming reads
//! stop at useful metadata; unchanged transcripts are reused between scans.
//! Provider files are never modified.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText};
use diri_client::{ClientError, DaemonClient};
use diri_proto::{AgentKind, DateMillis, HistoryEntry};
use serde_json::Value;

const CLAUDE_HEAD_CAP: usize = 8 << 20;
const CLAUDE_TAIL_BYTES: usize = 16 << 10;
const CODEX_FIRST_LINE_CAP: usize = 512 << 10;
const CODEX_FIRST_PROMPT_CAP: usize = 8 << 20;

#[derive(Clone, Debug)]
pub struct HistoryRoots {
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl HistoryRoots {
    pub fn in_home(home: &Path) -> Self {
        Self {
            claude: home.join(".claude/projects"),
            codex: home.join(".codex/sessions"),
        }
    }

    pub fn current_user() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        Self::in_home(&home)
    }
}

/// Scan both transcript stores, excluding agent conversation ids already
/// represented by daemon sessions. Results are newest-first and deduplicated.
#[cfg(test)]
pub fn scan(roots: &HistoryRoots, tracked: &HashSet<String>) -> Vec<HistoryEntry> {
    HistoryScanner::default().scan(roots, tracked)
}

#[derive(Default)]
pub struct HistoryScanner {
    files: HashMap<PathBuf, CachedTranscript>,
    visited: HashSet<PathBuf>,
}

struct CachedTranscript {
    modified: Option<SystemTime>,
    len: u64,
    provider_title: Option<String>,
    entry: Option<HistoryEntry>,
}

impl HistoryScanner {
    pub fn scan(&mut self, roots: &HistoryRoots, tracked: &HashSet<String>) -> Vec<HistoryEntry> {
        self.visited.clear();
        let mut entries = scan_claude(&roots.claude, self);
        entries.extend(scan_codex(&roots.codex, self));
        self.files.retain(|path, _| self.visited.contains(path));

        let mut seen = tracked.clone();
        entries.retain(|entry| seen.insert(entry.id.clone()));
        entries.sort_by(|left, right| {
            right
                .last_active_at
                .partial_cmp(&left.last_active_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.id.cmp(&right.id))
        });
        entries
    }

    fn read(
        &mut self,
        path: &Path,
        titles: Option<&HashMap<String, String>>,
        read: impl FnOnce() -> Option<HistoryEntry>,
    ) -> Option<HistoryEntry> {
        let metadata = fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        self.visited.insert(path.to_owned());
        let modified = metadata.modified().ok();
        if let Some(cached) = self.files.get(path)
            && modified.is_some()
            && cached.modified == modified
            && cached.len == metadata.len()
            && cached.provider_title.as_ref()
                == cached
                    .entry
                    .as_ref()
                    .and_then(|entry| titles.and_then(|titles| titles.get(&entry.id)))
        {
            let mut entry = cached.entry.clone()?;
            entry.cwd_exists = Path::new(&entry.cwd).is_dir();
            return Some(entry);
        }
        let entry = read();
        let provider_title = entry
            .as_ref()
            .and_then(|entry| titles.and_then(|titles| titles.get(&entry.id)))
            .cloned();
        self.files.insert(
            path.to_owned(),
            CachedTranscript {
                modified,
                len: metadata.len(),
                provider_title,
                entry: entry.clone(),
            },
        );
        entry
    }
}

/// Resume a durable conversation through the first-class Engine path. The
/// typed request preserves the Agent kind, conversation id, and transcript
/// path on the new record so another machine or Engine restart can resume it
/// again instead of degrading it into a one-shot generic shell command.
pub async fn resume(
    client: &DaemonClient,
    entry: &HistoryEntry,
) -> Result<diri_proto::SessionId, ClientError> {
    if !entry.cwd_exists || !Path::new(&entry.cwd).is_dir() {
        return Err(ClientError::Control(diri_proto::ControlError::bad_request(
            "the conversation folder is no longer available",
        )));
    }
    if !matches!(
        entry.kind.id(),
        AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID
    ) {
        return Err(ClientError::Control(diri_proto::ControlError::bad_request(
            "the conversation's agent cannot resume",
        )));
    }
    client
        .resume_from_history_with_prompt(entry.clone(), None)
        .await
        .map(|record| record.id)
}

#[derive(Default)]
pub struct HistorySearch {
    candidates: Vec<PreparedText>,
    matcher: FuzzyMatcher,
}

impl HistorySearch {
    pub fn rebuild(&mut self, entries: &[HistoryEntry]) {
        self.candidates = entries
            .iter()
            .map(|entry| {
                PreparedText::new(&format!(
                    "{} {} {} {}",
                    entry.title.as_deref().unwrap_or_default(),
                    entry.cwd,
                    entry.kind.id(),
                    entry.id
                ))
            })
            .collect();
    }

    /// Query parsing and candidate preparation happen once, outside rendering.
    /// Stable ties preserve newest-first order; words can match in any order.
    pub fn rank(&mut self, query: &str) -> Vec<usize> {
        let query = FuzzyQuery::new(query);
        if query.is_empty() {
            return (0..self.candidates.len()).collect();
        }
        let mut matches = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                query
                    .score(candidate, &mut self.matcher)
                    .map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        matches.into_iter().map(|(index, _)| index).collect()
    }
}

fn scan_claude(root: &Path, scanner: &mut HistoryScanner) -> Vec<HistoryEntry> {
    let mut result = Vec::new();
    for project in child_dirs(root) {
        let Ok(files) = fs::read_dir(project) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(entry) = scanner.read(&path, None, || claude_entry(&path)) {
                result.push(entry);
            }
        }
    }
    result
}

fn claude_entry(path: &Path) -> Option<HistoryEntry> {
    let id = path.file_stem()?.to_str()?.to_owned();
    if id.len() < 32 {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    let mut cwd = None;
    let mut first_prompt = None;
    let provider_title = latest_claude_ai_title(path);
    for line in capped_lines(path, CLAUDE_HEAD_CAP)
        .ok()?
        .map_while(Result::ok)
    {
        let Ok(object) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if first_prompt.is_none() {
            first_prompt = claude_user_text(&object).and_then(|text| useful_prompt(&text));
        }
        if let Some(value) = object.get("cwd").and_then(Value::as_str)
            && !value.is_empty()
        {
            cwd = Some(value.to_owned());
        }
        if cwd.is_some() && (first_prompt.is_some() || provider_title.is_some()) {
            break;
        }
    }
    let cwd = cwd?;
    let title = provider_title
        .or_else(|| first_prompt.map(title_from_prompt))
        .or_else(|| {
            Some(format!(
                "Claude · {} · {}",
                folder_name(&cwd),
                id.chars().take(8).collect::<String>()
            ))
        });
    Some(history_entry(
        id,
        AgentKind::CLAUDE_CODE,
        cwd,
        title,
        path,
        &metadata,
    ))
}

fn claude_user_text(object: &Value) -> Option<String> {
    if object.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = object.get("message")?.get("content")?;
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_owned());
    }
    content.as_array()?.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| item.get("text").and_then(Value::as_str))
            .flatten()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}

fn latest_claude_ai_title(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let end = file.seek(SeekFrom::End(0)).ok()?;
    let start = end.saturating_sub(CLAUDE_TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity(CLAUDE_TAIL_BYTES + (4 << 10));
    file.take((CLAUDE_TAIL_BYTES + (4 << 10)) as u64)
        .read_to_end(&mut bytes)
        .ok()?;

    let mut newest = None;
    let mut custom = None;
    for line in String::from_utf8_lossy(&bytes).lines() {
        if !line.contains("\"ai-title\"") && !line.contains("\"custom-title\"") {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("custom-title") {
            if let Some(title) = object
                .get("customTitle")
                .and_then(Value::as_str)
                .and_then(useful_prompt)
            {
                custom = Some(title);
            }
        } else if object.get("type").and_then(Value::as_str) == Some("ai-title")
            && let Some(title) = object
                .get("aiTitle")
                .and_then(Value::as_str)
                .and_then(useful_prompt)
        {
            newest = Some(title);
        }
    }
    custom.or(newest)
}

fn scan_codex(root: &Path, scanner: &mut HistoryScanner) -> Vec<HistoryEntry> {
    let mut result = Vec::new();
    let titles = provider_titles(root.parent().unwrap_or(root));
    for year in child_dirs(root) {
        for month in child_dirs(&year) {
            for day in child_dirs(&month) {
                let Ok(files) = fs::read_dir(day) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    let name = path.file_name().and_then(|name| name.to_str());
                    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
                        || !name.is_some_and(|name| name.starts_with("rollout-"))
                    {
                        continue;
                    }
                    // Names can change without the transcript changing. The
                    // index revision participates in cache invalidation below.
                    if let Some(entry) =
                        scanner.read(&path, Some(&titles), || codex_entry(&path, &titles))
                    {
                        result.push(entry);
                    }
                }
            }
        }
    }
    result
}

fn codex_entry(path: &Path, titles: &HashMap<String, String>) -> Option<HistoryEntry> {
    let first = read_first_line(path, CODEX_FIRST_LINE_CAP).ok()??;
    let object: Value = serde_json::from_str(&first).ok()?;
    if object.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = object.get("payload")?;
    // Internal worker threads are not conversations the user opened.
    if payload.get("source").is_some_and(|source| {
        source.get("subagent").is_some() || source.as_str() == Some("subagent")
    }) {
        return None;
    }
    let id = payload.get("id")?.as_str()?.to_owned();
    let cwd = payload.get("cwd")?.as_str()?.to_owned();
    if cwd.is_empty() {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    let title = titles
        .get(&id)
        .cloned()
        .or_else(|| first_codex_user_prompt(path).map(title_from_prompt))
        .or_else(|| {
            Some(format!(
                "Codex · {} · {}",
                folder_name(&cwd),
                id.chars().take(8).collect::<String>()
            ))
        });
    Some(history_entry(
        id,
        AgentKind::CODEX,
        cwd,
        title,
        path,
        &metadata,
    ))
}

fn first_codex_user_prompt(path: &Path) -> Option<String> {
    for line in capped_lines(path, CODEX_FIRST_PROMPT_CAP)
        .ok()?
        .map_while(Result::ok)
    {
        if !line.contains("\"user_message\"")
            && !line.contains("\"role\":\"user\"")
            && !line.contains("\"role\": \"user\"")
        {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(payload) = object.get("payload") else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("event_msg")
            && payload.get("type").and_then(Value::as_str) == Some("user_message")
            && let Some(message) = payload
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
            && let Some(prompt) = useful_prompt(message)
        {
            return Some(prompt);
        }
        if object.get("type").and_then(Value::as_str) == Some("response_item")
            && payload.get("role").and_then(Value::as_str) == Some("user")
            && let Some(content) = payload.get("content").and_then(Value::as_array)
        {
            for item in content {
                if let Some(prompt) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(useful_prompt)
                {
                    return Some(prompt);
                }
            }
        }
    }
    None
}

fn history_entry(
    id: String,
    kind: AgentKind,
    cwd: String,
    title: Option<String>,
    path: &Path,
    metadata: &Metadata,
) -> HistoryEntry {
    HistoryEntry {
        id,
        kind,
        cwd_exists: Path::new(&cwd).is_dir(),
        cwd,
        title,
        transcript_path: path.to_string_lossy().into_owned(),
        last_active_at: system_time(metadata.modified().ok()),
        created_at: metadata.created().ok().map(|time| system_time(Some(time))),
    }
}

fn system_time(time: Option<SystemTime>) -> DateMillis {
    let millis = time
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
    DateMillis(millis)
}

fn capped_lines(path: &Path, cap: usize) -> io::Result<impl Iterator<Item = io::Result<String>>> {
    let file = File::open(path)?;
    Ok(BufReader::new(file.take(cap as u64)).lines())
}

fn read_first_line(path: &Path, cap: usize) -> io::Result<Option<String>> {
    capped_lines(path, cap)?.next().transpose()
}

fn child_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(children) = fs::read_dir(root) else {
        return Vec::new();
    };
    children
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn title_from_prompt(prompt: String) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = collapsed.chars();
    if characters.clone().count() <= 160 {
        return collapsed;
    }
    characters.by_ref().take(159).collect::<String>() + "…"
}

/// Provider context and replay wrappers are not a user's conversation title.
fn useful_prompt(text: &str) -> Option<String> {
    let mut text = text.trim();
    for prefix in [
        "The following is the Codex agent history",
        "This historical conversation has just been resumed",
        "# AGENTS.md instructions",
        "<environment_context>",
        "<INSTRUCTIONS>",
        "<permissions instructions>",
        "<turn_aborted>",
        "You are an AI assistant",
    ] {
        if text.starts_with(prefix) {
            return None;
        }
    }
    // Image attachment metadata can precede the user's actual request.
    while text.starts_with("<image ") {
        let end = text.find("</image>")? + "</image>".len();
        text = text[end..].trim();
    }
    (!text.is_empty()).then(|| title_from_prompt(text.to_owned()))
}

/// Read provider names in batches, once per scan, rather than once per rollout.
/// SQLite is already used by the workspace; read-only access recovers names
/// without depending on a running provider process or scanning transcript bodies.
fn provider_titles(home: &Path) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let mut explicit = HashMap::new();
    let mut databases = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = name
                .to_str()?
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .collect::<Vec<_>>();
    databases.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    for (_, path) in databases.into_iter().take(16) {
        let Ok(connection) = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let _ = connection.busy_timeout(std::time::Duration::from_millis(20));
        let columns = connection
            .prepare("PRAGMA table_info(threads)")
            .ok()
            .and_then(|mut statement| {
                Some(
                    statement
                        .query_map([], |row| row.get::<_, String>(1))
                        .ok()?
                        .filter_map(Result::ok)
                        .collect::<HashSet<_>>(),
                )
            })
            .unwrap_or_default();
        if !columns.contains("id") {
            continue;
        }
        let field = |name: &str| if columns.contains(name) { name } else { "NULL" }.to_owned();
        let query = format!(
            "SELECT id, {}, {}, {} FROM threads LIMIT 100000",
            field("name"),
            field("title"),
            field("first_user_message")
        );
        let Ok(mut statement) = connection.prepare(&query) else {
            continue;
        };
        let Ok(rows) = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        }) else {
            continue;
        };
        for (id, name, title, prompt) in rows.flatten() {
            if let Some(name) = name.as_deref().and_then(useful_prompt) {
                explicit.entry(id.clone()).or_insert(name);
            }
            if let Some(title) = title
                .as_deref()
                .and_then(useful_prompt)
                .or_else(|| prompt.as_deref().and_then(useful_prompt))
            {
                titles.entry(id).or_insert(title);
            }
        }
    }
    let index_path = home.join("session_index.jsonl");
    if fs::symlink_metadata(&index_path).is_ok_and(|metadata| metadata.is_file())
        && let Ok(mut file) = File::open(index_path)
    {
        let end = file.seek(SeekFrom::End(0)).unwrap_or(0);
        let start = end.saturating_sub(16 << 20);
        if file.seek(SeekFrom::Start(start)).is_ok() {
            let mut lines = BufReader::new(file.take(16 << 20)).lines();
            if start > 0 {
                lines.next();
            }
            for line in lines.map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let (Some(id), Some(title)) = (
                    value.get("id").and_then(Value::as_str),
                    value
                        .get("thread_name")
                        .and_then(Value::as_str)
                        .and_then(useful_prompt),
                ) {
                    titles.insert(id.to_owned(), title);
                }
            }
        }
    }
    titles.extend(explicit);
    titles
}

fn folder_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use diri_client::DaemonClient;
    use diri_proto::{ControlMessage, HelloResult, Method, RUST_ENGINE_KIND, WIRE_VERSION};
    use tempfile::TempDir;

    use super::*;

    fn fixture_roots(temp: &TempDir) -> HistoryRoots {
        HistoryRoots {
            claude: temp.path().join("claude"),
            codex: temp.path().join("codex"),
        }
    }

    #[test]
    fn scans_claude_and_prefers_latest_ai_title() {
        let temp = TempDir::new().unwrap();
        let roots = fixture_roots(&temp);
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let project = roots.claude.join("encoded-project");
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join("12345678-1234-1234-1234-123456789abc.jsonl");
        fs::write(
            &transcript,
            format!(
                "{{\"type\":\"user\",\"cwd\":{},\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"first prompt\"}}]}}}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Older title\"}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Newest title\"}}\n",
                serde_json::to_string(&cwd.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();

        let entries = scan(&roots, &HashSet::new());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, AgentKind::CLAUDE_CODE);
        assert_eq!(entries[0].title.as_deref(), Some("Newest title"));
        assert!(entries[0].cwd_exists);
    }

    #[test]
    fn scans_codex_and_uses_first_user_prompt() {
        let temp = TempDir::new().unwrap();
        let roots = fixture_roots(&temp);
        let cwd = temp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let day = roots.codex.join("2026/07/22");
        fs::create_dir_all(&day).unwrap();
        let transcript = day.join("rollout-2026-07-22-thread-id.jsonl");
        fs::write(
            transcript,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread-id\",\"cwd\":{}}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"  Build   the thing\\ncarefully  \"}}}}\n",
                serde_json::to_string(&cwd.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();

        let entries = scan(&roots, &HashSet::new());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, AgentKind::CODEX);
        assert_eq!(
            entries[0].title.as_deref(),
            Some("Build the thing carefully")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn history_resume_uses_the_first_class_resume_rpc() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let ControlMessage::Request { id, method, params } =
                    serde_json::from_str(&line).unwrap()
                else {
                    continue;
                };
                let terminal = method != Method::HELLO;
                let response = if terminal {
                    request_tx.send((method, params)).unwrap();
                    ControlMessage::Response {
                        id,
                        result: Err(diri_proto::ControlError::bad_request("test complete")),
                    }
                } else {
                    ControlMessage::Response {
                        id,
                        result: Ok(serde_json::to_value(HelloResult {
                            proto: WIRE_VERSION,
                            build: "test-engine".to_owned(),
                            pid: std::process::id() as i32,
                            engine_kind: Some(RUST_ENGINE_KIND.to_owned()),
                            executable_hash: None,
                        })
                        .unwrap()),
                    }
                };
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                writer.write_all(&bytes).unwrap();
                writer.flush().unwrap();
                if terminal {
                    break;
                }
            }
        });

        let client = DaemonClient::with_socket_path(socket);
        client.connect();
        client
            .wait_until_connected(Duration::from_secs(2))
            .await
            .unwrap();
        let entry = HistoryEntry {
            id: "conversation-id".to_owned(),
            kind: AgentKind::CLAUDE_CODE,
            cwd: temp.path().to_string_lossy().into_owned(),
            title: Some("A title".to_owned()),
            transcript_path: temp
                .path()
                .join("conversation-id.jsonl")
                .to_string_lossy()
                .into_owned(),
            last_active_at: DateMillis(10.0),
            created_at: None,
            cwd_exists: true,
        };

        assert!(resume(&client, &entry).await.is_err());
        let (method, params) = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, Method::SESSION_RESUME_FROM_HISTORY);
        let params = params.unwrap();
        assert_eq!(params["entry"]["id"], entry.id);
        assert_eq!(
            params["entry"]["kind"],
            serde_json::json!({ "claudeCode": {} })
        );
        assert_eq!(params["entry"]["transcriptPath"], entry.transcript_path);
        assert!(
            params["initialPrompt"].is_null(),
            "opening history must not send a message"
        );

        client.shutdown().await;
        server.join().unwrap();
    }

    #[test]
    fn fuzzy_filter_is_case_insensitive_subsequence() {
        let entry = HistoryEntry {
            id: "id".to_owned(),
            kind: AgentKind::CLAUDE_CODE,
            cwd: "/tmp/Dirijor".to_owned(),
            title: Some("History palette".to_owned()),
            transcript_path: String::new(),
            last_active_at: DateMillis(0.0),
            created_at: None,
            cwd_exists: false,
        };
        let mut search = HistorySearch::default();
        search.rebuild(&[entry]);
        assert_eq!(search.rank("hpal"), [0]);
        assert_eq!(search.rank("diri"), [0]);
        assert_eq!(search.rank("claude tmp history"), [0]);
        assert!(search.rank("zebra").is_empty());
    }

    #[test]
    fn history_skips_injected_context_and_reads_response_item_prompts() {
        let temp = TempDir::new().unwrap();
        let roots = fixture_roots(&temp);
        let day = roots.codex.join("2026/09/05");
        fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-thread.jsonl");
        let records = [
            serde_json::json!({"type":"session_meta","payload":{"id":"thread","cwd":"/tmp"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"The following is the Codex agent history whose request activated this agent."}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp\n<INSTRUCTIONS>rules</INSTRUCTIONS>"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix conversation search performance"}]}}),
        ];
        fs::write(
            path,
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let entries = scan(&roots, &HashSet::new());
        assert_eq!(
            entries[0].title.as_deref(),
            Some("Fix conversation search performance")
        );
    }

    #[test]
    fn history_scan_large_transcripts_benchmark() {
        let temp = TempDir::new().unwrap();
        let roots = fixture_roots(&temp);
        let day = roots.codex.join("2026/09/05");
        fs::create_dir_all(&day).unwrap();
        for index in 0..64 {
            let mut file = File::create(day.join(format!("rollout-{index}.jsonl"))).unwrap();
            writeln!(file, "{}", serde_json::json!({"type":"session_meta","payload":{"id":format!("thread-{index}"),"cwd":"/tmp"}})).unwrap();
            writeln!(file, "{}", serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Fix search"}})).unwrap();
            file.set_len(8 << 20).unwrap();
        }
        let mut scanner = HistoryScanner::default();
        let started = std::time::Instant::now();
        assert_eq!(scanner.scan(&roots, &HashSet::new()).len(), 64);
        eprintln!("64 x 8 MiB history scan: {:?}", started.elapsed());
        let started = std::time::Instant::now();
        assert_eq!(scanner.scan(&roots, &HashSet::new()).len(), 64);
        eprintln!("64 x 8 MiB cached scan: {:?}", started.elapsed());
    }

    #[test]
    fn cached_history_refreshes_names_folders_and_deleted_transcripts() {
        let temp = TempDir::new().unwrap();
        let roots = fixture_roots(&temp);
        let day = roots.codex.join("2026/09/05");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&day).unwrap();
        fs::create_dir(&cwd).unwrap();
        let path = day.join("rollout-thread.jsonl");
        fs::write(&path, format!("{}\n{}\n", serde_json::json!({"type":"session_meta","payload":{"id":"thread","cwd":cwd}}), serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Fallback prompt"}}))).unwrap();
        let connection = rusqlite::Connection::open(temp.path().join("state_5.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE threads (id TEXT, name TEXT, title TEXT); INSERT INTO threads VALUES ('thread', NULL, 'Database title');").unwrap();
        let mut scanner = HistoryScanner::default();
        assert_eq!(
            scanner.scan(&roots, &HashSet::new())[0].title.as_deref(),
            Some("Database title")
        );
        fs::write(
            temp.path().join("session_index.jsonl"),
            "{\"id\":\"thread\",\"thread_name\":\"Indexed title\"}\n",
        )
        .unwrap();
        assert_eq!(
            scanner.scan(&roots, &HashSet::new())[0].title.as_deref(),
            Some("Indexed title")
        );
        connection
            .execute_batch("UPDATE threads SET name = 'My custom title';")
            .unwrap();
        assert_eq!(
            scanner.scan(&roots, &HashSet::new())[0].title.as_deref(),
            Some("My custom title")
        );
        assert!(
            scanner
                .scan(&roots, &HashSet::from(["thread".to_owned()]))
                .is_empty()
        );
        fs::remove_dir(cwd).unwrap();
        assert!(!scanner.scan(&roots, &HashSet::new())[0].cwd_exists);
        fs::remove_file(path).unwrap();
        assert!(scanner.scan(&roots, &HashSet::new()).is_empty());
        assert!(scanner.files.is_empty());
    }

    #[test]
    fn claude_cwd_before_user_prompt_and_custom_title() {
        let temp = TempDir::new().unwrap();
        let roots = fixture_roots(&temp);
        let project = roots.claude.join("project");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("12345678-1234-1234-1234-123456789abc.jsonl");
        fs::write(&path, "{\"type\":\"system\",\"cwd\":\"/tmp\"}\n{\"type\":\"user\",\"message\":{\"content\":\"Actual request\"}}\n").unwrap();
        let mut scanner = HistoryScanner::default();
        assert_eq!(
            scanner.scan(&roots, &HashSet::new())[0].title.as_deref(),
            Some("Actual request")
        );
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{{\"type\":\"custom-title\",\"customTitle\":\"My title\"}}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Generated later\"}}").unwrap();
        assert_eq!(
            scanner.scan(&roots, &HashSet::new())[0].title.as_deref(),
            Some("My title")
        );
    }

    #[test]
    #[ignore = "read-only benchmark of local history; prints only counts and timings"]
    fn history_real_store_benchmark() {
        let mut scanner = HistoryScanner::default();
        for label in ["cold", "warm"] {
            let start = std::time::Instant::now();
            let entries = scanner.scan(&HistoryRoots::current_user(), &HashSet::new());
            eprintln!(
                "{label}: {} conversations in {:?}; {} named",
                entries.len(),
                start.elapsed(),
                entries.iter().filter(|e| e.title.is_some()).count()
            );
            let mut search = HistorySearch::default();
            search.rebuild(&entries);
            let start = std::time::Instant::now();
            for query in [
                "d",
                "di",
                "diri",
                "codex search",
                "claude",
                "fix",
                "terminal",
                "!codex",
            ] {
                let _ = search.rank(query);
            }
            eprintln!("8 ranked searches: {:?}", start.elapsed());
        }
    }

    /// Explicit live acceptance check for T14. It is ignored by the normal
    /// gate because it creates a real Claude session in the shared daemon and
    /// intentionally leaves that resumed session available to the user.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "set DIRI_LIVE_HISTORY_TEST=1 to resume a real Claude conversation"]
    async fn live_historical_claude_resume_smoke() {
        assert_eq!(std::env::var("DIRI_LIVE_HISTORY_TEST").as_deref(), Ok("1"));
        let client = DaemonClient::new();
        client.connect();
        client
            .wait_until_connected(Duration::from_secs(8))
            .await
            .expect("live Dirijor daemon is not reachable");
        let current = client.sessions().await.expect("session.list failed");
        let tracked = current
            .sessions
            .into_iter()
            .filter_map(|session| session.agent_session_id)
            .collect();
        let requested_id = std::env::var("DIRI_HISTORY_ID").ok();
        let entry = scan(&HistoryRoots::current_user(), &tracked)
            .into_iter()
            .find(|entry| {
                entry.kind == AgentKind::CLAUDE_CODE
                    && entry.cwd_exists
                    && requested_id
                        .as_ref()
                        .is_none_or(|requested| &entry.id == requested)
            })
            .expect("no untracked historical Claude conversation with a live cwd");
        let session_id = if let Ok(existing) = std::env::var("DIRI_EXISTING_SESSION_ID") {
            diri_proto::SessionId::new(existing)
        } else {
            resume(&client, &entry)
                .await
                .expect("session.resume_from_history rejected the conversation")
        };

        let mut screen = String::new();
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(snapshot) = client.read_screen(&session_id).await {
                screen = snapshot.text;
            }
            if screen.contains("historical conversation has just been resumed")
                || screen.contains("recovered state")
                || screen.contains("Frolicking")
                || screen.contains("Working")
            {
                break;
            }
        }
        eprintln!(
            "resumed Claude history {} as daemon session {}\n{}",
            entry.id, session_id, screen
        );
        assert!(
            !screen.trim().is_empty(),
            "resumed session never painted a screen"
        );
        assert!(
            !screen.contains("No conversation found"),
            "Claude rejected the historical conversation id"
        );
        assert!(
            client
                .sessions()
                .await
                .expect("session.list after spawn failed")
                .sessions
                .iter()
                .any(|session| session.id == session_id),
            "spawned history session was not tracked by the daemon"
        );
    }
}
