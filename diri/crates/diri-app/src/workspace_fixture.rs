//! Opt-in native evidence backed by real disposable shell PTYs.
use diri_engine::{
    Authority, ControlServer, ManifestEngine, PtySpec, Registry, session::SessionSpec,
    workspace::WorkspaceStore,
};
use diri_proto::{SessionId, workspace::*};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) struct LiveWorkspace {
    _resources: ServerResources,
    pub(crate) services: Arc<crate::AppServices>,
    pub(crate) workspace: WorkspaceId,
    pub(crate) directory: tempfile::TempDir,
    registry: Arc<Mutex<Registry>>,
    initial_pids: Vec<i32>,
}
struct ServerResources {
    registry: Arc<Mutex<Registry>>,
    stop: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<std::os::unix::net::UnixStream>>>,
    server: Option<std::thread::JoinHandle<()>>,
}
impl LiveWorkspace {
    pub(crate) fn start() -> Self {
        Self::start_with_script(
            r#"stty -echo; printf ready > ready; while IFS= read -r line; do printf '\033[2J\033[H'; stty size > geometry; printf 'Actual PTY rows / columns: '; stty size; printf 'Output update: %s\n' "$line"; printf '\nThis is ordinary shell output. The terminal wraps this complete sentence at the width owned by this pane, including words that cross the right edge. No fixed screenshot grid is used here.\n\n$ '; done"#,
        )
    }

