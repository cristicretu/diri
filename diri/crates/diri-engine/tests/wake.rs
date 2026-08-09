//! Wake-on-input, end to end: the app NEVER calls `session.wake` — it relies
//! on the daemon waking a hibernated session implicitly when the user types
//! (control `session.send_text`) or selects it (data-channel attach). These
//! tests freeze a real held session and prove, from the outside, that the
//! child tree leaves SIGSTOP (its `ps` state stops reading `T`) and the input
//! actually reaches the child.

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
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::json;

fn engine() -> Arc<ManifestEngine> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../Sources/DirijorCore/Resources/manifests")
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// One daemon in miniature: control server on a private socket, holder-backed
/// spawns — the exact shape `dirijord-rs` runs in production.
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
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read reply");
        match serde_json::from_str::<ControlMessage>(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result,
            other => panic!("{method} failed: {other:?}"),
        }
    }
}

/// Spawns a held `cat`, waits for its prompt echo path to be live, and
/// returns its session id.
fn spawn_cat(control: &mut Control) -> String {
    let spawned = control.request(
        "session.spawn",
        json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", "printf 'cat-ready\\n'; exec cat"],
        }),
    );
    let id = spawned["id"].as_str().expect("session id").to_string();
    wait_until(
        "the child painted its banner",
        Duration::from_secs(10),
        || {
            control.request("session.read_screen", json!({ "sessionID": id }))["text"]
                .as_str()
                .is_some_and(|text| text.contains("cat-ready"))
        },
    );
    id
}

