//! The Engine-served `dirijor` MCP endpoint, driven end to end over HTTP the
//! way Claude Code and Codex drive it: initialize, tools/list, a tools/call
//! that resolves the calling session from its bearer token, cancellation of
//! a long wait from a second request, and token death with the session.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::{ControlServer, InjectionConfig};
use diri_engine::{ManifestEngine, Registry};
use serde_json::{Value, json};

struct Engine {
    server: Arc<ControlServer>,
    registry: Arc<Mutex<Registry>>,
    url: String,
    _temp: tempfile::TempDir,
}

fn start() -> Engine {
    let temp = tempfile::tempdir().unwrap();
    let (manifests, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(manifests),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
            .with_config_dir(temp.path().join("config"))
            .with_notes_dir(temp.path().join("notes"))
            .with_agent_naming_setting(|| true)
            .with_injection(InjectionConfig {
                inject_dir: temp.path().join("inject"),
                cli_path: temp.path().join("bin/dirijor"),
            }),
    );
    let listener = server.bind().unwrap();
    {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let server = Arc::clone(&server);
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
    }
    let url = server.start_mcp_http().unwrap();
    Engine {
        server,
        registry,
        url,
        _temp: temp,
    }
}

fn spawn_session(engine: &Engine, cwd: &Path) -> String {
    let record = dirijor_mcp::Bridge::new(engine.server.socket_path().to_owned(), None)
        .request(
            "session.spawn",
            json!({"kind":{"shell":{}}, "cwd":cwd, "argv":["/bin/sh","-c","sleep 60"]}),
            Duration::from_secs(20),
        )
        .unwrap();
    record["id"].as_str().unwrap().to_owned()
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    /// The JSON-RPC message in a JSON body or in the last SSE `data:` line.
    fn message(&self) -> Value {
        if self.head.contains("text/event-stream") {
            let data = self
                .body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .next_back()
                .unwrap_or_else(|| panic!("no event in {:?}", self.body));
            serde_json::from_str(data).unwrap()
        } else {
            serde_json::from_str(&self.body).unwrap()
        }
    }
}

