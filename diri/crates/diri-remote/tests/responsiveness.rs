#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::ManifestEngine;
use diri_engine::control::ControlServer;
use diri_engine::registry::Registry;
use diri_engine::remote::executor::ProcessExecutor;
use diri_engine::remote::manager::{ArtifactCatalog, RemoteManager};
use diri_proto::{ControlMessage, Method, WIRE_VERSION};
use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
    registry: Arc<Mutex<Registry>>,
    server: Arc<ControlServer>,
    peer: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let ssh = root.path().join("ssh");
        fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
export HOME='{home}'
export DIRI_REMOTE_STATE_DIR='{root}/remote-state'
for last; do :; done
case "$last" in
  *' launch'*) operation=launch;;
  *' kill'*) operation=kill;;
  *' attach'*) echo $$ > '{root}/bridge.pid'; operation=attach;;
  *) operation=other;;
esac
if [ -f '{root}/fail-'"$operation" ]; then exit 75; fi
if [ -f '{root}/delay-'"$operation" ]; then
  touch '{root}/entered'
  sleep 0.8
fi
exec /bin/sh -c "$last"
"#,
                home = home.display(),
                root = root.path().display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            root.path().join("hosts.json"),
            serde_json::to_vec(&json!({
                "hosts":[{"id":"fixture", "ssh":"fixture", "defaultCwd":"/"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let (engine, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(engine),
            root.path().join("state.json"),
        )));
        let manager = Arc::new(
            RemoteManager::new(
                ProcessExecutor::new(ssh),
                ArtifactCatalog::from_native_helper(Path::new(env!("CARGO_BIN_EXE_diri-remote")))
                    .unwrap(),
                root.path().join("control"),
            )
            .unwrap(),
        );
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), root.path().join("daemon.sock"))
                .with_remote(manager),
        );
        let peer = rpc(
            &server,
            Method::SESSION_SPAWN,
            Some(json!({
                "kind":{"shell":{}}, "cwd":"/", "argv":["/bin/sh", "-c", "printf ready; exec cat"]
            })),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            root,
            registry,
            server,
            peer,
        }
    }

    fn remote_params() -> Value {
        json!({"kind":{"shell":{}}, "cwd":"/", "host":"fixture", "argv":["/bin/sh", "-c", "printf ready; exec cat"]})
    }

    fn remote(&self) -> String {
        rpc(
            &self.server,
            Method::SESSION_SPAWN,
            Some(Self::remote_params()),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for operation in ["launch", "kill"] {
            let _ = fs::remove_file(self.root.path().join(format!("delay-{operation}")));
            let _ = fs::remove_file(self.root.path().join(format!("fail-{operation}")));
        }
        let mut registry = self.registry.lock().unwrap();
        let records = registry.records();
        for record in records {
            let _ = registry.terminate(&record.id.0, Duration::from_millis(100));
        }
    }
}

fn delayed_lifecycle_isolated(operation: &str) {
    let fixture = Fixture::new();
    let remote = (operation != "launch").then(|| fixture.remote());
    let delayed_command = if operation == "launch" {
        "launch"
    } else {
        "kill"
    };
    fs::write(
        fixture.root.path().join(format!("delay-{delayed_command}")),
        b"",
    )
    .unwrap();
    let (mut stream, connection) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let worker = {
        let server = Arc::clone(&fixture.server);
        std::thread::spawn(move || server.serve(connection).unwrap())
    };
    let send = |stream: &mut UnixStream, id, method: &str, params| {
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).unwrap();
    };
    let (method, params) = match remote {
        Some(ref id) => (
            match operation {
                "archive" => Method::SESSION_ARCHIVE,
                "remove" => Method::SESSION_REMOVE,
                _ => Method::SESSION_KILL,
            },
            json!({"sessionID":id}),
        ),
        None => (Method::SESSION_SPAWN, Fixture::remote_params()),
    };
    send(&mut stream, 1, method, params);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !fixture.root.path().join("entered").exists() {
        assert!(
            Instant::now() < deadline,
            "remote {operation} was not reached"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let start = Instant::now();
    send(
        &mut stream,
        2,
        Method::HELLO,
        json!({"proto":WIRE_VERSION, "build":"test"}),
    );
    send(
        &mut stream,
        3,
        Method::SESSION_SEND_TEXT,
        json!({"sessionID":fixture.peer, "text":"responsive", "submit":true}),
    );
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut latencies = [Duration::ZERO; 4];
    for _ in 0..3 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ControlMessage::Response { id, result } = serde_json::from_str(&line).unwrap() else {
            panic!("response")
        };
        assert!(result.is_ok(), "{result:?}");
        latencies[id as usize] = start.elapsed();
    }
    if operation == "archive" {
        let registry = fixture.registry.lock().unwrap();
        let record = registry
            .records()
            .into_iter()
            .find(|record| Some(&record.id.0) == remote.as_ref())
            .unwrap();
        assert!(record.archived_at.is_some());
        assert!(matches!(
            record.status,
            diri_proto::SessionStatus::Exited(diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::Archived,
                ..
            })
        ));
    }
    drop(reader);
    drop(stream);
    worker.join().unwrap();
    eprintln!(
        "{operation}: remote {:?}, hello {:?}, peer input {:?}",
        latencies[1], latencies[2], latencies[3]
    );
    assert!(
        latencies[2] < Duration::from_millis(400),
        "Hello blocked on remote {operation}"
    );
    assert!(
        latencies[3] < Duration::from_millis(400),
        "peer input blocked on remote {operation}"
    );
}

