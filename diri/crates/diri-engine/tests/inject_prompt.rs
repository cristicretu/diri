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
use diri_proto::{ControlError, ControlMessage};
use serde_json::json;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
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
        self.try_request(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }

    fn try_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ControlError> {
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
            ControlMessage::Response { result, .. } => result,
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

/// A successful spawn with an initial prompt is an acknowledgement that the
/// prompt reached the child, not merely that prompt delivery was scheduled.
#[test]
fn spawn_does_not_return_before_prompt_is_delivered() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let prompt = "acknowledge this prompt";
    let id = spawn(
        &mut control,
        r#"sleep 1.2; stty -echo; printf '\033[?2004h> '; exec cat"#,
        "/bin/sh",
        prompt,
    );

    let text = screen(&mut control, &id);
    assert!(
        text.contains(prompt),
        "session.spawn returned success before delivering its prompt: {text:?}"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}

#[test]
fn spawn_reports_when_the_child_exits_before_accepting_its_prompt() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let error = control
        .try_request(
            "session.spawn",
            json!({
                "kind": { "shell": {} },
                "cwd": "/tmp",
                "argv": ["/bin/sh", "-c", "exit 0"],
                "initialPrompt": "this cannot be delivered",
            }),
        )
        .expect_err("spawn must not acknowledge an undelivered prompt");

    assert_eq!(error.code, "initial_prompt_delivery_failed");
    assert!(
        error.message.contains("session s_") && error.message.contains("was not delivered"),
        "the error must identify the created session and the delivery failure: {error}"
    );
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

/// The Claude Code shape, and the one that used to lose prompts outright:
/// bracketed paste comes on EARLY, while the banner is still repainting, and
/// input typed into that window is discarded. A busy screen must not be
/// mistaken for "the prompt arrived" — the prompt itself has to show up.
#[test]
fn a_prompt_swallowed_behind_a_repainting_banner_still_lands() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    // Paste mode on immediately (the readiness tell), then ~7s of repainting
    // while every line typed is discarded, then a real reader. The screen
    // changes constantly throughout, so any "did the screen move?" check
    // reports success on the very first attempt and the prompt is lost.
    let id = spawn(
        &mut control,
        r#"printf '\033[?2004h'; stty -echo; end=$((SECONDS+7)); while [ $SECONDS -lt $end ]; do printf '.'; read -t 1 junk; done; exec cat"#,
        "/bin/bash",
        "prompt behind the banner",
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = screen(&mut control, &id);
        if text.contains("prompt behind the banner") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        text.contains("prompt behind the banner"),
        "a repainting banner was mistaken for a delivered prompt: {text:?}"
    );
    std::thread::sleep(Duration::from_millis(2500));
    let text = screen(&mut control, &id);
    assert_eq!(
        occurrences(&text, "prompt behind the banner"),
        1,
        "retries must stop the moment one attempt lands: {text:?}"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}

/// Codex collapses long pasted prompts in its composer. The visible summary
/// keeps the first line but can omit a probe chosen from the middle of the
/// prompt. Once Enter submits that accepted paste, losing the middle probe
/// from the viewport must not make the injector submit the prompt again.
#[test]
fn a_collapsed_long_prompt_is_submitted_once() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);

    let prompt = "Verify PR 5669\nhidden-probe-1234567\nprint the final report";
    let framed_len = prompt.len() + "\x1b[200~".len() + "\x1b[201~".len();
    let script = format!(
        r#"stty -echo -icanon min 1 time 0
printf '\033[?2004hREADY'
dd if=/dev/stdin of=/dev/null bs=1 count={framed_len} 2>/dev/null
printf '\r\033[2KVerify PR 5669'
dd if=/dev/stdin of=/dev/null bs=1 count=1 2>/dev/null
printf '\r\033[2KSUBMISSIONS=1'
dd if=/dev/stdin of=/dev/null bs=1 count=1 2>/dev/null
dd if=/dev/stdin of=/dev/null bs=1 count={framed_len} 2>/dev/null
printf '\r\033[2KVerify PR 5669'
dd if=/dev/stdin of=/dev/null bs=1 count=1 2>/dev/null
printf '\r\033[2KSUBMISSIONS=2 hidden-probe-1234567'
while :; do sleep 1; done"#
    );
    let id = spawn(
        &mut control,
        // Simulate a raw-mode TUI that renders only the first line while a
        // bracketed paste is in its composer. After submission it replaces
        // that summary with a counter. The second submission exposes the
        // hidden middle probe so the buggy retry loop terminates quickly.
        &script,
        "/bin/bash",
        prompt,
    );

    let text = screen(&mut control, &id);
    assert!(
        text.contains("SUBMISSIONS=1"),
        "an accepted collapsed prompt was submitted more than once: {text:?}"
    );
    assert!(
        !text.contains("hidden-probe-1234567"),
        "the injector retried until its off-screen probe appeared: {text:?}"
    );

    control.request("session.kill", json!({ "sessionID": id }));
}
