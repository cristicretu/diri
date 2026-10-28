//! Moving a pane by its title. A pane of a split tab can be dropped on an
//! edge of another pane (a split there, previewed live), on the middle of
//! one (the two trade places), on a sidebar row or tab (into that tab; a
//! hover of [`SPRING_LOAD`] opens it so the drop can pick an edge), or on
//! the free space below the rows (a tab of its own). Every outcome is one
//! Engine layout mutation that keeps the pane's id: the session it shows
//! keeps running and its mounted terminal is reused, never respawned.
//!
//! This module is the pure part: zones, the drop's mutation, the
//! spring-load clock and the motion curves. The workbench and the sidebar
//! feed it pointer positions; [`PaneDragState`] is what they share.
use std::time::{Duration, Instant};

use diri_proto::{
    SessionId,
    workspace::{
        DockEdge, LayoutAxis, LayoutNode, LayoutNodeId, PaneId, TabId, WorkspaceMutation,
        WorkspaceSnapshot, WorkspaceTab,
    },
};
use diri_ui::{Palette, SemanticColors, Typo};
use gpui::{Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px};

use crate::workspace_geometry::Rect;

/// How long a pane held over a tab waits before that tab opens under it,
/// the standard macOS spring-loading delay.
pub(crate) const SPRING_LOAD: Duration = Duration::from_millis(500);
/// How long the panes take to reach their new places after a move.
pub(crate) const MORPH: Duration = Duration::from_millis(240);
/// How long the drop preview takes to slide from one target to the next.
pub(crate) const GLIDE: Duration = Duration::from_millis(110);
/// A pointer farther than this share of the pane from every edge is in
/// the pane's middle, which swaps instead of splitting.
const SWAP_CORE: f32 = 0.3;
/// A move armed by a drop or a command animates only the layout that
/// arrives this soon after it; an unrelated later change does not.
pub(crate) const MORPH_ARM: Duration = Duration::from_secs(2);

/// What GPUI carries while a pane is dragged. Also its ghost: a small chip
/// with the pane's title that follows the pointer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DraggedPane {
    pub tab: TabId,
    pub pane: PaneId,
    pub session: SessionId,
    pub title: SharedString,
    pub colors: SemanticColors,
    /// The pane may leave its tab: the tab keeps a sidebar row afterwards.
    pub can_leave: bool,
    /// The pane may become a tab of its own: it has a sidebar row to show
    /// that tab (a pane shell does not) and its tab keeps another pane.
    pub can_break: bool,
}

