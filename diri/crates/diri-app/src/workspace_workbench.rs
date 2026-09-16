//! Mounted layout views. Saved panes reference sessions; one explicit visible
//! view per session owns geometry in the active window.
mod commands;
pub(crate) use commands::PaneCommand;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::{
    icons::sf_symbol,
    store::StoreRuntime,
    terminal_pane::{TerminalPane, TerminalPaneEvent, TerminalViewport},
    workspace_geometry::{PaneIdentity, Rect, WorkspaceGeometry},
};
use diri_proto::{
    SessionId,
    workspace::{
        DockEdge, LayoutAxis, LayoutNode, LayoutNodeId, PaneId, SplitId, TabId, WorkspaceMutation,
        WorkspaceTab,
    },
};
use diri_ui::{Fill, Metrics, Radius};
use gpui::{
    Context, CursorStyle, DragMoveEvent, Entity, EventEmitter, MouseButton, Render, Role,
    SharedString, Subscription, Window, div, prelude::*, px,
};

#[derive(Clone)]
pub(crate) enum WorkspaceWorkbenchEvent {
    Terminal(TerminalPaneEvent),
    Notice(String),
    RequestSplit {
        tab: TabId,
        pane: PaneId,
        edge: DockEdge,
    },
}
#[derive(Clone)]
struct DraggedWorkspacePane {
    tab: TabId,
    pane: PaneId,
    revision: u64,
}
impl Render for DraggedWorkspacePane {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(gpui::rgba(0x34363aff))
            .text_color(gpui::white())
            .child("Move pane · center to swap")
    }
}
fn dock_edge(x: f32, y: f32, width: f32, height: f32) -> Option<DockEdge> {
    let normalized = [
        (x / width, DockEdge::Left),
        ((width - x) / width, DockEdge::Right),
        (y / height, DockEdge::Top),
        ((height - y) / height, DockEdge::Bottom),
    ];
    normalized
        .into_iter()
        .filter(|(distance, _)| *distance < 0.25)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, edge)| edge)
}

#[derive(Clone)]
struct DraggedWorkspaceDivider;
impl Render for DraggedWorkspaceDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}
struct ResizeDraft {
    split: SplitId,
    axis: LayoutAxis,
    parent: Rect,
    revision: u64,
    original: LayoutNode,
    fraction: f32,
    submitted: bool,
}
fn set_fraction(node: &mut LayoutNode, id: &SplitId, value: f32) -> bool {
    match node {
        LayoutNode::Pane { .. } => false,
        LayoutNode::Split {
            id: split,
            fraction,
            first,
            second,
            ..
        } => {
            if split == id {
                *fraction = value;
                true
            } else {
                set_fraction(first, id, value) || set_fraction(second, id, value)
            }
        }
    }
}

struct MountedPane {
    session: SessionId,
    terminal: Entity<TerminalPane>,
    _focus: Subscription,
    _events: Subscription,
    _output: Subscription,
}

pub(crate) struct WorkspaceWorkbench {
    runtime: Arc<StoreRuntime>,
    tokio: Arc<tokio::runtime::Runtime>,
    window_store: Option<crate::store::WindowStore>,
    tab: Option<WorkspaceTab>,
    enabled: bool,
    placeholder_focus: gpui::FocusHandle,
    external_owner: Option<SessionId>,
    mounted: HashMap<PaneId, MountedPane>,
    recent: VecDeque<PaneId>,
    catalog_revision: Option<u64>,
    viewport: TerminalViewport,
    pending_focus: Option<PaneId>,
    sent_focus: Option<PaneId>,
    resize: Option<ResizeDraft>,
    _activation: Subscription,
}
impl EventEmitter<WorkspaceWorkbenchEvent> for WorkspaceWorkbench {}

/// Stable selection prevents render-order changes from alternating a shared
/// PTY between two sizes. The focused duplicate wins; otherwise lowest PaneId.
fn visible_owners(panes: &[PaneIdentity], focused: &PaneId) -> HashMap<SessionId, PaneId> {
    let mut owners: HashMap<SessionId, PaneId> = HashMap::new();
    for pane in panes {
        owners
            .entry(pane.session.clone())
            .and_modify(|owner| {
                if pane.pane == *focused || (*owner != *focused && pane.pane.0 < owner.0) {
                    *owner = pane.pane.clone();
                }
            })
            .or_insert_with(|| pane.pane.clone());
    }
    owners
}
fn leaves(node: &LayoutNode, output: &mut Vec<PaneIdentity>) {
    match node {
        LayoutNode::Pane { id, session_id } => output.push(PaneIdentity {
            pane: id.clone(),
            session: session_id.clone(),
        }),
        LayoutNode::Split { first, second, .. } => {
            leaves(first, output);
            leaves(second, output);
        }
    }
}

