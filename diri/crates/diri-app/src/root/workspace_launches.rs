use super::*;
use crate::store::WorkspaceSpawnTarget;

impl RootView {
    pub(super) fn workspace_spawn_target(&self) -> Option<WorkspaceSpawnTarget> {
        let workspace = self.active_workspace.clone()?;
        let store = self.window_store.read().expect("store");
        let selected_tab = store
            .workspace_catalog()
            .snapshot()?
            .workspaces
            .iter()
            .find(|w| w.id == workspace)?
            .selected_tab
            .clone();
        Some(WorkspaceSpawnTarget {
            owner: self.spawn_owner,
            workspace,
            selected_tab,
        })
    }

    pub(super) fn sync_workspace_spawn_context(&self, cx: &mut Context<Self>) {
        let target = self.workspace_spawn_target();
        self.launcher.update(cx, |launcher, _| {
            launcher.set_workspace_spawn_target(target.clone())
        });
        if let Some(navigation) = &self.navigation {
            navigation.update(cx, |navigation, cx| {
                navigation.set_workspace_spawn_target(target);
                navigation.set_workspace_palette_context(self.active_workspace.clone(), cx)
            });
        }
    }

    pub(super) fn open_workspace_launch_session(
        &mut self,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_store.write().expect("store").select(session);
        let opening_project = self
            .sidebar
            .update(cx, |sidebar, cx| sidebar.open_selected_project_agent(cx));
        if !opening_project {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
            self.activate_saved_workspace(None, window, cx);
        }
        self.services.store.publish_local_change();
        cx.notify();
    }
}
