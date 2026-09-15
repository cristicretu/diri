use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct Peer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    _temp: tempfile::TempDir,
}

impl Peer {
    fn new(respond: impl FnMut(&str) -> Value + Send + 'static) -> Self {
        Self::with_identity(
            respond,
            json!({"proto":diri_proto::WIRE_VERSION, "engineKind":diri_proto::RUST_ENGINE_KIND}),
        )
    }

    fn with_identity(
        mut respond: impl FnMut(&str) -> Value + Send + 'static,
        identity: Value,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("engine.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut subscriptions = Vec::new();
            while let Ok((mut stream, _)) = listener.accept() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    let diri_proto::ControlMessage::Request { id, method, .. } =
                        serde_json::from_str(&line).unwrap()
                    else {
                        panic!("request");
                    };
                    let value = if method == Method::HELLO {
                        identity.clone()
                    } else {
                        respond(&method)
                    };
                    serde_json::to_writer(
                        &mut stream,
                        &diri_proto::ControlMessage::Response {
                            id,
                            result: Ok(value),
                        },
                    )
                    .unwrap();
                    stream.write_all(b"\n").unwrap();
                    if method == Method::EVENTS_SUBSCRIBE {
                        subscriptions.push(stream.try_clone().unwrap());
                    }
                    if method != Method::HELLO {
                        break;
                    }
                }
            }
        });
        Self {
            path,
            stop,
            worker: Some(worker),
            _temp: temp,
        }
    }

    fn bridge(&self) -> Bridge {
        Bridge::new(self.path.clone(), Some("parent".into()))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn snapshots(children: &[(&str, SessionStatus)]) -> Value {
    let mut records = vec![super::tests::record("parent", None)];
    records.extend(children.iter().map(|(id, status)| {
        let mut record = super::tests::record(id, Some("parent"));
        record.status = status.clone();
        record
    }));
    json!({"sessions":records, "projects":[]})
}

#[test]
fn removed_child_does_not_make_another_working_child_settled() {
    let mut reads = 0;
    let peer = Peer::new(move |method| {
        assert_eq!(method, Method::SESSION_LIST);
        reads += 1;
        if reads == 1 {
            snapshots(&[
                ("removed", SessionStatus::Working),
                ("busy", SessionStatus::Working),
            ])
        } else {
            snapshots(&[("busy", SessionStatus::Working)])
        }
    });
    let result = peer
        .bridge()
        .call("wait_for_children", &json!({"timeout_s":0}))
        .unwrap();
    assert_eq!(
        result["settled"], false,
        "a missing child cannot complete the other child's work"
    );
}

#[test]
fn child_finishing_during_subscription_is_observed_without_another_event() {
    let mut subscribed = false;
    let peer = Peer::new(move |method| {
        if method == Method::EVENTS_SUBSCRIBE {
            subscribed = true; // Transition happens just before listener registration.
            json!({"subscribed":true})
        } else {
            assert_eq!(method, Method::SESSION_LIST);
            snapshots(&[(
                "child",
                if subscribed {
                    SessionStatus::Idle
                } else {
                    SessionStatus::Working
                },
            )])
        }
    });
    let result = peer
        .bridge()
        .call("wait_for_children", &json!({"timeout_s":0.2}))
        .unwrap();
    assert_eq!(
        result["settled"], true,
        "completion in the snapshot/subscribe gap was lost"
    );
    assert_eq!(result["timed_out"], false);
}

#[test]
fn single_agent_wait_returns_when_the_session_disappears() {
    let mut subscribed = false;
    let peer = Peer::new(move |method| {
        if method == Method::EVENTS_SUBSCRIBE {
            subscribed = true;
            json!({"subscribed":true})
        } else if subscribed {
            snapshots(&[])
        } else {
            snapshots(&[("child", SessionStatus::Working)])
        }
    });
    let result = peer
        .bridge()
        .call(
            "wait_for_agent",
            &json!({"session_id":"child", "timeout_s":0.2}),
        )
        .unwrap();
    assert_eq!(result["timedOut"], false);
    assert_eq!(result["removed"], true);
    assert_eq!(result["matched"], false);
}

#[test]
fn exited_agent_does_not_wait_forever_for_idle() {
    let peer = Peer::new(|_| {
        snapshots(&[(
            "child",
            SessionStatus::Exited(diri_proto::ExitInfo {
                reason: diri_proto::ExitReason::Exited,
                code: Some(1),
                signal: None,
            }),
        )])
    });
    let result = peer
        .bridge()
        .call(
            "wait_for_agent",
            &json!({"session_id":"child", "timeout_s":0}),
        )
        .unwrap();
    assert_eq!(result["timedOut"], false);
    assert_eq!(result["matched"], false);
}

#[test]
fn an_explicit_empty_child_selection_does_not_expand_to_every_child() {
    let peer = Peer::new(|_| snapshots(&[("child", SessionStatus::Working)]));
    let result = peer
        .bridge()
        .call(
            "wait_for_children",
            &json!({"session_ids":[], "timeout_s":0}),
        )
        .unwrap();
    assert_eq!(result["children"], json!([]));
    assert_eq!(result["settled"], true);
}

#[test]
fn mistyped_or_empty_routing_arguments_fail_before_discovery() {
    let bridge = Bridge::new(
        PathBuf::from("/unavailable-audit-fixture.sock"),
        Some("parent".into()),
    );
    for args in [
        json!({"kind":"claude", "cwd":"/tmp", "host":false}),
        json!({"kind":"claude", "cwd":"/tmp", "host":""}),
        json!({"kind":"claude", "cwd":"/tmp", "worktre":true}),
    ] {
        let error = bridge.call("spawn_agent", &args).unwrap_err();
        assert!(
            !error.contains("socket"),
            "invalid routing reached the Engine: {error}"
        );
    }
}

#[test]
fn malformed_submit_is_rejected_before_any_daemon_request() {
    let bridge = Bridge::new(
        PathBuf::from("/unavailable-audit-fixture.sock"),
        Some("parent".into()),
    );
    let error = bridge
        .call(
            "send_prompt",
            &json!({"session_id":"child", "text":"task", "submit":"false"}),
        )
        .unwrap_err();
    assert!(
        error.contains("submit") && error.contains("boolean"),
        "bad submit reached the daemon: {error}"
    );
}

#[test]
fn oversized_timeout_returns_an_error_instead_of_panicking() {
    let bridge = Bridge::new(
        PathBuf::from("/unavailable-audit-fixture.sock"),
        Some("parent".into()),
    );
    let result = std::panic::catch_unwind(|| {
        bridge.call(
            "wait_for_agent",
            &json!({"session_id":"child", "timeout_s":1e300}),
        )
    });
    assert!(result.is_ok(), "a tool argument crashed the MCP process");
    assert!(result.unwrap().unwrap_err().contains("timeout_s"));
}

#[test]
fn unverified_engine_never_receives_a_mutation() {
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = writes.clone();
    let peer = Peer::with_identity(
        move |method| {
            if method != Method::HELLO {
                observed.fetch_add(1, Ordering::SeqCst);
            }
            json!({}) // Missing the authoritative Rust Engine identity.
        },
        json!({}),
    );
    let result = peer.bridge().request(
        Method::SESSION_KILL,
        json!({"sessionID":"child"}),
        Duration::from_secs(1),
    );
    assert!(
        result.is_err(),
        "mutation was sent without verifying Engine identity"
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}
