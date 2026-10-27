//! Mounted layout views. Saved panes reference sessions; one explicit visible
//! view per session owns geometry in the active window.
mod commands;
pub(crate) use commands::PaneCommand;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::{
    haptics::{self, Haptic},
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
use crate::tooltip_warmth::WarmTooltip;
use gpui::{
    Context, CursorStyle, DragMoveEvent, Entity, EventEmitter, MouseButton, Render, Role,
    SharedString, StyleRefinement, Subscription, Window, div, prelude::*, px,
};

/// Compact panes meet at one hairline.
const COMPACT_SEAM: f32 = 1.0;
/// The hairline's grab area reaches this far into the panes on each side,
/// so it is as easy to catch as the old 5 pt strip.
const COMPACT_SEAM_REACH: f32 = 2.0;
/// Width of the line a hovered seam lights up.
const SEAM_HIGHLIGHT: f32 = 2.0;

#[derive(Clone)]
pub(crate) enum WorkspaceWorkbenchEvent {
    Terminal(TerminalPaneEvent),
    Notice(String),
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
    /// Whether the top-right pane carries the right sidebar toggle: true
    /// while the panel is closed, when no other title bar shows one.
    inspector_toggle: bool,
    mounted: HashMap<PaneId, MountedPane>,
    recent: VecDeque<PaneId>,
    /// Panes of the tab on screen. Warm panes of other tabs keep streaming,
    /// and only these may turn their output into a window repaint.
    visible: HashSet<PaneId>,
    catalog_revision: Option<u64>,
    viewport: TerminalViewport,
    pending_focus: Option<PaneId>,
    sent_focus: Option<PaneId>,
    /// Panes in the order they were focused, most recent last, per tab.
    /// When the focused pane goes away (⌘W, ⌘J, its shell exiting) focus
    /// returns to the one used before it, not the Engine's first pane.
    focus_history: HashMap<TabId, Vec<PaneId>>,
    /// Sessions this window just closed. Their panes stay in the layout
    /// until the Engine collapses the split; until then they paint blank,
    /// not as a saved pane whose session is unavailable.
    closing: HashSet<SessionId>,
    resize: Option<ResizeDraft>,
    /// What the drag in flight last snapped to: the pane a moved pane would
    /// land on, or the end of a divider's travel.
    drag_haptic: haptics::Crossing,
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
            inspector_toggle: false,
            mounted: HashMap::new(),
            recent: VecDeque::new(),
            visible: HashSet::new(),
            catalog_revision: None,
            viewport: TerminalViewport::default(),
            pending_focus: None,
            sent_focus: None,
            focus_history: HashMap::new(),
            closing: HashSet::new(),
            resize: None,
            drag_haptic: haptics::Crossing::default(),
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
        // Read before `reconcile` drops a pane: a dropped terminal's handle
        // can still be the window's focus, but it no longer reaches a key
        // binding, so typing and ⌘T would wait for a click.
        let held_focus = self.placeholder_focus.contains_focused(window, cx);
        self.enabled = true;
        let focus_changed = self.pending_focus.is_none()
            && self
                .tab
                .as_ref()
                .is_some_and(|current| current.focused_pane != tab.focused_pane);
        let switched = self.tab.as_ref().is_none_or(|current| current.id != tab.id);
        let previous_tab = self.tab.clone();
        let previous_focus = (!switched).then(|| self.focused_id().cloned()).flatten();
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
        let mut focus_changed = focus_changed;
        if let Some(gone) = previous_focus
            && let Some(tab) = &self.tab
            && !commands::contains_pane(&tab.layout, &gone)
            && let Some(recent) = self.focus_history.get(&tab.id).and_then(|history| {
                history
                    .iter()
                    .rev()
                    .find(|pane| **pane != gone && commands::contains_pane(&tab.layout, pane))
                    .cloned()
            })
            && recent != tab.focused_pane
        {
            self.pending_focus = Some(recent);
            focus_changed = true;
        }
        self.remember_focus();
        if let Some(tab) = &self.tab {
            self.closing
                .retain(|session| commands::pane_for_session(&tab.layout, session).is_some());
        }
        let changed = previous_tab != self.tab || viewport_changed;
        self.reconcile(window, cx);
        self.flush_focus();
        self.assign_visible_owners(window, cx);
        if switched || focus_changed || (held_focus && self.keyboard_stranded(window, cx)) {
            self.focus(window, cx);
        }
        if changed {
            cx.notify();
        }
    }

    /// Keyboard focus was inside this workbench but no mounted pane holds it:
    /// its pane was dropped with its session, or it waited on the placeholder
    /// for the focused pane's session, which has now arrived.
    fn keyboard_stranded(&self, window: &Window, cx: &gpui::App) -> bool {
        !self
            .mounted
            .values()
            .any(|pane| pane.terminal.read(cx).is_focused(window))
            && (self.focused_terminal().is_some() || !self.placeholder_focus.is_focused(window))
    }

    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = &self.tab else {
            return;
        };
        let mut required = Vec::new();
        leaves(&tab.layout, &mut required);
        // A hidden pane stays warm, but its Links popover is a window of its
        // own and would stay on screen over whatever replaced the pane.
        for (id, pane) in &self.mounted {
            if !required.iter().any(|required| required.pane == *id) {
                pane.terminal
                    .update(cx, |terminal, cx| terminal.dismiss_session_links(cx));
            }
        }
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
        self.visible.clone_from(&required_ids);
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
                    this.remember_focus();
                    this.flush_focus();
                    this.assign_visible_owners(window, cx);
                    cx.notify();
                }
            });
            let events = cx.subscribe(&terminal, |_, _, event: &TerminalPaneEvent, cx| {
                cx.emit(WorkspaceWorkbenchEvent::Terminal(event.clone()));
            });
            let output_pane = identity.pane.clone();
            let output = cx.observe(&terminal, move |this, _, cx| {
                // A hidden pane's own notify invalidates nothing, but this
                // one repaints the whole window: up to eight warm panes of
                // another tab would each buy a frame nobody can see.
                if this.visible.contains(&output_pane) {
                    cx.notify();
                }
            });
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
    pub(crate) fn seed_pane_grids_for_test(
        &self,
        grid: &diri_term::buffer::GridBuffer,
        cx: &mut Context<Self>,
    ) {
        for pane in self.mounted.values() {
            pane.terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(grid.clone(), cx)
            });
        }
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

    /// Seeds each mounted pane with its own coloured page (a git UI, a
    /// lockfile, a shell prompt, a test run) so style screenshots show how a
    /// theme's ANSI colours sit on every surface.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn seed_styled_panes_for_test(&self, cx: &mut Context<Self>) {
        use diri_proto::grid::TermColor;
        type Span = (&'static str, Option<u8>);
        const PAGES: [&[&[Span]]; 4] = [
            &[
                &[(
                    "─[1]─Status─────────┐┌─[0]─Unstaged changes───────",
                    Some(2),
                )],
                &[
                    ("─[2]─", None),
                    ("Files", Some(2)),
                    (" - Worktrees ││ ", None),
                    ("diff --git", Some(15)),
                ],
                &[
                    (" ▼ crates/diri-app/  ││ ", None),
                    ("a/crates/diri-app/src/root.rs", Some(15)),
                ],
                &[
                    ("   ", None),
                    ("M", Some(1)),
                    (" root.rs          ││ ", None),
                    ("index 5c6b31b..aeabfb7 100644", None),
                ],
                &[
                    ("                     ││ ", None),
                    ("@@ -4085,6 +4085,10 @@", Some(6)),
                ],
                &[
                    ("                     ││ ", None),
                    ("-        let mut card = div()", Some(1)),
                ],
                &[
                    ("                     ││ ", None),
                    ("+        let card_island = islands;", Some(2)),
                ],
                &[
                    ("                     ││ ", None),
                    ("+        let mut card = div()", Some(2)),
                ],
                &[(
                    "───────────1 of 2────┘└────────────────────────────",
                    Some(2),
                )],
                &[
                    ("─[3]─", None),
                    ("Local", Some(2)),
                    (" b1 of 24──┐┌─Command log───────────", None),
                ],
                &[
                    ("─[4]─", None),
                    ("Commit", Some(5)),
                    ("1 of 300──┤│ ", None),
                    ("on that commit", Some(2)),
                ],
                &[
                    ("Switch view: <tab> | …   ", Some(4)),
                    ("Donate", Some(5)),
                    (" ", None),
                    ("Ask Question", Some(3)),
                ],
            ],
            &[
                &[
                    (" 8 ", Some(8)),
                    ("        \"type\": ", Some(4)),
                    ("\"tarball\",", Some(3)),
                ],
                &[
                    (" 9 ", Some(8)),
                    ("        \"url\": ", Some(4)),
                    ("\"https://releases.nixos.org/\"", Some(3)),
                ],
                &[("10 ", Some(8)), ("      },", None)],
                &[("11 ", Some(8)), ("      \"original\": {", Some(4))],
                &[
                    ("12 ", Some(8)),
                    ("        \"type\": ", Some(4)),
                    ("\"tarball\",", Some(3)),
                ],
                &[
                    ("13 ", Some(8)),
                    ("        \"url\": ", Some(4)),
                    ("\"https://channels.nixos.org\"", Some(3)),
                ],
                &[("14 ", Some(8)), ("      }", None)],
                &[("15 ", Some(8)), ("    },", None)],
                &[("16 ", Some(8)), ("    \"root\": {", Some(4))],
                &[("17 ", Some(8)), ("      \"inputs\": {", Some(4))],
                &[
                    ("18 ", Some(8)),
                    ("        \"nixpkgs\": ", Some(4)),
                    ("\"nixpkgs\"", Some(3)),
                ],
                &[("19 ", Some(8)), ("      }", None)],
                &[(":", None)],
            ],
            &[
                &[
                    ("~/fun/diri", Some(4)),
                    (" main", Some(8)),
                    ("* ", Some(3)),
                    ("≡", Some(6)),
                ],
                &[("❯ ", Some(5)), ("lg", None)],
            ],
            &[
                &[("direnv: loading ~/fun/diri/.envrc", None)],
                &[
                    ("warning", Some(5)),
                    (": Git tree has uncommitted changes", None),
                ],
                &[
                    ("~/fun/diri", Some(4)),
                    (" main", Some(8)),
                    ("* ", Some(3)),
                    ("≡", Some(6)),
                ],
                &[("❯ ", Some(5)), ("cargo test -p diri-ui", None)],
                &[("   Compiling", Some(2)), (" diri-ui v0.9.6", None)],
                &[("running 4 tests", None)],
                &[("test glass_alpha_clears ... ", None), ("ok", Some(2))],
                &[("test neutral_palette ... ", None), ("ok", Some(2))],
                &[("test islands_inset ... ", None), ("ok", Some(2))],
                &[
                    ("test result: ", None),
                    ("ok", Some(2)),
                    (". 4 passed", None),
                ],
            ],
        ];
        let mut seeded = HashSet::new();
        let mut page = 0;
        let mut panes: Vec<_> = self.mounted.iter().collect();
        panes.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        for (_, pane) in panes {
            if !seeded.insert(pane.session.clone()) {
                continue;
            }
            // As tall as a quarter-window pane: a taller grid would show
            // its blank bottom rows, the way a terminal follows its end.
            let mut grid = diri_term::buffer::GridBuffer::new(100, 14);
            for (y, line) in PAGES[page % PAGES.len()].iter().enumerate() {
                let mut x = 0;
                for (text, color) in line.iter() {
                    for ch in text.chars() {
                        let cell = &mut grid.cells[y * 100 + x];
                        cell.scalar = ch as u32;
                        if let Some(color) = color {
                            cell.fg = TermColor::Ansi(*color);
                        }
                        x += 1;
                    }
                }
            }
            page += 1;
            pane.terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(grid, cx);
                cx.notify();
            });
        }
    }

    /// Seeds `session`'s pane with `grid`, for clips that script each pane.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn seed_session_grid_for_test(
        &self,
        session: &str,
        grid: &diri_term::buffer::GridBuffer,
        cx: &mut Context<Self>,
    ) {
        for pane in self.mounted.values().filter(|pane| pane.session.0 == session) {
            pane.terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(grid.clone(), cx);
                cx.notify();
            });
        }
    }

    /// Seeds each listed session's pane with its own lines of text.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn seed_pane_text_for_test(&self, texts: &[(&str, &str)], cx: &mut Context<Self>) {
        for pane in self.mounted.values() {
            let Some((_, text)) = texts.iter().find(|(id, _)| *id == pane.session.0) else {
                continue;
            };
            let lines: Vec<_> = text.lines().collect();
            // As tall as the text: a short pane shows a grid's bottom rows.
            let mut grid = diri_term::buffer::GridBuffer::new(100, lines.len() as u16 + 1);
            grid.cursor = diri_term::buffer::CursorState {
                col: lines.last().map_or(0, |line| line.chars().count()) as u16,
                row: lines.len().saturating_sub(1) as u16,
                visible: true,
            };
            for (y, line) in lines.iter().enumerate() {
                for (x, ch) in line.chars().take(100).enumerate() {
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
            pane.terminal.update(cx, |terminal, cx| {
                terminal.release_layout_control();
                terminal.dismiss_session_links(cx);
            });
        }
    }

    /// Moves the focused pane to the end of its tab's history, dropping
    /// panes the layout no longer has.
    fn remember_focus(&mut self) {
        let Some(tab) = &self.tab else {
            return;
        };
        let Some(focused) = self.focused_id().cloned() else {
            return;
        };
        let history = self.focus_history.entry(tab.id.clone()).or_default();
        history.retain(|pane| *pane != focused && commands::contains_pane(&tab.layout, pane));
        history.push(focused);
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
    pub(crate) fn set_inspector_toggle(&mut self, show: bool) {
        self.inspector_toggle = show;
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
    /// Every mounted pane's terminal, for multi-pane fixtures.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn terminals_for_test(&self) -> Vec<Entity<TerminalPane>> {
        let mut panes: Vec<_> = self.mounted.iter().collect();
        panes.sort_by(|left, right| left.0.0.cmp(&right.0.0));
        panes
            .into_iter()
            .map(|(_, pane)| pane.terminal.clone())
            .collect()
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
        let requested = position / available;
        // The ratio stops where the panes stop: at the geometry's minimum
        // pane size, so the saved ratio is the one on screen.
        let minimum = if resize.axis == LayoutAxis::Horizontal {
            crate::workspace_geometry::MIN_PANE_WIDTH
        } else {
            crate::workspace_geometry::MIN_PANE_HEIGHT
        };
        let floor = (minimum / available).clamp(0.1, 0.5);
        resize.fraction = requested.clamp(floor, 1.0 - floor);
        // A divider has no snap points; the ends of its travel are the only
        // thresholds a resize crosses.
        let limit = (requested != resize.fraction)
            .then(|| haptics::key("workspace-divider", (&resize.split, requested > 0.5)));
        if let Some(target) = self.drag_haptic.moved_to(limit, gpui::point(px(x), px(y))) {
            haptics::perform(Haptic::Limit, target);
        }
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
                crate::i18n::t("toast.workspace_layout_changed_resize").into(),
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
        let density = self
            .runtime
            .store
            .read()
            .expect("store")
            .preferences()
            .pane_density;
        WorkspaceGeometry::settled_with_seam(
            &tab,
            Rect {
                width: self.viewport.width,
                height: self.viewport.height,
                ..Rect::default()
            },
            if density.islands() {
                density.island_gap()
            } else {
                COMPACT_SEAM
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
        crate::perf_overlay::rendered("workbench");
        let (colors, islands) = {
            let store = self.runtime.store.read().expect("store");
            (
                crate::app_theme::colors_in(&store),
                store.preferences().pane_density.islands(),
            )
        };
        if !cx.has_active_drag() {
            self.drag_haptic.reset();
        }
        crate::terminal_pane::SeamDrag::publish(
            window.window_handle().window_id(),
            "workspace",
            self.resize.as_ref().is_some_and(|resize| !resize.submitted),
            cx,
        );
        let mut root = div()
            .id("workspace-workbench")
            .debug_selector(|| "workspace-workbench".into())
            .track_focus(&self.placeholder_focus)
            .relative()
            .size_full()
            .overflow_hidden()
            // Between islands the window chrome behind the card shows (the
            // card paints it once; a second translucent tint here would
            // compound under glass). Compact panes meet over the work surface.
            .when(!islands, |root| root.bg(colors.work_surface_nested()))
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
        // Painted after the panes so a compact seam's grab area, which
        // overlaps the panes either side, wins hit tests over them.
        let mut seams = Vec::with_capacity(geometry.dividers.len());
        for divider in &geometry.dividers {
            let divider = divider.clone();
            let horizontal = divider.axis == LayoutAxis::Horizontal;
            // Compact panes meet at a 1 pt seam; reach past it on both sides
            // so the grab area stays as wide as the old 5 pt divider.
            let reach = if islands { 0.0 } else { COMPACT_SEAM_REACH };
            let mut bounds = divider.bounds;
            if horizontal {
                bounds.x -= reach;
                bounds.width += 2.0 * reach;
            } else {
                bounds.y -= reach;
                bounds.height += 2.0 * reach;
            }
            let drag = divider.clone();
            let group = SharedString::from(format!("workspace-seam-{}", divider.id.0));
            seams.push(
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
                    // Compact draws the seam as one hairline; between islands
                    // the chrome itself is the divider.
                    .when(!islands, |seam| {
                        seam.child(
                            div()
                                .absolute()
                                .when(horizontal, |line| {
                                    line.top_0().bottom_0().left(px(reach)).w(px(1.0))
                                })
                                .when(!horizontal, |line| {
                                    line.left_0().right_0().top(px(reach)).h(px(1.0))
                                })
                                .bg(colors.primary.alpha(0.10)),
                        )
                    })
                    // Hover lights a 2 pt line down the middle, not the whole
                    // grab area: the gap between islands is 6 pt wide.
                    .group(group.clone())
                    .child({
                        let (along, across) = if horizontal {
                            (bounds.height, bounds.width)
                        } else {
                            (bounds.width, bounds.height)
                        };
                        let offset = (across - SEAM_HIGHLIGHT) / 2.0;
                        div()
                            .absolute()
                            .when(horizontal, |line| {
                                line.top_0()
                                    .left(px(offset))
                                    .w(px(SEAM_HIGHLIGHT))
                                    .h(px(along))
                            })
                            .when(!horizontal, |line| {
                                line.left_0()
                                    .top(px(offset))
                                    .h(px(SEAM_HIGHLIGHT))
                                    .w(px(along))
                            })
                            .rounded_full()
                            .group_hover(group, |line| line.bg(gpui::rgba(0x4f83f1ff).alpha(0.8)))
                    })
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
        let multiple_panes = self
            .tab
            .as_ref()
            .is_some_and(|tab| matches!(tab.layout, LayoutNode::Split { .. }));
        // A pane's only control is the ✕ of a terminal pane: it ends that
        // shell (⌘W). The agent's pane is the tab; it closes from the sidebar.
        let shells: HashSet<SessionId> = {
            let store = self.runtime.store.read().expect("store");
            geometry
                .panes
                .iter()
                .map(|pane| &pane.identity.session)
                .filter(|session| store.pane_owner(session) != **session)
                .cloned()
                .collect()
        };
        for pane in &geometry.panes {
            let bounds = pane.bounds;
            // The pane whose title bar ends at the window's trailing edge.
            let inspector_toggle = self.inspector_toggle
                && bounds.y <= 0.5
                && bounds.x + bounds.width >= self.viewport.width - 0.5;
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
                .when(islands, |surface| surface.rounded(px(Radius::CARD)))
                .overflow_hidden();
            let close_shell = multiple_panes && shells.contains(&pane.identity.session);
            if let Some(mounted) = self.mounted.get(&pane.identity.pane) {
                mounted.terminal.update(cx, |terminal, cx| {
                    // The ✕ and the right-panel toggle sit in the title
                    // bar's own trailing padding: each slot is a control and
                    // its gap, less that padding once, so every gap matches.
                    let count = u8::from(close_shell) + u8::from(inspector_toggle);
                    let inset = if count == 0 {
                        0.0
                    } else {
                        f32::from(count) * (Metrics::TOOLBAR_CONTROL_SIZE + 4.0) - 11.0
                    };
                    terminal.set_header_trailing_inset(inset, cx);
                    terminal.set_island_corners(islands, cx);
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
                // A sidebar tick or another pane's output must not repaint
                // this terminal. Its own notifications and bounds changes
                // invalidate the cached render, including input and resize.
                surface = surface.child(
                    mounted
                        .terminal
                        .clone()
                        .cached(StyleRefinement::default().size_full()),
                );
            } else if self.closing.contains(&pane.identity.session) {
                surface = surface.bg(colors.terminal_surface());
            } else {
                surface = surface.child(
                    div()
                        .debug_selector(|| "workspace-pane-unavailable".into())
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
            let pane_id = pane.identity.pane.clone();
            // The pane's own title bar makes the same room (`TerminalPane`).
            let caption_inset = crate::window_chrome::caption_inset(
                self.viewport.y + bounds.y,
                f32::from(window.viewport_size().width)
                    - (self.viewport.x + bounds.x + bounds.width),
            );
            let controls = div()
                .absolute()
                .top(px((if islands {
                    crate::terminal_pane::island_title_bar()
                } else {
                    Metrics::TITLE_BAR
                } - Metrics::TOOLBAR_CONTROL_SIZE)
                    / 2.0))
                .right(px(Metrics::TOOLBAR_EDGE_INSET + caption_inset))
                .opacity(crate::window_chrome::title_row_opacity(window))
                .flex()
                .gap(px(4.0))
                .when(close_shell, |controls| {
                    let session = pane.identity.session.clone();
                    let pane_focused = geometry.focused.pane == pane.identity.pane;
                    controls.child(
                        div()
                            .id(SharedString::from(format!("close-pane-{}", pane_id.0)))
                            .debug_selector({
                                let pane = pane_id.clone();
                                move || format!("close-pane-{}", pane.0)
                            })
                            .role(Role::Button)
                            .aria_label("Close pane")
                            .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                            .rounded(px(Radius::BADGE))
                            .hover(move |button| button.bg(Fill::subtle(colors)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .child(sf_symbol("xmark", 12.0, colors.secondary))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            // ⌘W closes the focused shell pane, so only its ✕ says so.
                            .shortcut_tooltip(
                                "Close pane",
                                pane_focused.then_some(crate::commands::CommandId::CloseSession),
                                colors,
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focus_before_closing(&session, window, cx);
                                // The Engine collapses the split once the
                                // session is gone.
                                this.runtime
                                    .store
                                    .write()
                                    .expect("store")
                                    .remove_sessions(vec![session.clone()]);
                                cx.notify();
                            })),
                    )
                })
                .when(inspector_toggle, |controls| {
                    controls.child(crate::right_panel::dispatching_toggle(
                        "workspace-toggle-inspector",
                        false,
                        colors,
                        0.0,
                    ))
                });
            let focused = geometry.panes.len() > 1 && geometry.focused.pane == pane.identity.pane;
            // Unfocused panes recede a little, the way a split terminal
            // marks where typing goes without a heavy frame; in a window
            // that is not key every pane recedes a step further.
            surface = crate::pane_fade::faded(
                surface,
                colors,
                window.is_window_active(),
                geometry.panes.len() == 1 || focused,
                islands.then_some(Radius::CARD),
            );
            if focused || islands {
                surface = surface.child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .left(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .when(islands, |edge| edge.rounded(px(Radius::CARD)))
                        .border_1()
                        .border_color(colors.primary.alpha(if focused { 0.16 } else { 0.10 })),
                );
            }
            surface = surface.child(controls);
            root = root.child(surface);
        }
        root.children(seams)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::workspace::{TabId, WorkspaceId, WorkspaceRecord, WorkspaceSnapshot};
    use gpui::TestAppContext;
    use std::cell::Cell;
    use std::rc::Rc;

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
    fn output_in_one_pane_does_not_render_unchanged_sibling(cx: &mut TestAppContext) {
        let (runtime, tokio, tab) = fixture(false);
        let (workbench, cx) = cx.add_window_view(|window, cx| {
            let mut workbench = WorkspaceWorkbench::new(runtime, tokio, window, cx);
            workbench.set_tab(tab, viewport(), window, cx);
            workbench
        });
        cx.run_until_parked();
        let (active, quiet) = workbench.read_with(cx, |workbench, _| {
            (
                workbench.mounted[&PaneId::new("a")].terminal.clone(),
                workbench.mounted[&PaneId::new("b")].terminal.clone(),
            )
        });
        let before_active = active.read_with(cx, |terminal, _| terminal.render_count);
        let before_quiet = quiet.read_with(cx, |terminal, _| terminal.render_count);
        assert!(before_active > 0 && before_quiet > 0);
        for _ in 0..8 {
            active.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        assert!(active.read_with(cx, |terminal, _| terminal.render_count) > before_active);
        assert_eq!(
            quiet.read_with(cx, |terminal, _| terminal.render_count),
            before_quiet,
            "an output notification must not redraw an unchanged split pane"
        );
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
    /// ⌘W on the focused pane: the Engine refocuses its first pane, but
    /// focus goes back to the pane used before the closed one.
    #[gpui::test]
    fn closing_the_focused_pane_returns_focus_to_the_last_used_one(cx: &mut TestAppContext) {
        let (runtime, tokio, two) = fixture(false);
        let pane = |id: &str, session: &str| LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(session),
        };
        let mut three = two.clone();
        three.layout = LayoutNode::Split {
            id: SplitId::new("outer"),
            axis: LayoutAxis::Horizontal,
            fraction: 0.5,
            first: Box::new(two.layout.clone()),
            second: Box::new(pane("c", "preview-cursor")),
        };
        let handle =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime.clone(), tokio, window, cx));
        handle
            .update(cx, |workbench, window, cx| {
                for (revision, focused) in [(2, "a"), (3, "b"), (4, "c")] {
                    let mut tab = three.clone();
                    tab.focused_pane = PaneId::new(focused);
                    seed(&runtime, vec![tab.clone()], revision);
                    workbench.set_tab(tab, viewport(), window, cx);
                }
                // "c" closes; the Engine's repair focuses the first pane.
                let mut closed = two.clone();
                closed.focused_pane = PaneId::new("a");
                seed(&runtime, vec![closed.clone()], 5);
                workbench.set_tab(closed, viewport(), window, cx);
                assert_eq!(workbench.focused_id(), Some(&PaneId::new("b")));
                window.remove_window();
            })
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
    #[gpui::test]
    fn a_divider_ticks_once_at_each_end_of_its_travel(cx: &mut TestAppContext) {
        let (runtime, tokio, tab) = fixture(false);
        let handle =
            cx.add_window(|window, cx| WorkspaceWorkbench::new(runtime, tokio, window, cx));
        handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tab, viewport(), window, cx);
                let divider = workbench.geometry().unwrap().dividers[0].clone();
                workbench.begin_resize(divider);
                let _ = haptics::testing::take();

                // Free travel is silent; the near end ticks once, however
                // far past it the pointer goes and however long it stays.
                for x in (-40..500).rev() {
                    workbench.drag_resize(x as f32, 0.0, cx);
                }
                workbench.drag_resize(-40.0, 0.0, cx);
                let near = haptics::testing::take();
                assert_eq!(near.len(), 1);
                assert_eq!(near[0].0, Haptic::Limit);

                for x in -40..1000 {
                    workbench.drag_resize(x as f32, 0.0, cx);
                }
                let far = haptics::testing::take();
                assert_eq!(far.len(), 1, "the far end is its own limit");
                assert_ne!(far[0].1, near[0].1);
                window.remove_window();
            })
            .unwrap();
        cx.run_until_parked();
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
    fn output_in_a_warm_hidden_pane_does_not_repaint_the_window(cx: &mut TestAppContext) {
        let (runtime, tokio, _) = fixture(false);
        let original =
            runtime.store.read().unwrap().sessions()[&SessionId::new("preview-claude")].clone();
        let mut tabs = Vec::new();
        for index in 0..2 {
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
        let repaints = Rc::new(Cell::new(0usize));
        let (hidden, shown, _observer) = handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tabs[0].clone(), viewport(), window, cx);
                workbench.set_tab(tabs[1].clone(), viewport(), window, cx);
                assert_eq!(workbench.mounted.len(), 2, "the first tab stays warm");
                let hidden = workbench.mounted[&PaneId::new("pane-0")].terminal.clone();
                let shown = workbench.mounted[&PaneId::new("pane-1")].terminal.clone();
                let count = repaints.clone();
                let observer = cx.observe(&cx.entity(), move |_, _, _| {
                    count.set(count.get() + 1);
                });
                (hidden, shown, observer)
            })
            .unwrap();
        cx.run_until_parked();
        repaints.set(0);

        hidden.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(repaints.get(), 0, "a hidden pane's output buys no frame");

        shown.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(repaints.get(), 1, "the pane on screen still repaints");

        handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tabs[0].clone(), viewport(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        repaints.set(0);
        hidden.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(repaints.get(), 1, "switching back makes it live again");

        handle
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
    #[gpui::test]
    fn a_pane_leaving_the_screen_closes_its_links_popover(cx: &mut TestAppContext) {
        let (runtime, tokio, _) = fixture(false);
        let original =
            runtime.store.read().unwrap().sessions()[&SessionId::new("preview-claude")].clone();
        let mut tabs = Vec::new();
        for index in 0..2 {
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
        let first = handle
            .update(cx, |workbench, window, cx| {
                workbench.set_tab(tabs[0].clone(), viewport(), window, cx);
                let first = workbench.mounted[&PaneId::new("pane-0")].terminal.clone();
                first.update(cx, |terminal, _| terminal.open_session_links_for_test());
                workbench.set_tab(tabs[1].clone(), viewport(), window, cx);
                first
            })
            .unwrap();
        assert!(
            !first.read_with(cx, |terminal, _| terminal.session_links_open_for_test()),
            "a warm hidden pane's popover would float over the new session"
        );
        handle
            .update(cx, |workbench, _, cx| {
                let second = workbench.mounted[&PaneId::new("pane-1")].terminal.clone();
                second.update(cx, |terminal, _| terminal.open_session_links_for_test());
                workbench.deactivate(cx);
                assert!(!second.read(cx).session_links_open_for_test());
            })
            .unwrap();
        handle
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
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
