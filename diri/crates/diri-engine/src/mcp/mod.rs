//! The MCP server agents use to orchestrate each other.
//!
//! Every session diri starts gets this wired in, which is how an agent can
//! spawn a second agent, watch it, and read its output. The transport is the
//! stdio one: newline-delimited JSON-RPC 2.0, no `Content-Length` framing.
//!
//! Message handling is pure — [`McpServer::handle`] takes a request value and
//! returns an optional response value — so the whole protocol surface is
//! testable without pipes. Tool execution is behind [`ToolHost`], so the same
//! server runs against a live registry or a fake.
//!
//! Ported from the Swift `DirijorMCP`.

pub mod host;
mod tools;

pub use host::RegistryHost;
pub use tools::{ToolDefinition, tool_definitions, tool_definitions_for};

use serde_json::{Value, json};

pub const SERVER_NAME: &str = "dirijor";
pub const SERVER_VERSION: &str = "0.1.0";
/// The revision we advertise when a client does not pin one.
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-06-18";

/// Executes tool calls. Implemented against the engine in `mcp::host`, and
/// against a stub in tests.
pub trait ToolHost: Send + Sync {
    /// Runs `tool` with `arguments`. An `Err` becomes an MCP error result
    /// rather than a transport-level failure, because a tool that fails is a
    /// normal outcome the agent should read and react to.
    fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String>;
}

pub struct McpServer<H: ToolHost> {
    tools: Vec<ToolDefinition>,
    host: H,
}

impl<H: ToolHost> McpServer<H> {
    pub fn new(tools: Vec<ToolDefinition>, host: H) -> Self {
        Self { tools, host }
    }

    /// Handles one JSON-RPC message.
    ///
    /// Returns `None` for notifications and for anything that is not a request
    /// — a response arriving here is the client's, not ours to answer.
    pub fn handle(&self, message: &Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error_response(&Value::Null, -32600, "Invalid Request"));
        };
        let method = object.get("method").and_then(Value::as_str)?;
        // No id means a notification: act, but never reply.
        let id = object.get("id");
        let params = object.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => reply(id, self.initialize(&params)),
            // Notifications, acknowledged by silence.
            "notifications/initialized" | "initialized" => None,
            "ping" => reply(id, json!({})),
            "tools/list" => reply(id, self.tools_list()),
            "tools/call" => reply(id, self.tools_call(&params)),
            unknown => {
                let id = id?;
                Some(error_response(
                    id,
                    -32601,
                    &format!("Method not found: {unknown}"),
                ))
            }
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        // Echo the client's protocol revision when it names one. Answering with
        // our own preferred version instead makes strict clients disconnect.
        let version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PREFERRED_PROTOCOL_VERSION);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })
    }

    fn tools_list(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })
            })
            .collect();
        json!({ "tools": tools })
    }

    fn tools_call(&self, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return tool_error("tools/call requires a tool name");
        };
        if !self.tools.iter().any(|tool| tool.name == name) {
            return tool_error(&format!("unknown tool {name:?}"));
        }
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        match self.host.call(name, &arguments) {
            Ok(value) => json!({
                "content": [{ "type": "text", "text": render(&value) }],
                "isError": false,
            }),
            // A failing tool is a result the agent reads, not a broken channel.
            Err(message) => tool_error(&message),
        }
    }
}

/// Tool results are rendered as text because that is what an MCP client shows
/// the model. Strings pass through unquoted; anything else is pretty JSON.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

