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
    let dir = diri_engine::detect::bundled_manifest_dir()
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

fn preview_socket(server: &ControlServer, id: &str) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(server.socket_path()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(stream, "{}", json!({"preview": id, "version": 1})).unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let ready: diri_proto::preview::PreviewReady = serde_json::from_str(&line).unwrap();
    assert_eq!(ready.preview.0, id);
    reader
}

fn preview_frame(reader: &mut impl Read) -> Frame {
    let mut codec = FrameCodec::new();
    loop {
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        if let Some(frame) = codec.feed(&byte).unwrap().pop() {
            return frame;
        }
    }
}

#[test]
fn previews_are_bounded_read_only_and_do_not_wake_a_stopped_tree() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let id = spawn_cat(&mut control);
    let pids = hibernate_and_verify_stopped(&mut control, &id);
    let before = control.request("session.list", json!({}))["sessions"][0].clone();
    let mut previews = Vec::new();
    for _ in 0..diri_proto::preview::MAX_PREVIEWS {
        let mut reader = preview_socket(&server, &id);
        let grid = preview_frame(&mut reader).grid_payload().unwrap().unwrap();
        assert_eq!(grid.changed_rows.len(), usize::from(grid.rows));
        assert_eq!((grid.cols, grid.rows), (80, 24));
        assert_eq!(preview_frame(&mut reader).frame_type, FrameType::Modes);
        previews.push(reader);
    }
    assert!(
        !server.attach_hub().has_sinks(&id),
        "previews are not governor visibility"
    );
    let mut excess_set = preview_set_socket(&server);
    excess_set.set(vec![preview_member(&id, 1)]);
    assert!(
        matches!(
            excess_set.next(),
            diri_proto::preview_set::PreviewSetPacket::Unavailable {
                reason: diri_proto::preview_set::PreviewUnavailable::AdmissionLimit,
                ..
            }
        ),
        "single and multiplexed previews share the admission budget"
    );
    drop(excess_set);
    let mut excess = UnixStream::connect(server.socket_path()).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(excess, "{}", json!({"preview": id, "version": 1})).unwrap();
    assert_eq!(
        excess.read(&mut [0]).unwrap(),
        0,
        "seventeenth preview rejected"
    );
    for frame in [
        Frame::input(b"must-not-run\n".to_vec()),
        Frame::mouse(b"x".to_vec()),
        Frame::resize(132, 42),
        Frame::scroll(0, 1, 0, 0),
    ] {
        let mut reader = previews.pop().unwrap();
        reader
            .get_mut()
            .write_all(&FrameCodec::encode(&frame).unwrap())
            .unwrap();
        assert_eq!(reader.read(&mut [0]).unwrap(), 0, "mutation closes preview");
    }
    for request in [
        json!({"preview": id, "attach": id, "version": 1}),
        json!({"preview": id, "version": 99}),
    ] {
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
    let mut replacement = preview_socket(&server, &id);
    let grid = preview_frame(&mut replacement)
        .grid_payload()
        .unwrap()
        .unwrap();
    assert_eq!((grid.cols, grid.rows), (80, 24), "resize was rejected");
    preview_frame(&mut replacement);
    replacement
        .get_mut()
        .write_all(&FrameCodec::encode(&Frame::ping()).unwrap())
        .unwrap();
    assert_eq!(preview_frame(&mut replacement).frame_type, FrameType::Pong);
    assert!(
        ps_states(&pids)
            .iter()
            .all(|(_, state)| state.starts_with('T'))
    );
    assert_eq!(tree_pids(&mut control, &id), pids);
    let after = control.request("session.list", json!({}))["sessions"][0].clone();
    for key in ["lastSeenAt", "hibernation"] {
        assert_eq!(before[key], after[key], "preview changed {key}");
    }
    assert!(!server.attach_hub().has_sinks(&id));
    drop(previews);
    drop(replacement);
    control.request("session.kill", json!({"sessionID": id}));
    control.request("session.remove", json!({"sessionID": id}));
}

#[test]
fn a_preview_receives_live_updates_without_a_desktop_attach() {
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let id = spawn_cat(&mut control);
    let before = control.request("session.list", json!({}))["sessions"][0]["lastSeenAt"].clone();
    let mut preview = preview_socket(&server, &id);
    let seed = preview_frame(&mut preview).grid_payload().unwrap().unwrap();
    assert!(seed.is_full_snapshot);
    assert_eq!(preview_frame(&mut preview).frame_type, FrameType::Modes);
    control.request(
        "session.send_text",
        json!({"sessionID":id,"text":"preview-live","submit":true}),
    );
    loop {
        let frame = preview_frame(&mut preview);
        if let Some(grid) = frame.grid_payload().unwrap() {
            let text: String = grid
                .changed_rows
                .iter()
                .flat_map(|row| &row.cells)
                .filter_map(|cell| char::from_u32(cell.scalar))
                .collect();
            if text.contains("preview-live") {
                break;
            }
        }
    }
    assert!(!server.attach_hub().has_sinks(&id));
    assert_eq!(
        control.request("session.list", json!({}))["sessions"][0]["lastSeenAt"],
        before
    );
    drop(preview);
    control.request("session.kill", json!({"sessionID":id}));
    control.request("session.remove", json!({"sessionID":id}));
}