fn post(url: &str, token: Option<&str>, body: &Value) -> Reply {
    let address = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = serde_json::to_string(body).unwrap();
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{auth}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    Reply {
        status: head[9..12].parse().unwrap(),
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn call(url: &str, token: &str, id: u64, tool: &str, arguments: Value) -> Value {
    let reply = post(
        url,
        Some(token),
        &json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
    );
    assert_eq!(reply.status, 200, "{}", reply.head);
    reply.message()
}

#[test]
fn an_agent_names_its_own_session_over_http() {
    let engine = start();
    let cwd = tempfile::tempdir().unwrap();
    let session = spawn_session(&engine, cwd.path());
    let token = engine.server.mcp_http_token(&session).unwrap();
    let title = || {
        let registry = engine.registry.lock().unwrap();
        let record = registry.record(&session).unwrap();
        (record.title.clone(), record.title_source)
    };

    let named = call(
        &engine.url,
        &token,
        1,
        "name_session",
        json!({"name": "Fix Login Redirect"}),
    );
    let named = &named["result"]["structuredContent"];
    assert_eq!(named["applied"], true, "{named}");
    assert_eq!(named["title"], "fix-login-redirect");
    assert_eq!(
        title(),
        (
            "fix-login-redirect".into(),
            diri_proto::TitleSource::AgentNamed
        )
    );
    let whoami = call(&engine.url, &token, 2, "whoami", json!({}));
    assert_eq!(
        whoami["result"]["structuredContent"]["name_set_by"],
        "agent"
    );

    // The user renames the tab: the Agent's next name is refused, politely.
    dirijor_mcp::Bridge::new(engine.server.socket_path().to_owned(), None)
        .request(
            "session.rename",
            json!({"sessionID": session, "title": "Login work"}),
            Duration::from_secs(5),
        )
        .unwrap();
    let kept = call(
        &engine.url,
        &token,
        3,
        "name_session",
        json!({"name": "other-task"}),
    );
    let kept = &kept["result"]["structuredContent"];
    assert_eq!(kept["applied"], false, "{kept}");
    assert_eq!(kept["reason"], "userNamed");
    assert!(kept["note"].as_str().unwrap().contains("keep their name"));
    assert_eq!(title().0, "Login work");
    let whoami = call(&engine.url, &token, 4, "whoami", json!({}));
    assert_eq!(whoami["result"]["structuredContent"]["name_set_by"], "user");

    // A name the user asked for lands as written.
    let asked = call(
        &engine.url,
        &token,
        5,
        "name_session",
        json!({"name": "Release prep", "user_requested": true}),
    );
    assert_eq!(asked["result"]["structuredContent"]["applied"], true);
    assert_eq!(title().0, "Release prep");
}

#[test]
fn an_agent_reaches_its_own_session_over_http() {
    let engine = start();
    let cwd = tempfile::tempdir().unwrap();
    let session = spawn_session(&engine, cwd.path());
    let token = engine.server.mcp_http_token(&session).unwrap();
    assert!(!token.is_empty());

    // The shared Claude config names the variable and never holds a token.
    let config =
        std::fs::read_to_string(engine._temp.path().join("inject/claude-mcp-http.json")).unwrap();
    let config: Value = serde_json::from_str(&config).unwrap();
    assert_eq!(config["mcpServers"]["dirijor"]["type"], "http");
    assert_eq!(config["mcpServers"]["dirijor"]["url"], engine.url);
    assert_eq!(
        config["mcpServers"]["dirijor"]["headers"]["Authorization"],
        "Bearer ${DIRIJOR_MCP_TOKEN}"
    );

    let initialized = post(
        &engine.url,
        Some(&token),
        &json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"test","version":"1"}}}),
    );
    assert_eq!(initialized.status, 200);
    let initialized = initialized.message();
    assert_eq!(initialized["result"]["serverInfo"]["name"], "dirijor");
    assert!(
        initialized["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("INSIDE Diri")
    );
    let ack = post(
        &engine.url,
        Some(&token),
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    assert_eq!(ack.status, 202);

    let listed = post(
        &engine.url,
        Some(&token),
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .message();
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for expected in [
        "whoami",
        "spawn_agent",
        "wait_for_agent",
        "wait_any",
        "browser",
        "read_note",
        "schedule_agent",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    // Identical to what the stdio frontend lists for the same Engine.
    let stdio = dirijor_mcp::Bridge::new(engine.server.socket_path().to_owned(), None)
        .tool_definitions()
        .unwrap()
        .iter()
        .map(|tool| tool.wire_value())
        .collect::<Vec<_>>();
    assert_eq!(listed["result"]["tools"], Value::Array(stdio));

    let whoami = call(&engine.url, &token, 2, "whoami", json!({}));
    assert_eq!(whoami["result"]["isError"], false, "{whoami}");
    let identity = &whoami["result"]["structuredContent"];
    assert_eq!(identity["hosted"], true);
    assert_eq!(identity["session"]["id"], session, "{identity}");

    // A mutation runs under the caller's identity: the write policy sees
    // this session and refuses to let it manage itself, as over stdio.
    let own = call(
        &engine.url,
        &token,
        3,
        "manage_agent",
        json!({"session_id": session, "action": "hibernate"}),
    );
    assert_eq!(own["result"]["isError"], true, "{own}");
    assert_eq!(
        own["result"]["content"][0]["text"],
        "an agent cannot manage its own session"
    );

    // Other tokens: unknown, forged for another id, or no token at all.
    let ping = json!({"jsonrpc":"2.0","id":9,"method":"ping"});
    assert_eq!(post(&engine.url, None, &ping).status, 401);
    assert_eq!(post(&engine.url, Some("nope"), &ping).status, 401);
    let (_, mac) = token.rsplit_once('.').unwrap();
    assert_eq!(
        post(&engine.url, Some(&format!("s-other.{mac}")), &ping).status,
        401
    );
    assert_eq!(post(&engine.url, Some(&token), &ping).status, 200);

    // A long read is cancelled by a notification on another request.
    let waiter = {
        let url = engine.url.clone();
        let token = token.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let reply = post(
                &url,
                Some(&token),
                &json!({"jsonrpc":"2.0","id":"long","method":"tools/call","params":{
                    "name":"wait_for_agent",
                    "arguments":{"session_id":session,"until":"exited","timeout_s":600}}}),
            );
            (reply, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    let cancelled = post(
        &engine.url,
        Some(&token),
        &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"long"}}),
    );
    assert_eq!(cancelled.status, 202);
    let (reply, elapsed) = waiter.join().unwrap();
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    assert_eq!(reply.status, 200);
    assert!(
        !reply.body.contains("\"result\""),
        "a cancelled request gets no response: {}",
        reply.body
    );

    // The token dies with the session.
    engine
        .registry
        .lock()
        .unwrap()
        .terminate(&session, Duration::from_secs(2))
        .unwrap();
    assert_eq!(post(&engine.url, Some(&token), &ping).status, 401);
}

/// A fake Agent CLI: answers `--version`, and otherwise records the argv and
/// whether it was handed a token, then idles like a live session.
fn fake_cli(dir: &Path, name: &str, version_line: &str, out: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version_line}'; exit 0; fi\n\
             {{ printf '%s\\n' \"$@\"; echo \"TOKEN=${{DIRIJOR_MCP_TOKEN:+set}}\"; }} > '{}'/\"$DIRIJOR_SESSION_ID.tmp\"\n\
             mv '{}'/\"$DIRIJOR_SESSION_ID.tmp\" '{}'/\"$DIRIJOR_SESSION_ID\"\n\
             exec sleep 60\n",
            out.display(),
            out.display(),
            out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn launch(engine: &Engine, cli: &str, cwd: &Path, out: &Path) -> Vec<String> {
    let bridge = dirijor_mcp::Bridge::new(engine.server.socket_path().to_owned(), None);
    bridge
        .request(
            "agent.configure",
            json!({"kind": diri_proto::AgentKind::CLAUDE_CODE, "executablePath": cli, "showInQuickCreate": true}),
            Duration::from_secs(10),
        )
        .unwrap();
    let record = bridge
        .request(
            "session.spawn",
            json!({"kind": diri_proto::AgentKind::CLAUDE_CODE, "cwd": cwd}),
            Duration::from_secs(30),
        )
        .unwrap();
    let id = record["id"].as_str().unwrap().to_owned();
    let file = out.join(&id);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let recorded = std::fs::read_to_string(&file).expect("the fake CLI ran");
    engine
        .registry
        .lock()
        .unwrap()
        .terminate(&id, Duration::from_secs(2))
        .unwrap();
    recorded.lines().map(str::to_owned).collect()
}

fn mcp_config(argv: &[String]) -> &str {
    let at = argv.iter().position(|arg| arg == "--mcp-config").unwrap();
    argv[at + 1].rsplit('/').next().unwrap()
}

#[test]
fn only_a_verified_cli_release_is_pointed_at_the_endpoint() {
    let engine = start();
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let cwd = work.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();

    // Below the minimum: main's stdio config, and no token in its environment.
    let old = fake_cli(work.path(), "claude-old", "2.1.288 (Claude Code)", &out);
    for _ in 0..2 {
        let argv = launch(&engine, &old, &cwd, &out);
        assert_eq!(mcp_config(&argv), "claude-mcp.json", "{argv:?}");
        assert_eq!(argv.last().unwrap(), "TOKEN=", "{argv:?}");
    }

    // At the minimum: the first launch finds the version unknown and keeps
    // stdio without waiting for the probe; once probed, it gets HTTP.
    let new = fake_cli(work.path(), "claude-new", "2.1.289 (Claude Code)", &out);
    let first = launch(&engine, &new, &cwd, &out);
    assert_eq!(mcp_config(&first), "claude-mcp.json", "{first:?}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let argv = loop {
        let argv = launch(&engine, &new, &cwd, &out);
        if mcp_config(&argv) != "claude-mcp.json" || Instant::now() > deadline {
            break argv;
        }
    };
    assert_eq!(mcp_config(&argv), "claude-mcp-http.json", "{argv:?}");
    assert_eq!(argv.last().unwrap(), "TOKEN=set", "{argv:?}");
}
