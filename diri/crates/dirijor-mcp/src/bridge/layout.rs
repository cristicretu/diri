//! Split panes for agents: place a new session beside a pane at spawn, move
//! an existing session's pane, unsplit, focus, zoom, equalize, and read a
//! tab's layout. The agent deals only in session ids; tab and pane ids are
//! resolved here from the Engine's workspace snapshot, and every mutation
//! carries that snapshot's revision (refetched on a revision conflict).

use super::*;
use diri_proto::workspace::{
    DockEdge, LayoutAxis, LayoutNode, LayoutNodeId, PaneId, SpawnPane, TabId, WorkspaceId,
    WorkspaceMutation, WorkspaceMutationParams, WorkspaceSnapshot, WorkspaceTab,
};

/// Attempts per mutation when another client changed the layout between our
/// snapshot read and the write.
const MUTATION_ATTEMPTS: usize = 3;
const REVISION_CONFLICT: &str = "workspace_revision_conflict";

/// Where a session is shown: its tab (and that tab's workspace) and pane.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub workspace_id: WorkspaceId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
}

pub fn edge(value: &str) -> Result<DockEdge, String> {
    match value {
        "right" => Ok(DockEdge::Right),
        "down" => Ok(DockEdge::Bottom),
        "left" => Ok(DockEdge::Left),
        "up" => Ok(DockEdge::Top),
        other => Err(format!(
            "split must be right, down, left, or up, not {other}"
        )),
    }
}

fn panes_of(node: &LayoutNode, session: &str, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Pane { id, session_id } => {
            if session_id.0 == session {
                out.push(id.clone());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            panes_of(first, session, out);
            panes_of(second, session, out);
        }
    }
}

fn pane_count(node: &LayoutNode) -> usize {
    match node {
        LayoutNode::Pane { .. } => 1,
        LayoutNode::Split { first, second, .. } => pane_count(first) + pane_count(second),
    }
}

fn pane_session(node: &LayoutNode, pane: &PaneId) -> Option<String> {
    match node {
        LayoutNode::Pane { id, session_id } => (id == pane).then(|| session_id.0.clone()),
        LayoutNode::Split { first, second, .. } => {
            pane_session(first, pane).or_else(|| pane_session(second, pane))
        }
    }
}

/// Every pane that shows `session`, the selected tab of a workspace first
/// (the same preference the Engine uses to dock a pane shell by its parent).
pub fn placements(snapshot: &WorkspaceSnapshot, session: &str) -> Vec<Placement> {
    let mut found: Vec<(bool, Placement)> = Vec::new();
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            let mut panes = Vec::new();
            panes_of(&tab.layout, session, &mut panes);
            let selected = workspace.selected_tab.as_ref() == Some(&tab.id);
            found.extend(panes.into_iter().map(|pane_id| {
                (
                    !selected,
                    Placement {
                        workspace_id: workspace.id.clone(),
                        tab_id: tab.id.clone(),
                        pane_id,
                    },
                )
            }));
        }
    }
    found.sort_by_key(|(unselected, _)| *unselected);
    found.into_iter().map(|(_, placement)| placement).collect()
}

pub fn locate(snapshot: &WorkspaceSnapshot, session: &str) -> Result<Placement, String> {
    placements(snapshot, session).into_iter().next().ok_or_else(|| {
        format!(
            "session {session} is not shown in any tab; open it in Diri first, or move it beside a shown session with manage_layout split"
        )
    })
}

fn find_tab<'a>(snapshot: &'a WorkspaceSnapshot, tab: &TabId) -> Option<&'a WorkspaceTab> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find(|candidate| &candidate.id == tab)
}

/// The mutation `manage_layout split` sends: move the session's own pane
/// when it is shown somewhere (preferring the anchor's tab), else add a pane.
pub fn split_mutation(
    snapshot: &WorkspaceSnapshot,
    session: &str,
    anchor: &str,
    edge: DockEdge,
) -> Result<WorkspaceMutation, String> {
    let target = locate(snapshot, anchor)?;
    let existing = placements(snapshot, session);
    let source = existing
        .iter()
        .find(|placement| placement.tab_id == target.tab_id)
        .or_else(|| existing.first());
    Ok(match source {
        Some(source) => WorkspaceMutation::MoveNode {
            source_tab: source.tab_id.clone(),
            node: LayoutNodeId::Pane(source.pane_id.clone()),
            destination_tab: target.tab_id,
            target: target.pane_id,
            edge,
        },
        None => WorkspaceMutation::SplitPane {
            tab_id: target.tab_id,
            target: target.pane_id,
            session_id: SessionId::new(session),
            edge,
        },
    })
}

