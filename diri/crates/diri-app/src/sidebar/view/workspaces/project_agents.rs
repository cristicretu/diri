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
        if self.workspace_focused_session().as_ref() == Some(&session) {
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
            if store.edit_workspace(WorkspaceMutation::OpenProjectAgent {
                session_id: request.session.clone(),
                preferred_workspace: request.preferred.clone(),
            }) {
                self.workspace_nav.project_agent.as_mut().unwrap().revision =
                    expected.checked_add(1);
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
                snapshot
                    .workspaces
                    .iter()
                    .filter(|workspace| {
                        request.preferred.as_ref() == Some(&workspace.id)
                            || workspace.project_id.as_ref() == Some(project)
                    })
                    .find(|workspace| {
                        workspace.tabs.iter().any(|tab| {
                            workspace.selected_tab.as_ref() == Some(&tab.id)
                                && focused_agent(tab) == Some(&request.session)
                        })
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
