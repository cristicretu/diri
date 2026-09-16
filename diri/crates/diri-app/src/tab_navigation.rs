//! One navigation scope for horizontal tabs and tab previews. This is a view
//! of the existing project/host group, never a second session or layout store.
use std::sync::Arc;

use diri_proto::{ProjectId, SessionRecord};

use crate::store::SessionStore;

pub const TAB_STRIP_HEIGHT: f32 = 42.0;

pub struct ProjectTabs {
    pub project: Option<ProjectId>,
    pub sessions: Vec<Arc<SessionRecord>>,
}

/// Include every active tab in saved tree order, even when its vertical row
/// is folded. A selected archived tab stays reachable until selection leaves it.
pub trait TabNavigationStore {
    fn tab_selection(&self) -> Option<(diri_proto::SessionId, ProjectId)>;
    fn tab_projection(&mut self) -> Arc<crate::store::SidebarProjection>;
}
impl TabNavigationStore for SessionStore {
    fn tab_selection(&self) -> Option<(diri_proto::SessionId, ProjectId)> {
        self.selected_session()
            .map(|session| (session.id.clone(), session.project_id.clone()))
    }
    fn tab_projection(&mut self) -> Arc<crate::store::SidebarProjection> {
        self.sidebar_projection()
    }
}
impl TabNavigationStore for crate::store::WindowWrite<'_> {
    fn tab_selection(&self) -> Option<(diri_proto::SessionId, ProjectId)> {
        self.selected_session()
            .map(|session| (session.id.clone(), session.project_id.clone()))
    }
    fn tab_projection(&mut self) -> Arc<crate::store::SidebarProjection> {
        self.sidebar_projection()
    }
}
pub fn selected_project_tabs(store: &mut impl TabNavigationStore) -> ProjectTabs {
    let selection = store.tab_selection();
    let selected = selection.as_ref().map(|(id, _)| id);
    let project = selection.as_ref().map(|(_, project)| project.clone());
    let projection = store.tab_projection();
    let group = projection
        .projects
        .iter()
        .find(|group| Some(&group.project.id) == project.as_ref())
        .or_else(|| {
            projection
                .projects
                .iter()
                .find(|group| !group.active.is_empty())
        });
    let Some(group) = group else {
        return ProjectTabs {
            project: None,
            sessions: Vec::new(),
        };
    };
    let mut sessions = group.active.clone();
    if let Some(archived) = group
        .archived
        .iter()
        .find(|session| Some(&session.id) == selected)
    {
        sessions.push(archived.clone());
    }
    ProjectTabs {
        project: Some(group.project.id.clone()),
        sessions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};

    #[test]
    fn tab_scope_preserves_order_when_project_or_descendants_are_collapsed() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let (mut store, _effects) = SessionStore::headless(fixture.prefs);
        store.hydrate(fixture.list);
        store.select(fixture.selected_session_id.unwrap());
        let before = selected_project_tabs(&mut store);
        let records = store.sessions().clone();
        let selected = store.selected_session_id().cloned();
        store
            .toggle_project_collapsed(before.project.unwrap())
            .unwrap();
        let after = selected_project_tabs(&mut store);
        assert_eq!(after.sessions, before.sessions);
        assert_eq!(store.selected_session_id(), selected.as_ref());
        assert_eq!(store.sessions(), &records);
    }
}
