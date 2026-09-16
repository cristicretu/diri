//! Durable organization intent behind one revisioned interface. This module
//! never launches, resizes, attaches to, or terminates a session.
mod tree;

use std::collections::HashSet;
use std::path::PathBuf;

use diri_proto::workspace::*;
use diri_proto::{ControlError, SessionId};
use serde_json::{Map, Value};

use crate::state_file::JsonStateFile;

const KEY: &str = "workspaceState";
pub struct WorkspaceStore {
    file: JsonStateFile,
}

impl WorkspaceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: JsonStateFile::new(path),
        }
    }

    pub fn snapshot(&self) -> Result<WorkspaceSnapshot, ControlError> {
        decode(self.file.read().map_err(storage_error)?.as_ref())
    }

    /// Revision comparison, validation, and atomic replacement happen under
    /// JsonStateFile's one advisory lock. Rejected edits leave the file intact. A storage failure requires a fresh
    /// snapshot because a rename may have committed before its final sync failed.
    pub fn apply(
        &self,
        params: WorkspaceMutationParams,
        sessions: &HashSet<SessionId>,
    ) -> Result<WorkspaceSnapshot, ControlError> {
        let mut outcome = None;
        let written = self.file.update_durable(|document| {
            let result = (|| {
                let mut next = decode(Some(document))?;
                if next.revision != params.expected_revision {
                    return Err(ControlError::new(
                        "workspace_revision_conflict",
                        format!(
                            "expected revision {}, current revision {}",
                            params.expected_revision, next.revision
                        ),
                    ));
                }
                mutate(&mut next, params.mutation, sessions)?;
                tree::repair(&mut next);
                validate(&next)?;
                next.revision = next
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("revision exhausted"))?;
                let value = serde_json::to_value(&next)
                    .map_err(|_| invalid("cannot encode workspace state"))?;
                document.insert(KEY.into(), value);
                Ok(next)
            })();
            let failed = result.is_err();
            outcome = Some(result);
            if failed {
                Err(std::io::Error::other("workspace mutation rejected"))
            } else {
                Ok(())
            }
        });
        match outcome {
            Some(Err(error)) => Err(error),
            Some(Ok(snapshot)) => {
                written.map_err(storage_error)?;
                Ok(snapshot)
            }
            None => Err(storage_error(written.unwrap_err())),
        }
    }
}

