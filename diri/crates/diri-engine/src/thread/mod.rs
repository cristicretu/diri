//! Structured thread observation for the Dual-Lens GUI.
//!
//! The Agent's PTY Holder stays the single source of truth for what runs and
//! what the user types. This module only *observes* the Agent's own JSONL
//! transcript, stateless per request: the caller holds the cursor, so an
//! Engine restart, an app restart or a dropped connection resumes exactly
//! where the client left off, and a stale cursor merely replays items the
//! client merges by key.

pub mod capsule;
pub mod normalize;
pub mod segments;
pub mod tail;
pub mod watch;

use std::path::{Path, PathBuf};

use diri_proto::thread::{
    ItemKey, THREAD_IMAGE_MAX_BYTES, THREAD_READ_DEFAULT_BYTES, THREAD_READ_MAX_BYTES,
    THREAD_WIRE_VERSION, ThreadBody, ThreadCursor, ThreadItem, ThreadReadParams, ThreadReadResult,
    ThreadSegment,
};
use diri_proto::{AgentKind, SessionRecord};

use normalize::Provider;

/// What a read needs besides the session record.
pub struct ReadContext<'a> {
    pub home: &'a Path,
    pub segments: Vec<ThreadSegment>,
}

/// The transcript a session's GUI Lens would read, if it has one.
pub struct Source {
    pub provider: Provider,
    pub path: PathBuf,
    pub store_root: PathBuf,
    pub agent_id: String,
}

/// Why a session cannot show a GUI Lens, or where its transcript lives.
pub fn source(home: &Path, record: &SessionRecord) -> Result<Option<Source>, &'static str> {
    if record.host.is_some() {
        return Err("Remote sessions show the terminal.");
    }
    if record.note_id.is_some() {
        return Err("Notes have no conversation.");
    }
    let provider = match record.effective_kind().id() {
        AgentKind::CLAUDE_CODE_ID => Provider::Claude,
        AgentKind::CODEX_ID => Provider::Codex,
        _ => return Err("This agent has no structured transcript; the terminal is the view."),
    };
    // A foreground agent typed into a shell runs with an id Diri never saw.
    if record.effective_kind() != &record.kind {
        return Err("This agent was started inside a shell; the terminal is the view.");
    }
    let (Some(path), Some(agent_id)) = (
        record.transcript_path.as_deref(),
        record.agent_session_id.as_deref(),
    ) else {
        return Ok(None);
    };
    let store_root = match provider {
        Provider::Claude => record.account_profile.as_ref().map_or_else(
            || home.join(".claude"),
            |profile| PathBuf::from(&profile.config_home),
        ),
        Provider::Codex => record.account_profile.as_ref().map_or_else(
            || home.join(".codex"),
            |profile| PathBuf::from(&profile.config_home),
        ),
    }
    .join(match provider {
        Provider::Claude => "projects",
        Provider::Codex => "sessions",
    });
    Ok(Some(Source {
        provider,
        path: PathBuf::from(path),
        store_root,
        agent_id: agent_id.to_owned(),
    }))
}

/// One `session.thread_read`. Never fails: an unreadable transcript is an
/// unstructured result with a reason, so the client simply stays in (or
/// falls back to) the TUI Lens.
pub fn read(
    record: &SessionRecord,
    params: &ThreadReadParams,
    context: ReadContext<'_>,
) -> (ThreadReadResult, Option<PathBuf>) {
    let unstructured = |reason: &str| ThreadReadResult {
        version: THREAD_WIRE_VERSION,
        structured: false,
        reason: Some(reason.to_owned()),
        next: params.from.clone(),
        ..ThreadReadResult::default()
    };
    let source = match source(context.home, record) {
        Err(reason) => return (unstructured(reason), None),
        Ok(None) => {
            // Structured Agent that has not written its first record yet.
            return (
                ThreadReadResult {
                    version: THREAD_WIRE_VERSION,
                    structured: true,
                    next: params.from.clone(),
                    segments: context.segments,
                    ..ThreadReadResult::default()
                },
                None,
            );
        }
        Ok(Some(source)) => source,
    };
    let kind = record.effective_kind();
    let opened = crate::transcript::open_validated_in(
        &source.store_root,
        &source.path,
        kind,
        &source.agent_id,
        &record.cwd,
    );
    let (mut file, metadata) = match opened {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                ThreadReadResult {
                    version: THREAD_WIRE_VERSION,
                    structured: true,
                    next: params.from.clone(),
                    segments: context.segments,
                    ..ThreadReadResult::default()
                },
                None,
            );
        }
        Err(_) => {
            return (unstructured("The transcript could not be read."), None);
        }
    };
    let generation = generation(&metadata, &source.path);
    let length = metadata.len();
    let mut offset = params.from.offset;
    let mut reset = false;
    let foreign =
        params.from.generation != generation && (offset > 0 || !params.from.generation.is_empty());
    if foreign || !tail::cursor_is_boundary(&mut file, offset, length).unwrap_or(false) {
        reset = true;
        offset = 0;
    }
    let max_bytes = params
        .max_bytes
        .unwrap_or(THREAD_READ_DEFAULT_BYTES)
        .clamp(4096, THREAD_READ_MAX_BYTES);
    let window = match tail::read_window(&mut file, offset, length, u64::from(max_bytes)) {
        Ok(window) => window,
        Err(_) => {
            return (unstructured("The transcript could not be read."), None);
        }
    };
    let mut budget = THREAD_IMAGE_MAX_BYTES;
    let mut items = Vec::new();
    if let Some(skipped) = window.skipped {
        items.push(ThreadItem {
            key: ItemKey {
                offset: skipped,
                part: 0,
            },
            at_ms: None,
            body: ThreadBody::Notice {
                text: "A record too large to show here is in the terminal.".into(),
            },
        });
    }
    for line in &window.lines {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line.bytes) else {
            continue;
        };
        items.extend(normalize::record_items(
            source.provider,
            line.offset,
            &record,
            &mut budget,
        ));
    }
    let watched = diri_platform::canonicalize(&source.path).ok();
    (
        ThreadReadResult {
            version: THREAD_WIRE_VERSION,
            structured: true,
            reason: None,
            reset,
            more: window.more,
            next: ThreadCursor {
                generation,
                offset: window.next,
            },
            items,
            segments: context.segments,
        },
        watched,
    )
}

