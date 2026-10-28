//! The binary data channel over a real socket: attach, get seeded, type,
//! see grid diffs — the app's terminal path against the Rust engine.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::grid::GridUpdate;
use diri_proto::terminal::MouseModes;
use serde_json::json;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// Decoded-frame reader that never drops frames arriving in one batch.
struct FrameReader {
    stream: UnixStream,
    codec: FrameCodec,
    queue: std::collections::VecDeque<Frame>,
}

impl FrameReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            codec: FrameCodec::new(),
            queue: std::collections::VecDeque::new(),
        }
    }

    /// Pops frames (reading more as needed) until `predicate` matches.
    fn until(&mut self, what: &str, mut predicate: impl FnMut(&Frame) -> bool) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut chunk = [0u8; 64 << 10];
        loop {
            if let Some(frame) = self.queue.pop_front() {
                if predicate(&frame) {
                    return frame;
                }
                continue;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let count = self.stream.read(&mut chunk).expect("read frames");
            assert!(count > 0, "data channel closed while waiting for {what}");
            self.queue
                .extend(self.codec.feed(&chunk[..count]).expect("valid frames"));
        }
    }
}

fn grid_text(update: &GridUpdate) -> String {
    update
        .changed_rows
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_attach_is_seeded_then_streams_diffs_and_answers_input() {
    let temp = tempfile::tempdir().expect("temp");
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
            .with_logs_dir(temp.path().join("logs")),
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

    // Control connection: spawn a cat session that echoes what we type.
    let control = UnixStream::connect(server.socket_path()).expect("connect control");
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": [
                "/bin/sh",
                "-c",
                "stty -echo; printf '\\033[?1h\\033=\\033[?2004hseeded-screen\\n'; IFS= read -r _; printf '\\033[?1l\\033>'; IFS= read -r _; printf '\\033[?2004l'; stty echo; exec cat"
            ],
        })),
    });
    let mut reader = std::io::BufReader::new(control.try_clone().expect("clone"));
    let id = {
        use std::io::BufRead;
        let mut line = String::new();
        reader.read_line(&mut line).expect("spawn reply");
        let reply: ControlMessage = serde_json::from_str(&line).expect("decode");
        match reply {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };

    // Give the child a beat to print its banner so the seed contains it.
    std::thread::sleep(Duration::from_millis(400));

    // Data channel: one JSON line, then binary frames.
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach_line = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach_line.push(b'\n');
    data.write_all(&attach_line).expect("attach");

    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));
    let seed = frames.until("the seed grid", |frame| frame.frame_type == FrameType::Grid);
    let update = seed.grid_payload().expect("decode").expect("grid");
    assert!(
        update.is_full_snapshot,
        "a fresh sink gets the whole screen"
    );
    assert!(
        grid_text(&update).contains("seeded-screen"),
        "the seed carries what the child already painted"
    );

    let modes = frames.until("initial modes", |frame| {
        frame.frame_type == FrameType::Modes
    });
    assert_eq!(
        modes.terminal_modes_payload(),
        Some((false, true, MouseModes::OFF)),
        "the attachment seed carries the child's current paste mode"
    );

    assert_eq!(
        modes.keyboard_state_payload().unwrap(),
        Some(diri_proto::terminal_input::KeyboardState {
            enhancements: None,
            application_cursor_keys: true,
            application_keypad: true
        })
    );
    data.write_all(&FrameCodec::encode(&Frame::input(b"finish-keys\n".to_vec())).unwrap())
        .unwrap();
    let keyboard_only = frames.until("keyboard-only mode change", |frame| {
        frame.frame_type == FrameType::Modes
            && frame.keyboard_state_payload().ok().flatten()
                == Some(diri_proto::terminal_input::KeyboardState::default())
    });
    assert_eq!(
        keyboard_only.terminal_modes_payload(),
        Some((false, true, MouseModes::OFF)),
        "keyboard-only change publishes while cell/cursor/paste/mouse state stays unchanged"
    );

    // The setup shell drops bracketed paste after its next input. A mode-only
    // terminal change must wake the attachment pump even when no visible cell
    // changes with it.
    data.write_all(&FrameCodec::encode(&Frame::input(b"finish-setup\n".to_vec())).expect("encode"))
        .expect("finish child setup");
    let modes = frames.until("updated modes", |frame| {
        frame.frame_type == FrameType::Modes
    });
    assert_eq!(
        modes.terminal_modes_payload(),
        Some((false, false, MouseModes::OFF)),
        "live mode changes propagate independently of grid damage"
    );

    // Exercise WouldBlock with both a fragmented frame header and body. The
    // decoder must preserve each prefix and deliver the input exactly once.
    let fragmented = FrameCodec::encode(&Frame::input(b"fragmented-input\n".to_vec())).unwrap();
    for part in [&fragmented[..2], &fragmented[2..7], &fragmented[7..]] {
        data.write_all(part).unwrap();
        std::thread::sleep(Duration::from_millis(3));
    }
    frames.until("fragmented input", |frame| {
        frame
            .grid_payload()
            .ok()
            .flatten()
            .is_some_and(|grid| grid_text(&grid).contains("fragmented-input"))
    });
    data.write_all(&FrameCodec::encode(&Frame::ping()).unwrap())
        .unwrap();
    frames.until("queued pong", |frame| frame.frame_type == FrameType::Pong);

    // Let the per-session pump establish its shared diff baseline. Its first
    // sample is allowed to be a FullSnapshot: if input beats that first tick,
    // the snapshot legitimately includes the new text. A second turn is the
    // deterministic seam for asserting steady-state diff behavior.
    data.write_all(&FrameCodec::encode(&Frame::input(b"warm-up-pump\n".to_vec())).expect("encode"))
        .expect("send warm-up input");
    frames.until("the warm-up echo", |frame| {
        frame.frame_type == FrameType::Grid
            && frame
                .grid_payload()
                .ok()
                .flatten()
                .is_some_and(|update| grid_text(&update).contains("warm-up-pump"))
    });

    // Mouse reports use their own frame kind so the Engine takes the raw
    // interactive path instead of treating escape bytes as prompt text. The
    // payload remains ordered with keyboard input and reaches the same PTY.
    data.write_all(
        &FrameCodec::encode(&Frame::mouse(b"mouse-over-attach\n".to_vec())).expect("encode"),
    )
    .expect("send mouse payload");
    frames.until("the raw mouse payload echo", |frame| {
        frame.frame_type == FrameType::Grid
            && frame
                .grid_payload()
                .ok()
                .flatten()
                .is_some_and(|update| grid_text(&update).contains("mouse-over-attach"))
    });

    // Typing through the established data channel: cat echoes, and each echo
    // comes back as a grid DIFF (not a full snapshot). Use the median so a
    // single scheduler hiccup cannot fail the test, while a fixed 16 ms frame
    // boundary on every keystroke still does.
    let mut interactive_latencies = Vec::new();
    for index in 0..101 {
        let marker = format!("typed-over-attach-{index}");
        let sent_at = Instant::now();
        data.write_all(
            &FrameCodec::encode(&Frame::input(format!("{marker}\n").into_bytes())).expect("encode"),
        )
        .expect("send input");
        let diff = frames.until("the echo diff", |frame| {
            frame.frame_type == FrameType::Grid
                && frame
                    .grid_payload()
                    .ok()
                    .flatten()
                    .is_some_and(|update| grid_text(&update).contains(&marker))
        });
        interactive_latencies.push(sent_at.elapsed());
        let update = diff.grid_payload().expect("decode").expect("grid");
        assert!(
            !update.is_full_snapshot,
            "steady-state frames are diffs, not full repaints"
        );
    }
    interactive_latencies.sort_unstable();
    let median = interactive_latencies[interactive_latencies.len() / 2];
    eprintln!("local input-to-grid median: {}us", median.as_micros());
    assert!(
        median <= Duration::from_millis(8),
        "local input-to-grid median was {median:?}; expected no fixed 16 ms frame boundary"
    );

    // Ping answers pong on the same channel.
    data.write_all(&FrameCodec::encode(&Frame::ping()).expect("encode"))
        .expect("send ping");
    frames.until("pong", |frame| frame.frame_type == FrameType::Pong);

    // A resize through the data channel reshapes the PTY; the next grid
    // carries the new geometry.
    data.write_all(&FrameCodec::encode(&Frame::resize(100, 30)).expect("encode"))
        .expect("send resize");
    frames.until("resized grid", |frame| {
        frame.frame_type == FrameType::Grid
            && frame
                .grid_payload()
                .ok()
                .flatten()
                .is_some_and(|update| update.cols == 100 && update.rows == 30)
    });

    // Clean up the child.
    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
}

