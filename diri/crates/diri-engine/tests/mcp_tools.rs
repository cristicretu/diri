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
use diri_engine::registry::{Registry, RegistryRunError};
use diri_engine::session::SessionSpec;
use diri_engine::status::Authority;
use diri_engine::status::ClaudeHook;
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
        originating_prompt: None,
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
        run: None,
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

fn hook_spec(id: &str, script: &str, logs: &Path) -> SessionSpec {
    let mut spec = spec(id, script, logs);
    spec.authority = Authority::HooksPrimary;
    spec
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

fn output_contains(registry: &Arc<Mutex<Registry>>, id: &str, needle: &str) -> bool {
    let registry = registry.lock().expect("registry");
    let Some(session) = registry.get(id) else {
        return false;
    };
    session.screen_lines().join("\n").contains(needle)
}

#[test]
fn stale_public_mutations_durably_commit_a_raw_successor_before_rejecting() {
    for interrupt in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let blocked_parent = temp.path().join("blocked-state-parent");
        std::fs::write(&blocked_parent, b"not a directory").expect("blocking file");
        let state_file = blocked_parent.join("state.json");
        let logs = temp.path().join("logs");
        let id = if interrupt { "interrupt" } else { "send" };
        let mut registry = Registry::new(engine(), &state_file);
        let mut child = record(id, None);
        child.kind = AgentKind::CODEX;
        child.status = SessionStatus::Idle;
        child.run = Some(diri_proto::AgentRun {
            id: 1,
            state: diri_proto::AgentRunState::Completed,
            started_at: DateMillis(1.0),
            finished_at: Some(DateMillis(2.0)),
            terminal_outcome: Some("completed".into()),
        });
        registry
            .spawn(
                hook_spec(
                    id,
                    "trap '' INT; while IFS= read -r line; do printf 'GOT:%s\\n' \"$line\"; done",
                    &logs,
                ),
                child,
            )
            .expect("spawn child");

        // This models an attached/raw Enter that reached the PTY between the
        // last Registry fold and the public mutation. The first public sync
        // observes run 2; expected generation 1 must be rejected only after
        // that successor is durable.
        registry
            .get(id)
            .expect("live child")
            .claude_hook(ClaudeHook::UserPromptSubmit, false);
        let first_error = if interrupt {
            registry.interrupt_run(id, Some(1), None).unwrap_err()
        } else {
            registry
                .send_run_text(id, "stale".into(), true, Some(1), true, None)
                .unwrap_err()
        };
        assert!(
            matches!(first_error, RegistryRunError::Io(_)),
            "the first lifecycle write must fail before stale is returned: {first_error}"
        );

        std::fs::remove_file(&blocked_parent).expect("remove blocking file");
        std::fs::create_dir(&blocked_parent).expect("restore state directory");
        let error = if interrupt {
            registry.interrupt_run(id, Some(1), None).unwrap_err()
        } else {
            registry
                .send_run_text(id, "stale".into(), true, Some(1), true, None)
                .unwrap_err()
        };
        assert!(matches!(
            error,
            RegistryRunError::Stale {
                expected: 1,
                current: 2
            }
        ));

        let mut restored = Registry::new(engine(), &state_file);
        restored.load().expect("reload committed successor");
        let run = restored
            .record(id)
            .and_then(|record| record.run)
            .expect("restored run");
        assert_eq!(run.id, 2);
        assert_eq!(run.state, diri_proto::AgentRunState::Running);
    }
}

