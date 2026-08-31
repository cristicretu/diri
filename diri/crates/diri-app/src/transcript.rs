//! Read-only, bounded projection of live Claude and Codex transcripts.
//!
//! Transcript files are provider-owned JSONL. This module extracts only
//! human/agent text turns for the inspector; malformed records and tool data
//! are ignored, and no transcript bytes are ever modified.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

use diri_proto::AgentKind;
use serde_json::Value;

const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TURNS: usize = 100;
const MAX_TURN_BYTES: usize = 256 * 1024;
const CODEX_FIRST_LINE_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub role: &'static str,
    pub text: String,
    /// One-based JSONL record number when the whole file was read; otherwise
    /// one-based within the bounded tail, which is still a useful approximate
    /// position for provenance.
    pub line: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptDocument {
    pub turns: Vec<TranscriptTurn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptVersion {
    length: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptSnapshot {
    pub document: TranscriptDocument,
    pub version: TranscriptVersion,
}

/// Opens and projects one provider transcript without ever following its final
/// path component or blocking on a special file. `home` is captured by the app
/// at startup; callers must also supply the identity associated with the live
/// session so stale persisted paths are not trusted merely because they parse.
pub fn load(
    home: &Path,
    path: &Path,
    kind: &AgentKind,
    agent_id: &str,
    cwd: &str,
    previous: Option<TranscriptVersion>,
) -> io::Result<Option<TranscriptSnapshot>> {
    if !matches!(kind.id(), AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID) {
        return Ok(Some(TranscriptSnapshot {
            document: TranscriptDocument::default(),
            version: TranscriptVersion {
                length: 0,
                modified: None,
            },
        }));
    }
    validate_provider_path(home, path, kind, agent_id)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transcript is not a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "transcript is not owned by this user",
        ));
    }
    validate_opened_identity(&mut file, kind, agent_id, cwd)?;

    let version = TranscriptVersion {
        length: metadata.len(),
        modified: metadata.modified().ok(),
    };
    if previous == Some(version) {
        return Ok(None);
    }

    let length = metadata.len();
    let clipped = length > MAX_READ_BYTES;
    if clipped {
        file.seek(SeekFrom::Start(length - MAX_READ_BYTES))?;
    } else {
        file.seek(SeekFrom::Start(0))?;
    }
    let mut reader = BufReader::new(file.take(MAX_READ_BYTES));
    if clipped {
        // The bounded tail normally starts inside a JSON record.
        let mut partial = String::new();
        reader.read_line(&mut partial)?;
    }

    let mut turns = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let parsed = match kind.id() {
            AgentKind::CLAUDE_CODE_ID => claude_turn(&record),
            AgentKind::CODEX_ID => codex_turn(&record),
            _ => None,
        };
        let Some((role, text)) = parsed else { continue };
        let text = bounded_text(&text);
        if text.trim().is_empty() {
            continue;
        }
        if turns
            .last()
            .is_some_and(|turn: &TranscriptTurn| turn.role == role && turn.text == text)
        {
            continue;
        }
        turns.push(TranscriptTurn {
            role,
            text,
            line: index + 1,
        });
        if turns.len() > MAX_TURNS {
            turns.remove(0);
        }
    }
    Ok(Some(TranscriptSnapshot {
        document: TranscriptDocument { turns },
        version,
    }))
}

fn validate_provider_path(
    home: &Path,
    path: &Path,
    kind: &AgentKind,
    agent_id: &str,
) -> io::Result<()> {
    if agent_id.is_empty()
        || agent_id.len() > 128
        || !agent_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid provider session identity",
        ));
    }
    let root = match kind.id() {
        AgentKind::CLAUDE_CODE_ID => home.join(".claude/projects"),
        AgentKind::CODEX_ID => home.join(".codex/sessions"),
        _ => return Ok(()),
    };
    let canonical_root = root.canonicalize()?;
    let canonical_path = path.canonicalize()?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "transcript is outside the provider store",
        ));
    }
    let expected_claude_name = format!("{agent_id}.jsonl");
    if kind.id() == AgentKind::CLAUDE_CODE_ID
        && path.file_name().and_then(|name| name.to_str()) != Some(expected_claude_name.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude transcript identity does not match the session",
        ));
    }
    Ok(())
}