impl Render for DraggedPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        // Escape keeps GPUI's drag alive until the button comes up; the
        // ghost hides instead.
        if cx
            .try_global::<PaneDragState>()
            .is_some_and(|state| state.cancelled)
        {
            return div();
        }
        div()
            .px(px(10.0))
            .h(px(26.0))
            .max_w(px(240.0))
            .flex()
            .items_center()
            .rounded(px(13.0))
            .bg(colors.background.alpha(0.94))
            .border_1()
            .border_color(colors.primary.alpha(0.12))
            .shadow(diri_ui::Glass::shadows(colors))
            .text_size(px(Typo::META.size))
            .text_color(colors.primary)
            .overflow_hidden()
            .text_ellipsis()
            .child(self.title.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneZone {
    /// Split the target and put the moved pane on this side.
    Edge(DockEdge),
    /// Trade places with the target.
    Swap,
}

/// Where in `rect` the point falls: the nearest edge, or the middle.
pub(crate) fn zone(rect: Rect, x: f32, y: f32) -> Option<PaneZone> {
    if rect.width <= 0.0
        || rect.height <= 0.0
        || x < rect.x
        || y < rect.y
        || x > rect.x + rect.width
        || y > rect.y + rect.height
    {
        return None;
    }
    let nx = (x - rect.x) / rect.width;
    let ny = (y - rect.y) / rect.height;
    let edges = [
        (nx, DockEdge::Left),
        (1.0 - nx, DockEdge::Right),
        (ny, DockEdge::Top),
        (1.0 - ny, DockEdge::Bottom),
    ];
    let (distance, edge) = edges
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .expect("four edges");
    Some(if distance > SWAP_CORE {
        PaneZone::Swap
    } else {
        PaneZone::Edge(edge)
    })
}

/// The part of `rect` the moved pane would take: the half on that edge
/// (the Engine splits at one half), or all of it for a swap.
pub(crate) fn zone_preview(rect: Rect, zone: PaneZone) -> Rect {
    let half_w = rect.width / 2.0;
    let half_h = rect.height / 2.0;
    match zone {
        PaneZone::Swap => rect,
        PaneZone::Edge(DockEdge::Left) => Rect {
            width: half_w,
            ..rect
        },
        PaneZone::Edge(DockEdge::Right) => Rect {
            x: rect.x + half_w,
            width: half_w,
            ..rect
        },
        PaneZone::Edge(DockEdge::Top) => Rect {
            height: half_h,
            ..rect
        },
        PaneZone::Edge(DockEdge::Bottom) => Rect {
            y: rect.y + half_h,
            height: half_h,
            ..rect
        },
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DropTarget {
    /// A pane of the tab on screen.
    Pane {
        tab: TabId,
        pane: PaneId,
        zone: PaneZone,
    },
    /// A sidebar row or tab: the tab that shows this session.
    Tab(SessionId),
    /// The free space below the rows or after the tabs.
    NewTab,
}

/// The tab that shows `session`, preferring `prefer` when it does too.
pub(crate) fn tab_of<'a>(
    snapshot: &'a WorkspaceSnapshot,
    session: &SessionId,
    prefer: Option<&TabId>,
) -> Option<&'a WorkspaceTab> {
    let mut found = None;
    for tab in snapshot.workspaces.iter().flat_map(|w| &w.tabs) {
        if shows(&tab.layout, session) {
            if Some(&tab.id) == prefer {
                return Some(tab);
            }
            found = found.or(Some(tab));
        }
    }
    found
}

fn shows(node: &LayoutNode, session: &SessionId) -> bool {
    match node {
        LayoutNode::Pane { session_id, .. } => session_id == session,
        LayoutNode::Split { first, second, .. } => shows(first, session) || shows(second, session),
    }
}

fn find_tab<'a>(snapshot: &'a WorkspaceSnapshot, tab: &TabId) -> Option<&'a WorkspaceTab> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|w| &w.tabs)
        .find(|candidate| &candidate.id == tab)
}

fn pane_count(node: &LayoutNode) -> usize {
    match node {
        LayoutNode::Pane { .. } => 1,
        LayoutNode::Split { first, second, .. } => pane_count(first) + pane_count(second),
    }
}

/// Whether `pane` may leave `tab` without stranding the rest: what stays
/// must still include a session with a sidebar row (`has_row`), or nothing
/// stays at all. A tab of pane shells alone would have no row to reach it.
pub(crate) fn can_leave(
    tab: &WorkspaceTab,
    pane: &PaneId,
    has_row: impl Fn(&SessionId) -> bool,
) -> bool {
    fn rest(node: &LayoutNode, pane: &PaneId, out: &mut Vec<SessionId>) {
        match node {
            LayoutNode::Pane { id, session_id } => {
                if id != pane {
                    out.push(session_id.clone());
                }
            }
            LayoutNode::Split { first, second, .. } => {
                rest(first, pane, out);
                rest(second, pane, out);
            }
        }
    }
    let mut remaining = Vec::new();
    rest(&tab.layout, pane, &mut remaining);
    remaining.is_empty() || remaining.iter().any(has_row)
}

/// The Engine's `take`, locally: `node` without `key`, its sibling taking
/// the parent split's place.
fn without(node: &LayoutNode, key: &PaneId) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { id, .. } => (id != key).then(|| node.clone()),
        LayoutNode::Split {
            id,
            axis,
            fraction,
            first,
            second,
        } => match (without(first, key), without(second, key)) {
            (Some(first), Some(second)) => Some(LayoutNode::Split {
                id: id.clone(),
                axis: *axis,
                fraction: *fraction,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        },
    }
}

/// The Engine's `dock`, locally, for comparing shapes.
fn docked(node: &LayoutNode, target: &PaneId, incoming: &LayoutNode, edge: DockEdge) -> LayoutNode {
    match node {
        LayoutNode::Pane { id, .. } if id == target => {
            let (first, second) = match edge {
                DockEdge::Left | DockEdge::Top => (incoming.clone(), node.clone()),
                DockEdge::Right | DockEdge::Bottom => (node.clone(), incoming.clone()),
            };
            LayoutNode::Split {
                id: diri_proto::workspace::SplitId::new("preview"),
                axis: match edge {
                    DockEdge::Left | DockEdge::Right => LayoutAxis::Horizontal,
                    DockEdge::Top | DockEdge::Bottom => LayoutAxis::Vertical,
                },
                fraction: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            }
        }
        LayoutNode::Pane { .. } => node.clone(),
        LayoutNode::Split {
            id,
            axis,
            fraction,
            first,
            second,
        } => LayoutNode::Split {
            id: id.clone(),
            axis: *axis,
            fraction: *fraction,
            first: Box::new(docked(first, target, incoming, edge)),
            second: Box::new(docked(second, target, incoming, edge)),
        },
    }
}