struct PreviewSetReader {
    stream: BufReader<UnixStream>,
    decoder: diri_proto::preview_set::PreviewSetDecoder,
    queued: std::collections::VecDeque<diri_proto::preview_set::PreviewSetPacket>,
}
impl PreviewSetReader {
    fn next(&mut self) -> diri_proto::preview_set::PreviewSetPacket {
        loop {
            if let Some(packet) = self.queued.pop_front() {
                return packet;
            }
            let mut bytes = [0; 64 * 1024];
            let count = self.stream.read(&mut bytes).unwrap();
            assert!(count > 0, "preview set closed before expected packet");
            self.queued
                .extend(self.decoder.feed(&bytes[..count]).unwrap());
        }
    }
    fn set(&mut self, members: Vec<diri_proto::preview_set::PreviewMember>) {
        writeln!(
            self.stream.get_mut(),
            "{}",
            serde_json::to_string(&diri_proto::preview_set::PreviewSetMembership { members })
                .unwrap()
        )
        .unwrap();
    }
}
fn preview_set_socket(server: &ControlServer) -> PreviewSetReader {
    let mut stream = UnixStream::connect(server.socket_path()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(stream, "{}", json!({"preview_set":true,"version":1})).unwrap();
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    let ready: diri_proto::preview_set::PreviewSetReady = serde_json::from_str(&line).unwrap();
    assert_eq!(ready.version, 1);
    PreviewSetReader {
        stream,
        decoder: Default::default(),
        queued: Default::default(),
    }
}
fn preview_member(id: &str, generation: u64) -> diri_proto::preview_set::PreviewMember {
    diri_proto::preview_set::PreviewMember {
        session_id: diri_proto::SessionId::new(id),
        generation,
    }
}

#[test]
fn a_preview_set_keeps_sources_independent_and_preserves_stopped_processes() {
    use diri_proto::preview_set::{PreviewSetPacket, PreviewUnavailable};
    let temp = tempfile::tempdir().unwrap();
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let stopped = spawn_cat(&mut control);
    let pids = hibernate_and_verify_stopped(&mut control, &stopped);
    let before = control.request("session.list", json!({}))["sessions"][0].clone();
    let live = spawn_cat(&mut control);
    let mut preview = preview_set_socket(&server);
    preview.set(vec![
        preview_member(&stopped, 1),
        preview_member(&live, 2),
        preview_member("missing", 3),
    ]);
    let mut grids = std::collections::HashSet::new();
    let mut modes = 0;
    let mut missing = false;
    while grids.len() < 2 || modes < 2 || !missing {
        match preview.next() {
            PreviewSetPacket::Chunk { member, frame } => {
                if let Some(grid) = frame.grid_payload().unwrap() {
                    if grids.insert(member.session_id.0) {
                        assert!(grid.is_full_snapshot);
                    }
                    assert_eq!((grid.cols, grid.rows), (80, 24));
                } else {
                    assert_eq!(frame.frame_type, FrameType::Modes);
                    modes += 1;
                }
            }
            PreviewSetPacket::Unavailable { member, reason } => {
                assert_eq!(member, preview_member("missing", 3));
                assert_eq!(reason, PreviewUnavailable::Missing);
                missing = true;
            }
        }
    }
    assert!(grids.contains(&stopped) && grids.contains(&live));
    assert!(!server.attach_hub().has_sinks(&stopped));
    assert!(!server.attach_hub().has_sinks(&live));
    control.request(
        "session.send_text",
        json!({"sessionID":live,"text":"mux-live","submit":true}),
    );
    loop {
        if let PreviewSetPacket::Chunk { member, frame } = preview.next()
            && member.session_id.0 == live
            && let Some(grid) = frame.grid_payload().unwrap()
        {
            let text: String = grid
                .changed_rows
                .iter()
                .flat_map(|row| &row.cells)
                .filter_map(|cell| char::from_u32(cell.scalar))
                .collect();
            if text.contains("mux-live") {
                break;
            }
        }
    }
    preview.set(vec![preview_member(&stopped, 4), preview_member(&live, 2)]);
    loop {
        if let PreviewSetPacket::Chunk { member, frame } = preview.next()
            && member == preview_member(&stopped, 4)
        {
            assert!(
                frame.grid_payload().unwrap().unwrap().is_full_snapshot,
                "new membership generation starts with a full grid"
            );
            break;
        }
    }
    writeln!(
        preview.stream.get_mut(),
        "{}",
        json!({"members":[],"input":"must-not-run"})
    )
    .unwrap();
    let mut tail = Vec::new();
    preview.stream.read_to_end(&mut tail).unwrap();
    assert!(
        ps_states(&pids)
            .iter()
            .all(|(_, state)| state.starts_with('T'))
    );
    assert_eq!(tree_pids(&mut control, &stopped), pids);
    let records = control.request("session.list", json!({}));
    let after = records["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == stopped)
        .unwrap();
    for key in ["lastSeenAt", "hibernation"] {
        assert_eq!(before[key], after[key], "preview set changed {key}");
    }
    for id in [stopped, live] {
        control.request("session.kill", json!({"sessionID":id}));
        control.request("session.remove", json!({"sessionID":id}));
    }
}