    pub(crate) fn start_with_script(script: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.sock");
        let state = directory.path().join("state.json");
        let (manifests, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let registry = Arc::new(Mutex::new(Registry::new(Arc::new(manifests), &state)));
        for (id, title) in [("build", "Build frontend"), ("review", "Review API")] {
            let cwd = directory.path().join(id);
            std::fs::create_dir(&cwd).unwrap();
            let record=serde_json::from_value(serde_json::json!({"id":id,"kind":diri_proto::AgentKind::SHELL,"cwd":cwd,"projectID":"fixture","title":title,"titleSource":diri_proto::TitleSource::UserRename,"status":diri_proto::SessionStatus::Idle,"resumability":diri_proto::Resumability::Live,"createdAt":0,"updatedAt":0,"pinned":false})).unwrap();
            registry
                .lock()
                .unwrap()
                .spawn(
                    SessionSpec {
                        id: id.into(),
                        pty: PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script.into()], &cwd)
                            .size(120, 40),
                        manifest_id: "shell".into(),
                        authority: Authority::ProcessOnly,
                        logs_dir: directory.path().join("logs"),
                        holder: None,
                        remote: None,
                        defer_launch: false,
                    },
                    record,
                )
                .unwrap();
        }
        let initial_pids = ["build", "review"]
            .iter()
            .map(|id| registry.lock().unwrap().get(id).unwrap().child_pid())
            .collect();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !["build", "review"]
            .iter()
            .all(|id| directory.path().join(id).join("ready").exists())
        {
            assert!(Instant::now() < deadline, "shell readiness");
            std::thread::sleep(Duration::from_millis(2));
        }
        let catalog = WorkspaceStore::new(&state);
        let sessions = HashSet::from([SessionId::new("build"), SessionId::new("review")]);
        let mut revision = 0;
        let mut apply = |mutation| {
            let snapshot = catalog
                .apply(
                    WorkspaceMutationParams {
                        expected_revision: revision,
                        mutation,
                    },
                    &sessions,
                )
                .unwrap();
            revision = snapshot.revision;
            snapshot
        };
        let snapshot = apply(WorkspaceMutation::CreateWorkspace {
            name: "Local verification".into(),
        });
        let workspace = snapshot.workspaces[0].id.clone();
        let snapshot = apply(WorkspaceMutation::CreateTab {
            select: true,
            workspace_id: workspace.clone(),
            session_id: SessionId::new("build"),
            title: Some("Build and review".into()),
        });
        let tab = &snapshot.workspaces[0].tabs[0];
        apply(WorkspaceMutation::SplitPane {
            tab_id: tab.id.clone(),
            target: tab.focused_pane.clone(),
            session_id: SessionId::new("review"),
            edge: DockEdge::Right,
        });
        let control = Arc::new(ControlServer::new(registry.clone(), &path));
        let listener = control.bind().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let connections = sockets.clone();
        let server = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        connections
                            .lock()
                            .unwrap()
                            .push(stream.try_clone().unwrap());
                        if stopped.load(Ordering::Acquire) {
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            break;
                        }
                        let control = control.clone();
                        workers.push(std::thread::spawn(move || {
                            if let Err(error) = control.serve(stream) {
                                eprintln!("fixture connection: {error}");
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        let resources = ServerResources {
            registry: registry.clone(),
            stop,
            sockets,
            server: Some(server),
        };
        let tokio = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        );
        let store = {
            let _entered = tokio.enter();
            Arc::new(
                crate::store::StoreRuntime::start(
                    Arc::new(diri_client::DaemonClient::with_socket_path(&path)),
                    directory.path().join("prefs.json"),
                )
                .unwrap(),
            )
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !store.store.read().unwrap().workspace_catalog().can_edit() {
            assert!(
                Instant::now() < deadline,
                "catalog hydration: {:?}, {:?}",
                store.store.read().unwrap().daemon_state(),
                store.store.read().unwrap().workspace_catalog().status()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.active_workspace = Some(workspace.clone());
                prefs.sidebar_visible = true;
                prefs.tab_orientation = crate::store::TabOrientation::Vertical;
                prefs.terminal_theme = "dirijor-light".into();
            })
            .unwrap();
        let services = Arc::new(crate::AppServices {
            store,
            usage_tx: tokio::sync::watch::channel(Default::default()).0,
            usage_limits_refresh: tokio::sync::mpsc::channel(1).0,
            updates: crate::updates::inert(),
            dev_build: None,
            daemon_startup: None,
            tokio,
        });
        Self {
            services,
            workspace,
            directory,
            registry,
            initial_pids,
            _resources: resources,
        }
    }
    pub(crate) fn continuous_output(&self) -> OutputDriver {
        let stop = Arc::new(AtomicBool::new(false));
        let for_thread = stop.clone();
        let registry = self.registry.clone();
        let ticks = Arc::new(AtomicU64::new(0));
        let thread_ticks = ticks.clone();
        let thread = std::thread::spawn(move || {
            let started = Instant::now();
            // Bound disposable evidence workloads even if a test event loop stalls.
            let deadline = started + Duration::from_secs(30);
            while !for_thread.load(Ordering::Acquire) && Instant::now() < deadline {
                let tick = thread_ticks.fetch_add(1, Ordering::Relaxed);
                let input = format!("update-{tick}\n");
                {
                    let registry = registry.lock().unwrap();
                    for id in ["build", "review"] {
                        registry
                            .get(id)
                            .unwrap()
                            .write_input(input.as_bytes())
                            .unwrap();
                    }
                }
                let next = started + Duration::from_millis(20 * (tick + 1));
                if let Some(delay) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(delay);
                }
            }
        });
        OutputDriver {
            ticks,
            stop,
            thread: Some(thread),
        }
    }

    pub(crate) fn held_spawn(&self) -> HeldLaunch {
        use std::os::unix::fs::PermissionsExt;
        let repo = self.directory.path().join("held-repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let entered = self.directory.path().join("held-entered");
        let release = self.directory.path().join("held-release");
        let quote = |path: &std::path::Path| {
            format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
        };
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ntouch {}\nwhile [ ! -f {} ]; do sleep 0.01; done\n",
                quote(&entered),
                quote(&release)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let params = self.services.store.store.read().unwrap().spawn_params(
            diri_proto::AgentKind::SHELL,
            crate::store::SpawnOptions {
                cwd: Some(repo.to_string_lossy().into_owned()),
                worktree: Some(crate::store::WorktreeSpawn {
                    create: true,
                    branch: Some("held-spawn".into()),
                }),
                ..Default::default()
            },
        );
        HeldLaunch {
            params,
            entered,
            release,
        }
    }

    pub(crate) fn verify_geometry(&self, expected: &[(SessionId, u16, u16)]) {
        for (id, cols, rows) in expected {
            assert_eq!(
                self.registry
                    .lock()
                    .unwrap()
                    .get(&id.0)
                    .unwrap()
                    .screen_size(),
                (usize::from(*cols), usize::from(*rows)),
                "Engine terminal geometry matches the actual PTY"
            );
            let path = self.directory.path().join(&id.0).join("geometry");
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                // The live shell truncates/replaces this fixture probe at 50Hz.
                // Read a complete matching observation, not the intermediate file.
                let actual = std::fs::read_to_string(&path).unwrap_or_default();
                let parsed = actual
                    .split_whitespace()
                    .map(str::parse::<u16>)
                    .collect::<Result<Vec<_>, _>>();
                if parsed.as_ref().is_ok_and(|size| size == &[*rows, *cols]) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "actual PTY geometry for {}: expected {} {}, got {:?}",
                    id.0,
                    rows,
                    cols,
                    actual
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.verify_process_identity();
    }
    pub(crate) fn verify_process_identity(&self) {
        let pids = ["build", "review"]
            .iter()
            .map(|id| self.registry.lock().unwrap().get(id).unwrap().child_pid())
            .collect::<Vec<_>>();
        assert_eq!(
            pids, self.initial_pids,
            "view layout preserves process identity"
        );
    }
}
impl Drop for ServerResources {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        let ids: Vec<_> = self
            .registry
            .lock()
            .unwrap()
            .records()
            .into_iter()
            .map(|record| record.id)
            .collect();
        for id in ids {
            let _ = self
                .registry
                .lock()
                .unwrap()
                .terminate(&id.0, Duration::ZERO);
        }
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

pub(crate) struct HeldLaunch {
    pub params: diri_proto::SessionSpawnParams,
    pub entered: std::path::PathBuf,
    release: std::path::PathBuf,
}
impl HeldLaunch {
    pub fn release(&self) {
        std::fs::write(&self.release, "release").unwrap();
    }
}
impl Drop for HeldLaunch {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, "release");
    }
}

pub(crate) struct OutputDriver {
    ticks: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl OutputDriver {
    pub(crate) fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }
}
impl Drop for OutputDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