impl WorkspaceWorkbench {
    pub(crate) fn new(
        runtime: Arc<StoreRuntime>,
        tokio: Arc<tokio::runtime::Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.cancel_resize();
            }
            this.assign_visible_owners(window, cx);
            cx.notify();
        });
        Self {
            runtime,
            tokio,
            window_store: None,
            tab: None,
            enabled: false,
            placeholder_focus: cx.focus_handle(),
            external_owner: None,
            mounted: HashMap::new(),
            recent: VecDeque::new(),
            catalog_revision: None,
            viewport: TerminalViewport::default(),
            pending_focus: None,
            sent_focus: None,
            resize: None,
            _activation: activation,
        }
    }

    pub(crate) fn set_window_store(
        &mut self,
        store: crate::store::WindowStore,
        cx: &mut Context<Self>,
    ) {
        for pane in self.mounted.values() {
            pane.terminal
                .update(cx, |terminal, _| terminal.set_window_store(store.clone()));
        }
        self.window_store = Some(store);
    }

    pub(crate) fn set_tab(
        &mut self,
        tab: WorkspaceTab,
        viewport: TerminalViewport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enabled = true;
        let focus_changed = self.pending_focus.is_none()
            && self
                .tab
                .as_ref()
                .is_some_and(|current| current.focused_pane != tab.focused_pane);
        let switched = self.tab.as_ref().is_none_or(|current| current.id != tab.id);
        let previous_tab = self.tab.clone();
        let viewport_changed = self.viewport != viewport;
        if switched {
            self.pending_focus = None;
            self.sent_focus = None;
        }
        self.viewport = viewport;
        self.tab = Some(tab);
        if let Some(resize) = &self.resize {
            let current = self
                .runtime
                .store
                .read()
                .expect("store")
                .workspace_catalog()
                .snapshot()
                .map(|snapshot| snapshot.revision);
            if current != Some(resize.revision)
                || switched
                || (resize.submitted
                    && self
                        .runtime
                        .store
                        .read()
                        .expect("store")
                        .workspace_catalog()
                        .can_edit())
            {
                self.resize = None;
            } else if let Some(tab) = &mut self.tab {
                set_fraction(&mut tab.layout, &resize.split, resize.fraction);
            }
        }
        if let Some(sent) = &self.sent_focus {
            let store = self.runtime.store.read().expect("store");
            if store.workspace_catalog().can_edit() {
                if self
                    .tab
                    .as_ref()
                    .is_none_or(|tab| tab.focused_pane != *sent)
                    || store.workspace_catalog().error.is_some()
                {
                    self.pending_focus = None; // external/conflicting layout wins
                }
                self.sent_focus = None;
            }
        }
        let changed = previous_tab != self.tab || viewport_changed;
        self.reconcile(window, cx);
        self.flush_focus();
        self.assign_visible_owners(window, cx);
        if switched || focus_changed {
            self.focus(window, cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = &self.tab else {
            return;
        };
        let mut required = Vec::new();
        leaves(&tab.layout, &mut required);
        {
            let store = self.runtime.store.read().expect("store");
            let revision = store
                .workspace_catalog()
                .snapshot()
                .map(|snapshot| snapshot.revision);
            if self.catalog_revision != revision {
                // Validate warm references only when the authoritative catalog
                // changes; terminal repaints do not scan every saved layout.
                let mut valid = Vec::new();
                if let Some(snapshot) = store.workspace_catalog().snapshot() {
                    for workspace in &snapshot.workspaces {
                        for tab in &workspace.tabs {
                            leaves(&tab.layout, &mut valid);
                        }
                    }
                }
                self.mounted.retain(|id, pane| {
                    valid
                        .iter()
                        .any(|valid| valid.pane == *id && valid.session == pane.session)
                });
                self.catalog_revision = revision;
            }
            self.mounted
                .retain(|_, pane| store.sessions().contains_key(&pane.session));
        }
        // Keep at most two full eight-pane tabs warm. Pane IDs survive tab
        // moves and swaps; dropping an evicted view never terminates its PTY.
        self.mounted.retain(|id, pane| {
            required
                .iter()
                .find(|required| required.pane == *id)
                .is_none_or(|required| required.session == pane.session)
        });
        self.recent.retain(|id| self.mounted.contains_key(id));
        let required_ids = required
            .iter()
            .map(|pane| pane.pane.clone())
            .collect::<HashSet<_>>();
        for pane in &required {
            self.recent.retain(|id| *id != pane.pane);
            self.recent.push_back(pane.pane.clone());
        }
        while self.recent.len() > 16 {
            let oldest = self.recent.pop_front().unwrap();
            if !required_ids.contains(&oldest) {
                self.mounted.remove(&oldest);
            }
        }
        for identity in required {
            if !self
                .runtime
                .store
                .read()
                .expect("store")
                .sessions()
                .contains_key(&identity.session)
                || self.mounted.contains_key(&identity.pane)
            {
                continue;
            }
            let runtime = self.runtime.clone();
            let tokio = self.tokio.clone();
            let id = identity.session.clone();
            let terminal = cx.new(|cx| TerminalPane::new_fixed(runtime, tokio, id, window, cx));
            if let Some(store) = &self.window_store {
                terminal.update(cx, |terminal, _| terminal.set_window_store(store.clone()));
            }
            let focus_handle = terminal.read(cx).quote_focus_handle();
            let pane_id = identity.pane.clone();
            let focus = cx.on_focus(&focus_handle, window, move |this, window, cx| {
                if window.is_window_active() {
                    this.pending_focus = Some(pane_id.clone());
                    this.flush_focus();
                    this.assign_visible_owners(window, cx);
                    cx.notify();
                }
            });
            let events = cx.subscribe(&terminal, |_, _, event: &TerminalPaneEvent, cx| {
                cx.emit(WorkspaceWorkbenchEvent::Terminal(event.clone()));
            });
            let output = cx.observe(&terminal, |_, _, cx| cx.notify());
            self.mounted.insert(
                identity.pane,
                MountedPane {
                    session: identity.session,
                    terminal,
                    _focus: focus,
                    _events: events,
                    _output: output,
                },
            );
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn send_owned_fixture_input(&self, cx: &gpui::App) -> Vec<(SessionId, u16, u16)> {
        self.mounted
            .values()
            .filter_map(|pane| pane.terminal.read(cx).send_owned_fixture_input())
            .collect()
    }
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn seed_panes_for_test(&self, cx: &mut Context<Self>) {
        let mut seeded = HashSet::new();
        for pane in self.mounted.values() {
            if !seeded.insert(pane.session.clone()) {
                continue;
            }
            let mut grid = diri_term::buffer::GridBuffer::new(100, 36);
            let text = format!(
                "$ pwd\n/work/diri\n\n$ cargo test\nrunning 4 tests\ntest stable_session_identity ... ok\ntest saved_layout_restores ... ok\ntest passive_views_do_not_resize ... ok\ntest input_stays_ordered ... ok\n\nSession: {}\n\n$ ",
                pane.session.0
            );
            for (y, line) in text.lines().enumerate() {
                for (x, ch) in line.chars().enumerate() {
                    grid.cells[y * 100 + x].scalar = ch as u32;
                }
            }
            pane.terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(grid, cx)
            });
        }
    }

    pub(crate) fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.enabled = false;
        self.cancel_resize();
        for pane in self.mounted.values() {
            pane.terminal
                .update(cx, |terminal, _| terminal.release_layout_control());
        }
    }

    fn focused_id(&self) -> Option<&PaneId> {
        self.pending_focus
            .as_ref()
            .filter(|id| {
                self.tab
                    .as_ref()
                    .is_some_and(|tab| commands::contains_pane(&tab.layout, id))
            })
            .or_else(|| self.tab.as_ref().map(|tab| &tab.focused_pane))
    }
    pub(crate) fn resident_preview_buffers(
        &self,
        cx: &gpui::App,
    ) -> HashMap<SessionId, diri_term::element::SharedGridBuffer> {
        self.mounted
            .values()
            .flat_map(|pane| pane.terminal.read(cx).resident_preview_buffers())
            .collect()
    }
    pub(crate) fn set_external_owner(&mut self, session: Option<SessionId>) {
        self.external_owner = session;
    }
    pub(crate) fn visible_session(&self, session: &SessionId) -> bool {
        self.enabled
            && self.geometry().is_some_and(|geometry| {
                geometry
                    .panes
                    .iter()
                    .any(|pane| &pane.identity.session == session)
            })
    }
    pub(crate) fn focused_session_id(&self) -> Option<SessionId> {
        if !self.enabled {
            return None;
        }
        self.focused_id()
            .and_then(|id| self.mounted.get(id))
            .map(|pane| pane.session.clone())
    }
    pub(crate) fn focused_terminal(&self) -> Option<Entity<TerminalPane>> {
        if !self.enabled {
            return None;
        }
        self.focused_id()
            .and_then(|id| self.mounted.get(id))
            .map(|pane| pane.terminal.clone())
    }
    pub(crate) fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.focused_terminal() {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        } else {
            window.focus(&self.placeholder_focus, cx);
        }
        self.assign_visible_owners(window, cx);
    }
    fn flush_focus(&mut self) {
        let (Some(pending), Some(tab)) = (&self.pending_focus, &self.tab) else {
            return;
        };
        if *pending == tab.focused_pane {
            self.pending_focus = None;
            return;
        }
        if self.sent_focus.is_none()
            && self.runtime.store.write().expect("store").edit_workspace(
                WorkspaceMutation::FocusPane {
                    tab_id: tab.id.clone(),
                    pane_id: pending.clone(),
                },
            )
        {
            self.sent_focus = Some(pending.clone());
        }
        // Keep local focus until the authoritative snapshot acknowledges it.
    }
    fn drop_pane(
        &mut self,
        dragged: &DraggedWorkspacePane,
        target: PaneId,
        bounds: Rect,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = &self.tab else {
            return;
        };
        if dragged.tab == tab.id && dragged.pane == target {
            return;
        }
        let position = window.mouse_position();
        let edge = dock_edge(
            f32::from(position.x) - self.viewport.x - bounds.x,
            f32::from(position.y) - self.viewport.y - bounds.y,
            bounds.width,
            bounds.height,
        );
        let mutation = if let Some(edge) = edge {
            WorkspaceMutation::MoveNode {
                source_tab: dragged.tab.clone(),
                node: LayoutNodeId::Pane(dragged.pane.clone()),
                destination_tab: tab.id.clone(),
                target,
                edge,
            }
        } else {
            WorkspaceMutation::SwapPanes {
                first_tab: dragged.tab.clone(),
                first: dragged.pane.clone(),
                second_tab: tab.id.clone(),
                second: target,
            }
        };
        let mut store = self.runtime.store.write().expect("store");
        if store
            .workspace_catalog()
            .snapshot()
            .map(|snapshot| snapshot.revision)
            != Some(dragged.revision)
            || !store.edit_workspace(mutation)
        {
            cx.emit(WorkspaceWorkbenchEvent::Notice(
                "The layout changed while moving. Try the move again.".into(),
            ));
        }
        cx.notify();
    }

    fn begin_resize(&mut self, divider: crate::workspace_geometry::DividerPlacement) {
        let store = self.runtime.store.read().expect("store");
        if !store.workspace_catalog().can_edit() {
            return;
        }
        let Some(revision) = store
            .workspace_catalog()
            .snapshot()
            .map(|snapshot| snapshot.revision)
        else {
            return;
        };
        let Some(tab) = &self.tab else {
            return;
        };
        self.resize = Some(ResizeDraft {
            split: divider.id,
            axis: divider.axis,
            parent: divider.parent,
            revision,
            original: tab.layout.clone(),
            fraction: divider.fraction,
            submitted: false,
        });
    }
    fn drag_resize(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some(resize) = &mut self.resize else {
            return;
        };
        if resize.submitted {
            return;
        }
        let (position, available) = if resize.axis == LayoutAxis::Horizontal {
            (
                x - self.viewport.x - resize.parent.x,
                resize.parent.width - crate::workspace_geometry::DIVIDER,
            )
        } else {
            (
                y - self.viewport.y - resize.parent.y,
                resize.parent.height - crate::workspace_geometry::DIVIDER,
            )
        };
        if available <= 0.0 {
            return;
        }
        resize.fraction = (position / available).clamp(0.1, 0.9);
        if let Some(tab) = &mut self.tab {
            set_fraction(&mut tab.layout, &resize.split, resize.fraction);
        }
        cx.notify();
    }
    fn finish_resize(&mut self, cx: &mut Context<Self>) {
        let Some(mut resize) = self.resize.take() else {
            return;
        };
        if resize.submitted {
            self.resize = Some(resize);
            return;
        }
        let Some(tab) = &mut self.tab else {
            return;
        };
        let mut store = self.runtime.store.write().expect("store");
        let current = store
            .workspace_catalog()
            .snapshot()
            .map(|snapshot| snapshot.revision);
        let accepted = current == Some(resize.revision)
            && store.edit_workspace(WorkspaceMutation::ResizeSplit {
                tab_id: tab.id.clone(),
                split_id: resize.split.clone(),
                fraction: resize.fraction,
            });
        if accepted {
            resize.submitted = true;
            self.resize = Some(resize);
        } else {
            tab.layout = resize.original;
            cx.emit(WorkspaceWorkbenchEvent::Notice(
                "The layout changed while resizing. Try the resize again.".into(),
            ));
        }
        cx.notify();
    }
    fn cancel_resize(&mut self) {
        if self.resize.as_ref().is_some_and(|resize| resize.submitted) {
            return;
        }
        if let Some(resize) = self.resize.take()
            && let Some(tab) = &mut self.tab
        {
            tab.layout = resize.original;
        }
    }

    fn geometry(&self) -> Option<WorkspaceGeometry> {
        let mut tab = self.tab.clone()?;
        if let Some(focused) = self.focused_id() {
            if tab.focused_pane != *focused {
                tab.zoomed_pane = tab.zoomed_pane.as_ref().map(|_| focused.clone());
            }
            tab.focused_pane = focused.clone();
        }
        WorkspaceGeometry::settled(
            &tab,
            Rect {
                width: self.viewport.width,
                height: self.viewport.height,
                ..Rect::default()
            },
        )
    }
    fn assign_visible_owners(&self, window: &Window, cx: &mut Context<Self>) {
        let Some(geometry) = self.geometry() else {
            return;
        };
        let identities = geometry
            .panes
            .iter()
            .map(|pane| pane.identity.clone())
            .collect::<Vec<_>>();
        let owners = visible_owners(&identities, &geometry.focused.pane);
        for (id, mounted) in &self.mounted {
            let owns = self.enabled
                && self.external_owner.as_ref() != Some(&mounted.session)
                && window.is_window_active()
                && owners.get(&mounted.session) == Some(id);
            mounted.terminal.update(cx, |terminal, _| {
                if owns {
                    terminal.claim_layout_control(window);
                } else {
                    terminal.release_layout_control();
                }
            });
        }
    }
}

