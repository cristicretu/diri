//! Shared organization records. Layout nodes reference existing sessions; they
//! never represent a process, attachment, or controller lease.
use serde::{Deserialize, Serialize};

use crate::{ProjectId, SessionId};

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
        }
    };
}
identity!(WorkspaceId);
identity!(TabId);
identity!(PaneId);
identity!(SplitId);

pub const WORKSPACE_SCHEMA_VERSION: u32 = 1;
pub const MAX_WORKSPACES: usize = 64;
pub const MAX_WORKSPACE_TABS: usize = 256;
pub const MAX_TAB_PANES: usize = 8;
pub const MAX_LAYOUT_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LayoutAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DockEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LayoutNode {
    Pane {
        id: PaneId,
        session_id: SessionId,
    },
    Split {
        id: SplitId,
        axis: LayoutAxis,
        fraction: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "id", rename_all = "camelCase")]
pub enum LayoutNodeId {
    Pane(PaneId),
    Split(SplitId),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTab {
    pub id: TabId,
    pub title: Option<String>,
    pub layout: LayoutNode,
    pub focused_pane: PaneId,
    pub zoomed_pane: Option<PaneId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    /// A project workspace uses the same durable identity as its agent rows.
    /// Older, independently named layouts remain unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub id: WorkspaceId,
    pub name: String,
    pub tabs: Vec<WorkspaceTab>,
    pub selected_tab: Option<TabId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub schema_version: u32,
    pub revision: u64,
    pub workspaces: Vec<WorkspaceRecord>,
}
impl Default for WorkspaceSnapshot {
    fn default() -> Self {
        Self {
            schema_version: WORKSPACE_SCHEMA_VERSION,
            revision: 0,
            workspaces: Vec::new(),
        }
    }
}

/// A candidate edit is validated and persisted under one Engine file lock.
/// Removing a placement never removes or terminates its referenced session.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WorkspaceMutation {
    /// Open an existing agent in its project layout without launching a process.
    OpenProjectAgent {
        session_id: SessionId,
        /// Preserve a currently viewed layout when it already contains the agent.
        #[serde(default)]
        preferred_workspace: Option<WorkspaceId>,
    },
    CreateWorkspace {
        name: String,
    },
    RenameWorkspace {
        workspace_id: WorkspaceId,
        name: String,
    },
    RemoveWorkspace {
        workspace_id: WorkspaceId,
    },
    MoveWorkspace {
        workspace_id: WorkspaceId,
        index: usize,
    },
    CreateTab {
        /// Existing clients select by default; delayed launches may append in the background.
        #[serde(default = "select_created_tab")]
        select: bool,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        title: Option<String>,
    },
    RenameTab {
        tab_id: TabId,
        title: Option<String>,
    },
    RemoveTab {
        tab_id: TabId,
    },
    MoveTab {
        tab_id: TabId,
        workspace_id: WorkspaceId,
        index: usize,
    },
    SelectTab {
        workspace_id: WorkspaceId,
        tab_id: TabId,
    },
    SplitPane {
        tab_id: TabId,
        target: PaneId,
        session_id: SessionId,
        edge: DockEdge,
    },
    RemovePane {
        tab_id: TabId,
        pane_id: PaneId,
    },
    MoveNode {
        source_tab: TabId,
        node: LayoutNodeId,
        destination_tab: TabId,
        target: PaneId,
        edge: DockEdge,
    },
    SwapPanes {
        first_tab: TabId,
        first: PaneId,
        second_tab: TabId,
        second: PaneId,
    },
    ResizeSplit {
        tab_id: TabId,
        split_id: SplitId,
        fraction: f32,
    },
    FocusPane {
        tab_id: TabId,
        pane_id: PaneId,
    },
    ZoomPane {
        tab_id: TabId,
        pane_id: Option<PaneId>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMutationParams {
    pub expected_revision: u64,
    pub mutation: WorkspaceMutation,
}

fn select_created_tab() -> bool {
    true
}