fn storage_error(_: std::io::Error) -> ControlError {
    ControlError::new(
        "workspace_storage_unavailable",
        "workspace state could not be read or durably saved",
    )
}
fn invalid(message: impl Into<String>) -> ControlError {
    ControlError::new("invalid_workspace", message)
}
fn missing() -> ControlError {
    ControlError::new(
        "workspace_target_not_found",
        "workspace, tab, pane, or divider no longer exists",
    )
}
fn decode(document: Option<&Map<String, Value>>) -> Result<WorkspaceSnapshot, ControlError> {
    let Some(value) = document.and_then(|document| document.get(KEY)) else {
        return Ok(WorkspaceSnapshot::default());
    };
    let state: WorkspaceSnapshot = serde_json::from_value(value.clone())
        .map_err(|_| invalid("workspace state is corrupt or unsupported"))?;
    validate(&state)?;
    Ok(state)
}
fn name(value: &str) -> Result<(), ControlError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(invalid("names must contain 1–256 printable bytes"))
    } else {
        Ok(())
    }
}
fn identity(value: &str, seen: &mut HashSet<String>) -> Result<(), ControlError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        || !seen.insert(value.into())
    {
        Err(invalid("invalid or duplicate layout identity"))
    } else {
        Ok(())
    }
}
fn validate(state: &WorkspaceSnapshot) -> Result<(), ControlError> {
    if state.schema_version != WORKSPACE_SCHEMA_VERSION {
        return Err(ControlError::new(
            "workspace_schema_unsupported",
            "workspace schema version is unsupported",
        ));
    }
    if state.workspaces.len() > MAX_WORKSPACES
        || state.workspaces.iter().map(|w| w.tabs.len()).sum::<usize>() > MAX_WORKSPACE_TABS
    {
        return Err(invalid("workspace or tab count exceeds the limit"));
    }
    let mut seen = HashSet::new();
    for workspace in &state.workspaces {
        identity(&workspace.id.0, &mut seen)?;
        name(&workspace.name)?;
        if workspace.tabs.is_empty() != workspace.selected_tab.is_none()
            || workspace
                .selected_tab
                .as_ref()
                .is_some_and(|selected| !workspace.tabs.iter().any(|tab| &tab.id == selected))
        {
            return Err(invalid("selected tab does not belong to its workspace"));
        }
        for tab in &workspace.tabs {
            identity(&tab.id.0, &mut seen)?;
            if let Some(title) = &tab.title {
                name(title)?;
            }
            validate_node(&tab.layout, 0, &mut seen)?;
            let ids = tree::panes(&tab.layout);
            if ids.len() > MAX_TAB_PANES
                || tab
                    .zoomed_pane
                    .as_ref()
                    .is_some_and(|id| id != &tab.focused_pane)
                || !ids.contains(&tab.focused_pane)
                || tab.zoomed_pane.as_ref().is_some_and(|id| !ids.contains(id))
            {
                return Err(invalid("invalid pane count, focus, or zoom target"));
            }
        }
    }
    Ok(())
}
fn validate_node(
    node: &LayoutNode,
    depth: usize,
    seen: &mut HashSet<String>,
) -> Result<(), ControlError> {
    if depth >= MAX_LAYOUT_DEPTH {
        return Err(invalid("layout tree is too deep"));
    }
    match node {
        LayoutNode::Pane { id, session_id } => {
            identity(&id.0, seen)?;
            // Pane identity is unique; a saved pane is a reference to a session.
            // Repeated references share one controller and do not spawn another PTY.
            if session_id.0.is_empty() || session_id.0.len() > 128 {
                return Err(invalid("invalid session reference"));
            }
        }
        LayoutNode::Split {
            id,
            fraction,
            first,
            second,
            ..
        } => {
            identity(&id.0, seen)?;
            if !fraction.is_finite() || !(0.1..=0.9).contains(fraction) {
                return Err(invalid("split fraction must be between 0.1 and 0.9"));
            }
            validate_node(first, depth + 1, seen)?;
            validate_node(second, depth + 1, seen)?;
        }
    }
    Ok(())
}
fn new_id(prefix: &str) -> Result<String, ControlError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| ControlError::internal("cannot create layout identity"))?;
    Ok(format!(
        "{prefix}_{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
fn workspace<'a>(
    state: &'a mut WorkspaceSnapshot,
    id: &WorkspaceId,
) -> Result<&'a mut WorkspaceRecord, ControlError> {
    state
        .workspaces
        .iter_mut()
        .find(|workspace| &workspace.id == id)
        .ok_or_else(missing)
}
fn tab<'a>(
    state: &'a mut WorkspaceSnapshot,
    id: &TabId,
) -> Result<&'a mut WorkspaceTab, ControlError> {
    state
        .workspaces
        .iter_mut()
        .flat_map(|workspace| &mut workspace.tabs)
        .find(|tab| &tab.id == id)
        .ok_or_else(missing)
}
fn remove_tab(state: &mut WorkspaceSnapshot, id: &TabId) -> Result<WorkspaceTab, ControlError> {
    for workspace in &mut state.workspaces {
        if let Some(index) = workspace.tabs.iter().position(|tab| &tab.id == id) {
            return Ok(workspace.tabs.remove(index));
        }
    }
    Err(missing())
}
fn existing(session: &SessionId, sessions: &HashSet<SessionId>) -> Result<(), ControlError> {
    if sessions.contains(session) {
        Ok(())
    } else {
        Err(ControlError::new(
            "workspace_session_unavailable",
            "the session is absent from the Engine inventory",
        ))
    }
}
fn require_pane(tab: &WorkspaceTab, id: &PaneId) -> Result<(), ControlError> {
    if tree::find(&tab.layout, &LayoutNodeId::Pane(id.clone())).is_some() {
        Ok(())
    } else {
        Err(missing())
    }
}

