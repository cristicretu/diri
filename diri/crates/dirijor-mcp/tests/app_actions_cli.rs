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
                {"name": "list-keys", "title": "List Key Bindings", "args": {"type":"object","properties":{},"additionalProperties":false}},
                {"name": "select-tab", "title": "Select Tab", "args": {"type":"object","properties":{"index":{"type":"integer","minimum":-1,"maximum":9}},"required":["index"],"additionalProperties":false}}
            ]
        }}),
    );
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"registered\":3"), "{line}");
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
            let result = if request["action"] == "list-keys" {
                json!({
                    "bindings": [
                        {"keys": "cmd-b", "action": "toggle-sidebar", "source": "default"},
                        {"keys": "h", "mode": "resize", "action": "pane-shrink-width", "source": "init.lua:4"},
                    ],
                    "modes": [{"name": "resize", "exclusive": true, "once": false, "source": "init.lua:2"}],
                })
            } else {
                json!({"ran": request["action"], "args": request["args"]})
            };
            send(
                &mut stream,
                json!({"id": next, "method": Method::APP_REPLY, "params": {
                    "requestId": request["requestId"],
                    "result": result
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

    let keymap = server.command(&["keymap"], None);
    assert!(keymap.status.success(), "{}", text(&keymap.stderr));
    let keymap = text(&keymap.stdout);
    assert!(keymap.starts_with("KEY"), "{keymap}");
    assert!(keymap.contains("resize/h"), "{keymap}");
    assert!(keymap.contains("init.lua:4"), "{keymap}");
    assert!(keymap.contains("exclusive"), "{keymap}");

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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_lua_script_calls_engine_methods_and_window_actions() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start(temp.path());
    let (asked, _worker) = fake_app(&server.socket);

    let listed = server.command(
        &["script", "-e", "print(#diri.session.list().sessions)"],
        None,
    );
    assert!(listed.status.success(), "{}", text(&listed.stderr));
    assert_eq!(text(&listed.stdout).trim(), "0");

    let action = server.command(
        &[
            "script",
            "-e",
            "print(diri.actions.select_tab{index = 3}.ran)",
        ],
        None,
    );
    assert!(action.status.success(), "{}", text(&action.stderr));
    assert_eq!(text(&action.stdout).trim(), "select-tab");
    assert_eq!(asked.lock().unwrap()[0]["args"], json!({"index": 3}));

    // A bad parameter fails before anything is sent, naming the method.
    let bad = server.command(&["script", "-e", "diri.session.spawn{cwd = 1}"], None);
    assert!(!bad.status.success());
    assert!(
        text(&bad.stderr).contains("session.spawn"),
        "{}",
        text(&bad.stderr)
    );

    let file = temp.path().join("listen.lua");
    std::fs::write(
        &file,
        r#"
diri.on("workspace.updated", function(snapshot)
  print("updated", type(snapshot))
  diri.stop()
end)
-- The event stream starts after the file loads; poke until it is heard.
diri.every(0.5, function()
  local snapshot = diri.workspace.snapshot()
  diri.workspace.mutate{
    expectedRevision = snapshot.revision,
    mutation = {type = "createWorkspace", name = "from lua"},
  }
end)
"#,
    )
    .unwrap();
    let listened = server.command(&["script", file.to_str().unwrap(), "--for", "20"], None);
    assert!(listened.status.success(), "{}", text(&listened.stderr));
    assert!(
        text(&listened.stdout).starts_with("updated"),
        "stdout: {}\nstderr: {}",
        text(&listened.stdout),
        text(&listened.stderr)
    );
}

#[test]
fn a_script_takes_arguments_its_session_and_an_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start(temp.path());
    let echoed = server.command(
        &[
            "script",
            "-e",
            "print(diri.args.branch, diri.args.count + 1, diri.session_id)",
            "branch=main",
            "count=2",
        ],
        Some("s_inside"),
    );
    assert!(echoed.status.success(), "{}", text(&echoed.stderr));
    assert_eq!(text(&echoed.stdout).trim(), "main\t3\ts_inside");

    let stopped = server.command(&["script", "-e", "diri.stop(3)"], None);
    assert_eq!(stopped.status.code(), Some(3), "{}", text(&stopped.stderr));

    // A wait in the top level: the script runs until it is satisfied.
    let slept = server.command(&["script", "-e", "diri.sleep(50) print('after')"], None);
    assert!(slept.status.success(), "{}", text(&slept.stderr));
    assert_eq!(text(&slept.stdout).trim(), "after");
}

#[test]
fn check_validates_without_running() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start(temp.path());
    let good = temp.path().join("good.lua");
    std::fs::write(
        &good,
        "diri.keymap.mode('resize', {exclusive = true})\n\
         diri.keymap.set('resize/h', 'pane-shrink-width')\n\
         diri.on('session.status', function() end)\n\
         diri.require_grant{exec = {'git'}}\n\
         diri.session.spawn{kind = 'shell', cwd = '/tmp'}\n\
         error('not run')\n",
    )
    .unwrap();
    let checked = server.command(&["script", "--check", good.to_str().unwrap()], None);
    assert!(checked.status.success(), "{}", text(&checked.stderr));
    let report = text(&checked.stdout);
    assert!(
        report.contains("1 key, 1 mode, 0 commands, 1 handler"),
        "{report}"
    );
    assert!(report.contains("asks to run: git"), "{report}");
    assert!(report.contains("stopped at good.lua:5"), "{report}");
    let listed = server.command(&["session", "list", "--json"], None);
    assert!(
        !text(&listed.stdout).contains("\"kind\""),
        "a check spawns nothing"
    );

    let bad = temp.path().join("bad.lua");
    std::fs::write(&bad, "diri.on('focus', function()\n").unwrap();
    let failed = server.command(&["script", "--check", bad.to_str().unwrap()], None);
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        text(&failed.stderr).contains("bad.lua:"),
        "{}",
        text(&failed.stderr)
    );
}

#[cfg(unix)]
#[test]
fn exec_runs_only_granted_programs_and_streams_their_output() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start(temp.path());
    let code = "local ok = diri.require_grant{exec = {'sh'}}\n\
                if not ok then print('denied') return end\n\
                local r = diri.exec({'sh', '-c', 'echo one; echo two'}, {\n\
                  on_output = function(line, stream) print(stream, line) end})\n\
                print('code', r.code)";
    let denied = server.command(&["script", "-e", code], None);
    assert!(denied.status.success(), "{}", text(&denied.stderr));
    assert_eq!(text(&denied.stdout).trim(), "denied");
    assert!(
        text(&denied.stderr).contains("--allow-exec sh"),
        "{}",
        text(&denied.stderr)
    );

    let allowed = server.command(&["script", "-e", code, "--allow-exec", "sh"], None);
    assert!(allowed.status.success(), "{}", text(&allowed.stderr));
    assert_eq!(
        text(&allowed.stdout).trim(),
        "stdout\tone\nstdout\ttwo\ncode\t0"
    );
}
