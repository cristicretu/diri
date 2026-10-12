//! Keyboard operations project the saved tree and submit typed Engine mutations.
use super::*;
use crate::commands::CommandId;
use diri_proto::workspace::SpawnPane;

#[derive(Clone, Copy, Debug)]
pub(crate) enum PaneCommand {
    Focus(DockEdge),
    /// ⌘[ / ⌘]: the previous or next pane in reading order, wrapping.
    Cycle(bool),
    /// A new shell pane on this side of the focused pane.
    Split(DockEdge),
    Equalize,
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
            CommandId::FocusPreviousPane => Self::Cycle(false),
            CommandId::FocusNextPane => Self::Cycle(true),
            CommandId::EqualizePanes => Self::Equalize,
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

impl PaneCommand {
    /// Pane navigation that, in a one-pane tab, moves between sessions
    /// instead: ⌥⌘arrows and ⌘[ / ⌘] keep their old meaning there.
    pub(crate) fn session_fallback(self) -> Option<bool> {
        match self {
            Self::Focus(DockEdge::Left | DockEdge::Top) | Self::Cycle(false) => Some(false),
            Self::Focus(DockEdge::Right | DockEdge::Bottom) | Self::Cycle(true) => Some(true),
            _ => None,
        }
    }
}

/// The pane after (or before) `focused` in reading order, wrapping.
pub(super) fn cycle(node: &LayoutNode, focused: &PaneId, forward: bool) -> Option<PaneId> {
    let mut panes = Vec::new();
    leaves(node, &mut panes);
    if panes.len() < 2 {
        return None;
    }
    let index = panes.iter().position(|pane| &pane.pane == focused)?;
    let next = if forward {
        (index + 1) % panes.len()
    } else {
        (index + panes.len() - 1) % panes.len()
    };
    Some(panes[next].pane.clone())
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
    /// Runs a pane command against the tab on screen. False when there was
    /// nothing for it to act on, so the caller can fall back (pane focus in
    /// a one-pane tab moves between sessions instead).
    pub(crate) fn execute_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(mut tab) = self.tab.clone() else {
            return false;
        };
        let Some(focused) = self.focused_id().cloned() else {
            return false;
        };
        if command.session_fallback().is_some() && !matches!(tab.layout, LayoutNode::Split { .. }) {
            return false;
        }
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
            return false;
        };
        let target = match command {
            PaneCommand::Focus(edge) => Some(nearest(&geometry, edge)),
            PaneCommand::Cycle(forward) => Some(cycle(&tab.layout, &focused, forward)),
            _ => None,
        };
        if let Some(target) = target {
            if let Some(next) = target {
                self.focus_pane(next, window, cx);
            }
            // A multi-pane tab keeps the keys at its edges: no session jump.
            return true;
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
            return true;
        }
        if let PaneCommand::Split(edge) = command {
            let Some(source) = pane_session(&tab.layout, &focused) else {
                return true;
            };
            self.runtime.store.write().expect("store").spawn_pane_shell(
                &source,
                SpawnPane::Beside {
                    tab_id: tab.id,
                    target: focused,
                    edge,
                },
            );
            return true;
        }
        let mutation = match command {
            PaneCommand::Zoom => {
                // Zoom needs company: one pane already fills the tab.
                if !matches!(tab.layout, LayoutNode::Split { .. }) {
                    return true;
                }
                WorkspaceMutation::ZoomPane {
                    tab_id: tab.id,
                    pane_id: self
                        .tab
                        .as_ref()
                        .is_some_and(|tab| tab.zoomed_pane.is_none())
                        .then_some(focused),
                }
            }
            PaneCommand::Equalize => WorkspaceMutation::EqualizeTab { tab_id: tab.id },
            PaneCommand::Remove => WorkspaceMutation::RemovePane {
                tab_id: tab.id,
                pane_id: focused,
            },
            PaneCommand::Resize(axis, grow) => {
                let Some((split_id, fraction)) = resize_ancestor(&tab.layout, &focused, axis, grow)
                else {
                    return true;
                };
                WorkspaceMutation::ResizeSplit {
                    tab_id: tab.id,
                    split_id,
                    fraction,
                }
            }
            PaneCommand::Swap(edge) => {
                let Some(target) = nearest(&geometry, edge) else {
                    return true;
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
                    return true;
                };
                WorkspaceMutation::MoveNode {
                    source_tab: tab.id.clone(),
                    node: LayoutNodeId::Pane(focused),
                    destination_tab: tab.id,
                    target,
                    edge,
                }
            }
            PaneCommand::Focus(_) | PaneCommand::Cycle(_) | PaneCommand::Split(_) => return true,
        };
        self.runtime
            .store
            .write()
            .expect("store")
            .edit_workspace(mutation);
        cx.notify();
        true
    }

    fn focus_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_focus = Some(pane);
        self.remember_focus();
        self.flush_focus();
        self.focus(window, cx);
        cx.notify();
    }

    /// ⌘J: hide the focused pane shell (it keeps running), or show the
    /// tab owner's hidden one below the focused pane, or start a new one
    /// there. Never touches the inspector terminal.
    pub(crate) fn toggle_bottom_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let _ = window;
        if !self.enabled {
            return false;
        }
        let (Some(tab), Some(focused)) = (self.tab.clone(), self.focused_id().cloned()) else {
            return false;
        };
        let Some(source) = pane_session(&tab.layout, &focused) else {
            return false;
        };
        let mut store = self.runtime.store.write().expect("store");
        if !store.workspace_catalog().can_edit() {
            return true;
        }
        let owner = store.pane_owner(&source);
        let multiple = matches!(tab.layout, LayoutNode::Split { .. });
        let below = below_sibling(&tab.layout, &focused)
            .filter(|(_, session)| store.pane_owner(session) != *session);
        if owner != source && multiple {
            store.edit_workspace(WorkspaceMutation::RemovePane {
                tab_id: tab.id,
                pane_id: focused,
            });
        } else if let Some((pane, _)) = below {
            // The terminal docked right below this pane: ⌘J hides it again.
            store.edit_workspace(WorkspaceMutation::RemovePane {
                tab_id: tab.id,
                pane_id: pane,
            });
        } else if let Some(hidden) = store.hidden_pane_shell(&owner) {
            store.edit_workspace(WorkspaceMutation::SplitPane {
                tab_id: tab.id,
                target: focused,
                session_id: hidden,
                edge: DockEdge::Bottom,
            });
        } else {
            store.spawn_pane_shell(
                &source,
                SpawnPane::Beside {
                    tab_id: tab.id,
                    target: focused,
                    edge: DockEdge::Bottom,
                },
            );
        }
        drop(store);
        cx.notify();
        true
    }

    /// Before ⌘W ends `session`, focus the pane used before it. The local
    /// removal lands first and would otherwise leave focus to the tab's
    /// agent pane, then the Engine's first pane.
    pub(crate) fn focus_before_closing(
        &mut self,
        session: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.closing.insert(session.clone());
        let Some(tab) = self.tab.clone() else {
            return;
        };
        let recent = self.focus_history.get(&tab.id).and_then(|history| {
            history
                .iter()
                .rev()
                .find(|pane| pane_session(&tab.layout, pane).is_some_and(|other| other != *session))
                .cloned()
        });
        if let Some(pane) = recent {
            self.focus_pane(pane, window, cx);
        }
    }

    /// The pane ⌘W closes: the focused pane's session when it is a pane
    /// shell sharing the tab. None closes the whole tab, as before: the
    /// owner's pane is the tab.
    pub(crate) fn closable_pane_session(&self) -> Option<SessionId> {
        if !self.enabled {
            return None;
        }
        let tab = self.tab.as_ref()?;
        if !matches!(tab.layout, LayoutNode::Split { .. }) {
            return None;
        }
        let session = pane_session(&tab.layout, self.focused_id()?)?;
        let store = self.runtime.store.read().expect("store");
        (store.pane_owner(&session) != session).then_some(session)
    }
}

