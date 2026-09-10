//! Mouse-driven TUI redraws must reach the actual attached client promptly.
//! Reading Session::screen directly misses delays in grid publication.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::session::HolderConfig;
use diri_engine::{ManifestEngine, Registry};
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::json;

struct SessionCleanup {
    registry: Arc<Mutex<Registry>>,
    id: String,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        let _ = self
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .terminate(&self.id, Duration::from_secs(2));
    }
}

#[test]
fn held_mouse_redraw_reaches_the_attached_grid_without_a_quiet_tick() {
    let temp = tempfile::tempdir().unwrap();
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
            .with_logs_dir(temp.path().join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.path().join("holders"),
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
    let mut control = UnixStream::connect(server.socket_path()).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut replies = BufReader::new(control.try_clone().unwrap());
    let mut request = |id, method: &str, params| {
        let message = ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        control.write_all(&bytes).unwrap();
        let mut line = String::new();
        replies.read_line(&mut line).unwrap();
        match serde_json::from_str::<ControlMessage>(&line).unwrap() {
            ControlMessage::Response {
                result: Ok(value), ..
            } => value,
            other => panic!("request failed: {other:?}"),
        }
    };
    // A tiny fullscreen TUI. One SGR wheel report causes one complete redraw,
    // followed by silence, just like an idle Claude transcript being scrolled.
    let spawned = request(
        1,
        "session.spawn",
        json!({
            "kind": { "shell": {} }, "cwd": "/tmp",
            "argv": ["/bin/bash", "--noprofile", "--norc", "-c",
                "stty -echo -icanon min 1 time 0; printf '\x1b[?1049h\x1b[?1000h\x1b[?1006hready'; n=0; while IFS= read -r -n 1 ch; do if [ \"$ch\" = M ]; then n=$((n+1)); printf '\x1b[Hscroll-%s\x1b[K\x1b[2;1Hfixed prompt' \"$n\"; fi; done"]
        }),
    );
    let id = spawned["id"].as_str().unwrap().to_owned();
    let _cleanup = SessionCleanup {
        registry,
        id: id.clone(),
    };
    let mut data = UnixStream::connect(server.socket_path()).unwrap();
    data.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(data, "{}", json!({ "attach": id })).unwrap();
    let mut codec = FrameCodec::new();
    let mut chunk = [0; 64 << 10];
    let mut input = data.try_clone().unwrap();
    let mut wait_for = |needle: &str| {
        loop {
            let count = data
                .read(&mut chunk)
                .unwrap_or_else(|err| panic!("waiting for {needle}: {err}"));
            assert!(count > 0, "attachment closed");
            for frame in codec.feed(&chunk[..count]).unwrap() {
                if frame.frame_type != FrameType::Grid {
                    continue;
                }
                let update = frame.grid_payload().unwrap().unwrap();
                if update.changed_rows.iter().any(|row| {
                    let text: String = row
                        .cells
                        .iter()
                        .map(|c| char::from_u32(c.scalar).unwrap_or(' '))
                        .collect();
                    text.contains(needle)
                }) {
                    return;
                }
            }
        }
    };
    wait_for("ready");
    let mut samples = Vec::new();
    for turn in 1..=12 {
        let started = Instant::now();
        input
            .write_all(&FrameCodec::encode(&Frame::scroll(0, 1, 0, 0)).unwrap())
            .unwrap();
        wait_for(&format!("scroll-{turn}"));
        if turn > 2 {
            samples.push(started.elapsed());
        }
    }
    request(2, "session.kill", json!({ "sessionID": id }));
    samples.sort();
    let median = samples[samples.len() / 2];
    eprintln!("held mouse-to-attached-grid median: {median:?}; samples: {samples:?}");
    assert!(
        median < Duration::from_millis(50),
        "scroll redraws wait for the 100 ms idle tick: {median:?}"
    );
}
