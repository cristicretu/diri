//! Agent navigation opens a project's saved layout without an extra creation step.
use super::*;

pub(super) struct ProjectAgentOpen {
    session: SessionId,
    preferred: Option<WorkspaceId>,
    revision: Option<u64>,
}

pub(super) fn first_agent(node: &diri_proto::workspace::LayoutNode) -> &SessionId {
    match node {
        diri_proto::workspace::LayoutNode::Pane { session_id, .. } => session_id,
        diri_proto::workspace::LayoutNode::Split { first, .. } => first_agent(first),
    }
}

pub(super) fn focused_agent(tab: &diri_proto::workspace::WorkspaceTab) -> Option<&SessionId> {
    fn find<'a>(
        node: &'a diri_proto::workspace::LayoutNode,
        pane: &PaneId,
    ) -> Option<&'a SessionId> {
        match node {
            diri_proto::workspace::LayoutNode::Pane { id, session_id } => {
                (id == pane).then_some(session_id)
            }
            diri_proto::workspace::LayoutNode::Split { first, second, .. } => {
                find(first, pane).or_else(|| find(second, pane))
            }
        }
    }
    find(&tab.layout, &tab.focused_pane)
}

impl Sidebar {
    /// One user intent, one revision-gated mutation. If another edit is in
    /// flight, keep only the latest requested agent; never replay a failed edit.
    pub(crate) fn open_selected_project_agent(&mut self, cx: &mut Context<Self>) -> bool {
        if self.preview {
            return false;
        }
        let session = {
            let store = self.store.read().expect("store");
            let Some(session) = store.selected_session() else {
                return false;
            };
            if session.is_archived()
                || matches!(
                    store.workspace_catalog().status(),
                    crate::store::WorkspaceCatalogStatus::Unavailable(_)
                )
            {
                return false;
            }
            session.id.clone()
        };
        if self.workspace_nav.project_agent.is_none()
            && self
                .store
                .read()
                .expect("store")
                .workspace_catalog()
                .can_edit()
            && self.workspace_focused_session().as_ref() == Some(&session)
        {
            self.workspace_nav.project_agent = None;
            cx.emit(SidebarEvent::WorkspaceTabActivated);
            return true;
        }
        self.workspace_nav.pending_activation = None;
        self.workspace_nav.project_agent = Some(ProjectAgentOpen {
            session,
            preferred: self.workspace_nav.active.clone(),
            revision: None,
        });
        self.reconcile_project_agent(cx);
        cx.notify();
        true
    }

    pub(super) fn reconcile_project_agent(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.workspace_nav.project_agent.as_ref() else {
            return;
        };
        let mut store = self.store.write().expect("store");
        if matches!(
            store.workspace_catalog().status(),
            crate::store::WorkspaceCatalogStatus::Unavailable(_)
        ) {
            drop(store);
            self.workspace_nav.project_agent = None;
            cx.emit(SidebarEvent::ProjectLayoutUnavailable);
            return;
        }
        if !store.workspace_catalog().can_edit() {
            return;
        }
        let Some(revision) = request.revision else {
            let expected = store
                .workspace_catalog()
                .snapshot()
                .expect("editable catalog")
                .revision;
            let Some(next_revision) = expected.checked_add(1) else {
                drop(store);
                self.workspace_nav.project_agent = None;
                cx.emit(SidebarEvent::ProjectLayoutUnavailable);
                return;
            };
            if store.edit_workspace(WorkspaceMutation::OpenProjectAgent {
                session_id: request.session.clone(),
                preferred_workspace: request.preferred.clone(),
            }) {
                self.workspace_nav.project_agent.as_mut().unwrap().revision = Some(next_revision);
            }
            return;
        };
        let catalog = store.workspace_catalog();
        let target = catalog
            .error
            .is_none()
            .then(|| {
                let snapshot = catalog.snapshot()?;
                if snapshot.revision < revision {
                    return None;
                }
                let project = &store.sessions().get(&request.session)?.project_id;
                let selected = |workspace: &&WorkspaceRecord| {
                    workspace.tabs.iter().any(|tab| {
                        workspace.selected_tab.as_ref() == Some(&tab.id)
                            && focused_agent(tab) == Some(&request.session)
                    })
                };
                snapshot
                    .workspaces
                    .iter()
                    .filter(|workspace| request.preferred.as_ref() == Some(&workspace.id))
                    .find(selected)
                    .or_else(|| {
                        snapshot
                            .workspaces
                            .iter()
                            .filter(|workspace| workspace.project_id.as_ref() == Some(project))
                            .find(selected)
                    })
                    .map(|workspace| workspace.id.clone())
            })
            .flatten();
        if target.is_some() {
            store.select(request.session.clone());
        }
        drop(store);
        self.workspace_nav.project_agent = None;
        if let Some(workspace) = target {
            if self.workspace_nav.active.as_ref() != Some(&workspace) {
                self.activate_workspace(Some(workspace), cx);
            } else {
                cx.emit(SidebarEvent::WorkspaceTabActivated);
            }
        } else {
            // The agent remains reachable even when its layout could not save.
            cx.emit(SidebarEvent::ProjectLayoutUnavailable);
        }
        cx.notify();
    }

