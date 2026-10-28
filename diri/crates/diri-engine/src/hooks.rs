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
    pub identity: crate::attention::SignalIdentity,
    pub agent_session_id: Option<String>,
    /// Whether this payload may move the session to a *different*
    /// conversation. Claude only switches conversation through SessionStart
    /// (startup, `/resume`, `/clear`); any other hook naming another
    /// conversation came from a different Claude process that inherited this
    /// session's environment, and must not rename or re-point the tab.
    pub binds_conversation: bool,
    pub transcript_path: Option<String>,
    pub first_prompt_title: Option<String>,
    pub needs_input: Option<NeedsInputDetail>,
    /// The directory the main Agent reports working in (`cwd`). Absolute and
    /// free of `.`/`..` components; subagents never move the Session.
    pub cwd: Option<String>,
    /// A file an edit tool just changed, from the Agent or a subagent. Lets a
    /// client follow work done in another checkout without a `cd`.
    pub edited_path: Option<String>,
}

/// Claude's file-editing tools, by the `tool_input` key naming the file.
const EDIT_TOOLS: [(&str, &str); 4] = [
    ("Edit", "file_path"),
    ("MultiEdit", "file_path"),
    ("Write", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

/// The path a payload's edit tool changed, resolved against its `cwd`.
fn edited_path(payload: &Value) -> Option<String> {
    let tool = payload.get("tool_name").and_then(Value::as_str)?;
    let (_, key) = EDIT_TOOLS.iter().find(|(name, _)| *name == tool)?;
    let raw = payload.get("tool_input")?.get(*key)?.as_str()?;
    if raw.starts_with('/') {
        return workspace_path(raw);
    }
    let cwd = workspace_path(payload.get("cwd")?.as_str()?)?;
    workspace_path(&format!("{}/{raw}", cwd.trim_end_matches('/')))
}

/// Accepts only an absolute, lexically normal path a client can map into a
/// checkout by prefix: `..` would let a path name one place and mean another.
pub(crate) fn workspace_path(path: &str) -> Option<String> {
    if !path.starts_with('/') || path.len() > 4096 || path.contains('\0') {
        return None;
    }
    if path
        .split('/')
        .any(|component| component == ".." || component == ".")
    {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    Some(if trimmed.is_empty() { "/" } else { trimmed }.to_owned())
}

/// Parses a Claude hook payload. Returns `None` for events we do not model.
pub fn parse_claude_hook(
    event: &str,
    payload: &Value,
    now: std::time::SystemTime,
) -> Option<(StatusSignal, HookMetadata)> {
    // A stale hook configuration must not route a child lifecycle callback
    // through the parent's Stop command. Payloads without the discriminator
    // retain compatibility with older agents.
    if payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .is_some_and(|reported| reported != event)
    {
        return None;
    }
    let mut meta = HookMetadata::default();
    if event == "PreToolUse" {
        meta.identity.started_tool = scoped_identity(payload, "session_id", "tool_use_id")
            .zip(string(payload, "tool_name").filter(|name| name.len() <= 128));
    }
    if event == "PermissionRequest" {
        meta.identity.request = scoped_identity(payload, "session_id", "tool_use_id");
    }
    if matches!(event, "PostToolUse" | "PostToolUseFailure") {
        meta.identity.resolved_request = scoped_identity(payload, "session_id", "tool_use_id");
    }
    let is_subagent = string(payload, "agent_id").is_some();

    // Identity rides on *every* payload, not just SessionStart: the transcript
    // moves to a different project directory when an agent enters a worktree
    // mid-session, and capturing it once would leave the record pointing at the
    // pre-worktree path forever.
    meta.agent_session_id =
        string(payload, "session_id").or_else(|| string(payload, "conversation_id"));
    // Cursor reports `conversation_id` and has no SessionStart of its own.
    meta.binds_conversation = event == "SessionStart" || string(payload, "session_id").is_none();
    meta.transcript_path = string(payload, "transcript_path");
    if !is_subagent {
        meta.cwd = string(payload, "cwd").and_then(|cwd| workspace_path(&cwd));
    }
    if matches!(event, "PostToolUse") {
        meta.edited_path = edited_path(payload);
    }

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
        "PostToolUse" | "PostToolUseFailure" => ClaudeHook::PostToolUse,
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
                    secret: false,
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
                    secret: false,
                    occurred_at: now.into(),
                });
            }
            ClaudeHook::Notification {
                notification_type,
                message,
            }
        }
        "Stop" => match string(payload, "last_assistant_message")
            .as_deref()
            .and_then(trailing_question)
        {
            Some(question) => ClaudeHook::StopAsking { question },
            None => ClaudeHook::Stop,
        },
        "SubagentStart" => {
            ClaudeHook::SubagentStart(string(payload, "agent_id").unwrap_or_else(|| "?".into()))
        }
        "SubagentStop" => {
            ClaudeHook::SubagentStop(string(payload, "agent_id").unwrap_or_else(|| "?".into()))
        }
        "SessionEnd" => ClaudeHook::SessionEnd,
        _ => return None,
    };

    Some((
        StatusSignal::ClaudeHook {
            hook,
            is_subagent,
            pending_work: diri_proto::recovery::claude_pending_work(payload),
        },
        meta,
    ))
}

