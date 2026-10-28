//! Scheduled tasks, end to end: a real Engine on a private socket starts a
//! real shell session when a schedule comes due, and on startup catches up
//! an occurrence it was not running for. The prompt is a shell command that
//! touches a marker file, so "the run happened" is observed from outside.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use serde_json::{Value, json};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// The Engine as `dirijord-rs` runs it, minus the scheduler, which each test
/// starts itself so it can stage the journal first.
fn start_server(temp: &Path) -> Arc<ControlServer> {
    let registry = Arc::new(Mutex::new(Registry::new(engine(), temp.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_schedule_power(diri_engine::wake::PowerConfig {
                socket_path: temp.join("wake.sock"),
                caffeinate: None,
            })
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
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spec(marker: &Path, when: Value, window_ms: u64) -> Value {
    json!({
        "title": "Touch the marker",
        "when": when,
        "spawn": {
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "initialPrompt": format!("touch '{}'", marker.display()),
        },
        "catchUpWindowMs": window_ms,
    })
}

/// Moves a stored schedule's next due time, as if the Engine had been down
/// when it came due.
fn backdate(temp: &Path, id: &str, due_ms: f64) {
    let db = rusqlite::Connection::open(temp.join("schedules-v1.sqlite")).expect("open");
    let raw: String = db
        .query_row("SELECT record FROM schedules_v1 WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .expect("row");
    let mut record: Value = serde_json::from_str(&raw).unwrap();
    record["nextDue"] = json!(due_ms);
    db.execute(
        "UPDATE schedules_v1 SET record=?1 WHERE id=?2",
        rusqlite::params![record.to_string(), id],
    )
    .expect("update");
}

fn kill_run(control: &mut Control, record: &Value) {
    if let Some(session) = record["runs"][0]["sessionId"].as_str() {
        control.request("session.kill", json!({ "sessionID": session }));
    }
}

#[test]
fn a_due_one_shot_starts_its_session_on_time() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let marker = temp.path().join("fired");

    let created = control.request(
        "schedule.create",
        spec(
            &marker,
            json!({ "kind": "once", "at": now_ms() + 1_500.0 }),
            60_000,
        ),
    );
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["enabled"], true);

    wait_until("the scheduled prompt ran", Duration::from_secs(30), || {
        marker.exists()
    });
    let mut record = control.schedule(&id);
    wait_until(
        "the run recorded its session",
        Duration::from_secs(10),
        || {
            record = control.schedule(&id);
            record["runs"][0]["sessionId"].is_string()
        },
    );
    assert_eq!(record["runs"][0]["outcome"], "onTime");
    assert_eq!(record["enabled"], false, "a one-shot ends after its run");
    assert!(record.get("nextDue").is_none());
    kill_run(&mut control, &record);
    let sessions = control.request("session.list", json!({}));
    let session = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == record["runs"][0]["sessionId"])
        .unwrap();
    assert_eq!(session["scheduledRun"]["scheduleId"], id);
    assert_eq!(session["scheduledRun"]["wokeMac"], false);
}

#[test]
fn an_occurrence_missed_while_not_running_fires_once_late() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let marker = temp.path().join("caught-up");

    let created = control.request(
        "schedule.create",
        spec(
            &marker,
            json!({ "kind": "cron", "expr": "0 9 29 2 *" }),
            12 * 3_600_000,
        ),
    );
    let id = created["id"].as_str().unwrap().to_owned();
    // Due ten minutes ago, before this Engine started: the 9:00-but-it's-9:10 case.
    // A leap-day expression keeps the schedule's own next occurrence years
    // away: a daily 9:00 made this run on time whenever CI ran at 9:00-9:10.
    backdate(temp.path(), &id, now_ms() - 10.0 * 60_000.0);
    server.spawn_scheduler();

    wait_until("the missed run caught up", Duration::from_secs(30), || {
        marker.exists()
    });
    let mut record = control.schedule(&id);
    wait_until(
        "the run recorded its session",
        Duration::from_secs(10),
        || {
            record = control.schedule(&id);
            record["runs"][0]["sessionId"].is_string()
        },
    );
    let runs = record["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "caught up exactly once: {runs:?}");
    assert_eq!(runs[0]["outcome"], "late");
    assert_eq!(runs[0]["lateReason"], "notRunning");
    assert_eq!(record["enabled"], true);
    assert!(
        record["nextDue"].as_f64().unwrap() > now_ms(),
        "re-armed ahead"
    );
    kill_run(&mut control, &record);
}

#[test]
fn an_occurrence_past_its_window_is_recorded_missed_and_not_run() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let marker = temp.path().join("should-not-exist");

    let created = control.request(
        "schedule.create",
        spec(
            &marker,
            json!({ "kind": "cron", "expr": "0 9 29 2 *" }),
            60_000,
        ),
    );
    let id = created["id"].as_str().unwrap().to_owned();
    // Leap day, as above: no real occurrence can fall inside the backdate.
    backdate(temp.path(), &id, now_ms() - 10.0 * 60_000.0);
    server.spawn_scheduler();

    let mut record = control.schedule(&id);
    wait_until("the miss was recorded", Duration::from_secs(10), || {
        record = control.schedule(&id);
        record["runs"]
            .as_array()
            .is_some_and(|runs| !runs.is_empty())
    });
    assert_eq!(record["runs"][0]["outcome"], "missed");
    assert!(record["runs"][0].get("sessionId").is_none());
    std::thread::sleep(Duration::from_secs(2));
    assert!(!marker.exists(), "a missed run must not start anything");
    assert!(record["nextDue"].as_f64().unwrap() > now_ms());
}

#[test]
fn run_now_starts_a_run_and_keeps_the_next_due_time() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let marker = temp.path().join("manual");

    let created = control.request(
        "schedule.create",
        spec(
            &marker,
            json!({ "kind": "cron", "expr": "0 9 * * *" }),
            60_000,
        ),
    );
    let id = created["id"].as_str().unwrap().to_owned();
    control.request("schedule.run_now", json!({ "id": id }));
    wait_until("the manual run ran", Duration::from_secs(30), || {
        marker.exists()
    });
    let mut record = control.schedule(&id);
    wait_until(
        "the run recorded its session",
        Duration::from_secs(10),
        || {
            record = control.schedule(&id);
            record["runs"][0]["sessionId"].is_string()
        },
    );
    assert_eq!(record["runs"][0]["outcome"], "manual");
    assert_eq!(record["nextDue"], created["nextDue"]);
    kill_run(&mut control, &record);

    control.request("schedule.delete", json!({ "id": id }));
    assert!(
        control.request("schedule.list", json!({}))["schedules"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        control
            .try_request("schedule.delete", json!({ "id": id }))
            .is_err()
    );
}

/// Each run's session id, oldest first, once every run has recorded one.
fn run_sessions(control: &mut Control, id: &str, runs: usize) -> Vec<String> {
    let mut sessions = Vec::new();
    wait_until(
        "every run recorded its session",
        Duration::from_secs(30),
        || {
            let record = control.schedule(id);
            sessions = record["runs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|run| run["sessionId"].as_str().map(str::to_owned))
                .collect();
            sessions.len() == runs
        },
    );
    sessions
}

#[test]
fn a_repeating_schedule_keeps_one_session_until_it_is_closed() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let marker = temp.path().join("run");
    // The prompt touches a marker named by a counter the shell keeps, so
    // the second run is visible only if it reached the first run's shell.
    let mut schedule = spec(
        &marker,
        json!({ "kind": "cron", "expr": "0 9 * * *" }),
        60_000,
    );
    schedule["spawn"]["initialPrompt"] =
        json!(format!("n=$((n+1)); touch '{}'-$n", marker.display()));
    let id = control.request("schedule.create", schedule)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let fired = |n: u32| temp.path().join(format!("run-{n}"));

    control.request("schedule.run_now", json!({ "id": id }));
    wait_until("the first run ran", Duration::from_secs(30), || {
        fired(1).exists()
    });
    let first = run_sessions(&mut control, &id, 1);

    control.request("schedule.run_now", json!({ "id": id }));
    wait_until(
        "the second run reached the same shell",
        Duration::from_secs(30),
        || fired(2).exists(),
    );
    let sessions = run_sessions(&mut control, &id, 2);
    assert_eq!(sessions[1], first[0], "the second run reuses the session");
    let list = control.request("session.list", json!({}));
    assert_eq!(
        list["sessions"].as_array().unwrap().len(),
        1,
        "no second tab"
    );

    // Closing the tab ends the conversation: the next run opens a new one.
    control.request("session.remove", json!({ "sessionID": first[0] }));
    control.request("schedule.run_now", json!({ "id": id }));
    let sessions = run_sessions(&mut control, &id, 3);
    assert_ne!(sessions[2], first[0]);
    wait_until(
        "the fresh shell ran its prompt",
        Duration::from_secs(30),
        || fired(1).exists(),
    );
    control.request("session.kill", json!({ "sessionID": sessions[2] }));
}

#[test]
fn invalid_schedules_are_rejected_before_storage() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let marker = temp.path().join("never");
    let error = control
        .try_request(
            "schedule.create",
            spec(&marker, json!({ "kind": "cron", "expr": "at nine" }), 0),
        )
        .unwrap_err();
    assert!(error.contains("five fields"), "{error}");
    let error = control
        .try_request(
            "schedule.create",
            spec(
                &marker,
                json!({ "kind": "once", "at": now_ms() - 3_600_000.0 }),
                0,
            ),
        )
        .unwrap_err();
    assert!(error.contains("past"), "{error}");
    assert!(
        control.request("schedule.list", json!({}))["schedules"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Auto-acknowledging helper on the fixture socket; no operating-system power APIs.
fn fake_helper(temp: &Path) -> std::sync::mpsc::Receiver<diri_engine::wake::Request> {
    let listener = std::os::unix::net::UnixListener::bind(temp.join("wake.sock")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let line = diri_engine::wake::read_frame(&mut stream).unwrap();
            let request = serde_json::from_str(&line).unwrap();
            if tx.send(request).is_err() {
                break;
            }
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        }
    });
    rx
}

#[test]
fn engine_syncs_disables_deletes_and_clears_after_restart() {
    use diri_engine::wake::Request;
    let temp = tempfile::tempdir().unwrap();
    let requests = fake_helper(temp.path());
    let server = start_server(temp.path());
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let due = (now_ms() / 1000.0).floor() * 1000.0 + 600_000.0;
    let mut schedule = spec(
        &temp.path().join("unused"),
        json!({"kind":"once", "at":due}),
        60_000,
    );
    schedule["wakeMac"] = json!(true);
    let created = control.request("schedule.create", schedule.clone());
    let id = created["id"].clone();
    let receive = || requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let expected = Request::SetWakes {
        times_ms: vec![due as i64 - diri_engine::wake::WAKE_LEAD_MS],
    };
    assert_eq!(receive(), expected);
    schedule["enabled"] = json!(false);
    control.request("schedule.update", {
        let mut update = schedule.clone();
        update["id"] = id.clone();
        update
    });
    assert_eq!(receive(), Request::SetWakes { times_ms: vec![] });
    schedule["enabled"] = json!(true);
    control.request("schedule.update", {
        let mut update = schedule.clone();
        update["id"] = id.clone();
        update
    });
    assert_eq!(receive(), expected);
    control.request("schedule.delete", json!({"id":id}));
    assert_eq!(receive(), Request::SetWakes { times_ms: vec![] });

    // Restore only the journal into a fresh Engine: no schedules remain, but
    // it must still reconcile power events left by a lost clear acknowledgement.
    let restarted = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(temp.path().join("schedules-v1.sqlite")).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    std::fs::copy(
        temp.path().join("schedules-v1.sqlite"),
        restarted.path().join("schedules-v1.sqlite"),
    )
    .unwrap();
    let restart_requests = fake_helper(restarted.path());
    let restarted_server = start_server(restarted.path());
    restarted_server.spawn_scheduler();
    assert_eq!(
        restart_requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        Request::SetWakes { times_ms: vec![] }
    );
}

#[test]
fn absent_helper_surfaces_error_without_preventing_schedules() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let mut schedule = spec(
        &temp.path().join("unused"),
        json!({"kind":"once", "at":now_ms() + 600_000.0}),
        60_000,
    );
    schedule["wakeMac"] = json!(true);
    let created = control.request("schedule.create", schedule);
    wait_until("wake helper error", Duration::from_secs(5), || {
        control.request("schedule.list", json!({}))["wakeHelperError"]
            .as_str()
            .is_some_and(|error| error.contains("isn't available"))
    });
    assert!(
        control.schedule(created["id"].as_str().unwrap())["enabled"]
            .as_bool()
            .unwrap()
    );
    control.request("schedule.delete", json!({"id":created["id"]}));
}

#[test]
fn a_failed_prompt_keeps_the_created_session_link_and_schedule_stamp() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    server.spawn_scheduler();
    let mut control = Control::connect(&server);
    let mut schedule = spec(
        &temp.path().join("unused"),
        json!({"kind":"once", "at":now_ms() + 600_000.0}),
        60_000,
    );
    schedule["spawn"]["kind"] = json!({"generic":{"command":"exit 0"}});
    let created = control.request("schedule.create", schedule);
    let id = created["id"].as_str().unwrap();
    control.request("schedule.run_now", json!({"id":id}));
    let mut record = control.schedule(id);
    wait_until("failed initial prompt", Duration::from_secs(20), || {
        record = control.schedule(id);
        record["runs"][0]["outcome"] == "failed"
    });
    assert!(
        record["runs"][0]["sessionId"].is_string(),
        "a created session must remain inspectable: {record}"
    );
    let listed = control.request("session.list", json!({}));
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "an uncertain prompt must not spawn twice"
    );
    assert_eq!(sessions[0]["id"], record["runs"][0]["sessionId"]);
    assert_eq!(sessions[0]["scheduledRun"]["scheduleId"], id);
    control.request("schedule.delete", json!({"id":id}));
}
