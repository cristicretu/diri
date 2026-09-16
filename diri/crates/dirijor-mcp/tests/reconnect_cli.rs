#![cfg(unix)]
use diri_proto::{ControlMessage, Method};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn reconnect_cli_sends_one_exact_request_and_preserves_uncertainty_in_json() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("fixture.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let expected = json!({"session":{"id":"fixture","remoteConnection":{"state":"reconnecting","since":0}},"started":true,"uncertainInputDiscarded":true});
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
        assert_eq!(method, Method::SESSION_RECONNECT);
        assert_eq!(params, Some(json!({"sessionID":"fixture"})));
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
        .args(["session", "reconnect", "fixture", "--json"])
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
fn reconnect_cli_rejects_unknown_options_before_contacting_engine() {
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["session", "reconnect", "fixture", "--restart-agent"])
        .env("DIRIJOR_SOCKET", "/nonexistent/reconnect-fixture.sock")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: session reconnect"));
}
