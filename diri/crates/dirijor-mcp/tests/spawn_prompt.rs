#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::{
    AgentKind, DateMillis, Method, Project, Resumability, SessionId, SessionRecord, SessionStatus,
    TitleSource,
};
use dirijor_mcp::Bridge;
use serde_json::json;

fn git(cwd: &Path, arguments: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_repository(path: &Path) {
    std::fs::create_dir_all(path).expect("create repository");
    git(path, &["init"]);
    std::fs::write(path.join("README.md"), "prompt delivery fixture\n").expect("write readme");
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Diri Tests",
            "-c",
            "user.email=diri@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
}

fn large_markdown_prompt() -> String {
    let section = "# DeliveryMarker\n\n`x` — y\n\n```text\na\nb\n```\n\n1. do\n2. go\n\n";
    section.repeat(72)
}

fn start_server(temp: &Path, fixture: &Path, parent_cwd: &Path) -> Arc<ControlServer> {
    let manifests = temp.join("manifests");
    std::fs::create_dir_all(&manifests).expect("create manifests");
    std::fs::write(
        manifests.join("prompt-fixture.json"),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "prompt-fixture",
            "version": "1",
            "statusModel": "processOnly",
            "agent": {
                "binary": fixture.to_string_lossy(),
                "statusAuthority": "process"
            },
            "rules": []
        }))
        .expect("encode manifest"),
    )
    .expect("write manifest");
    let (engine, failures) = ManifestEngine::load_dir(&manifests).expect("load manifests");
    assert!(failures.is_empty(), "manifest failures: {failures:?}");

    let mut registry = Registry::new(Arc::new(engine), temp.join("state.json"));
    let project: Project =
        serde_json::from_value(registry.add_project(parent_cwd.to_string_lossy().as_ref()))
            .expect("parent project");
    let now = DateMillis::from(std::time::SystemTime::now());
    registry.insert_record(SessionRecord {
        attention_state: None,
        id: SessionId::new("s_parent"),
        kind: AgentKind::CODEX,
        cwd: parent_cwd.to_string_lossy().into_owned(),
        project_id: project.id,
        worktree_path: None,
        git_branch: None,
        title: "parent".into(),
        title_source: TitleSource::Placeholder,
        account_profile: None,
        originating_prompt: None,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Idle,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        capabilities: None,
        parent: None,
        created_at: now,
        updated_at: now,
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
    });
    let registry = Arc::new(Mutex::new(registry));
    let server = Arc::new(
        ControlServer::new(registry, temp.join("daemon.sock")).with_logs_dir(temp.join("logs")),
    );
    let listener = server.bind().expect("bind control socket");
    {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let server = Arc::clone(&server);
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
    }
    server
}

fn call_spawn_agent_through_mcp(socket: &Path, arguments: serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dirijor-mcp"))
        .env_clear()
        .env("DIRIJOR_SOCKET", socket)
        .env("DIRIJOR_SESSION_ID", "s_parent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch MCP server");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "spawn_agent", "arguments": arguments },
    });
    {
        let mut stdin = child.stdin.take().expect("MCP stdin");
        for message in [
            json!({"jsonrpc":"2.0", "id":"init", "method":"initialize", "params":{
                "protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"test", "version":"1"}
            }}),
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
        ] {
            serde_json::to_writer(&mut stdin, &message).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        serde_json::to_writer(&mut stdin, &request).expect("encode MCP call");
        stdin.write_all(b"\n").expect("write MCP call");
    }

    let mut line = String::new();
    let mut output = BufReader::new(child.stdout.take().expect("MCP stdout"));
    output
        .read_line(&mut line)
        .expect("read initialize response");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"],
        "init"
    );
    line.clear();
    output.read_line(&mut line).expect("read MCP response");
    let status = child.wait().expect("wait for MCP server");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("MCP stderr")
        .read_to_string(&mut stderr)
        .expect("read MCP stderr");
    assert!(status.success(), "MCP server failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(&line).expect("decode MCP response");
    assert_eq!(
        response["result"]["isError"], false,
        "spawn_agent failed: {}",
        response["result"]["content"][0]["text"]
    );
    serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("MCP text content"),
    )
    .expect("decode spawn result")
}