/// Two trees with the same panes in the same arrangement, whatever their
/// split ids. Fractions count, at the precision a person can see.
fn same_shape(a: &LayoutNode, b: &LayoutNode) -> bool {
    match (a, b) {
        (LayoutNode::Pane { id: a, .. }, LayoutNode::Pane { id: b, .. }) => a == b,
        (
            LayoutNode::Split {
                axis: a_axis,
                fraction: a_fraction,
                first: a_first,
                second: a_second,
                ..
            },
            LayoutNode::Split {
                axis: b_axis,
                fraction: b_fraction,
                first: b_first,
                second: b_second,
                ..
            },
        ) => {
            a_axis == b_axis
                && (a_fraction - b_fraction).abs() < 0.02
                && same_shape(a_first, b_first)
                && same_shape(a_second, b_second)
        }
        _ => false,
    }
}

/// The layout mutation a drop on `target` asks for, or None when it would
/// change nothing or is not allowed (so the target does not light up).
pub(crate) fn drop_mutation(
    source: &DraggedPane,
    target: &DropTarget,
    snapshot: &WorkspaceSnapshot,
) -> Option<WorkspaceMutation> {
    let origin = find_tab(snapshot, &source.tab)?;
    let incoming = commands_find(&origin.layout, &source.pane)?;
    match target {
        DropTarget::Pane { tab, pane, zone } => {
            if *pane == source.pane {
                return None;
            }
            let destination = find_tab(snapshot, tab)?;
            commands_find(&destination.layout, pane)?;
            let across = *tab != source.tab;
            if across && !source.can_leave {
                return None;
            }
            match zone {
                PaneZone::Swap => Some(WorkspaceMutation::SwapPanes {
                    first_tab: source.tab.clone(),
                    first: source.pane.clone(),
                    second_tab: tab.clone(),
                    second: pane.clone(),
                }),
                PaneZone::Edge(edge) => {
                    if !across {
                        let rest = without(&origin.layout, &source.pane)?;
                        let after = docked(&rest, pane, &incoming, *edge);
                        if same_shape(&after, &origin.layout) {
                            return None;
                        }
                    }
                    Some(WorkspaceMutation::MoveNode {
                        source_tab: source.tab.clone(),
                        node: LayoutNodeId::Pane(source.pane.clone()),
                        destination_tab: tab.clone(),
                        target: pane.clone(),
                        edge: *edge,
                    })
                }
            }
        }
        DropTarget::Tab(session) => {
            if !source.can_leave {
                return None;
            }
            let destination = tab_of(snapshot, session, None)?;
            if destination.id == source.tab {
                return None;
            }
            Some(WorkspaceMutation::MoveNode {
                source_tab: source.tab.clone(),
                node: LayoutNodeId::Pane(source.pane.clone()),
                destination_tab: destination.id.clone(),
                target: destination.focused_pane.clone(),
                edge: DockEdge::Right,
            })
        }
        DropTarget::NewTab => (source.can_break && pane_count(&origin.layout) > 1).then(|| {
            WorkspaceMutation::BreakPane {
                tab_id: source.tab.clone(),
                pane_id: source.pane.clone(),
            }
        }),
    }
}

fn commands_find(node: &LayoutNode, pane: &PaneId) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { id, .. } => (id == pane).then(|| node.clone()),
        LayoutNode::Split { first, second, .. } => {
            commands_find(first, pane).or_else(|| commands_find(second, pane))
        }
    }
}

/// The spring-load clock: which row the dragged pane rests on, since when,
/// and whether that row already opened.
#[derive(Clone, Debug, Default)]
pub(crate) struct SpringLoader {
    resting: Option<(SessionId, Instant)>,
    opened: Option<SessionId>,
}