#[test]
fn queued_followups_survive_mcp_reconnect_and_interrupt_keeps_session_reusable() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    let mut child = record("child", Some("parent"));
    child.run = Some(diri_proto::AgentRun::starting(1, child.created_at));
    registry
        .spawn(
            hook_spec(
                "child",
                "trap '' INT; while IFS= read -r line; do printf 'GOT:%s\\n' \"$line\"; done",
                &logs,
            ),
            child,
        )
        .expect("spawn child");
    let registry = Arc::new(Mutex::new(registry));
    {
        let registry = registry.lock().expect("registry");
        registry
            .get("child")
            .unwrap()
            .claude_hook(ClaudeHook::UserPromptSubmit, false);
    }

    // The first MCP process queues two follow-ups while run 1 is working.
    {
        let server = McpServer::new(
            tool_definitions(),
            RegistryHost::new(Arc::clone(&registry), &logs).with_caller(Some("parent".into())),
        );
        for text in ["first", "second"] {
            let sent = call(
                &server,
                "send_prompt",
                json!({"session_id": "child", "text": text, "run_id": 1}),
            )
            .expect("queue follow-up");
            assert_eq!(sent["queued"], true);
        }
    }
    assert!(!output_contains(&registry, "child", "GOT:first"));

    // The MCP process reconnects. One completion boundary releases exactly
    // one FIFO item and starts run 2; the second remains queued.
    {
        let mut registry = registry.lock().expect("registry");
        registry
            .get("child")
            .unwrap()
            .claude_hook(ClaudeHook::UserPromptSubmit, false);
        registry
            .get("child")
            .unwrap()
            .claude_hook(ClaudeHook::Stop, false);
        std::thread::sleep(Duration::from_millis(150));
        registry
            .get("child")
            .unwrap()
            .feed_signal(diri_engine::status::StatusSignal::Tick);
        registry.sync_orchestration();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !output_contains(&registry, "child", "GOT:first")
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(output_contains(&registry, "child", "GOT:first"));
    assert!(!output_contains(&registry, "child", "GOT:second"));

    // A delayed command carrying run 1 cannot land in run 2.
    let reconnected = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs).with_caller(Some("parent".into())),
    );
    let stale = call(
        &reconnected,
        "send_prompt",
        json!({"session_id": "child", "text": "stale", "run_id": 1}),
    )
    .expect_err("stale generation");
    assert!(stale.contains("stale"), "{stale}");

    {
        let mut registry = registry.lock().expect("registry");
        registry
            .get("child")
            .unwrap()
            .claude_hook(ClaudeHook::UserPromptSubmit, false);
        registry
            .get("child")
            .unwrap()
            .claude_hook(ClaudeHook::Stop, false);
        std::thread::sleep(Duration::from_millis(150));
        registry
            .get("child")
            .unwrap()
            .feed_signal(diri_engine::status::StatusSignal::Tick);
        registry.sync_orchestration();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !output_contains(&registry, "child", "GOT:second")
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(output_contains(&registry, "child", "GOT:second"));

    let interrupted = call(
        &reconnected,
        "interrupt_agent",
        json!({
            "session_id": "child",
            "run_id": 3,
            "request_id": "interrupt-response-loss-1"
        }),
    )
    .expect("interrupt current run");
    assert_eq!(interrupted["run"]["state"], "aborted");
    let replayed_interrupt = call(
        &reconnected,
        "interrupt_agent",
        json!({
            "session_id": "child",
            "run_id": 3,
            "request_id": "interrupt-response-loss-1"
        }),
    )
    .expect("replay interrupt response");
    assert_eq!(replayed_interrupt["run"], interrupted["run"]);
    let next = call(
        &reconnected,
        "send_prompt",
        json!({"session_id": "child", "text": "after-abort", "run_id": 3}),
    )
    .expect("reuse after abort");
    assert_eq!(next["run"]["id"], 4);
}