/// A keystroke into a Holder-backed session, the only kind the daemon runs,
/// must not wait out the output batch. The Holder cannot say a lone echo is
/// the whole burst, so before the Engine learned to publish output answering
/// recent input, every echo sat out the 8 ms batch ceiling (median ~9 ms here).
#[test]
fn a_held_session_publishes_an_echo_without_waiting_out_the_batch() {
    // Short root: Holder sockets live under it and must fit SUN_LEN.
    let root = std::path::PathBuf::from(format!("/tmp/diri-echo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let registry = Arc::new(Mutex::new(Registry::new(engine(), root.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(diri_engine::session::HolderConfig {
                holders_dir: root.join("holders"),
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
    let control = UnixStream::connect(server.socket_path()).expect("connect control");
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", "printf 'ready\\n'; exec cat"],
        })),
    });
    let id = {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(control.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("spawn reply");
        match serde_json::from_str(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };
    std::thread::sleep(Duration::from_millis(400));
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");
    // Let the attach pump settle its baseline, then start from a quiet
    // channel so every grid read below answers the key just sent.
    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));
    frames.until("the seed grid", |frame| frame.frame_type == FrameType::Grid);
    for _ in 0..3 {
        data.write_all(&FrameCodec::encode(&Frame::input(b"w".to_vec())).unwrap())
            .expect("warm-up key");
        std::thread::sleep(Duration::from_millis(100));
    }
    data.set_read_timeout(Some(Duration::from_millis(200)))
        .expect("drain timeout");
    let mut codec = FrameCodec::new();
    let mut column = None;
    let mut chunk = [0u8; 64 << 10];
    // A quiet period ends the drain only once an echo has arrived: a loaded
    // runner can hold the first echo back for longer than one quiet period.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match data.read(&mut chunk) {
            Ok(count) => {
                assert!(count > 0, "data channel closed");
                for frame in codec.feed(&chunk[..count]).expect("valid frames") {
                    if let Ok(Some(update)) = frame.grid_payload() {
                        column = Some(update.cursor_col);
                    }
                }
            }
            Err(_) if column.is_some() => break,
            Err(_) => assert!(Instant::now() < deadline, "warm-up echoes"),
        }
    }
    data.set_read_timeout(None).expect("clear timeout");
    let start = column.expect("warm-up echoes");
    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));

    // Paced like typing, so each key is a lone echo rather than a stream.
    let mut latencies = Vec::new();
    for column in start..start + 31 {
        let sent = Instant::now();
        data.write_all(&FrameCodec::encode(&Frame::input(vec![b'a' + column as u8 % 26])).unwrap())
            .expect("send key");
        frames.until("the echo", |frame| {
            frame.frame_type == FrameType::Grid
                && frame
                    .grid_payload()
                    .ok()
                    .flatten()
                    .is_some_and(|update| update.cursor_col == column + 1)
        });
        latencies.push(sent.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    latencies.sort_unstable();
    let median = latencies[latencies.len() / 2];
    eprintln!("held input-to-grid median: {}us", median.as_micros());
    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
    std::thread::sleep(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        median <= Duration::from_millis(5),
        "held input-to-grid median was {median:?}; an echo must not wait out the 8 ms batch"
    );
}

#[test]
fn a_slow_reader_does_not_delay_an_active_reader() {
    use std::io::BufRead;
    use std::os::fd::AsRawFd;
    let temp = tempfile::tempdir().unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(ControlServer::new(
        Arc::clone(&registry),
        temp.path().join("daemon.sock"),
    ));
    let listener = server.bind().unwrap();
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let small_buffer: libc::c_int = 1024;
            // SAFETY: a live socket and correctly sized initialized option.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&small_buffer as *const libc::c_int).cast(),
                        std::mem::size_of_val(&small_buffer) as libc::socklen_t,
                    )
                },
                0
            );
            let server = Arc::clone(&serving);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let mut control = UnixStream::connect(server.socket_path()).unwrap();
    serde_json::to_writer(&mut control, &ControlMessage::Request {
        id: 1, method: "session.spawn".into(), params: Some(json!({
            "kind": {"generic": {}}, "cwd": temp.path(), "initialCols":80,"initialRows":24,
            "argv":["/bin/sh", "-c", "stty -echo; printf '\\033[?2004h'; while IFS= read -r line; do printf '\\033[H'; i=0; while [ \"$i\" -lt 24 ]; do printf '%s--ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz\\n' \"$line\"; i=$((i+1)); done; done"]
        }))
    }).unwrap();
    control.write_all(b"\n").unwrap();
    let mut line = String::new();
    std::io::BufReader::new(control.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let reply: ControlMessage = serde_json::from_str(&line).unwrap();
    let id = match reply {
        ControlMessage::Response {
            result: Ok(value), ..
        } => value["id"].as_str().unwrap().to_owned(),
        other => panic!("{other:?}"),
    };
    let ready_deadline = Instant::now() + Duration::from_secs(2);
    while !registry.lock().unwrap().get(&id).unwrap().bracketed_paste() {
        assert!(
            Instant::now() < ready_deadline,
            "fixture did not disable PTY echo"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let attach = || {
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(750)))
            .unwrap();
        serde_json::to_writer(&mut stream, &json!({"attach": id})).unwrap();
        stream.write_all(b"\n").unwrap();
        stream
    };
    let slow = attach();
    // Read only the initial seed, then leave this first sink completely stalled.
    let mut initial = FrameReader::new(slow.try_clone().unwrap());
    initial.until("slow seed", |frame| frame.frame_type == FrameType::Modes);
    let active = attach();
    let mut active = FrameReader::new(active);
    active.until("active seed", |frame| frame.frame_type == FrameType::Modes);
    let original_pid = registry.lock().unwrap().get(&id).unwrap().child_pid();
    let mut samples = Vec::new();
    let mut failure = None;
    for step in 0..40 {
        let marker = format!("sample{step:03}");
        let start = Instant::now();
        registry
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .write_input(format!("{marker}\n").as_bytes())
            .unwrap();
        let mut bytes = [0; 65536];
        let mut found = false;
        while start.elapsed() < Duration::from_millis(750) {
            while let Some(frame) = active.queue.pop_front() {
                if frame
                    .grid_payload()
                    .ok()
                    .flatten()
                    .is_some_and(|grid| grid_text(&grid).contains(&marker))
                {
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
            match active.stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => active
                    .queue
                    .extend(active.codec.feed(&bytes[..count]).unwrap()),
                Err(_) => break,
            }
        }
        if !found {
            failure = Some(marker);
            break;
        }
        samples.push(start.elapsed());
    }
    // Release the baseline's blocked write before reporting an assertion.
    let _ = slow.shutdown(std::net::Shutdown::Both);
    if failure.is_none() {
        let mut reconnected = FrameReader::new(attach());
        let seed = reconnected.until("reconnected full snapshot", |frame| {
            frame.frame_type == FrameType::Grid
        });
        let grid = seed.grid_payload().unwrap().unwrap();
        assert!(grid.is_full_snapshot);
        assert!(grid_text(&grid).contains("sample039"));
        assert_eq!(
            registry.lock().unwrap().get(&id).unwrap().child_pid(),
            original_pid
        );
        let _ = reconnected.stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = active.stream.shutdown(std::net::Shutdown::Both);
    registry
        .lock()
        .unwrap()
        .remove(&id, &temp.path().join("logs"))
        .unwrap();
    assert!(
        failure.is_none(),
        "slow reader blocked active output at {failure:?}"
    );
    eprintln!(
        "active reader samples_us: {:?}",
        samples.iter().map(Duration::as_micros).collect::<Vec<_>>()
    );
    samples.sort();
    eprintln!(
        "active reader with stalled peer: {} samples, p50={:?}, p90={:?}, max={:?}",
        samples.len(),
        samples[samples.len() / 2],
        samples[samples.len() * 9 / 10],
        samples.last().unwrap()
    );
    assert!(samples[samples.len() * 9 / 10] < Duration::from_millis(150));
}

#[test]
fn a_password_prompt_reaches_the_client_as_a_mode_and_never_as_cells() {
    let temp = tempfile::tempdir().expect("temp");
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
            .with_logs_dir(temp.path().join("logs")),
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

    let control = UnixStream::connect(server.socket_path()).expect("connect control");
    let mut request = serde_json::to_vec(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            // The first read echoes, so the seed is taken outside the prompt.
            "argv": [
                "/bin/sh",
                "-c",
                "printf ready; read a; stty -echo; printf 'Password:'; read secret; stty echo; printf accepted; exec cat"
            ],
        })),
    })
    .expect("encode");
    request.push(b'\n');
    (&control).write_all(&request).expect("spawn");
    let id = {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(control.try_clone().expect("clone"))
            .read_line(&mut line)
            .expect("spawn reply");
        match serde_json::from_str(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };

    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach_line = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach_line.push(b'\n');
    data.write_all(&attach_line).expect("attach");
    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));
    let seed = frames.until("initial modes", |frame| {
        frame.frame_type == FrameType::Modes
    });
    assert_eq!(seed.secret_input_payload(), Some(false));

    let mut painted = String::new();
    let watch = |frame: &Frame, painted: &mut String| {
        if let Ok(Some(grid)) = frame.grid_payload() {
            painted.push_str(&grid_text(&grid));
        }
    };
    data.write_all(&FrameCodec::encode(&Frame::input(b"\n".to_vec())).unwrap())
        .expect("reach the prompt");
    let prompting = frames.until("secret input on", |frame| {
        watch(frame, &mut painted);
        frame.secret_input_payload() == Some(true)
    });
    assert_eq!(
        prompting.terminal_modes_payload(),
        Some((false, false, MouseModes::OFF)),
        "the bit travels beside the other modes, not instead of them"
    );

    data.write_all(&FrameCodec::encode(&Frame::input(b"hunter2\n".to_vec())).unwrap())
        .expect("answer the prompt");
    // The banner and the mode may arrive in either order, and nothing follows
    // them, so both are awaited by one predicate.
    let mut echoing = false;
    frames.until("echo restored after the banner", |frame| {
        watch(frame, &mut painted);
        if let Some(secret) = frame.secret_input_payload() {
            echoing = !secret;
        }
        echoing && painted.contains("accepted")
    });
    assert!(painted.contains("Password:"));
    assert!(
        !painted.contains("hunter2"),
        "echo was off, so the secret never became cells: {painted:?}"
    );
}

