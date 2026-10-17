//! The MCP JSON-RPC surface shared by the stdio frontend (`dirijor-mcp`) and
//! the Engine-served Streamable HTTP endpoint. Both transports answer every
//! message through [`handle_message`], so the tool list, schemas, errors, and
//! instructions cannot drift between them.

use serde_json::{Value, json};

use crate::Bridge;

pub trait ToolBackend {
    fn tools(&mut self) -> Result<Value, String>;
    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String>;
}

pub struct DirectBackend {
    pub bridge: Bridge,
}

impl DirectBackend {
    pub fn new() -> Self {
        Self::with_bridge(Bridge::default())
    }

    pub fn with_bridge(bridge: Bridge) -> Self {
        Self { bridge }
    }
}

impl Default for DirectBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolBackend for DirectBackend {
    fn tools(&mut self) -> Result<Value, String> {
        let tools = json!({
            "tools": self
                .bridge
                .tool_definitions()?
                .iter()
                .map(|tool| tool.wire_value())
                .collect::<Vec<_>>()
        });
        Ok(tools)
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        self.bridge.call(name, arguments)
    }
}

pub fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

pub fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into()}})
}

pub fn tool_content(result: Result<Value, String>) -> Value {
    // Tools that relay another MCP server (computer use) hand over its
    // result as is: images stay image blocks.
    if let Ok(Value::Object(object)) = &result
        && let Some(Value::Object(relayed)) = object.get("mcpToolResult")
    {
        let mut relayed = relayed.clone();
        relayed.entry("content").or_insert_with(|| json!([]));
        relayed.entry("isError").or_insert(Value::Bool(false));
        return Value::Object(relayed);
    }
    let (value, is_error) = match result {
        Ok(value) => {
            let is_error = value.get("ok") == Some(&Value::Bool(false));
            (value, is_error)
        }
        Err(message) => (Value::String(message), true),
    };
    let text = value.as_str().map_or_else(
        || serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned()),
        str::to_owned,
    );
    // Typed results for clients that read structuredContent (MCP 2025-06-18);
    // the text block stays for every other client.
    if value.is_object() && !is_error {
        return json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false});
    }
    json!({"content":[{"type":"text","text":text}],"isError":is_error})
}

/// Claude Code keeps only the first 2048 characters of server instructions
/// and silently drops the rest, so these only route: the detailed rules live
/// in Diri's skills (`diri_proto::skills`), which Claude Code loads natively
/// and every other agent reads with `get_skill`.
#[cfg(test)]
const INSTRUCTIONS_LIMIT: usize = 2048;

fn instructions(browser: &str) -> String {
    format!(
        "This session runs INSIDE Diri, a desktop orchestrator for coding agents; these \
         tools control it. Use them proactively, without asking, when the user wants to \
         open/spawn/close an agent, session, tab, or terminal (Claude Code, Codex, Cursor, \
         Gemini, shell), see or message other sessions, or parallelize across worktrees.\n\n\
         Diri skills hold the detailed rules. Read the one that applies before acting: as \
         the Claude Code skill diri:<name>, or with get_skill {{\"name\":\"<name>\"}}.\n\
         - scheduling: anything to run later, at a time, or repeatedly. ALWAYS use \
         diri:scheduling and schedule_agent, never the built-in schedule skill, \
         CronCreate, /loop, reminders, or a sleeping shell; wake_mac:true wakes a \
         sleeping Mac for the run.\n\
         - notes: if whoami shows origin_note, read_note {{\"note\":\"origin\"}} first; it \
         is your brief. Link related notes with [[Title]]; read_note lists backlinks.\n\
         - orchestration: parallel agents in worktrees, tasks you assign or receive, \
         waiting, retries.\n\
         - api: building or debugging an HTTP API or dev server; open_api_request shows \
         the endpoint in your session's API tab.\n\n\
         To spawn an agent use its native kind (e.g. `claude`, `codex`; default your own) \
         with the task as `prompt`. Never use `shell` to launch an agent CLI (`claude`, \
         `codex`, ...): a child `shell` is a raw terminal in the parent's Cmd+J pane whose \
         prompt runs as shell commands.\n\n\
         A Diri task you receive: report_task acknowledged first, then completed/failed for \
         that task_id after verifying (JSON matching result_schema if given), or blocked \
         with your question. Delivery is at most once: reuse message_id/operation_id/\
         request_id on retries, never resend under a new identity.{browser}"
    )
}

pub fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|version| matches!(*version, "2024-11-05" | "2025-03-26" | "2025-06-18"))
        .unwrap_or("2025-06-18");
    let browser = if std::env::var_os("DIRIJOR_TEST_RUN_AVAILABLE").is_some() {
        " To test a web feature, use test_run with a preview URL from get_artifacts."
    } else {
        ""
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools":{}},
        "serverInfo": {"name":"dirijor","version":"0.1.0"},
        "instructions": instructions(browser),
    })
}

