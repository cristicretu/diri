#![cfg(unix)]
//! `dirijor do`, `dirijor actions` and `dirijor api` against a real Engine,
//! with a scripted app connection standing in for the Diri window.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use diri_engine::{ControlServer, ManifestEngine, Registry};
use diri_proto::{EventName, Method};
use serde_json::{Value, json};

struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: PathBuf,
}

impl Server {
    fn start(directory: &Path) -> Self {
        let (engine, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let registry = Registry::new(Arc::new(engine), directory.join("state.json"));
        let socket = directory.join("engine.sock");
        let server = Arc::new(ControlServer::new(Arc::new(Mutex::new(registry)), &socket));
        let listener = server.bind().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let server = server.clone();
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
        Self {
            stop,
            thread: Some(thread),
            socket,
        }
    }

    fn command(&self, words: &[&str], actor: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dirijor"));
        command
            .args(words)
            .env_clear()
            .env("DIRIJOR_SOCKET", &self.socket);
        if let Some(actor) = actor {
            command.env("DIRIJOR_SESSION_ID", actor);
        }
        command.output().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        self.thread.take().unwrap().join().unwrap();
    }
}

/// Publishes a two-action catalog, then answers every perform with the
/// arguments it received. Returns what it was asked.
fn fake_app(socket: &Path) -> (Arc<Mutex<Vec<Value>>>, std::thread::JoinHandle<()>) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let send = move |stream: &mut UnixStream, value: Value| {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        stream.write_all(&line).unwrap();
    };
    send(
        &mut stream,
        json!({"id": 1, "method": Method::APP_ACTIONS_PUBLISH, "params": {
            "build": "fake",
            "actions": [
                {"name": "toggle-sidebar", "title": "Toggle Sidebar", "args": {"type":"object","properties":{},"additionalProperties":false}},
                {"name": "select-tab", "title": "Select Tab", "args": {"type":"object","properties":{"index":{"type":"integer","minimum":-1,"maximum":9}},"required":["index"],"additionalProperties":false}}
            ]
        }}),
    );
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"registered\":2"), "{line}");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let record = asked.clone();
    let worker = std::thread::spawn(move || {
        let mut next = 10;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["event"] != EventName::APP_PERFORM_REQUESTED {
                continue;
            }
            let request = message["params"].clone();
            record.lock().unwrap().push(request.clone());
            next += 1;
            send(
                &mut stream,
                json!({"id": next, "method": Method::APP_REPLY, "params": {
                    "requestId": request["requestId"],
                    "result": {"ran": request["action"], "args": request["args"]}
                }}),
            );
        }
    });
    (asked, worker)
}

#[test]
fn the_cli_runs_app_actions_and_describes_the_api() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start(temp.path());

    // Before any app connects: a clear failure, not a hang.
    let offline = server.command(&["do", "toggle-sidebar"], None);
    assert!(!offline.status.success());
    assert!(
        String::from_utf8_lossy(&offline.stderr).contains("no Diri app is connected"),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );

    let (asked, _worker) = fake_app(&server.socket);
    let listed = server.command(&["actions"], None);
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("select-tab"), "{listed}");
    assert!(listed.contains("(index)"), "{listed}");

    // KEY=VALUE parses JSON values; an Agent's shell names it as the actor.
    let ran = server.command(&["do", "select-tab", "index=2", "--json"], Some("agent-7"));
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let ran: Value = serde_json::from_slice(&ran.stdout).unwrap();
    assert_eq!(ran, json!({"ran": "select-tab", "args": {"index": 2}}));
    let request = asked.lock().unwrap()[0].clone();
    assert_eq!(request["actor"], "agent-7");

    // Arguments are checked before the app is asked.
    let bad = server.command(&["do", "select-tab", "index=12"], None);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("invalid_args"));
    assert_eq!(asked.lock().unwrap().len(), 1);

    let api = server.command(&["api", "list", "--json"], None);
    let api: Value = serde_json::from_slice(&api.stdout).unwrap();
    assert_eq!(api["methods"].as_array().unwrap().len(), Method::ALL.len());
    assert_eq!(api["appConnected"], true);
    let describe = server.command(&["api", "describe", "app.perform", "--json"], None);
    let describe: Value = serde_json::from_slice(&describe.stdout).unwrap();
    assert_eq!(describe["method"]["params"]["required"], json!(["action"]));
    let llms = server.command(&["api", "llms"], None);
    let llms = String::from_utf8_lossy(&llms.stdout);
    assert!(llms.contains("- app.perform(action, "), "{llms}");
    assert!(llms.contains("- select-tab(index)"), "{llms}");
}