/// A fake agent TUI for keystroke timing: answers every key with a redraw of
/// its composer the way Ink (Claude Code) and ratatui (Codex) do.
///
/// - `sync`: one synchronized update (DECSET 2026) that redraws the composer
///   and shortens the hint row below it by two cells, so every answer ends
///   with fewer cells than the screen had (the way the placeholder and
///   "? for shortcuts" vanish when typing starts, or a menu closes).
/// - `chunks`: an unsynchronized redraw in three writes `gap_us` apart: erase
///   the composer, write it back, then update a status row.
const FAKE_TUI: &str = r#"
use Time::HiRes qw(usleep);
my ($mode, $gap) = @ARGV;
system("stty raw -echo");
$| = 1;
# The wheel mode is a fullscreen transcript: alternate screen, SGR mouse
# reporting, one plain redraw per wheel report.
syswrite STDOUT, "\e[?1049h\e[?1000h\e[?1006h" if $mode eq "wheel";
syswrite STDOUT, "\e[2J\e[H> \e[2mTry \"refactor the parser\"\e[0m\r\n" . ("x" x 80);
my $typed = "";
my $wheels = 0;
while (sysread(STDIN, my $key, 1)) {
    if ($mode eq "wheel") {
        next unless $key eq "M";
        $wheels++;
        syswrite STDOUT, "\e[H\e[2Kscrolled $wheels\r\n\e[2K" . ("y" x 40);
        next;
    }
    $typed .= $key;
    my $n = length $typed;
    if ($mode eq "sync") {
        syswrite STDOUT, "\e[?2026h\e[H\e[2K> $typed\r\n\e[2K" . ("x" x (80 - 2 * $n)) . "\e[1;" . ($n + 3) . "H\e[?2026l";
    } else {
        syswrite STDOUT, "\e[H\e[2K";
        usleep $gap;
        syswrite STDOUT, "> $typed";
        usleep $gap;
        syswrite STDOUT, "\r\n\e[2Kstatus $n\e[1;" . ($n + 3) . "H";
    }
}
"#;

