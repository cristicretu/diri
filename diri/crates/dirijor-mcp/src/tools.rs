//! The MCP tool catalog shared by the Rust stdio frontend and CLI.

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    fn new(name: &str, description: &str, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    pub fn wire_value(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

pub fn tool_definitions_for(kinds: &[String]) -> Vec<ToolDefinition> {
    let kind_enum: Vec<Value> = kinds.iter().map(|kind| json!(kind)).collect();
    let mut tools = vec![
        ToolDefinition::new(
            "spawn_agent",
            "Open a new Diri session running an agent or shell, locally or on a configured remote host. Use this whenever the user asks to spawn another agent, session, or terminal.",
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": kind_enum},
                    "cwd": {"type": "string"},
                    "host": {"type": "string"},
                    "worktree": {"type": "boolean"},
                    "branch": {"type": "string"},
                    "base": {"type": "string", "description": "Starting ref for a new worktree, e.g. main. Omitted preserves HEAD behavior."},
                    "prompt": {"type": "string"},
                    "name": {"type": "string"}
                },
                "required": ["kind", "cwd"]
            }),
        ),
        ToolDefinition::new(
            "list_agents",
            "List every agent session with its id, kind, title, status, parent, host, and working directory.",
            json!({"type": "object", "properties": {}}),
        ),
        ToolDefinition::new(
            "get_status",
            "Read the current status, title, and working directory of one session.",
            session_id_schema(),
        ),
        ToolDefinition::new(
            "send_prompt",
            "Type into an authorized session and optionally press Enter. Delegated agents may message their parent or direct children; root agents may coordinate their project and message direct children on any host. Cross-lineage messages are attributed to their sender. Identical messages from the same sender to the same target are delivered at most once, including across retries and restarts. Reuse message_id on retries; use a new message_id only to intentionally repeat identical text. A receipt acknowledges input delivery, not agent completion. Inspect an unknown outcome; never resend it under a new identity.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "text": {"type": "string"},
                    "message_id": message_id_schema(),
                    "submit": {"type": "boolean", "description": "Press Enter after typing; defaults to true."}
                },
                "required": ["session_id", "text"]
            }),
        ),
        ToolDefinition::new(
            "wait_for_agent",
            "Wait for a session status without model polling. Already matching states return immediately; this does not acknowledge completion of a particular message. Exit or removal also ends the wait; inspect matched, removed, and session before assuming success.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "until": {"type": "string", "enum": ["done", "needs_me", "idle", "exited"]},
                    "timeout_s": {"type": "number", "default": 600, "minimum": 0, "maximum": 600}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "read_output",
            "Read the current rendered screen of an agent session.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "mode": {"type": "string", "enum": ["screen", "tail"]},
                    "lines": {"type": "number", "default": 50}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::new(
            "get_artifacts",
            "Return PRs, issues, preview URLs, and listening ports discovered for a session.",
            session_id_schema(),
        ),
        ToolDefinition::new(
            "create_worktree",
            "Create a git worktree in the calling session's project so parallel work does not collide in one checkout.",
            json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "branch": {"type": "string"},
                    "base": {"type": "string"}
                },
                "required": ["repo"]
            }),
        ),
        ToolDefinition::new(
            "list_worktrees",
            "List a repository's worktrees with their paths and branches.",
            json!({"type": "object", "properties": {"repo": {"type": "string"}}, "required": ["repo"]}),
        ),
        ToolDefinition::new(
            "remove_worktree",
            "Remove a git worktree from the calling session's project.",
            json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "path": {"type": "string"},
                    "force": {"type": "boolean"}
                },
                "required": ["repo", "path"]
            }),
        ),
        ToolDefinition::new(
            "release_agent",
            "Terminate an authorized agent session. Delegated agents may release direct children; root agents may release sessions in their project. The caller and its ancestors are protected.",
            session_id_schema(),
        ),
        ToolDefinition::new(
            "test_run",
            "Run a known web flow across real browser engines and return pass/fail evidence. Use browser instead for open-ended exploration.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "engines": {"type": "array", "items": {"type": "string", "enum": ["chromium", "webkit", "firefox"]}},
                    "steps": {"type": "array", "items": {"type": "object"}},
                    "observe": {"type": "string", "enum": ["a11y", "screenshot"]},
                    "baseline": {"type": "string"},
                    "profile": {"type": "string"},
                    "auth": {"type": "object"}
                },
                "required": ["url", "steps"]
            }),
        ),
        ToolDefinition::new(
            "browser",
            "Drive a real browser isolated to this Diri session. Open a URL, inspect snapshot refs, act on those refs, and request a new snapshot after page changes.",
            browser_schema(),
        ),
        ToolDefinition::new(
            "whoami",
            "Describe this session's identity, parent, ancestors, children, worktree, and cross-session write policy.",
            json!({"type": "object", "properties": {}}),
        ),
        ToolDefinition::new(
            "list_children",
            "List the sessions spawned by this one, optionally including the whole descendant tree.",
            json!({
                "type": "object",
                "properties": {
                    "recursive": {"type": "boolean"},
                    "include_exited": {"type": "boolean", "default": true}
                }
            }),
        ),
        ToolDefinition::new(
            "wait_for_children",
            "Wait until selected child sessions settle, finish, or exit. Already matching states return immediately. Removed children are reported separately and cannot settle other working children. Omit session_ids for all direct children; an explicit empty array selects none.",
            json!({
                "type": "object",
                "properties": {
                    "session_ids": {"type": "array", "items": {"type": "string"}},
                    "until": {"type": "string", "enum": ["settled", "done", "exited"]},
                    "timeout_s": {"type": "number", "default": 600, "minimum": 0, "maximum": 600}
                }
            }),
        ),
        ToolDefinition::new(
            "summarize_children",
            "Collect compact screen tails, status, and artifacts for this session's children without interpreting their output.",
            json!({
                "type": "object",
                "properties": {
                    "session_ids": {"type": "array", "items": {"type": "string"}},
                    "rows": {"type": "number", "default": 14}
                }
            }),
        ),
        ToolDefinition::new(
            "report_to_parent",
            "Deliver a structured update, result, blocker, or question to the session that delegated this work at most once. Identical reports are deduplicated. Reuse message_id on retries; choose a new one only for an intentional repeat. Inspect unknown outcomes without resending.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
                    "message_id": message_id_schema(),
                    "status": {"type": "string", "enum": ["update", "done", "blocked", "failed"]},
                    "details": {"type": "string"},
                    "blockers": string_array(),
                    "questions": string_array(),
                    "next_steps": string_array(),
                    "changed_paths": string_array(),
                    "artifacts": string_array(),
                    "proof": string_array(),
                    "submit": {"type": "boolean"}
                },
                "required": ["summary"]
            }),
        ),
    ];

    if std::env::var_os("DIRIJOR_TEST_RUN_AVAILABLE").is_none() {
        tools.retain(|tool| tool.name != "test_run");
    }
    for tool in &mut tools {
        tool.input_schema["additionalProperties"] = json!(false);
        for key in [
            "kind",
            "cwd",
            "host",
            "branch",
            "base",
            "name",
            "repo",
            "path",
            "session_id",
        ] {
            if let Some(field) = tool.input_schema["properties"].get_mut(key) {
                field["minLength"] = json!(1);
            }
        }
        if let Some(field) = tool.input_schema["properties"].get_mut("session_ids") {
            field["items"]["minLength"] = json!(1);
        }
    }
    tools
}

