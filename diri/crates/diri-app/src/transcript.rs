//! Read-only, bounded projection of live Claude and Codex transcripts.
//!
//! Transcript files are provider-owned JSONL. This module extracts only
//! human/agent text turns for the inspector; malformed records and tool data
//! are ignored, and no transcript bytes are ever modified.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use diri_proto::AgentKind;
use serde_json::Value;

const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TURNS: usize = 100;
const MAX_TURN_BYTES: usize = 256 * 1024;

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

pub fn load(path: &Path, kind: &AgentKind) -> io::Result<TranscriptDocument> {
    if !matches!(kind.id(), AgentKind::CLAUDE_CODE_ID | AgentKind::CODEX_ID) {
        return Ok(TranscriptDocument::default());
    }
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let clipped = length > MAX_READ_BYTES;
    if clipped {
        file.seek(SeekFrom::Start(length - MAX_READ_BYTES))?;
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
    Ok(TranscriptDocument { turns })
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
    let sanitized: String = text
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
    use std::io::Write;

    #[test]
    fn loads_real_claude_turn_shapes_and_ignores_tool_records() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","message":{{"content":"Fix it"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"tool_result","content":"secret"}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Done"}},{{"type":"tool_use","name":"edit"}}]}}}}"#).unwrap();
        let document = load(file.path(), &AgentKind::CLAUDE_CODE).unwrap();
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
        let mut file = tempfile::NamedTempFile::new().unwrap();
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
        let document = load(file.path(), &AgentKind::CODEX).unwrap();
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
            load(file.path(), &AgentKind::SHELL)
                .unwrap()
                .turns
                .is_empty()
        );
    }
}
