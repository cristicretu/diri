//! Verified initial-prompt injection, end to end over the control socket:
//! the prompt must wait for the composer to come alive, land exactly once,
//! and be retried when a not-yet-ready TUI silently swallows it.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use serde_json::json;

fn engine() -> Arc<ManifestEngine> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../Sources/DirijorCore/Resources/manifests")
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

struct Control {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Control {
    fn connect(server: &ControlServer) -> Self {
        let stream = UnixStream::connect(server.socket_path()).expect("connect");
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

fn start_server(temp: &Path) -> Arc<ControlServer> {
    let registry = Arc::new(Mutex::new(Registry::new(engine(), temp.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), temp.join("daemon.sock"))
            .with_logs_dir(temp.join("logs")),
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

fn spawn(control: &mut Control, script: &str, shell: &str, prompt: &str) -> String {
    let spawned = control.request(
        "session.spawn",
        json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": [shell, "-c", script],
            "initialPrompt": prompt,
        }),
    );
    spawned["id"].as_str().expect("id").to_string()
}

fn screen(control: &mut Control, id: &str) -> String {
    control.request("session.read_screen", json!({ "sessionID": id }))["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// A TUI that paints nothing for over a second, then brings its composer up
/// (bracketed paste on). The prompt must not be typed into the void — it
/// lands promptly after the composer exists, exactly once.
#[test]
fn the_prompt_waits_for_the_composer_and_lands_once() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let started = Instant::now();
    let id = spawn(
        &mut control,
        // tty echo off, so each delivered prompt paints exactly once (cat's
        // copy) and the once-only assertion below is exact.
        r#"sleep 1.2; stty -echo; printf '\033[?2004h> '; exec cat"#,
        "/bin/sh",
        "hello from the injector",
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = screen(&mut control, &id);
        if text.contains("hello from the injector") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        text.contains("hello from the injector"),
        "the prompt never reached the composer: {text:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "bracketed paste is the readiness tell; the prompt should not wait \
         for a long stability timeout once it is on"
    );
    // Settle, then confirm no retry duplicated it.
    std::thread::sleep(Duration::from_millis(2500));
    let text = screen(&mut control, &id);
    assert_eq!(
        occurrences(&text, "hello from the injector"),
        1,
        "a delivered prompt must not be retyped: {text:?}"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}

/// A TUI that paints a banner but then SILENTLY eats input for a while (no
/// echo, no screen change) — the swallowed first attempt must be detected
/// and retried until the real reader is up, without duplication.
#[test]
fn a_silently_swallowed_prompt_is_retried_until_it_lands() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    // FROZEN paints immediately (so readiness fires on screen stability),
    // then every line typed for ~3s is discarded with echo off — the screen
    // stays byte-identical, which is the ONLY state that permits a retry.
    // Echo stays off after the swallow too, so a delivered prompt paints
    // exactly once (cat's copy) and the count below is exact.
    let id = spawn(
        &mut control,
        r#"printf FROZEN; stty -echo; end=$((SECONDS+3)); while [ $SECONDS -lt $end ]; do read -t 1 junk; done; exec cat"#,
        "/bin/bash",
        "the retried prompt",
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = screen(&mut control, &id);
        if text.contains("the retried prompt") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        text.contains("the retried prompt"),
        "the swallowed prompt was never retried: {text:?}"
    );
    std::thread::sleep(Duration::from_millis(2500));
    let text = screen(&mut control, &id);
    assert_eq!(
        occurrences(&text, "the retried prompt"),
        1,
        "retries must stop the moment one attempt lands: {text:?}"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}