// Length-prefixed JSON tuples avoid ambiguities and scope native ids to their conversation.
fn scoped_identity(payload: &Value, scope: &str, key: &str) -> Option<String> {
    let id = string(payload, key).filter(|id| !id.is_empty() && id.len() <= 256)?;
    let scope = string(payload, scope).filter(|scope| scope.len() <= 256)?;
    serde_json::to_string(&(scope, id)).ok()
}

/// Parses a Codex notify payload. Only turn-completion is meaningful.
pub fn parse_codex_notify(payload: &Value) -> Option<(StatusSignal, HookMetadata)> {
    if string(payload, "type").as_deref() != Some("agent-turn-complete") {
        return None;
    }
    let mut meta = HookMetadata {
        identity: crate::attention::SignalIdentity {
            completion: scoped_identity(payload, "thread-id", "turn-id"),
            ..Default::default()
        },
        agent_session_id: string(payload, "thread-id"),
        // Codex learns and changes threads only through notify.
        binds_conversation: true,
        cwd: string(payload, "cwd").and_then(|cwd| workspace_path(&cwd)),
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

/// Rehydrates the privacy-filtered activity written by the hook CLI while the
/// Engine was unavailable. The seed deliberately contains too little data to
/// recreate prompt titles or command summaries; lifecycle and conversation
/// identity are the durable contract.
pub fn parse_activity_seed(
    seed: &diri_proto::recovery::HookActivitySeed,
) -> Option<(StatusSignal, HookMetadata)> {
    if seed.version != diri_proto::recovery::HookActivitySeed::VERSION {
        return None;
    }
    let now = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_millis(seed.occurred_at_ms))
        .unwrap_or(std::time::SystemTime::now());
    let mut payload = serde_json::Map::new();
    if let Some(value) = &seed.agent_session_id {
        payload.insert(
            if seed.kind == "codex-notify" {
                "thread-id"
            } else {
                "session_id"
            }
            .into(),
            Value::String(value.clone()),
        );
    }
    for (key, value) in [
        ("transcript_path", seed.transcript_path.as_ref()),
        ("notification_type", seed.notification_type.as_ref()),
        ("tool_name", seed.tool_name.as_ref()),
        ("tool_use_id", seed.native_request_id.as_ref()),
        ("turn-id", seed.native_turn_id.as_ref()),
    ] {
        if let Some(value) = value {
            payload.insert(key.into(), Value::String(value.clone()));
        }
    }
    let payload = Value::Object(payload);
    match seed.kind.as_str() {
        "claude-hook" => {
            let (mut signal, metadata) = parse_claude_hook(seed.event.as_deref()?, &payload, now)?;
            if let StatusSignal::ClaudeHook { pending_work, .. } = &mut signal {
                *pending_work = seed.claude_pending_work;
            }
            Some((signal, metadata))
        }
        "codex-notify" => {
            let mut payload = payload.as_object().cloned().unwrap_or_default();
            payload.insert("type".into(), Value::String("agent-turn-complete".into()));
            parse_codex_notify(&Value::Object(payload))
        }
        _ => None,
    }
}

/// The longest question a turn-end detail carries.
const TURN_QUESTION_MAX_CHARS: usize = 160;

/// The question a turn's final message hands back, when it hands one back:
/// the last question in its final paragraph, outside any code block. Agents
/// often ask and then add a line of context ("Should I go ahead? It's a
/// one-line change."), so the question need not be the last sentence. Only
/// that question is kept, redacted. A message ending in code, results or
/// statements asks nothing; a `?` inside a URL or word is not a question.
pub(crate) fn trailing_question(message: &str) -> Option<String> {
    let mut in_code = false;
    let mut paragraph: Vec<&str> = Vec::new();
    for line in message.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            paragraph.clear();
            continue;
        }
        if in_code {
            continue;
        }
        if trimmed.is_empty() {
            paragraph.clear();
        } else {
            paragraph.push(trimmed);
        }
    }
    if in_code {
        return None;
    }
    paragraph
        .iter()
        .rev()
        .find_map(|line| last_question_in(line))
        .map(|question| {
            let question = redact(&question);
            match question.char_indices().nth(TURN_QUESTION_MAX_CHARS) {
                Some((end, _)) => format!("{}…", &question[..end]),
                None => question,
            }
        })
}

