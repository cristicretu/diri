//! The Context Ring end to end over a real control socket: a capture is
//! saved, sent with a prompt into a live PTY, remembered for its page, and
//! the remembered Session ranks first the next time that page is captured.
//! Everything lives in a private temp directory.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use serde_json::{Value, json};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

/// A 4x2 PNG header; the ring reads dimensions from it.
fn write_png(path: &Path) {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    std::fs::write(path, bytes).unwrap();
}

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("reply");
            match serde_json::from_str::<ControlMessage>(&line).expect("decode") {
                ControlMessage::Response { id, result } if id == self.next => {
                    return result.map_err(|e| serde_json::to_value(e).unwrap());
                }
                _ => continue,
            }
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }
}

#[test]
fn a_capture_is_sent_remembered_and_ranked_first_next_time() {
    let temp = tempfile::tempdir().expect("temp");
    let registry = Arc::new(Mutex::new(Registry::new(
        engine(),
        temp.path().join("state.json"),
    )));
    let server = ControlServer::new(Arc::clone(&registry), temp.path().join("daemon.sock"))
        .with_logs_dir(temp.path().join("logs"))
        .with_state_dir(temp.path().join("state"));
    let listener = server.bind().expect("bind");
    let server = Arc::new(server);
    let accepting = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let _ = server.serve(stream);
        })
    };
    let stream = UnixStream::connect(server.socket_path()).expect("connect");
    let mut client = Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream.try_clone().unwrap()),
        next: 0,
    };

    // A stand-in Agent that echoes what it is sent, and an idle neighbour.
    let spawn = |client: &mut Client, script: &str| -> String {
        client.ok(
            "session.spawn",
            json!({"kind": {"shell": {}}, "cwd": "/tmp", "argv": ["/bin/sh", "-c", script]}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let agent = spawn(
        &mut client,
        "stty -echo; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let other = spawn(&mut client, "sleep 30");

    // Saving copies the screenshot into an owner-only ring.
    let png = temp.path().join("capture.png");
    write_png(&png);
    let clip = client.ok(
        "clip.save",
        json!({
            "source": {
                "url": "http://127.0.0.1:3000/settings/?tab=2",
                "pageTitle": "Settings",
                "browser": "Arc",
                "appName": "Arc"
            },
            "text": "Save button overlaps the footer",
            "textOrigin": "ocr",
            "pngPath": png.to_string_lossy(),
        }),
    );
    let clip_id = clip["id"].as_str().unwrap().to_owned();
    assert_eq!(clip["tabKey"], "http://localhost:3000/settings");
    assert_eq!(
        (clip["width"].as_u64(), clip["height"].as_u64()),
        (Some(4), Some(2))
    );
    let stored = Path::new(clip["pngPath"].as_str().unwrap()).to_owned();
    assert_eq!(
        std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(stored.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let listed = client.ok("clip.list", json!({}));
    assert_eq!(listed["clips"][0]["id"], clip_id.as_str());
    assert_eq!(
        listed["clips"][0]["summary"],
        "localhost:3000/settings · 31 chars + PNG"
    );
    assert!(
        listed["clips"][0].get("text").is_none(),
        "lists carry no bodies"
    );

    // Nothing remembered yet: plain recency.
    let targets = client.ok("clip.targets", json!({"id": "latest"}));
    let reasons: Vec<&str> = targets["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["reason"].as_str().unwrap())
        .collect();
    assert!(reasons.iter().all(|r| *r == "recent"), "{reasons:?}");

    // Dispatch delivers prompt + context and remembers the page.
    let sent = client.ok(
        "clip.dispatch",
        json!({
            "id": clip_id,
            "prompt": "fix this padding",
            "destination": {"type": "session", "sessionID": agent},
        }),
    );
    assert_eq!(sent["delivered"], true);
    assert_eq!(sent["rememberedFor"], "http://localhost:3000/settings");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut screen = String::new();
    while Instant::now() < deadline && !screen.contains("got:fix this padding") {
        std::thread::sleep(Duration::from_millis(50));
        screen = client.ok("session.read_screen", json!({"sessionID": agent}))["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
    }
    assert!(screen.contains("got:fix this padding"), "{screen}");
    assert!(screen.contains("got:<diri-clip id="), "{screen}");

    // A later capture of another page on the same dev server suggests the
    // remembered Session first.
    write_png(&png);
    client.ok(
        "clip.save",
        json!({"source": {"url": "http://localhost:3000/billing"}, "pngPath": png.to_string_lossy()}),
    );
    let targets = client.ok("clip.targets", json!({"id": "latest"}));
    assert_eq!(targets["targets"][0]["sessionID"], agent.as_str());
    assert_eq!(targets["targets"][0]["reason"], "remembered");
    assert!(
        targets["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["sessionID"] == other.as_str())
    );

    // Ring-only dispatch remembers nothing; delete removes the screenshot.
    let kept = client.ok(
        "clip.dispatch",
        json!({"id": "latest", "destination": {"type": "ring"}}),
    );
    assert_eq!(kept["delivered"], false);
    assert!(kept.get("rememberedFor").is_none());
    client.ok("clip.delete", json!({"id": clip_id}));
    assert!(!stored.exists());
    let missing = client
        .call("clip.get", json!({"id": clip_id}))
        .expect_err("deleted");
    assert_eq!(missing["code"], "not_found");
    let bad = client
        .call("clip.capture", json!({"mode": "everything"}))
        .expect_err("validated");
    assert_eq!(bad["code"], "bad_request");
    client.ok("clip.capture", json!({"mode": "window"}));

    for id in [&agent, &other] {
        client.ok("session.kill", json!({"sessionID": id}));
    }
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    accepting.join().expect("server thread");
}