impl SpringLoader {
    /// The pointer is over `row` (None: no row). True when this starts a
    /// new rest, so the caller schedules a [`Self::due`] check.
    pub(crate) fn hover(&mut self, row: Option<&SessionId>, now: Instant) -> bool {
        match (row, &self.resting) {
            (Some(row), Some((resting, _))) if resting == row => false,
            (Some(row), _) => {
                self.resting = Some((row.clone(), now));
                self.opened = None;
                true
            }
            (None, _) => {
                self.resting = None;
                self.opened = None;
                false
            }
        }
    }

    /// The row being rested on and how far along its wait is (0 to 1).
    pub(crate) fn progress(&self, now: Instant) -> Option<(&SessionId, f32)> {
        let (row, since) = self.resting.as_ref()?;
        let elapsed = now.duration_since(*since).as_secs_f32();
        Some((row, (elapsed / SPRING_LOAD.as_secs_f32()).min(1.0)))
    }

    /// The spring-load timer for `row` ran out: open it if the pane still
    /// rests there and it has not opened yet.
    pub(crate) fn fire(&mut self, row: &SessionId) -> bool {
        if self.resting.as_ref().is_none_or(|(resting, _)| resting != row)
            || self.opened.as_ref() == Some(row)
        {
            return false;
        }
        self.opened = Some(row.clone());
        true
    }

}

/// The row's fill while a pane rests on it: a faint tint that deepens
/// through the wait, so the opening is expected, then holds.
pub(crate) fn spring_fill(progress: f32) -> gpui::Rgba {
    Palette::CLAY.alpha(0.06 + 0.12 * progress.clamp(0.0, 1.0))
}

pub(crate) fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub(crate) fn lerp_rect(from: Rect, to: Rect, t: f32) -> Rect {
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Rect {
        x: mix(from.x, to.x),
        y: mix(from.y, to.y),
        width: mix(from.width, to.width),
        height: mix(from.height, to.height),
    }
}

/// A rectangle sliding to a new place over `length`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Glide {
    pub from: Rect,
    pub to: Rect,
    pub start: Instant,
    pub length: Duration,
}

impl Glide {
    pub(crate) fn still(at: Rect, now: Instant) -> Self {
        Self {
            from: at,
            to: at,
            start: now,
            length: Duration::ZERO,
        }
    }

    pub(crate) fn progress(&self, now: Instant) -> f32 {
        if self.length.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f32() / self.length.as_secs_f32())
            .min(1.0)
    }

    pub(crate) fn at(&self, now: Instant) -> Rect {
        lerp_rect(self.from, self.to, ease_out(self.progress(now)))
    }

    pub(crate) fn moving(&self, now: Instant) -> bool {
        self.progress(now) < 1.0 && self.from != self.to
    }

    /// Head for `to` from wherever the rectangle is now. Reduce Motion (and
    /// a first target) jumps there.
    pub(crate) fn retarget(&mut self, to: Rect, now: Instant, animate: bool) {
        if to == self.to {
            return;
        }
        *self = Self {
            from: if animate { self.at(now) } else { to },
            to,
            start: now,
            length: if animate { GLIDE } else { Duration::ZERO },
        };
    }
}

/// The drag in flight, shared by the views that take part in it.
#[derive(Default)]
pub(crate) struct PaneDragState {
    pub source: Option<DraggedPane>,
    /// What the workbench's panes resolve the pointer to.
    pub pane_target: Option<DropTarget>,
    /// What the sidebar rows, tabs or free space resolve it to.
    pub row_target: Option<DropTarget>,
    pub spring: SpringLoader,
    /// Escape ended the gesture; GPUI keeps the drag until the button
    /// comes up, so every drop is a no-op until then.
    pub cancelled: bool,
    /// The last drop, for the workbench to animate the layout it brings.
    pub landing: Option<Landing>,
}
impl gpui::Global for PaneDragState {}

/// A pane just dropped (or moved by a command): which one, where it was
/// last seen in the workbench (for a pane coming from a tab not on screen,
/// the drop preview), and when.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Landing {
    pub pane: PaneId,
    pub from: Option<Rect>,
    pub at: Instant,
}