/// The last sentence of `line` that ends in a question mark followed by a
/// space or the line's end, after closing emphasis. A mark that closes a
/// quote or code span is quoted text (`the "switch plans?" dialog`).
fn last_question_in(line: &str) -> Option<String> {
    const CLOSERS: &str = "*_)]";
    let line = line.trim_start_matches(|c: char| c.is_whitespace() || "*_>-#•".contains(c));
    let mut end = None;
    for (i, c) in line.char_indices() {
        if c != '?' && c != '？' {
            continue;
        }
        let after = line[i + c.len_utf8()..].trim_start_matches(|c: char| CLOSERS.contains(c));
        if after.is_empty() || after.starts_with(char::is_whitespace) {
            end = Some(i + c.len_utf8());
        }
    }
    let end = end?;
    let body = &line[..end];
    let mark = body.chars().last()?.len_utf8();
    let head = &body[..body.len() - mark];
    // The sentence starts after the previous sentence's end.
    let start = head
        .char_indices()
        .rev()
        .find(|(i, c)| {
            matches!(c, '.' | '!' | '?' | '。' | '？' | ':')
                && head[i + c.len_utf8()..]
                    .trim_start_matches(|c: char| CLOSERS.contains(c))
                    .starts_with(char::is_whitespace)
        })
        .map_or(0, |(i, c)| i + c.len_utf8());
    let sentence = body[start..]
        .trim()
        .trim_start_matches(|c: char| c.is_whitespace() || CLOSERS.contains(c));
    // A word or two before the mark: "Go?" counts, a lone "?" does not.
    (sentence.chars().filter(|c| c.is_alphanumeric()).count() >= 2).then(|| sentence.to_owned())
}