/// Validate the advertised argument contract before discovery, authorization,
/// or any Engine call. Wrong optional types must never silently select defaults
/// (especially submit, force, host, or the selection of child sessions).
pub(crate) fn validate_arguments(tool: &str, arguments: &Value) -> Result<(), String> {
    let mut definition = tool_definitions_for(&[])
        .into_iter()
        .find(|definition| definition.name == tool)
        .ok_or_else(|| format!("unknown or unavailable tool: {tool}"))?;
    // Kind aliases/custom commands are resolved against the live catalog by
    // spawn; the static validator must not use an empty discovery enum.
    if tool == "spawn_agent" {
        definition.input_schema["properties"]["kind"]
            .as_object_mut()
            .unwrap()
            .remove("enum");
    }
    validate_value(arguments, &definition.input_schema, "arguments")
}

fn validate_value(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let expected = schema["type"].as_str().unwrap_or("any");
    let valid = match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        _ => true,
    };
    if !valid {
        return Err(format!("{path} must be {expected}"));
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not a supported value"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("missing required argument: {key}"));
                }
            }
        }
        for (key, field) in object {
            if let Some(field_schema) = schema["properties"].get(key) {
                validate_value(field, field_schema, &format!("{path}.{key}"))?;
            } else if schema["additionalProperties"] == false {
                return Err(format!("unsupported argument: {key}"));
            }
        }
    }
    if let Some(entries) = value.as_array() {
        for (index, entry) in entries.iter().enumerate() {
            validate_value(entry, &schema["items"], &format!("{path}[{index}]"))?;
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if schema["minLength"].as_u64().is_some_and(|min| length < min)
            || schema["maxLength"].as_u64().is_some_and(|max| length > max)
        {
            return Err(format!("{path} has an invalid length"));
        }
    }
    if let Some(number) = value.as_f64()
        && (schema["minimum"].as_f64().is_some_and(|min| number < min)
            || schema["maximum"].as_f64().is_some_and(|max| number > max))
    {
        return Err(format!("{path} is outside the supported range"));
    }
    Ok(())
}