#[test]
fn standalone_cli_spawn_creates_a_root_session_without_an_mcp_caller() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    let fixture = temp.path().join("prompt-fixture");
    std::fs::create_dir_all(&repo).expect("create repository directory");
    std::fs::write(&fixture, "#!/bin/sh\nsleep 30\n").expect("write fixture");
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))
        .expect("make fixture executable");

    let server = start_server(temp.path(), &fixture, &repo);
    let bridge = Bridge::new(server.socket_path().to_path_buf(), None);
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["session", "spawn", "prompt-fixture", "--cwd"])
        .arg(&repo)
        .arg("--json")
        .env("DIRIJOR_SOCKET", server.socket_path())
        .env_remove("DIRIJOR_SESSION_ID")
        .output()
        .expect("spawn standalone CLI");
    assert!(
        output.status.success(),
        "CLI spawn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let spawned: serde_json::Value = serde_json::from_slice(&output.stdout).expect("CLI result");
    let id = spawned["id"].as_str().expect("session id");
    let sessions = bridge
        .request(Method::SESSION_LIST, json!({}), Duration::from_secs(3))
        .expect("list sessions");
    let record = sessions["sessions"]
        .as_array()
        .expect("session array")
        .iter()
        .find(|record| record["id"] == id)
        .expect("spawned session record");

    assert!(record["parent"].is_null(), "standalone sessions are roots");
    bridge
        .request(
            Method::SESSION_KILL,
            json!({"sessionID": id}),
            Duration::from_secs(3),
        )
        .expect("release standalone session");
}

#[test]
fn spawn_agent_waits_for_a_large_multiline_prompt_in_a_new_worktree() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    let capture = temp.path().join("received.bin");
    let fixture = temp.path().join("prompt-fixture");
    initialize_repository(&repo);

    let prompt = large_markdown_prompt();
    assert!(
        prompt.len() > 4 * 1024,
        "fixture must exercise a large prompt"
    );
    let framed_prompt_len = prompt.len() + b"\x1b[200~\x1b[201~".len();
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\nsleep 1.2\nstty raw -echo\nprintf '\\033[?2004h> '\ndd bs=1 count={framed_prompt_len} of='{}' 2>/dev/null\nprintf '\\nDeliveryMarker\\n'\ndd bs=1 count=1 >>'{}' 2>/dev/null\nprintf '\\naccepted\\n'\nsleep 30\n",
            capture.display(),
            capture.display(),
        ),
    )
    .expect("write fixture");
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))
        .expect("make fixture executable");

    let server = start_server(temp.path(), &fixture, &repo);
    let bridge = Bridge::new(server.socket_path().to_path_buf(), Some("s_parent".into()));
    let started = Instant::now();
    let spawned = call_spawn_agent_through_mcp(
        server.socket_path(),
        json!({
            "kind": "prompt-fixture",
            "cwd": repo,
            "worktree": true,
            "branch": "test/prompt-delivery",
            "name": "prompt delivery regression",
            "prompt": prompt,
        }),
    );
    let elapsed = started.elapsed();

    let id = spawned["id"].as_str().expect("session id");
    let worktree = spawned["worktreePath"]
        .as_str()
        .expect("spawned worktree")
        .to_owned();
    let expected = format!("\x1b[200~{prompt}\x1b[201~\r").into_bytes();
    let deadline = Instant::now() + Duration::from_secs(1);
    let received = loop {
        let received = std::fs::read(&capture).unwrap_or_default();
        if received.len() >= expected.len() || Instant::now() >= deadline {
            break received;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let action_deadline = Instant::now() + Duration::from_secs(1);
    let acted = loop {
        let output = bridge
            .call("read_output", &json!({ "session_id": id }))
            .expect("read fixture output");
        if output["text"]
            .as_str()
            .is_some_and(|text| text.contains("accepted"))
        {
            break true;
        }
        if Instant::now() >= action_deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    bridge
        .call("release_agent", &json!({ "session_id": id }))
        .expect("release fixture agent");
    bridge
        .call(
            "remove_worktree",
            &json!({ "repo": repo, "path": worktree, "force": true }),
        )
        .expect("remove fixture worktree");

    assert!(
        elapsed >= Duration::from_secs(1),
        "spawn_agent returned before the delayed composer existed: {elapsed:?}"
    );
    assert_eq!(
        received, expected,
        "the MCP prompt must arrive exactly once"
    );
    assert!(acted, "the spawned agent must act on the submitted prompt");
}

#[test]
fn repeated_mcp_sends_deliver_one_copy() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let capture = temp.path().join("received");
    let fixture = temp.path().join("prompt-fixture");
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\nstty raw -echo\nprintf '\\033[?2004hREADY'\nexec cat > '{}'\n",
            capture.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = start_server(temp.path(), &fixture, &repo);
    let bridge = Bridge::new(server.socket_path().into(), Some("s_parent".into()));
    let spawned = bridge
        .call("spawn_agent", &json!({"kind":"prompt-fixture", "cwd":repo}))
        .unwrap();
    let id = spawned["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = bridge
            .call("read_output", &json!({"session_id":id}))
            .unwrap();
        if screen["text"]
            .as_str()
            .unwrap_or_default()
            .contains("READY")
        {
            break;
        }
        assert!(Instant::now() < deadline, "fixture did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Retry with both omitted identity and the exact identity in the receipt.
    let first = bridge
        .call(
            "send_prompt",
            &json!({"session_id":id, "text":"run this task once"}),
        )
        .unwrap();
    let receipt_id = first["receipt"]["message_id"].as_str().unwrap();
    for _ in 0..10 {
        let retry = Bridge::new(server.socket_path().into(), Some("s_parent".into()))
            .call(
                "send_prompt",
                &json!({"session_id":id, "text":"run this task once"}),
            )
            .unwrap();
        assert_eq!(retry["receipt"]["duplicate"], true);
    }
    let retry = bridge
        .call(
            "send_prompt",
            &json!({"session_id":id, "text":"run this task once", "message_id":receipt_id}),
        )
        .unwrap();
    assert_eq!(retry["receipt"]["duplicate"], true);
    // Simultaneous calls on separate control connections have one owner too.
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let bridge = &bridge;
            scope.spawn(move || {
                let result = bridge
                    .call(
                        "send_prompt",
                        &json!({"session_id":id, "text":"run this task once"}),
                    )
                    .unwrap();
                assert_eq!(result["receipt"]["duplicate"], true);
            });
        }
    });
    // Intentional repeats require a distinct logical message identity.
    for _ in 0..2 {
        bridge.call("send_prompt", &json!({"session_id":id, "text":"run this task once", "message_id":"intentional-repeat"})).unwrap();
    }
    // Lose the reply after dispatch. The retry may race the first connection;
    // the durable identity still permits only one PTY write.
    let mut disconnected = std::os::unix::net::UnixStream::connect(server.socket_path()).unwrap();
    serde_json::to_writer(&mut disconnected, &diri_proto::ControlMessage::Request {
        id: 900, method: Method::SESSION_DELIVER_MESSAGE.into(),
        params: Some(json!({"sessionID":id, "senderID":"s_parent", "messageID":"lost-reply", "text":"lost reply task", "submit":true})),
    }).unwrap();
    disconnected.write_all(b"\n").unwrap();
    drop(disconnected);
    for _ in 0..3 {
        bridge
            .call(
                "send_prompt",
                &json!({"session_id":id, "text":"lost reply task", "message_id":"lost-reply"}),
            )
            .unwrap();
    }
    // Raw interactive input deliberately has no content deduplication.
    for _ in 0..2 {
        bridge
            .request(
                Method::SESSION_SEND_TEXT,
                json!({"sessionID":id, "text":"raw", "submit":false}),
                Duration::from_secs(3),
            )
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(100));
    bridge
        .call("release_agent", &json!({"session_id":id}))
        .unwrap();
    assert_eq!(
        std::fs::read(capture).unwrap(),
        [
            b"\x1b[200~run this task once\x1b[201~\r".repeat(2),
            b"\x1b[200~lost reply task\x1b[201~\rrawraw".to_vec()
        ]
        .concat(),
        "repeated MCP calls must not repeat the prompt in the PTY"
    );
}

