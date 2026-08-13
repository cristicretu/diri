//! A repaint reaches an attached client as one frame.
//!
//! TUIs do not repaint in a single write: they erase the old frame and draw
//! the new one separately, and a PTY hands each write over the moment it
//! lands. A reader that publishes the grid per read publishes the half-erased
//! screen in between, which is what a pane paints as a flicker.

#![cfg(unix)]

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::grid::GridUpdate;
use serde_json::json;

/// Mirrors the grid the way the app does: a seed plus changed-row diffs.
#[derive(Default)]
struct Mirror {
    rows: Vec<String>,
}

impl Mirror {
    fn apply(&mut self, update: &GridUpdate) {
        self.rows.resize(usize::from(update.rows), String::new());
        for row in &update.changed_rows {
            let text: String = row
                .cells
                .iter()
                .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                .collect();
            if let Some(slot) = self.rows.get_mut(usize::from(row.y)) {
                *slot = text;
            }
        }
    }

    fn is_blank(&self) -> bool {
        !self.rows.is_empty() && self.rows.iter().all(|row| row.trim().is_empty())
    }

    fn text(&self) -> String {
        self.rows.join("\n")
    }
}

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

    fn next_frame(&mut self, what: &str, deadline: Instant) -> Frame {
        let mut chunk = [0u8; 64 << 10];
        loop {
            if let Some(frame) = self.queue.pop_front() {
                return frame;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let count = self.stream.read(&mut chunk).expect("read frames");
            assert!(count > 0, "data channel closed while waiting for {what}");
            self.queue
                .extend(self.codec.feed(&chunk[..count]).expect("valid frames"));
        }
    }
}

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

#[test]
fn an_erase_and_redraw_never_reach_a_client_as_a_blank_screen() {
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

    // A minimal TUI: every line of input repaints the screen the way a real
    // one does — erase, then draw — as two separate writes. Thirty
    // milliseconds is deliberately beyond the ordinary 16 ms partial-repaint
    // settle: only the bounded blank-frame grace can keep this atomic.
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
                "stty -echo; printf 'frame-0\\r\\n'; \
                 while read -r turn; do \
                   case \"$turn\" in \
                     blank) printf '\\033[2J\\033[H' ;; \
                     additive) printf 'additive-frame\\r\\n' ;; \
                     *) printf '\\033[2J\\033[H'; sleep 0.030; \
                        printf 'frame-%s\\r\\n' \"$turn\" ;; \
                   esac; \
                 done",
            ],
        })),
    });
    let mut reader = std::io::BufReader::new(control.try_clone().expect("clone"));
    let id = {
        let mut line = String::new();
        reader.read_line(&mut line).expect("spawn reply");
        match serde_json::from_str::<ControlMessage>(&line).expect("decode") {
            ControlMessage::Response {
                result: Ok(result), ..
            } => result["id"].as_str().expect("id").to_string(),
            other => panic!("spawn failed: {other:?}"),
        }
    };

    std::thread::sleep(Duration::from_millis(400));

    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach_line = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach_line.push(b'\n');
    data.write_all(&attach_line).expect("attach");

    let mut frames = FrameReader::new(data.try_clone().expect("clone data"));
    let mut mirror = Mirror::default();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let frame = frames.next_frame("the seed grid", deadline);
        if frame.frame_type != FrameType::Grid {
            continue;
        }
        let update = frame.grid_payload().expect("decode").expect("grid");
        mirror.apply(&update);
        if mirror.text().contains("frame-0") {
            break;
        }
    }

    // One turn is a coin flip on scheduling; a run of them is not. Every
    // repaint must land whole.
    for turn in 1..=20 {
        let expected = format!("frame-{turn}");
        data.write_all(
            &FrameCodec::encode(&Frame::input(format!("{turn}\n").into_bytes())).expect("encode"),
        )
        .expect("send turn");
        loop {
            let frame = frames.next_frame(&expected, deadline);
            if frame.frame_type != FrameType::Grid {
                continue;
            }
            let update = frame.grid_payload().expect("decode").expect("grid");
            mirror.apply(&update);
            assert!(
                !mirror.is_blank(),
                "turn {turn} published the erased half of a repaint as its own frame"
            );
            if mirror.text().contains(&expected) {
                break;
            }
        }
    }

    // An intentional erase must not be hidden forever. With no redraw, the
    // blank frame is published once the bounded grace expires.
    let blank_started = Instant::now();
    data.write_all(&FrameCodec::encode(&Frame::input(b"blank\n".to_vec())).expect("encode"))
        .expect("send blank");
    loop {
        let frame = frames.next_frame("the intentional blank", deadline);
        if frame.frame_type != FrameType::Grid {
            continue;
        }
        let update = frame.grid_payload().expect("decode").expect("grid");
        mirror.apply(&update);
        if mirror.is_blank() {
            break;
        }
    }
    assert!(
        blank_started.elapsed() < Duration::from_millis(500),
        "an intentional clear must publish by a bounded deadline"
    );

    // Additive output never uses the blank repaint grace. It should remain an
    // immediate streaming path even after an intentional clear.
    let additive_started = Instant::now();
    data.write_all(&FrameCodec::encode(&Frame::input(b"additive\n".to_vec())).expect("encode"))
        .expect("send additive output");
    loop {
        let frame = frames.next_frame("the additive frame", deadline);
        if frame.frame_type != FrameType::Grid {
            continue;
        }
        let update = frame.grid_payload().expect("decode").expect("grid");
        mirror.apply(&update);
        if mirror.text().contains("additive-frame") {
            break;
        }
    }
    assert!(
        additive_started.elapsed() < Duration::from_millis(100),
        "additive output regressed onto the repaint grace path"
    );

    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
}