/// An agent-readable tab: the split tree in session ids, with titles.
pub fn tab_view(snapshot: &WorkspaceSnapshot, tab_id: &TabId, sessions: &[SessionRecord]) -> Value {
    let Some((workspace, tab)) = snapshot.workspaces.iter().find_map(|workspace| {
        workspace
            .tabs
            .iter()
            .find(|tab| &tab.id == tab_id)
            .map(|tab| (workspace, tab))
    }) else {
        return Value::Null;
    };
    fn view_node(
        node: &LayoutNode,
        tab: &WorkspaceTab,
        sessions: &[SessionRecord],
        listed: &mut Vec<String>,
    ) -> Value {
        match node {
            LayoutNode::Pane { id, session_id } => {
                listed.push(session_id.0.clone());
                let record = sessions.iter().find(|record| record.id == *session_id);
                json!({
                    "session_id": session_id.0,
                    "title": record.map(|record| record.title.clone()),
                    "kind": record.map(|record| record.kind.id().to_owned()),
                    "status": record.map(|record| status_label(&record.status)),
                    "focused": &tab.focused_pane == id,
                    "zoomed": tab.zoomed_pane.as_ref() == Some(id),
                })
            }
            LayoutNode::Split {
                axis,
                fraction,
                first,
                second,
                ..
            } => json!({
                "split": match axis {
                    LayoutAxis::Horizontal => "side_by_side",
                    LayoutAxis::Vertical => "stacked",
                },
                "fraction": (f64::from(*fraction) * 100.0).round() / 100.0,
                "first": view_node(first, tab, sessions, listed),
                "second": view_node(second, tab, sessions, listed),
            }),
        }
    }
    let mut listed = Vec::new();
    let layout = view_node(&tab.layout, tab, sessions, &mut listed);
    json!({
        "tab_id": tab.id.0,
        "workspace": workspace.name,
        "title": tab.title,
        "selected": workspace.selected_tab.as_ref() == Some(&tab.id),
        "pane_count": pane_count(&tab.layout),
        "sessions": listed,
        "focused_session": pane_session(&tab.layout, &tab.focused_pane),
        "zoomed_session": tab.zoomed_pane.as_ref().and_then(|pane| pane_session(&tab.layout, pane)),
        "layout": layout,
    })
}

impl Bridge {
    pub(super) fn workspace_snapshot(&self) -> Result<WorkspaceSnapshot, String> {
        self.request_typed(Method::WORKSPACE_SNAPSHOT, json!({}), DEFAULT_TIMEOUT)
    }

