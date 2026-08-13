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

fn start_server(temp: &Path, fixture: &Path) -> Arc<ControlServer> {
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

    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
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
        serde_json::to_writer(&mut stdin, &request).expect("encode MCP call");
        stdin.write_all(b"\n").expect("write MCP call");
    }

    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("MCP stdout"))
        .read_line(&mut line)
        .expect("read MCP response");
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

    let server = start_server(temp.path(), &fixture);
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