fn session_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"session_id": {"type": "string"}},
        "required": ["session_id"]
    })
}

fn string_array() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}

fn browser_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["open", "snapshot", "click", "fill", "type", "press", "hover", "select", "check", "scroll", "get", "wait", "screenshot", "console", "back", "close", "list"]},
            "url": {"type": "string"},
            "ref": {"type": "string"},
            "selector": {"type": "string"},
            "text": {"type": "string"},
            "key": {"type": "string"},
            "value": {"type": "string"},
            "what": {"type": "string", "enum": ["url", "title", "text", "html", "value", "count"]},
            "ms": {"type": "number"},
            "state": {"type": "string"},
            "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
            "amount": {"type": "number"},
            "button": {"type": "string", "enum": ["left", "right", "middle"]},
            "double": {"type": "boolean"},
            "full": {"type": "boolean"},
            "annotate": {"type": "boolean"},
            "engine": {"type": "string", "enum": ["chromium", "webkit", "firefox"]},
            "profile": {"type": "string"}
        },
        "required": ["action"]
    })
}

fn message_id_schema() -> Value {
    json!({"type":"string", "minLength":1, "maxLength":200,
        "description":"Stable identity for this logical message. Reuse on retries. If omitted, identical content is deduplicated for this sender/target. Use a new value only for an intentional repeat."})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_valid_and_names_are_unique() {
        let mut names = Vec::new();
        for tool in tool_definitions_for(&["codex".into(), "shell".into()]) {
            assert!(!tool.name.is_empty());
            assert!(tool.description.len() > 20, "{}", tool.name);
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            if let Some(required) = tool.input_schema.get("required").and_then(Value::as_array) {
                let properties = tool.input_schema["properties"].as_object().unwrap();
                for key in required {
                    assert!(
                        properties.contains_key(key.as_str().unwrap()),
                        "{}",
                        tool.name
                    );
                }
            }
            names.push(tool.name);
        }
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total);
    }

    #[test]
    fn spawn_agents_come_from_the_runtime_catalog() {
        let tools = tool_definitions_for(&["opencode".into(), "shell".into()]);
        let spawn = tools
            .iter()
            .find(|tool| tool.name == "spawn_agent")
            .unwrap();
        assert_eq!(
            spawn.input_schema["properties"]["kind"]["enum"],
            json!(["opencode", "shell"])
        );
    }
}
