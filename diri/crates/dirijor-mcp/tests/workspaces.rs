#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use diri_engine::{ControlServer, ManifestEngine, Registry};
use diri_proto::{EventName, workspace::WorkspaceSnapshot};
use dirijor_mcp::ControlClient;
use serde_json::{Value, json};

struct Server {
    server: Arc<ControlServer>,
    registry: Arc<Mutex<Registry>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: PathBuf,
}
impl Server {
    fn start(directory: &Path, socket_name: &str) -> Self {
        let (engine, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let mut registry = Registry::new(Arc::new(engine), directory.join("state.json"));
        registry.load().unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let socket = directory.join(socket_name);
        let server = Arc::new(ControlServer::new(registry.clone(), &socket));
        let listener = server.bind().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let shared = server.clone();
        let thread = std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let server = shared.clone();
                std::thread::spawn(move || {
                    let _ = server.serve(stream);
                });
            }
        });
        Self {
            server,
            registry,
            stop,
            thread: Some(thread),
            socket,
        }
    }
    fn command(&self, words: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dirijor"))
            .args(words)
            .env_clear()
            .env("DIRIJOR_SOCKET", &self.socket)
            .output()
            .unwrap()
    }
    fn success(&self, words: &[&str]) -> Value {
        let output = self.command(words);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn record(id: &str, project: &str, host: Option<&str>) -> diri_proto::SessionRecord {
    use diri_proto::*;
    let now = DateMillis(1.0);
    SessionRecord {
        attention_state: None,
        id: SessionId::new(id),
        kind: AgentKind::CODEX,
        cwd: "/fixture".into(),
        project_id: ProjectId::new(project),
        worktree_path: None,
        git_branch: None,
        title: "parent".into(),
        title_source: TitleSource::Placeholder,
        account_profile: None,
        originating_prompt: None,
        agent_session_id: None,
        transcript_path: None,
        status: SessionStatus::Idle,
        status_evidence: None,
        needs_input: None,
        resumability: Resumability::Live,
        capabilities: None,
        parent: None,
        created_at: now,
        updated_at: now,
        last_turn_completed_at: None,
        last_seen_at: None,
        pinned: false,
        archived_at: None,
        host: host.map(str::to_owned),
        remote_persistence: None,
        hibernation: None,
        memory_bytes: None,
        artifacts: None,
        pull_requests: None,
        listening_ports: None,
        foreground_agent: None,
    }
}

#[test]
fn cli_and_another_client_share_durable_revisions_events_and_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("state.json"), serde_json::to_vec(&json!({
        "version": 1,
        "projects": [{"id":"local_project","name":"API","root":"/fixture"},{"id":"remote_project","name":"Deployment","root":"/fixture","host":"test_remote"}],
        "sessions": [record("local_session", "local_project", None), record("remote_session", "remote_project", Some("test_remote"))]
    })).unwrap()).unwrap();
    let server = Server::start(temp.path(), "first.sock");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let socket = server.socket.clone();
    let listener = std::thread::spawn(move || {
        let mut client = ControlClient::connect(&socket, Duration::from_secs(3)).unwrap();
        client
            .subscribe_observing(
                json!({"kinds": [EventName::WORKSPACE_UPDATED]}),
                Instant::now() + Duration::from_secs(5),
                |event| {
                    if let Some((name, _, value)) = event {
                        event_tx.send((name.to_owned(), value.clone())).unwrap();
                        Ok(false)
                    } else {
                        ready_tx.send(()).unwrap();
                        Ok(true)
                    }
                },
            )
            .unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let created = server.success(&[
        "workspace",
        "create",
        "API and deployment",
        "--revision",
        "0",
    ]);
    let (event_name, event) = event_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(event_name, EventName::WORKSPACE_UPDATED);
    assert_eq!(event["revision"], created["revision"]);
    listener.join().unwrap();
    let snapshot: WorkspaceSnapshot = serde_json::from_value(created.clone()).unwrap();
    let id = &snapshot.workspaces[0].id.0;
    let stale = server.command(&[
        "workspace",
        "rename",
        id,
        "Stale GUI title",
        "--revision",
        "0",
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("workspace_revision_conflict"));
    assert_eq!(server.success(&["workspace", "list", "--json"]), created);
    let tabs = server.success(&["tab", "create", id, "local_session"]);
    let tab_id = tabs["workspaces"][0]["tabs"][0]["id"].as_str().unwrap();
    let pane_id = tabs["workspaces"][0]["tabs"][0]["focusedPane"]
        .as_str()
        .unwrap();
    let split = server.success(&["pane", "split", tab_id, pane_id, "remote_session", "right"]);
    let split_id = split["workspaces"][0]["tabs"][0]["layout"]["id"]
        .as_str()
        .unwrap();
    let remote_pane = split["workspaces"][0]["tabs"][0]["focusedPane"]
        .as_str()
        .unwrap();
    server.success(&["pane", "resize", tab_id, split_id, "0.7"]);
    server.success(&["pane", "zoom", tab_id, remote_pane]);
    let renamed = server.success(&["workspace", "rename", id, "Shared name"]);
    let disk: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(disk["workspaceState"], renamed);
    assert_eq!(server.registry.lock().unwrap().record_count(), 2);

    for id in ["local_session", "remote_session"] {
        assert!(!server.server.attach_hub().has_sinks(id));
    }
    drop(server);
    let restarted = Server::start(temp.path(), "second.sock");
    assert_eq!(restarted.success(&["workspace", "list"]), renamed);
    assert_eq!(restarted.registry.lock().unwrap().record_count(), 2);
}
