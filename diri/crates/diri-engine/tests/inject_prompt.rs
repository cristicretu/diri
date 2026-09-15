//! Verified initial-prompt injection, end to end over the control socket:
//! the prompt must wait for the composer to come alive, land exactly once,
//! with uncertain outcomes reported without replaying text or Enter.

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
    start_server_with_engine(temp, engine())
}

fn start_server_with_engine(temp: &Path, engine: Arc<ManifestEngine>) -> Arc<ControlServer> {
    let registry = Arc::new(Mutex::new(Registry::new(engine, temp.join("state.json"))));
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

/// Keep Codex's real screen rules, with a deterministic child instead of an
/// installed Agent or account. The submitted prompt remains in the transcript.
#[test]
fn codex_prompt_retained_in_transcript_is_acknowledged_once() {
    assert_codex_delivery(false);
}

#[test]
fn codex_banner_repaint_neither_acknowledges_nor_retries_a_swallowed_enter() {
    assert_codex_delivery(true);
}

fn assert_codex_delivery(swallow_first_enter: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let manifests = temp.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(diri_engine::detect::bundled_manifest_dir().join("codex.json")).unwrap(),
    )
    .unwrap();
    manifest["agent"].as_object_mut().unwrap().remove("binary");
    std::fs::write(
        manifests.join("codex.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let (engine, _) = ManifestEngine::load_dir(&manifests).unwrap();
    let server = start_server_with_engine(temp.path(), Arc::new(engine));
    let mut control = Control::connect(&server);
    let prompt = "testing";
    let framed_len = prompt.len() + 12;
    let accepted = temp.path().join("accepted");
    let extra_enter = temp.path().join("extra-enter");
    let swallow = if swallow_first_enter {
        // Unrelated banner output while the actual composer keeps the paste.
        // A changed screen alone must not acknowledge this first Enter.
        "printf '\\033[s\\033[1;40HUpdated banner\\033[u'; dd of=/dev/null bs=1 count=1 2>/dev/null"
    } else {
        ""
    };
    let script = format!(
        r#"stty -echo -icanon min 1 time 0
printf '\033[?2004h› '
dd of=/dev/null bs=1 count={framed_len} 2>/dev/null
printf '{prompt}'
dd of=/dev/null bs=1 count=1 2>/dev/null
{swallow}
printf accepted > '{}'
printf '\r\033[2K› {prompt}\r\nAnswer received.\r\n› '
dd of=/dev/null bs=1 count=1 2>/dev/null
printf duplicate > '{}'
exec cat"#,
        accepted.display(),
        extra_enter.display()
    );
    let result = control.try_request(
        "session.spawn",
        json!({
            "kind": { "codex": {} }, "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", script], "initialPrompt": prompt,
        }),
    );
    let id = match &result {
        Ok(value) => value["id"].as_str().unwrap().to_owned(),
        Err(error) => error.message.split_whitespace().nth(1).unwrap().to_owned(),
    };
    control.request("session.kill", json!({ "sessionID": id }));
    if swallow_first_enter {
        assert!(!accepted.exists(), "a second Enter must not be sent");
        assert!(result.is_err(), "a swallowed Enter cannot be confirmed");
    } else {
        assert!(
            accepted.exists(),
            "fixture must receive the submitted prompt"
        );
        assert!(result.is_ok(), "sent prompt reported as failed: {result:?}");
    }
    assert!(
        !extra_enter.exists(),
        "accepted prompt received another Enter"
    );
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
        error.message.contains("session s_")
            && error.message.contains("delivery was not confirmed"),
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

/// The first Enter may be accepted while the old composer remains visible.
/// Sending a second Enter can submit another turn or answer a new dialog.
#[test]
fn a_visible_prompt_with_delayed_acceptance_gets_only_one_enter() {
    for prompt in ["verify delayed acceptance", "hi"] {
        let temp = tempfile::tempdir().expect("temp");
        let server = start_server(temp.path());
        let mut control = Control::connect(&server);
        let capture = temp.path().join("received");
        let framed_len = prompt.len() + 12;
        let script = format!(
            r#"stty raw -echo
printf '\033[?2004hREADY'
dd bs=1 count={framed_len} of='{}' 2>/dev/null
printf '\r\033[2K{prompt}'
dd bs=1 count=1 >>'{}' 2>/dev/null
# Accept without repainting; capture any erroneous extra Enter.
exec cat >>'{}'"#,
            capture.display(),
            capture.display(),
            capture.display(),
        );
        let error = control
            .try_request(
                "session.spawn",
                json!({
                    "kind": {"shell":{}}, "cwd":"/tmp",
                    "argv":["/bin/sh", "-c", script], "initialPrompt":prompt,
                }),
            )
            .expect_err("acceptance is unknown");
        let id = error.message.split_whitespace().nth(1).unwrap();
        control.request("session.kill", json!({"sessionID":id}));
        assert_eq!(
            std::fs::read(capture).unwrap(),
            format!("\x1b[200~{prompt}\x1b[201~\r").as_bytes(),
            "an unchanged composer must not cause extra Enter presses"
        );
    }
}

/// Accepting input does not require displaying it. A remote TUI can keep
/// painting the same viewport while it processes a submitted task.
#[test]
fn an_accepted_prompt_without_a_visible_echo_is_never_replayed() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start_server(temp.path());
    let mut control = Control::connect(&server);
    let prompt = "hidden task";
    let capture = temp.path().join("received");
    let bytes = prompt.len() + 13; // bracketed paste plus Enter
    let script = format!(
        r#"stty raw -echo
printf '\033[?2004hREADY'
dd bs=1 count={bytes} of='{}' 2>/dev/null
# The agent has accepted the task; its viewport has not changed.
dd bs=1 count={} >>'{}' 2>/dev/null
printf '{prompt}'
exec cat"#,
        capture.display(),
        bytes + 1,
        capture.display(),
    );
    let result = control.try_request(
        "session.spawn",
        json!({
            "kind": { "shell": {} }, "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", script], "initialPrompt": prompt,
        }),
    );
    let id = match &result {
        Ok(value) => value["id"].as_str().unwrap().to_owned(),
        Err(error) => error.message.split_whitespace().nth(1).unwrap().to_owned(),
    };
    control.request("session.kill", json!({ "sessionID": id }));
    let received = std::fs::read(capture).expect("captured input");
    assert_eq!(
        received,
        format!("\x1b[200~{prompt}\x1b[201~\r").as_bytes(),
        "missing screen echo must not replay a submitted task"
    );
    assert!(
        result.is_err(),
        "an unobservable outcome must be reported honestly"
    );
}