fn validate_opened_identity(
    file: &mut File,
    kind: &AgentKind,
    agent_id: &str,
    cwd: &str,
) -> io::Result<()> {
    if kind.id() != AgentKind::CODEX_ID {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut first_line = String::new();
    let bytes = BufReader::new(file.by_ref().take(CODEX_FIRST_LINE_BYTES + 1))
        .read_line(&mut first_line)?;
    if bytes == 0 || bytes as u64 > CODEX_FIRST_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex transcript metadata is missing or oversized",
        ));
    }
    let record: Value = serde_json::from_str(&first_line).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex transcript metadata is malformed",
        )
    })?;
    let payload = record.get("payload");
    let matches = record.get("type").and_then(Value::as_str) == Some("session_meta")
        && payload
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            == Some(agent_id)
        && payload
            .and_then(|value| value.get("cwd"))
            .and_then(Value::as_str)
            == Some(cwd);
    if !matches {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex transcript identity does not match the session",
        ));
    }
    Ok(())
}

fn claude_turn(record: &Value) -> Option<(&'static str, String)> {
    let role = match record.get("type").and_then(Value::as_str)? {
        "user" => "You",
        "assistant" => "Claude",
        _ => return None,
    };
    let content = record.get("message")?.get("content")?;
    extract_content(content).map(|text| (role, text))
}

fn codex_turn(record: &Value) -> Option<(&'static str, String)> {
    let record_type = record.get("type").and_then(Value::as_str)?;
    let payload = record.get("payload")?;
    match record_type {
        "event_msg" => {
            let (role, field) = match payload.get("type").and_then(Value::as_str)? {
                "user_message" => ("You", "message"),
                "agent_message" => ("Codex", "message"),
                _ => return None,
            };
            payload
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .map(|text| (role, text))
        }
        "response_item" if payload.get("type").and_then(Value::as_str) == Some("message") => {
            let role = match payload.get("role").and_then(Value::as_str)? {
                "user" => "You",
                "assistant" => "Codex",
                _ => return None,
            };
            extract_content(payload.get("content")?).map(|text| (role, text))
        }
        _ => None,
    }
}

fn extract_content(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let pieces = content
        .as_array()?
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("text" | "input_text" | "output_text")
            )
        })
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>();
    (!pieces.is_empty()).then(|| pieces.join("\n"))
}

