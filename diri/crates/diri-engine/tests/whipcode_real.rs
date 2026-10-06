//! Opt-in: the real WhipCode TUI driven through a private Engine.
//!
//! Nothing here needs a provider account. WhipCode is configured with a
//! custom `openai-completions` provider pointing at
//! `fixtures/fake_openai_api.py` (spawned on a free 127.0.0.1 port), which
//! scripts slow streams, a Starlark `shell.run` and a `user.ask` question
//! from keywords in the prompt. The same server is WhipCode's HTTP(S) proxy
//! and logs then refuses every other host. HOME (and so `~/.whipcode`, its
//! daemon socket and session store), the project and the Engine socket live
//! in a temp dir; the developer's `~/.whipcode` is never read or written.
//!
//! WhipCode starts a detached `whipcode _daemon` on first launch. The
//! fixture stops it with `whipcode daemon stop` (same HOME) on drop.
//!
//! `DIRI_WHIPCODE_BIN_DIR` must hold a `whipcode` executable, and `python3`
//! must be on PATH. The official installer takes a destination:
//!
//! ```sh
//! curl -fsSL https://raw.githubusercontent.com/context-labs/whip/main/install.sh \
//!   | WHIPCODE_BIN_DIR=/tmp/whip/bin sh   # note: it also appends that dir to ~/.zshrc, ~/.bashrc and ~/.profile
//! DIRI_WHIPCODE_BIN_DIR=/tmp/whip/bin \
//!   cargo test -p diri-engine --test whipcode_real -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The tests set process-wide environment the Engine hands to its children,
//! so they must run with `--test-threads=1`.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use serde_json::{Value, json};