/// The single pane docked directly below `pane`: its split partner when
/// they share a top-over-bottom split with `pane` on top.
pub(super) fn below_sibling(node: &LayoutNode, pane: &PaneId) -> Option<(PaneId, SessionId)> {
    let LayoutNode::Split {
        axis,
        first,
        second,
        ..
    } = node
    else {
        return None;
    };
    if *axis == LayoutAxis::Vertical
        && matches!(&**first, LayoutNode::Pane { id, .. } if id == pane)
        && let LayoutNode::Pane { id, session_id } = &**second
    {
        return Some((id.clone(), session_id.clone()));
    }
    below_sibling(first, pane).or_else(|| below_sibling(second, pane))
}

pub(super) fn pane_for_session(node: &LayoutNode, session: &SessionId) -> Option<PaneId> {
    match node {
        LayoutNode::Pane { id, session_id } => (session_id == session).then(|| id.clone()),
        LayoutNode::Split { first, second, .. } => {
            pane_for_session(first, session).or_else(|| pane_for_session(second, session))
        }
    }
}

pub(super) fn pane_session(node: &LayoutNode, pane: &PaneId) -> Option<SessionId> {
    match node {
        LayoutNode::Pane { id, session_id } => (id == pane).then(|| session_id.clone()),
        LayoutNode::Split { first, second, .. } => {
            pane_session(first, pane).or_else(|| pane_session(second, pane))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::TabId;
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

    #[test]
    fn pane_cycle_walks_reading_order_and_wraps() {
        let tree = split(
            "outer",
            LayoutAxis::Horizontal,
            0.5,
            leaf("a"),
            split("right", LayoutAxis::Vertical, 0.5, leaf("b"), leaf("c")),
        );
        let next = |from: &str, forward| cycle(&tree, &PaneId::new(from), forward);
        assert_eq!(next("a", true), Some(PaneId::new("b")));
        assert_eq!(next("c", true), Some(PaneId::new("a")));
        assert_eq!(next("a", false), Some(PaneId::new("c")));
        assert_eq!(cycle(&leaf("a"), &PaneId::new("a"), true), None);
    }

    #[test]
    fn only_navigation_falls_back_to_sessions() {
        assert_eq!(
            PaneCommand::Focus(DockEdge::Left).session_fallback(),
            Some(false)
        );
        assert_eq!(
            PaneCommand::Focus(DockEdge::Top).session_fallback(),
            Some(false)
        );
        assert_eq!(
            PaneCommand::Focus(DockEdge::Right).session_fallback(),
            Some(true)
        );
        assert_eq!(PaneCommand::Cycle(true).session_fallback(), Some(true));
        assert_eq!(PaneCommand::Cycle(false).session_fallback(), Some(false));
        assert_eq!(PaneCommand::Split(DockEdge::Right).session_fallback(), None);
        assert_eq!(PaneCommand::Zoom.session_fallback(), None);
    }

    #[test]
    fn the_terminal_below_is_a_pane_directly_under_the_focused_one() {
        let tree = split(
            "outer",
            LayoutAxis::Horizontal,
            0.5,
            split(
                "left",
                LayoutAxis::Vertical,
                0.6,
                leaf("agent"),
                leaf("shell"),
            ),
            leaf("side"),
        );
        assert_eq!(
            below_sibling(&tree, &PaneId::new("agent")),
            Some((PaneId::new("shell"), SessionId::new("shell")))
        );
        // Beside, or above: not "below".
        assert_eq!(below_sibling(&tree, &PaneId::new("side")), None);
        assert_eq!(below_sibling(&tree, &PaneId::new("shell")), None);
        assert_eq!(
            pane_session(&tree, &PaneId::new("side")),
            Some(SessionId::new("side"))
        );
    }

    #[test]
    fn split_commands_map_to_shells() {
        assert!(matches!(
            PaneCommand::from_id(CommandId::SplitPaneRight),
            Some(PaneCommand::Split(DockEdge::Right))
        ));
        assert!(matches!(
            PaneCommand::from_id(CommandId::SplitPaneBelow),
            Some(PaneCommand::Split(DockEdge::Bottom))
        ));
        assert!(matches!(
            PaneCommand::from_id(CommandId::EqualizePanes),
            Some(PaneCommand::Equalize)
        ));
    }
}