#[test]
fn remote_launch_does_not_block_peer_input() {
    delayed_lifecycle_isolated("launch");
}

#[test]
fn remote_stop_does_not_block_peer_input() {
    delayed_lifecycle_isolated("kill");
}

fn rpc(
    server: &Arc<ControlServer>,
    method: &str,
    params: Option<Value>,
) -> Result<Value, diri_proto::ControlError> {
    let (mut stream, connection) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let server = Arc::clone(server);
    let worker = std::thread::spawn(move || server.serve(connection).unwrap());
    let mut bytes = serde_json::to_vec(&ControlMessage::Request {
        id: 1,
        method: method.into(),
        params,
    })
    .unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let ControlMessage::Response { result, .. } = serde_json::from_str(&line).unwrap() else {
        panic!("response")
    };
    drop(reader);
    worker.join().unwrap();
    result
}

#[test]
fn stalled_ssh_input_does_not_block_peer_input() {
    let fixture = Fixture::new();
    let mut params = Fixture::remote_params();
    params["argv"] = json!([
        "/bin/sh",
        "-c",
        "stty raw -echo; printf ready; count=$(head -c 262144 | wc -c); printf 'received:%s' \"$count\"; exec cat"
    ]);
    let id = rpc(&fixture.server, Method::SESSION_SPAWN, Some(params)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture
        .registry
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .screen_lines()
        .join("")
        .contains("ready")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // FullSnapshot can arrive just before ControlGranted. Require a live
    // controller so this tests pipe backpressure, not the reconnect queue.
    while fixture
        .registry
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .signal_tree(libc::SIGCONT)
        .is_err()
    {
        assert!(Instant::now() < deadline, "controller was not granted");
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = fs::read_to_string(fixture.root.path().join("bridge.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the PID belongs to this fixture's single disposable SSH bridge.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let resume = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        // SAFETY: the fixture remains alive until this thread joins.
        unsafe {
            libc::kill(pid, libc::SIGCONT);
        }
    });
    let started = Instant::now();
    let sent = rpc(
        &fixture.server,
        Method::SESSION_SEND_TEXT,
        Some(json!({"sessionID":id, "text":"x".repeat(262144), "submit":false})),
    );
    let input_elapsed = started.elapsed();
    let peer_started = Instant::now();
    let peer = rpc(
        &fixture.server,
        Method::SESSION_SEND_TEXT,
        Some(json!({"sessionID":fixture.peer, "text":"peer", "submit":true})),
    );
    let peer_elapsed = peer_started.elapsed();
    resume.join().unwrap();
    sent.unwrap();
    peer.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = fixture
            .registry
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .screen_lines()
            .join("");
        if screen.contains("262144") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "accepted paste did not reach the PTY intact"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("stalled SSH: paste {input_elapsed:?}, peer {peer_elapsed:?}");
    assert!(
        input_elapsed < Duration::from_millis(400),
        "input waited for SSH to resume"
    );
    assert!(peer_elapsed < Duration::from_millis(400));
}

#[test]
fn remote_archive_does_not_block_peer_input() {
    delayed_lifecycle_isolated("archive");
}
#[test]
fn remote_remove_does_not_block_peer_input() {
    delayed_lifecycle_isolated("remove");
}

#[test]
fn failed_remote_stop_keeps_the_original_session_tracked() {
    let fixture = Fixture::new();
    let mut params = Fixture::remote_params();
    params["argv"] = json!(["/bin/sh", "-c", "trap '' TERM; printf ready; exec cat"]);
    let id = rpc(&fixture.server, Method::SESSION_SPAWN, Some(params)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture
        .registry
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .screen_lines()
        .join("")
        .contains("ready")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(fixture.root.path().join("fail-kill"), b"").unwrap();
    let result = rpc(
        &fixture.server,
        Method::SESSION_KILL,
        Some(json!({"sessionID":id})),
    );
    fs::remove_file(fixture.root.path().join("fail-kill")).unwrap();
    assert!(result.is_err());
    assert!(fixture.registry.lock().unwrap().get(&id).is_some());
}
