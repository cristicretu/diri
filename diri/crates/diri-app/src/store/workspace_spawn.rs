//! Request-scoped creation and placement. The runtime owns admitted work, while
//! windows only observe receipts. A failed placement never repeats a spawn.
use super::*;
use diri_proto::workspace::{
    LayoutNode, TabId, WorkspaceId, WorkspaceMutation, WorkspaceMutationParams,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_ACTIVE: usize = 8;
const MAX_RECEIPTS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SpawnOwner(u64);

impl Default for SpawnOwner {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSpawnTarget {
    pub owner: SpawnOwner,
    pub workspace: WorkspaceId,
    pub selected_tab: Option<TabId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSpawnTarget {
    pub owner: SpawnOwner,
    pub selected_session: Option<SessionId>,
    pub navigation_revision: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnDestination {
    Workspace(WorkspaceSpawnTarget),
    Window(WindowSpawnTarget),
}
impl From<WorkspaceSpawnTarget> for SpawnDestination {
    fn from(target: WorkspaceSpawnTarget) -> Self {
        Self::Workspace(target)
    }
}
impl From<WindowSpawnTarget> for SpawnDestination {
    fn from(target: WindowSpawnTarget) -> Self {
        Self::Window(target)
    }
}
impl SpawnDestination {
    pub fn workspace(&self) -> Option<&WorkspaceId> {
        match self {
            Self::Workspace(target) => Some(&target.workspace),
            Self::Window(_) => None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceSpawnState {
    Creating,
    Created {
        session: SessionId,
    },
    Placing(SessionId),
    Placed {
        session: SessionId,
        tab: TabId,
    },
    Unplaced {
        session: SessionId,
        detail: String,
    },
    /// The spawn response itself was not confirmed; there is no safe replay.
    Unconfirmed(String),
}

impl WorkspaceSpawnState {
    pub fn pending(&self) -> bool {
        matches!(self, Self::Creating | Self::Placing(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSpawnReceipt {
    pub id: u64,
    pub target: SpawnDestination,
    pub state: WorkspaceSpawnState,
}

#[derive(Default)]
pub(super) struct WorkspaceSpawns {
    next: u64,
    receipts: VecDeque<WorkspaceSpawnReceipt>,
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) fn seed_workspace_spawn_for_test(
        &mut self,
        target: WorkspaceSpawnTarget,
        state: WorkspaceSpawnState,
    ) -> u64 {
        self.workspace_spawns.next += 1;
        let id = self.workspace_spawns.next;
        self.workspace_spawns
            .receipts
            .push_back(WorkspaceSpawnReceipt {
                id,
                target: target.into(),
                state,
            });
        id
    }

    pub fn workspace_spawn_source(&self, target: &WorkspaceSpawnTarget) -> Option<&SessionRecord> {
        let workspace = self
            .workspace_catalog()
            .snapshot()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == target.workspace)?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| Some(&tab.id) == target.selected_tab.as_ref())?;
        fn focused<'a>(
            node: &'a LayoutNode,
            pane: &diri_proto::workspace::PaneId,
        ) -> Option<&'a SessionId> {
            match node {
                LayoutNode::Pane { id, session_id } => (id == pane).then_some(session_id),
                LayoutNode::Split { first, second, .. } => {
                    focused(first, pane).or_else(|| focused(second, pane))
                }
            }
        }
        self.sessions
            .get(focused(&tab.layout, &tab.focused_pane)?)
            .map(Arc::as_ref)
    }

    pub fn workspace_spawn_receipts(&self) -> impl Iterator<Item = &WorkspaceSpawnReceipt> {
        self.workspace_spawns.receipts.iter()
    }

    pub fn request_workspace_spawn(
        &mut self,
        target: impl Into<SpawnDestination>,
        params: SessionSpawnParams,
    ) -> Option<u64> {
        let target = target.into();
        let pending = self
            .workspace_spawns
            .receipts
            .iter()
            .filter(|r| r.state.pending())
            .count();
        if pending >= MAX_ACTIVE {
            self.reject_workspace_spawn(
                "Eight launches are already running. Wait for one to finish.",
            );
            return None;
        }
        if self.workspace_spawns.receipts.len() == MAX_RECEIPTS {
            if let Some(index) = self.workspace_spawns.receipts.iter().position(|r| {
                matches!(
                    r.state,
                    WorkspaceSpawnState::Placed { .. } | WorkspaceSpawnState::Created { .. }
                )
            }) {
                self.workspace_spawns.receipts.remove(index);
            } else {
                self.reject_workspace_spawn(
                    "Review or dismiss a previous launch result before creating another session.",
                );
                return None;
            }
        }
        self.workspace_spawns.next = self.workspace_spawns.next.checked_add(1)?;
        let id = self.workspace_spawns.next;
        self.workspace_spawns
            .receipts
            .push_back(WorkspaceSpawnReceipt {
                id,
                target,
                state: WorkspaceSpawnState::Creating,
            });
        self.emit(StoreEffect::WorkspaceSpawn {
            id,
            params: Some(params),
        });
        self.emit(StoreEffect::UiChanged);
        Some(id)
    }

    fn reject_workspace_spawn(&mut self, detail: &str) {
        self.last_action_failure = Some(ActionFailure {
            title: "Session was not requested".into(),
            detail: detail.into(),
            retrying: false,
            retry: None,
        });
        self.emit(StoreEffect::UiChanged);
    }

    pub fn dismiss_workspace_spawn(&mut self, id: u64) {
        self.workspace_spawns
            .receipts
            .retain(|r| r.id != id || r.state.pending());
        self.emit(StoreEffect::UiChanged);
    }

    pub fn retry_workspace_placement(&mut self, id: u64) -> bool {
        if self
            .workspace_spawns
            .receipts
            .iter()
            .filter(|r| r.state.pending())
            .count()
            >= MAX_ACTIVE
        {
            return false;
        }
        let Some(receipt) = self
            .workspace_spawns
            .receipts
            .iter_mut()
            .find(|r| r.id == id)
        else {
            return false;
        };
        let WorkspaceSpawnState::Unplaced { session, .. } = &receipt.state else {
            return false;
        };
        receipt.state = WorkspaceSpawnState::Placing(session.clone());
        self.emit(StoreEffect::WorkspaceSpawn { id, params: None });
        self.emit(StoreEffect::UiChanged);
        true
    }

    pub(super) fn finish_workspace_spawn(&mut self, id: u64, mut state: WorkspaceSpawnState) {
        if let WorkspaceSpawnState::Unplaced { detail, .. }
        | WorkspaceSpawnState::Unconfirmed(detail) = &mut state
            && let Some((byte, _)) = detail.char_indices().nth(1024)
        {
            detail.truncate(byte);
            detail.push('…');
        }
        if let Some(receipt) = self
            .workspace_spawns
            .receipts
            .iter_mut()
            .find(|r| r.id == id)
        {
            receipt.state = state;
        }
        // Event delivery can precede the RPC response or recover after a gap.
        // Refresh through the normal catalog owner, never clear another edit.
        self.refresh_workspaces();
        self.emit(StoreEffect::UiChanged);
    }
}

fn contains_session(layout: &LayoutNode, session: &SessionId) -> bool {
    match layout {
        LayoutNode::Pane { session_id, .. } => session_id == session,
        LayoutNode::Split { first, second, .. } => {
            contains_session(first, session) || contains_session(second, session)
        }
    }
}

pub(super) async fn run(
    id: u64,
    params: Option<SessionSpawnParams>,
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    changes: broadcast::Sender<()>,
) {
    let Some(receipt) = store
        .read()
        .expect("store")
        .workspace_spawn_receipts()
        .find(|r| r.id == id)
        .cloned()
    else {
        return;
    };
    let session = if let Some(params) = params {
        match client.spawn(params).await {
            Ok(session) => {
                store
                    .write()
                    .expect("store")
                    .finish_workspace_spawn(id, WorkspaceSpawnState::Placing(session.clone()));
                let _ = changes.send(());
                session
            }
            Err(error) => {
                store.write().expect("store").finish_workspace_spawn(id, WorkspaceSpawnState::Unconfirmed(format!("Creation was not confirmed: {error}. Check All sessions before creating another session.")));
                let _ = changes.send(());
                return;
            }
        }
    } else if let WorkspaceSpawnState::Placing(session) = receipt.state {
        session
    } else {
        return;
    };
    let state = match &receipt.target {
        SpawnDestination::Window(_) => WorkspaceSpawnState::Created { session },
        SpawnDestination::Workspace(target) => match place(&client, target, &session).await {
            Ok(tab) => WorkspaceSpawnState::Placed { session, tab },
            Err(detail) => WorkspaceSpawnState::Unplaced { session, detail },
        },
    };
    store
        .write()
        .expect("store")
        .finish_workspace_spawn(id, state);
    let _ = changes.send(());
}

async fn place(
    client: &DaemonClient,
    target: &WorkspaceSpawnTarget,
    session: &SessionId,
) -> Result<TabId, String> {
    let snapshot = client.workspaces().await.map_err(|e| e.to_string())?;
    if snapshot.schema_version != diri_proto::workspace::WORKSPACE_SCHEMA_VERSION {
        return Err("This workspace format is not supported by this app.".into());
    }
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|w| w.id == target.workspace)
        .ok_or_else(|| {
            "The destination workspace was removed. The session remains in All sessions.".to_owned()
        })?;
    // An earlier acknowledgment may have been lost after durable commit. A
    // fresh read resolves that outcome before allowing another placement.
    if let Some(tab) = workspace
        .tabs
        .iter()
        .find(|tab| contains_session(&tab.layout, session))
    {
        return Ok(tab.id.clone());
    }
    let result = client.mutate_workspace(&WorkspaceMutationParams {
        expected_revision: snapshot.revision,
        mutation: WorkspaceMutation::CreateTab { select: workspace.selected_tab == target.selected_tab, workspace_id: target.workspace.clone(), session_id: session.clone(), title: None },
    }).await.map_err(|e| format!("Placement was not confirmed: {e}. The session remains available in All sessions. Retry placement to check the current layout."))?;
    result.workspaces.iter().find(|w| w.id == target.workspace).and_then(|w| w.tabs.iter().find(|tab| contains_session(&tab.layout, session))).map(|tab| tab.id.clone()).ok_or_else(|| "The session was created, but its placement was not returned. Check the current workspace before retrying placement.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(owner: SpawnOwner, workspace: &str) -> WorkspaceSpawnTarget {
        WorkspaceSpawnTarget {
            owner,
            workspace: WorkspaceId::new(workspace),
            selected_tab: Some(TabId::new("original")),
        }
    }
    fn params(store: &SessionStore) -> SessionSpawnParams {
        store.spawn_params(
            AgentKind::SHELL,
            SpawnOptions {
                cwd: Some("/tmp".into()),
                ..Default::default()
            },
        )
    }

    #[test]
    fn admission_is_bounded_and_receipts_require_explicit_error_dismissal() {
        let (mut store, mut effects) = SessionStore::headless(Prefs::default());
        let owner = SpawnOwner::default();
        for _ in 0..MAX_ACTIVE {
            assert!(
                store
                    .request_workspace_spawn(target(owner, "one"), params(&store))
                    .is_some()
            );
        }
        assert!(
            store
                .request_workspace_spawn(target(owner, "one"), params(&store))
                .is_none()
        );
        assert!(store.action_failure().unwrap().detail.contains("Eight"));
        store.dismiss_workspace_spawn(1);
        assert_eq!(
            store.workspace_spawn_receipts().count(),
            MAX_ACTIVE,
            "pending work cannot be dismissed or cancelled"
        );
        let mut launches = 0;
        while let Ok(effect) = effects.try_recv() {
            if matches!(
                effect,
                StoreEffect::WorkspaceSpawn {
                    params: Some(_),
                    ..
                }
            ) {
                launches += 1;
            }
        }
        assert_eq!(launches, MAX_ACTIVE);
        for id in 1..=MAX_ACTIVE as u64 {
            store.finish_workspace_spawn(
                id,
                WorkspaceSpawnState::Unconfirmed("Check All sessions".into()),
            );
        }
        for _ in MAX_ACTIVE..MAX_RECEIPTS {
            let id = store
                .request_workspace_spawn(target(owner, "one"), params(&store))
                .unwrap();
            store.finish_workspace_spawn(
                id,
                WorkspaceSpawnState::Unconfirmed("Check All sessions".into()),
            );
        }
        assert!(
            store
                .request_workspace_spawn(target(owner, "one"), params(&store))
                .is_none()
        );
        assert_eq!(store.workspace_spawn_receipts().count(), MAX_RECEIPTS);
        store.dismiss_workspace_spawn(1);
        assert!(
            store
                .request_workspace_spawn(target(owner, "one"), params(&store))
                .is_some()
        );
        assert_eq!(store.workspace_spawn_receipts().count(), MAX_RECEIPTS);
    }

    #[test]
    fn spawn_directory_uses_captured_focused_pane_and_preserves_explicit_override() {
        use diri_proto::workspace::*;
        let (mut store, _) = SessionStore::headless(Prefs::default());
        let mut fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        fixture
            .list
            .sessions
            .iter_mut()
            .find(|s| s.id.0 == "preview-claude")
            .unwrap()
            .cwd = "/window-one".into();
        fixture
            .list
            .sessions
            .iter_mut()
            .find(|s| s.id.0 == "preview-codex")
            .unwrap()
            .cwd = "/window-two".into();
        store.hydrate(fixture.list);
        store.select(SessionId::new("preview-claude"));
        let captured = target(SpawnOwner::default(), "two");
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: captured.workspace.clone(),
                name: "Two".into(),
                selected_tab: captured.selected_tab.clone(),
                tabs: vec![WorkspaceTab {
                    id: captured.selected_tab.clone().unwrap(),
                    title: None,
                    focused_pane: PaneId::new("focused"),
                    zoomed_pane: None,
                    layout: LayoutNode::Pane {
                        id: PaneId::new("focused"),
                        session_id: SessionId::new("preview-codex"),
                    },
                }],
            }],
            ..Default::default()
        });
        let resolved = store.spawn_params(
            AgentKind::SHELL,
            SpawnOptions {
                workspace_target: Some(captured.clone()),
                ..Default::default()
            },
        );
        assert_eq!(resolved.cwd, "/window-two");
        let explicit = store.spawn_params(
            AgentKind::SHELL,
            SpawnOptions {
                workspace_target: Some(captured),
                cwd: Some("/chosen".into()),
                ..Default::default()
            },
        );
        assert_eq!(explicit.cwd, "/chosen");
        assert_eq!(
            store.selected_session_id(),
            Some(&SessionId::new("preview-claude"))
        );
    }

    #[test]
    fn owners_and_returned_session_ids_remain_independent_through_retry() {
        let (mut store, mut effects) = SessionStore::headless(Prefs::default());
        let one = target(SpawnOwner::default(), "one");
        let two = target(SpawnOwner::default(), "two");
        assert_ne!(one.owner, two.owner);
        let a = store
            .request_workspace_spawn(one.clone(), params(&store))
            .unwrap();
        let b = store
            .request_workspace_spawn(two.clone(), params(&store))
            .unwrap();
        store.apply_spawn_result(SessionId::new("unrelated"));
        store.finish_workspace_spawn(
            b,
            WorkspaceSpawnState::Unplaced {
                session: SessionId::new("second"),
                detail: "conflict".into(),
            },
        );
        store.finish_workspace_spawn(
            a,
            WorkspaceSpawnState::Placed {
                session: SessionId::new("first"),
                tab: TabId::new("placed"),
            },
        );
        while effects.try_recv().is_ok() {}
        assert!(store.retry_workspace_placement(b));
        assert!(!store.retry_workspace_placement(b));
        assert_eq!(
            effects.try_recv().unwrap(),
            StoreEffect::WorkspaceSpawn {
                id: b,
                params: None
            }
        );
        let receipts: Vec<_> = store.workspace_spawn_receipts().cloned().collect();
        assert_eq!(receipts[0].target, one.into());
        assert_eq!(receipts[1].target, two.into());
        assert_eq!(
            receipts[1].state,
            WorkspaceSpawnState::Placing(SessionId::new("second"))
        );
        assert_eq!(
            store.selected_session_id(),
            Some(&SessionId::new("unrelated"))
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod engine_tests {
    use super::*;
    use diri_proto::workspace::WorkspaceSnapshot;
    use std::os::unix::fs::PermissionsExt;

    fn wait(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "workspace launch deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn mutate(
        f: &crate::workspace_fixture::LiveWorkspace,
        mutation: WorkspaceMutation,
    ) -> WorkspaceSnapshot {
        f.services.tokio.block_on(async {
            let client = f.services.store.client();
            let snapshot = client.workspaces().await.unwrap();
            client
                .mutate_workspace(&WorkspaceMutationParams {
                    expected_revision: snapshot.revision,
                    mutation,
                })
                .await
                .unwrap()
        })
    }
    fn state(f: &crate::workspace_fixture::LiveWorkspace, id: u64) -> WorkspaceSpawnState {
        f.services
            .store
            .store
            .read()
            .unwrap()
            .workspace_spawn_receipts()
            .find(|r| r.id == id)
            .unwrap()
            .state
            .clone()
    }

    #[test]
    fn removed_destination_preserves_created_session_and_retry_never_respawns() {
        let f = crate::workspace_fixture::LiveWorkspace::start();
        let held = f.held_spawn();
        let id = f
            .services
            .store
            .store
            .write()
            .unwrap()
            .request_workspace_spawn(
                WorkspaceSpawnTarget {
                    owner: SpawnOwner::default(),
                    workspace: f.workspace.clone(),
                    selected_tab: None,
                },
                held.params.clone(),
            )
            .unwrap();
        wait(|| held.entered.exists());
        mutate(
            &f,
            WorkspaceMutation::RemoveWorkspace {
                workspace_id: f.workspace.clone(),
            },
        );
        held.release();
        wait(|| !state(&f, id).pending());
        let WorkspaceSpawnState::Unplaced { session, detail } = state(&f, id) else {
            panic!("{:?}", state(&f, id));
        };
        assert!(detail.contains("removed"));
        let before = f
            .services
            .tokio
            .block_on(f.services.store.client().sessions())
            .unwrap();
        assert!(before.sessions.iter().any(|record| record.id == session));
        assert!(
            f.services
                .store
                .store
                .write()
                .unwrap()
                .retry_workspace_placement(id)
        );
        wait(|| !state(&f, id).pending());
        assert!(
            matches!(state(&f, id), WorkspaceSpawnState::Unplaced { session: found, .. } if found == session)
        );
        let after = f
            .services
            .tokio
            .block_on(f.services.store.client().sessions())
            .unwrap();
        assert_eq!(
            before
                .sessions
                .iter()
                .map(|r| &r.id)
                .collect::<HashSet<_>>(),
            after.sessions.iter().map(|r| &r.id).collect::<HashSet<_>>()
        );
        assert!(
            f.services
                .tokio
                .block_on(f.services.store.client().workspaces())
                .unwrap()
                .workspaces
                .is_empty()
        );
        f.verify_process_identity();
    }

    #[test]
    fn concurrent_launches_keep_captured_workspaces_and_slow_launch_preserves_new_selection() {
        let f = crate::workspace_fixture::LiveWorkspace::start();
        let other = mutate(
            &f,
            WorkspaceMutation::CreateWorkspace {
                name: "Other window".into(),
            },
        )
        .workspaces
        .last()
        .unwrap()
        .id
        .clone();
        let original = f
            .services
            .tokio
            .block_on(f.services.store.client().workspaces())
            .unwrap()
            .workspaces[0]
            .selected_tab
            .clone();
        let repo = f.directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
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
        let entered = f.directory.path().join("spawn-entered");
        let release = f.directory.path().join("spawn-release");
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
                entered.display(),
                release.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Ensure a failing assertion never leaves a shell waiting on our hook.
        struct Release(PathBuf);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, "release");
            }
        }
        let _release = Release(release.clone());
        let a = {
            let mut store = f.services.store.store.write().unwrap();
            let params = store.spawn_params(
                AgentKind::SHELL,
                SpawnOptions {
                    cwd: Some(repo.to_string_lossy().into_owned()),
                    worktree: Some(WorktreeSpawn {
                        create: true,
                        branch: Some("receipt-test".into()),
                    }),
                    ..Default::default()
                },
            );
            store
                .request_workspace_spawn(
                    WorkspaceSpawnTarget {
                        owner: SpawnOwner::default(),
                        workspace: f.workspace.clone(),
                        selected_tab: original,
                    },
                    params,
                )
                .unwrap()
        };
        wait(|| entered.exists());
        let b = {
            let mut store = f.services.store.store.write().unwrap();
            let params = store.spawn_params(
                AgentKind::SHELL,
                SpawnOptions {
                    cwd: Some(f.directory.path().to_string_lossy().into_owned()),
                    ..Default::default()
                },
            );
            store
                .request_workspace_spawn(
                    WorkspaceSpawnTarget {
                        owner: SpawnOwner::default(),
                        workspace: other.clone(),
                        selected_tab: None,
                    },
                    params,
                )
                .unwrap()
        };
        wait(|| !state(&f, b).pending());
        assert!(
            matches!(state(&f, b), WorkspaceSpawnState::Placed { .. }),
            "{:?}",
            state(&f, b)
        );
        assert_eq!(
            state(&f, a),
            WorkspaceSpawnState::Creating,
            "second window completes while first is paused"
        );
        let changed = mutate(
            &f,
            WorkspaceMutation::CreateTab {
                select: true,
                workspace_id: f.workspace.clone(),
                session_id: SessionId::new("review"),
                title: Some("Selected while launch waits".into()),
            },
        );
        let selected_after_navigation = changed.workspaces[0].selected_tab.clone();
        std::fs::write(&release, "release").unwrap();
        wait(|| !state(&f, a).pending());
        let WorkspaceSpawnState::Placed {
            session: a_session,
            tab: a_tab,
        } = state(&f, a)
        else {
            panic!("{:?}", state(&f, a));
        };
        let WorkspaceSpawnState::Placed {
            session: b_session,
            tab: b_tab,
        } = state(&f, b)
        else {
            unreachable!()
        };
        assert_ne!(a_session, b_session);
        let snapshot = f
            .services
            .tokio
            .block_on(f.services.store.client().workspaces())
            .unwrap();
        let first = snapshot
            .workspaces
            .iter()
            .find(|w| w.id == f.workspace)
            .unwrap();
        let second = snapshot.workspaces.iter().find(|w| w.id == other).unwrap();
        assert_eq!(first.selected_tab, selected_after_navigation);
        assert!(
            first
                .tabs
                .iter()
                .any(|tab| tab.id == a_tab && contains_session(&tab.layout, &a_session))
        );
        assert!(
            second
                .tabs
                .iter()
                .any(|tab| tab.id == b_tab && contains_session(&tab.layout, &b_session))
        );
        assert_eq!(
            diri_engine::workspace::WorkspaceStore::new(f.directory.path().join("state.json"))
                .snapshot()
                .unwrap(),
            snapshot
        );
        f.verify_process_identity();
        // Simulate a lost placement acknowledgment. Explicit retry observes the
        // already committed reference and neither spawns nor creates a tab.
        f.services
            .store
            .store
            .write()
            .unwrap()
            .finish_workspace_spawn(
                a,
                WorkspaceSpawnState::Unplaced {
                    session: a_session.clone(),
                    detail: "Acknowledgment lost".into(),
                },
            );
        assert!(
            f.services
                .store
                .store
                .write()
                .unwrap()
                .retry_workspace_placement(a)
        );
        wait(|| !state(&f, a).pending());
        assert_eq!(
            state(&f, a),
            WorkspaceSpawnState::Placed {
                session: a_session,
                tab: a_tab
            }
        );
        assert_eq!(
            f.services
                .tokio
                .block_on(f.services.store.client().workspaces())
                .unwrap(),
            snapshot
        );
    }
}
