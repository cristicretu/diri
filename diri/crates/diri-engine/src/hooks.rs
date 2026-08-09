//! Parsing hook and notify payloads into status signals.
//!
//! Agents report what they are doing out of band: Claude through hook
//! callbacks, Codex through a notify program. The `dirijor` CLI forwards those
//! raw JSON payloads to the daemon, and this turns them into the signals the
//! reducer understands, plus the side metadata they carry — session identity,
//! transcript path, a title for the first prompt.
//!
//! Ported from the Swift `HookParsing`.

use diri_proto::{NeedsInputDetail, NeedsInputKind, NeedsInputSource};
use serde_json::Value;

use crate::detect::redact;
use crate::status::{ClaudeHook, StatusSignal, classify_risk};

/// What a payload carries besides the signal itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HookMetadata {
    pub agent_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub first_prompt_title: Option<String>,
    pub needs_input: Option<NeedsInputDetail>,
}

/// Parses a Claude hook payload. Returns `None` for events we do not model.
pub fn parse_claude_hook(
    event: &str,
    payload: &Value,
    now: std::time::SystemTime,
) -> Option<(StatusSignal, HookMetadata)> {
    let mut meta = HookMetadata::default();
    let is_subagent = string(payload, "agent_id").is_some();

    // Identity rides on *every* payload, not just SessionStart: the transcript
    // moves to a different project directory when an agent enters a worktree
    // mid-session, and capturing it once would leave the record pointing at the
    // pre-worktree path forever.
    meta.agent_session_id = string(payload, "session_id");
    meta.transcript_path = string(payload, "transcript_path");

    let hook = match event {
        "SessionStart" => ClaudeHook::SessionStart,
        "UserPromptSubmit" => {
            let prompt = string(payload, "prompt_text").or_else(|| string(payload, "prompt"));
            if let Some(prompt) = &prompt
                && !is_subagent
            {
                meta.first_prompt_title = Some(title_from_prompt(prompt));
            }
            ClaudeHook::UserPromptSubmit
        }
        "PreToolUse" => ClaudeHook::PreToolUse,
        "PermissionRequest" => {
            let (tool, summary) = tool_summary(payload);
            if !is_subagent {
                meta.needs_input = Some(NeedsInputDetail {
                    kind: NeedsInputKind::Permission,
                    source: NeedsInputSource::ClaudePermissionHook,
                    tool_name: tool.clone(),
                    summary: permission_summary(tool.as_deref(), summary.as_deref()),
                    prompt_excerpt: summary.clone(),
                    options: None,
                    risk_hint: classify_risk(summary.as_deref().unwrap_or_default()),
                    occurred_at: now.into(),
                });
            }
            ClaudeHook::PermissionRequest {
                tool_name: tool,
                input_summary: summary,
            }
        }
        "Notification" => {
            let notification_type = string(payload, "notification_type");
            let message = string(payload, "message");
            if !is_subagent && let Some(kind) = needs_input_kind(notification_type.as_deref()) {
                let text = message
                    .clone()
                    .unwrap_or_else(|| "Claude needs your input".into());
                meta.needs_input = Some(NeedsInputDetail {
                    kind,
                    source: NeedsInputSource::ClaudeNotificationHook,
                    tool_name: None,
                    summary: redact(&text),
                    prompt_excerpt: None,
                    options: None,
                    risk_hint: classify_risk(&text),
                    occurred_at: now.into(),
                });
            }
            ClaudeHook::Notification {
                notification_type,
                message,
            }
        }
        "Stop" => ClaudeHook::Stop,
        "SubagentStart" => {
            ClaudeHook::SubagentStart(string(payload, "agent_id").unwrap_or_else(|| "?".into()))
        }
        "SubagentStop" => {
            ClaudeHook::SubagentStop(string(payload, "agent_id").unwrap_or_else(|| "?".into()))
        }
        "SessionEnd" => ClaudeHook::SessionEnd,
        _ => return None,
    };

    Some((StatusSignal::ClaudeHook { hook, is_subagent }, meta))
}

