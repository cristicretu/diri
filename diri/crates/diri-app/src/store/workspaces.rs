//! Revision-gated cache of the Engine's saved layout catalog. The UI never
//! persists a second copy or retries a rejected edit against a new revision.
//! Nor does it resend an edit the Engine already rejected on its merits:
//! navigation can request the same placement on every click, and a full tab
//! limit rejects it identically until the layout or the sessions change.
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
    in_flight: Option<AttemptedEdit>,
    rejected: Option<RejectedEdit>,
    pub error: Option<String>,
    pub created_workspace: Option<(u64, diri_proto::workspace::WorkspaceId)>,
    pub create_request_id: u64,
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
            in_flight: None,
            rejected: None,
            error: None,
            created_workspace: None,
            create_request_id: 0,
            creating: false,
        }
    }
}

/// The Engine decides an edit from its saved layout (named by the revision)
/// and, for placements, which sessions exist. Only those inputs can change an
/// identical edit's outcome.
#[derive(Clone, Debug, PartialEq)]
struct AttemptedEdit {
    params: WorkspaceMutationParams,
    sessions: u64,
}

struct RejectedEdit {
    edit: AttemptedEdit,
    message: String,
}

/// Order-independent identity of the session inventory.
fn session_fingerprint(sessions: &HashMap<SessionId, Arc<SessionRecord>>) -> u64 {
    use std::hash::{Hash, Hasher};
    sessions.keys().fold(sessions.len() as u64, |acc, id| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        acc ^ hasher.finish()
    })
}