impl Render for WorkspaceWorkbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = {
            let store = self.runtime.store.read().expect("store");
            crate::app_theme::colors_in(&store)
        };
        let mut root = div()
            .id("workspace-workbench")
            .debug_selector(|| "workspace-workbench".into())
            .track_focus(&self.placeholder_focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(colors.work_surface_nested())
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedWorkspaceDivider>, _, cx| {
                    this.drag_resize(
                        f32::from(event.event.position.x),
                        f32::from(event.event.position.y),
                        cx,
                    );
                },
            ))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.resize.is_some() {
                    this.cancel_resize();
                    cx.stop_propagation();
                    cx.notify();
                }
            }));
        let Some(geometry) = self.geometry() else {
            return root.child("Choose a tab to continue");
        };
        self.assign_visible_owners(window, cx);
        let can_edit = self
            .runtime
            .store
            .read()
            .expect("store")
            .workspace_catalog()
            .can_edit();
        for divider in &geometry.dividers {
            let divider = divider.clone();
            let bounds = divider.bounds;
            let horizontal = divider.axis == LayoutAxis::Horizontal;
            let drag = divider.clone();
            root = root.child(
                div()
                    .id(SharedString::from(format!(
                        "workspace-divider-{}",
                        divider.id.0
                    )))
                    .debug_selector(move || format!("workspace-divider-{}", divider.id.0))
                    .absolute()
                    .left(px(bounds.x))
                    .top(px(bounds.y))
                    .w(px(bounds.width))
                    .h(px(bounds.height))
                    .cursor(if horizontal {
                        CursorStyle::ResizeLeftRight
                    } else {
                        CursorStyle::ResizeUpDown
                    })
                    .bg(colors.primary.alpha(0.07))
                    .hover(move |line| line.bg(gpui::rgba(0x4f83f1ff).alpha(0.6)))
                    .on_drag(DraggedWorkspaceDivider, |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| DraggedWorkspaceDivider)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.begin_resize(drag.clone());
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_resize(cx)),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_resize(cx)),
                    ),
            );
        }
        let multiple_panes = self.tab.as_ref().is_some_and(|tab| {
            matches!(tab.layout, LayoutNode::Split { .. })
        });
        for pane in &geometry.panes {
            let bounds = pane.bounds;
            let mut surface = div()
                .id(SharedString::from(format!(
                    "workspace-pane-{}",
                    pane.identity.pane.0
                )))
                .absolute()
                .left(px(bounds.x))
                .top(px(bounds.y))
                .w(px(bounds.width))
                .h(px(bounds.height))
                .overflow_hidden();
            if let Some(mounted) = self.mounted.get(&pane.identity.pane) {
                mounted.terminal.update(cx, |terminal, cx| {
                    terminal.set_header_trailing_inset(
                        if multiple_panes { 116.0 } else { 26.0 },
                        cx,
                    );
                    terminal.set_viewport(
                        TerminalViewport {
                            x: self.viewport.x + bounds.x,
                            y: self.viewport.y + bounds.y,
                            width: bounds.width,
                            height: bounds.height,
                        },
                        cx,
                    );
                });
                surface = surface.child(mounted.terminal.clone());
            } else {
                surface = surface.child(
                    div()
                        .p(px(18.0))
                        .text_color(colors.secondary)
                        .child("Session unavailable")
                        .child(
                            div()
                                .text_size(px(12.0))
                                .child("This saved pane stays in the layout."),
                        ),
                );
            }
            let tab = geometry.tab.clone();
            let pane_id = pane.identity.pane.clone();
            let split_tab = tab.clone();
            let split_pane = pane_id.clone();
            let zoom_tab = tab.clone();
            let zoom_pane = pane_id.clone();
            let zoomed = self
                .tab
                .as_ref()
                .is_some_and(|tab| tab.zoomed_pane.as_ref() == Some(&pane_id));
            let move_source = DraggedWorkspacePane {
                tab: tab.clone(),
                pane: pane_id.clone(),
                revision: self
                    .runtime
                    .store
                    .read()
                    .expect("store")
                    .workspace_catalog()
                    .snapshot()
                    .map_or(0, |snapshot| snapshot.revision),
            };
            let drop_target = pane_id.clone();
            surface = surface
                .drag_over::<DraggedWorkspacePane>(move |surface, dragged, _, _| {
                    if dragged.pane != drop_target {
                        surface.border_1().border_color(gpui::rgba(0x4f83f1ff))
                    } else {
                        surface
                    }
                })
                .on_drop(cx.listener({
                    let target = pane_id.clone();
                    move |this, dragged: &DraggedWorkspacePane, window, cx| {
                        this.drop_pane(dragged, target.clone(), bounds, window, cx);
                        cx.stop_propagation();
                    }
                }));
            let controls = div()
                .absolute()
                .top(px((Metrics::TITLE_BAR - Metrics::TOOLBAR_CONTROL_SIZE) / 2.0))
                .right(px(Metrics::TOOLBAR_EDGE_INSET))
                .flex()
                .gap(px(4.0))
                .when(multiple_panes, |controls| controls.child(
                    div()
                        .id(SharedString::from(format!("move-pane-{}", pane_id.0)))
                        .role(Role::Button)
                        .aria_label("Drag pane to an edge to move, or center to swap")
                        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                        .rounded(px(Radius::BADGE))
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(CursorStyle::OpenHand)
                        .child(sf_symbol("arrow.up.arrow.down", 14.0, colors.secondary))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_drag(move_source, |source, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| source.clone())
                        }),
                ))
                .child(
                    div()
                        .id(SharedString::from(format!("split-pane-{}", pane_id.0)))
                        .role(Role::Button)
                        .aria_label("Split pane")
                        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                        .rounded(px(Radius::BADGE))
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child(sf_symbol("rectangle.split.2x1", 14.0, colors.secondary))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if can_edit {
                                cx.emit(WorkspaceWorkbenchEvent::RequestSplit {
                                    tab: split_tab.clone(),
                                    pane: split_pane.clone(),
                                    edge: DockEdge::Right,
                                });
                            }
                        })),
                )
                .when(multiple_panes, |controls| controls.child(
                    div()
                        .id(SharedString::from(format!("zoom-pane-{}", pane_id.0)))
                        .role(Role::Button)
                        .aria_label(if zoomed { "Show all panes" } else { "Focus this pane" })
                        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                        .rounded(px(Radius::BADGE))
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child(sf_symbol(
                            if zoomed {
                                "arrow.down.right.and.arrow.up.left"
                            } else {
                                "arrow.up.left.and.arrow.down.right"
                            },
                            14.0,
                            colors.secondary,
                        ))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.runtime.store.write().expect("store").edit_workspace(
                                WorkspaceMutation::ZoomPane {
                                    tab_id: zoom_tab.clone(),
                                    pane_id: (!zoomed).then(|| zoom_pane.clone()),
                                },
                            );
                            cx.notify();
                        })),
                ))
                .when(multiple_panes, |controls| controls.child(
                    div()
                        .id(SharedString::from(format!("remove-pane-{}", pane_id.0)))
                        .role(Role::Button)
                        .aria_label("Remove pane from tab")
                        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                        .rounded(px(Radius::BADGE))
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child(sf_symbol("xmark", 12.0, colors.secondary))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.runtime.store.write().expect("store").edit_workspace(
                                WorkspaceMutation::RemovePane {
                                    tab_id: tab.clone(),
                                    pane_id: pane_id.clone(),
                                },
                            );
                            cx.notify();
                        })),
                ));
            if geometry.panes.len() > 1 && geometry.focused.pane == pane.identity.pane {
                surface = surface.child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .left(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .border_1()
                        .border_color(colors.primary.alpha(0.16)),
                );
            }
            surface = surface.child(controls);
            root = root.child(surface);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{WorkspaceId, WorkspaceRecord, WorkspaceSnapshot};
    use gpui::TestAppContext;

    fn tab(duplicate: bool) -> WorkspaceTab {
        WorkspaceTab {
            id: TabId::new("tab"),
            title: None,
            focused_pane: PaneId::new("a"),
            zoomed_pane: None,
            layout: LayoutNode::Split {
                id: SplitId::new("divider"),
                axis: LayoutAxis::Horizontal,
                fraction: 0.6,
                first: Box::new(LayoutNode::Pane {
                    id: PaneId::new("a"),
                    session_id: SessionId::new("preview-claude"),
                }),
                second: Box::new(LayoutNode::Pane {
                    id: PaneId::new("b"),
                    session_id: SessionId::new(if duplicate {
                        "preview-claude"
                    } else {
                        "preview-codex"
                    }),
                }),
            },
        }
    }
    fn seed(runtime: &StoreRuntime, tabs: Vec<WorkspaceTab>, revision: u64) {
        let mut store = runtime.store.write().unwrap();
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
            revision,
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: WorkspaceId::new("workspace"),
                name: "Release".into(),
                selected_tab: tabs.first().map(|tab| tab.id.clone()),
                tabs,
            }],
            ..Default::default()
        });
    }
    fn fixture(
        duplicate: bool,
    ) -> (
        Arc<StoreRuntime>,
        Arc<tokio::runtime::Runtime>,
        WorkspaceTab,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical)
                .list,
        );
        let tab = tab(duplicate);
        seed(&runtime, vec![tab.clone()], 1);
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        (runtime, tokio, tab)
    }
    fn viewport() -> TerminalViewport {
        TerminalViewport {
            width: 900.0,
            height: 600.0,
            ..Default::default()
        }
    }
    fn owners(workbench: &WorkspaceWorkbench, cx: &gpui::App) -> Vec<PaneId> {
        let mut owners = workbench
            .mounted
            .iter()
            .filter(|(_, pane)| pane.terminal.read(cx).layout_owner_for_test())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        owners.sort_by(|a, b| a.0.cmp(&b.0));
        owners
    }
    #[gpui::test]
    fn keyboard_focus_on_unavailable_reference_leaves_live_terminal_input(cx: &mut TestAppContext) {
        let (runtime, tokio, mut tab) = fixture(false);
        if let LayoutNode::Split { second, .. } = &mut tab.layout
            && let LayoutNode::Pane { session_id, .. } = second.as_mut()
        {
            *session_id = SessionId::new("unavailable");
        }
        seed(&runtime, vec![tab.clone()], 2);
        let window =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime, tokio, window, cx));
        window
            .update(cx, |workbench, window, cx| {
                window.activate_window();
                workbench.set_tab(tab, viewport(), window, cx);
                workbench.execute_command(PaneCommand::Focus(DockEdge::Right), window, cx);
                assert_eq!(workbench.focused_id(), Some(&PaneId::new("b")));
                assert!(workbench.focused_terminal().is_none());
                assert!(workbench.placeholder_focus.is_focused(window));
            })
            .unwrap();
    }

    #[gpui::test]
    fn keyboard_focus_keeps_zoom_and_shared_session_identity(cx: &mut TestAppContext) {
        let (runtime, tokio, mut tab) = fixture(true);
        tab.zoomed_pane = Some(PaneId::new("a"));
        seed(&runtime, vec![tab.clone()], 2);
        let window =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime, tokio, window, cx));
        window
            .update(cx, |workbench, window, cx| {
                window.activate_window();
                workbench.set_tab(tab, viewport(), window, cx);
                let before = workbench.mounted[&PaneId::new("a")].terminal.clone();
                workbench.execute_command(PaneCommand::Focus(DockEdge::Right), window, cx);
                let geometry = workbench.geometry().unwrap();
                assert_eq!(geometry.panes.len(), 1);
                assert_eq!(geometry.focused.pane, PaneId::new("b"));
                assert_eq!(geometry.focused.session, SessionId::new("preview-claude"));
                assert_eq!(workbench.mounted[&PaneId::new("a")].terminal, before);
            })
            .unwrap();
    }

    #[test]
    fn visible_owner_is_stable_and_focused_duplicate_wins() {
        let panes = vec![
            PaneIdentity {
                pane: PaneId::new("b"),
                session: SessionId::new("one"),
            },
            PaneIdentity {
                pane: PaneId::new("a"),
                session: SessionId::new("one"),
            },
            PaneIdentity {
                pane: PaneId::new("c"),
                session: SessionId::new("two"),
            },
        ];
        let mut reversed = panes.clone();
        reversed.reverse();
        assert_eq!(
            visible_owners(&panes, &PaneId::new("c")),
            visible_owners(&reversed, &PaneId::new("c"))
        );
        assert_eq!(
            visible_owners(&panes, &PaneId::new("b"))[&SessionId::new("one")],
            PaneId::new("b")
        );
        assert_eq!(
            visible_owners(&panes, &PaneId::new("c"))[&SessionId::new("one")],
            PaneId::new("a")
        );
    }
    #[gpui::test]
    fn inactive_first_workbenches_and_duplicate_focus_transfer(cx: &mut TestAppContext) {
        let (runtime, tokio, tab) = fixture(true);
        let first = cx.add_window(|window, cx| {
            WorkspaceWorkbench::new(runtime.clone(), tokio.clone(), window, cx)
        });
        let second = cx.add_window(|window, cx| {
            WorkspaceWorkbench::new(runtime.clone(), tokio.clone(), window, cx)
        });
        for handle in [first, second] {
            handle
                .update(cx, |workbench, window, cx| {
                    workbench.set_tab(tab.clone(), viewport(), window, cx);
                    assert!(owners(workbench, cx).is_empty());
                })
                .unwrap();
        }
        first
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        first
            .update(cx, |workbench, window, cx| {
                workbench.assign_visible_owners(window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("a")]);
                workbench.pending_focus = Some(PaneId::new("b"));
                workbench.assign_visible_owners(window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("b")]);
                workbench.set_external_owner(Some(SessionId::new("preview-claude")));
                workbench.assign_visible_owners(window, cx);
                assert!(
                    owners(workbench, cx).is_empty(),
                    "focused inspector duplicate owns its geometry"
                );
                workbench.set_external_owner(None);
                workbench.assign_visible_owners(window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("b")]);
                let a = workbench.mounted[&PaneId::new("a")]
                    .terminal
                    .read(cx)
                    .resident_preview_buffers();
                let b = workbench.mounted[&PaneId::new("b")]
                    .terminal
                    .read(cx)
                    .resident_preview_buffers();
                assert!(Arc::ptr_eq(
                    &a[&SessionId::new("preview-claude")],
                    &b[&SessionId::new("preview-claude")]
                ));
            })
            .unwrap();
        second
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        first
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tab.clone(), viewport(), window, cx);
                assert!(owners(workbench, cx).is_empty());
            })
            .unwrap();
        second
            .update(cx, |workbench, window, cx| {
                workbench.assign_visible_owners(window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("a")]);
                workbench.deactivate(cx);
                assert!(workbench.focused_terminal().is_none());
                assert!(workbench.focused_session_id().is_none());
                assert!(owners(workbench, cx).is_empty());
                window.remove_window();
            })
            .unwrap();
        first
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
    #[gpui::test]
    fn distinct_sessions_own_geometry_and_external_zoom_releases_hidden_view(
        cx: &mut TestAppContext,
    ) {
        let (runtime, tokio, tab) = fixture(false);
        let handle =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime.clone(), tokio, window, cx));
        handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tab.clone(), viewport(), window, cx);
                window.activate_window();
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .update(cx, |workbench, window, cx| {
                workbench.assign_visible_owners(window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("a"), PaneId::new("b")]);
                let before = workbench.mounted[&PaneId::new("b")].terminal.entity_id();
                let mut external = tab.clone();
                external.focused_pane = PaneId::new("b");
                external.zoomed_pane = Some(PaneId::new("b"));
                seed(&runtime, vec![external.clone()], 2);
                workbench.set_tab(external, viewport(), window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("b")]);
                assert_eq!(
                    workbench.mounted[&PaneId::new("b")].terminal.entity_id(),
                    before
                );
                seed(&runtime, vec![tab.clone()], 3);
                workbench.set_tab(tab.clone(), viewport(), window, cx);
                assert_eq!(owners(workbench, cx), [PaneId::new("a"), PaneId::new("b")]);
                window.remove_window();
            })
            .unwrap();
        cx.run_until_parked();
    }
    #[gpui::test]
    fn divider_draft_is_local_cancels_and_external_revision_wins(cx: &mut TestAppContext) {
        let (runtime, tokio, tab) = fixture(false);
        let handle =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime.clone(), tokio, window, cx));
        handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tab.clone(), viewport(), window, cx);
                let divider = workbench.geometry().unwrap().dividers[0].clone();
                workbench.begin_resize(divider.clone());
                for x in 200..500 {
                    workbench.drag_resize(x as f32, 0.0, cx);
                }
                assert!(
                    runtime.store.read().unwrap().workspace_catalog().can_edit(),
                    "pointer motion never commits durable edits"
                );
                assert_ne!(workbench.tab.as_ref().unwrap().layout, tab.layout);
                workbench.cancel_resize();
                assert_eq!(workbench.tab.as_ref().unwrap().layout, tab.layout);
                workbench.begin_resize(divider);
                workbench.drag_resize(250.0, 0.0, cx);
                let mut external = tab.clone();
                set_fraction(&mut external.layout, &SplitId::new("divider"), 0.8);
                seed(&runtime, vec![external.clone()], 2);
                workbench.set_tab(external.clone(), viewport(), window, cx);
                assert!(workbench.resize.is_none());
                workbench.finish_resize(cx);
                assert_eq!(workbench.tab.as_ref().unwrap().layout, external.layout);
                assert!(runtime.store.read().unwrap().workspace_catalog().can_edit());
                window.remove_window();
            })
            .unwrap();
        cx.run_until_parked();
    }
    #[test]
    fn docking_edges_and_center_are_unambiguous() {
        assert_eq!(dock_edge(2.0, 100.0, 400.0, 300.0), Some(DockEdge::Left));
        assert_eq!(dock_edge(398.0, 100.0, 400.0, 300.0), Some(DockEdge::Right));
        assert_eq!(dock_edge(200.0, 2.0, 400.0, 300.0), Some(DockEdge::Top));
        assert_eq!(
            dock_edge(200.0, 298.0, 400.0, 300.0),
            Some(DockEdge::Bottom)
        );
        assert_eq!(dock_edge(200.0, 150.0, 400.0, 300.0), None);
    }
    #[gpui::test]
    fn tab_switch_preserves_recent_view_identity_and_bounds_retained_mounts(
        cx: &mut TestAppContext,
    ) {
        let (runtime, tokio, _) = fixture(false);
        let original =
            runtime.store.read().unwrap().sessions()[&SessionId::new("preview-claude")].clone();
        let mut tabs = Vec::new();
        for index in 0..20 {
            let mut session = (*original).clone();
            session.id = SessionId::new(format!("session-{index}"));
            runtime
                .store
                .write()
                .unwrap()
                .upsert_session(session.clone());
            let pane = PaneId::new(format!("pane-{index}"));
            tabs.push(WorkspaceTab {
                id: TabId::new(format!("tab-{index}")),
                title: None,
                focused_pane: pane.clone(),
                zoomed_pane: None,
                layout: LayoutNode::Pane {
                    id: pane,
                    session_id: session.id,
                },
            });
        }
        seed(&runtime, tabs.clone(), 1);
        let handle =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime.clone(), tokio, window, cx));
        handle.update(cx,|workbench,window,cx| {
            let start=std::time::Instant::now();
            for tab in &tabs {
                workbench.set_tab(tab.clone(),viewport(),window,cx);
                for grid in workbench.focused_terminal().unwrap().read(cx).resident_preview_buffers().values() {
                    *grid.write().unwrap() = diri_term::buffer::GridBuffer::new(120,40);
                }
            }
            let cold_us=start.elapsed().as_micros();
            assert_eq!(workbench.mounted.len(),16);
            let identities=workbench.mounted.iter().map(|(id,pane)|(id.clone(),pane.terminal.entity_id())).collect::<HashMap<_,_>>();
            let start=std::time::Instant::now();
            for _ in 0..50 { for tab in &tabs[18..] { workbench.set_tab(tab.clone(),viewport(),window,cx); } }
            let warm_us=start.elapsed().as_micros();
            for (id,entity) in identities { assert_eq!(workbench.mounted[&id].terminal.entity_id(),entity); }
            let mut cells=0;
            let mut unique=HashSet::new();
            for pane in workbench.mounted.values() { for (id,grid) in pane.terminal.read(cx).resident_preview_buffers() { if unique.insert(id) { cells+=grid.read().unwrap().cells.capacity()*std::mem::size_of::<diri_proto::grid::GridCell>(); } } }
            eprintln!("workspace mounts: cold20={cold_us}us, warm100={warm_us}us, retainedViews={}, uniqueGrids={}, retainedCellBytes={cells}",workbench.mounted.len(),unique.len());
            assert_eq!(unique.len(),16);
            assert_eq!(cells,16*120*40*std::mem::size_of::<diri_proto::grid::GridCell>());
            assert!(owners(workbench,cx).is_empty(),"background warm views remain passive");
            window.remove_window();
        }).unwrap();
        cx.run_until_parked();
    }
    #[gpui::test]
    fn divider_release_commits_once_and_holds_pose_until_ack(cx: &mut TestAppContext) {
        let (runtime, tokio, tab) = fixture(false);
        let handle =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime.clone(), tokio, window, cx));
        handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tab.clone(), viewport(), window, cx);
                workbench.begin_resize(workbench.geometry().unwrap().dividers[0].clone());
                workbench.drag_resize(300.0, 0.0, cx);
                let candidate = workbench.tab.as_ref().unwrap().clone();
                workbench.finish_resize(cx);
                assert!(!runtime.store.read().unwrap().workspace_catalog().can_edit());
                assert!(workbench.resize.as_ref().unwrap().submitted);
                workbench.finish_resize(cx);
                workbench.set_tab(tab.clone(), viewport(), window, cx);
                assert_eq!(workbench.tab.as_ref().unwrap().layout, candidate.layout);
                seed(&runtime, vec![candidate.clone()], 2);
                workbench.set_tab(candidate.clone(), viewport(), window, cx);
                assert!(workbench.resize.is_none());
                assert_eq!(workbench.tab.as_ref().unwrap().layout, candidate.layout);
                window.remove_window();
            })
            .unwrap();
        cx.run_until_parked();
    }
}