/// Key → the frame that completes the fake TUI's redraw, per key, for a
/// Holder-backed session: what `input.echo` sees minus the agent's own
/// thinking time.
fn fake_tui_redraw_latencies(mode: &str, gap_us: u64, keys: usize) -> Vec<Duration> {
    // Short root: Holder sockets live under it and must fit SUN_LEN.
    let root = std::path::PathBuf::from(format!("/tmp/diri-tui-{}-{mode}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let registry = Arc::new(Mutex::new(Registry::new(engine(), root.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(diri_engine::session::HolderConfig {
                holders_dir: root.join("holders"),
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
    let control = UnixStream::connect(server.socket_path()).expect("connect control");
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": ["/usr/bin/perl", "-e", FAKE_TUI, mode, gap_us.to_string()],
        })),
    });
    let id = {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(control.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("spawn reply");
        match serde_json::from_str(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };
    std::thread::sleep(Duration::from_millis(500));
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");
    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));
    frames.until("the seed grid", |frame| frame.frame_type == FrameType::Grid);
    std::thread::sleep(Duration::from_millis(200));
    let mut latencies = Vec::new();
    let mut typed = String::new();
    for index in 0..keys {
        let key = b'a' + (index % 26) as u8;
        typed.push(char::from(key));
        let done = match mode {
            "sync" => format!("> {typed}"),
            "wheel" => format!("scrolled {}", index + 1),
            _ => format!("status {}", typed.len()),
        };
        let frame = if mode == "wheel" {
            Frame::scroll(0, 1, 10, 10)
        } else {
            Frame::input(vec![key])
        };
        let sent = Instant::now();
        data.write_all(&FrameCodec::encode(&frame).unwrap())
            .expect("send key");
        frames.until("the finished redraw", |frame| {
            frame.frame_type == FrameType::Grid
                && frame.grid_payload().ok().flatten().is_some_and(|update| {
                    grid_text(&update)
                        .lines()
                        .any(|line| line.trim_end() == done)
                })
        });
        latencies.push(sent.elapsed());
        // Paced like typing, so each key is its own burst.
        std::thread::sleep(Duration::from_millis(40));
    }
    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
    std::thread::sleep(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&root);
    latencies
}