/// Parses a Codex notify payload. Only turn-completion is meaningful.
pub fn parse_codex_notify(payload: &Value) -> Option<(StatusSignal, HookMetadata)> {
    if string(payload, "type").as_deref() != Some("agent-turn-complete") {
        return None;
    }
    let mut meta = HookMetadata {
        agent_session_id: string(payload, "thread-id"),
        ..Default::default()
    };
    if let Some(first) = payload
        .get("input-messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.first())
        .and_then(Value::as_str)
    {
        meta.first_prompt_title = Some(title_from_prompt(first));
    }
    Some((StatusSignal::CodexTurnComplete, meta))
}

fn needs_input_kind(notification_type: Option<&str>) -> Option<NeedsInputKind> {
    match notification_type {
        Some("permission_prompt") => Some(NeedsInputKind::Permission),
        Some("idle_prompt") | Some("agent_needs_input") | Some("elicitation_dialog") => {
            Some(NeedsInputKind::Question)
        }
        _ => None,
    }
}

/// `(tool_name, a human summary of tool_input)`.
pub fn tool_summary(payload: &Value) -> (Option<String>, Option<String>) {
    let tool = string(payload, "tool_name");
    let Some(input) = payload.get("tool_input").and_then(Value::as_object) else {
        return (tool, None);
    };
    let summary = match tool.as_deref() {
        Some("Bash") => input.get("command").and_then(Value::as_str),
        Some("Edit") | Some("Write") | Some("Read") | Some("NotebookEdit") => {
            input.get("file_path").and_then(Value::as_str)
        }
        Some("WebFetch") | Some("WebSearch") => input
            .get("url")
            .and_then(Value::as_str)
            .or_else(|| input.get("query").and_then(Value::as_str)),
        _ => input.values().find_map(Value::as_str),
    };
    // Truncate before redacting so a huge tool input cannot blow up the record.
    let summary = summary.map(|text| redact(&text.chars().take(120).collect::<String>()));
    (tool, summary)
}

/// The sentence shown in the sidebar when an agent asks for permission.
pub fn permission_summary(tool: Option<&str>, input_summary: Option<&str>) -> String {
    match tool {
        Some("Bash") => format!("wants to run `{}`", input_summary.unwrap_or("a command")),
        Some("Edit") | Some("Write") => format!("wants to edit {}", short_path(input_summary)),
        Some("WebFetch") => format!("wants to fetch {}", input_summary.unwrap_or("a URL")),
        // MCP tools arrive as `mcp__server__tool`; show `server:tool`.
        Some(name) if name.starts_with("mcp__") => {
            format!("wants to use {}", name[5..].replace("__", ":"))
        }
        Some(name) => match input_summary {
            Some(detail) => format!("wants to use {name}: {detail}"),
            None => format!("wants to use {name}"),
        },
        None => match input_summary {
            Some(detail) => format!("needs permission: {detail}"),
            None => "needs permission".into(),
        },
    }
}

/// A title from the first prompt: one line, trimmed to something a sidebar row
/// can show.
pub(crate) fn title_from_prompt(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let cleaned = redact(first_line);
    let trimmed: String = cleaned.chars().take(60).collect();
    trimmed.trim().to_string()
}

fn short_path(path: Option<&str>) -> String {
    let Some(path) = path else {
        return "a file".into();
    };
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let tail = parts.len().saturating_sub(2);
    parts[tail..].join("/")
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::RiskHint;
    use serde_json::json;

    fn now() -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn identity_is_captured_from_every_payload_not_just_session_start() {
        // The transcript moves when an agent enters a worktree mid-session;
        // reading it only at SessionStart leaves the record pointing at the old
        // path forever.
        let payload = json!({
            "session_id": "abc-123",
            "transcript_path": "/new/project/dir/abc-123.jsonl",
        });
        let (_, meta) = parse_claude_hook("Stop", &payload, now()).expect("parsed");
        assert_eq!(meta.agent_session_id.as_deref(), Some("abc-123"));
        assert_eq!(
            meta.transcript_path.as_deref(),
            Some("/new/project/dir/abc-123.jsonl")
        );
    }

    #[test]
    fn a_payload_with_an_agent_id_is_a_subagents() {
        let payload = json!({ "session_id": "s", "agent_id": "sub-1" });
        let (signal, _) = parse_claude_hook("Stop", &payload, now()).expect("parsed");
        match signal {
            StatusSignal::ClaudeHook { is_subagent, .. } => assert!(is_subagent),
            other => panic!("expected a hook, got {other:?}"),
        }
    }

    #[test]
    fn a_subagent_prompt_does_not_retitle_the_parent() {
        let payload = json!({
            "session_id": "s",
            "agent_id": "sub-1",
            "prompt": "go do a subtask",
        });
        let (_, meta) = parse_claude_hook("UserPromptSubmit", &payload, now()).expect("parsed");
        assert_eq!(meta.first_prompt_title, None);
    }

    #[test]
    fn the_first_prompt_becomes_a_title() {
        let payload = json!({ "session_id": "s", "prompt": "  Fix the login bug\nmore detail" });
        let (_, meta) = parse_claude_hook("UserPromptSubmit", &payload, now()).expect("parsed");
        assert_eq!(
            meta.first_prompt_title.as_deref(),
            Some("Fix the login bug")
        );
    }

    #[test]
    fn a_bash_permission_reads_as_a_command_with_its_risk() {
        let payload = json!({
            "session_id": "s",
            "tool_name": "Bash",
            "tool_input": { "command": "rm -rf build" },
        });
        let (_, meta) = parse_claude_hook("PermissionRequest", &payload, now()).expect("parsed");
        let detail = meta.needs_input.expect("a permission detail");
        assert_eq!(detail.summary, "wants to run `rm -rf build`");
        assert_eq!(detail.risk_hint, RiskHint::Destructive);
    }

    #[test]
    fn an_mcp_tool_name_is_made_readable() {
        assert_eq!(
            permission_summary(Some("mcp__dirijor__spawn_agent"), None),
            "wants to use dirijor:spawn_agent"
        );
    }

    #[test]
    fn an_edit_permission_shows_a_short_path() {
        let payload = json!({
            "session_id": "s",
            "tool_name": "Edit",
            "tool_input": { "file_path": "/Users/someone/code/project/src/main.rs" },
        });
        let (_, meta) = parse_claude_hook("PermissionRequest", &payload, now()).expect("parsed");
        let detail = meta.needs_input.expect("detail");
        assert_eq!(detail.summary, "wants to edit src/main.rs");
    }

    #[test]
    fn secrets_in_a_tool_input_are_masked() {
        let payload = json!({
            "session_id": "s",
            "tool_name": "Bash",
            "tool_input": { "command": "deploy --token=sk-secret-value" },
        });
        let (_, summary) = tool_summary(&payload);
        let summary = summary.expect("a summary");
        assert!(!summary.contains("sk-secret-value"), "leaked: {summary}");
        assert!(summary.contains("•••"));
    }

    #[test]
    fn a_notification_asking_for_input_carries_a_detail() {
        let payload = json!({
            "session_id": "s",
            "notification_type": "idle_prompt",
            "message": "Waiting for your answer",
        });
        let (_, meta) = parse_claude_hook("Notification", &payload, now()).expect("parsed");
        let detail = meta.needs_input.expect("detail");
        assert_eq!(detail.kind, NeedsInputKind::Question);
        assert_eq!(detail.summary, "Waiting for your answer");
    }

    #[test]
    fn an_unknown_event_is_ignored_rather_than_guessed_at() {
        let payload = json!({ "session_id": "s" });
        assert!(parse_claude_hook("SomeFutureHook", &payload, now()).is_none());
    }

    #[test]
    fn codex_turn_completion_is_recognized() {
        let payload = json!({
            "type": "agent-turn-complete",
            "thread-id": "t-1",
            "input-messages": ["Add a test for the parser"],
            "last-assistant-message": "done",
        });
        let (signal, meta) = parse_codex_notify(&payload).expect("parsed");
        assert!(matches!(signal, StatusSignal::CodexTurnComplete));
        assert_eq!(meta.agent_session_id.as_deref(), Some("t-1"));
        assert_eq!(
            meta.first_prompt_title.as_deref(),
            Some("Add a test for the parser")
        );
    }

    #[test]
    fn other_codex_notifications_are_not_turn_completions() {
        let payload = json!({ "type": "something-else", "thread-id": "t-1" });
        assert!(parse_codex_notify(&payload).is_none());
    }
}
