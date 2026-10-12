//! Opt-in Engine -> real Cursor CLI -> scripted localhost Cursor backend.
//!
//! Verified with 2026.09.28-64d2043 and 2026.10.01-e373342. Extract the official platform tarball into
//! a scratch directory (do not install globally), then run from `diri/`:
//!
//! ```sh
//! DIRI_CURSOR_BIN_DIR=/tmp/cursor-cli CARGO_TARGET_DIR=/tmp/cursor-target \
//!   cargo test -p diri-engine --test cursor_real -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Needs python3. The private ControlServer/Registry, HOME, XDG directories,
//! project and Cursor config/data are temporary. The child environment is
//! replaced and restored, credentials stay in Cursor's memory store, and
//! NO_OPEN_BROWSER prevents windows. A wrapper disables updates and indexing;
//! the unmodified CLI uses CURSOR_API_ENDPOINT and HTTP/1.1 Connect RPCs.
//! The fixture also refuses non-loopback proxy requests. No account or real
//! model is used, including the simulated browser-login completion.
//!
//! The server sends real conversation blobs/checkpoints, so resume assertions
//! verify the CLI's persisted history, not text echoed by the fixture.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::{Value, json};

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
    engine: Option<EngineThreads>,
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
        self.writer.write_all(&bytes).map_err(|e| e.to_string())?;
        loop {
            let mut line = String::new();
            if self
                .reader
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("Engine closed control socket".into());
            }
            if let ControlMessage::Response { id, result, .. } =
                serde_json::from_str::<ControlMessage>(&line).map_err(|e| e.to_string())?
                && id == self.next
            {
                return result.map_err(|error| format!("{error:?}"));
            }
        }
    }

    fn record(&mut self, id: &str) -> Value {
        self.call("session.list", json!({})).unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == id)
            .expect("session exists")
            .clone()
    }

    fn status(&mut self, id: &str) -> String {
        self.record(id)["status"].to_string()
    }

    fn screen(&mut self, id: &str) -> String {
        self.call("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    fn key(&mut self, id: &str, text: &str, submit: bool) {
        self.call(
            "session.send_text",
            json!({"sessionID":id,"text":text,"submit":submit}),
        )
        .unwrap();
    }

    fn send(&mut self, id: &str, text: &str) {
        self.call(
            "session.send_text",
            json!({ "sessionID": id, "text": text, "submit": true }),
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
                println!("[{label} final] {status}\n{}", indent(&screen));
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

struct EngineThreads {
    stop: Arc<AtomicBool>,
    socket: PathBuf,
    server: std::thread::JoinHandle<()>,
    watcher: std::thread::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let Some(engine) = self.engine.take() else {
            return;
        };
        // Also stop every test session on assertion failure.
        if let Ok(list) = self.call("session.list", json!({})) {
            for record in list["sessions"].as_array().into_iter().flatten() {
                let _ = self.call("session.kill", json!({"sessionID":record["id"]}));
            }
        }
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
        engine.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&engine.socket);
        let _ = engine.server.join();
        let _ = engine.watcher.join();
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    project: PathBuf,
    api: Child,
    saved_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.api.kill();
        let _ = self.api.wait();
        // SAFETY: all Engine and CLI threads/children are stopped before Fixture drops.
        unsafe {
            for (key, _) in std::env::vars_os() {
                std::env::remove_var(key);
            }
            for (key, value) in &self.saved_env {
                std::env::set_var(key, value);
            }
        }
    }
}

fn fixture(auth: bool) -> Fixture {
    let bin = std::env::var_os("DIRI_CURSOR_BIN_DIR").expect("set DIRI_CURSOR_BIN_DIR");
    assert!(Path::new(&bin).join("cursor-agent").is_file());
    let temp = tempfile::tempdir_in(Path::new("/tmp").canonicalize().unwrap()).unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_cursor_api.py"))
        .arg(port.to_string())
        .arg(temp.path().join("api.log"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::write(
        home.join(".cursor/cli-config.json"),
        json!({
            "version":1,"editor":{"vimMode":false},"permissions":{"allow":[],"deny":[]},
            "network":{"useHttp1ForAgent":true}, "channel":"static"
        })
        .to_string(),
    )
    .unwrap();
    let wrapper = temp.path().join("bin");
    std::fs::create_dir_all(&wrapper).unwrap();
    let executable = Path::new(&bin).join("cursor-agent").canonicalize().unwrap();
    let quoted = executable.to_str().unwrap().replace('\'', "'\\''");
    std::fs::write(wrapper.join("cursor-agent"), format!("#!/bin/sh\nexec '{quoted}' --disable-auto-update --disable-indexing --disable-codebase-ref \"$@\"\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        wrapper.join("cursor-agent"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut values = vec![
        ("TERM", Some("xterm-256color".into())),
        ("LANG", Some("en_US.UTF-8".into())),
        (
            "SHELL",
            Some(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())),
        ),
        (
            "HTTP_PROXY",
            Some(format!("http://127.0.0.1:{port}").into()),
        ),
        (
            "HTTPS_PROXY",
            Some(format!("http://127.0.0.1:{port}").into()),
        ),
        ("NO_PROXY", Some("127.0.0.1,localhost".into())),
        (
            "CURSOR_API_ENDPOINT",
            Some(format!("http://127.0.0.1:{port}").into()),
        ),
        (
            "CURSOR_WEBSITE_URL",
            Some(format!("http://127.0.0.1:{port}").into()),
        ),
        ("HOME", Some(home.clone().into_os_string())),
        (
            "PATH",
            Some(format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap()).into()),
        ),
        ("CURSOR_API_KEY", auth.then(|| "fake-diri-key".into())),
        ("CURSOR_AUTH_TOKEN", None),
        ("AGENT_CLI_CREDENTIAL_STORE", Some("memory".into())),
        ("NO_OPEN_BROWSER", Some("1".into())),
        (
            "CURSOR_CONFIG_DIR",
            Some(home.join(".cursor").into_os_string()),
        ),
        (
            "CURSOR_DATA_DIR",
            Some(temp.path().join("home/.cursor").into_os_string()),
        ),
    ];
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        let path = temp.path().join(key);
        std::fs::create_dir_all(&path).unwrap();
        values.push((key, Some(path.into_os_string())));
    }
    let saved_env = std::env::vars_os().collect();
    // SAFETY: opt-in, serial test binary, before starting any Engine thread.
    unsafe {
        for (key, _) in std::env::vars_os() {
            std::env::remove_var(key);
        }
        for (key, value) in values {
            if let Some(value) = value {
                std::env::set_var(key, value);
            }
        }
    }
    Fixture {
        temp,
        project,
        saved_env,
        api,
    }
}

fn connect(socket: &Path) -> Client {
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(90)))
        .unwrap();
    Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        next: 0,
        engine: None,
    }
}

