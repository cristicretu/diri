//! Engine resources return to their baseline after the churn a long-running
//! desktop session produces: a CLI connection per agent hook, tab switches
//! (attach/detach), event subscriptions, sessions spawned, killed and
//! removed, and hibernate/wake.
//!
//! Real telemetry (4.7 h, 0.8.10) showed Engine threads 47 → 59 and fds
//! 176 → 204. Both series were explained, minute by minute, by the live
//! gauges (records, clients, attachments), not by time or churn. This test
//! pins that property: after N cycles of each kind of churn, threads and
//! descriptors must come back to what the same Engine used after one warm-up
//! cycle. A leaked connection, attach or subscription thread, or a descriptor
//! kept by a removed session, fails it.
//!
//! One test in its own binary: thread and descriptor counts are per process,
//! and parallel tests in the same binary would count each other's.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_telemetry::ProcessStats;
use serde_json::json;

const CYCLES: usize = 40;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// The daemon's shape: private socket, a thread per accepted connection,
/// holder-backed sessions.
fn start_server(temp: &Path) -> Arc<ControlServer> {
    let registry = Arc::new(Mutex::new(Registry::new(engine(), temp.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.join("holders"),
                executable: env!("CARGO_BIN_EXE_diri-holder").into(),
            }),
    );
    let listener = server.bind().expect("bind");
    let accepting = Arc::clone(&server);
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let server = Arc::clone(&accepting);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
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
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let reader = BufReader::new(stream.try_clone().expect("clone"));
        Self {
            stream,
            reader,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
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
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("read reply");
            match serde_json::from_str::<ControlMessage>(&line).expect("decode") {
                ControlMessage::Response {
                    id: reply,
                    result: Ok(result),
                } if reply == id => return result,
                ControlMessage::Response { id: reply, result } if reply == id => {
                    panic!("{method} failed: {result:?}")
                }
                // Events on a subscribed connection interleave with replies.
                _ => {}
            }
        }
    }
}

fn spawn(control: &mut Control, script: &str) -> String {
    let spawned = control.request(
        "session.spawn",
        json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": ["/bin/sh", "-c", script] }),
    );
    spawned["id"].as_str().expect("session id").to_owned()
}

