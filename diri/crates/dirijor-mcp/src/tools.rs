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
            "Type into another session and optionally press Enter. Messages outside the direct parent-child channel are attributed to their sender.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "text": {"type": "string"},
                    "submit": {"type": "boolean", "description": "Press Enter after typing; defaults to true."}
                },
                "required": ["session_id", "text"]
            }),
        ),
        ToolDefinition::new(
            "wait_for_agent",
            "Wait for one session to finish a turn, need input, become idle, or exit without polling it from the model.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "until": {"type": "string", "enum": ["done", "needs_me", "idle", "exited"]},
                    "timeout_s": {"type": "number", "default": 600}
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
            "Create a git worktree so parallel work does not collide in one checkout.",
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
            "Remove a git worktree from a repository.",
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
            "Terminate an agent session. The caller, its parent, and its ancestors are protected from accidental release.",
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
            "Wait until this session's selected child sessions settle, finish, or exit, then return all final statuses together.",
            json!({
                "type": "object",
                "properties": {
                    "session_ids": {"type": "array", "items": {"type": "string"}},
                    "until": {"type": "string", "enum": ["settled", "done", "exited"]},
                    "timeout_s": {"type": "number", "default": 600}
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
            "Deliver a structured update, result, blocker, or question to the session that delegated this work.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
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
    tools
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
