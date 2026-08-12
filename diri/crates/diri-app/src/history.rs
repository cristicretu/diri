//! Read-only discovery of Claude and Codex transcripts.
//!
//! This is a Rust port of `DirijorDaemonKit/HistoryScanner.swift`. Keeping the
//! scan client-side lets diri show durable history without requiring a newer
//! daemon. Transcript files are never modified.

use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use diri_client::{ClientError, DaemonClient};
use diri_proto::{AgentKind, DateMillis, HistoryEntry};
use serde_json::Value;

const MAX_ENTRIES: usize = 500;
const CLAUDE_HEAD_CAP: usize = 8 << 20;
const CLAUDE_TAIL_BYTES: usize = 16 << 10;
const CODEX_FIRST_LINE_CAP: usize = 512 << 10;
const CODEX_FIRST_PROMPT_CAP: usize = 8 << 20;
const RESUME_PROMPT: &str = "This historical conversation has just been resumed. Briefly state the recovered state, inspect the current workspace, and continue the unfinished task without replaying completed work.";

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
pub fn scan(roots: &HistoryRoots, tracked: &HashSet<String>) -> Vec<HistoryEntry> {
    let mut entries = scan_claude(&roots.claude);
    entries.extend(scan_codex(&roots.codex));

    let mut seen = tracked.clone();
    entries.retain(|entry| seen.insert(entry.id.clone()));
    entries.sort_by(|left, right| {
        right
            .last_active_at
            .partial_cmp(&left.last_active_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.id.cmp(&right.id))
    });
    entries.truncate(MAX_ENTRIES);
    entries
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
        .resume_from_history_with_prompt(entry.clone(), Some(RESUME_PROMPT.to_owned()))
        .await
        .map(|record| record.id)
}

pub fn matches_query(entry: &HistoryEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let folder = Path::new(&entry.cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let candidate =
        format!("{} {folder}", entry.title.as_deref().unwrap_or_default()).to_lowercase();
    subsequence_match(&query, &candidate)
}

fn subsequence_match(query: &str, candidate: &str) -> bool {
    let mut query = query.chars();
    let mut wanted = query.next();
    for character in candidate.chars() {
        if wanted == Some(character) {
            wanted = query.next();
            if wanted.is_none() {
                return true;
            }
        }
    }
    wanted.is_none()
}

fn scan_claude(root: &Path) -> Vec<HistoryEntry> {
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
            if let Some(entry) = claude_entry(&path) {
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
    for line in read_capped_lines(path, CLAUDE_HEAD_CAP).ok()? {
        let Ok(object) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if first_prompt.is_none() {
            first_prompt = claude_user_text(&object);
        }
        if let Some(value) = object.get("cwd").and_then(Value::as_str)
            && !value.is_empty()
        {
            cwd = Some(value.to_owned());
            break;
        }
    }
    let cwd = cwd?;
    let title = latest_claude_ai_title(path).or_else(|| first_prompt.map(title_from_prompt));
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
    for line in String::from_utf8_lossy(&bytes).lines() {
        if !line.contains("\"ai-title\"") {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("ai-title")
            && let Some(title) = object
                .get("aiTitle")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
        {
            newest = Some(title.to_owned());
        }
    }
    newest
}

fn scan_codex(root: &Path) -> Vec<HistoryEntry> {
    let mut result = Vec::new();
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
                    if let Some(entry) = codex_entry(&path) {
                        result.push(entry);
                    }
                }
            }
        }
    }
    result
}

fn codex_entry(path: &Path) -> Option<HistoryEntry> {
    let first = read_first_line(path, CODEX_FIRST_LINE_CAP).ok()??;
    let object: Value = serde_json::from_str(&first).ok()?;
    if object.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = object.get("payload")?;
    let id = payload.get("id")?.as_str()?.to_owned();
    let cwd = payload.get("cwd")?.as_str()?.to_owned();
    if cwd.is_empty() {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    let title = first_codex_user_prompt(path)
        .map(title_from_prompt)
        .or_else(|| Some(format!("Codex — {}", folder_name(&cwd))));
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
    for line in read_capped_lines(path, CODEX_FIRST_PROMPT_CAP).ok()? {
        if !line.contains("\"user_message\"") {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = object.get("payload")?;
        if object.get("type").and_then(Value::as_str) == Some("event_msg")
            && payload.get("type").and_then(Value::as_str) == Some("user_message")
            && let Some(message) = payload
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        {
            return Some(message.to_owned());
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

fn read_capped_lines(path: &Path, cap: usize) -> io::Result<Vec<String>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file.take(cap as u64));
    let mut result = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        result.push(std::mem::take(&mut line));
    }
    Ok(result)
}

fn read_first_line(path: &Path, cap: usize) -> io::Result<Option<String>> {
    Ok(read_capped_lines(path, cap)?.into_iter().next())
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
    if characters.clone().count() <= 60 {
        return collapsed;
    }
    characters.by_ref().take(59).collect::<String>() + "…"
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
        assert!(params["initialPrompt"].as_str().is_some_and(|prompt| {
            prompt.contains("historical conversation has just been resumed")
        }));

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
        assert!(matches_query(&entry, "hpal"));
        assert!(matches_query(&entry, "diri"));
        assert!(!matches_query(&entry, "zebra"));
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