fn mutate(
    state: &mut WorkspaceSnapshot,
    mutation: WorkspaceMutation,
    sessions: &HashSet<SessionId>,
) -> Result<(), ControlError> {
    use WorkspaceMutation::*;
    match mutation {
        CreateWorkspace { name: title } => {
            name(&title)?;
            state.workspaces.push(WorkspaceRecord {
                id: WorkspaceId(new_id("workspace")?),
                name: title,
                tabs: Vec::new(),
                selected_tab: None,
            });
        }
        RenameWorkspace {
            workspace_id,
            name: title,
        } => {
            name(&title)?;
            workspace(state, &workspace_id)?.name = title;
        }
        RemoveWorkspace { workspace_id } => {
            let index = state
                .workspaces
                .iter()
                .position(|w| w.id == workspace_id)
                .ok_or_else(missing)?;
            state.workspaces.remove(index);
        }
        MoveWorkspace {
            workspace_id,
            index,
        } => {
            let old = state
                .workspaces
                .iter()
                .position(|w| w.id == workspace_id)
                .ok_or_else(missing)?;
            if index >= state.workspaces.len() {
                return Err(invalid("workspace destination is out of range"));
            }
            let record = state.workspaces.remove(old);
            state.workspaces.insert(index, record);
        }
        CreateTab {
            select,
            workspace_id,
            session_id,
            title,
        } => {
            existing(&session_id, sessions)?;
            let pane_id = PaneId(new_id("pane")?);
            let tab_id = TabId(new_id("tab")?);
            let workspace = workspace(state, &workspace_id)?;
            workspace.tabs.push(WorkspaceTab {
                id: tab_id.clone(),
                title,
                layout: LayoutNode::Pane {
                    id: pane_id.clone(),
                    session_id,
                },
                focused_pane: pane_id,
                zoomed_pane: None,
            });
            if select || workspace.selected_tab.is_none() {
                workspace.selected_tab = Some(tab_id);
            }
        }
        RenameTab { tab_id, title } => {
            tab(state, &tab_id)?.title = title;
        }
        RemoveTab { tab_id } => {
            remove_tab(state, &tab_id)?;
        }
        MoveTab {
            tab_id,
            workspace_id,
            index,
        } => {
            let record = remove_tab(state, &tab_id)?;
            let workspace = workspace(state, &workspace_id)?;
            if index > workspace.tabs.len() {
                return Err(invalid("tab destination is out of range"));
            }
            workspace.tabs.insert(index, record);
        }
        SelectTab {
            workspace_id,
            tab_id,
        } => {
            let workspace = workspace(state, &workspace_id)?;
            if !workspace.tabs.iter().any(|tab| tab.id == tab_id) {
                return Err(missing());
            }
            workspace.selected_tab = Some(tab_id);
        }
        SplitPane {
            tab_id,
            target,
            session_id,
            edge,
        } => {
            existing(&session_id, sessions)?;
            let pane_id = PaneId(new_id("pane")?);
            let incoming = LayoutNode::Pane {
                id: pane_id.clone(),
                session_id,
            };
            let tab = tab(state, &tab_id)?;
            if !tree::dock(
                &mut tab.layout,
                &target,
                &incoming,
                edge,
                &SplitId(new_id("split")?),
            ) {
                return Err(missing());
            }
            tab.focused_pane = pane_id;
        }
        RemovePane { tab_id, pane_id } => {
            let current = tab(state, &tab_id)?;
            let (retained, removed) =
                tree::take(current.layout.clone(), &LayoutNodeId::Pane(pane_id));
            if removed.is_none() {
                return Err(missing());
            }
            if let Some(layout) = retained {
                current.layout = layout;
            } else {
                remove_tab(state, &tab_id)?;
            }
        }
        MoveNode {
            source_tab,
            node,
            destination_tab,
            target,
            edge,
        } => {
            let source = tab(state, &source_tab)?;
            let incoming = tree::find(&source.layout, &node)
                .cloned()
                .ok_or_else(missing)?;
            if source_tab == destination_tab
                && tree::find(&incoming, &LayoutNodeId::Pane(target.clone())).is_some()
            {
                return Err(invalid("cannot dock a subtree into itself"));
            }
            let (remaining, _) = tree::take(source.layout.clone(), &node);
            if let Some(layout) = remaining {
                source.layout = layout;
            } else {
                remove_tab(state, &source_tab)?;
            }
            let destination = tab(state, &destination_tab)?;
            if !tree::dock(
                &mut destination.layout,
                &target,
                &incoming,
                edge,
                &SplitId(new_id("split")?),
            ) {
                return Err(missing());
            }
            destination.focused_pane = tree::panes(&incoming)[0].clone();
        }
        SwapPanes {
            first_tab,
            first,
            second_tab,
            second,
        } => {
            let a = tree::find(&tab(state, &first_tab)?.layout, &LayoutNodeId::Pane(first))
                .cloned()
                .ok_or_else(missing)?;
            let b = tree::find(
                &tab(state, &second_tab)?.layout,
                &LayoutNodeId::Pane(second),
            )
            .cloned()
            .ok_or_else(missing)?;
            tree::swap(&mut tab(state, &first_tab)?.layout, &a, &b);
            if first_tab != second_tab {
                tree::swap(&mut tab(state, &second_tab)?.layout, &a, &b);
            }
        }
        ResizeSplit {
            tab_id,
            split_id,
            fraction,
        } => {
            if !fraction.is_finite() || !(0.1..=0.9).contains(&fraction) {
                return Err(invalid("split fraction must be between 0.1 and 0.9"));
            }
            if !tree::resize(&mut tab(state, &tab_id)?.layout, &split_id, fraction) {
                return Err(missing());
            }
        }
        FocusPane { tab_id, pane_id } => {
            let tab = tab(state, &tab_id)?;
            require_pane(tab, &pane_id)?;
            tab.focused_pane = pane_id;
        }
        ZoomPane { tab_id, pane_id } => {
            let tab = tab(state, &tab_id)?;
            if let Some(id) = &pane_id {
                require_pane(tab, id)?;
                tab.focused_pane = id.clone();
            }
            tab.zoomed_pane = pane_id;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use WorkspaceMutation::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        path: PathBuf,
        store: WorkspaceStore,
        sessions: HashSet<SessionId>,
        snapshot: WorkspaceSnapshot,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state.json");
            std::fs::write(
                &path,
                r#"{"version":1,"sessions":[],"projects":[{"id":"p","future":"preserved"}]}"#,
            )
            .unwrap();
            Self {
                store: WorkspaceStore::new(&path),
                _temp: temp,
                path,
                sessions: (0..12)
                    .map(|i| SessionId::new(format!("session_{i}")))
                    .collect(),
                snapshot: WorkspaceSnapshot::default(),
            }
        }
        fn apply(&mut self, mutation: WorkspaceMutation) {
            self.snapshot = self
                .store
                .apply(
                    WorkspaceMutationParams {
                        expected_revision: self.snapshot.revision,
                        mutation,
                    },
                    &self.sessions,
                )
                .unwrap();
        }
        fn create_workspace(&mut self, name: &str) -> WorkspaceId {
            self.apply(CreateWorkspace { name: name.into() });
            self.snapshot.workspaces.last().unwrap().id.clone()
        }
        fn create_tab(&mut self, workspace_id: WorkspaceId, session: usize) -> (TabId, PaneId) {
            self.apply(CreateTab {
                select: true,
                workspace_id: workspace_id.clone(),
                session_id: SessionId::new(format!("session_{session}")),
                title: None,
            });
            let tab = self
                .snapshot
                .workspaces
                .iter()
                .find(|w| w.id == workspace_id)
                .unwrap()
                .tabs
                .last()
                .unwrap();
            (tab.id.clone(), tab.focused_pane.clone())
        }
        fn reject(&self, mutation: WorkspaceMutation, code: &str) {
            let before = std::fs::read(&self.path).unwrap();
            let error = self
                .store
                .apply(
                    WorkspaceMutationParams {
                        expected_revision: self.snapshot.revision,
                        mutation,
                    },
                    &self.sessions,
                )
                .unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(std::fs::read(&self.path).unwrap(), before);
        }
    }

    #[test]
    fn named_collections_reorder_move_and_restart_without_touching_session_records() {
        let mut f = Fixture::new();
        let first = f.create_workspace("Client API");
        let second = f.create_workspace("Operations");
        let (tab, _) = f.create_tab(first.clone(), 0);
        f.create_tab(first.clone(), 1);
        f.apply(RenameWorkspace {
            workspace_id: first.clone(),
            name: "API and remote deployment".into(),
        });
        f.apply(MoveWorkspace {
            workspace_id: second.clone(),
            index: 0,
        });
        f.apply(MoveTab {
            tab_id: tab.clone(),
            workspace_id: second.clone(),
            index: 0,
        });
        f.apply(RenameTab {
            tab_id: tab,
            title: Some("Remote logs".into()),
        });
        assert_eq!(f.snapshot.workspaces[0].id, second);
        assert_eq!(f.snapshot.workspaces[1].tabs.len(), 1);
        assert_eq!(WorkspaceStore::new(&f.path).snapshot().unwrap(), f.snapshot);
        let document: Value = serde_json::from_slice(&std::fs::read(&f.path).unwrap()).unwrap();
        assert_eq!(document["sessions"], serde_json::json!([]));
        assert_eq!(document["projects"][0]["future"], "preserved");
    }

    #[test]
    fn split_move_swap_resize_focus_zoom_and_close_preserve_placement_identity() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Work");
        let (first, first_pane) = f.create_tab(workspace.clone(), 0);
        let (second, second_pane) = f.create_tab(workspace, 1);
        f.apply(SplitPane {
            tab_id: first.clone(),
            target: first_pane.clone(),
            session_id: SessionId::new("session_2"),
            edge: DockEdge::Right,
        });
        let split = match &f.snapshot.workspaces[0].tabs[0].layout {
            LayoutNode::Split { id, .. } => id.clone(),
            _ => panic!(),
        };
        let added = f.snapshot.workspaces[0].tabs[0].focused_pane.clone();
        f.apply(ResizeSplit {
            tab_id: first.clone(),
            split_id: split.clone(),
            fraction: 0.7,
        });
        f.apply(ZoomPane {
            tab_id: first.clone(),
            pane_id: Some(first_pane.clone()),
        });
        f.apply(FocusPane {
            tab_id: first.clone(),
            pane_id: added.clone(),
        });
        assert_eq!(
            f.snapshot.workspaces[0].tabs[0].zoomed_pane,
            Some(added.clone())
        );
        f.apply(SwapPanes {
            first_tab: first.clone(),
            first: added.clone(),
            second_tab: second.clone(),
            second: second_pane.clone(),
        });
        assert!(tree::panes(&f.snapshot.workspaces[0].tabs[0].layout).contains(&second_pane));
        f.apply(MoveNode {
            source_tab: second.clone(),
            node: LayoutNodeId::Pane(added.clone()),
            destination_tab: first.clone(),
            target: first_pane.clone(),
            edge: DockEdge::Bottom,
        });
        assert_eq!(f.snapshot.workspaces[0].tabs.len(), 1);
        assert!(tree::panes(&f.snapshot.workspaces[0].tabs[0].layout).contains(&added));
        f.apply(RemovePane {
            tab_id: first.clone(),
            pane_id: added,
        });
        f.apply(ZoomPane {
            tab_id: first.clone(),
            pane_id: None,
        });
        assert!(
            matches!(&f.snapshot.workspaces[0].tabs[0].layout, LayoutNode::Split { id, fraction, .. } if *id == split && *fraction == 0.7)
        );
        f.apply(RemovePane {
            tab_id: first.clone(),
            pane_id: second_pane,
        });
        f.apply(RemovePane {
            tab_id: first,
            pane_id: first_pane,
        });
        assert!(f.snapshot.workspaces[0].tabs.is_empty());
        assert_eq!(f.sessions.len(), 12);
    }

    #[test]
    fn repeated_session_references_keep_unique_panes_and_invalid_moves_are_atomic() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Work");
        let (tab, pane) = f.create_tab(workspace, 0);
        f.apply(SplitPane {
            tab_id: tab.clone(),
            target: pane.clone(),
            session_id: SessionId::new("session_0"),
            edge: DockEdge::Right,
        });
        let saved = &f.snapshot.workspaces[0].tabs[0];
        let panes = tree::panes(&saved.layout);
        assert_eq!(panes.len(), 2);
        assert_ne!(panes[0], panes[1]);
        assert_eq!(f.store.snapshot().unwrap(), f.snapshot);
        assert_eq!(f.sessions.len(), 12);

        f.reject(
            MoveNode {
                source_tab: tab.clone(),
                node: LayoutNodeId::Pane(pane.clone()),
                destination_tab: tab.clone(),
                target: pane,
                edge: DockEdge::Left,
            },
            "invalid_workspace",
        );
        f.reject(
            MoveTab {
                tab_id: tab,
                workspace_id: WorkspaceId::new("missing"),
                index: 0,
            },
            "workspace_target_not_found",
        );
    }

    #[test]
    fn moving_a_whole_tree_keeps_divider_ids_and_stale_dividers_cannot_retarget() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Work");
        let (first, first_pane) = f.create_tab(workspace.clone(), 0);
        let (second, second_pane) = f.create_tab(workspace, 1);
        f.apply(SplitPane {
            tab_id: first.clone(),
            target: first_pane.clone(),
            session_id: SessionId::new("session_2"),
            edge: DockEdge::Right,
        });
        let original = match &f.snapshot.workspaces[0].tabs[0].layout {
            LayoutNode::Split { id, .. } => id.clone(),
            _ => panic!(),
        };
        f.apply(ResizeSplit {
            tab_id: first.clone(),
            split_id: original.clone(),
            fraction: 0.65,
        });
        f.apply(MoveNode {
            source_tab: first,
            node: LayoutNodeId::Split(original.clone()),
            destination_tab: second.clone(),
            target: second_pane.clone(),
            edge: DockEdge::Bottom,
        });
        let moved = &f.snapshot.workspaces[0].tabs[0].layout;
        assert!(
            matches!(tree::find(moved, &LayoutNodeId::Split(original.clone())), Some(LayoutNode::Split { fraction, .. }) if *fraction == 0.65)
        );
        f.apply(RemovePane {
            tab_id: second.clone(),
            pane_id: first_pane,
        });
        f.reject(
            ResizeSplit {
                tab_id: second.clone(),
                split_id: original,
                fraction: 0.4,
            },
            "workspace_target_not_found",
        );
        assert!(tree::panes(&f.snapshot.workspaces[0].tabs[0].layout).contains(&second_pane));
    }

    #[test]
    fn concurrent_gui_and_cli_edits_share_one_compare_and_swap() {
        let mut f = Fixture::new();
        f.create_workspace("Work");
        let revision = f.snapshot.revision;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads = (0..2)
            .map(|i| {
                let path = f.path.clone();
                let sessions = f.sessions.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    WorkspaceStore::new(path).apply(
                        WorkspaceMutationParams {
                            expected_revision: revision,
                            mutation: CreateWorkspace {
                                name: format!("Window {i}"),
                            },
                        },
                        &sessions,
                    )
                })
            })
            .collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .next()
                .unwrap()
                .code,
            "workspace_revision_conflict"
        );
        assert_eq!(f.store.snapshot().unwrap().revision, revision + 1);
        assert_eq!(f.store.snapshot().unwrap().workspaces.len(), 2);
    }

    #[test]
    fn background_tab_creation_preserves_selection_and_old_wire_defaults_to_select() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Work");
        let (original, _) = f.create_tab(workspace.clone(), 0);
        f.apply(CreateTab {
            select: false,
            workspace_id: workspace.clone(),
            session_id: SessionId::new("session_1"),
            title: None,
        });
        assert_eq!(f.snapshot.workspaces[0].selected_tab, Some(original));
        assert_eq!(f.snapshot.workspaces[0].tabs.len(), 2);
        assert_eq!(f.store.snapshot().unwrap(), f.snapshot);
        let old_wire = serde_json::json!({"type":"createTab", "workspaceId":workspace, "sessionId":"session_2", "title":null});
        let mutation = serde_json::from_value(old_wire).unwrap();
        assert!(matches!(mutation, CreateTab { select: true, .. }));
        f.apply(mutation);
        assert_eq!(
            f.snapshot.workspaces[0].selected_tab.as_ref(),
            Some(&f.snapshot.workspaces[0].tabs[2].id)
        );
    }

    #[test]
    fn removal_race_keeps_unavailable_reference_and_never_recreates_session() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Work");
        // The inventory was captured immediately before session removal wins.
        let captured = f.sessions.clone();
        f.sessions.remove(&SessionId::new("session_0"));
        f.snapshot = f
            .store
            .apply(
                WorkspaceMutationParams {
                    expected_revision: f.snapshot.revision,
                    mutation: CreateTab {
                        select: true,
                        workspace_id: workspace.clone(),
                        session_id: SessionId::new("session_0"),
                        title: None,
                    },
                },
                &captured,
            )
            .unwrap();
        assert_eq!(f.store.snapshot().unwrap(), f.snapshot);
        f.apply(RenameWorkspace {
            workspace_id: workspace.clone(),
            name: "Unavailable work is retained".into(),
        });
        f.reject(
            CreateTab {
                select: true,
                workspace_id: workspace,
                session_id: SessionId::new("session_0"),
                title: None,
            },
            "workspace_session_unavailable",
        );
        assert!(!f.sessions.contains(&SessionId::new("session_0")));
        assert_eq!(f.snapshot.workspaces[0].tabs.len(), 1);
    }

    #[test]
    fn unsupported_corrupt_and_unwritable_state_never_report_success() {
        let mut f = Fixture::new();
        f.create_workspace("Work");
        let mut document: Value = serde_json::from_slice(&std::fs::read(&f.path).unwrap()).unwrap();
        document[KEY]["schemaVersion"] = Value::from(999);
        std::fs::write(&f.path, serde_json::to_vec(&document).unwrap()).unwrap();
        f.reject(
            CreateWorkspace { name: "No".into() },
            "workspace_schema_unsupported",
        );
        std::fs::write(&f.path, b"{broken").unwrap();
        f.reject(
            CreateWorkspace { name: "No".into() },
            "workspace_storage_unavailable",
        );
        let directory = f._temp.path().join("directory.json");
        std::fs::create_dir(&directory).unwrap();
        let error = WorkspaceStore::new(directory)
            .apply(
                WorkspaceMutationParams {
                    expected_revision: 0,
                    mutation: CreateWorkspace { name: "No".into() },
                },
                &f.sessions,
            )
            .unwrap_err();
        assert_eq!(error.code, "workspace_storage_unavailable");
    }

    #[test]
    fn pane_limits_and_invalid_ratio_leave_the_prior_layout_unchanged() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Work");
        let (tab, pane) = f.create_tab(workspace, 0);
        for i in 1..MAX_TAB_PANES {
            f.apply(SplitPane {
                tab_id: tab.clone(),
                target: pane.clone(),
                session_id: SessionId::new(format!("session_{i}")),
                edge: DockEdge::Right,
            });
        }
        f.reject(
            SplitPane {
                tab_id: tab.clone(),
                target: pane,
                session_id: SessionId::new("session_8"),
                edge: DockEdge::Right,
            },
            "invalid_workspace",
        );
        let split = match &f.snapshot.workspaces[0].tabs[0].layout {
            LayoutNode::Split { id, .. } => id.clone(),
            _ => panic!(),
        };
        for fraction in [f32::NAN, f32::INFINITY, 0.0, 1.0] {
            f.reject(
                ResizeSplit {
                    tab_id: tab.clone(),
                    split_id: split.clone(),
                    fraction,
                },
                "invalid_workspace",
            );
        }
    }
}
