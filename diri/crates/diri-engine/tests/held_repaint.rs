//! A repaint reaches an attached client as one frame when a Holder owns the
//! PTY, as it does for every session the app runs.
//!
//! `repaint.rs` covers the direct pump. The held pump reads the Holder's
//! output stream and decides on its own when a frame is whole; it used to
//! publish on the first quiet poll, so a TUI that clears and redraws a frame
//! later (Cursor on every resize) showed a blank pane in between.

#![cfg(unix)]

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::json;

/// Widened for the same reason as in `repaint.rs`: a contended runner can
/// stall a shell for longer than the shipping 80 ms between two writes.
const BLANK_CEILING: Duration = Duration::from_millis(1200);

/// Beyond the held pump's quiet poll and the 16 ms partial-repaint settle,
/// so only the blank grace can keep the erase and the redraw together.
const REPAINT_GAP: Duration = Duration::from_millis(150);

fn screen_is_blank(rows: &[String]) -> bool {
    !rows.is_empty() && rows.iter().all(|row| row.trim().is_empty())
}

#[test]
fn a_held_erase_and_redraw_never_reach_a_client_as_a_blank_screen() {
    // Safe here: this binary holds one test, and nothing has been spawned.
    unsafe {
        std::env::set_var(
            "DIRIJOR_BLANK_REPAINT_CEILING_MS",
            BLANK_CEILING.as_millis().to_string(),
        )
    };
    let temp = tempfile::tempdir().expect("temp");
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
            .with_logs_dir(temp.path().join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.path().join("holders"),
                executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
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
    let mut request = serde_json::to_vec(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": [
                "/bin/sh",
                "-c",
                format!(
                    "stty -echo; printf 'frame-0\\r\\n'; \
                     while read -r turn; do \
                       printf '\\033[2J\\033[H'; sleep {gap}; \
                       printf 'frame-%s\\r\\n' \"$turn\"; \
                     done",
                    gap = REPAINT_GAP.as_secs_f32(),
                ),
            ],
        })),
    })
    .expect("encode");
    request.push(b'\n');
    (&control).write_all(&request).expect("spawn");
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

    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    writeln!(data, "{}", json!({ "attach": id })).expect("attach");
    let mut codec = FrameCodec::new();
    let mut rows: Vec<String> = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut wait_for = |expected: &str, data: &mut UnixStream, blank_allowed: bool| loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected}"
        );
        let count = data.read(&mut chunk).expect("read frames");
        assert!(count > 0, "data channel closed waiting for {expected}");
        for frame in codec.feed(&chunk[..count]).expect("valid frames") {
            if frame.frame_type != FrameType::Grid {
                continue;
            }
            let update = frame.grid_payload().expect("decode").expect("grid");
            rows.resize(usize::from(update.rows), String::new());
            for row in &update.changed_rows {
                if let Some(slot) = rows.get_mut(usize::from(row.y)) {
                    *slot = row
                        .cells
                        .iter()
                        .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                        .collect();
                }
            }
            assert!(
                blank_allowed || !screen_is_blank(&rows),
                "a held repaint published its erased half while waiting for {expected}"
            );
        }
        if rows.iter().any(|row| row.trim() == expected) {
            return;
        }
    };
    wait_for("frame-0", &mut data, true);
    for turn in 1..=12 {
        data.write_all(
            &FrameCodec::encode(&Frame::input(format!("{turn}\n").into_bytes())).expect("encode"),
        )
        .expect("send turn");
        wait_for(&format!("frame-{turn}"), &mut data, false);
    }

    let mut request = serde_json::to_vec(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    })
    .expect("encode");
    request.push(b'\n');
    (&control).write_all(&request).expect("kill");
}