#[test]
fn parent_reports_are_deduplicated_across_sender_renames() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("child")).unwrap();
    let fixture = temp.path().join("prompt-fixture");
    std::fs::write(
        &fixture,
        "#!/bin/sh\nstty raw -echo\nprintf '\\033[?2004hREADY'\nexec cat > received\n",
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = start_server(temp.path(), &fixture, &repo);
    let root = Bridge::new(server.socket_path().into(), Some("s_parent".into()));
    let parent = root
        .call("spawn_agent", &json!({"kind":"prompt-fixture", "cwd":repo}))
        .unwrap();
    let parent_id = parent["id"].as_str().unwrap();
    let parent_bridge = Bridge::new(server.socket_path().into(), Some(parent_id.into()));
    let child = parent_bridge
        .call(
            "spawn_agent",
            &json!({"kind":"prompt-fixture", "cwd":repo.join("child")}),
        )
        .unwrap();
    let child_id = child["id"].as_str().unwrap();
    let child_bridge = Bridge::new(server.socket_path().into(), Some(child_id.into()));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = parent_bridge
            .call("read_output", &json!({"session_id":parent_id}))
            .unwrap();
        if screen["text"]
            .as_str()
            .unwrap_or_default()
            .contains("READY")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "parent fixture did not become ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let args = json!({"summary":"task complete", "status":"done", "proof":["checks passed"]});
    let first = child_bridge.call("report_to_parent", &args).unwrap();
    root.request(
        Method::SESSION_RENAME,
        json!({"sessionID":child_id, "title":"renamed worker"}),
        Duration::from_secs(3),
    )
    .unwrap();
    for _ in 0..10 {
        let result = Bridge::new(server.socket_path().into(), Some(child_id.into()))
            .call("report_to_parent", &args)
            .unwrap();
        assert_eq!(result["receipt"]["duplicate"], true);
    }
    std::thread::sleep(Duration::from_millis(100));
    parent_bridge
        .call("release_agent", &json!({"session_id":child_id}))
        .unwrap();
    root.call("release_agent", &json!({"session_id":parent_id}))
        .unwrap();
    assert_eq!(
        std::fs::read(repo.join("received")).unwrap(),
        format!(
            "\x1b[200~{}\x1b[201~\r",
            first["delivered"].as_str().unwrap()
        )
        .as_bytes()
    );
}

