//! Keyboard operations project the saved tree and submit typed Engine mutations.
use super::*;
use crate::commands::CommandId;

#[derive(Clone, Copy, Debug)]
pub(crate) enum PaneCommand {
    Focus(DockEdge),
    Split(DockEdge),
    Zoom,
    Remove,
    Resize(LayoutAxis, bool),
    Swap(DockEdge),
    Move(DockEdge),
}
impl PaneCommand {
    pub(crate) fn from_id(id: CommandId) -> Option<Self> {
        Some(match id {
            CommandId::FocusPaneLeft => Self::Focus(DockEdge::Left),
            CommandId::FocusPaneRight => Self::Focus(DockEdge::Right),
            CommandId::FocusPaneUp => Self::Focus(DockEdge::Top),
            CommandId::FocusPaneDown => Self::Focus(DockEdge::Bottom),
            CommandId::SplitPaneRight => Self::Split(DockEdge::Right),
            CommandId::SplitPaneBelow => Self::Split(DockEdge::Bottom),
            CommandId::TogglePaneZoom => Self::Zoom,
            CommandId::RemoveFocusedPane => Self::Remove,
            CommandId::PaneGrowWidth => Self::Resize(LayoutAxis::Horizontal, true),
            CommandId::PaneShrinkWidth => Self::Resize(LayoutAxis::Horizontal, false),
            CommandId::PaneGrowHeight => Self::Resize(LayoutAxis::Vertical, true),
            CommandId::PaneShrinkHeight => Self::Resize(LayoutAxis::Vertical, false),
            CommandId::SwapPaneLeft => Self::Swap(DockEdge::Left),
            CommandId::SwapPaneRight => Self::Swap(DockEdge::Right),
            CommandId::SwapPaneUp => Self::Swap(DockEdge::Top),
            CommandId::SwapPaneDown => Self::Swap(DockEdge::Bottom),
            CommandId::MovePaneLeft => Self::Move(DockEdge::Left),
            CommandId::MovePaneRight => Self::Move(DockEdge::Right),
            CommandId::MovePaneUp => Self::Move(DockEdge::Top),
            CommandId::MovePaneDown => Self::Move(DockEdge::Bottom),
            _ => return None,
        })
    }
}

pub(super) fn contains_pane(node: &LayoutNode, pane: &PaneId) -> bool {
    match node {
        LayoutNode::Pane { id, .. } => id == pane,
        LayoutNode::Split { first, second, .. } => {
            contains_pane(first, pane) || contains_pane(second, pane)
        }
    }
}

fn nearest(geometry: &WorkspaceGeometry, edge: DockEdge) -> Option<PaneId> {
    let origin = geometry
        .panes
        .iter()
        .find(|pane| pane.identity.pane == geometry.focused.pane)?
        .bounds;
    let horizontal = matches!(edge, DockEdge::Left | DockEdge::Right);
    let positive = matches!(edge, DockEdge::Right | DockEdge::Bottom);
    let rect = |r: Rect| {
        if horizontal {
            (r.x + r.width / 2.0, r.y, r.y + r.height)
        } else {
            (r.y + r.height / 2.0, r.x, r.x + r.width)
        }
    };
    let (center, start, end) = rect(origin);
    geometry
        .panes
        .iter()
        .filter(|pane| pane.identity.pane != geometry.focused.pane)
        .filter_map(|pane| {
            let (other, low, high) = rect(pane.bounds);
            let forward = if positive {
                other - center
            } else {
                center - other
            };
            if forward <= 0.1 {
                return None;
            }
            let overlap = low < end && high > start;
            let side = ((low + high) / 2.0 - (start + end) / 2.0).abs();
            Some((!overlap, forward, side, &pane.identity.pane))
        })
        .min_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.total_cmp(&b.1))
                .then(a.2.total_cmp(&b.2))
                .then(a.3.0.cmp(&b.3.0))
        })
        .map(|(_, _, _, id)| id.clone())
}

fn resize_ancestor(
    node: &LayoutNode,
    pane: &PaneId,
    axis: LayoutAxis,
    grow: bool,
) -> Option<(SplitId, f32)> {
    let LayoutNode::Split {
        id,
        axis: split_axis,
        fraction,
        first,
        second,
    } = node
    else {
        return None;
    };
    let in_first = contains_pane(first, pane);
    let in_second = contains_pane(second, pane);
    let child = if in_first {
        first
    } else if in_second {
        second
    } else {
        return None;
    };
    resize_ancestor(child, pane, axis, grow).or_else(|| {
        (*split_axis == axis).then(|| {
            let increase = grow == in_first;
            (
                id.clone(),
                (*fraction + if increase { 0.05 } else { -0.05 }).clamp(0.1, 0.9),
            )
        })
    })
}