/// Ends the drag with a drop: the mutation to send, if the pointer rests
/// on a target that takes the pane. The state resets either way.
pub(crate) fn take_drop(
    snapshot: Option<&WorkspaceSnapshot>,
    from: Option<Rect>,
    cx: &mut gpui::App,
) -> Option<WorkspaceMutation> {
    let state = cx.default_global::<PaneDragState>();
    let mutation = match (&state.source, state.target(), snapshot) {
        (Some(source), Some(target), Some(snapshot)) => drop_mutation(source, target, snapshot),
        _ => None,
    };
    let landing = mutation.as_ref().and(state.source.as_ref()).map(|source| Landing {
        pane: source.pane.clone(),
        from,
        at: now(),
    });
    *state = PaneDragState {
        landing: landing.or(state.landing.take()),
        ..PaneDragState::default()
    };
    mutation
}

/// A command is about to move `pane`: animate the layout it brings.
pub(crate) fn arm(pane: PaneId, cx: &mut gpui::App) {
    cx.default_global::<PaneDragState>().landing = Some(Landing {
        pane,
        from: None,
        at: now(),
    });
}

/// The drag ended without a drop on a target (or the gesture is over):
/// forget it, keeping the last landing.
pub(crate) fn end_if_released(cx: &mut gpui::App) -> bool {
    if cx.has_active_drag() {
        return false;
    }
    let Some(state) = cx.try_global::<PaneDragState>() else {
        return false;
    };
    if state.source.is_none() {
        return false;
    }
    let state = cx.global_mut::<PaneDragState>();
    *state = PaneDragState {
        landing: state.landing.take(),
        ..PaneDragState::default()
    };
    true
}

/// Escape: the gesture is over. True when a pane drag was in flight.
pub(crate) fn cancel(cx: &mut gpui::App) -> bool {
    let Some(state) = cx.try_global::<PaneDragState>() else {
        return false;
    };
    if !state.active() {
        return false;
    }
    let state = cx.global_mut::<PaneDragState>();
    state.cancelled = true;
    state.pane_target = None;
    state.row_target = None;
    state.spring = SpringLoader::default();
    true
}

impl PaneDragState {
    pub(crate) fn target(&self) -> Option<&DropTarget> {
        if self.cancelled {
            return None;
        }
        self.pane_target.as_ref().or(self.row_target.as_ref())
    }

    pub(crate) fn active(&self) -> bool {
        self.source.is_some() && !self.cancelled
    }
}

/// Wall clock, or a stepped one for frame renders.
pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = test_clock::get() {
        return now;
    }
    Instant::now()
}