/// Waits until the child has painted, so its holder is serving.
fn spawn_painted(control: &mut Control, script: &str) -> String {
    let id = spawn(control, script);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if control.request("session.read_screen", json!({ "sessionID": id }))["text"]
            .as_str()
            .is_some_and(|text| text.contains("ready"))
        {
            return id;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("{id} never painted");
}

/// A `dirijor hook` process: connect, Hello, one report, close.
fn hook_connection(server: &ControlServer, session: &str) {
    let mut hook = Control::connect(server);
    hook.request(
        "hello",
        json!({ "proto": diri_proto::WIRE_VERSION, "build": "hook" }),
    );
    hook.request(
        "hook.report",
        json!({
            "kind": "claude-hook",
            "dirijorSessionID": session,
            "event": "Stop",
            "payload": {},
        }),
    );
}

/// A tab switch: attach, read the first frames for a moment, close.
fn attach_and_detach(server: &ControlServer, session: &str) {
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut line = serde_json::to_vec(&json!({ "attach": session })).expect("encode");
    line.push(b'\n');
    data.write_all(&line).expect("attach");
    data.set_read_timeout(Some(Duration::from_millis(50)))
        .expect("timeout");
    let mut buffer = [0u8; 16 * 1024];
    let until = Instant::now() + Duration::from_millis(60);
    while Instant::now() < until {
        match data.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// An app window's event stream, opened and dropped.
fn subscribe_and_close(server: &ControlServer) {
    let mut client = Control::connect(server);
    client.request(
        "hello",
        json!({ "proto": diri_proto::WIRE_VERSION, "build": "app" }),
    );
    client.request("events.subscribe", json!({}));
}

fn spawn_kill_remove(control: &mut Control) {
    let id = spawn(control, "sleep 600");
    control.request("session.kill", json!({ "sessionID": id }));
    control.request("session.remove", json!({ "sessionID": id }));
}

fn churn(server: &ControlServer, control: &mut Control, idle: &str, busy: &str, cycles: usize) {
    for index in 0..cycles {
        hook_connection(server, idle);
        attach_and_detach(server, if index % 2 == 0 { idle } else { busy });
        subscribe_and_close(server);
        control.request("session.hibernate", json!({ "sessionID": idle }));
        control.request("session.wake", json!({ "sessionID": idle }));
        if index % 4 == 0 {
            spawn_kill_remove(control);
        }
    }
}

/// Waits for detached per-connection work to wind down (event forwarders poll
/// their stop flag every 250 ms), then samples.
fn settled(baseline: ProcessStats) -> ProcessStats {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = ProcessStats::current();
        if (now.threads <= baseline.threads && now.open_fds <= baseline.open_fds)
            || Instant::now() >= deadline
        {
            return now;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Preserve the identities behind a Linux count failure, so a single late
/// worker/file can be distinguished from a leaked connection on CI.
fn descriptor_targets() -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let mut targets: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            std::fs::read_link(entry.path())
                .ok()
                .map(|target| (entry.file_name().to_string_lossy().into_owned(), target))
        })
        .collect();
    targets.sort();
    targets
}

/// Kills the long-lived sessions however the test ends, so a failure does
/// not leave Holders and their children running.
struct KillOnDrop<'a> {
    server: &'a ControlServer,
    ids: Vec<String>,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let Ok(mut stream) = UnixStream::connect(self.server.socket_path()) else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(clone) => clone,
            Err(_) => return,
        });
        for (index, id) in self.ids.iter().enumerate() {
            let request =
                json!({ "id": index + 1, "method": "session.kill", "params": { "sessionID": id } });
            if writeln!(stream, "{request}").is_err() {
                return;
            }
            let _ = reader.read_line(&mut String::new());
        }
    }
}

#[test]
fn threads_and_descriptors_return_to_baseline_after_churn() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    control.request(
        "hello",
        json!({ "proto": diri_proto::WIRE_VERSION, "build": "test" }),
    );
    let idle = spawn_painted(&mut control, "printf ready; while :; do sleep 5; done");
    let busy_gate = temp.path().join("start-output");
    let quoted_gate = busy_gate.to_string_lossy().replace('\'', "'\\''");
    let busy = spawn_painted(
        &mut control,
        &format!(
            "printf ready; while [ ! -f '{quoted_gate}' ]; do sleep 0.02; done; while :; do printf 'tick %s\\n' \"$$\"; sleep 0.02; done"
        ),
    );
    let _cleanup = KillOnDrop {
        server: &server,
        ids: vec![idle.clone(), busy.clone()],
    };

    // One cycle of everything first: lazily started workers (the holder
    // manager connection, the persist flusher, the activity log) belong to
    // the baseline, not to growth.
    churn(&server, &mut control, &idle, &busy, 4);
    // Both followers need an initially drained log to establish their output
    // subscriptions. Continuous output from process startup can defer that
    // subscription until after the baseline, counting one legitimate retained
    // socket as a leak on Linux. Keep both children quiet through warm-up,
    // then measure the same idle/busy workload before and after churn.
    std::thread::sleep(Duration::from_millis(600));
    std::fs::write(&busy_gate, b"start").expect("start busy output");
    std::thread::sleep(Duration::from_millis(600));
    let baseline = ProcessStats::current();
    let baseline_targets = descriptor_targets();

    churn(&server, &mut control, &idle, &busy, CYCLES);
    let after = settled(baseline);

    assert!(
        after.threads <= baseline.threads,
        "threads grew from {} to {} over {CYCLES} churn cycles",
        baseline.threads,
        after.threads
    );
    assert!(
        after.open_fds <= baseline.open_fds,
        "descriptors grew from {} to {} over {CYCLES} churn cycles; before: {baseline_targets:?}; after: {:?}",
        baseline.open_fds,
        after.open_fds,
        descriptor_targets(),
    );
}
