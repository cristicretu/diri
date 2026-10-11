//! Durable organization intent behind one revisioned interface. This module
//! never launches, resizes, attaches to, or terminates a session.
#[cfg(test)]
mod project_agent_tests;
mod tree;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use diri_proto::workspace::*;
use diri_proto::{ControlError, Project, ProjectId, SessionId};
use serde_json::value::RawValue;

use crate::state_file::JsonStateFile;

const KEY: &str = "workspaceState";
/// Marker: pre-split `⌘J` terminals were moved into their tabs.
const PANE_SHELLS_KEY: &str = "workspacePaneShells";

/// Resolved under Registry once, before the workspace file transaction. The
/// client supplies only a session identity, never a project name or location.
pub(crate) struct ProjectAgentInventory {
    pub session_id: SessionId,
    pub project: Project,
    pub session_projects: HashMap<SessionId, ProjectId>,
}
pub struct WorkspaceStore {
    file: JsonStateFile,
}

impl WorkspaceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: JsonStateFile::new(path),
        }
    }

    /// Shares an existing handle (the Registry's) and so its cached image.
    pub(crate) fn with_state_file(file: JsonStateFile) -> Self {
        Self { file }
    }

    pub fn snapshot(&self) -> Result<WorkspaceSnapshot, ControlError> {
        decode(
            self.file
                .read_section(KEY)
                .map_err(storage_error)?
                .as_deref(),
        )
    }

    /// Revision comparison, validation, and atomic replacement happen under
    /// JsonStateFile's one advisory lock. Rejected edits leave the file intact. A storage failure requires a fresh
    /// snapshot because a rename may have committed before its final sync failed.
    pub fn apply(
        &self,
        params: WorkspaceMutationParams,
        sessions: &HashSet<SessionId>,
    ) -> Result<WorkspaceSnapshot, ControlError> {
        self.apply_with_project_agent(params, sessions, None)
    }

    pub(crate) fn apply_with_project_agent(
        &self,
        params: WorkspaceMutationParams,
        sessions: &HashSet<SessionId>,
        project_agent: Option<&ProjectAgentInventory>,
    ) -> Result<WorkspaceSnapshot, ControlError> {
        self.transact(|next| {
            if next.revision != params.expected_revision {
                return Err(ControlError::new(
                    "workspace_revision_conflict",
                    format!(
                        "expected revision {}, current revision {}",
                        params.expected_revision, next.revision
                    ),
                ));
            }
            mutate(next, params.mutation, sessions, project_agent)?;
            reclaim_unavailable_tabs(next, sessions);
            Ok(true)
        })
        .map(|(snapshot, _)| snapshot)
    }

    /// Records where a session that just spawned goes. Never an error the
    /// spawn has to answer for: a parent that is placed nowhere, a full tab,
    /// or a target that moved leaves the session unplaced (`⌘J` brings a
    /// pane shell back). Returns the new snapshot when the layout changed.
    pub(crate) fn place_spawned(
        &self,
        session: &SessionId,
        parent: Option<&SessionId>,
        placement: &SpawnPane,
    ) -> Option<WorkspaceSnapshot> {
        let result = self.transact(|next| {
            Ok(place(next, session, parent, placement).is_ok_and(|placed| placed))
        });
        match result {
            Ok((snapshot, true)) => Some(snapshot),
            _ => None,
        }
    }

    /// A removed session's panes leave every tab they share with another
    /// pane, so closing a pane collapses its split instead of leaving a
    /// "Session unavailable" hole. A one-pane tab is left to its owner.
    pub(crate) fn prune_session(&self, session: &SessionId) -> Option<WorkspaceSnapshot> {
        let result = self.transact(|next| {
            let mut changed = false;
            for tab in next.workspaces.iter_mut().flat_map(|w| &mut w.tabs) {
                changed |= tree::prune(&mut tab.layout, session);
            }
            Ok(changed)
        });
        match result {
            Ok((snapshot, true)) => Some(snapshot),
            _ => None,
        }
    }

    /// One-time move of pre-split `⌘J` terminals into their parent's tab:
    /// `children` pairs each live pane shell with its parent, newest first.
    /// The newest unplaced shell of each placed parent docks below it. A
    /// marker beside the layout makes this run once; later `⌘J` toggles
    /// decide on their own what is shown.
    pub(crate) fn adopt_pane_shells(
        &self,
        children: &[(SessionId, SessionId)],
    ) -> Option<WorkspaceSnapshot> {
        let mut changed = None;
        let written = self.file.update_durable(|document| {
            if document.get(PANE_SHELLS_KEY).is_some() {
                return Err(std::io::Error::other("already adopted"));
            }
            let mut next =
                decode(document.get(KEY)).map_err(|_| std::io::Error::other("corrupt"))?;
            let mut adopted = HashSet::new();
            let mut any = false;
            for (child, parent) in children {
                if adopted.contains(parent) {
                    continue;
                }
                let before = next.clone();
                match place(&mut next, child, Some(parent), &SpawnPane::BelowParent) {
                    Ok(true) => {
                        tree::repair(&mut next);
                        if validate(&next).is_ok() {
                            adopted.insert(parent.clone());
                            any = true;
                        } else {
                            next = before;
                        }
                    }
                    _ => next = before,
                }
            }
            if any {
                next.revision = next
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("revision"))?;
                let encoded = serde_json::to_value(&next).map_err(std::io::Error::other)?;
                document.insert(KEY, &encoded)?;
                changed = Some(next);
            }
            document.insert(PANE_SHELLS_KEY, &1u32)?;
            Ok(())
        });
        written.ok().and(changed)
    }

    /// Validation, revision bump, and atomic replacement under one lock.
    /// `edit` returns whether it changed anything; an unchanged layout is
    /// not rewritten and keeps its revision.
    fn transact(
        &self,
        edit: impl FnOnce(&mut WorkspaceSnapshot) -> Result<bool, ControlError>,
    ) -> Result<(WorkspaceSnapshot, bool), ControlError> {
        let mut outcome = None;
        let written = self.file.update_durable(|document| {
            let result = (|| {
                let mut next = decode(document.get(KEY))?;
                if !edit(&mut next)? {
                    return Ok((next, false));
                }
                tree::repair(&mut next);
                validate(&next)?;
                next.revision = next
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("revision exhausted"))?;
                // Through a JSON value, exactly as replies are encoded, so the
                // stored text matches what clients were told: `f32` fractions
                // print differently when serialized directly.
                let encoded = serde_json::to_value(&next)
                    .map_err(|_| invalid("cannot encode workspace state"))?;
                document
                    .insert(KEY, &encoded)
                    .map_err(|_| invalid("cannot encode workspace state"))?;
                Ok((next, true))
            })();
            // An unchanged document is not rewritten (`Document::changed`).
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
            Some(Ok((snapshot, changed))) => {
                written.map_err(storage_error)?;
                Ok((snapshot, changed))
            }
            None => Err(storage_error(written.unwrap_err())),
        }
    }
}