/// The `ps` state letter for each pid; `T` is stopped. A pid that is gone
/// reads as gone — the assertions below treat that as failure, since the
/// whole point of hibernation is that the tree stays alive.
fn ps_states(pids: &[i64]) -> Vec<(i64, String)> {
    pids.iter()
        .map(|pid| {
            let output = std::process::Command::new("ps")
                .args(["-o", "state=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            (
                *pid,
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )
        })
        .collect()
}

fn tree_pids(control: &mut Control, id: &str) -> Vec<i64> {
    let list = control.request("session.list", json!({}));
    let session = list["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|session| session["id"] == id)
        .expect("our session")
        .clone();
    session["hibernation"]["treePids"]
        .as_array()
        .expect("a hibernated record carries its tree pids")
        .iter()
        .map(|pid| pid.as_i64().expect("pid"))
        .collect()
}

fn hibernation_cleared(control: &mut Control, id: &str) -> bool {
    let list = control.request("session.list", json!({}));
    list["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|session| session["id"] == id)
        .is_some_and(|session| session["hibernation"].is_null())
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("timed out waiting for {what}");
}

fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

/// Freezes the session over the socket and proves the tree really stopped.
fn hibernate_and_verify_stopped(control: &mut Control, id: &str) -> Vec<i64> {
    control.request("session.hibernate", json!({ "sessionID": id }));
    let pids = tree_pids(control, id);
    assert!(!pids.is_empty(), "a held tree reports its pids");
    // SIGSTOP is not instantaneous for a whole tree; give it a beat.
    wait_until("the whole tree to stop", Duration::from_secs(5), || {
        ps_states(&pids)
            .iter()
            .all(|(_, state)| state.starts_with('T'))
    });
    pids
}

/// Typing wakes: `session.send_text` — the only input path the app uses from
/// the composer — must SIGCONT the tree and deliver the text, with no
/// `session.wake` anywhere in sight.
#[test]
fn send_text_wakes_a_hibernated_tree_and_delivers_the_text() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let id = spawn_cat(&mut control);
    let pids = hibernate_and_verify_stopped(&mut control, &id);

    control.request(
        "session.send_text",
        json!({ "sessionID": id, "text": "typed-into-a-frozen-session\n", "submit": false }),
    );

    // The tree is SIGCONT-ed…
    wait_until("the tree to resume", Duration::from_secs(5), || {
        ps_states(&pids)
            .iter()
            .all(|(_, state)| !state.is_empty() && !state.starts_with('T'))
    });
    // …the text reaches the child (cat echoes it back to the screen)…
    wait_until("the echo to land", Duration::from_secs(10), || {
        control.request("session.read_screen", json!({ "sessionID": id }))["text"]
            .as_str()
            .is_some_and(|text| text.contains("typed-into-a-frozen-session"))
    });
    // …and the record no longer claims hibernation.
    assert!(
        hibernation_cleared(&mut control, &id),
        "waking must clear the hibernation record"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}

/// Selecting wakes: a data-channel attach alone — before any keystroke —
/// must SIGCONT the tree, and input frames typed through the channel land.
#[test]
fn a_data_channel_attach_wakes_a_hibernated_tree() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let id = spawn_cat(&mut control);
    let pids = hibernate_and_verify_stopped(&mut control, &id);

    // Attach the way the app's terminal does: one JSON line, then frames.
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach_line = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach_line.push(b'\n');
    data.write_all(&attach_line).expect("attach");

    // The attach itself is the wake trigger.
    wait_until(
        "the tree to resume on attach",
        Duration::from_secs(5),
        || {
            ps_states(&pids)
                .iter()
                .all(|(_, state)| !state.is_empty() && !state.starts_with('T'))
        },
    );
    assert!(
        hibernation_cleared(&mut control, &id),
        "an attach must clear the hibernation record"
    );

    // And typing through the channel reaches the (now running) child.
    data.write_all(
        &FrameCodec::encode(&Frame::input(b"typed-over-the-channel\n".to_vec())).expect("encode"),
    )
    .expect("send input");
    let mut codec = FrameCodec::new();
    let mut chunk = [0u8; 64 << 10];
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut echoed = false;
    'read: while Instant::now() < deadline {
        let count = data.read(&mut chunk).expect("read frames");
        if count == 0 {
            break;
        }
        for frame in codec.feed(&chunk[..count]).expect("valid frames") {
            if frame.frame_type != FrameType::Grid {
                continue;
            }
            let Some(update) = frame.grid_payload().ok().flatten() else {
                continue;
            };
            let text = update
                .changed_rows
                .iter()
                .map(|row| {
                    row.cells
                        .iter()
                        .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.contains("typed-over-the-channel") {
                echoed = true;
                break 'read;
            }
        }
    }
    assert!(
        echoed,
        "input typed over the data channel never echoed back"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}

/// A daemon can adopt a holder whose process tree is already SIGSTOPped while
/// its persisted hibernation marker is stale or missing. That inconsistent
/// state used to look live in the UI, but attaching and typing only wrote into
/// a stopped PTY forever. The attach boundary must reconcile the real process
/// state instead of trusting the record blindly.
#[test]
fn a_data_channel_attach_recovers_a_stopped_tree_with_stale_metadata() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let id = spawn_cat(&mut control);
    let pids = hibernate_and_verify_stopped(&mut control, &id);
    control.request("session.wake", json!({ "sessionID": id }));
    wait_until("the normal wake to finish", Duration::from_secs(5), || {
        hibernation_cleared(&mut control, &id)
            && ps_states(&pids)
                .iter()
                .all(|(_, state)| !state.is_empty() && !state.starts_with('T'))
    });

    // Recreate the production inconsistency: the whole tree is stopped, but
    // neither the record nor Session's in-memory flag knows it is hibernated.
    for pid in &pids {
        // SAFETY: these are live child pids returned by the private test
        // holder, and the test terminates the session before returning.
        unsafe { libc::kill(*pid as i32, libc::SIGSTOP) };
    }
    wait_until(
        "the externally stopped tree",
        Duration::from_secs(5),
        || {
            ps_states(&pids)
                .iter()
                .all(|(_, state)| state.starts_with('T'))
        },
    );
    assert!(
        hibernation_cleared(&mut control, &id),
        "the premise: process state and persisted metadata disagree"
    );

    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach_line = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach_line.push(b'\n');
    data.write_all(&attach_line).expect("attach");
    data.write_all(
        &FrameCodec::encode(&Frame::input(b"typed-into-a-stale-stop\n".to_vec())).expect("encode"),
    )
    .expect("send input");

    let resumed = eventually(Duration::from_secs(2), || {
        ps_states(&pids)
            .iter()
            .all(|(_, state)| !state.is_empty() && !state.starts_with('T'))
    });
    let echoed = resumed
        && eventually(Duration::from_secs(2), || {
            control.request("session.read_screen", json!({ "sessionID": id }))["text"]
                .as_str()
                .is_some_and(|text| text.contains("typed-into-a-stale-stop"))
        });

    control.request("session.kill", json!({ "sessionID": id }));
    assert!(resumed, "attach left the stale-stopped process tree frozen");
    assert!(echoed, "input never reached the stale-stopped session");
}