/// A conflict means the revision moved; anything else the Engine returned was
/// decided against the layout this edit named and repeats until it changes.
fn deterministic_rejection(error: &ClientError) -> bool {
    matches!(error, ClientError::Control(error) if error.code != "workspace_revision_conflict")
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
    /// A fixture's store that reads as connected, without the Connecting toast.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn mark_connected_for_test(&mut self) {
        self.daemon_state = DaemonState::Connected;
    }

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

    #[cfg(test)]
    pub(crate) fn finish_workspace_refresh_for_test(&mut self, snapshot: WorkspaceSnapshot) {
        self.finish_workspace_request(self.workspaces.generation, false, Ok(snapshot));
    }

    #[cfg(test)]
    pub(crate) fn reject_workspace_edit_for_test(&mut self, code: &str) {
        let error = diri_proto::control::ControlError::new(code, "rejected");
        self.finish_workspace_request(
            self.workspaces.generation,
            true,
            Err(ClientError::Control(error)),
        );
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
        catalog.in_flight = None;
        // Another Engine may decide differently.
        catalog.rejected = None;
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

    fn attempted_edit(&self, mutation: WorkspaceMutation) -> Option<AttemptedEdit> {
        Some(AttemptedEdit {
            params: WorkspaceMutationParams {
                expected_revision: self.workspaces.snapshot.as_ref()?.revision,
                mutation,
            },
            sessions: session_fingerprint(&self.sessions),
        })
    }

    /// The Engine already rejected this exact edit against the visible layout
    /// and the current sessions; sending it again would fail the same way.
    pub fn workspace_edit_rejected(&self, mutation: &WorkspaceMutation) -> Option<&str> {
        let rejected = self.workspaces.rejected.as_ref()?;
        (rejected.edit.params.mutation == *mutation
            && self.attempted_edit(mutation.clone()).as_ref() == Some(&rejected.edit))
        .then_some(rejected.message.as_str())
    }

    pub fn edit_workspace(&mut self, mutation: WorkspaceMutation) -> bool {
        if !self.workspaces.can_edit() || self.workspace_edit_rejected(&mutation).is_some() {
            return false;
        }
        let Some(edit) = self.attempted_edit(mutation) else {
            return false;
        };
        let params = edit.params.clone();
        let catalog = &mut self.workspaces;
        catalog.in_flight = Some(edit);
        catalog.editing = true;
        catalog.creating = matches!(&params.mutation, WorkspaceMutation::CreateWorkspace { .. });
        if catalog.creating {
            catalog.create_request_id = catalog.create_request_id.wrapping_add(1);
        }
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
        let attempted = if mutation {
            catalog.editing = false;
            catalog.in_flight.take()
        } else {
            None
        };
        if !mutation {
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
                        .map(|workspace| (catalog.create_request_id, workspace.id.clone()));
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
                    if deterministic_rejection(&error) {
                        catalog.rejected = attempted.map(|edit| RejectedEdit {
                            edit,
                            message: error.to_string(),
                        });
                    }
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
    fn creation_results_carry_the_admitted_request_identity() {
        let (mut store, _, generation) = connected();
        store.finish_workspace_request(generation, false, Ok(snapshot(0)));
        let mut state = snapshot(1);
        assert!(store.edit_workspace(WorkspaceMutation::CreateWorkspace {
            name: "First".into()
        }));
        let first = store.workspace_catalog().create_request_id;
        state
            .workspaces
            .push(diri_proto::workspace::WorkspaceRecord {
                project_id: None,
                id: diri_proto::workspace::WorkspaceId::new("first"),
                name: "First".into(),
                tabs: vec![],
                selected_tab: None,
            });
        store.finish_workspace_request(generation, true, Ok(state.clone()));
        assert_eq!(
            store
                .workspace_catalog()
                .created_workspace
                .as_ref()
                .map(|(request, id)| (*request, id.0.as_str())),
            Some((first, "first"))
        );
        assert!(store.edit_workspace(WorkspaceMutation::CreateWorkspace {
            name: "Second".into()
        }));
        let second = store.workspace_catalog().create_request_id;
        assert_ne!(first, second);
        assert!(store.workspace_catalog().created_workspace.is_none());
        state.revision += 1;
        state
            .workspaces
            .push(diri_proto::workspace::WorkspaceRecord {
                project_id: None,
                id: diri_proto::workspace::WorkspaceId::new("second"),
                name: "Second".into(),
                tabs: vec![],
                selected_tab: None,
            });
        store.finish_workspace_request(generation, true, Ok(state));
        assert_eq!(
            store
                .workspace_catalog()
                .created_workspace
                .as_ref()
                .map(|(request, id)| (*request, id.0.as_str())),
            Some((second, "second"))
        );
    }

    fn limit_reached() -> ClientError {
        ClientError::Control(diri_proto::control::ControlError::new(
            "workspace_limit_reached",
            "workspace or tab count exceeds the limit",
        ))
    }

    /// Submits `mutation` and answers it with `result`, returning whether an
    /// RPC went out at all.
    fn submit(
        store: &mut SessionStore,
        effects: &mut mpsc::UnboundedReceiver<StoreEffect>,
        generation: u64,
        mutation: &WorkspaceMutation,
        result: Result<WorkspaceSnapshot, ClientError>,
    ) -> bool {
        if !store.edit_workspace(mutation.clone()) {
            return false;
        }
        assert!(matches!(
            effects.try_recv(),
            Ok(StoreEffect::MutateWorkspace { .. })
        ));
        store.finish_workspace_request(generation, true, result);
        // Settle the reload every failed edit asks for.
        let revision = store.workspace_catalog().snapshot().unwrap().revision;
        while let Ok(effect) = effects.try_recv() {
            if matches!(effect, StoreEffect::RefreshWorkspaces { .. }) {
                store.finish_workspace_request(generation, false, Ok(snapshot(revision)));
            }
        }
        true
    }

    /// A tab limit full of placements rejected every agent activation, and
    /// navigation asked again on every click: 1,727 identical rejections and
    /// 929 toasts on one install. The Engine's verdict on the same layout and
    /// sessions is final; only a new revision or session inventory can change it.
    #[test]
    fn a_rejected_edit_is_not_resent_until_the_layout_or_sessions_change() {
        let (mut store, mut effects, generation) = connected();
        store.finish_workspace_request(generation, false, Ok(snapshot(7)));
        let open = WorkspaceMutation::OpenProjectAgent {
            session_id: SessionId::new("agent"),
            preferred_workspace: None,
        };
        let mut rpcs = 0;
        for _ in 0..50 {
            rpcs += usize::from(submit(
                &mut store,
                &mut effects,
                generation,
                &open,
                Err(limit_reached()),
            ));
        }
        assert_eq!(rpcs, 1, "one rejection, not one per activation");
        assert!(store.workspace_edit_rejected(&open).is_some());
        assert!(store.workspace_catalog().can_edit());

        // A different edit, such as closing a tab to make room, still goes out.
        let close = WorkspaceMutation::RemoveTab {
            tab_id: diri_proto::workspace::TabId::new("stale"),
        };
        assert!(store.workspace_edit_rejected(&close).is_none());

        // A session ending lets the Engine reclaim its tab: ask again.
        store.upsert_session(super::super::tests::session("gone", "p", 1.0));
        while effects.try_recv().is_ok() {}
        assert!(submit(
            &mut store,
            &mut effects,
            generation,
            &open,
            Err(limit_reached()),
        ));
        assert!(!submit(
            &mut store,
            &mut effects,
            generation,
            &open,
            Err(limit_reached())
        ));

        // So does any committed layout change.
        store.workspace_announced(8);
        let StoreEffect::RefreshWorkspaces { .. } = effects.try_recv().unwrap() else {
            panic!("refresh")
        };
        store.finish_workspace_request(generation, false, Ok(snapshot(8)));
        assert!(store.workspace_edit_rejected(&open).is_none());
        assert!(submit(
            &mut store,
            &mut effects,
            generation,
            &open,
            Ok(snapshot(9))
        ));
    }

    #[test]
    fn conflicts_and_uncertain_failures_are_not_remembered_as_rejections() {
        let (mut store, mut effects, generation) = connected();
        store.finish_workspace_request(generation, false, Ok(snapshot(3)));
        let open = WorkspaceMutation::OpenProjectAgent {
            session_id: SessionId::new("agent"),
            preferred_workspace: None,
        };
        let conflict = ClientError::Control(diri_proto::control::ControlError::new(
            "workspace_revision_conflict",
            "expected revision 3, current revision 4",
        ));
        assert!(submit(
            &mut store,
            &mut effects,
            generation,
            &open,
            Err(conflict)
        ));
        assert!(store.workspace_edit_rejected(&open).is_none());
        let io = ClientError::Io("directory sync failed".into());
        assert!(submit(&mut store, &mut effects, generation, &open, Err(io)));
        assert!(store.workspace_edit_rejected(&open).is_none());
        // A new Engine connection decides afresh.
        assert!(submit(
            &mut store,
            &mut effects,
            generation,
            &open,
            Err(limit_reached())
        ));
        assert!(store.workspace_edit_rejected(&open).is_some());
        store.workspace_connection_changed(false);
        store.workspace_connection_changed(true);
        assert!(store.workspace_edit_rejected(&open).is_none());
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