/// Docks `session` per `placement`. `Ok(false)`: nothing to do (already
/// placed, detached, or the parent is placed nowhere).
fn place(
    state: &mut WorkspaceSnapshot,
    session: &SessionId,
    parent: Option<&SessionId>,
    placement: &SpawnPane,
) -> Result<bool, ControlError> {
    if state
        .workspaces
        .iter()
        .flat_map(|w| &w.tabs)
        .any(|tab| !tree::panes_of(&tab.layout, session).is_empty())
    {
        return Ok(false);
    }
    let (tab_id, target, edge) = match placement {
        SpawnPane::Detached => return Ok(false),
        SpawnPane::Beside {
            tab_id,
            target,
            edge,
        } => (tab_id.clone(), target.clone(), *edge),
        SpawnPane::BelowParent => {
            let Some(parent) = parent else {
                return Ok(false);
            };
            // The parent's own tab first: the selected tab of a workspace
            // that shows it, else the first tab that places it.
            let found = state
                .workspaces
                .iter()
                .flat_map(|w| {
                    w.tabs
                        .iter()
                        .map(move |tab| (w.selected_tab.as_ref() == Some(&tab.id), tab))
                })
                .filter_map(|(selected, tab)| {
                    let pane = tree::panes_of(&tab.layout, parent).into_iter().next()?;
                    Some((!selected, tab.id.clone(), pane))
                })
                .min_by_key(|(unselected, _, _)| *unselected);
            let Some((_, tab_id, pane)) = found else {
                return Ok(false);
            };
            (tab_id, pane, DockEdge::Bottom)
        }
    };
    let tab = tab(state, &tab_id)?;
    if tree::panes(&tab.layout).len() >= MAX_TAB_PANES {
        return Ok(false);
    }
    let pane_id = PaneId(new_id("pane")?);
    let incoming = LayoutNode::Pane {
        id: pane_id.clone(),
        session_id: session.clone(),
    };
    if !tree::dock(
        &mut tab.layout,
        &target,
        &incoming,
        edge,
        &SplitId(new_id("split")?),
    ) {
        return Ok(false);
    }
    tab.focused_pane = pane_id;
    // A new pane shows: zoom on another pane would hide it.
    tab.zoomed_pane = None;
    Ok(true)
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
fn decode(section: Option<&RawValue>) -> Result<WorkspaceSnapshot, ControlError> {
    let Some(section) = section else {
        return Ok(WorkspaceSnapshot::default());
    };
    let state: WorkspaceSnapshot = serde_json::from_str(section.get())
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
        return Err(ControlError::new(
            "workspace_limit_reached",
            "workspace or tab count exceeds the limit",
        ));
    }
    let mut seen = HashSet::new();
    let mut project_ids = HashSet::new();
    for workspace in &state.workspaces {
        if let Some(project) = &workspace.project_id
            && (project.0.is_empty() || project.0.len() > 128 || !project_ids.insert(project))
        {
            return Err(invalid("invalid or duplicate project workspace binding"));
        }
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
/// A placement outlives its deleted session and stays visible as unavailable.
/// Nothing removes those tabs, so a long-used install fills the tab limit with
/// them and then rejects every new placement (opening an agent, or placing a
/// spawned child). Only when an edit would exceed the limit are the tabs whose
/// every pane is unavailable dropped; this runs after the edit, so it can still
/// target them. A split that keeps one live pane is never dropped.
///
/// The inventory was read before the file lock, but the revision check makes
/// that sound: a session created after the read can only be placed by an edit
/// that commits first, and that commit rejects this edit's expected revision.
fn reclaim_unavailable_tabs(state: &mut WorkspaceSnapshot, sessions: &HashSet<SessionId>) {
    fn available(node: &LayoutNode, sessions: &HashSet<SessionId>) -> bool {
        match node {
            LayoutNode::Pane { session_id, .. } => sessions.contains(session_id),
            LayoutNode::Split { first, second, .. } => {
                available(first, sessions) || available(second, sessions)
            }
        }
    }
    if state.workspaces.iter().map(|w| w.tabs.len()).sum::<usize>() <= MAX_WORKSPACE_TABS {
        return;
    }
    for workspace in &mut state.workspaces {
        workspace
            .tabs
            .retain(|tab| available(&tab.layout, sessions));
    }
}
fn require_pane(tab: &WorkspaceTab, id: &PaneId) -> Result<(), ControlError> {
    if tree::find(&tab.layout, &LayoutNodeId::Pane(id.clone())).is_some() {
        Ok(())
    } else {
        Err(missing())
    }
}

fn matching_pane(tab: &WorkspaceTab, session: &SessionId) -> Option<PaneId> {
    fn find(node: &LayoutNode, session: &SessionId) -> Option<PaneId> {
        match node {
            LayoutNode::Pane { id, session_id } => (session_id == session).then(|| id.clone()),
            LayoutNode::Split { first, second, .. } => {
                find(first, session).or_else(|| find(second, session))
            }
        }
    }
    // Duplicate references are valid: retain the already focused occurrence.
    if let Some(LayoutNode::Pane { session_id, .. }) =
        tree::find(&tab.layout, &LayoutNodeId::Pane(tab.focused_pane.clone()))
        && session_id == session
    {
        return Some(tab.focused_pane.clone());
    }
    find(&tab.layout, session)
}

fn select_agent(workspace: &mut WorkspaceRecord, session: &SessionId) -> bool {
    let index = workspace
        .tabs
        .iter()
        .position(|tab| {
            Some(&tab.id) == workspace.selected_tab.as_ref()
                && matching_pane(tab, session).is_some()
        })
        .or_else(|| {
            workspace
                .tabs
                .iter()
                .position(|tab| matching_pane(tab, session).is_some())
        });
    let Some(index) = index else { return false };
    let tab = &mut workspace.tabs[index];
    let pane = matching_pane(tab, session).expect("matched agent pane");
    if tab
        .zoomed_pane
        .as_ref()
        .is_some_and(|zoomed| zoomed != &pane)
    {
        tab.zoomed_pane = None;
    }
    tab.focused_pane = pane;
    workspace.selected_tab = Some(tab.id.clone());
    true
}

fn belongs_to_project(node: &LayoutNode, inventory: &ProjectAgentInventory) -> bool {
    match node {
        LayoutNode::Pane { session_id, .. } => {
            inventory.session_projects.get(session_id) == Some(&inventory.project.id)
        }
        LayoutNode::Split { first, second, .. } => {
            belongs_to_project(first, inventory) && belongs_to_project(second, inventory)
        }
    }
}

fn open_project_agent(
    state: &mut WorkspaceSnapshot,
    session: &SessionId,
    preferred: Option<&WorkspaceId>,
    inventory: &ProjectAgentInventory,
) -> Result<(), ControlError> {
    // A preferred layout is a view, not evidence of project membership. Mixed
    // layouts remain intact and unbound, with only the requested focus changed.
    if let Some(preferred) = preferred
        && let Some(workspace) = state
            .workspaces
            .iter_mut()
            .find(|workspace| &workspace.id == preferred)
        && select_agent(workspace, session)
    {
        return Ok(());
    }
    let bound = state
        .workspaces
        .iter()
        .position(|workspace| workspace.project_id.as_ref() == Some(&inventory.project.id));
    let adopted = if bound.is_none() {
        let mut candidates = state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| {
                workspace.project_id.is_none()
                    && workspace
                        .tabs
                        .iter()
                        .any(|tab| matching_pane(tab, session).is_some())
                    && workspace
                        .tabs
                        .iter()
                        .all(|tab| belongs_to_project(&tab.layout, inventory))
            });
        let first = candidates.next().map(|(index, _)| index);
        if candidates.next().is_none() {
            first
        } else {
            None
        }
    } else {
        None
    };
    let index = if let Some(index) = bound.or(adopted) {
        // Preserve legacy names and every existing layout identity on adoption.
        state.workspaces[index].project_id = Some(inventory.project.id.clone());
        index
    } else {
        name(&inventory.project.name)?;
        state.workspaces.push(WorkspaceRecord {
            project_id: Some(inventory.project.id.clone()),
            id: WorkspaceId(new_id("workspace")?),
            name: inventory.project.name.clone(),
            tabs: Vec::new(),
            selected_tab: None,
        });
        state.workspaces.len() - 1
    };
    let workspace = &mut state.workspaces[index];
    if !select_agent(workspace, session) {
        let pane = PaneId(new_id("pane")?);
        let tab = TabId(new_id("tab")?);
        workspace.tabs.push(WorkspaceTab {
            id: tab.clone(),
            title: None,
            layout: LayoutNode::Pane {
                id: pane.clone(),
                session_id: session.clone(),
            },
            focused_pane: pane,
            zoomed_pane: None,
        });
        workspace.selected_tab = Some(tab);
    }
    Ok(())
}

fn mutate(
    state: &mut WorkspaceSnapshot,
    mutation: WorkspaceMutation,
    sessions: &HashSet<SessionId>,
    project_agent: Option<&ProjectAgentInventory>,
) -> Result<(), ControlError> {
    use WorkspaceMutation::*;
    match mutation {
        OpenProjectAgent {
            session_id,
            preferred_workspace,
        } => {
            existing(&session_id, sessions)?;
            let inventory = project_agent
                .filter(|inventory| inventory.session_id == session_id)
                .ok_or_else(|| {
                    ControlError::new(
                        "workspace_project_unavailable",
                        "the agent project is absent from the Engine inventory",
                    )
                })?;
            open_project_agent(state, &session_id, preferred_workspace.as_ref(), inventory)?;
        }
        CreateWorkspace { name: title } => {
            name(&title)?;
            state.workspaces.push(WorkspaceRecord {
                project_id: None,
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
        EqualizeTab { tab_id } => {
            tree::equalize(&mut tab(state, &tab_id)?.layout);
        }
        ApplyLayout { tab_id, preset } => {
            let tab = tab(state, &tab_id)?;
            let panes = tree::leaves(&tab.layout);
            let ids = (0..panes.len())
                .map(|_| new_id("split").map(SplitId))
                .collect::<Result<Vec<_>, _>>()?;
            tab.layout = tree::arrange(&panes, preset, &mut ids.into_iter());
            // A zoomed pane would hide the layout just asked for.
            tab.zoomed_pane = None;
        }
        BreakPane { tab_id, pane_id } => {
            let (workspace_index, tab_index) = state
                .workspaces
                .iter()
                .enumerate()
                .find_map(|(w, workspace)| {
                    workspace
                        .tabs
                        .iter()
                        .position(|tab| tab.id == tab_id)
                        .map(|t| (w, t))
                })
                .ok_or_else(missing)?;
            let source = &mut state.workspaces[workspace_index].tabs[tab_index];
            let (retained, removed) =
                tree::take(source.layout.clone(), &LayoutNodeId::Pane(pane_id.clone()));
            let removed = removed.ok_or_else(missing)?;
            // A one-pane tab is already its own tab.
            let retained = retained.ok_or_else(|| invalid("the tab has only this pane"))?;
            source.layout = retained;
            let broken = WorkspaceTab {
                id: TabId(new_id("tab")?),
                title: None,
                layout: removed,
                focused_pane: pane_id,
                zoomed_pane: None,
            };
            let workspace = &mut state.workspaces[workspace_index];
            workspace.selected_tab = Some(broken.id.clone());
            workspace.tabs.insert(tab_index + 1, broken);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use WorkspaceMutation::*;
    use serde_json::Value;

    pub(super) struct Fixture {
        _temp: tempfile::TempDir,
        pub(super) path: PathBuf,
        pub(super) store: WorkspaceStore,
        pub(super) sessions: HashSet<SessionId>,
        pub(super) snapshot: WorkspaceSnapshot,
    }
    impl Fixture {
        pub(super) fn new() -> Self {
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
        pub(super) fn apply(&mut self, mutation: WorkspaceMutation) {
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
        pub(super) fn create_workspace(&mut self, name: &str) -> WorkspaceId {
            self.apply(CreateWorkspace { name: name.into() });
            self.snapshot.workspaces.last().unwrap().id.clone()
        }
        pub(super) fn create_tab(
            &mut self,
            workspace_id: WorkspaceId,
            session: usize,
        ) -> (TabId, PaneId) {
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

    fn fractions(node: &LayoutNode, out: &mut Vec<f32>) {
        if let LayoutNode::Split {
            fraction,
            first,
            second,
            ..
        } = node
        {
            out.push(*fraction);
            fractions(first, out);
            fractions(second, out);
        }
    }

    fn split_at(
        f: &mut Fixture,
        tab: &TabId,
        target: &PaneId,
        session: usize,
        edge: DockEdge,
    ) -> PaneId {
        f.apply(SplitPane {
            tab_id: tab.clone(),
            target: target.clone(),
            session_id: SessionId::new(format!("session_{session}")),
            edge,
        });
        f.snapshot.workspaces[0].tabs[0].focused_pane.clone()
    }

    /// The tree's shape with pane ids and rounded fractions, for asserting
    /// a whole preset at once: `H(.4 a V(.5 b c))`.
    fn shape(node: &LayoutNode) -> String {
        match node {
            LayoutNode::Pane { session_id, .. } => {
                session_id.0.trim_start_matches("session_").into()
            }
            LayoutNode::Split {
                axis,
                fraction,
                first,
                second,
                ..
            } => format!(
                "{}({:.2} {} {})",
                if *axis == LayoutAxis::Horizontal {
                    "H"
                } else {
                    "V"
                },
                fraction,
                shape(first),
                shape(second)
            ),
        }
    }

    #[test]
    fn layout_presets_rearrange_the_same_panes_in_reading_order() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, a) = f.create_tab(workspace, 0);
        let b = split_at(&mut f, &tab, &a, 1, DockEdge::Bottom);
        let c = split_at(&mut f, &tab, &b, 2, DockEdge::Right);
        let d = split_at(&mut f, &tab, &c, 3, DockEdge::Bottom);
        split_at(&mut f, &tab, &d, 4, DockEdge::Right);
        f.apply(FocusPane {
            tab_id: tab.clone(),
            pane_id: c.clone(),
        });
        f.apply(ZoomPane {
            tab_id: tab.clone(),
            pane_id: Some(c.clone()),
        });
        let before = tree::panes(&f.snapshot.workspaces[0].tabs[0].layout);
        let expected = [
            (
                LayoutPreset::EvenHorizontal,
                "H(0.40 H(0.50 0 1) H(0.33 2 H(0.50 3 4)))",
            ),
            (
                LayoutPreset::EvenVertical,
                "V(0.40 V(0.50 0 1) V(0.33 2 V(0.50 3 4)))",
            ),
            (
                LayoutPreset::MainVertical,
                "H(0.60 0 V(0.50 V(0.50 1 2) V(0.50 3 4)))",
            ),
            (
                LayoutPreset::MainHorizontal,
                "V(0.60 0 H(0.50 H(0.50 1 2) H(0.50 3 4)))",
            ),
            (
                LayoutPreset::Tiled,
                "V(0.50 H(0.33 0 H(0.50 1 2)) H(0.50 3 4))",
            ),
        ];
        for (preset, want) in expected {
            f.apply(ApplyLayout {
                tab_id: tab.clone(),
                preset,
            });
            let saved = &f.snapshot.workspaces[0].tabs[0];
            assert_eq!(shape(&saved.layout), want, "{preset:?}");
            // Same panes, same order, same focus; the zoom gives way.
            assert_eq!(tree::panes(&saved.layout), before);
            assert_eq!(saved.focused_pane, c);
            assert_eq!(saved.zoomed_pane, None);
        }
        // `next` walks all five and wraps.
        let mut preset = LayoutPreset::EvenHorizontal;
        for _ in 0..5 {
            preset = preset.next();
        }
        assert_eq!(preset, LayoutPreset::EvenHorizontal);
    }

    #[test]
    fn every_preset_fits_the_pane_and_depth_limits() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, mut last) = f.create_tab(workspace, 0);
        for session in 1..MAX_TAB_PANES {
            last = split_at(&mut f, &tab, &last, session, DockEdge::Right);
        }
        for preset in LayoutPreset::ALL {
            f.apply(ApplyLayout {
                tab_id: tab.clone(),
                preset,
            });
            assert_eq!(
                tree::panes(&f.snapshot.workspaces[0].tabs[0].layout).len(),
                MAX_TAB_PANES
            );
        }
        // One pane: every preset is that pane.
        let (single, pane) = f.create_tab(f.snapshot.workspaces[0].id.clone(), 9);
        f.apply(ApplyLayout {
            tab_id: single,
            preset: LayoutPreset::Tiled,
        });
        let tab = f.snapshot.workspaces[0].tabs.last().unwrap();
        assert!(matches!(&tab.layout, LayoutNode::Pane { id, .. } if *id == pane));
    }

    #[test]
    fn break_pane_moves_one_pane_into_a_new_selected_tab_after_its_own() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (first, a) = f.create_tab(workspace.clone(), 0);
        let (other, _) = f.create_tab(workspace, 5);
        let b = split_at(&mut f, &first, &a, 1, DockEdge::Right);
        f.apply(SelectTab {
            workspace_id: f.snapshot.workspaces[0].id.clone(),
            tab_id: first.clone(),
        });
        f.apply(BreakPane {
            tab_id: first.clone(),
            pane_id: b.clone(),
        });
        let saved = &f.snapshot.workspaces[0];
        assert_eq!(saved.tabs.len(), 3);
        assert_eq!(saved.tabs[0].id, first);
        assert!(matches!(&saved.tabs[0].layout, LayoutNode::Pane { id, .. } if *id == a));
        // The source's focus falls back to what it kept.
        assert_eq!(saved.tabs[0].focused_pane, a);
        let broken = &saved.tabs[1];
        assert!(matches!(&broken.layout, LayoutNode::Pane { id, session_id }
            if *id == b && session_id.0 == "session_1"));
        assert_eq!(broken.focused_pane, b);
        assert_eq!(saved.selected_tab.as_ref(), Some(&broken.id));
        assert_eq!(saved.tabs[2].id, other);

        // Joining it back is a move of that pane onto another tab's pane.
        let broken = broken.id.clone();
        f.apply(MoveNode {
            source_tab: broken,
            node: LayoutNodeId::Pane(b.clone()),
            destination_tab: first.clone(),
            target: a.clone(),
            edge: DockEdge::Bottom,
        });
        let saved = &f.snapshot.workspaces[0];
        assert_eq!(saved.tabs.len(), 2, "the emptied tab closes");
        assert_eq!(tree::panes(&saved.tabs[0].layout), vec![a.clone(), b]);

        // A one-pane tab has nothing to break away from.
        f.reject(
            BreakPane {
                tab_id: other,
                pane_id: saved.tabs[1].focused_pane.clone(),
            },
            "invalid_workspace",
        );
        f.reject(
            BreakPane {
                tab_id: first,
                pane_id: PaneId::new("missing"),
            },
            "workspace_target_not_found",
        );
    }

    #[test]
    fn equalize_gives_each_column_and_row_an_equal_share() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, a) = f.create_tab(workspace, 0);
        // A | B | C across, then C split down into C / D.
        let b = split_at(&mut f, &tab, &a, 1, DockEdge::Right);
        let c = split_at(&mut f, &tab, &b, 2, DockEdge::Right);
        split_at(&mut f, &tab, &c, 3, DockEdge::Bottom);
        let split = |f: &Fixture| match &f.snapshot.workspaces[0].tabs[0].layout {
            LayoutNode::Split { id, .. } => id.clone(),
            _ => panic!(),
        };
        f.apply(ResizeSplit {
            tab_id: tab.clone(),
            split_id: split(&f),
            fraction: 0.8,
        });
        f.apply(EqualizeTab {
            tab_id: tab.clone(),
        });
        let mut found = Vec::new();
        fractions(&f.snapshot.workspaces[0].tabs[0].layout, &mut found);
        // Root: A against (B | C/D), one column of three. Then B against
        // C/D, one of two; the C/D stack splits its column in half.
        assert_eq!(found.len(), 3);
        assert!((found[0] - 1.0 / 3.0).abs() < 1e-6, "{found:?}");
        assert!((found[1] - 0.5).abs() < 1e-6, "{found:?}");
        assert!((found[2] - 0.5).abs() < 1e-6, "{found:?}");
    }

    #[test]
    fn equalize_stays_inside_the_ratio_clamp_at_the_pane_limit() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, mut last) = f.create_tab(workspace, 0);
        for session in 1..MAX_TAB_PANES {
            last = split_at(&mut f, &tab, &last, session, DockEdge::Right);
        }
        f.apply(EqualizeTab { tab_id: tab });
        let mut found = Vec::new();
        fractions(&f.snapshot.workspaces[0].tabs[0].layout, &mut found);
        assert!(found.iter().all(|fraction| (0.1..=0.9).contains(fraction)));
        // Eight columns: the first is one of eight, the next one of seven…
        assert!((found[0] - 1.0 / 8.0).abs() < 1e-6, "{found:?}");
        assert!((found[6] - 0.5).abs() < 1e-6, "{found:?}");
    }

    #[test]
    fn spawned_pane_docks_beside_its_target_or_below_its_parent() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, owner) = f.create_tab(workspace, 0);
        let parent = SessionId::new("session_0");
        // A split: right of the focused pane, and focused.
        let placed = f
            .store
            .place_spawned(
                &SessionId::new("session_1"),
                Some(&parent),
                &SpawnPane::Beside {
                    tab_id: tab.clone(),
                    target: owner.clone(),
                    edge: DockEdge::Right,
                },
            )
            .expect("placed");
        let layout = &placed.workspaces[0].tabs[0].layout;
        let LayoutNode::Split { axis, second, .. } = layout else {
            panic!("split expected: {layout:?}");
        };
        assert_eq!(*axis, LayoutAxis::Horizontal);
        assert!(
            matches!(&**second, LayoutNode::Pane { session_id, .. } if session_id.0 == "session_1")
        );
        assert_eq!(
            tree::panes_of(layout, &SessionId::new("session_1")),
            vec![placed.workspaces[0].tabs[0].focused_pane.clone()]
        );
        // An MCP child shell: below the parent's pane.
        let placed = f
            .store
            .place_spawned(
                &SessionId::new("session_2"),
                Some(&parent),
                &SpawnPane::BelowParent,
            )
            .expect("placed");
        assert_eq!(tree::panes(&placed.workspaces[0].tabs[0].layout).len(), 3);
        // Placing twice, a detached shell, or a parent placed nowhere: no edit.
        for (session, parent, placement) in [
            ("session_2", "session_0", SpawnPane::BelowParent),
            ("session_3", "session_0", SpawnPane::Detached),
            ("session_4", "session_9", SpawnPane::BelowParent),
        ] {
            assert!(
                f.store
                    .place_spawned(
                        &SessionId::new(session),
                        Some(&SessionId::new(parent)),
                        &placement
                    )
                    .is_none()
            );
        }
        assert_eq!(f.store.snapshot().unwrap(), placed);
    }

    #[test]
    fn a_full_tab_leaves_the_spawned_pane_unplaced() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, mut last) = f.create_tab(workspace, 0);
        for session in 1..MAX_TAB_PANES {
            last = split_at(&mut f, &tab, &last, session, DockEdge::Right);
        }
        assert!(
            f.store
                .place_spawned(
                    &SessionId::new("session_11"),
                    Some(&SessionId::new("session_0")),
                    &SpawnPane::BelowParent
                )
                .is_none()
        );
    }

    #[test]
    fn closing_a_pane_collapses_its_parent_split_but_keeps_a_one_pane_tab() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, a) = f.create_tab(workspace.clone(), 0);
        let b = split_at(&mut f, &tab, &a, 1, DockEdge::Right);
        split_at(&mut f, &tab, &b, 2, DockEdge::Bottom);
        f.create_tab(workspace, 3);
        let snapshot = f
            .store
            .prune_session(&SessionId::new("session_2"))
            .expect("pruned");
        // A | B again: the B/C split collapsed into B, the root kept its id.
        let layout = &snapshot.workspaces[0].tabs[0].layout;
        let LayoutNode::Split { first, second, .. } = layout else {
            panic!("{layout:?}");
        };
        assert!(matches!(&**first, LayoutNode::Pane { id, .. } if *id == a));
        assert!(matches!(&**second, LayoutNode::Pane { id, .. } if *id == b));
        // Focus left the removed pane.
        assert!(tree::panes(layout).contains(&snapshot.workspaces[0].tabs[0].focused_pane));
        // The only pane of a tab stays: closing that tab is its owner's call.
        assert!(
            f.store
                .prune_session(&SessionId::new("session_3"))
                .is_none()
        );
        assert!(
            f.store
                .prune_session(&SessionId::new("session_7"))
                .is_none()
        );
    }

    #[test]
    fn pre_split_terminals_move_into_their_parent_tab_once() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        f.create_tab(workspace, 0);
        let pair = |child: &str, parent: &str| (SessionId::new(child), SessionId::new(parent));
        // Newest first: session_2 is the ⌘J terminal; session_1 an older one.
        let adopted = f
            .store
            .adopt_pane_shells(&[
                pair("session_2", "session_0"),
                pair("session_1", "session_0"),
                pair("session_5", "session_9"),
            ])
            .expect("adopted");
        let layout = &adopted.workspaces[0].tabs[0].layout;
        let LayoutNode::Split { axis, second, .. } = layout else {
            panic!("{layout:?}");
        };
        assert_eq!(*axis, LayoutAxis::Vertical);
        assert!(
            matches!(&**second, LayoutNode::Pane { session_id, .. } if session_id.0 == "session_2")
        );
        assert_eq!(tree::panes(layout).len(), 2);
        // Never again: a terminal hidden with ⌘J stays hidden.
        f.store.prune_session(&SessionId::new("session_2"));
        assert!(
            f.store
                .adopt_pane_shells(&[pair("session_1", "session_0")])
                .is_none()
        );
        assert_eq!(
            tree::panes(&f.store.snapshot().unwrap().workspaces[0].tabs[0].layout).len(),
            1
        );
    }

    #[test]
    fn a_split_layout_survives_reopening_the_state_file() {
        let mut f = Fixture::new();
        let workspace = f.create_workspace("Project");
        let (tab, a) = f.create_tab(workspace, 0);
        let b = split_at(&mut f, &tab, &a, 1, DockEdge::Right);
        split_at(&mut f, &tab, &b, 2, DockEdge::Bottom);
        f.apply(ZoomPane {
            tab_id: tab.clone(),
            pane_id: Some(b),
        });
        let reopened = WorkspaceStore::new(&f.path).snapshot().unwrap();
        assert_eq!(reopened, f.snapshot);
    }
}
