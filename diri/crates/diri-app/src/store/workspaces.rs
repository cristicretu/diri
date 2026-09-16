//! Revision-gated cache of the Engine's saved layout catalog. The UI never
//! persists a second copy or retries a rejected edit against a new revision.
use super::*;
use diri_proto::workspace::{
    WORKSPACE_SCHEMA_VERSION, WorkspaceMutation, WorkspaceMutationParams, WorkspaceSnapshot,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceCatalogStatus {
    Loading,
    Ready,
    Unavailable(String),
}

pub struct WorkspaceCatalog {
    snapshot: Option<Arc<WorkspaceSnapshot>>,
    status: WorkspaceCatalogStatus,
    generation: u64,
    connected: bool,
    hydrated: bool,
    refreshing: bool,
    refresh_again: bool,
    editing: bool,
    announced_revision: u64,
    pub error: Option<String>,
    pub created_workspace: Option<diri_proto::workspace::WorkspaceId>,
    creating: bool,
}

impl Default for WorkspaceCatalog {
    fn default() -> Self {
        Self {
            snapshot: None,
            status: WorkspaceCatalogStatus::Loading,
            generation: 0,
            connected: false,
            hydrated: false,
            refreshing: false,
            refresh_again: false,
            editing: false,
            announced_revision: 0,
            error: None,
            created_workspace: None,
            creating: false,
        }
    }
}

impl WorkspaceCatalog {
    pub fn snapshot(&self) -> Option<&WorkspaceSnapshot> {
        self.snapshot.as_deref()
    }
    pub fn status(&self) -> &WorkspaceCatalogStatus {
        &self.status
    }
    pub fn can_edit(&self) -> bool {
        self.connected
            && !self.editing
            && !self.refreshing
            && self.status == WorkspaceCatalogStatus::Ready
    }
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) fn seed_workspace_snapshot_for_test(&mut self, snapshot: WorkspaceSnapshot) {
        self.daemon_state = DaemonState::Connected;
        self.workspaces.snapshot = Some(Arc::new(snapshot));
        self.workspaces.status = WorkspaceCatalogStatus::Ready;
        self.workspaces.connected = true;
        self.workspaces.hydrated = true;
    }

    #[cfg(test)]
    pub(crate) fn finish_workspace_edit_for_test(&mut self, snapshot: WorkspaceSnapshot) {
        self.finish_workspace_request(self.workspaces.generation, true, Ok(snapshot));
    }

    pub fn workspace_catalog(&self) -> &WorkspaceCatalog {
        &self.workspaces
    }

    pub(super) fn workspace_request_is_current(&self, generation: u64) -> bool {
        self.workspaces.connected && self.workspaces.generation == generation
    }

    pub(super) fn workspace_connection_changed(&mut self, connected: bool) {
        let catalog = &mut self.workspaces;
        catalog.generation = catalog.generation.wrapping_add(1);
        catalog.connected = connected;
        catalog.hydrated = false;
        catalog.refreshing = false;
        catalog.refresh_again = false;
        catalog.editing = false;
        catalog.creating = false;
        catalog.created_workspace = None;
        catalog.announced_revision = 0;
        catalog.status = if connected {
            WorkspaceCatalogStatus::Loading
        } else {
            WorkspaceCatalogStatus::Unavailable("Connect to Diri to load workspaces.".into())
        };
        // Cached content remains paintable but cannot authorize an edit until
        // this connection returns its own authoritative snapshot.
        if connected {
            self.refresh_workspaces();
        }
    }

    pub fn refresh_workspaces(&mut self) {
        let catalog = &mut self.workspaces;
        if !catalog.connected {
            return;
        }
        if catalog.refreshing || catalog.editing {
            catalog.refresh_again = true;
            return;
        }
        catalog.refreshing = true;
        catalog.status = WorkspaceCatalogStatus::Loading;
        let generation = catalog.generation;
        self.emit(StoreEffect::RefreshWorkspaces { generation });
    }

    pub(super) fn workspace_announced(&mut self, revision: u64) {
        let catalog = &mut self.workspaces;
        if revision <= catalog.announced_revision {
            return;
        }
        catalog.announced_revision = revision;
        if catalog
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| revision > snapshot.revision)
        {
            self.refresh_workspaces();
        }
    }

    pub fn edit_workspace(&mut self, mutation: WorkspaceMutation) -> bool {
        let catalog = &mut self.workspaces;
        if !catalog.can_edit() {
            return false;
        }
        let Some(snapshot) = &catalog.snapshot else {
            return false;
        };
        let params = WorkspaceMutationParams {
            expected_revision: snapshot.revision,
            mutation,
        };
        catalog.editing = true;
        catalog.creating = matches!(&params.mutation, WorkspaceMutation::CreateWorkspace { .. });
        catalog.created_workspace = None;
        catalog.error = None;
        let generation = catalog.generation;
        self.emit(StoreEffect::MutateWorkspace { generation, params });
        self.emit(StoreEffect::UiChanged);
        true
    }

    pub(super) fn finish_workspace_request(
        &mut self,
        generation: u64,
        mutation: bool,
        result: Result<WorkspaceSnapshot, ClientError>,
    ) {
        let catalog = &mut self.workspaces;
        if generation != catalog.generation || !catalog.connected {
            return;
        }
        if mutation {
            catalog.editing = false;
        } else {
            catalog.refreshing = false;
        }
        match result {
            Ok(snapshot) if snapshot.schema_version != WORKSPACE_SCHEMA_VERSION => {
                catalog.status = WorkspaceCatalogStatus::Unavailable(
                    "This workspace format is not supported by this app.".into(),
                );
                catalog.refresh_again = false;
            }
            Ok(snapshot) => {
                if mutation && catalog.creating {
                    catalog.created_workspace = snapshot
                        .workspaces
                        .last()
                        .map(|workspace| workspace.id.clone());
                }
                // Within one connection a delayed response cannot roll back
                // newer event/mutation state. Reconnect hydration is allowed
                // to replace the read-only cache from a prior Engine.
                let accepts = !catalog.hydrated
                    || catalog
                        .snapshot
                        .as_ref()
                        .is_none_or(|current| snapshot.revision >= current.revision);
                if accepts {
                    catalog.snapshot = Some(Arc::new(snapshot));
                    catalog.hydrated = true;
                }
                catalog.status = WorkspaceCatalogStatus::Ready;
                if catalog
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.revision < catalog.announced_revision)
                    && !catalog.refresh_again
                {
                    catalog.status = WorkspaceCatalogStatus::Unavailable(
                        "Workspace changes are still synchronizing. Refresh to try again.".into(),
                    );
                }
            }
            Err(error) => {
                if mutation {
                    catalog.error = Some(format!(
                        "Workspace change was not confirmed: {error}. Reloading the current layout."
                    ));
                    // A post-rename failure may have committed. Refetch, never
                    // replay the candidate against a different revision.
                    catalog.refresh_again = true;
                } else {
                    catalog.status = WorkspaceCatalogStatus::Unavailable(error.to_string());
                    catalog.refresh_again = false;
                }
            }
        }
        if std::mem::take(&mut catalog.refresh_again) {
            self.refresh_workspaces();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(revision: u64) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            revision,
            ..Default::default()
        }
    }
    fn connected() -> (SessionStore, mpsc::UnboundedReceiver<StoreEffect>, u64) {
        let (mut store, mut effects) = SessionStore::headless(Prefs::default());
        store.workspace_connection_changed(true);
        let StoreEffect::RefreshWorkspaces { generation } = effects.try_recv().unwrap() else {
            panic!("initial refresh")
        };
        (store, effects, generation)
    }

    #[test]
    fn loading_and_failure_are_distinct_from_an_empty_catalog() {
        let (mut store, _, generation) = connected();
        assert!(store.workspace_catalog().snapshot().is_none());
        assert!(!store.workspace_catalog().can_edit());
        store.finish_workspace_request(
            generation,
            false,
            Err(ClientError::Io("unreadable state".into())),
        );
        assert!(matches!(
            store.workspace_catalog().status(),
            WorkspaceCatalogStatus::Unavailable(_)
        ));
        store.refresh_workspaces();
        store.finish_workspace_request(generation, false, Ok(snapshot(0)));
        assert!(store.workspace_catalog().can_edit());
        assert!(
            store
                .workspace_catalog()
                .snapshot()
                .unwrap()
                .workspaces
                .is_empty()
        );
    }

    #[test]
    fn event_bursts_coalesce_and_refresh_again_after_the_inflight_response() {
        let (mut store, mut effects, generation) = connected();
        for revision in 1..100 {
            store.workspace_announced(revision);
        }
        assert!(
            effects.try_recv().is_err(),
            "one request is already in flight"
        );
        store.finish_workspace_request(generation, false, Ok(snapshot(1)));
        assert!(matches!(
            effects.try_recv(),
            Ok(StoreEffect::RefreshWorkspaces { .. })
        ));
        assert!(effects.try_recv().is_err());
        store.finish_workspace_request(generation, false, Ok(snapshot(99)));
        assert_eq!(store.workspace_catalog().snapshot().unwrap().revision, 99);
        assert!(store.workspace_catalog().can_edit());
        store.workspace_announced(99);
        assert!(
            effects.try_recv().is_err(),
            "duplicate event does not fetch again"
        );
    }

    #[test]
    fn delayed_responses_cannot_roll_back_or_cross_connection_generations() {
        let (mut store, _, generation) = connected();
        store.finish_workspace_request(generation, false, Ok(snapshot(20)));
        store.refresh_workspaces();
        store.finish_workspace_request(generation, false, Ok(snapshot(10)));
        assert_eq!(store.workspace_catalog().snapshot().unwrap().revision, 20);
        store.workspace_connection_changed(false);
        assert!(!store.workspace_request_is_current(generation));
        store.workspace_connection_changed(true);
        let current = store.workspaces.generation;
        store.finish_workspace_request(generation, true, Ok(snapshot(30)));
        assert!(!store.workspace_catalog().can_edit());
        store.finish_workspace_request(current, false, Ok(snapshot(2)));
        assert_eq!(
            store.workspace_catalog().snapshot().unwrap().revision,
            2,
            "new Engine replaces a read-only prior cache"
        );
    }

    #[test]
    fn mutations_use_the_visible_revision_and_refetch_uncertain_outcomes_without_replay() {
        let (mut store, mut effects, generation) = connected();
        store.finish_workspace_request(generation, false, Ok(snapshot(4)));
        let mutation = WorkspaceMutation::CreateWorkspace {
            name: "Release".into(),
        };
        assert!(store.edit_workspace(mutation.clone()));
        assert!(
            !store.edit_workspace(mutation.clone()),
            "one in-flight edit per cache"
        );
        let StoreEffect::MutateWorkspace { params, .. } = effects.try_recv().unwrap() else {
            panic!("mutation")
        };
        assert_eq!(params.expected_revision, 4);
        assert_eq!(params.mutation, mutation);
        effects.try_recv().unwrap(); // UI publication
        store.finish_workspace_request(
            generation,
            true,
            Err(ClientError::Io("directory sync failed".into())),
        );
        assert!(store.workspace_catalog().error.is_some());
        assert!(!store.workspace_catalog().can_edit());
        assert!(matches!(
            effects.try_recv(),
            Ok(StoreEffect::RefreshWorkspaces { .. })
        ));
        assert!(effects.try_recv().is_err(), "the edit is never replayed");
        store.finish_workspace_request(generation, false, Ok(snapshot(5)));
        assert!(store.workspace_catalog().can_edit());
        assert_eq!(store.workspace_catalog().snapshot().unwrap().revision, 5);
    }

    #[test]
    fn unknown_schema_and_unavailable_announced_revision_fail_closed_without_fetch_loops() {
        let (mut store, mut effects, generation) = connected();
        store.finish_workspace_request(
            generation,
            false,
            Ok(WorkspaceSnapshot {
                schema_version: 2,
                ..Default::default()
            }),
        );
        assert!(!store.workspace_catalog().can_edit());
        store.refresh_workspaces();
        effects.try_recv().unwrap();
        store.workspace_announced(8);
        store.finish_workspace_request(generation, false, Ok(snapshot(7)));
        effects.try_recv().unwrap();
        store.finish_workspace_request(generation, false, Ok(snapshot(7)));
        assert!(matches!(
            store.workspace_catalog().status(),
            WorkspaceCatalogStatus::Unavailable(_)
        ));
        assert!(effects.try_recv().is_err());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn store_runtime_edits_and_observes_the_same_durable_engine_catalog_as_other_clients() {
        use diri_engine::{ControlServer, ManifestEngine, Registry};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let state_path = temp.path().join("state.json");
        let (manifests, _) =
            ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
        let registry = Arc::new(Mutex::new(Registry::new(Arc::new(manifests), &state_path)));
        let server = Arc::new(ControlServer::new(registry.clone(), &path));
        let listener = server.bind().unwrap();
        let worker = std::thread::spawn(move || {
            let mut connections = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let server = server.clone();
                connections.push(std::thread::spawn(move || server.serve(stream)));
            }
            for connection in connections {
                let _ = connection.join().unwrap();
            }
        });
        let client = Arc::new(DaemonClient::with_socket_path(&path));
        let runtime = StoreRuntime::start(client, temp.path().join("preferences.json")).unwrap();
        async fn wait_revision(runtime: &StoreRuntime, revision: u64) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let ready = {
                        let store = runtime.store.read().unwrap();
                        store.workspace_catalog().can_edit()
                            && store
                                .workspace_catalog()
                                .snapshot()
                                .is_some_and(|snapshot| snapshot.revision == revision)
                    };
                    if ready {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
        wait_revision(&runtime, 0).await;
        assert!(runtime.store.write().unwrap().edit_workspace(
            WorkspaceMutation::CreateWorkspace {
                name: "Release".into()
            }
        ));
        wait_revision(&runtime, 1).await;
        let external = DaemonClient::with_socket_path(&path);
        external.connect();
        external
            .wait_until_connected(Duration::from_secs(5))
            .await
            .unwrap();
        let external_snapshot = external.workspaces().await.unwrap();
        assert_eq!(external_snapshot.workspaces[0].name, "Release");
        let id = external_snapshot.workspaces[0].id.clone();
        external
            .mutate_workspace(&WorkspaceMutationParams {
                expected_revision: 1,
                mutation: WorkspaceMutation::RenameWorkspace {
                    workspace_id: id.clone(),
                    name: "Across machines".into(),
                },
            })
            .await
            .unwrap();
        // No manual refresh: the explicitly subscribed workspace event drives
        // the GUI cache through the production StoreRuntime.
        wait_revision(&runtime, 2).await;
        let persisted = diri_engine::workspace::WorkspaceStore::new(&state_path)
            .snapshot()
            .unwrap();
        assert_eq!(persisted.workspaces[0].id, id);
        assert_eq!(persisted.workspaces[0].name, "Across machines");
        assert_eq!(
            runtime.store.read().unwrap().workspace_catalog().snapshot(),
            Some(&persisted)
        );
        assert!(
            registry.lock().unwrap().records().is_empty(),
            "organization edits create no PTYs"
        );
        external.shutdown().await;
        runtime.shutdown().await;
        tokio::task::spawn_blocking(move || worker.join().unwrap())
            .await
            .unwrap();
    }
}
