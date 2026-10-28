//! Resuming a session starts its new child at once, at the size the previous
//! run was using, instead of waiting out the deferred-launch fallback for a
//! size the App will not send (its pane did not change).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::session::HolderConfig;
use diri_engine::{ManifestEngine, Registry};
use diri_proto::ControlMessage;
use serde_json::{Value, json};

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

struct Control {
    writer: UnixStream,
    replies: BufReader<UnixStream>,
    next: u64,
}

impl Control {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, diri_proto::ControlError> {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).unwrap();
        let mut line = String::new();
        self.replies.read_line(&mut line).unwrap();
        match serde_json::from_str::<ControlMessage>(&line).unwrap() {
            ControlMessage::Response { result, .. } => result,
            other => panic!("{method}: unexpected {other:?}"),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error:?}"))
    }
}

/// A held-transport Engine on a private socket, as the app runs it.
fn start(temp: &std::path::Path) -> (Arc<Mutex<Registry>>, Control) {
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.join("holders"),
                executable: env!("CARGO_BIN_EXE_diri-holder").into(),
            }),
    );
    let listener = server.bind().unwrap();
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let server = Arc::clone(&serving);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let control = UnixStream::connect(server.socket_path()).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let writer = control.try_clone().unwrap();
    (
        registry,
        Control {
            writer,
            replies: BufReader::new(control),
            next: 0,
        },
    )
}

#[test]
fn resume_launches_at_the_previous_size_without_waiting_for_the_app() {
    let temp = tempfile::tempdir().unwrap();
    let (registry, mut control) = start(temp.path());

    // The previous run: sized by the App to its pane, then gone.
    let spawned = control.request(
        "session.spawn",
        json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": ["/bin/sh", "-c", "read line"] }),
    );
    let id = spawned["id"].as_str().unwrap().to_owned();
    let child = || {
        registry
            .lock()
            .unwrap()
            .get(&id)
            .map_or(0, diri_engine::session::Session::child_pid)
    };
    wait_until("first child", Duration::from_secs(5), || child() != 0);
    {
        let registry = registry.lock().unwrap();
        let session = registry.get(&id).unwrap();
        session.resize(132, 41).unwrap();
        session.write_input(b"\n").unwrap();
    }
    let first_pid = child();
    wait_until("first run exits", Duration::from_secs(5), || {
        registry
            .lock()
            .unwrap()
            .record(&id)
            .is_some_and(|record| matches!(record.status, diri_proto::SessionStatus::Exited(_)))
    });

    let started = Instant::now();
    control.request("session.resume", json!({ "sessionID": id }));
    wait_until("resumed child", Duration::from_secs(5), || {
        let pid = child();
        pid != 0 && pid != first_pid
    });
    let launched_after = started.elapsed();
    assert!(
        launched_after < Duration::from_millis(300),
        "the resumed child waited {launched_after:?}, as if for a size the App never sends"
    );
    assert_eq!(
        registry.lock().unwrap().get(&id).unwrap().screen_size(),
        (132, 41),
        "the resumed child starts at the size its previous run was using"
    );

    let _ = registry
        .lock()
        .unwrap()
        .terminate(&id, Duration::from_secs(2));
}

/// A session whose project or worktree folder was deleted cannot be brought
/// back. Resume used to evict the dead run, launch a Holder that could only
/// fail with ENOENT, and answer `internal` after the launch wait (~7 s). It
/// now refuses at once with `cwd_missing` and leaves the record untouched.
#[test]
fn resuming_a_session_whose_folder_is_gone_is_refused_at_once() {
    let temp = tempfile::tempdir().unwrap();
    let (registry, mut control) = start(temp.path());
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let spawned = control.request(
        "session.spawn",
        json!({ "kind": { "shell": {} }, "cwd": project, "argv": ["/bin/sh", "-c", "exit 3"] }),
    );
    let id = spawned["id"].as_str().unwrap().to_owned();
    let exited = || {
        registry
            .lock()
            .unwrap()
            .record(&id)
            .is_some_and(|record| matches!(record.status, diri_proto::SessionStatus::Exited(_)))
    };
    wait_until("the run exits", Duration::from_secs(5), exited);
    std::fs::remove_dir(&project).unwrap();

    let started = Instant::now();
    let error = control
        .call("session.resume", json!({ "sessionID": id }))
        .expect_err("a missing folder cannot be resumed");
    assert_eq!(error.code, diri_proto::control::CWD_MISSING, "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused after {:?}",
        started.elapsed()
    );
    assert!(exited(), "the record keeps its last run");

    let error = control
        .call(
            "session.spawn",
            json!({ "kind": { "shell": {} }, "cwd": project, "argv": ["/bin/true"] }),
        )
        .expect_err("nothing spawns in a missing folder");
    assert_eq!(error.code, diri_proto::control::CWD_MISSING, "{error:?}");
}

/// A command whose program does not exist used to abort inside the spawn
/// (SIGABRT, `fatal runtime error` on screen). Through a Holder it now ends
/// the way a shell reports it: `command not found` on the terminal, exit 127,
/// and how long it ran, so the app keeps that tab open on the error.
#[test]
fn a_missing_program_explains_itself_and_exits_127() {
    let temp = tempfile::tempdir().unwrap();
    let (registry, mut control) = start(temp.path());
    for (program, explanation) in [
        (
            "diri-no-such-tool-xyz",
            "diri: diri-no-such-tool-xyz: command not found",
        ),
        (
            "/nonexistent/diri-tool",
            "diri: /nonexistent/diri-tool: No such file or directory",
        ),
    ] {
        let spawned = control.request(
            "session.spawn",
            json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": [program], "keepOpen": true }),
        );
        let id = spawned["id"].as_str().unwrap().to_owned();
        assert_eq!(spawned["commandRun"], json!({ "keepOpen": true }));
        let record = || registry.lock().unwrap().record(&id).unwrap();
        wait_until("the run exits", Duration::from_secs(10), || {
            matches!(record().status, diri_proto::SessionStatus::Exited(_))
        });
        let record = record();
        let diri_proto::SessionStatus::Exited(exit) = &record.status else {
            unreachable!()
        };
        assert_eq!(
            (exit.reason, exit.code, exit.signal),
            (diri_proto::ExitReason::Exited, Some(127), None),
            "{program}"
        );
        let run = record.command_run.clone().expect("a command run");
        assert!(run.keep_open);
        let ran = run.ran_ms.expect("the Engine timed the run");
        assert!(ran < 5_000, "{program} ran {ran} ms");
        assert!(record.command_keeps_tab());
        let screen = registry.lock().unwrap().get(&id).unwrap().screen_lines();
        assert!(
            screen.iter().any(|line| line.contains(explanation)),
            "{program}: {screen:?}"
        );
    }
}

/// `keepOpen` belongs to commands. An Agent's or a login shell's tab already
/// outlives its program; the flag is refused there before anything starts.
#[test]
fn keep_open_is_refused_for_a_login_shell() {
    let temp = tempfile::tempdir().unwrap();
    let (registry, mut control) = start(temp.path());
    let error = control
        .call(
            "session.spawn",
            json!({ "kind": { "shell": {} }, "cwd": "/tmp", "keepOpen": true }),
        )
        .expect_err("a login shell has no command to keep open");
    assert_eq!(error.code, "bad_request", "{error:?}");
    assert!(registry.lock().unwrap().records().is_empty());
}
