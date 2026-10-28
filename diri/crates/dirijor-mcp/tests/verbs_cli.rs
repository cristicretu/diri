#![cfg(unix)]

//! The session verbs (`send-key`, `wait`, `kill`, `split`, `focus`, `zoom`,
//! `move`, `swap`, `resize`, `whoami`, `version`, `session inspect|rename|
//! close`, `tab ls`, `pane ls`, `events -s`) against a real in-process
//! Engine and real PTYs.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::{ControlServer, InjectionConfig};
use diri_engine::{detect::ManifestEngine, registry::Registry};
use serde_json::{Value, json};

struct Fixture {
    server: Arc<ControlServer>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_state(None)
    }

    /// An Engine that starts from `state` (a `state.json`), for records that
    /// need no process, such as an Agent caller for the release gate.
    fn with_state(state: Option<Value>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let (engine, errors) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        assert!(errors.is_empty());
        let mut registry = Registry::new(Arc::new(engine), temp.path().join("state.json"));
        if let Some(state) = state {
            std::fs::write(
                temp.path().join("state.json"),
                serde_json::to_vec(&state).unwrap(),
            )
            .unwrap();
            registry.load().unwrap();
        }
        let registry = Arc::new(Mutex::new(registry));
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
                let _ = connection.join();
            }
        });
        Self {
            server,
            stop,
            workers: vec![watcher, accept],
            temp,
        }
    }

    fn command(&self, caller: Option<&str>, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dirijor"));
        command
            .env("DIRIJOR_SOCKET", self.server.socket_path())
            .env_remove("DIRIJOR_SESSION_ID")
            .current_dir(self.temp.path())
            .args(args);
        if let Some(caller) = caller {
            command.env("DIRIJOR_SESSION_ID", caller);
        }
        command
    }

    fn cli_as(&self, caller: Option<&str>, args: &[&str]) -> Output {
        self.command(caller, args).output().unwrap()
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.cli_as(None, args)
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
        let text = self.ok(args);
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{args:?}: {error}: {text}"))
    }

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

    fn sleeper(&self, title: &str) -> String {
        self.ok(&["run", "--label", title, "--", "/bin/sh", "-c", "sleep 30"])
            .trim()
            .to_owned()
    }

    /// A workspace with one tab showing `session`.
    fn show(&self, session: &str) -> (String, String) {
        let created = self.json(&["workspace", "create", "Verbs"]);
        let workspace = created["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|workspace| workspace["name"] == "Verbs")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let tabs = self.json(&["tab", "create", &workspace, session]);
        let tab = tabs["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|workspace| workspace["tabs"].as_array().unwrap())
            .find(|tab| tab["layout"]["sessionId"] == session)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        (workspace, tab)
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
fn send_key_presses_named_chords_and_wait_reports_the_exit() {
    let fixture = Fixture::new();
    let id = fixture
        .ok(&[
            "run",
            "--label",
            "keys",
            "--",
            "/bin/sh",
            "-c",
            "trap 'echo GOT_INT; exit 3' INT; echo ready; read line; echo \"got:$line\"; \
             while :; do sleep 0.05; done",
        ])
        .trim()
        .to_owned();
    fixture.capture_until(&id, "ready");

    // A bad key name fails before anything is typed.
    let bad = fixture.cli(&["send-key", &id, "x", "enterprise"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown key \"enterprise\""));

    // Letter names are case-insensitive; shift+ makes the capital.
    fixture.ok(&["send", &id, "-l", "hi"]);
    let pressed = fixture.json(&[
        "send-key", "keys", "G", "space", "shift+g", "--repeat", "1", "--json",
    ]);
    assert_eq!(pressed["keys"], 3, "{pressed}");
    fixture.ok(&["send-key", &id, "enter"]);
    fixture.capture_until(&id, "got:hig G");

    // Still running: a bounded wait times out with exit 2 and says so.
    let started = Instant::now();
    let waited = fixture.cli(&["wait", &id, "--for", "300ms"]);
    assert_eq!(waited.status.code(), Some(2));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(String::from_utf8_lossy(&waited.stderr).contains("still"));

    fixture.ok(&["send-key", &id, "ctrl+c"]);
    let waited = fixture.json(&["wait", &id, "--for", "10s", "--json"]);
    assert_eq!(waited["timedOut"], false);
    assert_eq!(waited["status"], "exited");
    assert_eq!(waited["exitCode"], 3, "{waited}");
    assert_eq!(waited["until"], json!(["exited"]));
    assert_eq!(fixture.ok(&["wait", &id]), format!("{id} exited 3\n"));
}

#[test]
fn run_wait_takes_durations() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = fixture.cli(&[
        "run",
        "--wait",
        "--timeout",
        "300ms",
        "--",
        "/bin/sh",
        "-c",
        "sleep 5",
    ]);
    assert_eq!(output.status.code(), Some(124));
    assert!(started.elapsed() < Duration::from_secs(4));
    let output = fixture.cli(&["run", "--wait", "--timeout", "soon", "--", "/usr/bin/true"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid duration"));
}

#[test]
fn kill_stops_and_closes_and_keep_leaves_the_record() {
    let fixture = Fixture::new();
    let id = fixture.sleeper("doomed");
    let killed = fixture.json(&["kill", "doomed", "--json"]);
    assert_eq!(killed, json!({"ok": true, "id": id, "removed": true}));
    assert_eq!(fixture.cli(&["session", "get", &id]).status.code(), Some(3));

    let kept = fixture.sleeper("kept");
    assert_eq!(
        fixture.ok(&["kill", &kept, "--keep"]),
        format!("killed {kept}\n")
    );
    let record = fixture.json(&["session", "get", &kept, "--json"]);
    assert!(record["status"].get("exited").is_some(), "{record}");
    // Killing what already ended still closes it.
    fixture.ok(&["session", "close", &kept]);
    assert_eq!(fixture.cli(&["kill", &kept]).status.code(), Some(3));
}

fn record(id: &str, project: &str, parent: Option<&str>, exited: bool) -> Value {
    let status = if exited {
        serde_json::to_value(diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
            reason: diri_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
            interrupted: false,
        }))
        .unwrap()
    } else {
        serde_json::to_value(diri_proto::SessionStatus::Idle).unwrap()
    };
    let mut record = json!({
        "id": id,
        "kind": diri_proto::AgentKind::CODEX,
        "cwd": format!("/fixture/{project}"),
        "projectID": project,
        "title": id,
        "titleSource": diri_proto::TitleSource::Placeholder,
        "status": status,
        "resumability": diri_proto::Resumability::Live,
        "createdAt": 1.0,
        "updatedAt": 1.0,
        "pinned": false,
    });
    if let Some(parent) = parent {
        record["parent"] = json!(parent);
    }
    record
}

#[test]
fn agents_kill_only_what_mcp_lets_them_release() {
    let fixture = Fixture::with_state(Some(json!({
        "version": 1,
        "projects": [
            {"id": "mine", "name": "Mine", "root": "/fixture/mine"},
            {"id": "theirs", "name": "Theirs", "root": "/fixture/theirs"},
        ],
        "sessions": [
            record("s_root", "mine", None, false),
            record("s_done", "mine", Some("s_root"), true),
            record("s_other", "theirs", None, true),
            record("s_child", "mine", Some("s_root"), false),
        ],
    })));
    let denied = |caller: &str, args: &[&str]| {
        let output = fixture.cli_as(Some(caller), args);
        assert_eq!(
            output.status.code(),
            Some(6),
            "{args:?} as {caller}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    // Another project's session, and the caller itself, are out of reach.
    assert!(denied("s_root", &["kill", "s_other"]).contains("release_agent"));
    assert!(denied("s_root", &["kill", "s_root"]).contains("cannot terminate its caller"));
    // A delegated agent cannot stop its parent, with any spelling.
    denied("s_child", &["session", "close", "s_root"]);
    denied("s_child", &["session", "release", "s_root"]);
    // A stale identity fails closed, as MCP does.
    denied("s_gone", &["kill", "s_done"]);
    // In reach: a finished child in the caller's project.
    let output = fixture.cli_as(Some("s_root"), &["kill", "s_done", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Outside Diri (no DIRIJOR_SESSION_ID) the owner is not gated.
    fixture.ok(&["kill", "s_other"]);
}

#[test]
fn a_person_at_a_diri_prompt_is_not_gated() {
    let fixture = Fixture::new();
    let shell = fixture
        .ok(&[
            "new",
            "-c",
            fixture.temp.path().to_str().unwrap(),
            "--label",
            "prompt",
        ])
        .trim()
        .to_owned();
    let target = fixture.sleeper("target");
    let output = fixture.cli_as(Some(&shell), &["kill", &target]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.ok(&["kill", &shell]);
}

#[test]
fn layout_verbs_address_panes_by_session() {
    let fixture = Fixture::new();
    let editor = fixture.sleeper("editor");
    let (_, tab) = fixture.show(&editor);

    // Not shown anywhere: a helpful not-found, nothing started.
    let loose = fixture.sleeper("loose");
    let output = fixture.cli(&["split", &loose]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not shown in any tab"));

    let split = fixture.json(&[
        "split", "editor", "--edge", "down", "--ratio", "30", "--label", "logs", "--json", "--",
        "/bin/sh", "-c", "sleep 30",
    ]);
    let logs = split["id"].as_str().unwrap().to_owned();
    assert_eq!(split["layout"]["placed"], true, "{split}");
    assert_eq!(split["layout"]["tabId"], tab.as_str());
    assert_eq!(split["session"]["title"], "logs");

    let panes = fixture.json(&["pane", "ls", "editor", "--json"]);
    let sessions: Vec<&str> = panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, [editor.as_str(), logs.as_str()]);
    let tabs = fixture.json(&["tab", "ls", "--json"]);
    let row = tabs["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["tab_id"] == tab.as_str())
        .unwrap();
    assert_eq!(row["pane_count"], 2, "{row}");

    // The new pane got 30% of a stacked split: it is second, so 0.7.
    let inspect = fixture.json(&["session", "inspect", &logs]);
    assert_eq!(inspect["tabs"][0]["layout"]["split"], "stacked");
    assert_eq!(inspect["tabs"][0]["layout"]["fraction"], 0.7);
    assert_eq!(inspect["panes"][0]["tabId"], tab.as_str());
    let resized = fixture.json(&["resize", &logs, "--ratio", "40", "--json"]);
    assert_eq!(resized["tab"]["layout"]["fraction"], 0.6);
    assert!(
        fixture
            .cli(&["resize", &logs, "--ratio", "95"])
            .status
            .code()
            == Some(1)
    );

    // Zoom toggles, and --on/--off are idempotent.
    assert_eq!(fixture.json(&["zoom", &logs, "--json"])["zoomed"], true);
    assert_eq!(fixture.json(&["zoom", &logs, "--json"])["zoomed"], false);
    assert_eq!(
        fixture.json(&["zoom", &logs, "--on", "--json"])["zoomed"],
        true
    );
    assert_eq!(
        fixture.json(&["zoom", &logs, "--on", "--json"])["zoomed"],
        true
    );
    let off = fixture.json(&["zoom", &logs, "--off", "--json"]);
    assert_eq!(off["tab"]["zoomed_session"], Value::Null);

    let swapped = fixture.json(&["swap", &editor, &logs, "--json"]);
    assert_eq!(
        swapped["tabs"][0]["sessions"],
        json!([logs.as_str(), editor.as_str()])
    );
    // Move the unshown session in beside the editor, on the right, at 25%.
    let moved = fixture.json(&["move", &loose, "right", &editor, "--ratio", "25", "--json"]);
    assert_eq!(moved["tab"]["pane_count"], 3, "{moved}");
    let focused = fixture.json(&["focus", &loose, "--json"]);
    assert_eq!(focused["tab"]["focused_session"], loose.as_str());

    // run --split docks beside a pane too.
    let ran = fixture.json(&[
        "run", "--split", "right", "--beside", &editor, "--json", "--", "/bin/sh", "-c", "sleep 30",
    ]);
    assert_eq!(ran["layout"]["placed"], true, "{ran}");
    assert_eq!(
        fixture
            .cli(&["run", "--ratio", "30", "--", "/usr/bin/true"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn whoami_names_the_session_tab_and_pane() {
    let fixture = Fixture::new();
    let own = fixture.sleeper("me");
    let (_, tab) = fixture.show(&own);
    let output = fixture.cli_as(Some(&own), &["whoami", "--json"]);
    assert!(output.status.success());
    let who: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(who["sessionId"], own.as_str());
    assert_eq!(who["session"]["title"], "me");
    assert_eq!(who["panes"][0]["tabId"], tab.as_str());
    assert_eq!(who["panes"][0]["focusedPane"], true);
    assert!(who["engine"]["pid"].as_i64().unwrap() > 0);

    // A split with no TARGET goes beside the caller.
    let output = fixture.cli_as(Some(&own), &["split", "--edge", "right", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let split: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(split["layout"]["besideSessionId"], own.as_str());
    assert!(split["session"]["kind"].get("shell").is_some(), "{split}");

    let outside = fixture.json(&["whoami", "--json"]);
    assert_eq!(outside["sessionId"], Value::Null);
    let stale = fixture.cli_as(Some("s_gone"), &["whoami"]);
    assert!(String::from_utf8_lossy(&stale.stdout).contains("not a live session"));
    assert_eq!(fixture.cli(&["split"]).status.code(), Some(1));
}

#[test]
fn version_works_with_and_without_an_engine() {
    let fixture = Fixture::new();
    let version = fixture.json(&["version", "--json"]);
    assert_eq!(version["cli"], env!("CARGO_PKG_VERSION"));
    assert!(version["engine"]["build"].is_string(), "{version}");
    let offline = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .env("DIRIJOR_SOCKET", fixture.temp.path().join("missing.sock"))
        .args(["version", "--json"])
        .output()
        .unwrap();
    assert!(offline.status.success());
    let offline: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(offline["engine"], Value::Null);
    // Help needs no Engine either.
    let help = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .env("DIRIJOR_SOCKET", fixture.temp.path().join("missing.sock"))
        .args(["kill", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--keep"));
}

#[test]
fn session_rename_and_events_stream_the_change() {
    let fixture = Fixture::new();
    let id = fixture.sleeper("before");
    let mut events = fixture
        .command(
            None,
            &[
                "events",
                "--json",
                "-s",
                &id,
                "--kind",
                "session.updated",
                "--count",
                "1",
                "--timeout",
                "10",
            ],
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Give the subscription a moment to register before the change.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        fixture.json(&["session", "rename", &id, "after", "rename", "--json"]),
        json!({"ok": true, "id": id, "title": "after rename"})
    );
    let mut line = String::new();
    BufReader::new(events.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let event: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(event["event"], "session.updated", "{event}");
    assert!(event["seq"].as_u64().is_some());
    assert!(events.wait().unwrap().success());
    assert_eq!(
        fixture.json(&["session", "get", &id, "--json"])["title"],
        "after rename"
    );
    assert_eq!(
        fixture.cli(&["session", "rename", &id]).status.code(),
        Some(1)
    );
}

/// Wall time of one CLI call, median of `runs`. `cargo test -p dirijor-mcp
/// --test verbs_cli -- --ignored --nocapture cli_latency` prints the table.
#[test]
#[ignore = "measurement; run by hand"]
fn cli_latency() {
    let fixture = Fixture::new();
    let id = fixture
        .ok(&[
            "run",
            "--label",
            "bench",
            "--",
            "/bin/sh",
            "-c",
            "cat > /dev/null",
        ])
        .trim()
        .to_owned();
    for index in 0..19 {
        fixture.sleeper(&format!("filler {index}"));
    }
    let done = fixture
        .ok(&["run", "--", "/usr/bin/true"])
        .trim()
        .to_owned();
    fixture.ok(&["wait", &done, "--for", "5s"]);
    let (_, _) = fixture.show(&id);
    let median = |args: &[&str]| {
        let mut samples: Vec<f64> = (0..31)
            .map(|_| {
                let started = Instant::now();
                let output = fixture.cli(args);
                assert!(output.status.success(), "{args:?}");
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    let rows = [
        (
            "session key ID enter (existing)",
            median(&["session", "key", &id, "enter"]),
        ),
        ("send-key ID enter", median(&["send-key", &id, "enter"])),
        (
            "send-key ID ctrl+a ctrl+e (2 keys)",
            median(&["send-key", &id, "ctrl+a", "ctrl+e"]),
        ),
        (
            "session wait ID --until exited (existing, exited)",
            median(&["session", "wait", &done, "--until", "exited"]),
        ),
        ("wait ID (exited)", median(&["wait", &done])),
        (
            "__complete targets (21 sessions)",
            median(&["__complete", "targets"]),
        ),
        ("whoami --json", median(&["whoami", "--json"])),
        ("focus ID", median(&["focus", &id])),
        ("version --json", median(&["version", "--json"])),
        (
            "completion zsh (no Engine call)",
            median(&["completion", "zsh"]),
        ),
    ];
    for (name, ms) in rows {
        println!("| `{name}` | {ms:.1} ms |");
    }
}
