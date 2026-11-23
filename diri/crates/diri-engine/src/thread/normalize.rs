//! Provider JSONL records → normalized [`ThreadItem`]s.
//!
//! Each record is parsed on its own (no state carried between records), so
//! a record always yields the same items with the same keys however the file
//! was windowed. Unknown record types yield nothing: a provider adding a new
//! record kind degrades to "not shown", never to a wrong item.

use diri_proto::thread::{
    EditHunk, ItemKey, ThreadBody, ThreadImage, ThreadItem, ToolDetail, TurnUsage,
};
use serde_json::Value;

/// Assistant prose and user prompts above this are clipped with a notice.
const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Tool output shown in a collapsed strip; the TUI Lens has the rest.
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
/// File contents carried by Write and Edit cards.
const MAX_EDIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Claude,
    Codex,
}

/// Items produced by one record that starts at byte `offset`. `budget` is
/// the inline image allowance left in this read; images past it keep their
/// size but drop their payload.
pub fn record_items(
    provider: Provider,
    offset: u64,
    record: &Value,
    budget: &mut usize,
) -> Vec<ThreadItem> {
    let at_ms = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms);
    let bodies = match provider {
        Provider::Claude => claude(record, budget),
        Provider::Codex => codex(record, budget),
    };
    bodies
        .into_iter()
        .enumerate()
        .map(|(part, body)| ThreadItem {
            key: ItemKey {
                offset,
                part: u16::try_from(part).unwrap_or(u16::MAX),
            },
            at_ms,
            body,
        })
        .collect()
}

fn claude(record: &Value, budget: &mut usize) -> Vec<ThreadBody> {
    let kind = record.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("user" | "assistant"))
        || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        if kind == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            return vec![ThreadBody::Notice {
                text: "Context compacted".into(),
            }];
        }
        return Vec::new();
    }
    let Some(message) = record.get("message") else {
        return Vec::new();
    };
    let content = message.get("content");
    let mut out = Vec::new();
    if kind == Some("user") {
        match content {
            Some(Value::String(text)) => user_text(text, Vec::new(), &mut out),
            Some(Value::Array(blocks)) => {
                let mut text = String::new();
                let mut images = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(piece) = block.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(piece);
                            }
                        }
                        Some("image") => images.extend(claude_image(block, budget)),
                        Some("tool_result") => out.push(claude_tool_result(block, budget)),
                        _ => {}
                    }
                }
                if !text.is_empty() || !images.is_empty() {
                    user_text(&text, images, &mut out);
                }
            }
            _ => {}
        }
        return out;
    }
    if let Some(Value::Array(blocks)) = content {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        out.push(ThreadBody::Assistant {
                            text: bounded(text, MAX_TEXT_BYTES),
                        });
                    }
                }
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        out.push(ThreadBody::Thinking {
                            text: bounded(text, MAX_TEXT_BYTES),
                        });
                    }
                }
                Some("tool_use") => {
                    let name = str_at(block, "name");
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    out.push(ThreadBody::ToolCall {
                        call_id: str_at(block, "id"),
                        detail: claude_tool_detail(&name, &input),
                        name,
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(usage) = message.get("usage") {
        out.push(ThreadBody::Usage(TurnUsage {
            model: message
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.starts_with('<'))
                .map(str::to_owned),
            at_ms: record
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_ms)
                .unwrap_or_default(),
            input: u64_at(usage, "input_tokens"),
            output: u64_at(usage, "output_tokens"),
            cache_read: u64_at(usage, "cache_read_input_tokens"),
            cache_write: u64_at(usage, "cache_creation_input_tokens"),
            cache_write_1h: usage
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        }));
    }
    out
}

