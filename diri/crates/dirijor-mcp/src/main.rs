use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

trait ToolBackend {
    fn tools(&mut self) -> Result<Value, String>;
    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String>;
}

struct ProcessBackend {
    cli: PathBuf,
    cached_tools: Option<Value>,
}

impl ProcessBackend {
    fn discover() -> Self {
        let cli = env::var_os("DIRIJOR_CLI")
            .map(PathBuf::from)
            .filter(|path| is_executable(path))
            .or_else(|| {
                env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().map(|parent| parent.join("dirijor")))
                    .filter(|path| is_executable(path))
            })
            .or_else(|| {
                env::var_os("HOME").and_then(|home| {
                    let path =
                        PathBuf::from(home).join("Library/Application Support/Dirijor/bin/dirijor");
                    is_executable(&path).then_some(path)
                })
            })
            .unwrap_or_else(|| PathBuf::from("dirijor"));
        Self {
            cli,
            cached_tools: None,
        }
    }

    fn invoke(&self, args: &[&str], input: Option<&Value>) -> Result<Value, String> {
        let mut child = Command::new(&self.cli)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not launch {}: {error}", self.cli.display()))?;
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or("tool backend stdin was unavailable")?;
            serde_json::to_writer(&mut stdin, input).map_err(|error| error.to_string())?;
            stdin.write_all(b"\n").map_err(|error| error.to_string())?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "invalid response from {}: {error}: {}",
                self.cli.display(),
                String::from_utf8_lossy(&output.stdout).trim()
            )
        })
    }
}

impl ToolBackend for ProcessBackend {
    fn tools(&mut self) -> Result<Value, String> {
        if let Some(tools) = &self.cached_tools {
            return Ok(tools.clone());
        }
        let tools = self.invoke(&["mcp-tools"], None)?;
        self.cached_tools = Some(tools.clone());
        Ok(tools)
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        let envelope = self.invoke(&["mcp-call", "--tool", name], Some(arguments))?;
        if let Some(error) = envelope.get("error").and_then(Value::as_str) {
            Err(error.to_owned())
        } else {
            envelope
                .get("ok")
                .cloned()
                .ok_or_else(|| "tool backend omitted both 'ok' and 'error'".to_owned())
        }
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
    let browser = if env::var_os("DIRIJOR_TEST_RUN_AVAILABLE").is_some() {
        " To test a web feature, use test_run with a preview URL from get_artifacts."
    } else {
        ""
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools":{}},
        "serverInfo": {"name":"dirijor","version":"0.1.0"},
        "instructions": format!(
            "This session is running INSIDE Dirijor, a macOS orchestrator for coding agents. \
             These tools control it. Use them proactively whenever the user asks to \
             open/start/spawn/close another agent, session, tab, or terminal (Claude Code, \
             Codex, Cursor, Gemini, or a shell), to check what other sessions are doing, to \
             talk to another session, or to parallelize work across git worktrees — no \
             extra confirmation of intent needed.\n\nTypical orchestration flow: spawn_agent \
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

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut backend = ProcessBackend::discover();
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
    fn serves_mcp_without_a_resident_swift_runtime() {
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
}