fn reply(id: Option<&Value>, result: Value) -> Option<Value> {
    let id = id?;
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingHost {
        calls: Mutex<Vec<(String, Value)>>,
        fail_with: Option<String>,
    }

    impl ToolHost for RecordingHost {
        fn call(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
            self.calls
                .lock()
                .expect("calls")
                .push((tool.to_string(), arguments.clone()));
            match &self.fail_with {
                Some(message) => Err(message.clone()),
                None => Ok(json!({ "ok": true, "tool": tool })),
            }
        }
    }

    fn server() -> McpServer<RecordingHost> {
        McpServer::new(tool_definitions(), RecordingHost::default())
    }

    fn request(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    }

    #[test]
    fn initialize_echoes_the_clients_protocol_revision() {
        // Answering with our own preferred version instead of the client's is
        // what makes strict clients hang up.
        let response = server()
            .handle(&request(
                "initialize",
                json!({ "protocolVersion": "2025-03-26" }),
            ))
            .expect("a reply");
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(response["result"]["serverInfo"]["name"], "dirijor");
    }

    #[test]
    fn initialize_falls_back_to_the_preferred_revision() {
        let response = server()
            .handle(&request("initialize", json!({})))
            .expect("a reply");
        assert_eq!(
            response["result"]["protocolVersion"],
            PREFERRED_PROTOCOL_VERSION
        );
    }

    #[test]
    fn notifications_get_no_reply() {
        let server = server();
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(server.handle(&notification).is_none());

        // A request without an id is also a notification, even for a real method.
        let no_id = json!({ "jsonrpc": "2.0", "method": "ping" });
        assert!(server.handle(&no_id).is_none());
    }

    #[test]
    fn tools_list_advertises_the_whole_surface() {
        let response = server()
            .handle(&request("tools/list", json!({})))
            .expect("a reply");
        let tools = response["result"]["tools"].as_array().expect("array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();

        for expected in [
            "spawn_agent",
            "list_agents",
            "get_status",
            "send_prompt",
            "read_output",
            "release_agent",
            "create_worktree",
            "whoami",
            "search_workspace",
            "read_source",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
        for tool in tools {
            assert_eq!(
                tool["inputSchema"]["type"], "object",
                "{} has no object schema",
                tool["name"]
            );
        }
    }

    #[test]
    fn a_tool_call_reaches_the_host_and_comes_back_as_text() {
        let server = server();
        let response = server
            .handle(&request(
                "tools/call",
                json!({ "name": "get_status", "arguments": { "session_id": "s_1" } }),
            ))
            .expect("a reply");

        assert_eq!(response["result"]["isError"], false);
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        assert!(text.contains("get_status"), "got {text}");

        let calls = server.host.calls.lock().expect("calls");
        assert_eq!(calls[0].0, "get_status");
        assert_eq!(calls[0].1["session_id"], "s_1");
    }

    #[test]
    fn a_failing_tool_is_an_error_result_not_a_broken_channel() {
        // The agent should read the failure and react, not lose the connection.
        let host = RecordingHost {
            fail_with: Some("no session s_missing".into()),
            ..Default::default()
        };
        let server = McpServer::new(tool_definitions(), host);
        let response = server
            .handle(&request(
                "tools/call",
                json!({ "name": "get_status", "arguments": { "session_id": "s_missing" } }),
            ))
            .expect("a reply");

        assert_eq!(response["result"]["isError"], true);
        assert!(response["error"].is_null(), "not a transport error");
        assert_eq!(
            response["result"]["content"][0]["text"],
            "no session s_missing"
        );
    }

    #[test]
    fn an_unknown_tool_is_rejected_before_the_host_sees_it() {
        let server = server();
        let response = server
            .handle(&request(
                "tools/call",
                json!({ "name": "rm_rf_everything", "arguments": {} }),
            ))
            .expect("a reply");

        assert_eq!(response["result"]["isError"], true);
        assert!(
            server.host.calls.lock().expect("calls").is_empty(),
            "the host must not be asked to run tools that were never advertised"
        );
    }

    #[test]
    fn an_unknown_method_is_a_json_rpc_error() {
        let response = server()
            .handle(&request("tools/subscribe", json!({})))
            .expect("a reply");
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn a_non_object_message_is_an_invalid_request() {
        let response = server().handle(&json!("hello")).expect("a reply");
        assert_eq!(response["error"]["code"], -32600);
    }

    #[test]
    fn a_response_from_the_client_is_ignored() {
        // Responses have no `method`; answering one would loop.
        let response = server().handle(&json!({ "jsonrpc": "2.0", "id": 7, "result": {} }));
        assert!(response.is_none());
    }
}