fn bounded_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let sanitized: String = normalized
        .chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            character if character.is_control() => '\u{fffd}',
            character => character,
        })
        .collect();
    if sanitized.len() <= MAX_TURN_BYTES {
        return sanitized;
    }
    let notice = "\n[… turn clipped by diri …]";
    let mut end = MAX_TURN_BYTES - notice.len();
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{notice}", &sanitized[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn claude_transcript(home: &Path, agent_id: &str) -> (std::fs::File, std::path::PathBuf) {
        let path = home
            .join(".claude/projects/-tmp-project")
            .join(format!("{agent_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        (File::create(&path).unwrap(), path)
    }

    fn codex_transcript(home: &Path, agent_id: &str, cwd: &str) -> (File, std::path::PathBuf) {
        let path = home
            .join(".codex/sessions/2026/08/13")
            .join(format!("rollout-2026-08-13T12-00-00-{agent_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"{agent_id}","cwd":"{cwd}"}}}}"#
        )
        .unwrap();
        (file, path)
    }

    #[test]
    fn loads_real_claude_turn_shapes_and_ignores_tool_records() {
        let home = tempfile::tempdir().unwrap();
        let agent_id = "11111111-1111-4111-8111-111111111111";
        let (mut file, path) = claude_transcript(home.path(), agent_id);
        writeln!(
            file,
            r#"{{"type":"user","message":{{"content":"Fix it"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"tool_result","content":"secret"}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Done"}},{{"type":"tool_use","name":"edit"}}]}}}}"#).unwrap();
        let document = load(
            home.path(),
            &path,
            &AgentKind::CLAUDE_CODE,
            agent_id,
            "/tmp/project",
            None,
        )
        .unwrap()
        .unwrap()
        .document;
        assert_eq!(
            document.turns,
            vec![
                TranscriptTurn {
                    role: "You",
                    text: "Fix it".to_owned(),
                    line: 1,
                },
                TranscriptTurn {
                    role: "Claude",
                    text: "Done".to_owned(),
                    line: 3,
                },
            ]
        );
    }

    #[test]
    fn loads_codex_turns_and_collapses_adjacent_duplicate_projections() {
        let home = tempfile::tempdir().unwrap();
        let agent_id = "22222222-2222-4222-8222-222222222222";
        let cwd = "/tmp/project";
        let (mut file, path) = codex_transcript(home.path(), agent_id, cwd);
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"Review this"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Review this"}}]}}}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"Looks good"}}}}"#
        )
        .unwrap();
        let document = load(home.path(), &path, &AgentKind::CODEX, agent_id, cwd, None)
            .unwrap()
            .unwrap()
            .document;
        assert_eq!(document.turns.len(), 2);
        assert_eq!(document.turns[0].role, "You");
        assert_eq!(document.turns[0].text, "Review this");
        assert_eq!(document.turns[1].role, "Codex");
        assert_eq!(document.turns[1].text, "Looks good");
    }

    #[test]
    fn unsupported_terminal_transcripts_are_not_projected_as_agent_turns() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            load(
                Path::new("/unused"),
                file.path(),
                &AgentKind::SHELL,
                "",
                "",
                None,
            )
            .unwrap()
            .unwrap()
            .document
            .turns
            .is_empty()
        );
    }

    #[test]
    fn transcript_text_normalizes_crlf_before_control_sanitizing() {
        assert_eq!(
            bounded_text("first\r\n\tsecond\rthird"),
            "first\n\tsecond\nthird"
        );
    }

    #[test]
    fn unchanged_version_skips_reparsing_but_an_append_is_observed() {
        let home = tempfile::tempdir().unwrap();
        let agent_id = "33333333-3333-4333-8333-333333333333";
        let (mut file, path) = claude_transcript(home.path(), agent_id);
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":"First"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();
        let first = load(
            home.path(),
            &path,
            &AgentKind::CLAUDE_CODE,
            agent_id,
            "/tmp/project",
            None,
        )
        .unwrap()
        .unwrap();
        assert!(
            load(
                home.path(),
                &path,
                &AgentKind::CLAUDE_CODE,
                agent_id,
                "/tmp/project",
                Some(first.version),
            )
            .unwrap()
            .is_none()
        );
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":"Second"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();
        let appended = load(
            home.path(),
            &path,
            &AgentKind::CLAUDE_CODE,
            agent_id,
            "/tmp/project",
            Some(first.version),
        )
        .unwrap()
        .unwrap();
        assert_eq!(appended.document.turns.len(), 2);
        assert_eq!(appended.document.turns[1].text, "Second");
    }

    #[cfg(unix)]
    #[test]
    fn transcript_open_rejects_symlinks_and_fifos_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let home = tempfile::tempdir().unwrap();
        let agent_id = "44444444-4444-4444-8444-444444444444";
        let (mut regular, regular_path) = claude_transcript(home.path(), agent_id);
        writeln!(
            regular,
            r#"{{"type":"assistant","message":{{"content":"safe"}}}}"#
        )
        .unwrap();
        let link_id = "55555555-5555-4555-8555-555555555555";
        let link = regular_path.with_file_name(format!("{link_id}.jsonl"));
        symlink(&regular_path, &link).unwrap();
        assert!(
            load(
                home.path(),
                &link,
                &AgentKind::CLAUDE_CODE,
                link_id,
                "/tmp/project",
                None,
            )
            .is_err()
        );

        let fifo_id = "66666666-6666-4666-8666-666666666666";
        let fifo = regular_path.with_file_name(format!("{fifo_id}.jsonl"));
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(
            load(
                home.path(),
                &fifo,
                &AgentKind::CLAUDE_CODE,
                fifo_id,
                "/tmp/project",
                None,
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
