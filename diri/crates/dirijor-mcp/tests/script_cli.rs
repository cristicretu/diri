#![cfg(unix)]

//! The tmux-shaped scripting verbs (`list`, `new`, `send`, `capture`,
//! `run --wait`) against a real in-process Engine and real PTYs.

use std::os::unix::net::UnixStream;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::{ControlServer, InjectionConfig};
use diri_engine::{detect::ManifestEngine, registry::Registry};
use serde_json::Value;

struct Fixture {
    server: Arc<ControlServer>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
    temp: tempfile::TempDir,
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
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
                .with_injection(InjectionConfig {
                    inject_dir: temp.path().join("inject"),
                    cli_path: env!("CARGO_BIN_EXE_dirijor").into(),
                }),
        );
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
            temp,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dirijor"))
            .env("DIRIJOR_SOCKET", self.server.socket_path())
            .env_remove("DIRIJOR_SESSION_ID")
            .current_dir(self.temp.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    /// Polls `capture` until `needle` is on screen.
    fn capture_until(&self, id: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = self.ok(&["capture", id]);
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "{needle:?} never appeared in {screen:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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
fn send_types_text_and_named_keys_then_capture_reads_it_back() {
    let fixture = Fixture::new();
    // `run` without --wait prints only the id, so `id=$(dirijor run ...)` works.
    let id = fixture
        .ok(&[
            "run",
            "--name",
            "echo loop",
            "--",
            "/bin/sh",
            "-c",
            "trap 'echo GOT_INT; exit 3' INT; echo ready; read line; echo \"got:$line\"; \
             while :; do sleep 0.05; done",
        ])
        .trim()
        .to_owned();
    assert!(id.starts_with("s_"), "{id:?}");
    fixture.capture_until(&id, "ready");

    // A unique title fragment is a target; adjacent text tokens join without
    // spaces, as with tmux send-keys.
    fixture.ok(&["send", "echo loop", "hello", " world", "Enter"]);
    let screen = fixture.capture_until(&id, "got:hello world");
    assert!(
        !screen.ends_with(" \n"),
        "capture trims trailing blanks: {screen:?}"
    );
    assert_eq!(
        fixture.ok(&["capture", &id, "--lines", "1"]),
        "got:hello world\n"
    );

    // C-c reaches the foreground process group as SIGINT.
    let prefix = &id[..id.len() - 2];
    fixture.ok(&["send", prefix, "C-c"]);
    fixture.capture_until(&id, "GOT_INT");
    let waited = fixture.json(&[
        "session",
        "wait",
        &id,
        "--until",
        "exited",
        "--timeout",
        "5",
        "--json",
    ]);
    assert_eq!(
        waited["session"]["status"]["exited"]["_0"]["code"], 3,
        "{waited}"
    );

    let listed = fixture.json(&["list", "--json"]);
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"] == id.as_str())
    );
}

#[test]
fn literal_send_does_not_interpret_key_names() {
    let fixture = Fixture::new();
    let id = fixture
        .ok(&[
            "run",
            "--",
            "/bin/sh",
            "-c",
            "echo ready; read line; echo \"got:$line\"; sleep 5",
        ])
        .trim()
        .to_owned();
    fixture.capture_until(&id, "ready");
    fixture.ok(&["send", &id, "-l", "--", "Enter", "C-c"]);
    fixture.ok(&["send", &id, "Enter"]);
    fixture.capture_until(&id, "got:EnterC-c");
    fixture.ok(&["session", "release", &id, "--remove"]);
}

#[test]
fn new_terminals_run_the_configured_shell_command() {
    let fixture = Fixture::new();
    // What `terminal.configure` persists beside the socket: the Engine reads
    // it when a terminal starts. The script is one argv word; only the
    // `sh -c` the user chose interprets it.
    std::fs::write(
        fixture.temp.path().join("terminal.json"),
        serde_json::to_vec(&serde_json::json!({
            "shellIntegration": true,
            "shellCommand": ["/bin/sh", "-c", "echo \"custom shell $((40 + 2))\"; sleep 5"],
        }))
        .unwrap(),
    )
    .unwrap();
    let id = fixture.ok(&["new"]).trim().to_owned();
    assert!(id.starts_with("s_"), "{id:?}");
    fixture.capture_until(&id, "custom shell 42");
    fixture.ok(&["session", "release", &id, "--remove"]);
}

#[test]
fn run_wait_prints_output_and_exits_with_the_command_status() {
    let fixture = Fixture::new();
    let output = fixture.cli(&[
        "run",
        "--wait",
        "--",
        "/bin/sh",
        "-c",
        "echo \"self=$DIRIJOR_SESSION_ID\"; echo second; exit 7",
    ]);
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    let own = lines.next().unwrap().strip_prefix("self=").unwrap();
    assert!(own.starts_with("s_"), "run panes know their id: {stdout:?}");
    assert_eq!(lines.next(), Some("second"));
    // The finished run stays as a tab unless --rm.
    assert_eq!(fixture.json(&["session", "get", own, "--json"])["id"], own);

    let output = fixture.cli(&["run", "--wait", "--rm", "--json", "--", "/usr/bin/true"]);
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exitCode"], 0);
    let id = result["id"].as_str().unwrap();
    assert_eq!(fixture.cli(&["session", "get", id]).status.code(), Some(3));

    let signalled = fixture.cli(&["run", "--wait", "--", "/bin/sh", "-c", "kill -TERM $$"]);
    assert_eq!(signalled.status.code(), Some(128 + 15));
}

#[test]
fn run_wait_timeout_and_own_failures_use_reserved_codes() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = fixture.cli(&[
        "run",
        "--wait",
        "--timeout",
        "0.3",
        "--",
        "/bin/sh",
        "-c",
        "sleep 5",
    ]);
    assert_eq!(output.status.code(), Some(124));
    assert!(started.elapsed() < Duration::from_secs(4));
    let output = fixture.cli(&["run", "--wait", "--cwd", "relative", "--", "/usr/bin/true"]);
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(
        fixture
            .cli(&["run", "--rm", "--", "/usr/bin/true"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn targets_report_missing_ambiguous_and_unreachable_distinctly() {
    let fixture = Fixture::new();
    for _ in 0..2 {
        fixture.ok(&["run", "--name", "twin", "--", "/bin/sh", "-c", "sleep 5"]);
    }
    assert_eq!(
        fixture.cli(&["capture", "nothing-like-this"]).status.code(),
        Some(3)
    );
    assert_eq!(fixture.cli(&["send", "twin", "x"]).status.code(), Some(5));
    assert_eq!(fixture.cli(&["capture", "s_"]).status.code(), Some(5));
    let unreachable = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .env("DIRIJOR_SOCKET", fixture.temp.path().join("missing.sock"))
        .args(["list"])
        .output()
        .unwrap();
    assert_eq!(unreachable.status.code(), Some(4));
}

#[test]
fn new_opens_a_shell_tab_and_prints_its_id() {
    let fixture = Fixture::new();
    let id = fixture
        .ok(&["new", "--cwd", fixture.temp.path().to_str().unwrap()])
        .trim()
        .to_owned();
    let record = fixture.json(&["session", "get", &id, "--json"]);
    assert!(record["kind"].get("shell").is_some(), "{record}");
    fixture.ok(&["session", "release", &id, "--remove"]);
}
