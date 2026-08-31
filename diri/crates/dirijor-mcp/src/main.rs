use std::io::{self, BufRead, Write};

use dirijor_mcp::Bridge;
use serde_json::{Value, json};

trait ToolBackend {
    fn tools(&mut self) -> Result<Value, String>;
    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String>;
}

struct DirectBackend {
    bridge: Bridge,
    cached_tools: Option<Value>,
}

impl DirectBackend {
    fn new() -> Self {
        Self {
            bridge: Bridge::default(),
            cached_tools: None,
        }
    }
}

impl ToolBackend for DirectBackend {
    fn tools(&mut self) -> Result<Value, String> {
        if let Some(tools) = &self.cached_tools {
            return Ok(tools.clone());
        }
        let tools = json!({
            "tools": self
                .bridge
                .tool_definitions()?
                .iter()
                .map(|tool| tool.wire_value())
                .collect::<Vec<_>>()
        });
        self.cached_tools = Some(tools.clone());
        Ok(tools)
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        self.bridge.call(name, arguments)
    }
}

fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into()}})
}

fn tool_content(result: Result<Value, String>) -> Value {
    let (value, is_error) = match result {
        Ok(value) => (value, false),
        Err(message) => (Value::String(message), true),
    };
    let text = value.as_str().map_or_else(
        || serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned()),
        str::to_owned,
    );
    json!({"content":[{"type":"text","text":text}],"isError":is_error})
}

fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
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
        "instructions": format!(
            "This session is running INSIDE Diri, a desktop orchestrator for coding agents. \
             These tools control it. Use them proactively whenever the user asks to \
             open/start/spawn/close another agent, session, tab, or terminal (Claude Code, \
             Codex, Cursor, Gemini, or a shell), to check what other sessions are doing, to \
             talk to another session, or to parallelize work across git worktrees — no \
             extra confirmation of intent needed.\n\nAgent vs terminal rule: when asked to spawn \
             another agent, select that agent's native kind (for example `claude` or `codex`) \
             and pass its task as `prompt`. If no agent is named, use your own native kind when \
             it is available. Never use `shell` to launch an agent CLI such as `claude`, \
             `codex`, `cursor`, or `gemini`. A child `shell` is an interactive terminal shown \
             in the parent's Cmd+J pane, and its prompt is executed as shell commands; use it \
             only when the user explicitly wants a terminal or raw commands.\n\nTypical orchestration flow: spawn_agent \
             (optionally worktree:true and an initial prompt) → wait_for_agent(until:\"done\") \
             → read_output → send_prompt for follow-ups → release_agent when finished. \
             get_artifacts returns PR/Linear/preview URLs and listening ports a session has \
             produced; PR entries include live GitHub status (state, review decision, checks, \
             comment counts, +/- lines).{browser}"
        )
    })
}

fn handle_message(message: Value, backend: &mut impl ToolBackend) -> Option<Value> {
    let object = match message.as_object() {
        Some(object) => object,
        None => return Some(error(Value::Null, -32600, "Invalid Request")),
    };
    let method = object.get("method")?.as_str()?;
    let id = object.get("id").cloned();
    let params = object.get("params").cloned().unwrap_or(Value::Null);

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

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut backend = DirectBackend::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_message(message, &mut backend),
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response
            && (serde_json::to_writer(&mut stdout, &response).is_err()
                || stdout.write_all(b"\n").is_err()
                || stdout.flush().is_err())
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