#[test]
fn reports_wait_for_the_parent_composer_and_preserve_fifo_order() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    let mut parent = record("parent", None);
    parent.kind = AgentKind::CODEX;
    let mut reporter = record("reporter", Some("parent"));
    reporter.kind = AgentKind::CODEX;
    for (record, script) in [
        (
            parent,
            "trap '' INT; while IFS= read -r line; do printf 'PARENT:%s\\n' \"$line\"; done",
        ),
        (
            reporter,
            "trap '' INT; while IFS= read -r line; do printf 'REPORTER:%s\\n' \"$line\"; done",
        ),
    ] {
        let id = record.id.0.clone();
        registry
            .spawn(hook_spec(&id, script, &logs), record)
            .expect("spawn");
    }
    registry
        .get("parent")
        .unwrap()
        .claude_hook(ClaudeHook::UserPromptSubmit, false);
    let first = registry
        .report_to_parent(
            "reporter",
            "report-one".into(),
            true,
            Some(1),
            Some("report-response-loss-1".into()),
        )
        .expect("queue first report");
    let replay = registry
        .report_to_parent(
            "reporter",
            "report-one".into(),
            true,
            Some(1),
            Some("report-response-loss-1".into()),
        )
        .expect("replay first report");
    assert_eq!(replay, first);
    let (parent, queued, _) = registry
        .report_to_parent("reporter", "report-two".into(), true, Some(1), None)
        .expect("queue report");
    assert_eq!(parent, "parent");
    assert!(queued);
    let registry = Arc::new(Mutex::new(registry));
    assert!(!output_contains(&registry, "parent", "PARENT:report-one"));

    for (completion, absent) in [
        ("PARENT:report-one", "PARENT:report-two"),
        ("PARENT:report-two", "PARENT:never"),
    ] {
        {
            let mut registry = registry.lock().expect("registry");
            registry
                .get("parent")
                .unwrap()
                .claude_hook(ClaudeHook::Stop, false);
            std::thread::sleep(Duration::from_millis(150));
            registry
                .get("parent")
                .unwrap()
                .feed_signal(diri_engine::status::StatusSignal::Tick);
            registry.sync_orchestration();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline
            && !output_contains(&registry, "parent", completion)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(output_contains(&registry, "parent", completion));
        assert!(!output_contains(&registry, "parent", absent));
        if completion.ends_with("one") {
            registry
                .lock()
                .expect("registry")
                .get("parent")
                .unwrap()
                .claude_hook(ClaudeHook::UserPromptSubmit, false);
        }
    }

    // Merely acknowledging reporter run 2 must not stale the still-current
    // run 1: that run must remain able to deliver its final report until the
    // future generation actually becomes active.
    let mut registry = registry.lock().expect("registry");
    registry
        .interrupt_run("reporter", Some(1), None)
        .expect("abort");
    registry
        .send_run_text("reporter", "next".into(), true, Some(1), true, None)
        .expect("start next run");
    let accepted = registry
        .report_to_parent("reporter", "late".into(), true, Some(1), None)
        .expect("current report remains valid while run 2 is only queued");
    assert_eq!(accepted.0, "parent");
}

#[test]
fn the_binary_free_shell_manifest_spawns_the_users_login_shell() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::clone(&registry), &logs),
    );

    let spawned = call(
        &server,
        "spawn_agent",
        json!({"kind": "shell", "cwd": temp.path()}),
    )
    .expect("spawn shell");
    let id = spawned["id"].as_str().expect("session id");
    assert!(registry.lock().expect("registry").get(id).is_some());
    call(&server, "release_agent", json!({"session_id": id})).expect("release shell");
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
                "for i in 1 2; do read answer; printf 'got:%s\\n' \"$answer\"; done; sleep 30",
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

    let first = call(
        &server,
        "send_prompt",
        json!({
            "session_id": "s_ask",
            "text": "hello-there",
            "request_id": "lost-response-retry-1"
        }),
    )
    .expect("send");
    // Model a response lost after the daemon committed delivery: the caller
    // retries the same request id and must receive the original result without
    // another PTY write.
    let replay = call(
        &server,
        "send_prompt",
        json!({
            "session_id": "s_ask",
            "text": "hello-there",
            "request_id": "lost-response-retry-1"
        }),
    )
    .expect("replay");
    assert_eq!(replay["queued"], first["queued"]);
    assert_eq!(replay["run"], first["run"]);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while std::time::Instant::now() < deadline && !output.contains("got:hello-there") {
        let read = call(
            &server,
            "read_output",
            json!({ "session_id": "s_ask", "max_bytes": 4096 }),
        )
        .expect("read");
        output = read["output"].as_str().unwrap_or_default().to_owned();
        if !output.contains("got:hello-there") {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(
        output.contains("got:hello-there"),
        "the session never received the submitted prompt"
    );
    std::thread::sleep(Duration::from_millis(100));
    let read = call(
        &server,
        "read_output",
        json!({ "session_id": "s_ask", "max_bytes": 4096 }),
    )
    .expect("read after replay");
    let output = read["output"].as_str().unwrap_or_default();
    assert_eq!(
        output.matches("got:hello-there").count(),
        1,
        "a response-loss retry must not submit the prompt twice: {output:?}"
    );
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
    assert_eq!(waited["reached"], false);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "waiting on a dead session must not run to the timeout"
    );
}

