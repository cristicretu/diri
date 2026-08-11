//! The tool surface, as data.
//!
//! Descriptions are written for a model to read, not a human: they say *when*
//! to reach for a tool, because that is what decides whether an agent uses it
//! correctly. Schemas are JSON Schema objects, as MCP requires.
//!
//! Ported from the Swift `DirijorMCPTools`. The spawnable-agent list is filled
//! in from the manifest catalog rather than written out, so a new agent
//! manifest becomes spawnable over MCP with no code change.

use serde_json::{Value, json};

pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

fn tool(name: &str, description: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

/// The tool surface with a generic agent list. Prefer
/// [`tool_definitions_for`] when a manifest catalog is available.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    tool_definitions_for(&[
        "claude".into(),
        "codex".into(),
        "cursor".into(),
        "gemini".into(),
        "shell".into(),
    ])
}

/// The tool surface, with `spawn_agent` constrained to `kinds`.
pub fn tool_definitions_for(kinds: &[String]) -> Vec<ToolDefinition> {
    let kind_enum: Vec<Value> = kinds.iter().map(|kind| json!(kind)).collect();

    vec![
        tool(
            "spawn_agent",
            "Open a NEW session (tab) in diri running an agent or a shell. USE THIS whenever the \
             user asks to open, start, spawn or launch another agent, session, or terminal. \
             Optionally create a fresh git worktree for it and give it an initial prompt.",
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": kind_enum, "description": "Which agent to run." },
                    "cwd": { "type": "string", "description": "Working directory (a repo path when worktree is true)." },
                    "worktree": { "type": "boolean", "description": "Create a fresh git worktree off cwd and run there (local spawns only)." },
                    "prompt": { "type": "string", "description": "Initial prompt to send once the agent is ready." },
                    "name": { "type": "string", "description": "Session title." }
                },
                "required": ["kind", "cwd"]
            }),
        ),
        tool(
            "list_agents",
            "List all active agent sessions with id, kind, title, status, parent, and cwd.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "get_status",
            "Current status of one session: working, idle, needsInput (with detail), or exited, \
             plus title and cwd.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "The session id." }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "send_prompt",
            "Send text to a session as if typed, then submit it. Use for follow-up instructions \
             and for answering a prompt the agent is blocked on.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "text": { "type": "string", "description": "The text to send." },
                    "submit": { "type": "boolean", "description": "Press return afterwards. Defaults to true." }
                },
                "required": ["session_id", "text"]
            }),
        ),
        tool(
            "wait_for_agent",
            "Block until a session reaches a state: done (idle after working), needsInput, or \
             exited. Returns as soon as the state is reached or the timeout elapses.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "until": { "type": "string", "enum": ["done", "needsInput", "exited", "any"], "description": "What to wait for. Defaults to done." },
                    "timeout_seconds": { "type": "number", "description": "Give up after this long. Defaults to 300." }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "read_output",
            "Read a session's recent terminal output. Use after wait_for_agent to see what it did.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "max_bytes": { "type": "number", "description": "How much to read from the end. Defaults to 8000." }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "release_agent",
            "End a session and kill its process tree. The record stays in the list.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "create_worktree",
            "Create a git worktree off a repository, on a new branch, so parallel work does not \
             collide in one checkout.",
            json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Path to the repository." },
                    "branch": { "type": "string", "description": "Branch to create. Generated when omitted." },
                    "base": { "type": "string", "description": "Commit or branch to start from." }
                },
                "required": ["repo"]
            }),
        ),
        tool(
            "list_worktrees",
            "List a repository's worktrees with their paths and branches.",
            json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string" }
                },
                "required": ["repo"]
            }),
        ),
        tool(
            "remove_worktree",
            "Remove a git worktree.",
            json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string" },
                    "worktree": { "type": "string", "description": "Path of the worktree to remove." },
                    "force": { "type": "boolean", "description": "Remove even with uncommitted changes." }
                },
                "required": ["repo", "worktree"]
            }),
        ),
        tool(
            "whoami",
            "Identify the calling session: its id, kind, title, cwd, and parent if it was spawned \
             by another agent.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "list_children",
            "List the sessions this one spawned, with their statuses.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "wait_for_children",
            "Block until every session this one spawned is done, or the timeout elapses.",
            json!({
                "type": "object",
                "properties": {
                    "timeout_seconds": { "type": "number", "description": "Defaults to 600." }
                }
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_name_a_description_and_an_object_schema() {
        for tool in tool_definitions() {
            assert!(!tool.name.is_empty());
            assert!(
                tool.description.len() > 20,
                "{} needs a description a model can act on",
                tool.name
            );
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
        }
    }

    #[test]
    fn required_arguments_are_declared_in_properties() {
        // A required key that is not in properties is a schema a strict client
        // rejects outright.
        for tool in tool_definitions() {
            let Some(required) = tool.input_schema["required"].as_array() else {
                continue;
            };
            let properties = tool.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{} has required keys but no properties", tool.name));
            for key in required {
                let key = key.as_str().expect("a string");
                assert!(
                    properties.contains_key(key),
                    "{} requires {key:?} but does not declare it",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn the_spawnable_list_comes_from_the_catalog() {
        let kinds = vec!["opencode".to_string(), "shell".to_string()];
        let tools = tool_definitions_for(&kinds);
        let spawn = tools
            .iter()
            .find(|tool| tool.name == "spawn_agent")
            .expect("spawn_agent");

        let enumerated: Vec<&str> = spawn.input_schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(enumerated, vec!["opencode", "shell"]);
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<String> = tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate tool names");
    }
}