pub fn handle_message(message: Value, backend: &mut impl ToolBackend) -> Option<Value> {
    let object = match message.as_object() {
        Some(object) => object,
        None => return Some(error(Value::Null, -32600, "Invalid Request")),
    };
    let id = object.get("id").cloned();
    let response_id = id
        .clone()
        .filter(|id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
        .unwrap_or(Value::Null);
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(error(
            response_id,
            -32600,
            "Invalid Request: method must be a string",
        ));
    };
    if object.get("jsonrpc") != Some(&json!("2.0"))
        || id
            .as_ref()
            .is_some_and(|id| !(id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()))
    {
        return Some(error(response_id, -32600, "Invalid Request"));
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return id.map(|id| error(id, -32602, "params must be an object"));
    }

    match method {
        "initialize" => id.map(|id| success(id, initialize(&params))),
        "ping" => id.map(|id| success(id, json!({}))),
        "tools/list" => id.map(|id| match backend.tools() {
            Ok(tools) => success(id, tools),
            Err(message) => error(id, -32603, message),
        }),
        "tools/call" => id.map(|id| {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return success(
                    id,
                    tool_content(Err("tools/call missing 'name'".to_owned())),
                );
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            success(id, tool_content(backend.call(name, &arguments)))
        }),
        _ if id.is_none() => None,
        _ => Some(error(
            id.unwrap_or(Value::Null),
            -32601,
            format!("Method not found: {method}"),
        )),
    }
}

/// Whether a request only reads Engine state. Reads may overlap and may be
/// cancelled at any time; everything else is a mutation that runs in arrival
/// order and is never cancelled once dispatched.
pub fn read_only(message: &Value) -> bool {
    message["method"] == "tools/list"
        || matches!(
            message["params"]["name"].as_str(),
            Some(
                "list_agents"
                    | "get_task"
                    | "wait_for_task"
                    | "get_status"
                    | "wait_for_agent"
                    | "read_output"
                    | "get_artifacts"
                    | "list_worktrees"
                    | "list_tasks"
                    | "list_schedules"
                    | "get_skill"
                    | "wait_any"
                    | "get_diff"
                    | "whoami"
                    | "list_children"
                    | "wait_for_children"
                    | "summarize_children"
                    | "list_notes"
                    | "read_note"
                    | "note_links"
            )
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_fit_the_client_cap_and_route_to_every_skill() {
        let longest = instructions(
            " To test a web feature, use test_run with a preview URL from get_artifacts.",
        );
        assert!(
            longest.chars().count() <= INSTRUCTIONS_LIMIT,
            "{} chars: Claude Code drops everything past {INSTRUCTIONS_LIMIT}",
            longest.chars().count()
        );
        for skill in diri_proto::skills::ALL {
            assert!(
                longest.contains(&format!("- {}:", skill.name)),
                "instructions must route to {}",
                skill.name
            );
        }
        assert!(longest.find("schedule_agent").unwrap() < 1024);
        assert!(longest.contains("wake_mac"));
        assert!(longest.contains("get_skill"));
    }

    struct Fake;

    impl ToolBackend for Fake {
        fn tools(&mut self) -> Result<Value, String> {
            Ok(json!({"tools":[{"name":"list_agents"}]}))
        }

        fn call(&mut self, name: &str, _: &Value) -> Result<Value, String> {
            (name == "list_agents")
                .then(|| json!({"agents":[]}))
                .ok_or_else(|| "unknown tool".to_owned())
        }
    }

    #[test]
    fn unknown_delivery_is_not_marked_as_a_successful_tool_call() {
        let result = tool_content(Ok(json!({"ok":false, "receipt":{"delivery":"unknown"}})));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn relayed_results_keep_their_images() {
        let result = tool_content(Ok(json!({"mcpToolResult": {
            "content": [{"type":"text","text":"tree"},{"type":"image","data":"AAAA","mimeType":"image/png"}],
            "structuredContent": {"snapshot_id": "s1"}
        }})));
        assert_eq!(result["content"][1]["type"], "image");
        assert_eq!(result["structuredContent"]["snapshot_id"], "s1");
        assert_eq!(result["isError"], false);
        let failed = tool_content(Ok(
            json!({"mcpToolResult": {"content": [], "isError": true}}),
        ));
        assert_eq!(failed["isError"], true);
    }

    #[test]
    fn serves_mcp_through_a_rust_backend() {
        let mut backend = Fake;
        let listed = handle_message(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            &mut backend,
        )
        .unwrap();
        assert_eq!(listed["result"]["tools"][0]["name"], "list_agents");

        let called = handle_message(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_agents","arguments":{}}}),
            &mut backend,
        )
        .unwrap();
        assert_eq!(called["result"]["isError"], false);
        assert_eq!(called["result"]["content"][0]["text"], "{\"agents\":[]}");
    }

    #[test]
    fn instructions_distinguish_agent_sessions_from_shell_panes() {
        let initialized = initialize(&json!({}));
        let instructions = initialized["instructions"].as_str().expect("instructions");

        assert!(instructions.contains("native kind"));
        assert!(instructions.contains("Never use `shell` to launch an agent CLI"));
        assert!(instructions.contains("Cmd+J"));
    }

    #[test]
    fn the_notes_skill_teaches_the_whole_contract() {
        // The contract moved out of the instructions, which Claude Code cut
        // before it ever arrived; the kept prefix routes to the skill.
        let initialized = initialize(&json!({}));
        let routing = initialized["instructions"].as_str().expect("instructions");
        assert!(routing.contains("- notes:") && routing.contains("origin_note"));
        let instructions = diri_proto::skills::NOTES.markdown;
        for step in [
            "read it first with read_note {\"note\":\"origin\"}",
            "a decision, a finding, a blocker, a result, a link",
            "no progress chatter",
            "Prefer adding",
            "Never silently delete the person's writing",
            "edit_note",
            "replace_section",
            "Tick your own sub-tasks",
            "the person reviews your work and ticks it",
            "Finish with a one-paragraph result",
            "create_note",
            "open:true",
        ] {
            assert!(instructions.contains(step), "missing: {step}");
        }
    }
}