    pub(crate) fn sync_focused_agent_selection(&mut self) {
        if self.workspace_nav.project_agent.is_some() {
            return;
        }
        if let Some(session) = self.workspace_focused_session() {
            let mut store = self.store.write().expect("store");
            if store.selected_session_id() != Some(&session)
                && store.sessions().contains_key(&session)
            {
                store.select(session);
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
impl Sidebar {
    pub(crate) fn project_agent_center_for_test(&self, id: &SessionId) -> Option<Point<Pixels>> {
        self.row_bounds
            .borrow()
            .get(id)
            .map(|bounds| bounds.center())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{LayoutNode, WorkspaceSnapshot, WorkspaceTab};

    fn snapshot(session: &SessionId, project: &ProjectId) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            revision: 4,
            workspaces: vec![WorkspaceRecord {
                id: WorkspaceId::new("project-view"),
                project_id: Some(project.clone()),
                name: "Project".into(),
                selected_tab: Some(TabId::new("agent-tab")),
                tabs: vec![WorkspaceTab {
                    id: TabId::new("agent-tab"),
                    title: None,
                    layout: LayoutNode::Pane {
                        id: PaneId::new("pane"),
                        session_id: session.clone(),
                    },
                    focused_pane: PaneId::new("pane"),
                    zoomed_pane: None,
                }],
            }],
            ..Default::default()
        }
    }

    #[gpui::test]
    fn active_project_keeps_new_agent_and_real_agent_rows(cx: &mut gpui::TestAppContext) {
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let session = store.sessions()[&SessionId::new("preview-claude")].clone();
            store.seed_workspace_snapshot_for_test(snapshot(&session.id, &session.project_id));
            drop(store);
            sidebar.workspace_nav.active = Some(WorkspaceId::new("project-view"));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("new-agent").is_some());
        assert!(cx.debug_bounds("SESSION_preview-claude").is_some());
        assert!(cx.debug_bounds("SESSION_preview-codex").is_some());
        assert!(cx.debug_bounds("workspace-picker").is_none());
        let new_agent = cx.debug_bounds("new-agent").unwrap();
        cx.simulate_click(new_agent.center(), gpui::Modifiers::default());
        sidebar.read_with(cx, |sidebar, _| {
            assert!(matches!(sidebar.ui.popover, Some(Popover::NewAgent { .. })))
        });
    }

    #[gpui::test]
    fn active_project_vertical_shortcuts_rename_and_close_agents(cx: &mut gpui::TestAppContext) {
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let session = store.sessions()[&SessionId::new("preview-claude")].clone();
            store.seed_workspace_snapshot_for_test(snapshot(&session.id, &session.project_id));
            drop(store);
            sidebar.workspace_nav.active = Some(WorkspaceId::new("project-view"));
            assert!(
                sidebar.select_shortcut(1, cx),
                "second agent is available despite only one open layout tab"
            );
        });
        sidebar.update_in(cx, |sidebar, window, cx| {
            let selected = sidebar
                .store
                .read()
                .unwrap()
                .selected_session_id()
                .cloned()
                .unwrap();
            assert!(sidebar.rename_selected(window, cx));
            assert_eq!(sidebar.ui.renaming.as_ref(), Some(&selected));
            sidebar.ui.cancel_rename();
            assert!(sidebar.close_selected_now(cx));
            let store = sidebar.store.read().unwrap();
            assert!(!store.sessions().contains_key(&selected));
            assert_eq!(
                store.workspace_catalog().snapshot().unwrap().workspaces[0]
                    .tabs
                    .len(),
                1,
                "closing an agent is not silently converted to removing a tab placement"
            );
        });
    }
    #[gpui::test]
    fn rapid_agent_selection_waits_for_the_latest_intent_without_replaying_failure(
        cx: &mut gpui::TestAppContext,
    ) {
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
        sidebar.update(cx, |sidebar, cx| {
            sidebar.preview = false;
            let claude = SessionId::new("preview-claude");
            let codex = SessionId::new("preview-codex");
            let mut store = sidebar.store.write().unwrap();
            let project = store.sessions()[&claude].project_id.clone();
            let original = snapshot(&claude, &project);
            store.seed_workspace_snapshot_for_test(original.clone());
            store.select(codex.clone());
            drop(store);
            sidebar.workspace_nav.active = Some(WorkspaceId::new("project-view"));
            assert!(sidebar.open_selected_project_agent(cx));
            assert_eq!(
                sidebar
                    .workspace_nav
                    .project_agent
                    .as_ref()
                    .unwrap()
                    .revision,
                Some(5)
            );
            sidebar.store.write().unwrap().select(claude.clone());
            assert!(sidebar.open_selected_project_agent(cx));
            assert_eq!(
                sidebar
                    .workspace_nav
                    .project_agent
                    .as_ref()
                    .unwrap()
                    .revision,
                None
            );
            // Old response completes first. It cannot acknowledge the newer intent.
            let mut old_reply = original.clone();
            old_reply.revision = 5;
            old_reply.workspaces[0].tabs[0].layout = LayoutNode::Pane {
                id: PaneId::new("pane"),
                session_id: codex,
            };
            sidebar
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(old_reply);
            sidebar.reconcile_project_agent(cx);
            let pending = sidebar.workspace_nav.project_agent.as_ref().unwrap();
            assert_eq!(pending.session, claude);
            assert_eq!(pending.revision, Some(6));
            // A response without the requested revision/layout is not success,
            // and render reconciliation must not submit it again.
            sidebar
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(original);
            sidebar.reconcile_project_agent(cx);
            assert!(sidebar.workspace_nav.project_agent.is_none());
            assert!(sidebar.store.read().unwrap().workspace_catalog().can_edit());
            sidebar.reconcile_project_agent(cx);
            assert!(sidebar.store.read().unwrap().workspace_catalog().can_edit());
        });
    }
}