impl WorkspaceWorkbench {
    pub(crate) fn execute_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let Some(mut tab) = self.tab.clone() else {
            return;
        };
        let Some(focused) = self.focused_id().cloned() else {
            return;
        };
        // Neighbor navigation uses the complete saved tree even while zoomed.
        tab.focused_pane = focused.clone();
        tab.zoomed_pane = None;
        let Some(geometry) = WorkspaceGeometry::settled(
            &tab,
            Rect {
                width: self.viewport.width,
                height: self.viewport.height,
                ..Default::default()
            },
        ) else {
            return;
        };
        if let PaneCommand::Focus(edge) = command {
            if let Some(next) = nearest(&geometry, edge) {
                self.pending_focus = Some(next);
                self.flush_focus();
                self.focus(window, cx);
                cx.notify();
            }
            return;
        }
        if !self
            .runtime
            .store
            .read()
            .expect("store")
            .workspace_catalog()
            .can_edit()
            || self.resize.is_some()
        {
            return;
        }
        if let PaneCommand::Split(edge) = command {
            cx.emit(WorkspaceWorkbenchEvent::RequestSplit {
                tab: tab.id,
                pane: focused,
                edge,
            });
            return;
        }
        let mutation = match command {
            PaneCommand::Zoom => WorkspaceMutation::ZoomPane {
                tab_id: tab.id,
                pane_id: self
                    .tab
                    .as_ref()
                    .is_some_and(|tab| tab.zoomed_pane.is_none())
                    .then_some(focused),
            },
            PaneCommand::Remove => WorkspaceMutation::RemovePane {
                tab_id: tab.id,
                pane_id: focused,
            },
            PaneCommand::Resize(axis, grow) => {
                let Some((split_id, fraction)) = resize_ancestor(&tab.layout, &focused, axis, grow)
                else {
                    return;
                };
                WorkspaceMutation::ResizeSplit {
                    tab_id: tab.id,
                    split_id,
                    fraction,
                }
            }
            PaneCommand::Swap(edge) => {
                let Some(target) = nearest(&geometry, edge) else {
                    return;
                };
                WorkspaceMutation::SwapPanes {
                    first_tab: tab.id.clone(),
                    first: focused,
                    second_tab: tab.id,
                    second: target,
                }
            }
            PaneCommand::Move(edge) => {
                let Some(target) = nearest(&geometry, edge) else {
                    return;
                };
                WorkspaceMutation::MoveNode {
                    source_tab: tab.id.clone(),
                    node: LayoutNodeId::Pane(focused),
                    destination_tab: tab.id,
                    target,
                    edge,
                }
            }
            PaneCommand::Focus(_) | PaneCommand::Split(_) => return,
        };
        self.runtime
            .store
            .write()
            .expect("store")
            .edit_workspace(mutation);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn leaf(id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(id),
        }
    }
    fn split(
        id: &str,
        axis: LayoutAxis,
        fraction: f32,
        first: LayoutNode,
        second: LayoutNode,
    ) -> LayoutNode {
        LayoutNode::Split {
            id: SplitId::new(id),
            axis,
            fraction,
            first: Box::new(first),
            second: Box::new(second),
        }
    }
    #[test]
    fn uneven_nested_neighbors_follow_spatial_direction_and_overlap() {
        let mut tab = WorkspaceTab {
            id: TabId::new("tab"),
            title: None,
            focused_pane: PaneId::new("lower"),
            zoomed_pane: None,
            layout: split(
                "outer",
                LayoutAxis::Horizontal,
                0.35,
                split(
                    "left",
                    LayoutAxis::Vertical,
                    0.3,
                    leaf("upper"),
                    leaf("lower"),
                ),
                leaf("right"),
            ),
        };
        let bounds = Rect {
            width: 1200.0,
            height: 800.0,
            ..Default::default()
        };
        let geometry = WorkspaceGeometry::settled(&tab, bounds).unwrap();
        assert_eq!(
            nearest(&geometry, DockEdge::Top),
            Some(PaneId::new("upper"))
        );
        assert_eq!(
            nearest(&geometry, DockEdge::Right),
            Some(PaneId::new("right"))
        );
        assert_eq!(nearest(&geometry, DockEdge::Left), None);
        assert_eq!(nearest(&geometry, DockEdge::Bottom), None);
        tab.focused_pane = PaneId::new("right");
        assert_eq!(
            nearest(
                &WorkspaceGeometry::settled(&tab, bounds).unwrap(),
                DockEdge::Left
            ),
            Some(PaneId::new("lower"))
        );
    }
    #[test]
    fn resize_uses_deepest_matching_divider_and_correct_child_sign() {
        let tree = split(
            "outer",
            LayoutAxis::Horizontal,
            0.5,
            split("inner", LayoutAxis::Horizontal, 0.89, leaf("a"), leaf("b")),
            leaf("c"),
        );
        assert_eq!(
            resize_ancestor(&tree, &PaneId::new("a"), LayoutAxis::Horizontal, true),
            Some((SplitId::new("inner"), 0.9))
        );
        let (id, fraction) =
            resize_ancestor(&tree, &PaneId::new("b"), LayoutAxis::Horizontal, true).unwrap();
        assert_eq!(id, SplitId::new("inner"));
        assert!((fraction - 0.84).abs() < 0.0001);
        assert_eq!(
            resize_ancestor(&tree, &PaneId::new("c"), LayoutAxis::Horizontal, false),
            Some((SplitId::new("outer"), 0.55))
        );
        assert_eq!(
            resize_ancestor(&tree, &PaneId::new("a"), LayoutAxis::Vertical, true),
            None
        );
        assert_eq!(
            resize_ancestor(&tree, &PaneId::new("missing"), LayoutAxis::Horizontal, true),
            None
        );
    }
}