fn start(temp: &Path) -> Client {
    start_with(temp, false)
}

/// `held` runs sessions through a real Holder, as the app does.
fn start_with(temp: &Path, held: bool) -> Client {
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    let socket = temp.join("daemon.sock");
    let mut server =
        ControlServer::new(Arc::clone(&registry), socket.clone()).with_logs_dir(temp.join("logs"));
    if held {
        server = server.with_holder(HolderConfig {
            holders_dir: temp.join("holders"),
            executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
        });
    }
    let server = Arc::new(server);
    let listener = server.bind().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher =
        diri_engine::events::spawn_registry_watcher(registry, server.events(), stop.clone());
    let stopped = stop.clone();
    let server_thread = std::thread::spawn(move || {
        let mut clients = Vec::new();
        for stream in listener.incoming().flatten() {
            if stopped.load(Ordering::SeqCst) {
                break;
            }
            let server = server.clone();
            clients.push(std::thread::spawn(move || {
                let _ = server.serve(stream);
            }));
        }
        for client in clients {
            let _ = client.join();
        }
    });
    let mut client = connect(&socket);
    client.engine = Some(EngineThreads {
        stop,
        socket,
        server: server_thread,
        watcher,
    });
    client
}

impl Fixture {
    fn requests(&self) -> Vec<Value> {
        let log = std::fs::read_to_string(self.temp.path().join("api.log")).unwrap();
        println!("Backend requests:\n{log}");
        log.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

fn expect_screen(
    client: &mut Client,
    id: &str,
    label: &str,
    status: &str,
    text: &str,
) -> Vec<String> {
    let seen = client.watch(id, label, Duration::from_secs(15), |s, screen| {
        s.contains(status) && screen.contains(text)
    });
    if !(client.status(id).contains(status) && client.screen(id).contains(text)) {
        let record = client.record(id);
        println!("Unexpected status evidence: {}", record["statusEvidence"]);
        if let Some(path) = record["transcriptPath"].as_str() {
            println!(
                "Fixture transcript: {}",
                std::fs::read_to_string(path).unwrap()
            );
        }
    }
    assert!(
        client.status(id).contains(status) && client.screen(id).contains(text),
        "{label}: expected {status} and {text:?}, saw {seen:?}\n{}",
        client.screen(id)
    );
    seen
}

fn spawn(
    client: &mut Client,
    project: &Path,
    prompt: Option<&str>,
) -> (String, Result<(), String>) {
    spawn_sized(client, project, prompt, 30)
}

fn spawn_sized(
    client: &mut Client,
    project: &Path,
    prompt: Option<&str>,
    rows: u16,
) -> (String, Result<(), String>) {
    let mut params = json!({
        "kind": { "cursor": {} },
        "cwd": project,
        "initialCols": 100,
        "initialRows": rows,
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

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn first_run_and_browser_login_need_input() {
    let fixture = fixture(false);
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, None);
    assert!(result.is_ok(), "{result:?}");
    expect_screen(
        &mut client,
        &id,
        "first-run",
        "needsInput",
        "Press any key to log in...",
    );
    client.key(&id, " ", false);
    expect_screen(
        &mut client,
        &id,
        "login",
        "needsInput",
        "If your browser didn't open",
    );
    assert!(client.screen(&id).contains("http://127.0.0.1:"));
    assert!(fixture.requests().is_empty());
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn workspace_trust_needs_input_until_answered() {
    let fixture = fixture(true);
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, None);
    assert!(result.is_ok(), "{result:?}");
    expect_screen(
        &mut client,
        &id,
        "trust",
        "needsInput",
        "Workspace Trust Required",
    );
    assert!(fixture.requests().is_empty());
    client.key(&id, "a", false);
    expect_screen(
        &mut client,
        &id,
        "trusted",
        "idle",
        "→ Plan, search, build anything",
    );
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn initial_prompt_survives_trust() {
    let fixture = fixture(true);
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, Some("PINEAPPLE initial"));
    assert!(result.is_ok(), "{result:?}\n{}", client.screen(&id));
    expect_screen(&mut client, &id, "initial", "idle", "PINEAPPLE\n");
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["prompt"], "PINEAPPLE initial");
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn initial_prompt_waits_for_simulated_login_then_survives_trust() {
    let fixture = fixture(false);
    let mut client = start(fixture.temp.path());
    let mut spawning = connect(&fixture.temp.path().join("daemon.sock"));
    let project = fixture.project.clone();
    let spawn_thread =
        std::thread::spawn(move || spawn(&mut spawning, &project, Some("PINEAPPLE after login")));
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = loop {
        let list = client.call("session.list", json!({})).unwrap();
        if let Some(id) = list["sessions"][0]["id"].as_str() {
            break id.to_owned();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    };
    expect_screen(
        &mut client,
        &id,
        "onboarding-with-prompt",
        "needsInput",
        "Press any key to log in...",
    );
    // The injector must not treat the prompt as the onboarding's "any key".
    std::thread::sleep(Duration::from_millis(1800));
    assert!(client.screen(&id).contains("Press any key to log in..."));
    client.key(&id, " ", false);
    expect_screen(
        &mut client,
        &id,
        "browser-with-prompt",
        "needsInput",
        "If your browser didn't open",
    );
    // Simulates completion entirely on localhost with memory-only fake credentials.
    std::fs::write(fixture.temp.path().join("allow-login"), "").unwrap();
    let (spawned_id, result) = spawn_thread.join().unwrap();
    assert_eq!(spawned_id, id);
    assert!(result.is_ok(), "{result:?}\n{}", client.screen(&id));
    expect_screen(&mut client, &id, "after-login", "idle", "PINEAPPLE\n");
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["prompt"], "PINEAPPLE after login");
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn prompts_stream_approve_and_resume_exact_conversation() {
    let fixture = fixture(true);
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, Some("PINEAPPLE first"));
    assert!(result.is_ok(), "{result:?}");
    expect_screen(&mut client, &id, "initial", "idle", "PINEAPPLE\n");
    client.send(&id, "SLOW second");
    expect_screen(&mut client, &id, "streaming", "working", "slow part ");
    // Submission itself briefly sets Working; require it during actual output,
    // past the idle debounce, so that transient cannot hide a broken spinner rule.
    std::thread::sleep(Duration::from_secs(1));
    assert!(client.status(&id).contains("working"));
    assert!(!client.screen(&id).contains("SLOWDONE"));
    expect_screen(&mut client, &id, "stream-complete", "idle", "SLOWDONE");
    client.send(&id, "RUNCMD third");
    expect_screen(
        &mut client,
        &id,
        "permission",
        "permission",
        "Run this command?",
    );
    assert!(
        !fixture.project.join("diri-e2e-file").exists(),
        "tool ran before approval"
    );
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let approve = engine
        .manifest("cursor")
        .unwrap()
        .agent
        .as_ref()
        .unwrap()
        .approve
        .as_ref()
        .unwrap();
    assert_eq!(approve.text.as_deref(), Some("y"));
    assert!(!approve.submit);
    client.key(
        &id,
        approve.text.as_deref().unwrap_or_default(),
        approve.submit,
    );
    expect_screen(&mut client, &id, "approved", "idle", "DONECMD");
    assert!(
        fixture.project.join("diri-e2e-file").exists(),
        "approved tool did not execute"
    );
    let requests = fixture.requests();
    assert_eq!(
        requests
            .iter()
            .map(|v| v["prompt"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["PINEAPPLE first", "SLOW second", "RUNCMD third"]
    );
    let conversation = requests[0]["conversation"].clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.record(&id)["agentSessionID"] != conversation {
        assert!(
            Instant::now() < deadline,
            "Engine did not discover native id: {}",
            client.record(&id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    client
        .call("session.kill", json!({"sessionID":id}))
        .unwrap();
    expect_screen(&mut client, &id, "killed", "exited", "");
    // Put a newer conversation in the same project. Exact resume must not pick it.
    let (other, result) = spawn(&mut client, &fixture.project, Some("DECOY newer"));
    assert!(result.is_ok(), "{result:?}");
    expect_screen(&mut client, &other, "decoy", "idle", "DECOY\n");
    client
        .call("session.kill", json!({"sessionID":other}))
        .unwrap();
    client
        .call("session.resume", json!({"sessionID":id}))
        .unwrap();
    expect_screen(&mut client, &id, "resumed", "idle", "DONECMD");
    client.send(&id, "MANGO fourth");
    expect_screen(&mut client, &id, "after-resume", "idle", "MANGO\n");
    let requests = fixture.requests();
    let last = requests.last().unwrap();
    assert_eq!(last["prompt"], "MANGO fourth");
    assert_eq!(last["conversation"], conversation);
    assert_eq!(
        last["history"],
        json!(["PINEAPPLE first", "SLOW second", "RUNCMD third"])
    );
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn help_and_manifest_agree_on_resume_grammar() {
    let _fixture = fixture(false);
    let help = Command::new("cursor-agent").arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("[prompt...]"));
    assert!(help.contains("--resume [chatId]"));
    assert!(help.contains("Resume the latest chat session"));
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let descriptor = engine.manifest("cursor").unwrap().agent.as_ref().unwrap();
    assert_eq!(descriptor.resume_args(None).unwrap(), ["resume"]);
    assert_eq!(
        descriptor.resume_args(Some("native-id")).unwrap(),
        ["--resume", "native-id"]
    );
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn slow_stream_without_initial_prompt_stays_working() {
    let fixture = fixture(true);
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, None);
    assert!(result.is_ok(), "{result:?}");
    expect_screen(
        &mut client,
        &id,
        "trust",
        "needsInput",
        "Workspace Trust Required",
    );
    client.key(&id, "a", false);
    expect_screen(
        &mut client,
        &id,
        "ready",
        "idle",
        "→ Plan, search, build anything",
    );
    client.send(&id, "SLOW only");
    expect_screen(&mut client, &id, "streaming", "working", "slow part ");
    std::thread::sleep(Duration::from_secs(1));
    assert!(client.status(&id).contains("working"));
    assert!(!client.screen(&id).contains("SLOWDONE"));
    expect_screen(&mut client, &id, "stream-complete", "idle", "SLOWDONE");
    assert_eq!(fixture.requests()[0]["prompt"], "SLOW only");
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn unattended_login_reports_delivery_failure_without_consuming_the_prompt() {
    let fixture = fixture(false);
    let mut client = start(fixture.temp.path());
    let (id, result) = spawn(&mut client, &fixture.project, Some("PINEAPPLE retained"));
    assert!(
        result
            .unwrap_err()
            .contains("initial_prompt_delivery_failed")
    );
    expect_screen(
        &mut client,
        &id,
        "unattended-login",
        "needsInput",
        "Press any key to log in...",
    );
    assert_eq!(
        client.record(&id)["originatingPrompt"],
        "PINEAPPLE retained"
    );
    assert!(fixture.requests().is_empty());
}

/// One published grid as an attached pane saw it.
struct Shot {
    at: Instant,
    cols: u16,
    rows: Vec<String>,
}

/// What an attached pane saw: every published grid, in order.
struct Recorder {
    input: UnixStream,
    frames: Arc<Mutex<Vec<Shot>>>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    fn attach(client: &Client, id: &str) -> Self {
        let socket = &client.engine.as_ref().unwrap().socket;
        let mut stream = UnixStream::connect(socket).unwrap();
        writeln!(stream, "{}", json!({ "attach": id })).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let input = stream.try_clone().unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let (frames, done) = (frames.clone(), done.clone());
            std::thread::spawn(move || {
                use std::io::Read;
                let mut codec = FrameCodec::new();
                let mut rows: Vec<String> = Vec::new();
                let mut chunk = vec![0u8; 256 << 10];
                while !done.load(Ordering::SeqCst) {
                    let count = match stream.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(count) => count,
                        Err(_) => continue,
                    };
                    for frame in codec.feed(&chunk[..count]).unwrap() {
                        if frame.frame_type != FrameType::Grid {
                            continue;
                        }
                        let Ok(Some(update)) = frame.grid_payload() else {
                            continue;
                        };
                        if update.is_full_snapshot {
                            rows.clear();
                        }
                        rows.resize(usize::from(update.rows), String::new());
                        for row in &update.changed_rows {
                            if let Some(slot) = rows.get_mut(usize::from(row.y)) {
                                *slot = row
                                    .cells
                                    .iter()
                                    .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                                    .collect::<String>()
                                    .trim_end()
                                    .to_string();
                            }
                        }
                        frames.lock().unwrap().push(Shot {
                            at: Instant::now(),
                            cols: update.cols,
                            rows: rows.clone(),
                        });
                    }
                }
            })
        };
        Self {
            input,
            frames,
            done,
            thread: Some(thread),
        }
    }

    /// Types like a person: one key per data-channel frame, as the app sends.
    fn type_text(&mut self, text: &str, gap: Duration) {
        for key in text.chars() {
            let bytes = key.to_string().into_bytes();
            self.input
                .write_all(&FrameCodec::encode(&Frame::input(bytes)).unwrap())
                .unwrap();
            std::thread::sleep(gap);
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.input
            .write_all(&FrameCodec::encode(&Frame::resize(cols, rows)).unwrap())
            .unwrap();
    }

    fn finish(mut self) -> Vec<Shot> {
        self.stop();
        std::mem::take(&mut *self.frames.lock().unwrap())
    }

    fn stop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = self.input.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Recorder {
    // The Engine joins every client connection on shutdown, so an attach left
    // open by a failed assertion would hang the test instead of failing it.
    fn drop(&mut self) {
        self.stop();
    }
}

fn filled(rows: &[String]) -> usize {
    rows.iter()
        .map(|row| row.chars().filter(|c| !c.is_whitespace()).count())
        .sum()
}

/// Frames a pane painted as a flicker: a row whose content vanished for one
/// frame and came straight back, or a frame holding a whole row less than
/// both neighbours. Either is the erased half of a repaint published alone.
fn flashes(frames: &[Shot]) -> Vec<String> {
    let mut found = Vec::new();
    for i in 1..frames.len().saturating_sub(1) {
        let (before, now, after) = (&frames[i - 1].rows, &frames[i].rows, &frames[i + 1].rows);
        // Across a resize the layout legitimately changes and changes back.
        let cols = frames[i].cols;
        if before.len() != now.len()
            || now.len() != after.len()
            || frames[i - 1].cols != cols
            || frames[i + 1].cols != cols
        {
            continue;
        }
        let cols = usize::from(cols);
        let dip = filled(now) + cols <= filled(before).min(filled(after));
        // A row that shows something else for exactly one frame. A full
        // clear published mid-rewrite shows other conversation rows here,
        // not blanks.
        let blinked: Vec<usize> = (0..now.len())
            .filter(|&y| !before[y].is_empty() && before[y] == after[y] && now[y] != before[y])
            .collect();
        if dip || blinked.len() >= 2 || blinked.iter().any(|&y| now[y].is_empty()) {
            found.push(format!(
                "frame {i} (+{}ms after previous): filled {} -> {} -> {}, blinked rows {blinked:?}\n{}",
                frames[i].at.duration_since(frames[i - 1].at).as_millis(),
                filled(before),
                filled(now),
                filled(after),
                indent(&now.join("\n")),
            ));
        }
    }
    found
}

#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn held_cursor_never_publishes_a_half_erased_repaint() {
    never_publishes_a_half_erased_repaint(30);
}

/// A pane shorter than Cursor's live region (a split, a small window) makes
/// every repaint clear the screen and scrollback and rewrite the whole
/// conversation, which spans many PTY writes.
#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn short_held_cursor_never_publishes_a_half_erased_repaint() {
    never_publishes_a_half_erased_repaint(10);
}

fn never_publishes_a_half_erased_repaint(rows: u16) {
    // Read before the fixture replaces the environment.
    let dump = std::env::var_os("DIRI_FLICKER_DUMP");
    let fixture = fixture(true);
    let mut client = start_with(fixture.temp.path(), true);
    let (id, result) = spawn_sized(&mut client, &fixture.project, None, rows);
    assert!(result.is_ok(), "{result:?}");
    expect_screen(
        &mut client,
        &id,
        "trust",
        "needsInput",
        "Trust this workspace",
    );
    client.key(&id, "a", false);
    expect_screen(
        &mut client,
        &id,
        "ready",
        "idle",
        "→ Plan, search, build anything",
    );

    let mut recorder = Recorder::attach(&client, &id);
    std::thread::sleep(Duration::from_millis(500));
    for turn in 0..3 {
        recorder.type_text(
            &format!("TALL answer number {turn} please\r"),
            Duration::from_millis(35),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let answered = || {
            std::fs::read_to_string(fixture.temp.path().join("api.log"))
                .unwrap_or_default()
                .matches("\"prompt\"")
                .count()
                > turn
        };
        // The finished answer may have scrolled off a short pane, so the turn
        // is done once the backend saw it and Cursor has stayed idle a while.
        let mut idle_since: Option<Instant> = None;
        let mut statuses: Vec<String> = Vec::new();
        loop {
            let status = client.status(&id);
            if statuses.last() != Some(&status) {
                statuses.push(status.clone());
            }
            if answered() && status.contains("idle") {
                if idle_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(4) {
                    println!("turn {turn} statuses: {statuses:?}");
                    break;
                }
            } else {
                idle_since = None;
            }
            assert!(
                Instant::now() < deadline,
                "turn {turn} never finished: {}\n{}",
                client.status(&id),
                client.screen(&id)
            );
            // Typing while the answer streams is the other half of the
            // complaint: every key repaints Cursor's whole live region.
            recorder.type_text("x", Duration::from_millis(60));
            recorder.type_text("\x7f", Duration::from_millis(60));
        }
    }
    let frames = recorder.finish();
    if let Some(path) = dump {
        let dump: Vec<Value> = frames
            .iter()
            .map(|shot| json!({"ms": shot.at.duration_since(frames[0].at).as_millis(), "cols": shot.cols, "rows": shot.rows}))
            .collect();
        std::fs::write(
            Path::new(&path).join(format!("frames-{rows}.json")),
            serde_json::to_string(&dump).unwrap(),
        )
        .unwrap();
    }
    let found = flashes(&frames);
    println!("{} frames published, {} flashes", frames.len(), found.len());
    assert!(
        found.is_empty(),
        "{} of {} published frames were half-erased repaints:\n{}",
        found.len(),
        frames.len(),
        found.join("\n\n")
    );
}

/// Window drags, split panes and the sidebar all resize the pane, and Cursor
/// answers every resize by clearing the screen and scrollback and rewriting
/// the conversation.
#[test]
#[ignore = "needs DIRI_CURSOR_BIN_DIR; run serially"]
fn held_cursor_resizes_without_flashing() {
    let dump = std::env::var_os("DIRI_FLICKER_DUMP");
    let fixture = fixture(true);
    let mut client = start_with(fixture.temp.path(), true);
    let (id, result) = spawn_sized(&mut client, &fixture.project, None, 30);
    assert!(result.is_ok(), "{result:?}");
    expect_screen(
        &mut client,
        &id,
        "trust",
        "needsInput",
        "Trust this workspace",
    );
    client.key(&id, "a", false);
    expect_screen(
        &mut client,
        &id,
        "ready",
        "idle",
        "→ Plan, search, build anything",
    );
    client.send(&id, "TALL history to reflow");
    expect_screen(&mut client, &id, "answered", "idle", "TALLDONE");

    let mut recorder = Recorder::attach(&client, &id);
    std::thread::sleep(Duration::from_millis(500));
    let sizes = [
        (98, 30),
        (100, 30),
        (100, 24),
        (100, 30),
        (90, 20),
        (100, 30),
    ];
    for &(cols, rows) in sizes.iter().cycle().take(18) {
        recorder.resize(cols, rows);
        std::thread::sleep(Duration::from_millis(400));
    }
    let frames = recorder.finish();
    if let Some(path) = dump {
        let dump: Vec<Value> = frames
            .iter()
            .map(|shot| json!({"ms": shot.at.duration_since(frames[0].at).as_millis(), "cols": shot.cols, "rows": shot.rows}))
            .collect();
        std::fs::write(
            Path::new(&path).join("frames-resize.json"),
            serde_json::to_string(&dump).unwrap(),
        )
        .unwrap();
    }
    let found = flashes(&frames);
    println!("{} frames published, {} flashes", frames.len(), found.len());
    assert!(
        found.is_empty(),
        "{} of {} published frames flashed during resizes:\n{}",
        found.len(),
        frames.len(),
        found.join("\n\n")
    );
}