#[test]
fn embedded_waits_match_run_generation_and_idle_semantics() {
    let temp = tempfile::tempdir().expect("temp");
    let logs = temp.path().join("logs");
    let mut registry = Registry::new(engine(), temp.path().join("state.json"));
    let mut agent = record("agent", None);
    agent.kind = AgentKind::CODEX;
    agent.status = SessionStatus::Idle;
    agent.run = Some(diri_proto::AgentRun::starting(3, agent.created_at));
    registry.insert_record(agent);
    let mut dead = record("dead", None);
    dead.kind = AgentKind::CODEX;
    dead.status = SessionStatus::Exited(diri_proto::ExitInfo {
        reason: diri_proto::ExitReason::Exited,
        code: Some(0),
        signal: None,
    });
    dead.run = Some(diri_proto::AgentRun {
        id: 1,
        state: diri_proto::AgentRunState::Failed,
        started_at: dead.created_at,
        finished_at: Some(dead.updated_at),
        terminal_outcome: Some("process_exited".into()),
    });
    registry.insert_record(dead);
    let server = McpServer::new(
        tool_definitions(),
        RegistryHost::new(Arc::new(Mutex::new(registry)), &logs),
    );

    let stale = call(
        &server,
        "wait_for_agent",
        json!({"session_id": "agent", "run_id": 2, "timeout_seconds": 0}),
    )
    .expect("stale wait");
    assert_eq!(stale["superseded"], true);

    let idle = call(
        &server,
        "wait_for_agent",
        json!({"session_id": "agent", "run_id": 3, "until": "idle", "timeout_seconds": 0}),
    )
    .expect("idle wait");
    assert_eq!(idle["reached"], true);

    let future = call(
        &server,
        "wait_for_agent",
        json!({"session_id": "agent", "run_id": 4, "timeout_seconds": 0}),
    )
    .expect("future wait");
    assert_eq!(future["timedOut"], true);
    assert_ne!(future["reached"], true);

    let started = std::time::Instant::now();
    let dead_future = call(
        &server,
        "wait_for_agent",
        json!({"session_id": "dead", "run_id": 2, "timeout_seconds": 60}),
    )
    .expect("dead future wait");
    assert_eq!(dead_future["reached"], false);
    assert_eq!(dead_future["superseded"], false);
    assert!(started.elapsed() < Duration::from_secs(1));
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
        "no session should have been started by any rejected request"
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
            "agent": {
                "binary": "/bin/sh",
                "spawnArgs": ["-c", "stty -echo; printf '\\033[?2004h> '; exec cat"],
                "statusAuthority": "process"
            },
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
    assert!(
        spawned.get("pendingPrompt").is_none(),
        "a successful spawn has already delivered its prompt"
    );

    let children = call(&server, "list_children", json!({})).expect("children");
    let children = children["children"].as_array().expect("array");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], id);
    assert_eq!(children[0]["title"], "worker one");

    call(&server, "release_agent", json!({ "session_id": id })).expect("release");
}
