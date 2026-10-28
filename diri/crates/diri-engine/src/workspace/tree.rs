use diri_proto::SessionId;
use diri_proto::workspace::*;

pub(super) fn panes(node: &LayoutNode) -> Vec<PaneId> {
    match node {
        LayoutNode::Pane { id, .. } => vec![id.clone()],
        LayoutNode::Split { first, second, .. } => {
            let mut result = panes(first);
            result.extend(panes(second));
            result
        }
    }
}

pub(super) fn matches(node: &LayoutNode, key: &LayoutNodeId) -> bool {
    match (node, key) {
        (LayoutNode::Pane { id, .. }, LayoutNodeId::Pane(key)) => id == key,
        (LayoutNode::Split { id, .. }, LayoutNodeId::Split(key)) => id == key,
        _ => false,
    }
}

pub(super) fn find<'a>(node: &'a LayoutNode, key: &LayoutNodeId) -> Option<&'a LayoutNode> {
    if matches(node, key) {
        return Some(node);
    }
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split { first, second, .. } => find(first, key).or_else(|| find(second, key)),
    }
}

/// Remove a subtree and collapse its empty ancestors without changing any
/// surviving placement identity or ratio.
pub(super) fn take(
    node: LayoutNode,
    key: &LayoutNodeId,
) -> (Option<LayoutNode>, Option<LayoutNode>) {
    if matches(&node, key) {
        return (None, Some(node));
    }
    match node {
        LayoutNode::Pane { .. } => (Some(node), None),
        LayoutNode::Split {
            id,
            axis,
            fraction,
            first,
            second,
        } => {
            let (first, removed_first) = take(*first, key);
            let (second, removed_second) = take(*second, key);
            let retained = match (first, second) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    id,
                    axis,
                    fraction,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (first, second) => first.or(second),
            };
            (retained, removed_first.or(removed_second))
        }
    }
}

pub(super) fn dock(
    node: &mut LayoutNode,
    target: &PaneId,
    incoming: &LayoutNode,
    edge: DockEdge,
    split: &SplitId,
) -> bool {
    match node {
        LayoutNode::Pane { id, .. } if id == target => {
            let previous = node.clone();
            let (first, second) = match edge {
                DockEdge::Left | DockEdge::Top => (incoming.clone(), previous),
                DockEdge::Right | DockEdge::Bottom => (previous, incoming.clone()),
            };
            *node = LayoutNode::Split {
                id: split.clone(),
                axis: match edge {
                    DockEdge::Left | DockEdge::Right => LayoutAxis::Horizontal,
                    _ => LayoutAxis::Vertical,
                },
                fraction: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            };
            true
        }
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split { first, second, .. } => {
            dock(first, target, incoming, edge, split)
                || dock(second, target, incoming, edge, split)
        }
    }
}

pub(super) fn resize(node: &mut LayoutNode, target: &SplitId, value: f32) -> bool {
    match node {
        LayoutNode::Split { id, fraction, .. } if id == target => {
            *fraction = value;
            true
        }
        LayoutNode::Split { first, second, .. } => {
            resize(first, target, value) || resize(second, target, value)
        }
        LayoutNode::Pane { .. } => false,
    }
}

pub(super) fn swap(node: &mut LayoutNode, a: &LayoutNode, b: &LayoutNode) {
    let (LayoutNode::Pane { id: a_id, .. }, LayoutNode::Pane { id: b_id, .. }) = (a, b) else {
        return;
    };
    match node {
        LayoutNode::Pane { id, .. } if id == a_id => *node = b.clone(),
        LayoutNode::Pane { id, .. } if id == b_id => *node = a.clone(),
        LayoutNode::Split { first, second, .. } => {
            swap(first, a, b);
            swap(second, a, b);
        }
        _ => {}
    }
}

pub(super) fn repair(snapshot: &mut WorkspaceSnapshot) {
    for workspace in &mut snapshot.workspaces {
        if !workspace
            .tabs
            .iter()
            .any(|tab| Some(&tab.id) == workspace.selected_tab.as_ref())
        {
            workspace.selected_tab = workspace.tabs.first().map(|tab| tab.id.clone());
        }
        for tab in &mut workspace.tabs {
            let ids = panes(&tab.layout);
            if !ids.contains(&tab.focused_pane) {
                tab.focused_pane = ids[0].clone();
            }
            if tab.zoomed_pane.as_ref().is_some_and(|id| !ids.contains(id)) {
                tab.zoomed_pane = None;
            }
            if tab.zoomed_pane.is_some() {
                tab.zoomed_pane = Some(tab.focused_pane.clone());
            }
        }
    }
}

/// How many panes sit side by side along `axis` inside `node`: a split on
/// the same axis adds its halves, a split across it is as wide as its wider
/// half. Equal weights give every visible column (or row) the same share.
/// Rebalance every divider so panes in a row or column share it equally.
/// Returns whether any fraction moved.
pub(super) fn equalize(node: &mut LayoutNode) -> bool {
    let Some(balanced) = node.balanced_fraction() else {
        return false;
    };
    let LayoutNode::Split {
        fraction,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    let moved = (*fraction - balanced).abs() > f32::EPSILON;
    *fraction = balanced;
    let first_moved = equalize(first);
    let second_moved = equalize(second);
    moved || first_moved || second_moved
}

/// Drop every pane showing `session` from a tab that keeps at least one
/// other pane. A tab whose only pane it is stays as it was.
pub(super) fn prune(layout: &mut LayoutNode, session: &SessionId) -> bool {
    let mut changed = false;
    loop {
        let doomed = panes_of(layout, session).into_iter().next();
        let Some(pane) = doomed else {
            return changed;
        };
        let (retained, _) = take(layout.clone(), &LayoutNodeId::Pane(pane));
        let Some(retained) = retained else {
            return changed;
        };
        *layout = retained;
        changed = true;
    }
}

pub(super) fn panes_of(node: &LayoutNode, session: &SessionId) -> Vec<PaneId> {
    match node {
        LayoutNode::Pane { id, session_id } if session_id == session => vec![id.clone()],
        LayoutNode::Pane { .. } => Vec::new(),
        LayoutNode::Split { first, second, .. } => {
            let mut result = panes_of(first, session);
            result.extend(panes_of(second, session));
            result
        }
    }
}