#[test]
fn retrying_a_spawn_returns_the_same_session() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("prompt-fixture");
    std::fs::write(&fixture, "#!/bin/sh\nexec cat\n").unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = start_server(temp.path(), &fixture, temp.path());
    let bridge = Bridge::new(server.socket_path().into(), Some("s_parent".into()));
    let args = json!({"kind":"prompt-fixture", "cwd":temp.path()});
    let first = bridge.call("spawn_agent", &args).unwrap();
    let second = Bridge::new(server.socket_path().into(), Some("s_parent".into()))
        .call("spawn_agent", &args)
        .unwrap();
    for id in [first["id"].clone(), second["id"].clone()] {
        let _ = bridge.call("release_agent", &json!({"session_id":id}));
    }
    assert_eq!(first["id"], second["id"], "retry created a second Agent");
}

#[test]
fn task_completion_is_explicit_and_specific_to_the_submitted_task() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("prompt-fixture");
    std::fs::write(&fixture, "#!/bin/sh\nstty raw -echo\nexec cat > received\n").unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = start_server(temp.path(), &fixture, temp.path());
    let parent = Bridge::new(server.socket_path().into(), Some("s_parent".into()));
    let child = parent
        .call(
            "spawn_agent",
            &json!({"kind":"prompt-fixture", "cwd":temp.path()}),
        )
        .unwrap();
    let id = child["id"].as_str().unwrap();
    let child_bridge = Bridge::new(server.socket_path().into(), Some(id.into()));
    let args = json!({"session_id":id, "text":"do this specific task", "request_id":"task-one"});
    let first = parent.call("submit_task", &args).unwrap();
    let task_id = first["task"]["task_id"].as_str().unwrap();
    let recovered = parent
        .call("get_task", &json!({"request_id":"task-one"}))
        .unwrap();
    assert_eq!(recovered["task_id"], task_id);
    let second = parent.call("submit_task", &args).unwrap();
    assert_eq!(second["duplicate"], true);
    assert_eq!(second["task"]["task_id"], task_id);
    let waiting = parent
        .call("wait_for_task", &json!({"task_id":task_id,"timeout_s":0}))
        .unwrap();
    assert_eq!(waiting["completed"], false);
    assert_eq!(waiting["timed_out"], true);
    assert!(
        child_bridge
            .call(
                "report_task",
                &json!({"task_id":task_id,"status":"completed"})
            )
            .is_err()
    );
    assert!(
        parent
            .call(
                "report_task",
                &json!({"task_id":task_id,"status":"acknowledged"})
            )
            .is_err()
    );
    child_bridge
        .call(
            "report_task",
            &json!({"task_id":task_id,"status":"acknowledged"}),
        )
        .unwrap();
    let wait_parent = parent.clone();
    let wait_id = task_id.to_owned();
    let waiter = std::thread::spawn(move || {
        wait_parent
            .call("wait_for_task", &json!({"task_id":wait_id,"timeout_s":3}))
            .unwrap()
    });
    child_bridge
        .call(
            "report_task",
            &json!({"task_id":task_id,"status":"completed", "result":"verified outcome"}),
        )
        .unwrap();
    let finished = waiter.join().unwrap();
    assert_eq!(finished["completed"], true);
    assert_eq!(finished["task"]["result"], "verified outcome");
    let another = parent
        .call(
            "submit_task",
            &json!({"session_id":id,"text":"a separate task", "request_id":"task-two"}),
        )
        .unwrap();
    assert_eq!(another["task"]["status"], "awaiting_acknowledgement");
    parent
        .call("release_agent", &json!({"session_id":id}))
        .unwrap();
}