fn percentile(samples: &mut [Duration], q: f64) -> Duration {
    samples.sort_unstable();
    samples[((samples.len() - 1) as f64 * q).round() as usize]
}

/// A synchronized redraw is complete by the child's own declaration, so a
/// keystroke answered by one is published at once even when the answer
/// removed cells (Claude Code's placeholder and shortcut hint disappearing as
/// typing starts). Before, such an echo looked like a half-erased repaint and
/// waited out the 8 ms output batch.
#[test]
fn a_synchronized_redraw_that_removes_cells_answers_a_keystroke_at_once() {
    let mut latencies = fake_tui_redraw_latencies("sync", 0, 21);
    let median = percentile(&mut latencies, 0.5);
    let p90 = percentile(&mut latencies, 0.9);
    eprintln!(
        "sync redraw: median {}us p90 {}us",
        median.as_micros(),
        p90.as_micros()
    );
    assert!(
        median <= Duration::from_millis(5),
        "a synchronized echo that removed cells took {median:?} (median); it must not wait out the 8 ms batch"
    );
}

/// A wheel report is answered by the program redrawing its transcript, and
/// that redraw is the scroll the user is waiting to see. It used to reach the
/// PTY as background output, so the answer sat out the 8 ms batch (median
/// ~10 ms) while a click's was published at once.
#[test]
fn a_wheel_reports_redraw_is_published_at_once() {
    let mut latencies = fake_tui_redraw_latencies("wheel", 0, 21);
    let median = percentile(&mut latencies, 0.5);
    eprintln!("wheel redraw: median {}us", median.as_micros());
    assert!(
        median <= Duration::from_millis(5),
        "a wheel report's redraw took {median:?} (median); it must not wait out the 8 ms batch"
    );
}

