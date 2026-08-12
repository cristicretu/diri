//! MCP tools against a live registry.
//!
//! Proves the orchestration surface an agent actually uses: list what is
//! running, read its output, send it input, wait for it, release it. Sessions
//! here are short-lived children of the test process.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diri_engine::detect::ManifestEngine;
use diri_engine::mcp::{McpServer, RegistryHost, tool_definitions};
use diri_engine::pty::PtySpec;
use diri_engine::registry::Registry;
use diri_engine::session::SessionSpec;
use diri_engine::status::Authority;
use diri_proto::{
    AgentKind, DateMillis, ProjectId, Resumability, SessionId, SessionRecord, SessionStatus,
    TitleSource,
};
use serde_json::{Value, json};

fn manifest_dir() -> PathBuf {
    diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests")
}

fn engine() -> Arc<ManifestEngine> {
    let (engine, _) = ManifestEngine::load_dir(&manifest_dir()).expect("load");
    Arc::new(engine)
}

fn record(id: &str, parent: Option<&str>) -> SessionRecord {
    SessionRecord {
        id: SessionId(id.into()),
        kind: AgentKind::SHELL,
        cwd: "/tmp".into(),
        project_id: ProjectId("p".into()),
        worktree_path: None,
        git_branch: None,
        title: format!("test {id}"),
        title_source: TitleSource::Placeholder,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Starting,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        parent: parent.map(|value| SessionId(value.into())),
        created_at: DateMillis(0.0),
        updated_at: DateMillis(0.0),
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: None,
        remote_persistence: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

fn spec(id: &str, script: &str, logs: &Path) -> SessionSpec {
    SessionSpec {
        id: id.into(),
        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], "/tmp")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .size(80, 24),
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: logs.to_path_buf(),
        holder: None,
        remote: None,
        defer_launch: false,
    }
}

/// Calls a tool through the full MCP server and returns the parsed result.
fn call(server: &McpServer<RegistryHost>, tool: &str, arguments: Value) -> Result<Value, String> {
    let response = server
        .handle(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }))
        .expect("a reply");

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_string();
    if response["result"]["isError"] == json!(true) {
        return Err(text);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

#[test]
fn an_agent_can_list_read_and_release_another_session() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    registry
        .spawn(
            spec("s_worker", "printf 'work-output\\n'; sleep 30", &logs),
            record("s_worker", None),
        )
        .expect("spawn");
    let registry = Arc::new(Mutex::new(registry));

    let host = RegistryHost::new(Arc::clone(&registry), &logs).with_caller(None);
    let server = McpServer::new(tool_definitions(), host);

    // list_agents sees it.
    let listed = call(&server, "list_agents", json!({})).expect("list");
    let agents = listed["agents"].as_array().expect("array");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], "s_worker");

    // read_output returns what it printed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while std::time::Instant::now() < deadline && !output.contains("work-output") {
        let read = call(
            &server,
            "read_output",
            json!({ "session_id": "s_worker", "max_bytes": 4096 }),
        )
        .expect("read");
        output = read["output"].as_str().unwrap_or_default().to_string();
        if !output.contains("work-output") {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(output.contains("work-output"), "got {output:?}");

    // get_status reports something sane.
    let status = call(&server, "get_status", json!({ "session_id": "s_worker" })).expect("status");
    assert_eq!(status["id"], "s_worker");
    assert!(status["status"].is_string());

    // release_agent ends it.
    call(
        &server,
        "release_agent",
        json!({ "session_id": "s_worker" }),
    )
    .expect("release");
    let after = call(&server, "get_status", json!({ "session_id": "s_worker" })).expect("status");
    assert_eq!(after["status"], "exited");
}

#[test]
fn send_prompt_types_into_a_session_and_submits() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    registry
        .spawn(
            spec(
                "s_ask",
                "read answer; printf 'got:%s\\n' \"$answer\"; sleep 30",
                &logs,
            ),
            record("s_ask", None),
        )
        .expect("spawn");
    let registry = Arc::new(Mutex::new(registry));

    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs),
    );

    call(
        &server,
        "send_prompt",
        json!({ "session_id": "s_ask", "text": "hello-there" }),
    )
    .expect("send");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while std::time::Instant::now() < deadline && !seen {
        let read = call(
            &server,
            "read_output",
            json!({ "session_id": "s_ask", "max_bytes": 4096 }),
        )
        .expect("read");
        seen = read["output"]
            .as_str()
            .is_some_and(|text| text.contains("got:hello-there"));
        if !seen {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(seen, "the session never received the submitted prompt");
}

#[test]
fn waiting_on_an_exited_session_returns_immediately() {
    // A dead session will never reach any other state; waiting the full
    // timeout for it would strand the caller.
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    registry
        .spawn(
            spec("s_quick", "printf 'bye\\n'; exit 0", &logs),
            record("s_quick", None),
        )
        .expect("spawn");
    let registry = Arc::new(Mutex::new(registry));

    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs),
    );

    let started = std::time::Instant::now();
    let waited = call(
        &server,
        "wait_for_agent",
        json!({ "session_id": "s_quick", "until": "done", "timeout_seconds": 30 }),
    )
    .expect("wait");

    assert_eq!(waited["status"], "exited");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "waiting on a dead session must not run to the timeout"
    );
}