/// A user prompt, a slash-command chip, or nothing for the CLI's own
/// bookkeeping messages (command output echoes and caveats).
fn user_text(text: &str, images: Vec<ThreadImage>, out: &mut Vec<ThreadBody>) {
    let trimmed = text.trim();
    if let Some(name) = between(trimmed, "<command-name>", "</command-name>") {
        let args = between(trimmed, "<command-args>", "</command-args>").unwrap_or_default();
        out.push(ThreadBody::Command {
            name: name.trim().to_owned(),
            args: args.trim().to_owned(),
        });
        return;
    }
    if trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<command-message>")
    {
        return;
    }
    if trimmed.starts_with("[Request interrupted by user") {
        out.push(ThreadBody::Notice {
            text: "Interrupted".into(),
        });
        return;
    }
    if trimmed.is_empty() && images.is_empty() {
        return;
    }
    out.push(ThreadBody::User {
        text: bounded(trimmed, MAX_TEXT_BYTES),
        images,
    });
}

fn claude_tool_result(block: &Value, budget: &mut usize) -> ThreadBody {
    let mut preview = String::new();
    let mut images = Vec::new();
    match block.get("content") {
        Some(Value::String(text)) => preview.push_str(text),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !preview.is_empty() {
                                preview.push('\n');
                            }
                            preview.push_str(text);
                        }
                    }
                    Some("image") => images.extend(claude_image(part, budget)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    ThreadBody::ToolResult {
        call_id: str_at(block, "tool_use_id"),
        is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
        preview: bounded(&preview, MAX_PREVIEW_BYTES),
        images,
    }
}

fn claude_image(block: &Value, budget: &mut usize) -> Option<ThreadImage> {
    let source = block.get("source")?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let media_type = source.get("media_type").and_then(Value::as_str)?;
    let data = source.get("data").and_then(Value::as_str)?;
    Some(image(media_type, data, budget))
}

fn image(media_type: &str, base64: &str, budget: &mut usize) -> ThreadImage {
    let bytes = (base64.len() as u64 / 4) * 3;
    let fits = media_type.starts_with("image/") && base64.len() <= *budget;
    if fits {
        *budget -= base64.len();
    }
    ThreadImage {
        media_type: media_type.to_owned(),
        bytes,
        base64: fits.then(|| base64.to_owned()),
    }
}

