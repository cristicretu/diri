//! Stable targets for workspace commands. The owning Root handles these events;
//! neither delayed editor submission nor dispatch consults another window.
use crate::{
    commands::{self, CommandId},
    palette::{PaletteAction, PaletteCommand},
};
use diri_proto::{
    SessionId, SessionRecord,
    workspace::{TabId, WorkspaceId, WorkspaceSnapshot},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceCommand {
    Create,
    Browse,
    Switch(Option<WorkspaceId>),
    Rename(WorkspaceId),
    RenameTab(TabId),
    CloseTab(TabId),
    RenameSession(SessionId),
    CloseSession(SessionId),
}

pub(crate) fn actions(
    snapshot: Option<&WorkspaceSnapshot>,
    active: Option<&WorkspaceId>,
    selected: Option<&SessionRecord>,
    sessions: &std::collections::HashMap<SessionId, std::sync::Arc<SessionRecord>>,
    editable: bool,
) -> Vec<PaletteAction> {
    let mut rows = vec![
        row(
            "new-workspace".into(),
            "New Workspace".into(),
            WorkspaceCommand::Create,
            None,
            None,
            editable,
        ),
        row(
            "switch-workspace".into(),
            "Switch Workspace".into(),
            WorkspaceCommand::Browse,
            None,
            Some(
                active
                    .and_then(|id| {
                        snapshot?
                            .workspaces
                            .iter()
                            .find(|w| &w.id == id)
                            .map(|w| w.name.clone())
                    })
                    .unwrap_or_else(|| {
                        if active.is_some() {
                            "Unavailable workspace".into()
                        } else {
                            "All sessions".into()
                        }
                    }),
            ),
            snapshot.is_some(),
        ),
    ];
    rows.push(row(
        "workspace-all-sessions".into(),
        "Switch to All sessions".into(),
        WorkspaceCommand::Switch(None),
        None,
        active.is_none().then(|| "Current".into()),
        true,
    ));
    if let Some(snapshot) = snapshot {
        for workspace in &snapshot.workspaces {
            rows.push(row(
                format!("switch-workspace-{}", workspace.id.0),
                format!("Switch to {}", workspace.name),
                WorkspaceCommand::Switch(Some(workspace.id.clone())),
                None,
                (Some(&workspace.id) == active).then(|| "Current".into()),
                true,
            ));
            if Some(&workspace.id) != active {
                continue;
            }
            rows.push(row(
                "rename-workspace".into(),
                "Rename Workspace".into(),
                WorkspaceCommand::Rename(workspace.id.clone()),
                None,
                Some(workspace.name.clone()),
                editable,
            ));
            if let Some(tab) = workspace
                .tabs
                .iter()
                .find(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
            {
                fn first_session(node: &diri_proto::workspace::LayoutNode) -> &SessionId {
                    match node {
                        diri_proto::workspace::LayoutNode::Pane { session_id, .. } => session_id,
                        diri_proto::workspace::LayoutNode::Split { first, .. } => {
                            first_session(first)
                        }
                    }
                }
                let detail = tab.title.clone().unwrap_or_else(|| {
                    sessions
                        .get(first_session(&tab.layout))
                        .map(|session| session.title.clone())
                        .unwrap_or_else(|| "Unavailable session".into())
                });
                rows.push(row(
                    "rename-selected-tab".into(),
                    "Rename Tab".into(),
                    WorkspaceCommand::RenameTab(tab.id.clone()),
                    Some(CommandId::RenameSelectedSession),
                    Some(detail.clone()),
                    editable,
                ));
                rows.push(row(
                    "close-selected-tab".into(),
                    "Close Tab".into(),
                    WorkspaceCommand::CloseTab(tab.id.clone()),
                    Some(CommandId::CloseSession),
                    Some(detail),
                    editable,
                ));
            }
        }
    }
    if active.is_none()
        && let Some(selected) = selected
    {
        rows.push(row(
            "rename-selected-session".into(),
            "Rename Session".into(),
            WorkspaceCommand::RenameSession(selected.id.clone()),
            Some(CommandId::RenameSelectedSession),
            Some(selected.title.clone()),
            true,
        ));
        rows.push(row(
            "close-selected-session".into(),
            "Close Session".into(),
            WorkspaceCommand::CloseSession(selected.id.clone()),
            Some(CommandId::CloseSession),
            Some(selected.title.clone()),
            true,
        ));
    }
    rows
}
fn row(
    id: String,
    title: String,
    command: WorkspaceCommand,
    shortcut: Option<CommandId>,
    detail: Option<String>,
    enabled: bool,
) -> PaletteAction {
    let (system_image, keywords) = match &command {
        WorkspaceCommand::Create => ("plus", "workspace collection new create"),
        WorkspaceCommand::Browse | WorkspaceCommand::Switch(_) => (
            "square.stack.3d.up",
            "workspace collection switch change select",
        ),
        WorkspaceCommand::Rename(_) => ("pencil", "workspace collection rename name title"),
        WorkspaceCommand::RenameTab(_) => ("pencil", "tab rename name title"),
        WorkspaceCommand::CloseTab(_) => {
            ("xmark", "tab close remove placement keep session running")
        }
        WorkspaceCommand::RenameSession(_) => ("pencil", "session rename name title"),
        WorkspaceCommand::CloseSession(_) => ("xmark", "session close stop terminate"),
    };
    PaletteAction {
        id,
        title,
        system_image,
        shortcut: shortcut.and_then(|id| commands::command(id).shortcut_label()),
        detail,
        enabled,
        is_default: false,
        command: PaletteCommand::Workspace(command),
        keywords: keywords.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> WorkspaceSnapshot {
        serde_json::from_value(serde_json::json!({"schemaVersion":1,"revision":7,"workspaces":[{"id":"workspace-a","name":"Delivery","selectedTab":"tab-a","tabs":[{"id":"tab-a","title":"Build","focusedPane":"pane-a","layout":{"type":"pane","id":"pane-a","sessionId":"session-a"}},{"id":"tab-b","title":"Review","focusedPane":"pane-b","layout":{"type":"pane","id":"pane-b","sessionId":"session-b"}}]}]})).unwrap()
    }
    #[test]
    fn captured_tab_commands_remain_stable_when_selection_and_name_change() {
        let mut snapshot = snapshot();
        let active = snapshot.workspaces[0].id.clone();
        let rows = actions(
            Some(&snapshot),
            Some(&active),
            None,
            &Default::default(),
            true,
        );
        let captured = rows
            .iter()
            .find(|row| row.id == "rename-selected-tab")
            .unwrap()
            .clone();
        assert_eq!(captured.title, "Rename Tab");
        assert_eq!(captured.detail.as_deref(), Some("Build"));
        assert_eq!(
            captured.command,
            PaletteCommand::Workspace(WorkspaceCommand::RenameTab(TabId::new("tab-a")))
        );
        assert!(rows.iter().any(
            |row| row.title == "Switch to Delivery" && row.detail.as_deref() == Some("Current")
        ));
        assert!(!rows.iter().any(|row| row.title == "Rename Session"));
        snapshot.workspaces[0].selected_tab = Some(TabId::new("tab-b"));
        snapshot.workspaces[0].name = "New name".into();
        let updated = actions(
            Some(&snapshot),
            Some(&active),
            None,
            &Default::default(),
            true,
        );
        assert!(
            updated
                .iter()
                .any(|row| row.title == "Rename Tab" && row.detail.as_deref() == Some("Review"))
        );
        assert_eq!(
            captured.command,
            PaletteCommand::Workspace(WorkspaceCommand::RenameTab(TabId::new("tab-a")))
        );
    }
    #[test]
    fn unavailable_catalog_disables_edits_and_does_not_claim_all_sessions_is_current() {
        let rows = actions(
            None,
            Some(&WorkspaceId::new("missing")),
            None,
            &Default::default(),
            false,
        );
        assert!(
            rows.iter()
                .any(|row| row.title == "New Workspace" && !row.enabled)
        );
        assert!(rows.iter().any(|row| row.title == "Switch Workspace"
            && row.detail.as_deref() == Some("Unavailable workspace")));
        assert!(
            !rows
                .iter()
                .any(|row| row.detail.as_deref() == Some("Current"))
        );
    }
}
