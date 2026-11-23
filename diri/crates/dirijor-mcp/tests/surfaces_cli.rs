//! Live Surfaces end to end: the `dirijor surface` CLI against a private
//! surfaces directory, and the MCP `surface` action through the compact
//! (`get_skill` + `act`) profile Claude Code and Codex use, so surfaces add
//! no tool schemas to their prompts.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dirijor_mcp::Bridge;
use dirijor_mcp::protocol::{ToolBackend, expand_action, handle_profile};
use serde_json::{Value, json};

fn dirijor(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dirijor"));
    command
        .args(args)
        .env("DIRI_SURFACES_DIR", dir)
        .env_remove("DIRIJOR_SESSION_ID")
        .env("DIRIJOR_SOCKET", dir.join("no-engine.sock"))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("run dirijor");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn json_out(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "dirijor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json output")
}

#[test]
fn cli_builds_updates_and_promotes_a_surface() {
    let dir = tempfile::tempdir().unwrap();
    let surfaces = dir.path().join("surfaces");

    let created = json_out(&dirijor(
        &surfaces,
        &["surface", "create", "Release readiness", "--session", "s-1"],
        Some(
            r#"[{"type":"checklist","id":"gates","items":[{"id":"ci","text":"CI green"},{"id":"notes","text":"Notes written"}]}]"#,
        ),
    ));
    // `--root` was not given, so stdin is ignored: an empty surface.
    let id = created["surface"].as_str().unwrap().to_owned();
    assert_eq!(created["scope"]["kind"], "session");

    let updated = json_out(&dirijor(
        &surfaces,
        &["surface", "update", &id, "--ops", "-", "--expect", "1"],
        Some(
            r#"[{"op":"insert","node":{"type":"checklist","id":"gates","items":[{"id":"ci","text":"CI green","checked":true},{"id":"notes","text":"Notes written"}]}},{"op":"insert","node":{"type":"link","url":"https://github.com/acme/web/pull/9"}}]"#,
        ),
    ));
    assert_eq!(updated["revision"], 2);

    let outline = dirijor(
        &surfaces,
        &["surface", "get", "readiness", "--outline"],
        None,
    );
    let text = String::from_utf8_lossy(&outline.stdout);
    assert!(text.contains("- [x] CI green (ci)"), "{text}");
    assert!(text.contains("https://github.com/acme/web/pull/9"));

    let refused = dirijor(
        &surfaces,
        &["surface", "update", &id, "--ops", "-"],
        Some(r#"[{"op":"insert","node":{"type":"link","url":"javascript:alert(1)"}}]"#),
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("invalid_url"));

    let history = json_out(&dirijor(&surfaces, &["surface", "history", &id], None));
    assert_eq!(history["versions"].as_array().unwrap().len(), 2);

    let promoted = json_out(&dirijor(&surfaces, &["surface", "promote", &id], None));
    let saved = promoted["surface"].as_str().unwrap();
    let list =
        String::from_utf8_lossy(&dirijor(&surfaces, &["surface", "list", "--saved"], None).stdout)
            .into_owned();
    assert!(list.contains(saved) && list.contains("global"), "{list}");

    json_out(&dirijor(&surfaces, &["surface", "delete", &id], None));
    let after = json_out(&dirijor(&surfaces, &["surface", "list", "--json"], None));
    assert_eq!(after["surfaces"].as_array().unwrap().len(), 1);
}

#[test]
fn cli_writes_a_lua_app_and_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let surfaces = dir.path().join("surfaces");
    let created = json_out(&dirijor(
        &surfaces,
        &[
            "surface",
            "create",
            "Tally",
            "--global",
            "--icon",
            "chart.bar",
            "--accent",
            "#16a34a",
            "--lua",
            "-",
        ],
        Some("function render(s, d) return ui.text('n=' .. ((d or {}).n or 0)) end"),
    ));
    assert_eq!(created["check"]["ok"], true, "{created}");
    let id = created["surface"].as_str().unwrap().to_owned();
    json_out(&dirijor(
        &surfaces,
        &["surface", "data", &id, "-"],
        Some(r#"{"n": 4}"#),
    ));
    let data = json_out(&dirijor(&surfaces, &["surface", "data", &id], None));
    assert_eq!(data["data"]["n"], 4);
    let written = json_out(&dirijor(
        &surfaces,
        &["surface", "write", &id, "app.lua", "-"],
        Some("function render() return ui.row{ ui.text('a'), ui.text('b') } end"),
    ));
    assert_eq!(written["check"]["nodes"], 3);
    let schema = dirijor(&surfaces, &["surface", "schema"], None);
    let guide = String::from_utf8_lossy(&schema.stdout);
    assert!(guide.contains("## app.lua") && guide.contains("ui.sparkline"));
}

/// Tools come from the static catalog: no Engine is running here.
struct Backend(Bridge);

impl ToolBackend for Backend {
    fn tools(&mut self) -> Result<Value, String> {
        Ok(
            json!({"tools": dirijor_mcp::tools::tool_definitions_for(&[])
            .iter()
            .map(|tool| tool.wire_value())
            .collect::<Vec<_>>()}),
        )
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, String> {
        self.0.call(name, arguments)
    }
}

fn rpc(backend: &mut Backend, id: u64, method: &str, params: Value) -> Value {
    let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    expand_action(&mut message);
    handle_profile(message, backend, true).expect("reply")
}

fn content(reply: &Value) -> Value {
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    serde_json::from_str(text).unwrap_or(Value::String(text.to_owned()))
}

#[test]
fn compact_profile_serves_surfaces_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let notes = PathBuf::from(dir.path()).join("notes");
    let mut backend = Backend(
        Bridge::new(dir.path().join("none.sock"), Some("s-agent".into())).with_notes_dir(notes),
    );

    // The prompt-visible catalog stays two tools: surfaces add nothing.
    let listed = rpc(&mut backend, 1, "tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["get_skill", "act"]);

    // Discoverable by name, schema on demand.
    let catalog = content(&rpc(
        &mut backend,
        2,
        "tools/call",
        json!({"name": "get_skill", "arguments": {}}),
    ));
    assert!(
        catalog["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "surface")
    );
    let schema = content(&rpc(
        &mut backend,
        3,
        "tools/call",
        json!({"name": "get_skill", "arguments": {"action": "surface"}}),
    ));
    assert_eq!(schema["inputSchema"]["required"], json!(["op"]));

    let created = content(&rpc(
        &mut backend,
        4,
        "tools/call",
        json!({"name": "act", "arguments": {"action": "surface", "arguments": {"op": "create", "title": "Board",
            "root": [{"type": "metric", "id": "m-review", "label": "Open reviews", "value": "4"}]}}}),
    ));
    assert_eq!(created["scope"]["session"], "s-agent");
    let id = created["surface"].as_str().unwrap();
    let patched = content(&rpc(
        &mut backend,
        5,
        "tools/call",
        json!({"name": "act", "arguments": {"action": "surface", "arguments": {
            "op": "update", "surface": id,
            "ops": [{"op": "set", "id": "m-review", "fields": {"value": "2"}}]
        }}}),
    ));
    assert_eq!(patched["revision"], 2);
    let got = content(&rpc(
        &mut backend,
        6,
        "tools/call",
        json!({"name": "act", "arguments": {"action": "surface", "arguments": {"op": "get", "surface": id}}}),
    ));
    assert_eq!(got["surface"]["owner"], "s-agent");
}