    /// Applies the mutation `build` derives from a fresh snapshot, refetching
    /// and rebuilding when another client wrote in between.
    pub(super) fn mutate_layout(
        &self,
        mut build: impl FnMut(&WorkspaceSnapshot) -> Result<WorkspaceMutation, String>,
    ) -> Result<WorkspaceSnapshot, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let snapshot = self.workspace_snapshot()?;
            let params = WorkspaceMutationParams {
                expected_revision: snapshot.revision,
                mutation: build(&snapshot)?,
            };
            let params = serde_json::to_value(params).map_err(|error| error.to_string())?;
            match self.request_failure(Method::WORKSPACE_MUTATE, params, DEFAULT_TIMEOUT) {
                Ok(value) => {
                    return serde_json::from_value(value)
                        .map_err(|error| format!("invalid workspace snapshot: {error}"));
                }
                Err(ControlFailure::Daemon(error))
                    if error.code == REVISION_CONFLICT && attempt < MUTATION_ATTEMPTS => {}
                Err(error) => return Err(render_failure(error)),
            }
        }
    }

    /// Resolves `split`/`beside_session_id` of a spawn entry into the
    /// Engine's `pane` placement. `None` when the entry asks for no split.
    pub(super) fn spawn_placement(
        &self,
        arguments: &Value,
        tracked: bool,
    ) -> Result<Option<(SpawnPane, String)>, String> {
        let split = optional_string(arguments, "split");
        let beside = optional_string(arguments, "beside_session_id");
        if split.is_none() && beside.is_none() {
            return Ok(None);
        }
        let edge = edge(split.as_deref().unwrap_or("right"))?;
        let anchor = beside
            .or_else(|| self.caller.clone())
            .ok_or("split needs beside_session_id outside a Diri session")?;
        if tracked {
            let snapshot = self.snapshot()?;
            McpPolicy::new(
                &snapshot.sessions,
                &snapshot.projects,
                self.caller.as_deref(),
            )?
            .authorize(WriteAction::Layout {
                target: &anchor,
                anchor: true,
            })?;
        }
        let target = locate(&self.workspace_snapshot()?, &anchor)?;
        Ok(Some((
            SpawnPane::Beside {
                tab_id: target.tab_id,
                target: target.pane_id,
                edge,
            },
            anchor,
        )))
    }

    /// What a spawn result reports about the pane it asked for.
    pub(super) fn spawned_layout(&self, session: Option<&str>, anchor: &str) -> Value {
        let Some(session) = session else {
            return json!({"placed": false});
        };
        let Ok(snapshot) = self.workspace_snapshot() else {
            return json!({"placed": false, "note": "the layout could not be read"});
        };
        match placements(&snapshot, session).into_iter().next() {
            Some(placement) => {
                json!({"placed": true, "tab_id": placement.tab_id.0, "beside_session_id": anchor})
            }
            None => json!({
                "placed": false,
                "note": "the session started but was not docked (the tab already has 8 panes, or the target pane moved); place it with manage_layout split",
            }),
        }
    }

    pub(super) fn get_layout(&self, arguments: &Value) -> Result<Value, String> {
        let session = optional_string(arguments, "session_id")
            .or_else(|| self.caller.clone())
            .ok_or("session_id is required outside a Diri session")?;
        let snapshot = self.workspace_snapshot()?;
        let sessions = self.sessions()?;
        let mut tabs: Vec<TabId> = Vec::new();
        for placement in placements(&snapshot, &session) {
            if !tabs.contains(&placement.tab_id) {
                tabs.push(placement.tab_id);
            }
        }
        Ok(json!({
            "session_id": session,
            "placed": !tabs.is_empty(),
            "tabs": tabs.iter().map(|tab| tab_view(&snapshot, tab, &sessions)).collect::<Vec<_>>(),
        }))
    }

    pub(super) fn manage_layout(&self, arguments: &Value) -> Result<Value, String> {
        let caller = self.require_caller()?.to_owned();
        let action = required_string(arguments, "action")?;
        let session = optional_string(arguments, "session_id").unwrap_or_else(|| caller.clone());
        let records = self.snapshot()?;
        let policy = McpPolicy::new(&records.sessions, &records.projects, Some(&caller))?;
        policy.authorize(WriteAction::Layout {
            target: &session,
            anchor: false,
        })?;
        let shown_tab: TabId;
        let snapshot = match action.as_str() {
            "split" => {
                let anchor =
                    optional_string(arguments, "beside_session_id").unwrap_or(caller.clone());
                if anchor == session {
                    return Err(
                        "split moves session_id beside beside_session_id; name two different sessions"
                            .into(),
                    );
                }
                policy.authorize(WriteAction::Layout {
                    target: &anchor,
                    anchor: true,
                })?;
                let edge = edge(
                    optional_string(arguments, "split")
                        .as_deref()
                        .unwrap_or("right"),
                )?;
                let snapshot = self
                    .mutate_layout(|snapshot| split_mutation(snapshot, &session, &anchor, edge))?;
                shown_tab = locate(&snapshot, &anchor)?.tab_id;
                snapshot
            }
            "unsplit" => {
                let mut workspace = None;
                self.mutate_layout(|snapshot| {
                    let placement = locate(snapshot, &session)?;
                    let tab = find_tab(snapshot, &placement.tab_id).ok_or("tab vanished")?;
                    if pane_count(&tab.layout) < 2 {
                        return Err(format!("{session} is already alone in its tab"));
                    }
                    workspace = Some(placement.workspace_id.clone());
                    Ok(WorkspaceMutation::RemovePane {
                        tab_id: placement.tab_id,
                        pane_id: placement.pane_id,
                    })
                })?;
                let workspace = workspace.ok_or("unsplit resolved no workspace")?;
                let snapshot = self
                    .mutate_layout(|_| {
                        Ok(WorkspaceMutation::CreateTab {
                            select: false,
                            workspace_id: workspace.clone(),
                            session_id: SessionId::new(&session),
                            title: None,
                        })
                    })
                    .map_err(|error| {
                        format!(
                            "{session} left the split but could not get its own tab ({error}); it keeps running and opens again from the sidebar"
                        )
                    })?;
                shown_tab = locate(&snapshot, &session)?.tab_id;
                snapshot
            }
            "close" => {
                let mut tab = None;
                let snapshot = self.mutate_layout(|snapshot| {
                    let placement = locate(snapshot, &session)?;
                    tab = Some(placement.tab_id.clone());
                    Ok(WorkspaceMutation::RemovePane {
                        tab_id: placement.tab_id,
                        pane_id: placement.pane_id,
                    })
                })?;
                let tab = tab.ok_or("close resolved no tab")?;
                let sessions = self.sessions()?;
                return Ok(json!({
                    "ok": true,
                    "revision": snapshot.revision,
                    "note": "the pane closed; the session keeps running",
                    "tab": tab_view(&snapshot, &tab, &sessions),
                }));
            }
            "focus" | "zoom" | "unzoom" | "equalize" => {
                let mut tab = None;
                let snapshot = self.mutate_layout(|snapshot| {
                    let placement = locate(snapshot, &session)?;
                    tab = Some(placement.tab_id.clone());
                    Ok(match action.as_str() {
                        "focus" => WorkspaceMutation::FocusPane {
                            tab_id: placement.tab_id,
                            pane_id: placement.pane_id,
                        },
                        "zoom" => WorkspaceMutation::ZoomPane {
                            tab_id: placement.tab_id,
                            pane_id: Some(placement.pane_id),
                        },
                        "unzoom" => WorkspaceMutation::ZoomPane {
                            tab_id: placement.tab_id,
                            pane_id: None,
                        },
                        _ => WorkspaceMutation::EqualizeTab {
                            tab_id: placement.tab_id,
                        },
                    })
                })?;
                shown_tab = tab.ok_or("layout resolved no tab")?;
                snapshot
            }
            other => {
                return Err(format!(
                    "action must be split, unsplit, close, focus, zoom, unzoom, or equalize, not {other}"
                ));
            }
        };
        let sessions = self.sessions()?;
        Ok(json!({
            "ok": true,
            "revision": snapshot.revision,
            "tab": tab_view(&snapshot, &shown_tab, &sessions),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{SplitId, WorkspaceRecord};

    fn pane(id: &str, session: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(session),
        }
    }

    fn fixture() -> WorkspaceSnapshot {
        let tab = |id: &str, layout: LayoutNode, focused: &str| WorkspaceTab {
            id: TabId::new(id),
            title: None,
            layout,
            focused_pane: PaneId::new(focused),
            zoomed_pane: None,
        };
        WorkspaceSnapshot {
            revision: 4,
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: WorkspaceId::new("w"),
                name: "API".into(),
                tabs: vec![
                    tab("t_child", pane("p_child_own", "child"), "p_child_own"),
                    tab(
                        "t_parent",
                        LayoutNode::Split {
                            id: SplitId::new("s"),
                            axis: LayoutAxis::Vertical,
                            fraction: 0.6,
                            first: Box::new(pane("p_parent", "parent")),
                            second: Box::new(pane("p_shell", "shell")),
                        },
                        "p_parent",
                    ),
                ],
                selected_tab: Some(TabId::new("t_parent")),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn placements_prefer_the_selected_tab() {
        let mut snapshot = fixture();
        snapshot.workspaces[0].tabs[0].layout = pane("p_parent_copy", "parent");
        let found = placements(&snapshot, "parent");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].tab_id, TabId::new("t_parent"));
        assert_eq!(found[0].pane_id, PaneId::new("p_parent"));
        assert!(locate(&snapshot, "nobody").is_err());
    }

    #[test]
    fn split_moves_a_shown_pane_and_adds_an_unshown_one() {
        let snapshot = fixture();
        assert_eq!(
            split_mutation(&snapshot, "child", "parent", DockEdge::Right).unwrap(),
            WorkspaceMutation::MoveNode {
                source_tab: TabId::new("t_child"),
                node: LayoutNodeId::Pane(PaneId::new("p_child_own")),
                destination_tab: TabId::new("t_parent"),
                target: PaneId::new("p_parent"),
                edge: DockEdge::Right,
            }
        );
        // Already in the anchor's tab: rearranged within it.
        assert_eq!(
            split_mutation(&snapshot, "shell", "parent", DockEdge::Left).unwrap(),
            WorkspaceMutation::MoveNode {
                source_tab: TabId::new("t_parent"),
                node: LayoutNodeId::Pane(PaneId::new("p_shell")),
                destination_tab: TabId::new("t_parent"),
                target: PaneId::new("p_parent"),
                edge: DockEdge::Left,
            }
        );
        assert_eq!(
            split_mutation(&snapshot, "fresh", "parent", DockEdge::Bottom).unwrap(),
            WorkspaceMutation::SplitPane {
                tab_id: TabId::new("t_parent"),
                target: PaneId::new("p_parent"),
                session_id: SessionId::new("fresh"),
                edge: DockEdge::Bottom,
            }
        );
        assert!(split_mutation(&snapshot, "child", "nobody", DockEdge::Right).is_err());
    }

    #[test]
    fn tab_view_speaks_in_session_ids() {
        let snapshot = fixture();
        let view = tab_view(&snapshot, &TabId::new("t_parent"), &[]);
        assert_eq!(view["pane_count"], 2);
        assert_eq!(view["sessions"], json!(["parent", "shell"]));
        assert_eq!(view["focused_session"], "parent");
        assert_eq!(view["zoomed_session"], Value::Null);
        assert_eq!(view["selected"], true);
        assert_eq!(view["layout"]["split"], "stacked");
        assert_eq!(view["layout"]["fraction"], 0.6);
        assert_eq!(view["layout"]["first"]["focused"], true);
        assert_eq!(view["layout"]["second"]["session_id"], "shell");
        assert_eq!(edge("down").unwrap(), DockEdge::Bottom);
        assert!(edge("sideways").is_err());
    }
}