#[path = "support/teardown.rs"]
mod teardown;

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            if let ControlMessage::Response { id, result, .. } =
                serde_json::from_str::<ControlMessage>(&line).unwrap()
                && id == self.next
            {
                return result.map_err(|error| format!("{error:?}"));
            }
        }
    }

    fn status(&mut self, id: &str) -> String {
        let list = self.call("session.list", json!({})).unwrap();
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == id)
            .map(|record| record["status"].to_string())
            .unwrap_or_default()
    }

    fn screen(&mut self, id: &str) -> String {
        self.call("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    fn send(&mut self, id: &str, text: &str, submit: bool) {
        self.call(
            "session.send_text",
            json!({ "sessionID": id, "text": text, "submit": submit }),
        )
        .expect("send_text");
    }

    /// Samples status every 100 ms until `done` holds or `within` passes,
    /// printing each transition with its screen. Returns every status seen.
    fn watch(
        &mut self,
        id: &str,
        label: &str,
        within: Duration,
        mut done: impl FnMut(&str, &str) -> bool,
    ) -> Vec<String> {
        let start = Instant::now();
        let mut seen: Vec<String> = Vec::new();
        loop {
            let status = self.status(id);
            let screen = self.screen(id);
            if seen.last() != Some(&status) {
                println!(
                    "[{label} +{}ms] {status}\n{}",
                    start.elapsed().as_millis(),
                    indent(&screen)
                );
                seen.push(status.clone());
            }
            if done(&status, &screen) || start.elapsed() > within {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn indent(screen: &str) -> String {
    screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| format!("    | {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Fixture {
    temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    whipcode: PathBuf,
    api: Child,
}

impl Fixture {
    fn requests(&self) -> String {
        std::fs::read_to_string(self.temp.path().join("api.log")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The daemon is detached (setsid) and outlives every tab; stop it
        // before its HOME disappears.
        teardown::stop_daemon(
            &self.whipcode,
            &["daemon", "stop"],
            &self.home,
            "WHIPCODE_HOME",
        );
        let _ = self.api.kill();
        let _ = self.api.wait();
        teardown::sweep(self.temp.path());
        teardown::remove_tree(self.temp.path());
    }
}

/// The fake provider as WhipCode's default (and compaction) model, with the
/// MCP import offer and remote brand icons switched off.
fn config(port: u16) -> String {
    json!({
        "defaultModel": "fake-model",
        "compactModel": "fake-model",
        "providers": {
            "fake": {
                "name": "Fake",
                "baseUrl": format!("http://127.0.0.1:{port}/v1"),
                "api": "openai-completions",
                "auth": "none",
            }
        },
        "models": { "fake-model": { "providers": ["fake"], "context": 128000 } },
        "mcpImport": {
            "claude": { "enabled": false },
            "codex": { "enabled": false },
            "opencode": { "enabled": false },
            "offered": true,
        },
        "brandIcons": false,
    })
    .to_string()
}

/// A private HOME and project, with the Engine's inherited environment
/// pointed at them and at the WhipCode under test. `configured` writes the
/// fake provider config; without it WhipCode starts as on a fresh machine.
/// None when not opted in.
fn fixture(configured: bool) -> Option<Fixture> {
    let Some(bin) = std::env::var_os("DIRI_WHIPCODE_BIN_DIR") else {
        eprintln!("DIRI_WHIPCODE_BIN_DIR unset; skipping");
        return None;
    };
    let whipcode = Path::new(&bin).join("whipcode");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    if configured {
        std::fs::create_dir_all(home.join(".whipcode")).unwrap();
        std::fs::write(home.join(".whipcode/config.json"), config(port)).unwrap();
    } else {
        std::fs::create_dir_all(&home).unwrap();
    }
    let api = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_openai_api.py"))
        .arg(port.to_string())
        .arg(temp.path().join("api.log"))
        .spawn()
        .expect("python3 for the fake OpenAI-compatible API");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "fake API never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    let path = format!(
        "{}:{}",
        Path::new(&bin).display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let proxy = format!("http://127.0.0.1:{port}");
    // SAFETY: --test-threads=1, and set before the Engine spawns anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            std::env::set_var(name, &proxy);
        }
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        for name in [
            "WHIPCODE_HOME",
            "WHIP_THEME",
            "INFERENCE_API_KEY",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            std::env::remove_var(name);
        }
    }
    Some(Fixture {
        temp,
        home,
        project,
        whipcode,
        api,
    })
}

fn start(temp: &Path) -> Client {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .unwrap();
    let (engine, _) = ManifestEngine::load_dir(&dir).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs")),
    );
    let listener = server.bind().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let stream = UnixStream::connect(temp.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        next: 0,
    }
}

fn spawn(
    client: &mut Client,
    project: &Path,
    prompt: Option<&str>,
) -> (String, Result<(), String>) {
    let mut params = json!({
        "kind": { "whipcode": {} },
        "cwd": project,
        "initialCols": 120,
        "initialRows": 36,
    });
    if let Some(prompt) = prompt {
        params["initialPrompt"] = json!(prompt);
    }
    match client.call("session.spawn", params) {
        Ok(record) => (record["id"].as_str().unwrap().to_string(), Ok(())),
        Err(error) => {
            let list = client.call("session.list", json!({})).unwrap();
            let id = list["sessions"][0]["id"].as_str().unwrap().to_string();
            (id, Err(error))
        }
    }
}

/// Records a failure unless the last status a watch saw was idle.
fn settled(failures: &mut Vec<String>, what: &str, seen: &[String]) {
    if !seen.last().is_some_and(|status| status.contains("idle")) {
        failures.push(format!("{what} never settled idle: {seen:?}"));
    }
}

/// Initial prompt, a streamed turn, a shell call behind WhipCode's
/// permission dialog answered with the manifest's Approve and then its Deny,
/// and a `user.ask` question.
#[test]
#[ignore = "needs DIRI_WHIPCODE_BIN_DIR and a real WhipCode"]
fn prompts_stream_ask_permission_and_question() {
    let Some(fixture) = fixture(true) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let mut failures = Vec::new();

    let (id, delivered) = spawn(
        &mut client,
        &fixture.project,
        Some("Reply with the word PINEAPPLE please."),
    );
    if let Err(error) = delivered {
        failures.push(format!("initial prompt: {error}"));
    }
    // The transcript echoes the prompt, so the reply is the second PINEAPPLE.
    let seen = client.watch(&id, "initial", Duration::from_secs(30), |status, screen| {
        status.contains("idle") && screen.matches("PINEAPPLE").count() >= 2
    });
    settled(&mut failures, "the initial turn", &seen);
    if !fixture
        .requests()
        .contains("tools=True last_user='Reply with the word PINEAPPLE please.'")
    {
        failures.push(format!(
            "the initial prompt never reached the API:\n{}",
            fixture.requests()
        ));
    }

    client.send(&id, "SLOW stream something", true);
    let seen = client.watch(&id, "stream", Duration::from_secs(20), |status, screen| {
        status.contains("idle") && screen.contains("SLOWDONE")
    });
    if !seen.iter().any(|status| status.contains("working")) {
        failures.push(format!("a streamed turn never read as working: {seen:?}"));
    }
    settled(&mut failures, "the streamed turn", &seen);

    // Deny first: Esc rejects without a reason and the file never appears.
    client.send(&id, "RUNCMD for me", true);
    let seen = client.watch(&id, "permission", Duration::from_secs(15), |status, _| {
        status.contains("needsInput")
    });
    if !seen.iter().any(|status| status.contains("permission")) {
        failures.push(format!(
            "the shell permission dialog never read as a permission: {seen:?}"
        ));
    }
    client.send(&id, "\u{1b}", false);
    let seen = client.watch(&id, "denied", Duration::from_secs(15), |status, screen| {
        status.contains("idle") && screen.contains("DONECMD")
    });
    settled(&mut failures, "the denied turn", &seen);
    if fixture.project.join("diri-e2e-file").exists() {
        failures.push("denying still ran the shell command".into());
    }
    if !fixture
        .requests()
        .contains("TOOL_RESULT 'Error: Permission denied: rejected without a reason")
    {
        failures.push(format!(
            "Esc did not reject the shell call:\n{}",
            fixture.requests()
        ));
    }

    // Then approve with the manifest's keystroke: a raw `a`.
    client.send(&id, "RUNCMD again", true);
    let seen = client.watch(&id, "permission-2", Duration::from_secs(15), |status, _| {
        status.contains("needsInput")
    });
    if !seen.iter().any(|status| status.contains("permission")) {
        failures.push(format!(
            "the second permission dialog never read as a permission: {seen:?}"
        ));
    }
    client.send(&id, "a", false);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !fixture.project.join("diri-e2e-file").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if !fixture.project.join("diri-e2e-file").exists() {
        failures.push("approving did not run the shell command".into());
    }
    let seen = client.watch(&id, "approved", Duration::from_secs(15), |status, _| {
        status.contains("idle")
    });
    settled(&mut failures, "the approved turn", &seen);

    client.send(&id, "ASKQ which database", true);
    let seen = client.watch(&id, "question", Duration::from_secs(15), |status, _| {
        status.contains("needsInput")
    });
    if !seen
        .last()
        .is_some_and(|status| status.contains("question"))
    {
        failures.push(format!("user.ask never read as a question: {seen:?}"));
    }
    client.send(&id, "\r", false);
    let seen = client.watch(&id, "answered", Duration::from_secs(15), |status, _| {
        status.contains("idle")
    });
    settled(&mut failures, "the answered question", &seen);

    let _ = client.call("session.kill", json!({ "sessionID": id }));
    println!("requests:\n{}", fixture.requests());
    assert!(
        failures.is_empty(),
        "failures:\n  {}",
        failures.join("\n  ")
    );
}

/// A fresh machine with no provider configured: WhipCode opens its provider
/// picker over the composer. Text typed now would land in the picker's search
/// box, so the session must wait for the user instead of reading as idle.
#[test]
#[ignore = "needs DIRI_WHIPCODE_BIN_DIR and a real WhipCode"]
fn first_run_provider_picker_waits_for_the_user() {
    let Some(fixture) = fixture(false) else {
        return;
    };
    let mut client = start(fixture.temp.path());
    let (id, _) = spawn(&mut client, &fixture.project, None);
    let seen = client.watch(&id, "first-run", Duration::from_secs(20), |status, _| {
        status.contains("needsInput")
    });
    let screen = client.screen(&id);
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    assert!(
        screen.contains("Connect a provider"),
        "WhipCode did not open its provider picker; the scenario is stale:\n{}",
        indent(&screen)
    );
    assert!(
        seen.last()
            .is_some_and(|status| status.contains("question")),
        "the provider picker never read as a question: {seen:?}"
    );
}