/// File identity: a copied or rotated transcript is another generation.
fn generation(metadata: &std::fs::Metadata, path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let _ = path;
        format!("{:x}-{:x}", metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        format!("{:x}", fxhash(path.to_string_lossy().as_bytes()))
    }
}

#[cfg(not(unix))]
fn fxhash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const AGENT: &str = "11111111-1111-4111-8111-111111111111";

    fn claude_record(home: &Path) -> (SessionRecord, PathBuf) {
        let path = home
            .join(".claude/projects/-tmp-project")
            .join(format!("{AGENT}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "id": "s-1", "kind": "claude-code", "cwd": "/tmp/project", "projectID": "p",
            "title": "t", "titleSource": serde_json::to_value(diri_proto::TitleSource::UserRename).unwrap(), "status": serde_json::to_value(diri_proto::SessionStatus::Idle).unwrap(),
            "resumability": serde_json::to_value(diri_proto::Resumability::Resumable).unwrap(), "createdAt": 0.0, "updatedAt": 0.0,
            "pinned": false,
            "agentSessionID": AGENT,
            "transcriptPath": path.to_string_lossy(),
        }))
        .unwrap();
        (record, path)
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn read_from(home: &Path, record: &SessionRecord, from: ThreadCursor) -> ThreadReadResult {
        read(
            record,
            &ThreadReadParams {
                session_id: record.id.clone(),
                from,
                max_bytes: None,
                watch: false,
            },
            ReadContext {
                home,
                segments: Vec::new(),
            },
        )
        .0
    }

    const USER: &str = r#"{"type":"user","message":{"content":"Fix the parser"}}"#;
    const ASSISTANT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"text","text":"On it."}]}}"#;

    /// Lean `replay_idempotent`: a stale or zero cursor replays the same keys.
    #[test]
    fn stale_cursor_replay_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let (record, path) = claude_record(home.path());
        append(&path, &format!("{USER}\n{ASSISTANT}\n"));
        let first = read_from(home.path(), &record, ThreadCursor::default());
        assert!(first.structured && !first.reset);
        let keys: Vec<_> = first.items.iter().map(|item| item.key).collect();
        append(&path, &format!("{USER}\n"));
        let resumed = read_from(home.path(), &record, first.next.clone());
        assert_eq!(resumed.items.len(), 1);
        assert!(resumed.items[0].key.offset >= first.next.offset);
        let replay = read_from(home.path(), &record, ThreadCursor::default());
        let replay_keys: Vec<_> = replay.items.iter().map(|item| item.key).collect();
        assert_eq!(&replay_keys[..keys.len()], &keys[..]);
        assert_eq!(replay.next, resumed.next);
    }

    #[test]
    fn a_cursor_from_another_file_or_mid_record_resets() {
        let home = tempfile::tempdir().unwrap();
        let (record, path) = claude_record(home.path());
        append(&path, &format!("{USER}\n{ASSISTANT}\n"));
        let first = read_from(home.path(), &record, ThreadCursor::default());
        let foreign = read_from(
            home.path(),
            &record,
            ThreadCursor {
                generation: "other".into(),
                offset: first.next.offset,
            },
        );
        assert!(foreign.reset);
        assert_eq!(foreign.items.len(), first.items.len());
        let torn = read_from(
            home.path(),
            &record,
            ThreadCursor {
                generation: first.next.generation.clone(),
                offset: 5,
            },
        );
        assert!(torn.reset);
    }

    #[test]
    fn shells_and_remote_sessions_stay_in_the_terminal() {
        let home = tempfile::tempdir().unwrap();
        let (mut record, _) = claude_record(home.path());
        record.host = Some("forge".into());
        assert!(!read_from(home.path(), &record, ThreadCursor::default()).structured);
        record.host = None;
        record.kind = AgentKind::SHELL;
        assert!(!read_from(home.path(), &record, ThreadCursor::default()).structured);
    }

    #[test]
    fn a_transcript_not_written_yet_is_structured_and_empty() {
        let home = tempfile::tempdir().unwrap();
        let (record, path) = claude_record(home.path());
        std::fs::remove_file(path).unwrap();
        let result = read_from(home.path(), &record, ThreadCursor::default());
        assert!(result.structured);
        assert!(result.items.is_empty());
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    fn percentile(samples: &mut [f64], p: f64) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[((samples.len() as f64 - 1.0) * p).round() as usize]
    }

    /// `cargo test -p diri-engine --release --lib thread::bench -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; prints throughput and append latency"]
    fn bench_thread_tail() {
        let home = tempfile::tempdir().unwrap();
        let (record, path) = tests_support::claude_record(home.path());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let assistant = r#"{"type":"assistant","timestamp":"2026-11-23T10:00:00.000Z","message":{"model":"claude-opus-5-5","content":[{"type":"text","text":"Here is a paragraph of assistant prose that is long enough to look like a real answer, with `code` and **bold** spans and a path like src/main.rs:42."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test -p diri-engine"}}],"usage":{"input_tokens":2,"output_tokens":300,"cache_read_input_tokens":40000,"cache_creation_input_tokens":300}}}"#;
        let result = r#"{"type":"user","timestamp":"2026-11-23T10:00:01.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"test result: ok. 18 passed; 0 failed"}]}}"#;
        for _ in 0..25_000 {
            writeln!(file, "{assistant}\n{result}").unwrap();
        }
        file.sync_all().unwrap();
        let bytes = std::fs::metadata(&path).unwrap().len();
        let read = |from: ThreadCursor| {
            super::read(
                &record,
                &ThreadReadParams {
                    session_id: record.id.clone(),
                    from,
                    max_bytes: Some(diri_proto::thread::THREAD_READ_MAX_BYTES),
                    watch: false,
                },
                ReadContext {
                    home: home.path(),
                    segments: Vec::new(),
                },
            )
            .0
        };
        let started = Instant::now();
        let mut cursor = ThreadCursor::default();
        let mut items = 0;
        loop {
            let page = read(cursor.clone());
            items += page.items.len();
            cursor = page.next;
            if !page.more {
                break;
            }
        }
        let cold = started.elapsed();
        let mut appends = Vec::new();
        for _ in 0..200 {
            writeln!(file, "{assistant}").unwrap();
            let started = Instant::now();
            let page = read(cursor.clone());
            appends.push(started.elapsed().as_secs_f64() * 1e6);
            assert_eq!(page.items.len(), 3);
            cursor = page.next;
        }
        println!(
            "thread tail: cold {:.1} MiB, {items} items in {:.1} ms ({:.0} MiB/s); one-record append read p50 {:.0} µs p95 {:.0} µs",
            bytes as f64 / 1048576.0,
            cold.as_secs_f64() * 1e3,
            bytes as f64 / 1048576.0 / cold.as_secs_f64(),
            percentile(&mut appends, 0.5),
            percentile(&mut appends, 0.95),
        );
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;
    pub fn claude_record(home: &Path) -> (SessionRecord, PathBuf) {
        let agent = "11111111-1111-4111-8111-111111111111";
        let path = home
            .join(".claude/projects/-tmp-project")
            .join(format!("{agent}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let record: SessionRecord = serde_json::from_value(serde_json::json!({
            "id": "s-1", "kind": "claude-code", "cwd": "/tmp/project", "projectID": "p",
            "title": "t", "titleSource": serde_json::to_value(diri_proto::TitleSource::UserRename).unwrap(),
            "status": serde_json::to_value(diri_proto::SessionStatus::Idle).unwrap(),
            "resumability": serde_json::to_value(diri_proto::Resumability::Resumable).unwrap(),
            "createdAt": 0.0, "updatedAt": 0.0, "pinned": false,
            "agentSessionID": agent, "transcriptPath": path.to_string_lossy(),
        }))
        .unwrap();
        (record, path)
    }
}
