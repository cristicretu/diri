#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

struct Mcp {
    child: Child,
    input: ChildStdin,
    replies: Receiver<Value>,
}

impl Mcp {
    fn new(socket: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dirijor-mcp"))
            .env("DIRIJOR_SOCKET", socket)
            .env("DIRIJOR_SESSION_ID", "parent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            replies,
        }
    }

    fn send(&mut self, value: Value) {
        serde_json::to_writer(&mut self.input, &value).unwrap();
        self.input.write_all(b"\n").unwrap();
        self.input.flush().unwrap();
    }

    fn initialize(&mut self) {
        self.send(json!({"jsonrpc":"2.0", "id":"init", "method":"initialize", "params":{
            "protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"test", "version":"1"}
        }}));
        assert_eq!(
            self.replies.recv_timeout(Duration::from_secs(2)).unwrap()["id"],
            "init"
        );
        self.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn receive_request(listener: &UnixListener) -> UnixStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let (stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "MCP never connected to fixture Engine"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(serde_json::from_str::<Value>(&line).is_ok());
    stream
}

#[test]
fn a_slow_tool_does_not_block_ping() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut mcp = Mcp::new(&socket);
    mcp.initialize();
    mcp.send(
        json!({"jsonrpc":"2.0", "id":"wait", "method":"tools/call", "params":{
            "name":"wait_for_agent", "arguments":{"session_id":"child", "timeout_s":10}
        }}),
    );
    let connection = receive_request(&listener); // Tool is now blocked in the Engine.
    let mut timings = Vec::new();
    for _ in 0..50 {
        let start = std::time::Instant::now();
        mcp.send(json!({"jsonrpc":"2.0", "id":"ping", "method":"ping"}));
        let response = mcp.replies.recv_timeout(Duration::from_millis(300));
        assert_eq!(
            response.expect("a waiting tool blocked the MCP input loop")["id"],
            "ping"
        );
        timings.push(start.elapsed());
    }
    drop(connection);
    timings.sort();
    eprintln!(
        "50 MCP pings during a blocked tool: median {:?}, p95 {:?}",
        timings[25], timings[47]
    );
}

#[test]
fn cancelling_a_wait_closes_its_engine_connection() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut mcp = Mcp::new(&socket);
    mcp.initialize();
    mcp.send(
        json!({"jsonrpc":"2.0", "id":"wait", "method":"tools/call", "params":{
            "name":"wait_for_agent", "arguments":{"session_id":"child", "timeout_s":10}
        }}),
    );
    let mut connection = receive_request(&listener);
    connection
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    mcp.send(
        json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":"wait"}}),
    );
    assert!(
        matches!(connection.read(&mut [0]), Ok(0)),
        "cancelled wait kept the Engine socket open"
    );
    mcp.send(json!({"jsonrpc":"2.0", "id":"ping", "method":"ping"}));
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        "ping",
        "cancelled requests must not emit a late response"
    );
}

#[test]
fn malformed_request_gets_an_error_instead_of_silent_disappearance() {
    let mut mcp = Mcp::new(Path::new("/unavailable-audit-fixture.sock"));
    mcp.initialize();
    mcp.send(json!({"jsonrpc":"2.0", "id":1}));
    mcp.send(json!({"jsonrpc":"2.0", "id":2, "method":"ping"}));
    let response = mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response["id"], 1, "malformed request was silently dropped");
    assert_eq!(response["error"]["code"], -32600);
}

#[test]
fn tools_cannot_run_before_the_mcp_handshake() {
    let mut mcp = Mcp::new(Path::new("/unavailable-audit-fixture.sock"));
    mcp.send(
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
            "name":"release_agent", "arguments":{"session_id":"child"}
        }}),
    );
    let response = mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        response["error"]["code"], -32000,
        "uninitialized tool reached the backend"
    );
    mcp.initialize();
    mcp.send(json!({"jsonrpc":"2.0", "id":2, "method":"ping"}));
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        2
    );
}

#[test]
fn unknown_protocol_versions_are_not_blindly_echoed() {
    let mut mcp = Mcp::new(Path::new("/unavailable-audit-fixture.sock"));
    mcp.send(json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
        "protocolVersion":"2099-99-99", "capabilities":{}, "clientInfo":{"name":"test", "version":"1"}
    }}));
    let response = mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
}

#[test]
fn reads_are_bounded_and_overload_does_not_block_ping() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut mcp = Mcp::new(&socket);
    mcp.initialize();
    let mut connections = Vec::new();
    for id in 0..8 {
        mcp.send(
            json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
                "name":"wait_for_agent", "arguments":{"session_id":"child", "timeout_s":10}
            }}),
        );
        connections.push(receive_request(&listener));
    }
    mcp.send(
        json!({"jsonrpc":"2.0", "id":"overflow", "method":"tools/call", "params":{
            "name":"wait_for_agent", "arguments":{"session_id":"child", "timeout_s":10}
        }}),
    );
    let response = mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response["id"], "overflow");
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    mcp.send(json!({"jsonrpc":"2.0", "id":"ping", "method":"ping"}));
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        "ping"
    );
    for (id, connection) in connections.iter_mut().enumerate() {
        mcp.send(
            json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":id}}),
        );
        assert!(matches!(connection.read(&mut [0]), Ok(0)));
    }
}

#[test]
fn mutations_are_ordered_and_a_cancelled_queued_mutation_never_dispatches() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut mcp = Mcp::new(&socket);
    mcp.initialize();
    let mutation = |id| {
        json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
            "name":"release_agent", "arguments":{"session_id":"child"}
        }})
    };
    mcp.send(mutation(1));
    let first = receive_request(&listener);
    mcp.send(mutation(2));
    mcp.send(
        json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2}}),
    );
    mcp.send(json!({"jsonrpc":"2.0", "id":"ping", "method":"ping"}));
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        "ping"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "a second mutation overtook the first"
    );
    drop(first);
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        1
    );
    // Put another mutation behind the cancelled slot to prove the queue drained.
    mcp.send(mutation(3));
    let third = receive_request(&listener);
    drop(third);
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        3
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "cancelled mutation reached the Engine"
    );
}

#[test]
fn oversized_frames_are_rejected_and_the_next_request_still_works() {
    let mut mcp = Mcp::new(Path::new("/unavailable-audit-fixture.sock"));
    mcp.initialize();
    mcp.input
        .write_all(&vec![b' '; diri_proto::control::MAX_CONTROL_LINE_BYTES + 1])
        .unwrap();
    mcp.input.write_all(b"\n").unwrap();
    let response = mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response["error"]["code"], -32600);
    mcp.send(json!({"jsonrpc":"2.0", "id":2, "method":"ping"}));
    assert_eq!(
        mcp.replies.recv_timeout(Duration::from_secs(1)).unwrap()["id"],
        2
    );
}