fn needs_input_kind(notification_type: Option<&str>) -> Option<NeedsInputKind> {
    match notification_type {
        Some("permission_prompt") => Some(NeedsInputKind::Permission),
        Some("agent_needs_input") | Some("elicitation_dialog") => Some(NeedsInputKind::Question),
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
        .map(without_leading_paths)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let cleaned = redact(first_line);
    let trimmed: String = cleaned.chars().take(60).collect();
    trimmed.trim().to_string()
}

/// Drops file paths at the start of a prompt line. A pasted clipboard image
/// or a Finder drop reaches the PTY as a path the Agent turns into an
/// attachment (`[Image #1]`); it names a temp file, not the conversation.
/// Paths may be quoted or carry Terminal.app-style `\ ` escapes. A slash
/// command (`/review`) has a single separator and is kept.
fn without_leading_paths(line: &str) -> &str {
    let mut rest = line.trim();
    loop {
        let token_end = path_token_end(rest);
        let token = rest[..token_end].trim_matches(|c| c == '\'' || c == '"');
        let is_path =
            (token.starts_with('/') || token.starts_with("~/")) && token.matches('/').count() >= 2;
        if !is_path {
            return rest;
        }
        rest = rest[token_end..].trim_start();
    }
}

/// End of the first whitespace-separated token, honouring quotes and
/// backslash-escaped spaces.
fn path_token_end(text: &str) -> usize {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quote.is_none() {
            escaped = true;
        } else if Some(character) == quote {
            quote = None;
        } else if quote.is_none() && (character == '\'' || character == '"') && index == 0 {
            quote = Some(character);
        } else if quote.is_none() && character.is_whitespace() {
            return index;
        }
    }
    text.len()
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
    fn claude_background_work_suppresses_completion_until_a_drained_stop() {
        use crate::status::{Authority, StatusReducer};
        use diri_proto::SessionStatus;
        use std::time::Duration;

        for pending in [
            json!({"background_tasks": [{"status": "running"}]}),
            json!({"session_crons": [{"id": "wake-later"}]}),
        ] {
            let mut reducer = StatusReducer::new(Authority::HooksPrimary, now());
            let (start, _) = parse_claude_hook("UserPromptSubmit", &json!({}), now()).unwrap();
            reducer.reduce(start, now());
            for (seconds, event, payload) in [
                (1, "Stop", pending.clone()),
                (2, "Stop", pending),
                (
                    3,
                    "Notification",
                    json!({"notification_type": "agent_completed"}),
                ),
                (
                    30,
                    "Notification",
                    json!({"notification_type": "idle_prompt"}),
                ),
            ] {
                let at = now() + Duration::from_secs(seconds);
                let (signal, _) = parse_claude_hook(event, &payload, at).unwrap();
                assert!(!reducer.reduce(signal, at).turn_completed);
                assert!(!reducer.reduce(StatusSignal::Tick, at).turn_completed);
                assert_eq!(reducer.status(), &SessionStatus::Working);
            }
            let at = now() + Duration::from_secs(31);
            let (stop, _) = parse_claude_hook(
                "Stop",
                &json!({"background_tasks": [], "session_crons": []}),
                at,
            )
            .unwrap();
            let first = reducer.reduce(stop.clone(), at);
            let settled = reducer.reduce(StatusSignal::Tick, at);
            assert_eq!(
                usize::from(first.turn_completed) + usize::from(settled.turn_completed),
                1
            );
            assert!(!reducer.reduce(stop, at).turn_completed);
        }
    }

    #[test]
    fn claude_pending_work_keeps_permission_alerts_and_ignores_child_stops() {
        use crate::status::{Authority, StatusReducer};
        use diri_proto::SessionStatus;
        let mut reducer = StatusReducer::new(Authority::HooksPrimary, now());
        for (event, payload) in [
            ("UserPromptSubmit", json!({})),
            ("Stop", json!({"background_tasks": [{"status": "running"}]})),
            (
                "Notification",
                json!({"notification_type": "permission_prompt", "message": "Approve this command"}),
            ),
        ] {
            let (signal, _) = parse_claude_hook(event, &payload, now()).unwrap();
            assert!(!reducer.reduce(signal, now()).turn_completed);
        }
        assert_eq!(
            reducer.status(),
            &SessionStatus::NeedsInput(NeedsInputKind::Permission)
        );
        for payload in [
            json!({"agent_id": "child", "background_tasks": []}),
            json!({"hook_event_name": "SubagentStop"}),
        ] {
            if let Some((signal, _)) = parse_claude_hook("Stop", &payload, now()) {
                assert!(!reducer.reduce(signal, now()).turn_completed);
            }
            assert_eq!(
                reducer.status(),
                &SessionStatus::NeedsInput(NeedsInputKind::Permission)
            );
        }
        let (reminder, _) = parse_claude_hook("Notification", &json!({"notification_type": "idle_prompt", "background_tasks": [{"status": "running"}]}), now()).unwrap();
        reducer.reduce(reminder, now());
        assert_eq!(
            reducer.status(),
            &SessionStatus::NeedsInput(NeedsInputKind::Permission)
        );
    }

    #[test]
    fn claude_old_and_drained_payloads_still_complete_once() {
        use crate::status::{Authority, StatusReducer};
        use diri_proto::SessionStatus;
        for payload in [
            json!({}),
            json!({"background_tasks": [], "session_crons": []}),
            json!({"background_tasks": [{"status": "completed"}, {"status": "failed"}, {"status": "cancelled"}]}),
        ] {
            let mut reducer = StatusReducer::new(Authority::HooksPrimary, now());
            let (start, _) = parse_claude_hook("UserPromptSubmit", &json!({}), now()).unwrap();
            reducer.reduce(start, now());
            let (stop, _) = parse_claude_hook("Stop", &payload, now()).unwrap();
            reducer.reduce(stop.clone(), now());
            assert!(reducer.reduce(StatusSignal::Tick, now()).turn_completed);
            assert!(!reducer.reduce(stop, now()).turn_completed);
            assert_eq!(reducer.status(), &SessionStatus::Idle);
        }
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
    fn cursor_conversation_id_is_accepted_as_session_identity() {
        let payload = json!({
            "conversation_id": "11111fcb-7655-4342-8b2f-88068c650200",
            "prompt": "Fix the cursor session title",
        });
        let (_, meta) = parse_claude_hook("UserPromptSubmit", &payload, now()).expect("parsed");
        assert_eq!(
            meta.agent_session_id.as_deref(),
            Some("11111fcb-7655-4342-8b2f-88068c650200")
        );
        assert_eq!(
            meta.first_prompt_title.as_deref(),
            Some("Fix the cursor session title")
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
            "notification_type": "agent_needs_input",
            "message": "Waiting for your answer",
        });
        let (_, meta) = parse_claude_hook("Notification", &payload, now()).expect("parsed");
        let detail = meta.needs_input.expect("detail");
        assert_eq!(detail.kind, NeedsInputKind::Question);
        assert_eq!(detail.summary, "Waiting for your answer");
    }

    #[test]
    fn the_main_agents_cwd_and_edited_files_are_reported() {
        let (_, meta) = parse_claude_hook(
            "PostToolUse",
            &json!({
                "session_id": "s",
                "cwd": "/repo/.claude/worktrees/fix/",
                "tool_name": "Edit",
                "tool_input": { "file_path": "src/lib.rs" },
            }),
            now(),
        )
        .unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/repo/.claude/worktrees/fix"));
        assert_eq!(
            meta.edited_path.as_deref(),
            Some("/repo/.claude/worktrees/fix/src/lib.rs")
        );

        let (_, notebook) = parse_claude_hook(
            "PostToolUse",
            &json!({
                "session_id": "s",
                "cwd": "/repo",
                "tool_name": "NotebookEdit",
                "tool_input": { "notebook_path": "/other/n.ipynb" },
            }),
            now(),
        )
        .unwrap();
        assert_eq!(notebook.edited_path.as_deref(), Some("/other/n.ipynb"));

        // Only an applied edit counts: a read, a pending edit, or a refused
        // one names a file without changing it.
        for (event, tool) in [("PostToolUse", "Read"), ("PreToolUse", "Edit")] {
            let (_, meta) = parse_claude_hook(
                event,
                &json!({
                    "session_id": "s",
                    "cwd": "/repo",
                    "tool_name": tool,
                    "tool_input": { "file_path": "/repo/a.rs" },
                }),
                now(),
            )
            .unwrap();
            assert_eq!(meta.edited_path, None, "{event} {tool}");
            assert_eq!(meta.cwd.as_deref(), Some("/repo"));
        }
    }

    #[test]
    fn a_subagent_edits_but_never_moves_the_session() {
        let (_, meta) = parse_claude_hook(
            "PostToolUse",
            &json!({
                "session_id": "s",
                "agent_id": "child",
                "cwd": "/repo/.claude/worktrees/agent-1",
                "tool_name": "Write",
                "tool_input": { "file_path": "/repo/.claude/worktrees/agent-1/a.rs" },
            }),
            now(),
        )
        .unwrap();
        assert_eq!(meta.cwd, None);
        assert_eq!(
            meta.edited_path.as_deref(),
            Some("/repo/.claude/worktrees/agent-1/a.rs")
        );
    }

    #[test]
    fn relative_or_dotted_paths_are_refused() {
        for path in ["relative/dir", "/repo/../etc", "/repo/./x", "/a\0b"] {
            assert_eq!(workspace_path(path), None, "{path}");
        }
        assert_eq!(workspace_path("/").as_deref(), Some("/"));
        let (_, meta) = parse_claude_hook(
            "PostToolUse",
            &json!({
                "session_id": "s",
                "cwd": "/repo",
                "tool_name": "Edit",
                "tool_input": { "file_path": "../escape.rs" },
            }),
            now(),
        )
        .unwrap();
        assert_eq!(meta.edited_path, None);
    }

    #[test]
    fn codex_notify_reports_its_cwd() {
        let (_, meta) = parse_codex_notify(&json!({
            "type": "agent-turn-complete",
            "thread-id": "t",
            "turn-id": "1",
            "cwd": "/repo-feature",
        }))
        .unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/repo-feature"));
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

    #[test]
    fn a_durable_seed_rehydrates_lifecycle_and_safe_identity() {
        let seed = diri_proto::recovery::HookActivitySeed {
            native_request_id: None,
            native_turn_id: None,
            claude_pending_work: None,
            version: diri_proto::recovery::HookActivitySeed::VERSION,
            kind: "claude-hook".into(),
            event: Some("PermissionRequest".into()),
            occurred_at_ms: 42,
            agent_session_id: Some("conversation".into()),
            transcript_path: Some("/tmp/transcript.jsonl".into()),
            notification_type: None,
            tool_name: Some("Bash".into()),
        };
        let (signal, metadata) = parse_activity_seed(&seed).expect("seed");
        assert!(matches!(
            signal,
            StatusSignal::ClaudeHook {
                hook: ClaudeHook::PermissionRequest { .. },
                ..
            }
        ));
        assert_eq!(metadata.agent_session_id.as_deref(), Some("conversation"));
        let detail = metadata.needs_input.expect("generic permission detail");
        assert_eq!(detail.summary, "wants to run `a command`");
    }

    #[test]
    fn pasted_attachment_paths_do_not_name_the_conversation() {
        assert_eq!(
            title_from_prompt(
                "/var/folders/2y/w5660/T/dirijor-clipboard-BcoRjF.png Can you investigate this?"
            ),
            "Can you investigate this?"
        );
        assert_eq!(
            title_from_prompt("'/Users/me/Desktop/Screen Shot.png' /Users/me/My\\ Shot.png fix it"),
            "fix it"
        );
        assert_eq!(title_from_prompt("/tmp/dirijor-clipboard-a1.png"), "");
        assert_eq!(title_from_prompt("/review the diff"), "/review the diff");
        assert_eq!(
            title_from_prompt("explain /Users/me/project/main.rs"),
            "explain /Users/me/project/main.rs"
        );
    }

    #[test]
    fn a_turn_ending_on_a_question_is_read_as_asking() {
        for (message, question) in [
            (
                "Done.\n\nShould I also update the docs?",
                Some("Should I also update the docs?"),
            ),
            (
                "Tests pass. Want me to open a PR?",
                Some("Want me to open a PR?"),
            ),
            (
                "**Which branch should I use?**\n",
                Some("Which branch should I use?"),
            ),
            ("- red\n- blue\n\n> Red or blue? ", Some("Red or blue?")),
            ("要我继续吗？", Some("要我继续吗？")),
            ("All done. Nothing else to do.", None),
            (
                "Should I go ahead? Each is a one-line change.",
                Some("Should I go ahead?"),
            ),
            (
                "**Which one:** FSL or AGPL? I'll update `LICENSE` once you pick.",
                Some("FSL or AGPL?"),
            ),
            (
                "It was stuck on the \"exit and stop background tasks?\" dialog.",
                None,
            ),
            ("Asked `what changed?` in the panel. It answered.", None),
            ("See https://x.dev/?share=abc and https://y.dev/a?b=1", None),
            ("Run:\n```sh\nls -la # what?\n```", None),
            ("```\nunterminated?", None),
            ("?", None),
            ("", None),
        ] {
            assert_eq!(
                trailing_question(message).as_deref(),
                question,
                "{message:?}"
            );
        }
    }

    #[test]
    fn a_stop_payload_carries_its_question_and_pending_work() {
        let payload = json!({
            "hook_event_name": "Stop",
            "session_id": "s",
            "last_assistant_message": "Started the agent.\n\nShould I wait for it?",
            "background_tasks": [{"id": "a", "type": "subagent", "status": "running"}],
        });
        let (signal, _) = parse_claude_hook("Stop", &payload, now()).expect("parsed");
        let StatusSignal::ClaudeHook {
            hook, pending_work, ..
        } = signal
        else {
            panic!("{signal:?}");
        };
        assert_eq!(
            hook,
            ClaudeHook::StopAsking {
                question: "Should I wait for it?".into()
            }
        );
        assert_eq!(pending_work, Some(true));
        let plain = json!({"hook_event_name": "Stop", "last_assistant_message": "Done."});
        let (signal, _) = parse_claude_hook("Stop", &plain, now()).expect("parsed");
        assert!(matches!(
            signal,
            StatusSignal::ClaudeHook {
                hook: ClaudeHook::Stop,
                ..
            }
        ));
    }
}
