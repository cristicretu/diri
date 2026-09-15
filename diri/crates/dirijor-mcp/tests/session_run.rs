#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diri_engine::{control::ControlServer, detect::ManifestEngine, registry::Registry};
use diri_proto::{Method, SessionStatus};
use dirijor_mcp::Bridge;
use serde_json::{Value, json};

struct Fixture {
    server: Arc<ControlServer>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let (engine, errors) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        assert!(errors.is_empty());
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(engine),
            temp.path().join("state.json"),
        )));
        let server = Arc::new(ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = diri_engine::events::spawn_registry_watcher(
            registry,
            server.events(),
            Arc::clone(&stop),
        );
        let listener = server.bind().unwrap();
        let accept_stop = Arc::clone(&stop);
        let accept_server = Arc::clone(&server);
        let accept = std::thread::spawn(move || {
            let mut connections = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                if accept_stop.load(Ordering::SeqCst) {
                    break;
                }
                let server = Arc::clone(&accept_server);
                connections.push(std::thread::spawn(move || {
                    let _ = server.serve(stream);
                }));
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            server,
            stop,
            workers: vec![watcher, accept],
            _temp: temp,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dirijor"))
            .env("DIRIJOR_SOCKET", self.server.socket_path())
            .env_remove("DIRIJOR_SESSION_ID")
            .current_dir(self._temp.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(self.server.socket_path());
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[test]
fn literal_command_retains_exit_output_and_supports_event_wait() {
    let fixture = Fixture::new();
    // Only this explicit -c argument is shell syntax. All later arguments must
    // remain literal, including empty strings, option-looking values and Unicode.
    let spawned = fixture.json(&[
        "session",
        "run",
        "--title",
        "finite job",
        "--json",
        "--",
        "/bin/sh",
        "-c",
        "printf '<%s>\\n' \"$@\" > arguments; cat arguments; read finish; printf 'FINAL OUTPUT\\n'; exit 7",
        "fixture",
        "",
        "a b",
        "$(touch injected)",
        "`touch injected-too`",
        "--json",
        "--host",
        "界",
    ]);
    let id = spawned["id"].as_str().unwrap();
    assert_eq!(spawned["title"], "finite job");
    assert!(spawned["parent"].is_null());
    let bridge = Bridge::new(fixture.server.socket_path().into(), None);
    let mut waiter = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .env("DIRIJOR_SOCKET", fixture.server.socket_path())
        .args([
            "session",
            "wait",
            id,
            "--until",
            "exited",
            "--timeout",
            "5",
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The child deliberately waits for input. The CLI must stay pending, then
    // resolve from the exit-bearing status update after input is delivered.
    std::thread::sleep(Duration::from_millis(200));
    assert!(waiter.try_wait().unwrap().is_none());
    bridge
        .request(
            Method::SESSION_SEND_TEXT,
            json!({"sessionID": id, "text": "finish", "submit": true}),
            Duration::from_secs(3),
        )
        .unwrap();
    let waited = waiter.wait_with_output().unwrap();
    assert!(
        waited.status.success(),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let waited: Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["timedOut"], false);
    let status: SessionStatus =
        serde_json::from_value(waited["session"]["status"].clone()).unwrap();
    assert!(matches!(status, SessionStatus::Exited(info) if info.code == Some(7)));
    // Subscribe-before-precheck also handles commands which already finished.
    let again = fixture.json(&[
        "session",
        "wait",
        id,
        "--until",
        "exited",
        "--timeout",
        "0",
        "--json",
    ]);
    assert_eq!(again["timedOut"], false);
    let captured = fixture.json(&["session", "read", id, "--json"]);
    let text = captured["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for literal in [
        "<>",
        "<a b>",
        "<$(touch injected)>",
        "<`touch injected-too`>",
        "<--json>",
        "<--host>",
        "<界",
        "FINAL OUTPUT",
    ] {
        assert!(text.contains(literal), "missing {literal:?} in {text:?}");
    }
    assert_eq!(
        std::fs::read_to_string(fixture._temp.path().join("arguments")).unwrap(),
        "<>\n<a b>\n<$(touch injected)>\n<`touch injected-too`>\n<--json>\n<--host>\n<界>\n"
    );
    assert!(!fixture._temp.path().join("injected").exists());
    assert!(!fixture._temp.path().join("injected-too").exists());
    fixture.json(&["session", "release", id, "--remove", "--json"]);
    assert_eq!(fixture.cli(&["session", "get", id]).status.code(), Some(3));
}

#[test]
fn option_like_title_does_not_enable_json_output() {
    let fixture = Fixture::new();
    let output = fixture.cli(&["session", "run", "--title", "--json", "--", "/usr/bin/true"]);
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    let id = output
        .trim()
        .strip_prefix("started ")
        .expect("human output");
    let record = fixture.json(&["session", "get", id, "--json"]);
    assert_eq!(record["title"], "--json");
    fixture.json(&["session", "release", id, "--remove", "--json"]);
}

#[test]
fn malformed_launch_fails_before_creating_work() {
    let fixture = Fixture::new();
    let bridge = Bridge::new(fixture.server.socket_path().into(), None);
    for argv in [
        json!("echo"),
        json!(["/bin/echo", 42]),
        json!([]),
        json!([""]),
    ] {
        let result = bridge.request(
            Method::SESSION_SPAWN,
            json!({"kind": "generic", "cwd": fixture._temp.path(), "argv": argv}),
            Duration::from_secs(3),
        );
        assert!(result.unwrap_err().contains("bad_request"));
    }
    let result = fixture.json(&["session", "list", "--all", "--json"]);
    assert!(result["sessions"].as_array().unwrap().is_empty());
    assert!(
        !fixture
            .cli(&[
                "session",
                "run",
                "--host",
                "unknown",
                "--cwd",
                "/tmp",
                "--",
                "/bin/echo"
            ])
            .status
            .success()
    );
    assert!(
        fixture.json(&["session", "list", "--all", "--json"])["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
