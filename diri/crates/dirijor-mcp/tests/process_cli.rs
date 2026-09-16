#![cfg(unix)]
use diri_proto::{ControlMessage, Method};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn process_cli_sends_one_exact_request_and_preserves_typed_partial_facts_and_group_labels() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("fixture.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let expected = json!({"sessionID":"fixture","host":"test-host","observedAt":0,"process":{
        "identity":{"pid":401,"birth":{"platform":"linux","bootId":"01234567-89ab-cdef-0123-456789abcdef","startTicks":77,"clockTicksPerSecond":100}},
        "executable":{"status":"available","value":"/usr/bin/fixture"},
        "workingDirectory":{"status":"available","value":"/work/雪"},
        "userIds":{"status":"available","value":{"real":1000,"effective":1001}},
        "account":{"status":"unavailable","reason":"timed_out"},
        "processGroup":{"status":"available","value":401},
        "foregroundProcessGroup":{"status":"available","value":902}
    }});
    for as_json in [true, false] {
        let response = expected.clone();
        let listener = listener.try_clone().unwrap();
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
            let ControlMessage::Request { id, method, .. } =
                serde_json::from_str(&hello_line).unwrap()
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
            let ControlMessage::Request { id, method, params } =
                serde_json::from_str(&line).unwrap()
            else {
                panic!("expected request");
            };
            assert_eq!(method, Method::SESSION_PROCESS_INFO);
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_dirijor"));
        command.args(["session", "process", "fixture"]);
        if as_json {
            command.arg("--json");
        }
        let output = command.env("DIRIJOR_SOCKET", &socket).output().unwrap();
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if as_json {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
                expected
            );
        } else {
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("Child PID: 401"));
            assert!(text.contains("Foreground PGID: 902"));
            assert!(text.contains("Effective account: unavailable (timed_out)"));
            assert!(text.contains("雪"));
            assert!(!text.contains("Foreground PID"));
        }
    }
}

#[test]
fn process_cli_rejects_unknown_options_before_contacting_engine() {
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["session", "process", "fixture", "--restart-agent"])
        .env("DIRIJOR_SOCKET", "/nonexistent/reconnect-fixture.sock")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: session process"));
}