#[test]
fn lineage_tools_answer_for_the_calling_session() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    registry
        .spawn(
            spec("s_parent", "sleep 30", &logs),
            record("s_parent", None),
        )
        .expect("spawn parent");
    registry
        .spawn(
            spec("s_child", "sleep 30", &logs),
            record("s_child", Some("s_parent")),
        )
        .expect("spawn child");
    let registry = Arc::new(Mutex::new(registry));

    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs).with_caller(Some("s_parent".to_string())),
    );

    let me = call(&server, "whoami", json!({})).expect("whoami");
    assert_eq!(me["id"], "s_parent");

    let children = call(&server, "list_children", json!({})).expect("children");
    let children = children["children"].as_array().expect("array");
    assert_eq!(children.len(), 1, "only the session's own children");
    assert_eq!(children[0]["id"], "s_child");
}

#[test]
fn lineage_tools_say_so_when_the_caller_is_unknown() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs).with_caller(None),
    );

    let error = call(&server, "whoami", json!({})).expect_err("should fail");
    assert!(
        error.contains("DIRIJOR_SESSION_ID"),
        "the message should name what is missing: {error}"
    );
}

#[test]
fn worktree_tools_work_against_a_real_repository() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&repo)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", temp.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repo.join("f.txt"), b"x").expect("write");
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=t@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "init",
    ]);

    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), temp.path().join("logs")),
    );

    let repo_arg = repo.to_string_lossy().to_string();
    let created = call(&server, "create_worktree", json!({ "repo": repo_arg })).expect("create");
    let path = created["path"].as_str().expect("a path").to_string();
    assert!(Path::new(&path).is_dir());

    let listed = call(&server, "list_worktrees", json!({ "repo": repo_arg })).expect("list");
    let paths: Vec<&str> = listed["worktrees"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert!(
        paths.contains(&path.as_str()),
        "the created worktree should be listed: {paths:?}"
    );

    call(
        &server,
        "remove_worktree",
        json!({ "repo": repo_arg, "worktree": path, "force": true }),
    )
    .expect("remove");
    assert!(!Path::new(&path).exists());
}

#[test]
fn spawn_agent_starts_a_session_owned_by_its_caller() {
    // Lineage is the point: a spawned session must record who spawned it, or
    // list_children and wait_for_children have nothing to work with.
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs)
            .with_caller(Some("s_orchestrator".to_string())),
    );

    // `shell` declares no binary, so spawning it by name is refused with a
    // message that says why rather than starting something unexpected.
    let refused = call(
        &server,
        "spawn_agent",
        json!({ "kind": "shell", "cwd": "/tmp" }),
    )
    .expect_err("shell has no binary");
    assert!(refused.contains("no binary"), "got {refused}");

    // An unknown agent is refused too.
    let unknown = call(
        &server,
        "spawn_agent",
        json!({ "kind": "not-an-agent", "cwd": "/tmp" }),
    )
    .expect_err("unknown agent");
    assert!(unknown.contains("no manifest"), "got {unknown}");

    // A missing directory is caught before anything is started.
    let bad_cwd = call(
        &server,
        "spawn_agent",
        json!({ "kind": "claude-code", "cwd": "/no/such/dir" }),
    )
    .expect_err("bad cwd");
    assert!(bad_cwd.contains("not a directory"), "got {bad_cwd}");

    // Old clients may still send `host`; fail before cwd inspection, host
    // lookup, code sync, or session creation while the new transport is dark.
    let unavailable = call(
        &server,
        "spawn_agent",
        json!({ "kind": "claude-code", "cwd": "/no/such/dir", "host": "forge" }),
    )
    .expect_err("remote transport unavailable");
    assert!(
        unavailable.contains("remote_transport_unavailable"),
        "got {unavailable}"
    );

    assert_eq!(
        registry.lock().expect("registry").live_count(),
        0,
        "no session should have been started by any of those"
    );
}

#[test]
fn a_spawned_session_records_its_parent_and_appears_as_a_child() {
    // Uses a real binary that exists everywhere, through a manifest override
    // directory, so the spawn path is exercised end to end.
    let temp = tempfile::tempdir().expect("temp");
    let manifests = temp.path().join("manifests");
    std::fs::create_dir_all(&manifests).expect("mkdir");
    std::fs::write(
        manifests.join("sleeper.json"),
        r#"{
            "schemaVersion": 1,
            "id": "sleeper",
            "version": "1",
            "statusModel": "processOnly",
            "agent": { "binary": "/bin/cat", "statusAuthority": "process" },
            "rules": []
        }"#,
    )
    .expect("write manifest");

    let (custom, failed) = ManifestEngine::load_dir(&manifests).expect("load");
    assert!(failed.is_empty(), "{failed:?}");

    let logs = temp.path().join("logs");
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(custom),
        temp.path().join("state.json"),
    )));
    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs).with_caller(Some("s_parent".to_string())),
    );

    let spawned = call(
        &server,
        "spawn_agent",
        json!({ "kind": "sleeper", "cwd": "/tmp", "name": "worker one", "prompt": "do the thing" }),
    )
    .expect("spawn");

    let id = spawned["id"].as_str().expect("an id").to_string();
    assert!(id.starts_with("s_"));
    assert_eq!(spawned["parent"], "s_parent");
    assert_eq!(
        spawned["pendingPrompt"], "do the thing",
        "the prompt is returned rather than typed into a terminal that is still starting"
    );

    let children = call(&server, "list_children", json!({})).expect("children");
    let children = children["children"].as_array().expect("array");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], id);
    assert_eq!(children[0]["title"], "worker one");

    call(&server, "release_agent", json!({ "session_id": id })).expect("release");
}
