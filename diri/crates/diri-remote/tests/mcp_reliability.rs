#![cfg(unix)]
//! The ordinary test runs real Holders through a disposable fake SSH executable.
//! The ignored test uses a real, explicitly opted-in SSH account. Both exercise
//! production Engine + MCP Bridge calls; the Agent is a deterministic shell fixture.
use diri_engine::remote::{
    binding::RemoteBindingStore,
    executor::ProcessExecutor,
    manager::{ArtifactCatalog, InstalledHelper, RemoteManager},
};
use diri_engine::{ManifestEngine, control::ControlServer, registry::Registry};
use diri_proto::remote_pty::{RemoteProcessState, SessionSelector};
use diri_proto::{HostEntry, Method};
use dirijor_mcp::Bridge;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

struct Engine {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    registry: Arc<Mutex<Registry>>,
}
impl Engine {
    fn start(root: &Path, manager: Arc<RemoteManager>) -> Self {
        let manifests = root.join("manifests");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(manifests.join("fixture.json"),serde_json::to_vec(&json!({
            "schemaVersion":1,"id":"mcp-fixture","version":"1","statusModel":"processOnly",
            "agent":{"binary":"/bin/sh","statusAuthority":"process","spawnArgs":["-c","stty -echo; printf 'READY\\n'; while IFS= read -r line; do printf 'ACCEPT:%s\\n' \"$line\"; done"]},"rules":[]
        })).unwrap()).unwrap();
        let (catalog, errors) = ManifestEngine::load_dir(&manifests).unwrap();
        assert!(errors.is_empty());
        let mut registry = Registry::new(Arc::new(catalog), root.join("state.json"));
        registry.load().unwrap();
        if registry.records().iter().all(|r| r.id.0 != "parent") {
            registry.insert_record(serde_json::from_value(json!({
                "id":"parent","kind":{"codex":{}},"cwd":root,"projectID":"parent-project","title":"fixture parent","titleSource":0,
                "status":{"idle":{}},"resumability":"notResumable","createdAt":0,"updatedAt":0,"pinned":false
            })).unwrap());
        }
        let registry = Arc::new(Mutex::new(registry));
        let server = Arc::new(
            ControlServer::new(registry.clone(), root.join("daemon.sock")).with_remote(manager),
        );
        let listener = server.bind().unwrap();
        listener.set_nonblocking(true).unwrap();
        server.spawn_remote_restore();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let server = server.clone();
                        requests.push(std::thread::spawn(move || {
                            let _ = server.serve(stream);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
            for request in requests {
                request.join().unwrap();
            }
        });
        Self {
            stop,
            worker: Some(worker),
            registry,
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
        self.registry
            .lock()
            .unwrap()
            .persist_for_shutdown()
            .unwrap();
    }
}

// Drop exactly one selected Engine reply after the operation has completed.
// All discovery/Hello requests still run through the actual Bridge.
fn lose_reply(
    root: &Path,
    method: &'static str,
    call: impl FnOnce(Bridge) -> Result<Value, String>,
) -> Result<Value, String> {
    let socket = root.join("loss.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let real = root.join("daemon.sock");
    let worker = std::thread::spawn(move || {
        let mut dropped = false;
        while !stopped.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((client, _)) => {
                    client.set_nonblocking(false).unwrap();
                    client
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut client = BufReader::new(client);
                    let mut engine = BufReader::new(UnixStream::connect(&real).unwrap());
                    engine
                        .get_ref()
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .unwrap();
                    let mut request = String::new();
                    while client.read_line(&mut request).unwrap_or(0) > 0 {
                        engine.get_mut().write_all(request.as_bytes()).unwrap();
                        let value: Value = serde_json::from_str(&request).unwrap();
                        request.clear();
                        let mut response = String::new();
                        engine.read_line(&mut response).unwrap();
                        if !dropped && value["method"] == method {
                            dropped = true;
                            break;
                        }
                        client.get_mut().write_all(response.as_bytes()).unwrap();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => panic!("loss proxy: {e}"),
            }
        }
        dropped
    });
    let result = call(Bridge::new(socket, Some("parent".into())));
    stop.store(true, Ordering::SeqCst);
    assert!(
        worker.join().unwrap(),
        "requested failure boundary {method} was not exercised: {result:?}"
    );
    result
}

struct Cleanup {
    manager: Arc<RemoteManager>,
    helper: InstalledHelper,
    selector: SessionSelector,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.manager.kill(&self.helper, &self.selector);
    }
}
fn screen(bridge: &Bridge, id: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(value) = bridge.call("read_output", &json!({"session_id":id})) {
            let text = value["text"].as_str().unwrap_or_default().to_owned();
            if text.contains(needle) {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "remote screen did not show {needle}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn run(real: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    let ssh = if real {
        std::env::var_os("DIRI_REMOTE_SSH_EXECUTABLE").unwrap_or_else(|| "ssh".into())
    } else {
        let path = root.join("ssh");
        std::fs::write(&path,format!("#!/bin/sh\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n",home.display(),root.join("remote-state").display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.into_os_string()
    };
    let helper = std::env::var_os("DIRI_REMOTE_HELPER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_diri-remote")));
    let manager = Arc::new(
        RemoteManager::new(
            ProcessExecutor::new(ssh),
            ArtifactCatalog::from_native_helper(&helper).unwrap(),
            root.join("control"),
        )
        .unwrap(),
    );
    let host = HostEntry {
        id: "soak".into(),
        name: None,
        ssh: if real {
            std::env::var("DIRI_REMOTE_SSH_TARGET").expect("opt-in disposable SSH account")
        } else {
            "fixture".into()
        },
        default_cwd: None,
        node: None,
    };
    std::fs::write(
        root.join("hosts.json"),
        serde_json::to_vec(&json!({"hosts":[host]})).unwrap(),
    )
    .unwrap();
    let mut engine = Some(Engine::start(root, manager.clone()));
    let bridge = Bridge::new(root.join("daemon.sock"), Some("parent".into()));
    let args = json!({"kind":"mcp-fixture","host":"soak","cwd":if real{std::env::var("DIRI_REMOTE_CWD").unwrap_or_else(|_|"~".into())}else{home.to_string_lossy().into_owned()},"operation_id":"spawn-one"});
    assert!(
        lose_reply(root, Method::SESSION_SPAWN_TRACKED, |b| b
            .call("spawn_agent", &args))
        .is_err()
    );
    let spawned = bridge.call("spawn_agent", &args).unwrap();
    assert_eq!(spawned["spawn_receipt"]["duplicate"], true);
    assert_eq!(spawned["ok"], true, "{spawned}");
    let id = spawned["id"].as_str().unwrap();
    let bindings = RemoteBindingStore::new(root.join("remote-bindings")).unwrap();
    let owned = bindings.load_all().unwrap();
    assert_eq!(owned.len(), 1, "spawn retry orphaned a Holder");
    let binding = &owned[0];
    let helper = manager
        .existing_helper(&host, &binding.helper_build_id, binding.protocol)
        .unwrap();
    let selector = SessionSelector {
        session_id: id.into(),
        session_token: binding.session_token.clone(),
        expected_incarnation: Some(binding.session_incarnation.clone()),
    };
    let cleanup = Cleanup {
        manager: manager.clone(),
        helper: helper.clone(),
        selector: selector.clone(),
    };
    let before = manager.inspect(&helper, &selector).unwrap();
    assert!(matches!(
        before.process_state,
        RemoteProcessState::Running { .. }
    ));
    screen(&bridge, id, "READY");
    let rounds = if real {
        std::env::var("DIRI_MCP_SOAK_ROUNDS")
            .ok()
            .map(|v| v.parse::<usize>().unwrap())
            .unwrap_or(5)
    } else {
        2
    };
    assert!((1..=100).contains(&rounds));
    for round in 0..rounds {
        let marker = format!("only-once-{round}");
        let message =
            json!({"session_id":id,"text":marker,"message_id":format!("message-{round}")});
        assert!(
            lose_reply(root, Method::SESSION_DELIVER_MESSAGE, |b| b
                .call("send_prompt", &message))
            .is_err()
        );
        for _ in 0..10 {
            assert_eq!(
                bridge.call("send_prompt", &message).unwrap()["receipt"]["duplicate"],
                true
            );
        }
        assert_eq!(
            screen(&bridge, id, &format!("ACCEPT:{marker}"))
                .matches(&format!("ACCEPT:{marker}"))
                .count(),
            1
        );
        let task_args = json!({"session_id":id,"text":format!("task-payload-{round}"),"request_id":format!("task-{round}")});
        assert!(
            lose_reply(root, Method::TASK_SUBMIT, |b| b
                .call("submit_task", &task_args))
            .is_err()
        );
        let task = bridge.call("submit_task", &task_args).unwrap();
        assert_eq!(task["duplicate"], true);
        let task_id = task["task"]["task_id"].as_str().unwrap();
        let cancel = dirijor_mcp::cancellation::Cancellation::default();
        let waiting = bridge.clone().with_cancellation(cancel.clone());
        let wait_id = task_id.to_owned();
        let waiter = std::thread::spawn(move || {
            waiting.call("wait_for_task", &json!({"task_id":wait_id,"timeout_s":60}))
        });
        std::thread::sleep(Duration::from_millis(30));
        let start = Instant::now();
        cancel.cancel();
        assert!(waiter.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            bridge
                .call("get_task", &json!({"task_id":task_id}))
                .unwrap()["status"],
            "awaiting_acknowledgement"
        );
        screen(&bridge, id, &format!("ACCEPT:task-payload-{round}"));
        drop(engine.take());
        manager.close_control_masters().unwrap();
        if real {
            std::thread::sleep(Duration::from_secs(
                std::env::var("DIRI_REMOTE_SOAK_SECONDS")
                    .ok()
                    .map(|v| v.parse().unwrap())
                    .unwrap_or(2),
            ));
        }
        engine = Some(Engine::start(root, manager.clone()));
        screen(&bridge, id, &format!("ACCEPT:task-payload-{round}"));
        let after = manager.inspect(&helper, &selector).unwrap();
        assert_eq!(
            before.process_state, after.process_state,
            "Agent process changed across restart"
        );
        assert_eq!(bridge.call("spawn_agent", &args).unwrap()["id"], id);
        assert_eq!(
            bridge.call("submit_task", &task_args).unwrap()["duplicate"],
            true
        );
        let child = Bridge::new(root.join("daemon.sock"), Some(id.into()));
        child
            .call(
                "report_task",
                &json!({"task_id":task_id,"status":"acknowledged"}),
            )
            .unwrap();
        let result = json!({"task_id":task_id,"status":"completed","result":format!("verified round {round}")});
        child.call("report_task", &result).unwrap();
        child.call("report_task", &result).unwrap();
        assert_eq!(
            bridge
                .call("wait_for_task", &json!({"task_id":task_id,"timeout_s":0}))
                .unwrap()["completed"],
            true
        );
        eprintln!(
            "MCP remote round {round}: one spawn, one message, task acknowledgement, cancellation, same process after Engine/SSH restart"
        );
    }
    drop(engine.take());
    drop(cleanup);
}
#[test]
fn mcp_failures_preserve_one_remote_session_and_task_identity() {
    run(false);
}
#[test]
#[ignore = "requires DIRI_REMOTE_SSH_TARGET naming a disposable real SSH account"]
fn real_ssh_mcp_reliability_soak() {
    run(true);
}
