//! File-triggered schedules, end to end: a real Engine on a private socket
//! watches a temporary folder through the platform watcher (FSEvents on
//! macOS, inotify on Linux) and hands new files to a real session. The
//! session runs `cat`, appending every prompt it receives to a capture file,
//! so the exact prompt (and which session got it) is observed from outside.
//!
//! The settle debounce is shortened to 300 ms; the batching and settling
//! rules themselves are covered with an injected clock in
//! `diri_engine::file_trigger`'s unit tests.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use serde_json::{Value, json};

const SETTLE: Duration = Duration::from_millis(300);

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn start_server(temp: &Path) -> Arc<ControlServer> {
    let registry = Arc::new(Mutex::new(Registry::new(engine(), temp.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_schedule_power(diri_engine::wake::PowerConfig {
                socket_path: temp.join("wake.sock"),
                caffeinate: None,
            })
            .with_file_trigger_settle(SETTLE)
            .with_logs_dir(temp.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.join("holders"),
                executable: env!("CARGO_BIN_EXE_diri-holder").into(),
            }),
    );
    let listener = server.bind().expect("bind");
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

struct Control {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Control {
    fn connect(server: &ControlServer) -> Self {
        let stream = UnixStream::connect(server.socket_path()).expect("connect control");
        let reader = BufReader::new(stream.try_clone().expect("clone"));
        Self {
            stream,
            reader,
            next_id: 1,
        }
    }

    fn try_request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .expect("encode");
        bytes.push(b'\n');
        self.stream.write_all(&bytes).expect("write");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read reply");
        match serde_json::from_str::<ControlMessage>(&line).expect("decode") {
            ControlMessage::Response { result, .. } => result.map_err(|error| error.message),
            other => panic!("{method}: unexpected {other:?}"),
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.try_request(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }

    fn schedule(&mut self, id: &str) -> Value {
        self.request("schedule.list", json!({}))["schedules"]
            .as_array()
            .expect("schedules")
            .iter()
            .find(|record| record["id"] == id)
            .cloned()
            .expect("schedule listed")
    }

    fn runs(&mut self, id: &str) -> Vec<Value> {
        self.schedule(id)["runs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn kill_runs(&mut self, id: &str) {
        let mut seen = Vec::new();
        for run in self.runs(id) {
            if let Some(session) = run["sessionId"].as_str()
                && !seen.contains(&session.to_owned())
            {
                seen.push(session.to_owned());
                let _ = self.try_request("session.kill", json!({ "sessionID": session }));
            }
        }
    }
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A watched folder and a capture file, both under a canonical temp root
/// (FSEvents reports `/private/var/...`, not `/var/...`).
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    inbox: PathBuf,
    capture: PathBuf,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let inbox = root.join("Inbox");
    std::fs::create_dir(&inbox).unwrap();
    let capture = root.join("prompts.txt");
    Fixture {
        _temp: temp,
        root,
        inbox,
        capture,
    }
}

fn trigger(fx: &Fixture, glob: Option<&str>, recursive: bool) -> Value {
    let mut when = json!({ "kind": "files", "folder": fx.inbox });
    if let Some(glob) = glob {
        when["glob"] = json!(glob);
    }
    if recursive {
        when["recursive"] = json!(true);
    }
    json!({
        "title": "Inbox triage",
        "when": when,
        "spawn": {
            "kind": { "generic": { "command": reader(&fx.capture) } },
            "cwd": fx.root,
            "initialPrompt": "Triage the bug report.",
        },
    })
}

/// A line reader that answers every line, like an agent acknowledging a
/// prompt, and appends what it read to `capture`.
fn reader(capture: &Path) -> String {
    format!(
        "/bin/sh -c 'printf \"ready> \"; while IFS= read -r line; do printf \"%s\\n\" \"$line\" >> \"$0\"; printf \"ok\\nready> \"; done' '{}'",
        capture.display()
    )
}

fn captured(fx: &Fixture) -> String {
    std::fs::read_to_string(&fx.capture).unwrap_or_default()
}

/// Runs that have recorded their session (or failed), newest last.
fn settled_runs(control: &mut Control, id: &str, count: usize) -> Vec<Value> {
    let mut runs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(29);
    wait_until(
        &format!("{count} recorded run(s)"),
        Duration::from_secs(30),
        || {
            runs = control.runs(id);
            if Instant::now() > deadline {
                eprintln!("runs so far: {runs:?}");
            }
            runs.len() >= count
                && runs
                    .iter()
                    .all(|run| run["sessionId"].is_string() || run["outcome"] == "failed")
        },
    );
    runs
}

fn files_of(run: &Value) -> Vec<String> {
    run["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .map(|file| file.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_new_file_fires_once_with_its_quoted_path() {
    let fx = fixture();
    std::fs::write(fx.inbox.join("old.txt"), "already here").unwrap();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let created = control.request("schedule.create", trigger(&fx, None, false));
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["when"]["kind"], "files");
    assert!(created.get("nextDue").is_none());

    let name = "bug \"report\" $(whoami).txt";
    std::fs::write(fx.inbox.join(name), "crash log").unwrap();
    let runs = settled_runs(&mut control, &id, 1);
    let expected = fx.inbox.join(name).to_string_lossy().into_owned();
    assert_eq!(files_of(&runs[0]), vec![expected.clone()]);
    assert_eq!(runs[0]["outcome"], "onTime");
    wait_until(
        "the prompt reached the session",
        Duration::from_secs(20),
        || captured(&fx).contains("untrusted"),
    );
    let prompt = captured(&fx);
    assert!(prompt.starts_with("Triage the bug report."), "{prompt}");
    let quoted = serde_json::to_string(&expected).unwrap();
    assert!(prompt.contains(&format!("- {quoted}")), "{prompt}");
    assert!(
        !prompt.contains("old.txt"),
        "pre-existing files are not new"
    );
    assert!(!prompt.contains("crash log"), "contents are never inlined");

    // Editing the handled file, or touching it, never fires again.
    std::fs::write(fx.inbox.join(name), "crash log, edited").unwrap();
    std::fs::write(fx.inbox.join("old.txt"), "edited too").unwrap();
    std::thread::sleep(SETTLE * 6);
    assert_eq!(control.runs(&id).len(), 1, "edits are not new files");

    let sessions = control.request("session.list", json!({}));
    let session = &sessions["sessions"].as_array().unwrap()[0];
    assert_eq!(session["scheduledRun"]["scheduleId"], id);
    control.kill_runs(&id);
}

#[test]
fn a_burst_is_one_batch_and_ignored_names_never_fire() {
    let fx = fixture();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let id = control.request("schedule.create", trigger(&fx, Some("*.png"), false))["id"]
        .as_str()
        .unwrap()
        .to_owned();

    for ignored in [
        "notes.txt",
        ".DS_Store",
        ".hidden.png",
        "shot.png.crdownload",
        "~$lock.png",
    ] {
        std::fs::write(fx.inbox.join(ignored), "x").unwrap();
    }
    std::thread::sleep(SETTLE * 6);
    assert!(control.runs(&id).is_empty(), "ignored names fired");

    for index in 0..5 {
        std::fs::write(fx.inbox.join(format!("shot-{index}.png")), "png").unwrap();
    }
    let runs = settled_runs(&mut control, &id, 1);
    std::thread::sleep(SETTLE * 4);
    let runs_after = control.runs(&id);
    assert_eq!(runs_after.len(), 1, "one run for the burst: {runs_after:?}");
    let mut files = files_of(&runs[0]);
    files.sort();
    let expected: Vec<String> = (0..5)
        .map(|index| {
            fx.inbox
                .join(format!("shot-{index}.png"))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(files, expected);
    assert_eq!(
        control.request("session.list", json!({}))["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "not one tab per file"
    );
    control.kill_runs(&id);
}

#[test]
fn a_half_written_file_waits_for_its_size_to_hold() {
    let fx = fixture();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let id = control.request("schedule.create", trigger(&fx, None, false))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = fx.inbox.join("video.mov");
    let mut file = std::fs::File::create(&path).unwrap();
    // Keep growing for well over the settle time.
    for _ in 0..8 {
        file.write_all(&[0u8; 4096]).unwrap();
        file.flush().unwrap();
        std::thread::sleep(SETTLE / 2);
        assert!(control.runs(&id).is_empty(), "fired while still growing");
    }
    drop(file);
    let runs = settled_runs(&mut control, &id, 1);
    assert_eq!(
        files_of(&runs[0]),
        vec![path.to_string_lossy().into_owned()]
    );
    control.kill_runs(&id);
}

#[test]
fn symlinks_out_of_the_folder_are_refused() {
    let fx = fixture();
    let outside = fx.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "s").unwrap();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);

    for folder in ["/".to_owned(), std::env::var("HOME").unwrap()] {
        let mut bad = trigger(&fx, None, false);
        bad["when"]["folder"] = json!(folder);
        let error = control.try_request("schedule.create", bad).unwrap_err();
        assert!(
            error.contains("disk") || error.contains("home folder"),
            "{folder}: {error}"
        );
    }
    let mut missing = trigger(&fx, None, false);
    missing["when"]["folder"] = json!(fx.root.join("nope"));
    assert!(control.try_request("schedule.create", missing).is_err());

    let id = control.request("schedule.create", trigger(&fx, None, true))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::os::unix::fs::symlink(outside.join("secret.txt"), fx.inbox.join("innocent.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, fx.inbox.join("escape")).unwrap();
    std::fs::write(outside.join("later.txt"), "l").unwrap();
    std::fs::write(fx.inbox.join("real.txt"), "r").unwrap();
    let runs = settled_runs(&mut control, &id, 1);
    std::thread::sleep(SETTLE * 4);
    assert_eq!(control.runs(&id).len(), 1);
    assert_eq!(
        files_of(&runs[0]),
        vec![fx.inbox.join("real.txt").to_string_lossy().into_owned()],
        "only the real file inside the folder"
    );
    control.kill_runs(&id);
}

#[test]
fn later_batches_continue_the_same_session() {
    let fx = fixture();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    // A shell goes idle at its prompt, as an agent does after a turn. The
    // counter lives in the shell, so `run-2` exists only if the second batch
    // reached the first batch's shell. (The appended path lines run as
    // harmless unknown commands.)
    let marker = fx.root.join("run");
    let mut spec = trigger(&fx, None, false);
    spec["spawn"]["kind"] = json!({ "shell": {} });
    spec["spawn"]["initialPrompt"] = json!(format!("n=$((n+1)); touch '{}'-$n", marker.display()));
    let id = control.request("schedule.create", spec)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let fired = |n: u32| fx.root.join(format!("run-{n}"));

    std::fs::write(fx.inbox.join("first.csv"), "1").unwrap();
    let first = settled_runs(&mut control, &id, 1);
    wait_until("the first batch ran", Duration::from_secs(30), || {
        fired(1).exists()
    });
    std::fs::write(fx.inbox.join("second.csv"), "2").unwrap();
    let runs = settled_runs(&mut control, &id, 2);
    assert_eq!(runs[1]["sessionId"], first[0]["sessionId"], "continued");
    wait_until(
        "the second batch reached the same shell",
        Duration::from_secs(30),
        || fired(2).exists(),
    );
    assert_eq!(
        control.request("session.list", json!({}))["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "no second tab"
    );
    control.kill_runs(&id);
}

#[test]
fn files_that_arrived_while_down_fire_once_and_handled_ones_never_replay() {
    let fx = fixture();
    let first_root = fx.root.join("engine-a");
    std::fs::create_dir(&first_root).unwrap();
    let server = start_server(&first_root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let id = control.request("schedule.create", trigger(&fx, None, false))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::write(fx.inbox.join("handled.txt"), "h").unwrap();
    settled_runs(&mut control, &id, 1);
    control.kill_runs(&id);

    // "Quit" the first Engine: keep its journal, stop its watcher.
    let journal = first_root.join("schedules-v1.sqlite");
    let db = rusqlite::Connection::open(&journal).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(db);
    let second_root = fx.root.join("engine-b");
    std::fs::create_dir(&second_root).unwrap();
    std::fs::copy(&journal, second_root.join("schedules-v1.sqlite")).unwrap();
    control.request("schedule.delete", json!({ "id": id }));

    // Arrives while no Engine watches.
    std::fs::write(fx.inbox.join("while-down.txt"), "d").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let restarted = start_server(&second_root);
    restarted.spawn_scheduler();
    let mut control = Control::connect(&restarted);
    let runs = settled_runs(&mut control, &id, 2);
    std::thread::sleep(SETTLE * 4);
    let runs_after = control.runs(&id);
    assert_eq!(
        runs_after.len(),
        2,
        "caught up exactly once: {runs_after:?}"
    );
    assert_eq!(
        files_of(&runs[1]),
        vec![
            fx.inbox
                .join("while-down.txt")
                .to_string_lossy()
                .into_owned()
        ],
        "the handled file is not replayed"
    );
    assert_eq!(runs[1]["outcome"], "late");
    assert_eq!(runs[1]["lateReason"], "notRunning");
    control.kill_runs(&id);
}

#[test]
fn pausing_ignores_files_and_resuming_starts_from_the_folder_as_is() {
    let fx = fixture();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let mut spec = trigger(&fx, None, false);
    spec["enabled"] = json!(false);
    let id = control.request("schedule.create", spec.clone())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::write(fx.inbox.join("while-paused.txt"), "p").unwrap();
    std::thread::sleep(SETTLE * 4);
    assert!(
        control.runs(&id).is_empty(),
        "a paused trigger does not fire"
    );
    // Resuming starts from the folder as it is: nothing floods in.
    spec["enabled"] = json!(true);
    spec["id"] = json!(id);
    control.request("schedule.update", spec);
    std::thread::sleep(SETTLE * 4);
    assert!(
        control.runs(&id).is_empty(),
        "resume replayed a paused file"
    );
    std::fs::write(fx.inbox.join("after.txt"), "a").unwrap();
    let runs = settled_runs(&mut control, &id, 1);
    assert_eq!(
        files_of(&runs[0]),
        vec![fx.inbox.join("after.txt").to_string_lossy().into_owned()]
    );
    control.kill_runs(&id);
}

fn cpu_ms() -> f64 {
    // SAFETY: getrusage only writes the zeroed struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let ms = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// Idle cost of armed watchers: process CPU over a quiet window with no
/// trigger, then with one and ten armed. Prints the numbers for the PR.
/// `DIRI_IDLE_SECS` sets the window (default 30 s).
#[test]
#[ignore = "measurement; run with --ignored --nocapture"]
fn idle_cpu_with_watchers_armed() {
    let secs: u64 = std::env::var("DIRI_IDLE_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .unwrap_or(30);
    let fx = fixture();
    let server = start_server(&fx.root);
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let measure = |label: &str| {
        std::thread::sleep(Duration::from_secs(2));
        let before = cpu_ms();
        std::thread::sleep(Duration::from_secs(secs));
        let used = cpu_ms() - before;
        eprintln!(
            "idle {label}: {used:.1} ms CPU over {secs} s = {:.4}% of one core",
            used / (secs as f64 * 10.0)
        );
        used
    };
    let baseline = measure("engine, no trigger");
    control.request("schedule.create", trigger(&fx, None, false));
    let one = measure("engine + 1 armed trigger");
    for index in 0..9 {
        let folder = fx.root.join(format!("watch-{index}"));
        std::fs::create_dir(&folder).unwrap();
        let mut spec = trigger(&fx, None, index % 2 == 0);
        spec["when"]["folder"] = json!(folder);
        control.request("schedule.create", spec);
    }
    let ten = measure("engine + 10 armed triggers");
    assert!(
        one - baseline < 50.0 && ten - baseline < 50.0,
        "watchers cost CPU while idle"
    );
}