#[cfg(test)]
pub(crate) mod test_clock {
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    thread_local! {
        static NOW: Cell<Option<Instant>> = const { Cell::new(None) };
    }
    pub(crate) fn get() -> Option<Instant> {
        NOW.with(Cell::get)
    }
    pub(crate) fn start() {
        NOW.with(|now| now.set(Some(Instant::now())));
    }
    pub(crate) fn advance(by: Duration) {
        NOW.with(|now| now.set(now.get().map(|at| at + by)));
    }
    pub(crate) fn stop() {
        NOW.with(|now| now.set(None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{SplitId, WorkspaceId, WorkspaceRecord};

    fn leaf(id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(id),
        }
    }
    fn split(axis: LayoutAxis, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            id: SplitId::new("s"),
            axis,
            fraction: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }
    fn tab(id: &str, layout: LayoutNode, focused: &str) -> WorkspaceTab {
        WorkspaceTab {
            id: TabId::new(id),
            title: None,
            layout,
            focused_pane: PaneId::new(focused),
            zoomed_pane: None,
        }
    }
    fn snapshot(tabs: Vec<WorkspaceTab>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            revision: 1,
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: WorkspaceId::new("w"),
                name: "w".into(),
                selected_tab: Some(tabs[0].id.clone()),
                tabs,
            }],
            ..Default::default()
        }
    }
    fn dragged(tab: &str, pane: &str) -> DraggedPane {
        DraggedPane {
            tab: TabId::new(tab),
            pane: PaneId::new(pane),
            session: SessionId::new(pane),
            title: pane.to_owned().into(),
            colors: SemanticColors::dark(),
            can_leave: true,
            can_break: true,
        }
    }
    const RECT: Rect = Rect {
        x: 100.0,
        y: 50.0,
        width: 400.0,
        height: 200.0,
    };

    #[test]
    fn zones_are_the_nearest_edge_with_a_swap_core() {
        assert_eq!(
            zone(RECT, 110.0, 150.0),
            Some(PaneZone::Edge(DockEdge::Left))
        );
        assert_eq!(
            zone(RECT, 490.0, 150.0),
            Some(PaneZone::Edge(DockEdge::Right))
        );
        assert_eq!(
            zone(RECT, 300.0, 55.0),
            Some(PaneZone::Edge(DockEdge::Top))
        );
        assert_eq!(
            zone(RECT, 300.0, 245.0),
            Some(PaneZone::Edge(DockEdge::Bottom))
        );
        assert_eq!(zone(RECT, 300.0, 150.0), Some(PaneZone::Swap));
        assert_eq!(zone(RECT, 99.0, 150.0), None);
        assert_eq!(zone(RECT, 300.0, 251.0), None);
        // Distances are relative: a wide pane's right fifth is still right.
        assert_eq!(
            zone(RECT, 430.0, 150.0),
            Some(PaneZone::Edge(DockEdge::Right))
        );
    }

    #[test]
    fn previews_cover_the_half_the_engine_splits_off() {
        assert_eq!(
            zone_preview(RECT, PaneZone::Edge(DockEdge::Right)),
            Rect {
                x: 300.0,
                y: 50.0,
                width: 200.0,
                height: 200.0
            }
        );
        assert_eq!(
            zone_preview(RECT, PaneZone::Edge(DockEdge::Top)),
            Rect {
                x: 100.0,
                y: 50.0,
                width: 400.0,
                height: 100.0
            }
        );
        assert_eq!(zone_preview(RECT, PaneZone::Swap), RECT);
    }

    #[test]
    fn a_drop_inside_the_tab_moves_swaps_or_does_nothing() {
        let layout = split(LayoutAxis::Horizontal, leaf("a"), leaf("b"));
        let state = snapshot(vec![tab("t", layout, "a")]);
        let source = dragged("t", "a");
        let on = |pane: &str, zone| DropTarget::Pane {
            tab: TabId::new("t"),
            pane: PaneId::new(pane),
            zone,
        };
        // a | b: a on b's left edge is where it already is.
        assert_eq!(
            drop_mutation(&source, &on("b", PaneZone::Edge(DockEdge::Left)), &state),
            None
        );
        // Onto itself: nothing.
        assert_eq!(
            drop_mutation(&source, &on("a", PaneZone::Edge(DockEdge::Right)), &state),
            None
        );
        assert_eq!(
            drop_mutation(&source, &on("b", PaneZone::Edge(DockEdge::Bottom)), &state),
            Some(WorkspaceMutation::MoveNode {
                source_tab: TabId::new("t"),
                node: LayoutNodeId::Pane(PaneId::new("a")),
                destination_tab: TabId::new("t"),
                target: PaneId::new("b"),
                edge: DockEdge::Bottom,
            })
        );
        assert_eq!(
            drop_mutation(&source, &on("b", PaneZone::Swap), &state),
            Some(WorkspaceMutation::SwapPanes {
                first_tab: TabId::new("t"),
                first: PaneId::new("a"),
                second_tab: TabId::new("t"),
                second: PaneId::new("b"),
            })
        );
    }

    #[test]
    fn a_drop_on_a_tab_docks_beside_its_focused_pane() {
        let state = snapshot(vec![
            tab(
                "t",
                split(LayoutAxis::Vertical, leaf("agent"), leaf("shell")),
                "agent",
            ),
            tab(
                "u",
                split(LayoutAxis::Horizontal, leaf("other"), leaf("log")),
                "log",
            ),
        ]);
        let source = dragged("t", "shell");
        assert_eq!(
            drop_mutation(&source, &DropTarget::Tab(SessionId::new("other")), &state),
            Some(WorkspaceMutation::MoveNode {
                source_tab: TabId::new("t"),
                node: LayoutNodeId::Pane(PaneId::new("shell")),
                destination_tab: TabId::new("u"),
                target: PaneId::new("log"),
                edge: DockEdge::Right,
            })
        );
        // Its own tab's row, or a session no tab shows yet: nothing.
        assert_eq!(
            drop_mutation(&source, &DropTarget::Tab(SessionId::new("agent")), &state),
            None
        );
        assert_eq!(
            drop_mutation(&source, &DropTarget::Tab(SessionId::new("nowhere")), &state),
            None
        );
        // A pane that would strand its tab goes nowhere else.
        let stuck = DraggedPane {
            can_leave: false,
            ..source.clone()
        };
        assert_eq!(
            drop_mutation(&stuck, &DropTarget::Tab(SessionId::new("other")), &state),
            None
        );
        assert_eq!(
            drop_mutation(
                &stuck,
                &DropTarget::Pane {
                    tab: TabId::new("u"),
                    pane: PaneId::new("other"),
                    zone: PaneZone::Edge(DockEdge::Top),
                },
                &state
            ),
            None
        );
    }

    #[test]
    fn free_space_breaks_a_pane_out_only_when_it_can_have_a_row() {
        let state = snapshot(vec![
            tab(
                "t",
                split(LayoutAxis::Horizontal, leaf("a"), leaf("b")),
                "a",
            ),
            tab("lone", leaf("c"), "c"),
        ]);
        assert_eq!(
            drop_mutation(&dragged("t", "b"), &DropTarget::NewTab, &state),
            Some(WorkspaceMutation::BreakPane {
                tab_id: TabId::new("t"),
                pane_id: PaneId::new("b"),
            })
        );
        let shell = DraggedPane {
            can_break: false,
            ..dragged("t", "b")
        };
        assert_eq!(drop_mutation(&shell, &DropTarget::NewTab, &state), None);
        assert_eq!(
            drop_mutation(&dragged("lone", "c"), &DropTarget::NewTab, &state),
            None
        );
    }

    #[test]
    fn a_tab_of_shells_alone_is_never_left_behind() {
        let tab = tab(
            "t",
            split(
                LayoutAxis::Horizontal,
                leaf("agent"),
                split(LayoutAxis::Vertical, leaf("shell1"), leaf("shell2")),
            ),
            "agent",
        );
        let has_row = |session: &SessionId| session.0 == "agent";
        assert!(can_leave(&tab, &PaneId::new("shell1"), has_row));
        assert!(!can_leave(&tab, &PaneId::new("agent"), has_row));
        let lone = super::tests::tab("l", leaf("agent"), "agent");
        assert!(can_leave(&lone, &PaneId::new("agent"), has_row));
    }

    #[test]
    fn spring_load_opens_a_rested_on_row_once_after_the_delay() {
        let start = Instant::now();
        let mut spring = SpringLoader::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        // A new rest asks for a timer; staying on the row does not.
        assert!(spring.hover(Some(&a), start));
        assert!(!spring.hover(Some(&a), start + Duration::from_millis(200)));
        let (_, halfway) = spring.progress(start + Duration::from_millis(250)).unwrap();
        assert!((halfway - 0.5).abs() < 0.01);
        let (_, full) = spring.progress(start + Duration::from_secs(3)).unwrap();
        assert_eq!(full, 1.0);
        // The timer for the row fires once.
        assert!(spring.fire(&a));
        assert!(!spring.fire(&a), "once");
        // A timer for a row the pane already left does nothing.
        assert!(spring.hover(Some(&b), start + Duration::from_secs(3)));
        assert!(!spring.fire(&a));
        // Leaving every row cancels the wait.
        spring.hover(None, start + Duration::from_secs(4));
        assert!(!spring.fire(&b));
        assert_eq!(spring.progress(start), None);
        // Coming back to a row that already opened waits again.
        assert!(spring.hover(Some(&a), start + Duration::from_secs(5)));
        assert!(spring.fire(&a));
    }

    #[test]
    fn the_preview_glides_between_targets_unless_motion_is_reduced() {
        let start = Instant::now();
        let a = RECT;
        let b = Rect { x: 500.0, ..RECT };
        let mut glide = Glide::still(a, start);
        glide.retarget(b, start, true);
        assert_eq!(glide.at(start), a);
        assert!(glide.moving(start + GLIDE / 2));
        let middle = glide.at(start + GLIDE / 2);
        assert!(middle.x > 100.0 && middle.x < 500.0);
        assert_eq!(glide.at(start + GLIDE), b);
        assert!(!glide.moving(start + GLIDE));
        // Interrupted: it heads off from where it is, not where it began.
        let mut interrupted = Glide::still(a, start);
        interrupted.retarget(b, start, true);
        let at = interrupted.at(start + GLIDE / 2);
        interrupted.retarget(a, start + GLIDE / 2, true);
        assert_eq!(interrupted.at(start + GLIDE / 2), at);
        let mut reduced = Glide::still(a, start);
        reduced.retarget(b, start, false);
        assert_eq!(reduced.at(start), b);
        assert!(!reduced.moving(start));
    }
}