/// An unsynchronized redraw split into three writes: the last write should
/// reach the client about when it lands, not a batch later.
#[test]
#[ignore = "timing report; run explicitly"]
fn multi_chunk_redraw_timing() {
    for gap_us in [0u64, 500, 2_000] {
        let mut latencies = fake_tui_redraw_latencies("chunks", gap_us, 31);
        let median = percentile(&mut latencies, 0.5);
        let p90 = percentile(&mut latencies, 0.9);
        eprintln!(
            "chunks gap={gap_us}us: last write at ~{}us; finished redraw median {}us p90 {}us",
            gap_us * 2,
            median.as_micros(),
            p90.as_micros()
        );
    }
    let mut latencies = fake_tui_redraw_latencies("sync", 0, 31);
    let median = percentile(&mut latencies, 0.5);
    let p90 = percentile(&mut latencies, 0.9);
    eprintln!(
        "sync: finished redraw median {}us p90 {}us",
        median.as_micros(),
        p90.as_micros()
    );
}

/// Output nobody typed for, written to a held session's TTY from outside the
/// way a background program prints, reaches an attached client as soon as it
/// goes quiet. Before, a lone write waited out the whole 8 ms output batch for
/// the empty poll that proved its burst had ended (9 ms at the median).
#[test]
fn unsolicited_output_reaches_the_client_without_waiting_out_the_batch() {
    // Short root: Holder sockets live under it and must fit SUN_LEN.
    let root = std::path::PathBuf::from(format!("/tmp/diri-lead-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let tty_file = root.join("tty");
    let registry = Arc::new(Mutex::new(Registry::new(engine(), root.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(diri_engine::session::HolderConfig {
                holders_dir: root.join("holders"),
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
    let control = UnixStream::connect(server.socket_path()).expect("connect control");
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": [
                "/bin/sh",
                "-c",
                format!("tty > '{}'; stty -echo; printf 'ready\\r\\n'; exec cat", tty_file.display()),
            ],
        })),
    });
    let id = {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(control.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("spawn reply");
        match serde_json::from_str(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");
    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));
    frames.until("the seed grid", |frame| frame.frame_type == FrameType::Grid);
    let deadline = Instant::now() + Duration::from_secs(10);
    let tty = loop {
        if let Ok(path) = std::fs::read_to_string(&tty_file)
            && path.trim().starts_with("/dev/")
        {
            break path.trim().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "the child never reported its tty"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    frames.until("the ready line", |frame| {
        frame.frame_type == FrameType::Grid
            && frame
                .grid_payload()
                .ok()
                .flatten()
                .is_some_and(|update| grid_text(&update).contains("ready"))
    });
    std::thread::sleep(Duration::from_millis(200));
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open(&tty)
        .expect("open tty");
    let mut latencies = Vec::new();
    let mut printed = String::new();
    // The first writes warm up the Holder's output subscription, which the
    // pump takes only once it has drained the log; they are not measured.
    for index in 0..26 {
        let byte = b'a' + (index % 26) as u8;
        printed.push(char::from(byte));
        let sent = Instant::now();
        terminal.write_all(&[byte]).expect("tty write");
        frames.until("the printed byte", |frame| {
            frame.frame_type == FrameType::Grid
                && frame.grid_payload().ok().flatten().is_some_and(|update| {
                    grid_text(&update)
                        .lines()
                        .any(|line| line.trim_end() == printed)
                })
        });
        if index >= 5 {
            latencies.push(sent.elapsed());
        }
        // Paced, so every write starts after quiet.
        std::thread::sleep(Duration::from_millis(40));
    }
    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
    std::thread::sleep(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&root);
    let median = percentile(&mut latencies, 0.5);
    eprintln!("unsolicited output: median {}us", median.as_micros());
    assert!(
        median <= Duration::from_millis(5),
        "a lone write took {median:?} (median) to reach the client; it must not wait out the 8 ms batch"
    );
}
