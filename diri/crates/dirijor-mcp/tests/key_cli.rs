#![cfg(unix)]
use diri_proto::{ControlMessage, Method};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn key_cli_sends_one_typed_request_and_reports_admission() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("fixture.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let expected = json!({"bytesAccepted": 7});
    let response = expected.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut hello_line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut hello_line)
            .unwrap();
        let ControlMessage::Request { id, method, .. } = serde_json::from_str(&hello_line).unwrap()
        else {
            panic!("hello");
        };
        assert_eq!(method, Method::HELLO);
        serde_json::to_writer(&mut stream, &ControlMessage::Response { id, result: Ok(json!({"proto":diri_proto::WIRE_VERSION,"engineKind":diri_proto::RUST_ENGINE_KIND})) }).unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ControlMessage::Request { id, method, params } = serde_json::from_str(&line).unwrap()
        else {
            panic!("expected request");
        };
        assert_eq!(method, Method::SESSION_SEND_KEY);
        assert_eq!(
            params,
            Some(
                json!({"sessionID":"fixture", "key":{"named":"f5"}, "modifiers":{"shift":true,"ctrl":false,"alt":true,"cmd":false}, "action":"repeat"})
            )
        );
        serde_json::to_writer(
            &mut stream,
            &ControlMessage::Response {
                id,
                result: Ok(response),
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args([
            "session", "key", "fixture", "f5", "--shift", "--alt", "--repeat", "--json",
        ])
        .env("DIRIJOR_SOCKET", &socket)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        expected
    );
}

#[test]
fn key_cli_rejects_invalid_events_before_contacting_engine() {
    for arguments in [
        vec!["session", "key", "fixture", "enter", "--release"],
        vec!["session", "key", "fixture", "enter", "--ctrl", "--ctrl"],
        vec!["session", "key", "fixture", "two characters"],
        vec![
            "session",
            "key",
            "fixture",
            "enter",
            "--json",
            "--unsupported",
        ],
        vec![
            "session",
            "key",
            "fixture",
            "enter",
            "--repeat",
            "--release",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
            .args(arguments)
            .env("DIRIJOR_SOCKET", "/nonexistent/key-fixture.sock")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            !error.contains("connect"),
            "validation must precede Engine connection: {error}"
        );
    }
}
