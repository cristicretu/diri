//! The `structured.*` RPC surface, driven through the registry with fake Agents.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use diri_engine::structured_registry::StructuredRegistry;
use serde_json::{Value, json};

const CLAUDE_TEXT: &str =
    include_str!("../../diri-proto/tests/fixtures/structured/claude-text-stream.jsonl");

fn fake_claude(dir: &std::path::Path) -> PathBuf {
    let replay = dir.join("replay.jsonl");
    fs::write(&replay, CLAUDE_TEXT).unwrap();
    let path = dir.join("claude");
    fs::write(
        &path,
        format!("#!/bin/sh\ncat '{}'\ncat > /dev/null\n", replay.display()),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn call(registry: &mut StructuredRegistry, method: &str, params: Value) -> Result<Value, String> {
    let result = match method {
        "structured.open" => registry.open(Some(params)),
        "structured.send" => registry.send(Some(params)),
        "structured.configure" => registry.configure(Some(params)),
        "structured.poll" => registry.poll(Some(params)),
        "structured.answer_permission" => registry.answer_permission(Some(params)),
        "structured.stop" => registry.stop(Some(params)),
        "structured.close" => registry.close(Some(params)),
        "structured.models" => registry.models(Some(params)),
        other => panic!("unexpected method {other}"),
    };
    result.map_err(|error| error.code)
}

#[test]
fn open_send_poll_round_trip_through_the_rpc_surface() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry =
        StructuredRegistry::default().with_program("claude", fake_claude(dir.path()));
    let opened = call(
        &mut registry,
        "structured.open",
        json!({"harness": "claude", "cwd": dir.path()}),
    )
    .unwrap();
    assert_eq!(opened["spawnCount"], 0, "open starts no process");
    let id = opened["conversationId"].as_str().unwrap().to_owned();

    call(
        &mut registry,
        "structured.configure",
        json!({"conversationId": id, "model": "claude-opus-5-5", "effort": "high"}),
    )
    .unwrap();
    let sent = call(
        &mut registry,
        "structured.send",
        json!({"conversationId": id, "text": "hi"}),
    )
    .unwrap();
    assert_eq!(sent["spawnCount"], 1);

    let mut text = String::new();
    let mut sequences = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let polled = call(
            &mut registry,
            "structured.poll",
            json!({"conversationId": id}),
        )
        .unwrap();
        for event in polled["events"].as_array().unwrap() {
            sequences.push(event["sequence"].as_u64().unwrap());
            if event["kind"] == "textDelta" {
                text.push_str(event["text"].as_str().unwrap());
            }
        }
        if polled["turnInFlight"] == false {
            break;
        }
        assert!(Instant::now() < deadline, "turn did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(text, "pong");
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn unknown_conversations_and_bad_decisions_are_structured_errors() {
    let mut registry = StructuredRegistry::default();
    assert_eq!(
        call(
            &mut registry,
            "structured.poll",
            json!({"conversationId": "nope"})
        )
        .unwrap_err(),
        "not_found"
    );
    let opened = call(
        &mut registry,
        "structured.open",
        json!({"harness": "claude", "cwd": "/tmp"}),
    )
    .unwrap();
    let id = opened["conversationId"].as_str().unwrap();
    assert_eq!(
        call(
            &mut registry,
            "structured.answer_permission",
            json!({"conversationId": id, "requestId": "x", "decision": "maybe"})
        )
        .unwrap_err(),
        "bad_request"
    );
}

#[test]
fn claude_catalog_is_static_and_codex_catalog_is_probed_once() {
    let mut registry = StructuredRegistry::default();
    let claude = call(
        &mut registry,
        "structured.models",
        json!({"harness": "claude"}),
    )
    .unwrap();
    assert_eq!(claude["source"], "static");
    assert!(
        claude["efforts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "xhigh")
    );
}

#[test]
fn a_harness_is_sent_as_its_camel_case_name() {
    let mut registry = StructuredRegistry::default();
    let opened = call(
        &mut registry,
        "structured.open",
        json!({"harness": "antigravity", "cwd": "/tmp"}),
    )
    .unwrap();
    assert_eq!(opened["harness"], "antigravity");
}

fn run_one_turn(registry: &mut StructuredRegistry, id: &str) {
    call(
        registry,
        "structured.send",
        json!({"conversationId": id, "text": "hi"}),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let polled = call(registry, "structured.poll", json!({"conversationId": id})).unwrap();
        if polled["turnInFlight"] == false {
            return;
        }
        assert!(Instant::now() < deadline, "turn did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_pty_and_the_structured_driver_never_hold_one_conversation_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry =
        StructuredRegistry::default().with_program("claude", fake_claude(dir.path()));
    let opened = call(
        &mut registry,
        "structured.open",
        json!({"harness": "claude", "cwd": dir.path()}),
    )
    .unwrap();
    let id = opened["conversationId"].as_str().unwrap().to_owned();

    // No native session before turn 1: nothing to resume.
    let before = registry
        .switch_to_pty(Some(json!({"conversationId": id})))
        .unwrap_err();
    assert_eq!(before.code, "bad_request");

    run_one_turn(&mut registry, &id);
    let command = registry
        .switch_to_pty(Some(json!({"conversationId": id})))
        .unwrap();
    assert_eq!(command.program, "claude");
    assert_eq!(
        command.args,
        vec!["--resume".to_owned(), command.native_id.clone()]
    );

    // While the PTY holds it, the structured side refuses to write.
    assert_eq!(
        call(
            &mut registry,
            "structured.send",
            json!({"conversationId": id, "text": "x"})
        )
        .unwrap_err(),
        "pty_attached"
    );
    assert_eq!(
        registry
            .switch_to_pty(Some(json!({"conversationId": id})))
            .unwrap_err()
            .code,
        "pty_attached",
        "a second handoff is refused"
    );

    registry
        .switch_to_gui(Some(json!({"conversationId": id})))
        .unwrap();
    run_one_turn(&mut registry, &id);
}

#[test]
fn antigravity_refuses_the_pty_handoff() {
    let mut registry = StructuredRegistry::default();
    let opened = call(
        &mut registry,
        "structured.open",
        json!({"harness": "antigravity", "cwd": "/tmp"}),
    )
    .unwrap();
    let id = opened["conversationId"].as_str().unwrap();
    assert_eq!(
        registry
            .switch_to_pty(Some(json!({"conversationId": id})))
            .unwrap_err()
            .code,
        "bad_request",
        "no native session yet"
    );
}