fn claude_tool_detail(name: &str, input: &Value) -> ToolDetail {
    let path = || {
        input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .or_else(|| input.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match name {
        "Bash" | "BashOutput" => ToolDetail::Command {
            command: bounded(&str_at(input, "command"), MAX_PREVIEW_BYTES),
        },
        "Read" => ToolDetail::Read { path: path() },
        "Grep" | "Glob" => ToolDetail::Search {
            pattern: str_at(input, "pattern"),
            path: input.get("path").and_then(Value::as_str).map(str::to_owned),
        },
        "Edit" => ToolDetail::Edit {
            path: path(),
            hunks: vec![EditHunk {
                old: bounded(&str_at(input, "old_string"), MAX_EDIT_BYTES),
                new: bounded(&str_at(input, "new_string"), MAX_EDIT_BYTES),
            }],
        },
        "MultiEdit" => ToolDetail::Edit {
            path: path(),
            hunks: input
                .get("edits")
                .and_then(Value::as_array)
                .map(|edits| {
                    edits
                        .iter()
                        .take(64)
                        .map(|edit| EditHunk {
                            old: bounded(&str_at(edit, "old_string"), MAX_EDIT_BYTES),
                            new: bounded(&str_at(edit, "new_string"), MAX_EDIT_BYTES),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        "Write" => ToolDetail::Write {
            path: path(),
            content: bounded(&str_at(input, "content"), MAX_EDIT_BYTES),
        },
        "Task" | "Agent" => ToolDetail::Task {
            description: input
                .get("description")
                .or_else(|| input.get("prompt"))
                .and_then(Value::as_str)
                .map(|text| bounded(text, MAX_PREVIEW_BYTES))
                .unwrap_or_default(),
        },
        _ => ToolDetail::Other {
            input: bounded(&compact_json(input), MAX_PREVIEW_BYTES),
        },
    }
}

fn codex(record: &Value, budget: &mut usize) -> Vec<ThreadBody> {
    let Some(payload) = record.get("payload") else {
        return Vec::new();
    };
    let payload_type = payload.get("type").and_then(Value::as_str);
    match record.get("type").and_then(Value::as_str) {
        Some("turn_context") => payload
            .get("model")
            .and_then(Value::as_str)
            .map(|model| {
                vec![ThreadBody::Usage(TurnUsage {
                    model: Some(model.to_owned()),
                    at_ms: record
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339_ms)
                        .unwrap_or_default(),
                    ..TurnUsage::default()
                })]
            })
            .unwrap_or_default(),
        Some("event_msg") => match payload_type {
            Some("user_message") => {
                let text = str_at(payload, "message");
                let images = payload
                    .get("images")
                    .and_then(Value::as_array)
                    .map(|urls| {
                        urls.iter()
                            .filter_map(Value::as_str)
                            .filter_map(|url| {
                                let rest = url.strip_prefix("data:")?;
                                let (media_type, data) = rest.split_once(";base64,")?;
                                Some(image(media_type, data, budget))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut out = Vec::new();
                user_text(&text, images, &mut out);
                out
            }
            Some("token_count") => payload
                .pointer("/info/last_token_usage")
                .map(|usage| {
                    let cached = u64_at(usage, "cached_input_tokens");
                    vec![ThreadBody::Usage(TurnUsage {
                        model: None,
                        at_ms: record
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .and_then(parse_rfc3339_ms)
                            .unwrap_or_default(),
                        input: u64_at(usage, "input_tokens").saturating_sub(cached),
                        output: u64_at(usage, "output_tokens"),
                        cache_read: cached,
                        cache_write: 0,
                        cache_write_1h: 0,
                    })]
                })
                .unwrap_or_default(),
            Some("context_compacted") => vec![ThreadBody::Notice {
                text: "Context compacted".into(),
            }],
            Some("turn_aborted") => vec![ThreadBody::Notice {
                text: "Interrupted".into(),
            }],
            _ => Vec::new(),
        },
        Some("response_item") => match payload_type {
            Some("message") if payload.get("role").and_then(Value::as_str) == Some("assistant") => {
                let text = payload
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|part| {
                                part.get("type").and_then(Value::as_str) == Some("output_text")
                            })
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![ThreadBody::Assistant {
                        text: bounded(&text, MAX_TEXT_BYTES),
                    }]
                }
            }
            Some("reasoning") => {
                let text = payload
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![ThreadBody::Thinking {
                        text: bounded(&text, MAX_TEXT_BYTES),
                    }]
                }
            }
            Some("function_call") => {
                let name = str_at(payload, "name");
                let arguments = payload
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or(Value::Null);
                vec![ThreadBody::ToolCall {
                    call_id: str_at(payload, "call_id"),
                    detail: codex_tool_detail(&name, &arguments),
                    name,
                }]
            }
            Some("custom_tool_call") => {
                let name = str_at(payload, "name");
                let input = str_at(payload, "input");
                let detail = if name == "apply_patch" {
                    ToolDetail::Patch {
                        patch: bounded(&input, MAX_EDIT_BYTES),
                    }
                } else {
                    ToolDetail::Other {
                        input: bounded(&input, MAX_PREVIEW_BYTES),
                    }
                };
                vec![ThreadBody::ToolCall {
                    call_id: str_at(payload, "call_id"),
                    name,
                    detail,
                }]
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let raw = payload.get("output");
                let (text, is_error) = codex_output(raw);
                vec![ThreadBody::ToolResult {
                    call_id: str_at(payload, "call_id"),
                    is_error,
                    preview: bounded(&text, MAX_PREVIEW_BYTES),
                    images: Vec::new(),
                }]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn codex_tool_detail(name: &str, arguments: &Value) -> ToolDetail {
    match name {
        "shell" | "exec_command" | "container.exec" | "local_shell" => {
            let command = match arguments.get("command").or_else(|| arguments.get("cmd")) {
                Some(Value::Array(argv)) => {
                    let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
                    // `bash -lc "<script>"` reads best as the script.
                    match argv.as_slice() {
                        [shell, flag, script] if shell.ends_with("sh") && flag.starts_with('-') => {
                            (*script).to_owned()
                        }
                        _ => argv.join(" "),
                    }
                }
                Some(Value::String(command)) => command.clone(),
                _ => String::new(),
            };
            ToolDetail::Command {
                command: bounded(&command, MAX_PREVIEW_BYTES),
            }
        }
        "apply_patch" => ToolDetail::Patch {
            patch: bounded(
                arguments
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                MAX_EDIT_BYTES,
            ),
        },
        _ => ToolDetail::Other {
            input: bounded(&compact_json(arguments), MAX_PREVIEW_BYTES),
        },
    }
}

/// Codex wraps shell output as `{"output": "...", "metadata": {"exit_code": n}}`
/// inside a JSON string; other tools return plain text.
fn codex_output(raw: Option<&Value>) -> (String, bool) {
    let Some(raw) = raw else {
        return (String::new(), false);
    };
    let text = match raw {
        Value::String(text) => text.clone(),
        other => compact_json(other),
    };
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&text) {
        let exit = object
            .get("metadata")
            .and_then(|meta| meta.get("exit_code"))
            .and_then(Value::as_i64);
        if let Some(output) = object.get("output").and_then(Value::as_str) {
            return (output.to_owned(), exit.is_some_and(|code| code != 0));
        }
    }
    // `Exit code: 1\nWall time: ...\nOutput:\n...` from exec_command.
    let is_error = text
        .strip_prefix("Exit code: ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|code| code.parse::<i64>().ok())
        .is_some_and(|code| code != 0);
    (text, is_error)
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

fn str_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn compact_json(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Normalizes line endings, replaces control characters (escape sequences
/// never reach the GUI), and clips at a character boundary.
pub fn bounded(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len().min(limit));
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        let character = match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                '\n'
            }
            '\n' | '\t' => character,
            c if c.is_control() => '\u{fffd}',
            c => c,
        };
        if out.len() + character.len_utf8() > limit {
            out.push_str("\n[… clipped by diri …]");
            break;
        }
        out.push(character);
    }
    out
}

/// `2026-11-23T10:15:30.123Z` (or with a numeric offset) → Unix milliseconds.
pub fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let b = value.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        std::str::from_utf8(&b[from..to]).ok()?.parse().ok()
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut cursor = 19;
    let mut millis = 0i64;
    if b.get(cursor) == Some(&b'.') {
        cursor += 1;
        let mut scale = 100;
        while let Some(digit) = b.get(cursor).filter(|d| d.is_ascii_digit()) {
            millis += i64::from(digit - b'0') * scale;
            scale /= 10;
            cursor += 1;
        }
    }
    let offset_minutes = match b.get(cursor) {
        Some(b'Z' | b'z') => 0,
        Some(sign @ (b'+' | b'-')) if b.len() >= cursor + 6 => {
            let hours = num(cursor + 1, cursor + 3)?;
            let minutes = num(cursor + 4, cursor + 6)?;
            let total = hours * 60 + minutes;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::thread::THREAD_IMAGE_MAX_BYTES;
    use serde_json::json;

    fn items(provider: Provider, record: Value) -> Vec<ThreadBody> {
        let mut budget = THREAD_IMAGE_MAX_BYTES;
        record_items(provider, 0, &record, &mut budget)
            .into_iter()
            .map(|item| item.body)
            .collect()
    }

    #[test]
    fn rfc3339_parses_utc_fractions_and_offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-11-23T10:15:30.123Z"),
            Some(1_795_428_930_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-23T12:15:30.123+02:00"),
            Some(1_795_428_930_123)
        );
        assert_eq!(parse_rfc3339_ms("yesterday"), None);
    }

    #[test]
    fn claude_assistant_blocks_become_ordered_items_with_usage() {
        let bodies = items(
            Provider::Claude,
            json!({
                "type": "assistant",
                "timestamp": "2026-11-23T10:00:00Z",
                "message": {
                    "model": "claude-opus-5-5",
                    "content": [
                        {"type": "thinking", "thinking": "plan"},
                        {"type": "text", "text": "Fixing it."},
                        {"type": "tool_use", "id": "toolu_1", "name": "Edit",
                         "input": {"file_path": "/r/src/a.rs", "old_string": "a", "new_string": "b"}}
                    ],
                    "usage": {"input_tokens": 2, "output_tokens": 9,
                              "cache_read_input_tokens": 100, "cache_creation_input_tokens": 50,
                              "cache_creation": {"ephemeral_1h_input_tokens": 50}}
                }
            }),
        );
        assert_eq!(bodies.len(), 4);
        assert!(matches!(&bodies[0], ThreadBody::Thinking { text } if text == "plan"));
        assert!(matches!(&bodies[1], ThreadBody::Assistant { text } if text == "Fixing it."));
        assert!(matches!(
            &bodies[2],
            ThreadBody::ToolCall { detail: ToolDetail::Edit { path, hunks }, .. }
                if path == "/r/src/a.rs" && hunks[0].new == "b"
        ));
        let ThreadBody::Usage(usage) = &bodies[3] else {
            panic!("usage expected")
        };
        assert_eq!(usage.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!((usage.cache_read, usage.cache_write_1h), (100, 50));
    }

    #[test]
    fn claude_bookkeeping_is_hidden_and_commands_become_chips() {
        assert!(items(Provider::Claude, json!({"type":"api-request-blob","message":{"content":[{"type":"text","text":"dup"}]}})).is_empty());
        assert!(
            items(
                Provider::Claude,
                json!({"type":"user","isSidechain":true,"message":{"content":"sub"}})
            )
            .is_empty()
        );
        assert!(items(Provider::Claude, json!({"type":"user","message":{"content":"<local-command-stdout>ok</local-command-stdout>"}})).is_empty());
        assert_eq!(
            items(
                Provider::Claude,
                json!({"type":"user","message":{"content":"<command-name>/model</command-name>\n<command-args>sonnet</command-args>"}})
            ),
            vec![ThreadBody::Command {
                name: "/model".into(),
                args: "sonnet".into()
            }]
        );
    }

    #[test]
    fn claude_tool_results_and_images_respect_the_budget() {
        let mut budget = 8;
        let bodies: Vec<_> = record_items(
            Provider::Claude,
            7,
            &json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"boom"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAAAAAA"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"BBBB"}}
            ]}}),
            &mut budget,
        )
        .into_iter()
        .map(|item| (item.key, item.body))
        .collect();
        assert_eq!(bodies[0].0, ItemKey { offset: 7, part: 0 });
        assert!(
            matches!(&bodies[0].1, ThreadBody::ToolResult { is_error: true, preview, .. } if preview == "boom")
        );
        let ThreadBody::User { images, .. } = &bodies[1].1 else {
            panic!("user images expected")
        };
        assert!(images[0].base64.is_some());
        assert!(images[1].base64.is_none(), "over budget keeps size only");
        assert_eq!(budget, 0);
    }

    #[test]
    fn codex_records_map_calls_outputs_and_models() {
        let call = items(
            Provider::Codex,
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1",
                "arguments":"{\"command\":[\"bash\",\"-lc\",\"cargo test\"]}"}}),
        );
        assert_eq!(
            call,
            vec![ThreadBody::ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                detail: ToolDetail::Command {
                    command: "cargo test".into()
                }
            }]
        );
        let output = items(
            Provider::Codex,
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c1",
                "output":"{\"output\":\"1 failed\",\"metadata\":{\"exit_code\":101}}"}}),
        );
        assert!(
            matches!(&output[0], ThreadBody::ToolResult { is_error: true, preview, .. } if preview == "1 failed")
        );
        let context = items(
            Provider::Codex,
            json!({"type":"turn_context","timestamp":"2026-11-23T10:00:00Z","payload":{"model":"gpt-5.5-codex"}}),
        );
        assert!(
            matches!(&context[0], ThreadBody::Usage(usage) if usage.model.as_deref() == Some("gpt-5.5-codex"))
        );
        assert!(items(Provider::Codex, json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>"}]}})).is_empty());
    }

    #[test]
    fn text_is_sanitized_and_clipped_on_a_char_boundary() {
        assert_eq!(bounded("a\r\nb\u{1b}[31m", 100), "a\nb\u{fffd}[31m");
        let clipped = bounded("ééééé", 5);
        assert!(clipped.starts_with("éé"));
        assert!(clipped.ends_with("clipped by diri …]"));
    }
}
