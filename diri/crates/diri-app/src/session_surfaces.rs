//! GPUI rendering and event routing for T13 navigation surfaces.
//!
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::store::StoreRuntime;
use crate::switcher::{
    OverviewArrow, OverviewFilter, OverviewLane, OverviewMode, SwitcherKey, display_title,
};
use diri_proto::{AgentKind as ProtoAgentKind, AttentionLevel, RiskHint, SessionId, SessionRecord};
use diri_term::element::{SharedGridBuffer, TerminalElement};
use diri_ui::{
    AgentKind, AgentLogo, HairlineDivider, Ink, Palette, Radius, SemanticColors, StatusGlyph,
    StatusState,
};
use gpui::{
    AnyElement, BoxShadow, ClickEvent, Context, Entity, FocusHandle, FontWeight, KeyDownEvent,
    KeyUpEvent, ModifiersChangedEvent, MouseButton, Render, ScrollHandle, SharedString, Task,
    Window, div, point, prelude::*, px, rgba,
};

#[path = "overview_calm.rs"]
mod overview_calm;
#[path = "overview_zoom_surface.rs"]
mod overview_zoom_surface;
#[path = "tab_peek_surface.rs"]
mod tab_peek_surface;
#[path = "workspace_peek_surface.rs"]
mod workspace_peek_surface;
pub(crate) use overview_calm::OverviewVariant;

#[derive(Clone, Debug, PartialEq, Eq)]
enum PeekItem {
    Session(SessionId),
    Tab(diri_proto::workspace::TabId),
}
impl PeekItem {
    fn session(&self) -> Option<&SessionId> {
        match self {
            Self::Session(id) => Some(id),
            Self::Tab(_) => None,
        }
    }
}

pub(crate) struct TabPeekActivated;
impl gpui::EventEmitter<TabPeekActivated> for SessionSurfaces {}

pub struct SessionSurfaces {
    peek: crate::tab_peek::TabPeek<PeekItem>,
    peek_workspace: Option<diri_proto::workspace::WorkspaceId>,
    peek_settled_bounds: crate::workspace_geometry::Rect,
    workspace_previews: crate::workspace_preview_source::WorkspacePreviews,
    workspace_preview_views: HashMap<diri_proto::workspace::PaneId, TerminalElement>,
    peek_left: f32,
    peek_top: f32,
    peek_width: f32,
    peek_scroll: ScrollHandle,
    peek_scroll_anchor: f32,
    peek_previous_focus: Option<FocusHandle>,
    peek_frame_pending: bool,
    closing_previews: HashMap<SessionId, TerminalElement>,
    live_previews: crate::tab_preview::PreviewSet<crate::tab_preview::LivePreview>,
    store: crate::store::WindowStore,
    focus_handle: FocusHandle,
    resident_previews: HashMap<SessionId, TerminalElement>,
    status_glyphs: HashMap<(SessionId, u16, diri_ui::AgentKind), Entity<StatusGlyph>>,
    overview_grid_scroll: ScrollHandle,
    client: Arc<diri_client::DaemonClient>,
    tokio: Option<tokio::runtime::Handle>,
    screens: HashMap<SessionId, ScreenPreview>,
    /// Colored screens of sessions that are not mounted, read once per open
    /// so every calm-overview card is a real miniature, not a blank box.
    screen_grids: HashMap<SessionId, TerminalElement>,
    screen_requests: HashMap<SessionId, ScreenRequest>,
    overview_was_visible: bool,
    overview_list_scroll: ScrollHandle,
    /// Safari-style zoom between the page and its overview card.
    zoom: overview_zoom_surface::ZoomPresentation,
    /// Phase-1 design exploration: which overview layout renders.
    pub(crate) overview_variant: OverviewVariant,
    /// This view is `.cached()` in RootView, so ambient window redraws no
    /// longer reach it: store changes must notify it directly.
    _store_changes: Task<()>,
}

enum ScreenPreview {
    Ready(Vec<String>),
    Empty,
    Unavailable,
}

struct ScreenRequest {
    _task: Task<()>,
    abort: tokio::task::AbortHandle,
}

impl Drop for ScreenRequest {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

// Keep the latest useful part of the screen readable, including its prompt.
// This is plain text from the Engine's parser, never a second ANSI parser.
fn screen_excerpt(text: &str) -> Vec<String> {
    let lines: Vec<_> = text.lines().collect();
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);
    lines[end.saturating_sub(12)..end]
        .iter()
        .map(|line| line.chars().take(160).collect())
        .collect()
}

fn overview_columns(width: f32) -> usize {
    ((width - 48.0 + 16.0) / 336.0).floor().clamp(1.0, 5.0) as usize
}

impl SessionSurfaces {
    pub(crate) fn set_window_store(&mut self, store: crate::store::WindowStore) {
        self.store = store;
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn configure_preview_fixture(
        &mut self,
        source: &crate::tab_preview::screenshot_fixture::Source,
    ) {
        self.client = Arc::new(diri_client::DaemonClient::with_socket_path(&source.socket));
        self.tokio = Some(source.runtime.handle().clone());
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn preview_fixture_states(
        &self,
    ) -> Vec<tokio::sync::watch::Receiver<crate::tab_preview::PreviewState>> {
        self.peek
            .sessions
            .iter()
            .filter_map(|id| {
                self.live_previews
                    .get(id.session()?)
                    .map(|preview| preview.state.clone())
            })
            .collect()
    }

    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio: Option<tokio::runtime::Handle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut changes = runtime.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Self {
            peek: Default::default(),
            peek_left: 0.0,
            peek_top: 0.0,
            peek_width: 0.0,
            peek_scroll: ScrollHandle::new(),
            peek_scroll_anchor: 0.0,
            peek_previous_focus: None,
            peek_frame_pending: false,
            closing_previews: HashMap::new(),
            peek_workspace: None,
            peek_settled_bounds: Default::default(),
            workspace_previews: Default::default(),
            workspace_preview_views: HashMap::new(),
            live_previews: Default::default(),
            store: crate::store::WindowStore::from_canonical(Arc::clone(&runtime.store)),
            focus_handle: cx.focus_handle(),
            resident_previews: HashMap::new(),
            status_glyphs: HashMap::new(),
            overview_grid_scroll: ScrollHandle::new(),
            client: Arc::clone(runtime.client()),
            tokio,
            screens: HashMap::new(),
            screen_grids: HashMap::new(),
            screen_requests: HashMap::new(),
            overview_was_visible: false,
            overview_list_scroll: ScrollHandle::new(),
            zoom: Default::default(),
            // Prototype switch while the design is chosen: DIRI_OVERVIEW_VARIANT
            // = a (Safari) | b (gallery) | c (mini windows, default) | current.
            overview_variant: OverviewVariant::from_env(
                &std::env::var("DIRI_OVERVIEW_VARIANT").unwrap_or_else(|_| "c".into()),
            ),
            _store_changes: store_changes,
        }
    }

    fn colors(&self) -> SemanticColors {
        let store = self.store.read().expect("session store lock poisoned");
        crate::app_theme::colors_in(&store)
    }

    /// T11 supplies the same resident buffer used by the mounted terminal. A
    /// separate painter/cache renders it into switcher and overview thumbnails
    /// without reading back the onscreen Metal layer.
    pub(crate) fn set_resident_buffer(&mut self, id: SessionId, buffer: SharedGridBuffer) {
        self.resident_previews.insert(
            id,
            TerminalElement::new(buffer).focused(false).without_cursor(),
        );
    }

    pub(crate) fn remove_resident_buffer(&mut self, id: &SessionId) {
        self.resident_previews.remove(id);
    }

    pub(crate) fn sync_resident_buffers(&mut self, buffers: HashMap<SessionId, SharedGridBuffer>) {
        let stale: Vec<_> = self
            .resident_previews
            .keys()
            .filter(|id| !buffers.contains_key(*id))
            .cloned()
            .collect();
        for id in stale {
            self.remove_resident_buffer(&id);
        }
        for (id, buffer) in buffers {
            // Only rebuild when the underlying buffer actually changed: every
            // TerminalElement carries a fresh global element id, and GPUI
            // retains per-id render state, so unconditionally recreating
            // previews on each store event leaks textures without bound.
            let unchanged = self
                .resident_previews
                .get(&id)
                .is_some_and(|element| Arc::ptr_eq(&element.buffer(), &buffer));
            if !unchanged {
                self.set_resident_buffer(id, buffer);
            }
        }
    }

    pub(crate) fn toggle_overview(&mut self, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        store.toggle_overview();
        cx.notify();
    }

    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.cancel_tab_peek_immediately(cx);
        let mut store = self.store.write().expect("session store lock poisoned");
        store.cancel_switcher();
        store.dismiss_overview();
        drop(store);
        self.reset_overview_zoom(cx);
        self.overview_was_visible = false;
        cx.notify();
    }
}

impl Render for SessionSurfaces {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_tab_peek_focus(window, cx);
        if !self.peek.paint_visible() {
            self.live_previews.clear();
            self.workspace_previews.clear();
            self.closing_previews.clear();
            self.workspace_preview_views.clear();
        }
        if self.peek.is_settling() && !self.peek_frame_pending {
            self.peek_frame_pending = true;
            let entity = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = entity.update(cx, |this, cx| {
                    this.peek_frame_pending = false;
                    if this.peek.is_settling() {
                        this.peek.advance_motion(cx.background_executor().now());
                        cx.notify();
                    }
                });
            });
        }
        let (overview_visible, switcher_visible) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                store.overview_state().is_visible(),
                store.switcher_state().is_visible(),
            )
        };
        if switcher_visible || self.peek.paint_visible() {
            // Another surface took over; never fly the overview underneath it.
            self.reset_overview_zoom(cx);
        } else if overview_visible != self.overview_was_visible {
            self.follow_overview_visibility(overview_visible, window, cx);
        }
        self.advance_zoom(window, cx);
        self.settle_zoom_images(overview_visible, window);
        if !overview_visible && !self.zoom.zoom.is_active() {
            // Previews refresh on every open; drop them once nothing shows them.
            self.screen_requests.clear();
            self.screens.clear();
            self.screen_grids.clear();
            self.zoom.forget_cards();
        }
        if overview_visible && !self.overview_was_visible {
            if self.overview_variant == OverviewVariant::Current {
                self.overview_grid_scroll.scroll_to_item(0);
            }
            self.overview_list_scroll.scroll_to_item(0);
        }
        self.overview_was_visible = overview_visible;
        if overview_visible || switcher_visible {
            let session_ids: HashSet<_> = {
                let store = self.store.read().expect("session store lock poisoned");
                store.sessions().keys().cloned().collect()
            };
            self.status_glyphs
                .retain(|(id, _, _), _| session_ids.contains(id));
        }
        let root = div()
            .id("session-surfaces")
            .absolute()
            // Cached entity roots are laid out independently. Insets alone do
            // not give this absolute root a definite size, which previously
            // collapsed the overview hitbox/background to its 42 pt top inset
            // while every child visibly overflowed into the window.
            .size_full()
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(Self::handle_key_down))
            .capture_key_up(cx.listener(Self::handle_key_up))
            .on_modifiers_changed(cx.listener(Self::handle_modifiers_changed));
        if self.peek.paint_visible() {
            root.inset_0().child(self.render_tab_peek(window, cx))
        } else if self.zoom_painting() {
            root.inset_0().child(self.render_overview_zoom(window, cx))
        } else if overview_visible {
            root.inset_0().child(self.render_overview(window, cx))
        } else if switcher_visible {
            root.inset_0().child(self.render_switcher(window, cx))
        } else {
            root.size(px(0.0))
        }
    }
}

const SWITCHER_PREVIEW_WIDTH: f32 = 620.0;
const SWITCHER_PREVIEW_HEIGHT: f32 = 348.0;

impl SessionSurfaces {
    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_tab_peek_key(event, window, cx) {
            return;
        }
        let mut store = self.store.write().expect("session store lock poisoned");
        let key = switcher_key(event);
        let switcher_was_visible = store.switcher_state().is_visible();
        let switcher_handled =
            if switcher_was_visible || matches!(key, SwitcherKey::Tab { control: true, .. }) {
                store.handle_switcher_key(key)
            } else {
                false
            };
        if switcher_handled {
            if !switcher_was_visible && store.switcher_state().is_visible() {
                store.dismiss_overview();
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let modifiers = event.keystroke.modifiers;
        if !store.overview_state().is_visible() {
            if event.keystroke.key == "escape" && !store.sidebar_selection().is_empty() {
                // Match Swift: clear Finder-style sidebar gathering, but do not
                // swallow Esc because the focused terminal still needs it.
                store.clear_sidebar_selection();
                cx.notify();
            }
            return;
        }

        if self.overview_variant == OverviewVariant::Current {
            // The calm overview sets its own column count as it lays out.
            store.set_overview_columns(overview_columns(f32::from(window.viewport_size().width)));
        }
        let handled = match event.keystroke.key.as_str() {
            "escape" => store.overview_escape(),
            "backspace" | "delete" => store.overview_backspace(),
            "left" => store.move_overview_focus(OverviewArrow::Left),
            "right" => store.move_overview_focus(OverviewArrow::Right),
            "up" => store.move_overview_focus(OverviewArrow::Up),
            "down" => store.move_overview_focus(OverviewArrow::Down),
            "enter" => store.activate_overview_focus(),
            "a" if modifiers.platform => {
                store.select_all_overview_sessions();
                true
            }
            "space" if !modifiers.platform && !modifiers.control => {
                store.append_overview_query(" ")
            }
            _ if !modifiers.platform && !modifiers.control => event
                .keystroke
                .key_char
                .as_deref()
                .is_some_and(|text| store.append_overview_query(text)),
            _ => false,
        };
        if handled {
            drop(store);
            self.reveal_overview_focus(window);
            cx.stop_propagation();
            cx.notify();
        }
        // Boundary arrows, Backspace on an empty query, and stray typing
        // belong to this overlay too; never send them to the covered PTY.
        if !modifiers.platform && !modifiers.control {
            cx.stop_propagation();
        }
    }

    fn reveal_overview_focus(&self, window: &Window) {
        if self.overview_variant != OverviewVariant::Current {
            let focused = self
                .store
                .read()
                .expect("session store lock poisoned")
                .overview_state()
                .focused()
                .cloned();
            if let Some(id) = focused {
                self.reveal_calm_card(&id, window, false);
            }
            return;
        }
        let mut store = self.store.write().expect("session store lock poisoned");
        let sessions = store.ordered_sessions();
        let state = store.overview_state();
        if let Some(index) = state
            .visible_sessions(&sessions)
            .position(|s| Some(&s.id) == state.focused())
        {
            if state.mode() == OverviewMode::Grid {
                self.overview_grid_scroll.scroll_to_item(
                    index / overview_columns(f32::from(window.viewport_size().width)),
                );
            } else {
                self.overview_list_scroll.scroll_to_item(index);
            }
        }
    }

    pub(crate) fn handle_key_up(
        &mut self,
        event: &KeyUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // macOS normally emits ModifiersChanged for this. KeyUp is a defensive
        // fallback for platforms/backends that report the released modifier as
        // a regular key.
        let mut store = self.store.write().expect("session store lock poisoned");
        if store.switcher_state().is_visible()
            && matches!(event.keystroke.key.as_str(), "control" | "ctrl")
        {
            store.handle_switcher_modifiers_changed(false);
            cx.notify();
        }
    }

    pub(crate) fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let was_visible = store.switcher_state().is_visible();
        store.handle_switcher_modifiers_changed(event.modifiers.control);
        if was_visible != store.switcher_state().is_visible() {
            cx.notify();
        }
    }

    fn render_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (state, sessions) = {
            let store = self.store.read().expect("session store lock poisoned");
            let state = store.switcher_state().clone();
            let sessions: Vec<_> = state
                .order()
                .iter()
                .filter_map(|id| store.sessions().get(id).cloned())
                .collect();
            (state, sessions)
        };
        let highlighted = sessions.get(state.index()).cloned();
        let colors = self.colors();

        let preview_content = highlighted.as_ref().map_or_else(
            || div().size_full().into_any_element(),
            |session| self.render_grid_or_logo(session, 56.0, 8.0, colors),
        );
        let footer = highlighted.as_ref().map(|session| {
            let kind = ui_agent_kind(session.effective_kind());
            let status = self.status_glyph(session, 22.0, colors, window, cx);
            let mut details = div().flex().flex_col().min_w_0().gap(px(2.0)).child(
                div()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.primary)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(display_title(session)),
            );
            let folder = Path::new(&session.cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&session.cwd);
            let metadata = if let Some(branch) = session.git_branch.as_ref() {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(sf_symbol("arrow.branch", 11.0, colors.secondary))
                    .child(branch.clone())
                    .child(folder.to_owned())
                    .into_any_element()
            } else {
                div().child(folder.to_owned()).into_any_element()
            };
            details = details.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(metadata),
            );
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .h(px(54.0))
                .px(px(14.0))
                .bg(colors.floating_surface())
                .child(AgentLogo::new(kind, 26.0, colors))
                .child(details)
                .child(div().flex_1())
                .child(status)
                .into_any_element()
        });

        let mut filmstrip = div()
            .id("switcher-filmstrip")
            .flex()
            .w(px(SWITCHER_PREVIEW_WIDTH))
            .gap(px(10.0))
            .p(px(6.0))
            .overflow_x_scroll();
        for (index, session) in sessions.iter().enumerate() {
            let active = index == state.index();
            filmstrip = filmstrip.child(
                div()
                    .id(("switcher-chip", index))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(7.0))
                    .max_w(px(190.0))
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(Radius::ROW))
                    .bg(if active {
                        Palette::CLAY.alpha(0.22)
                    } else {
                        colors.primary.alpha(0.06)
                    })
                    .border_1()
                    .border_color(if active {
                        Palette::CLAY
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .commit_switcher_index(index);
                        cx.notify();
                    }))
                    .child(AgentLogo::new(
                        ui_agent_kind(session.effective_kind()),
                        18.0,
                        colors,
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(13.0))
                            .text_color(if active {
                                colors.primary
                            } else {
                                colors.secondary
                            })
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(display_title(session)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .size(px(5.0))
                            .rounded_full()
                            .bg(status_color(session, colors)),
                    )
                    .when(active, |chip| chip.shadow_sm())
                    .when(!active, |chip| chip.opacity(0.92)),
            );
        }

        let panel = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .p(px(18.0))
            .rounded(px(22.0))
            .bg(colors.sidebar_surface())
            .border_1()
            .border_color(colors.primary.alpha(0.08))
            .shadow(vec![BoxShadow {
                color: rgba(0x00000080).into(),
                offset: point(px(0.0), px(18.0)),
                blur_radius: px(44.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(SWITCHER_PREVIEW_WIDTH))
                    .rounded(px(Radius::CARD))
                    .overflow_hidden()
                    .border_2()
                    .border_color(Palette::CLAY)
                    .child(
                        div()
                            .relative()
                            .w(px(SWITCHER_PREVIEW_WIDTH))
                            .h(px(SWITCHER_PREVIEW_HEIGHT))
                            .bg(colors.background)
                            .child(preview_content),
                    )
                    .children(footer),
            )
            .child(filmstrip);

        div()
            .id("switcher-scrim")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000001a))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .cancel_switcher();
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(panel)
            .into_any_element()
    }

    fn render_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.overview_variant != OverviewVariant::Current {
            return self.render_calm_overview(self.overview_variant, window, cx);
        }
        let (sessions, state) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            (store.ordered_sessions(), store.overview_state().clone())
        };
        let colors = self.colors();
        let project_count = sessions
            .iter()
            .map(|session| &session.project_id)
            .collect::<HashSet<_>>()
            .len();
        let summary = format!(
            "{} session{} · {} project{}",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" },
            project_count,
            if project_count == 1 { "" } else { "s" }
        );

        let mode_selector = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(Radius::ROW))
            .bg(colors.primary.alpha(0.045))
            .border_1()
            .border_color(colors.primary.alpha(0.07))
            .child(self.mode_button(OverviewMode::Grid, state.mode(), "Gallery", colors, cx))
            .child(self.mode_button(OverviewMode::List, state.mode(), "List", colors, cx));

        let header =
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.0))
                .h(px(64.0))
                .px(px(24.0))
                .child(
                    div()
                        .text_size(px(16.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.primary)
                        .child("Sessions"),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(colors.tertiary)
                        .child(summary),
                )
                .child(div().flex_1())
                .when(f32::from(window.viewport_size().width) >= 800.0, |header| {
                    header.child(div().text_size(px(11.0)).text_color(colors.tertiary).child(
                        format!("{} to select", crate::commands::primary_click_label()),
                    ))
                })
                .child(mode_selector)
                .child(
                    div()
                        .id("overview-refresh")
                        .h(px(28.0))
                        .px(px(9.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::ROW))
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .hover(|s| s.bg(colors.primary.alpha(0.07)))
                        .child("Refresh")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.screen_requests.clear();
                            this.screens.clear();
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("close-overview")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(26.0))
                        .rounded_full()
                        .bg(colors.primary.alpha(0.045))
                        .border_1()
                        .border_color(colors.primary.alpha(0.07))
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.secondary)
                        .cursor_pointer()
                        .hover(|style| style.bg(colors.primary.alpha(0.10)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .dismiss_overview();
                            cx.notify();
                        }))
                        .child(sf_symbol_weighted(
                            "xmark",
                            11.0,
                            SymbolWeight::Semibold,
                            colors.secondary,
                        )),
                );

        let mut filters = div()
            .id("overview-filters")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(38.0))
            .px(px(24.0))
            .overflow_x_scroll();
        filters = filters.child(self.filter_chip(
            OverviewFilter::All,
            "All",
            sessions.len(),
            state.filter(),
            colors,
            cx,
        ));
        for lane in OverviewLane::ALL {
            let count = sessions
                .iter()
                .filter(|session| OverviewLane::for_session(session) == lane)
                .count();
            if count > 0 {
                filters = filters.child(self.filter_chip(
                    OverviewFilter::Lane(lane),
                    lane.label(),
                    count,
                    state.filter(),
                    colors,
                    cx,
                ));
            }
        }
        let search = div()
            .id("overview-search")
            .flex()
            .items_center()
            .gap(px(9.0))
            .mx(px(24.0))
            .mb(px(12.0))
            .px(px(12.0))
            .h(px(38.0))
            .flex_none()
            .rounded(px(Radius::ROW))
            .bg(colors.primary.alpha(0.035))
            .border_1()
            .border_color(colors.primary.alpha(0.12))
            .on_click(cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)))
            .child(sf_symbol("magnifyingglass", 14.0, colors.secondary))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.0))
                    .text_color(if state.query().is_empty() {
                        colors.tertiary
                    } else {
                        colors.primary
                    })
                    .child(if state.query().is_empty() {
                        "Type to find a session, folder, branch, or agent…".to_owned()
                    } else {
                        state.query().to_owned()
                    }),
            )
            .when(!state.query().is_empty(), |search| {
                search.child(
                    div()
                        .id("overview-clear-search")
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child("Clear")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.write().unwrap().overview_escape();
                            cx.notify();
                        })),
                )
            });

        let visible_count = state.visible_sessions(&sessions).count();
        let body = if visible_count == 0 {
            self.overview_empty_state(&state, colors, cx)
        } else if state.mode() == OverviewMode::Grid {
            self.overview_gallery(&sessions, &state, colors, window, cx)
        } else {
            self.overview_list(&sessions, &state, colors, window, cx)
        };

        let chrome = div()
            .flex()
            .flex_none()
            .flex_col()
            .bg(colors.background)
            .child(header)
            .child(search)
            .child(filters)
            .child(HairlineDivider::horizontal(colors));

        let content = div()
            .id("overview-content")
            .debug_selector(|| "OVERVIEW_CONTENT".into())
            .absolute()
            .inset_0()
            .size_full()
            .flex()
            .flex_col()
            .pt(px(diri_ui::Metrics::TITLE_BAR))
            .bg(colors.background)
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(chrome)
            .child(body)
            .child(
                div()
                    .flex_none()
                    .h(px(34.0))
                    .px(px(24.0))
                    .flex()
                    .items_center()
                    .gap(px(18.0))
                    .border_t_1()
                    .border_color(colors.primary.alpha(0.07))
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child("↑ ↓ ← →  Navigate")
                    .child("↵  Open session")
                    .child("esc  Back")
                    .child(div().flex_1())
                    .when(f32::from(window.viewport_size().width) >= 800.0, |footer| {
                        footer.child("Screen previews · refresh on open")
                    }),
            )
            .when(!state.selection().is_empty(), |content| {
                content.child(self.bulk_close_bar(state.selection().len(), visible_count, cx))
            });

        // The entrance is the zoom from the page (overview_zoom_surface), so
        // the grid itself no longer fades in on its own.
        let content = content.into_any_element();

        div()
            .id("overview-scrim")
            .absolute()
            .inset_0()
            .size_full()
            .bg(colors.work_surface())
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .overview_escape();
                cx.notify();
            }))
            .child(content)
            .into_any_element()
    }

    fn mode_button(
        &self,
        mode: OverviewMode,
        current: OverviewMode,
        label: &'static str,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = mode == current;
        div()
            .id(SharedString::from(format!("overview-mode-{label}")))
            .flex_none()
            .h(px(24.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(Radius::BADGE))
            .bg(colors.primary.alpha(if active { 0.10 } else { 0.0 }))
            .text_size(px(11.0))
            .text_color(if active {
                colors.primary
            } else {
                colors.secondary
            })
            .cursor_pointer()
            .hover(|style| style.bg(colors.primary.alpha(if active { 0.12 } else { 0.055 })))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .set_overview_mode(mode);
                this.reveal_overview_focus(window);
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    fn filter_chip(
        &self,
        filter: OverviewFilter,
        label: &'static str,
        count: usize,
        current: OverviewFilter,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = filter == current;
        div()
            .id(SharedString::from(format!("overview-filter-{label}")))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.0))
            .h(px(22.0))
            .px(px(9.0))
            .rounded_full()
            .bg(colors.primary.alpha(if active { 0.10 } else { 0.0 }))
            .border_1()
            .border_color(colors.primary.alpha(if active { 0.16 } else { 0.08 }))
            .text_size(px(11.0))
            .text_color(if active {
                colors.primary
            } else {
                colors.secondary
            })
            .cursor_pointer()
            .hover(|style| style.bg(colors.primary.alpha(if active { 0.12 } else { 0.05 })))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .set_overview_filter(filter);
                this.reveal_overview_focus(window);
                cx.notify();
            }))
            .child(label)
            .child(div().text_color(colors.tertiary).child(count.to_string()))
            .into_any_element()
    }

    fn overview_empty_state(
        &self,
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = if !state.query().is_empty() {
            format!("No matches for “{}”", state.query())
        } else {
            match state.filter() {
                OverviewFilter::All => "No sessions".to_owned(),
                OverviewFilter::Lane(lane) => {
                    format!("No {} sessions", lane.label().to_lowercase())
                }
            }
        };
        let filtered = !state.query().is_empty() || state.filter() != OverviewFilter::All;
        div()
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .text_color(colors.tertiary)
            .child(sf_symbol_weighted(
                "square.grid.2x2",
                26.0,
                SymbolWeight::Regular,
                colors.tertiary,
            ))
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.secondary)
                    .child(title),
            )
            .child(div().text_size(px(12.0)).child(if filtered {
                "Try another name, folder, or branch."
            } else {
                "Start a session from your workspace to see it here."
            }))
            .child(
                div()
                    .id("overview-empty-action")
                    .mt(px(8.0))
                    .px(px(14.0))
                    .py(px(8.0))
                    .rounded(px(Radius::ROW))
                    .bg(colors.primary.alpha(0.07))
                    .text_color(colors.primary)
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(colors.primary.alpha(0.12)))
                    .child(if filtered {
                        "Show all sessions"
                    } else {
                        "Back to workspace"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let mut store = this.store.write().unwrap();
                        if filtered {
                            if !store.overview_state().query().is_empty() {
                                store.overview_escape();
                            }
                            store.set_overview_filter(OverviewFilter::All);
                        } else {
                            store.dismiss_overview();
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn overview_gallery(
        &mut self,
        sessions: &[SessionRecord],
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let columns = overview_columns(f32::from(window.viewport_size().width));
        self.store.write().unwrap().set_overview_columns(columns);
        let visible: Vec<_> = state.visible_sessions(sessions).collect();
        let mut gallery = div()
            .id("overview-gallery")
            .debug_selector(|| "OVERVIEW_GALLERY".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .gap(px(16.0))
            .p(px(24.0))
            .pb(px(if state.selection().is_empty() {
                24.0
            } else {
                82.0
            }))
            .track_scroll(&self.overview_grid_scroll)
            .overflow_y_scroll();
        for row in visible.chunks(columns) {
            let mut cards = div().flex().flex_none().gap(px(16.0));
            for session in row {
                cards = cards.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.overview_card(session, state, colors, window, cx)),
                );
            }
            for _ in row.len()..columns {
                cards = cards.child(div().flex_1().min_w_0());
            }
            gallery = gallery.child(cards);
        }
        gallery.into_any_element()
    }

    fn overview_list(
        &mut self,
        sessions: &[SessionRecord],
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let visible: Vec<_> = state.visible_sessions(sessions).cloned().collect();
        let bottom_padding = if state.selection().is_empty() {
            18.0
        } else {
            82.0
        };
        let mut list = div()
            .id("overview-list")
            .debug_selector(|| "OVERVIEW_LIST".into())
            .flex()
            .flex_1()
            .min_h_0()
            .flex_col()
            .gap(px(8.0))
            .px(px(20.0))
            .pt(px(14.0))
            .pb(px(bottom_padding))
            .track_scroll(&self.overview_list_scroll)
            .overflow_y_scroll();
        list.style().restrict_scroll_to_axis = Some(true);
        for session in &visible {
            list = list.child(self.overview_list_row(session, state, colors, window, cx));
        }
        list.into_any_element()
    }

    fn overview_card(
        &mut self,
        session: &SessionRecord,
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = state.selection().contains(&session.id);
        let focused = state.focused() == Some(&session.id);
        let has_selection = !state.selection().is_empty();
        let id = session.id.clone();
        let close_id = id.clone();
        let status = self.status_glyph(session, 14.0, colors, window, cx);
        let preview = self.overview_preview(session, colors, cx);

        let mut thumbnail = div()
            .relative()
            .w_full()
            .h(px(178.0))
            .rounded(px(Radius::ROW))
            .overflow_hidden()
            .bg(colors.background)
            .border_1()
            .border_color(colors.primary.alpha(0.075))
            .when(session.hibernation.is_some(), |thumbnail| {
                thumbnail.opacity(0.68)
            })
            .child(preview);
        if selected {
            thumbnail = thumbnail.child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .right(px(6.0))
                    .size(px(18.0))
                    .rounded_full()
                    .bg(Palette::CLAY)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol_weighted(
                        "checkmark",
                        9.0,
                        SymbolWeight::Bold,
                        colors.primary,
                    )),
            );
        } else if !has_selection {
            thumbnail = thumbnail.child(
                div()
                    .id(SharedString::from(format!("close-card-{}", close_id.0)))
                    .absolute()
                    .top(px(6.0))
                    .left(px(6.0))
                    .size(px(20.0))
                    .rounded_full()
                    .bg(colors.floating_surface())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.primary)
                    .cursor_pointer()
                    .invisible()
                    .group_hover("overview-card", |style| style.visible())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .close_overview_session(close_id.clone());
                        cx.notify();
                    }))
                    .child(sf_symbol_weighted(
                        "xmark",
                        9.0,
                        SymbolWeight::Bold,
                        colors.primary,
                    )),
            );
        }

        div()
            .id(SharedString::from(format!("overview-card-{}", id.0)))
            .debug_selector(|| format!("OVERVIEW_CARD_{}", id.0))
            .group("overview-card")
            .flex()
            .flex_none()
            .flex_col()
            .gap(px(8.0))
            .p(px(8.0))
            .rounded(px(Radius::CARD))
            .bg(if selected {
                Palette::CLAY.alpha(0.085)
            } else if focused {
                colors.primary.alpha(0.055)
            } else {
                colors.primary.alpha(0.032)
            })
            .border_1()
            .border_color(if selected {
                Palette::CLAY
            } else if focused {
                colors.primary.alpha(0.24)
            } else {
                colors.primary.alpha(0.06)
            })
            .cursor_pointer()
            .hover(|style| {
                if selected {
                    style
                        .bg(Palette::CLAY.alpha(0.105))
                        .border_color(Palette::CLAY)
                } else {
                    style
                        .bg(colors.primary.alpha(0.06))
                        .border_color(colors.primary.alpha(0.12))
                }
            })
            .active(|style| style.bg(colors.primary.alpha(0.075)))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.modifiers().platform {
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .toggle_overview_selection(id.clone());
                } else {
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .activate_overview_session(id.clone());
                }
                cx.notify();
            }))
            .child(thumbnail)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(2.0))
                    .pb(px(1.0))
                    .child(status)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(display_title(session)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(2.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(format!(
                                "{}{}",
                                Path::new(&session.cwd)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(&session.cwd),
                                session
                                    .git_branch
                                    .as_ref()
                                    .map(|b| format!("  /  {b}"))
                                    .unwrap_or_default()
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(status_color(session, colors))
                            .child(OverviewLane::for_session(session).label()),
                    ),
            )
            .into_any_element()
    }

    fn overview_list_row(
        &mut self,
        session: &SessionRecord,
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = state.selection().contains(&session.id);
        let focused = state.focused() == Some(&session.id);
        let id = session.id.clone();
        let close_id = id.clone();
        let status = self.status_glyph(session, 16.0, colors, window, cx);
        div()
            .id(SharedString::from(format!("overview-row-{}", id.0)))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .h(px(94.0))
            .px(px(10.0))
            .rounded(px(Radius::ROW))
            .bg(colors.primary.alpha(if selected {
                0.085
            } else if focused {
                0.055
            } else {
                0.028
            }))
            .border_1()
            .border_color(if selected {
                Palette::CLAY
            } else {
                colors.primary.alpha(if focused { 0.18 } else { 0.06 })
            })
            .cursor_pointer()
            .hover(|style| {
                if selected {
                    style
                        .bg(colors.primary.alpha(0.10))
                        .border_color(Palette::CLAY)
                } else {
                    style.bg(colors.primary.alpha(0.06))
                }
            })
            .active(|style| style.bg(colors.primary.alpha(0.075)))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.modifiers().platform {
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .toggle_overview_selection(id.clone());
                } else {
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .activate_overview_session(id.clone());
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(160.0))
                    .h(px(76.0))
                    .rounded(px(Radius::BADGE))
                    .overflow_hidden()
                    .bg(colors.background)
                    .child(self.overview_preview(session, colors, cx)),
            )
            .child(status)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .flex_1()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(display_title(session)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(11.0))
                            .text_color(colors.tertiary)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(session.cwd.clone()),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(status_color(session, colors).alpha(0.12))
                    .text_size(px(11.0))
                    .text_color(status_color(session, colors))
                    .child(OverviewLane::for_session(session).label()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("close-row-{}", close_id.0)))
                    .size(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .text_color(colors.secondary)
                    .hover(|style| style.bg(colors.primary.alpha(0.08)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .close_overview_session(close_id.clone());
                        cx.notify();
                    }))
                    .child(sf_symbol_weighted(
                        "xmark",
                        10.0,
                        SymbolWeight::Bold,
                        colors.secondary,
                    )),
            )
            .into_any_element()
    }

    fn bulk_close_bar(
        &self,
        count: usize,
        visible_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        div()
            .absolute()
            .bottom(px(20.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.0))
                    .px(px(16.0))
                    .py(px(10.0))
                    .rounded_full()
                    .bg(colors.floating_surface())
                    .border_1()
                    .border_color(colors.primary.alpha(0.10))
                    .shadow_lg()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child(format!("{count} selected")),
                    )
                    .when(count < visible_count, |bar| {
                        bar.child(
                            div()
                                .id("select-all-overview")
                                .text_size(px(13.0))
                                .text_color(colors.secondary)
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .select_all_overview_sessions();
                                    cx.notify();
                                }))
                                .child("Select All"),
                        )
                    })
                    .child(div().w(px(1.0)).h(px(16.0)).bg(colors.primary.alpha(0.10)))
                    .child(
                        div()
                            .id("cancel-overview-selection")
                            .text_size(px(13.0))
                            .text_color(colors.secondary)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store
                                    .write()
                                    .expect("session store lock poisoned")
                                    .clear_overview_selection();
                                cx.notify();
                            }))
                            .child("Cancel"),
                    )
                    .child(
                        div()
                            .id("close-overview-selection")
                            .px(px(12.0))
                            .h(px(28.0))
                            .flex()
                            .items_center()
                            .rounded_full()
                            .bg(Ink::DANGER)
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.primary)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store
                                    .write()
                                    .expect("session store lock poisoned")
                                    .close_overview_selection();
                                cx.notify();
                            }))
                            .child(if count == 1 {
                                "Close 1 Session".to_owned()
                            } else {
                                format!("Close {count} Sessions")
                            }),
                    ),
            )
            .into_any_element()
    }

    /// Screens are fetched while the grid is up, and from the first touch of
    /// a pinch so the page's card is ready before it is visible.
    fn overview_wants_screens(&self) -> bool {
        self.store.read().unwrap().overview_state().is_visible() || self.zoom.zoom.is_active()
    }

    fn request_screen(&mut self, id: SessionId, cx: &mut Context<Self>) {
        if !self.overview_wants_screens()
            || self.screens.contains_key(&id)
            || self.screen_requests.contains_key(&id)
            || self.screen_requests.len() >= 4
        {
            return;
        }
        let Some(tokio) = &self.tokio else {
            return;
        };
        let client = Arc::clone(&self.client);
        let request_id = id.clone();
        let request = tokio.spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let screen = client.read_screen(&request_id).await;
                // The visible grid with its colors: one probe for where the
                // live grid starts, then the rows themselves.
                let grid = async {
                    let probe = client.read_scrollback_cells(&request_id, 0, 1).await.ok()?;
                    let rows = (probe.total_rows - probe.live_start_row).clamp(1, 200);
                    client
                        .read_scrollback_cells(&request_id, probe.live_start_row, rows)
                        .await
                        .ok()
                }
                .await;
                (screen, grid)
            })
            .await
            .map(|(screen, grid)| screen.map(|screen| (screen, grid)))
        });
        let abort = request.abort_handle();
        let task_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut grid = None;
            let preview = match request.await {
                Ok(Ok(Ok((screen, cells)))) => {
                    grid = cells.and_then(|cells| {
                        let cols = u16::try_from(cells.cols).ok()?.max(1);
                        let count = usize::try_from(cells.row_count).ok()?;
                        let rows =
                            diri_proto::grid::GridRowCodec::decode_rows(&cells.payload, count)
                                .ok()?;
                        let height = u16::try_from(rows.len()).ok()?.max(1);
                        let element = TerminalElement::with_buffer(
                            diri_term::buffer::GridBuffer::new(cols, height),
                        );
                        element.apply_damage(diri_proto::grid::GridUpdate {
                            cols,
                            rows: height,
                            cursor_col: 0,
                            cursor_row: 0,
                            cursor_visible: false,
                            is_full_snapshot: true,
                            changed_rows: rows
                                .into_iter()
                                .enumerate()
                                .map(|(row, cells)| {
                                    diri_proto::grid::ChangedRow::new(row as u16, cells)
                                })
                                .collect(),
                            cursor_style: Default::default(),
                            history: None,
                        });
                        Some(element)
                    });
                    let lines = screen_excerpt(&screen.text);
                    if lines.is_empty() {
                        ScreenPreview::Empty
                    } else {
                        ScreenPreview::Ready(lines)
                    }
                }
                _ => ScreenPreview::Unavailable,
            };
            let _ = this.update(cx, |this, cx| {
                this.screen_requests.remove(&task_id);
                if this.overview_wants_screens() {
                    if let Some(grid) = grid {
                        this.screen_grids.insert(task_id.clone(), grid);
                    }
                    this.screens.insert(task_id, preview);
                }
                cx.notify();
            });
        });
        self.screen_requests
            .insert(id, ScreenRequest { _task: task, abort });
    }

    fn overview_preview(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        let id = session.id.clone();
        let mut preview = div()
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(colors.work_surface());
        if let Some(ScreenPreview::Ready(lines)) = self.screens.get(&id) {
            preview = preview.child(
                div()
                    .p(px(12.0))
                    .text_size(px(10.0))
                    .line_height(px(12.5))
                    .font_family(crate::fonts::mono_family())
                    .text_color(colors.secondary)
                    .whitespace_nowrap()
                    .child(lines.join("\n")),
            );
        } else {
            let label = match self.screens.get(&id) {
                _ if session.is_note() => crate::i18n::t("nav.notes.note"),
                Some(ScreenPreview::Unavailable) => "Preview unavailable",
                Some(ScreenPreview::Empty) => "No screen output yet",
                _ => "Loading preview…",
            };
            preview = preview.child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .child(
                        AgentLogo::new(ui_agent_kind(session.effective_kind()), 28.0, colors)
                            .badged(false),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(colors.tertiary)
                            .child(label),
                    ),
            );
        }
        // A note has no terminal to preview.
        if session.is_note()
            || self.screens.contains_key(&id)
            || self.screen_requests.contains_key(&id)
        {
            return preview.into_any_element();
        }
        // Prepaint receives the scroll viewport's clip. Offscreen sessions do
        // no I/O; completions repaint and allow the next four visible requests.
        preview
            .child(
                gpui::canvas(
                    move |bounds, window, cx| {
                        if bounds.intersects(&window.content_mask().bounds) {
                            cx.defer(move |cx| {
                                let _ = weak.update(cx, |this, cx| this.request_screen(id, cx));
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    fn render_grid_or_logo(
        &self,
        session: &SessionRecord,
        logo_size: f32,
        font_size: f32,
        colors: SemanticColors,
    ) -> AnyElement {
        if let Some(preview) = self.resident_previews.get(&session.id) {
            preview.clone().font_size(px(font_size)).into_any_element()
        } else {
            div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .bg(colors.work_surface())
                .opacity(0.60)
                .child(
                    AgentLogo::new(ui_agent_kind(session.effective_kind()), logo_size, colors)
                        .badged(false),
                )
                .into_any_element()
        }
    }

    fn status_glyph(
        &mut self,
        session: &SessionRecord,
        size: f32,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<StatusGlyph> {
        let state = ui_status_state(session);
        let kind = ui_agent_kind(session.effective_kind());
        let key = (session.id.clone(), (size * 10.0).round() as u16, kind);
        let glyph = self
            .status_glyphs
            .entry(key)
            .or_insert_with(|| StatusGlyph::entity(kind, state, size, colors, cx))
            .clone();
        glyph.update(cx, |glyph, cx| {
            glyph.set_state(state, window, cx);
            glyph.set_colors(colors, cx);
        });
        glyph
    }
}

pub(crate) fn switcher_key(event: &KeyDownEvent) -> SwitcherKey {
    match event.keystroke.key.as_str() {
        "tab" => SwitcherKey::Tab {
            control: event.keystroke.modifiers.control,
            shift: event.keystroke.modifiers.shift,
        },
        "escape" => SwitcherKey::Escape,
        "enter" => SwitcherKey::Enter,
        "left" => SwitcherKey::ArrowLeft,
        "right" => SwitcherKey::ArrowRight,
        "up" => SwitcherKey::ArrowUp,
        "down" => SwitcherKey::ArrowDown,
        _ => SwitcherKey::Other,
    }
}

fn ui_agent_kind(kind: &ProtoAgentKind) -> AgentKind {
    // Brand vocabulary, not a protocol type: a manifest agent the client has
    // no hand-drawn mark for falls back to the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        id => AgentKind::from_agent_id(id),
    }
}

fn ui_status_state(session: &SessionRecord) -> StatusState {
    if session.hibernation.is_some() {
        return StatusState::Hibernated;
    }
    match session.attention() {
        AttentionLevel::Working => StatusState::Working,
        AttentionLevel::NeedsInput => StatusState::NeedsInput {
            destructive: session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == RiskHint::Destructive),
        },
        AttentionLevel::DoneUnseen => StatusState::DoneUnseen,
        AttentionLevel::IdleSeen => StatusState::IdleSeen,
        AttentionLevel::None | AttentionLevel::Unknown => StatusState::None,
    }
}

fn status_color(session: &SessionRecord, colors: SemanticColors) -> gpui::Rgba {
    match ui_status_state(session) {
        StatusState::Working => Ink::working(ui_agent_kind(session.effective_kind()), colors),
        StatusState::NeedsInput { destructive: true } => Ink::DANGER,
        StatusState::NeedsInput { destructive: false } => Ink::ATTENTION,
        StatusState::DoneUnseen => Ink::FRESH,
        StatusState::IdleSeen | StatusState::None | StatusState::Hibernated => colors.secondary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use diri_proto::{
        AgentKind as ProtoAgentKind, DateMillis, ProjectId, Resumability, SessionListResult,
        SessionStatus, TitleSource,
    };
    use gpui::{ScrollDelta, ScrollWheelEvent, StyleRefinement, TestAppContext, size};

    struct OverviewHarness {
        surfaces: Entity<SessionSurfaces>,
        background_scrolls: Arc<AtomicUsize>,
    }

    impl Render for OverviewHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let background_scrolls = Arc::clone(&self.background_scrolls);
            let background_keys = Arc::clone(&self.background_scrolls);
            div()
                .bg(self.surfaces.read(cx).colors().background)
                .size_full()
                .on_key_down(move |_, _, _| {
                    background_keys.fetch_add(1, Ordering::Relaxed);
                })
                .child(div().absolute().inset_0().on_scroll_wheel(move |_, _, _| {
                    background_scrolls.fetch_add(1, Ordering::Relaxed);
                }))
                .child(
                    self.surfaces
                        .clone()
                        .cached(StyleRefinement::default().absolute().inset_0()),
                )
        }
    }

    fn session(index: usize) -> SessionRecord {
        SessionRecord { command_run: None,
            attention_state: None,
            id: SessionId::new(format!("running-{index:02}")),
            kind: ProtoAgentKind::CODEX,
            cwd: "/work/overview".into(),
            project_id: ProjectId::new("overview"),
            worktree_path: None,
            git_branch: Some(format!("feature/session-{index:02}")),
            title: format!("Overflowing session {index:02}"),
            title_source: TitleSource::AgentProvided,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
            capabilities: None,
            parent: None,
            created_at: DateMillis(index as f64),
            updated_at: DateMillis(index as f64),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
            terminal_cwd: None,
            agent_workspace: None,
            note_id: None,
            foreground_ports: None,
            terminal_progress: None,
            scheduled_run: None,
        }
    }

    #[gpui::test]
    fn gallery_scrolls_without_reaching_the_background(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: (0..18).map(session).collect(),
            projects: vec![],
        });

        let background_scrolls = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&background_scrolls);
        let (view, cx) = cx.add_window_view(move |_, cx| OverviewHarness {
            surfaces: cx.new(|cx| {
                let mut surfaces = SessionSurfaces::new(runtime, None, cx);
                // This covers the original gallery, not the calm prototypes.
                surfaces.overview_variant = OverviewVariant::Current;
                surfaces.store.write().unwrap().toggle_overview();
                surfaces
            }),
            background_scrolls: probe,
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));
        let surfaces = view.read_with(cx, |h, _| h.surfaces.clone());
        // Let the opening cross-fade land; the grid takes input once it rests.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        surfaces.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let bounds = cx.debug_bounds("OVERVIEW_GALLERY").unwrap();
        assert_eq!(
            cx.debug_bounds("OVERVIEW_CONTENT").unwrap().size,
            size(px(1100.0), px(700.0))
        );
        assert!(bounds.size.height > px(300.0));
        assert_eq!(
            surfaces.read_with(cx, |s, _| s.overview_grid_scroll.max_offset().x),
            px(0.0)
        );
        assert!(surfaces.read_with(cx, |s, _| s.overview_grid_scroll.max_offset().y) > px(0.0));
        cx.simulate_event(ScrollWheelEvent {
            position: bounds.center(),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-80.0))),
            ..ScrollWheelEvent::default()
        });
        assert!(surfaces.read_with(cx, |s, _| s.overview_grid_scroll.offset().y) < px(0.0));
        assert_eq!(background_scrolls.load(Ordering::Relaxed), 0);
        cx.simulate_resize(size(px(680.0), px(700.0)));
        assert_eq!(
            surfaces.read_with(cx, |s, _| s.overview_grid_scroll.max_offset().x),
            px(0.0)
        );
    }

    /// A note card has no terminal: asking for its screen could only fail
    /// with `session_has_no_terminal`, every time the overview opened.
    #[gpui::test]
    fn overview_never_asks_a_note_for_its_screen(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        // A current-thread runtime nothing drives: requests are queued (which
        // is what this test checks) but never run, so no worker thread wakes
        // the GPUI test scheduler from outside and the test stays deterministic.
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut note = session(1);
        note.kind = ProtoAgentKind::NOTE;
        note.note_id = Some("n_plan".into());
        let (terminal_id, note_id) = (session(0).id, note.id.clone());
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: vec![session(0), note],
            projects: vec![],
        });
        let handle = tokio.handle().clone();
        let (view, cx) = cx.add_window_view(move |_, cx| OverviewHarness {
            surfaces: cx.new(|cx| {
                let mut surfaces = SessionSurfaces::new(runtime, Some(handle), cx);
                surfaces.overview_variant = OverviewVariant::Current;
                surfaces.store.write().unwrap().toggle_overview();
                surfaces
            }),
            background_scrolls: Arc::new(AtomicUsize::new(0)),
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));
        let surfaces = view.read_with(cx, |h, _| h.surfaces.clone());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        surfaces.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        surfaces.read_with(cx, |s, _| {
            let asked =
                |id: &SessionId| s.screen_requests.contains_key(id) || s.screens.contains_key(id);
            assert!(asked(&terminal_id), "the terminal card asks for its screen");
            assert!(!asked(&note_id), "the note card must not");
        });
        drop(tokio);
    }

    #[gpui::test]
    fn overview_keeps_boundary_keys_away_from_the_terminal(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: vec![session(0)],
            projects: vec![],
        });

        let escaped = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&escaped);
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let surfaces = cx.new(|cx| SessionSurfaces::new(runtime, None, cx));
            surfaces.read(cx).store.write().unwrap().toggle_overview();
            surfaces.read(cx).focus_handle.clone().focus(window, cx);
            OverviewHarness {
                surfaces,
                background_scrolls: probe,
            }
        });
        cx.simulate_keystrokes("left up backspace tab right down");
        assert_eq!(escaped.load(Ordering::Relaxed), 0);
    }

    #[gpui::test]
    fn tab_peek_preserves_grid_and_selection_until_commit(cx: &mut TestAppContext) {
        use crate::tab_peek::GestureFrame;
        use diri_term::buffer::GridBuffer;
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: (0..4)
                .map(|i| {
                    let mut record = session(i);
                    if i >= 2 {
                        record.project_id = ProjectId::new("other-project");
                    }
                    record
                })
                .collect(),
            projects: vec![],
        });
        runtime.store.write().unwrap().select(session(0).id);
        // Collapsing navigation must not remove tabs from the work collection.
        runtime
            .store
            .write()
            .unwrap()
            .toggle_project_collapsed(ProjectId::new("overview"))
            .unwrap();
        let grid = Arc::new(RwLock::new(GridBuffer::new(100, 40)));
        let live = grid.clone();
        let escaped = Arc::new(AtomicUsize::new(0));
        let probe = escaped.clone();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let surfaces = cx.new(|cx| {
                let mut surfaces = SessionSurfaces::new(runtime, None, cx);
                surfaces.set_resident_buffer(session(0).id, live);
                surfaces.tab_gesture(GestureFrame::Tracking(100.0), cx);
                surfaces
            });
            OverviewHarness {
                surfaces,
                background_scrolls: probe,
            }
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));
        let surfaces = view.read_with(cx, |h, _| h.surfaces.clone());
        let store = surfaces.read_with(cx, |surfaces, _| surfaces.store.clone());
        assert!(cx.debug_bounds("TAB_PEEK_CARD_0").is_some());
        assert_eq!(surfaces.read_with(cx, |s, _| s.peek.sessions.len()), 4);
        assert_eq!(
            store.read().unwrap().selected_session_id(),
            Some(&session(0).id)
        );
        assert_eq!(
            (grid.read().unwrap().cols, grid.read().unwrap().rows),
            (100, 40)
        );
        surfaces.update(cx, |s, cx| s.tab_gesture(GestureFrame::Released(300.0), cx));
        cx.simulate_keystrokes("right a left up down escape");
        assert_eq!(escaped.load(Ordering::Relaxed), 0);
        assert_eq!(
            store.read().unwrap().selected_session_id(),
            Some(&session(0).id)
        );
        assert!(!surfaces.read_with(cx, |s, _| s.tab_peek_visible()));
        surfaces.update(cx, |s, cx| s.toggle_tab_peek(cx));
        cx.simulate_keystrokes("right right enter");
        assert_eq!(
            store.read().unwrap().selected_session_id(),
            Some(&session(2).id)
        );
        assert!(!surfaces.read_with(cx, |s, _| s.tab_peek_visible()));
        assert_eq!(
            (grid.read().unwrap().cols, grid.read().unwrap().rows),
            (100, 40)
        );
    }

    #[gpui::test]
    fn closing_peek_returns_scroll_without_waiting_for_animation(cx: &mut TestAppContext) {
        use crate::tab_peek::GestureFrame;
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: vec![session(0)],
            projects: vec![],
        });
        runtime.store.write().unwrap().select(session(0).id);
        let scrolls = Arc::new(AtomicUsize::new(0));
        let probe = scrolls.clone();
        let (view, cx) = cx.add_window_view(move |_, cx| OverviewHarness {
            surfaces: cx.new(|cx| {
                let mut surface = SessionSurfaces::new(runtime, None, cx);
                surface.tab_gesture(GestureFrame::Tracking(140.0), cx);
                surface
            }),
            background_scrolls: probe,
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        let event = ScrollWheelEvent {
            position: point(px(100.0), px(100.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-80.0))),
            ..ScrollWheelEvent::default()
        };
        cx.simulate_event(event.clone());
        assert_eq!(scrolls.load(Ordering::Relaxed), 0);
        let surfaces = view.read_with(cx, |view, _| view.surfaces.clone());
        surfaces.update(cx, |surface, cx| {
            surface.dismiss_tab_peek(cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(surfaces.read_with(cx, |surface, _| surface.peek.paint_visible()));
        cx.simulate_event(event);
        assert_eq!(scrolls.load(Ordering::Relaxed), 1);
    }

    #[gpui::test]
    fn tab_peek_settling_requests_frames_only_until_settled_or_cancelled(cx: &mut TestAppContext) {
        use crate::tab_peek::GestureFrame;
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: vec![session(0)],
            projects: vec![],
        });
        runtime.store.write().unwrap().select(session(0).id);
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let surfaces = cx.new(|cx| {
                let mut s = SessionSurfaces::new(runtime, None, cx);
                s.tab_gesture(GestureFrame::Tracking(100.0), cx);
                s.tab_gesture(GestureFrame::Released(100.0), cx);
                s
            });
            OverviewHarness {
                surfaces,
                background_scrolls: Arc::new(AtomicUsize::new(0)),
            }
        });
        cx.simulate_resize(size(px(1000.0), px(700.0)));
        let surfaces = view.read_with(cx, |view, _| view.surfaces.clone());
        let first = surfaces.read_with(cx, |s, cx| s.tab_peek_offset(cx));
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        view.update_in(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 1);
        });
        cx.run_until_parked();
        let halfway = surfaces.read_with(cx, |s, cx| s.tab_peek_offset(cx));
        assert!(halfway > first);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        view.update_in(cx, |_, window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        assert!(!surfaces.read_with(cx, |s, _| s.peek.is_settling()));
        view.update_in(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 0);
        });
        surfaces.update(cx, |s, cx| {
            s.dismiss_tab_peek(cx);
            cx.notify();
        });
        cx.run_until_parked();
        surfaces.update(cx, |s, cx| s.cancel_tab_peek_immediately(cx));
        view.update_in(cx, |_, window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        assert!(!surfaces.read_with(cx, |s, _| s.peek.paint_visible()));
        view.update_in(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 0);
        });
    }

    #[gpui::test]
    fn tab_peek_late_card_stays_mounted_through_expand_and_reverse(cx: &mut TestAppContext) {
        use crate::tab_peek::GestureFrame;
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: (0..24).map(session).collect(),
            projects: vec![],
        });
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let surfaces = cx.new(|cx| {
                let mut surface = SessionSurfaces::new(runtime, None, cx);
                let items: Vec<_> = (0..24).map(|i| PeekItem::Session(session(i).id)).collect();
                surface
                    .peek
                    .begin(items, Some(&PeekItem::Session(session(23).id)));
                surface.tab_gesture(GestureFrame::Tracking(140.0), cx);
                surface
            });
            OverviewHarness {
                surfaces,
                background_scrolls: Arc::new(AtomicUsize::new(0)),
            }
        });
        cx.simulate_resize(size(px(400.0), px(700.0)));
        let surfaces = view.read_with(cx, |view, _| view.surfaces.clone());
        let mut previous = cx.debug_bounds("TAB_PEEK_CARD_23").unwrap();
        for step in (0..=120).chain((0..120).rev()) {
            surfaces.update(cx, |surface, cx| {
                surface.tab_gesture(GestureFrame::Tracking(140.0 + step as f32 * 2.0), cx)
            });
            cx.run_until_parked();
            let card = cx
                .debug_bounds("TAB_PEEK_CARD_23")
                .expect("selected preview remains mounted");
            assert!(
                card.top() >= px(0.0) && card.bottom() <= px(700.0),
                "{step}: {card:?}"
            );
            assert!(
                (card.top() - previous.top()).abs() < px(20.0),
                "card jumped at {step}"
            );
            previous = card;
        }
        assert_eq!(
            surfaces.read_with(cx, |s, _| s.peek_scroll.offset().y),
            px(0.0)
        );
    }

    #[gpui::test]
    fn tab_peek_streams_across_projects_with_one_resident_and_drops_on_escape(
        cx: &mut TestAppContext,
    ) {
        use crate::{tab_peek::GestureFrame, tab_preview::PreviewState};
        use diri_proto::{
            frames::{Frame, FrameCodec},
            grid::GridUpdate,
        };
        use diri_term::buffer::GridBuffer;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("preview.sock");
        // GPUI's deterministic executor rejects wakes from foreign threads.
        // Drive real socket tasks on this test thread while waiting for I/O.
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = {
            let _entered = executor.enter();
            diri_platform::ipc::asynchronous::UnixListener::bind(&socket).unwrap()
        };
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let began = std::time::Instant::now();
        let events = Arc::new(std::sync::Mutex::new(Vec::<(String, u128)>::new()));
        let server_events = events.clone();
        let server_opened = opened.clone();
        let server_closed = closed.clone();
        let server = executor.spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let opened = server_opened.clone();
                let closed = server_closed.clone();
                let events = server_events.clone();
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert!(request.get("attach").is_none());
                    assert_ne!(request["preview"], "running-00");
                    let mut ack = serde_json::to_vec(&request).unwrap();
                    ack.push(b'\n');
                    stream.get_mut().write_all(&ack).await.unwrap();
                    let update = GridUpdate {
                        cols: 80,
                        rows: 24,
                        cursor_col: 0,
                        cursor_row: 0,
                        cursor_visible: false,
                        is_full_snapshot: true,
                        changed_rows: vec![],
                        cursor_style: Default::default(),
                        history: None,
                    };
                    stream
                        .get_mut()
                        .write_all(&FrameCodec::encode(&Frame::grid(&update).unwrap()).unwrap())
                        .await
                        .unwrap();
                    events.lock().unwrap().push((
                        format!("opened {}", request["preview"]),
                        began.elapsed().as_micros(),
                    ));
                    opened.fetch_add(1, Ordering::SeqCst);
                    let mut effects = Vec::new();
                    stream.read_to_end(&mut effects).await.unwrap();
                    assert!(effects.is_empty());
                    events.lock().unwrap().push((
                        format!("EOF {}", request["preview"]),
                        began.elapsed().as_micros(),
                    ));
                    closed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: (0..4)
                .map(|i| {
                    let mut record = session(i);
                    if i >= 2 {
                        record.project_id = ProjectId::new("other-project");
                        record.host = Some("fixture-host".into());
                    }
                    record
                })
                .collect(),
            projects: vec![],
        });
        runtime.store.write().unwrap().select(session(0).id);
        let store = runtime.store.clone();
        let resident = Arc::new(RwLock::new(GridBuffer::new(100, 40)));
        let original = resident.clone();
        let handle = executor.handle().clone();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let surfaces = cx.new(|cx| {
                let mut surfaces = SessionSurfaces::new(runtime, Some(handle), cx);
                surfaces.client = Arc::new(diri_client::DaemonClient::with_socket_path(socket));
                surfaces.set_resident_buffer(session(0).id, resident);
                surfaces.tab_gesture(GestureFrame::Released(140.0), cx);
                surfaces
            });
            OverviewHarness {
                surfaces,
                background_scrolls: Arc::new(AtomicUsize::new(0)),
            }
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));
        assert!(cx.debug_bounds("TAB_PEEK_CARD_0").is_none());
        assert_eq!(
            opened.load(Ordering::SeqCst),
            0,
            "offscreen reveal must not open streams"
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        view.update_in(cx, |_, window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("TAB_PEEK_CARD_0").is_some());
        let surfaces = view.read_with(cx, |h, _| h.surfaces.clone());
        let mut states: Vec<_> = surfaces.read_with(cx, |s, _| {
            assert_eq!(s.resident_previews.len(), 1);
            (1..4)
                .map(|i| s.live_previews.get(&session(i).id).unwrap().state.clone())
                .collect()
        });
        executor.block_on(async {
            for state in &mut states {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while *state.borrow() != PreviewState::Live {
                        state.changed().await.unwrap();
                    }
                })
                .await
                .unwrap();
            }
        });
        surfaces.read_with(cx, |s, _| {
            for i in 1..4 {
                let preview = s.live_previews.get(&session(i).id).unwrap();
                assert_eq!(
                    (preview.element.grid_cols(), preview.element.grid_rows()),
                    (80, 24)
                );
            }
        });
        assert_eq!(opened.load(Ordering::SeqCst), 3);
        assert_eq!(
            store.read().unwrap().selected_session_id(),
            Some(&session(0).id)
        );
        assert_eq!(
            (original.read().unwrap().cols, original.read().unwrap().rows),
            (100, 40)
        );
        // Moving two cards offscreen drops their sockets before dismissal.
        events
            .lock()
            .unwrap()
            .push(("resize begins".into(), began.elapsed().as_micros()));
        cx.simulate_resize(size(px(250.0), px(700.0)));
        assert!(cx.debug_bounds("TAB_PEEK_CARD_1").is_some());
        assert!(cx.debug_bounds("TAB_PEEK_CARD_2").is_none());
        surfaces.read_with(cx, |s, _| {
            assert!(s.live_previews.get(&session(1).id).is_some());
            assert!(s.live_previews.get(&session(2).id).is_none());
            assert!(s.live_previews.get(&session(3).id).is_none());
        });
        events.lock().unwrap().push((
            "slots retained running-01; dropped running-02/running-03".into(),
            began.elapsed().as_micros(),
        ));
        executor.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while closed.load(Ordering::SeqCst) != 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{error}: opened {}, closed {}; events (microseconds): {:?}",
                    opened.load(Ordering::SeqCst),
                    closed.load(Ordering::SeqCst),
                    events.lock().unwrap()
                )
            });
        });
        cx.simulate_keystrokes("escape");
        assert!(!surfaces.read_with(cx, |s, _| s.peek.visible()));
        executor.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while closed.load(Ordering::SeqCst) != 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
        eprintln!(
            "preview lifecycle (microseconds): {:?}",
            events.lock().unwrap()
        );
        server.abort();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes deterministic tab peek screenshots"]
    fn render_tab_peek_screenshot() {
        use diri_term::buffer::GridBuffer;
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT");
        let distance = std::env::var("DIRI_PEEK_DISTANCE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(380.0);
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let window=cx.open_window(size(px(1100.0),px(700.0)),|_,cx| {
            let runtime=Arc::new(StoreRuntime::inert());
            { let mut store=runtime.store.write().unwrap();
              // `DIRI_VISUAL_PROJECTS=1` spreads the cards over four projects.
              let spread=std::env::var_os("DIRI_VISUAL_PROJECTS").is_some();
              store.hydrate(SessionListResult{sessions:(0..4).map(|i| { let mut s=session(i); if spread { s.project_id=diri_proto::ProjectId::new(["preview-api","preview-web","preview-ios","preview-api"][i]); } s }).collect(),projects:vec![]});
              store.select(session(0).id);
              if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() { store.update_preferences(|p|p.terminal_theme="dirijor-light".into()).unwrap(); }
              if let Ok(theme)=std::env::var("DIRI_VISUAL_THEME") { store.update_preferences(|p|p.terminal_theme=theme).unwrap(); }
            }
            let surfaces=cx.new(|cx| {
                let mut surfaces=SessionSurfaces::new(runtime,None,cx);
                for i in 0..3 {
                    let mut buffer=GridBuffer::new(80,24);
                    let sample=format!("diri / project {}\n\n$ cargo test --workspace\nrunning 4 tests\n\ntest reconnect_preserves_identity ... ok\ntest no_preview_resize ... ok\ntest no_controller_change ... ok\ntest restores_focus ... ok\n\ntest result: ok. 4 passed; 0 failed\n\n$ ",i+1);
                    for (y,line) in sample.lines().enumerate() {for (x,ch) in line.chars().enumerate().take(80) {buffer.cells[y*80+x].scalar=ch as u32;}}
                    surfaces.set_resident_buffer(session(i).id,Arc::new(RwLock::new(buffer)));
                }
                surfaces.tab_gesture(crate::tab_peek::GestureFrame::Tracking(distance),cx);
                surfaces
            });
            cx.new(|_|OverviewHarness{surfaces,background_scrolls:Arc::new(AtomicUsize::new(0))})
        }).unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }

    #[test]
    fn previews_keep_recent_output_and_blank_lines_without_unbounded_text() {
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let excerpt = screen_excerpt(&format!("{text}\n\n  "));
        assert_eq!(excerpt.len(), 12);
        assert_eq!(excerpt.first().unwrap(), "line 8");
        assert_eq!(excerpt.last().unwrap(), "line 19");
        assert_eq!(screen_excerpt("hello\n\n> "), vec!["hello", "", "> "]);
        assert!(screen_excerpt(" \n\n").is_empty());
        assert_eq!(screen_excerpt(&"界".repeat(500))[0].chars().count(), 160);
    }

    #[test]
    fn gallery_columns_follow_available_width() {
        assert_eq!(overview_columns(680.0), 1);
        assert_eq!(overview_columns(900.0), 2);
        assert_eq!(overview_columns(1100.0), 3);
        assert_eq!(overview_columns(1800.0), 5);
        assert_eq!(overview_columns(0.0), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes deterministic overview screenshots"]
    fn render_overview_screenshot() {
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT");
        let light = std::env::var_os("DIRI_VISUAL_LIGHT").is_some();
        let width = std::env::var("DIRI_VISUAL_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1200.0);
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let window = cx.open_window(size(px(width), px(820.0)), |_, cx| {
            let runtime = Arc::new(StoreRuntime::inert());
            let titles = ["Make session switching feel effortless", "Review authentication changes", "Fix the flaky reconnect test", "Update the onboarding flow", "Local development server", "Investigate slow workspace startup"];
            let mut sessions: Vec<_> = titles.iter().enumerate().map(|(i, title)| {
                let mut s = session(i);
                s.title = (*title).into();
                s.cwd = if i % 2 == 0 { "/work/diri" } else { "/work/anara" }.into();
                s.kind = if i % 2 == 0 { ProtoAgentKind::CODEX } else { ProtoAgentKind::CLAUDE_CODE };
                s
            }).collect();
            sessions[1].status = SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission);
            {
                let mut store = runtime.store.write().unwrap();
                store.hydrate(SessionListResult { sessions, projects: vec![diri_proto::Project { id: ProjectId::new("overview"), root: "/work".into(), name: "Workspace".into(), pinned_order: None, host: None }] });
                store.update_preferences(|p| p.terminal_theme = if light { "dirijor-light" } else { "dirijor-dark" }.into()).unwrap();
                store.toggle_overview();
                match std::env::var("DIRI_VISUAL_STATE").as_deref() {
                    Ok("empty") => { store.append_overview_query("missing-session"); }
                    Ok("list") => store.set_overview_mode(OverviewMode::List),
                    Ok("selected") => { store.toggle_overview_selection(session(0).id); }
                    _ => {}
                }
            }
            let surfaces = cx.new(|cx| {
                let mut view = SessionSurfaces::new(runtime, None, cx);
                let samples = [
                    "› Improve the session overview\n\n• Read session_surfaces.rs\n• Read switcher.rs\n\n  The gallery now follows the window width.\n  Checking keyboard navigation and previews.\n\n  cargo test -p diri-app\n  test result: ok. 42 passed\n\n› ",
                    "╭─ Claude Code ──────────────────────╮\n│ /work/anara                       │\n╰───────────────────────────────────╯\n\n  I found two issues in the auth callback.\n  The redirect needs to preserve state.\n\n  Allow editing src/auth/callback.ts?\n\n  ❯ 1. Yes\n    2. No\n",
                    "$ cargo test reconnect -- --nocapture\n\nrunning 3 tests\ntest preserves_session_identity ... ok\ntest restores_terminal_snapshot ... ok\ntest rejects_stale_controller ... ok\n\ntest result: ok. 3 passed; 0 failed\n\n$ git diff --stat\n src/reconnect.rs | 12 +++++---\n$ ",
                ];
                for i in 0..6 { view.screens.insert(session(i).id, ScreenPreview::Ready(screen_excerpt(samples[i % samples.len()]))); }
                view
            });
            cx.new(|_| OverviewHarness { surfaces, background_scrolls: Arc::new(AtomicUsize::new(0)) })
        }).unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }

    fn calm_fleet(count: usize) -> SessionListResult {
        SessionListResult {
            sessions: (0..count)
                .map(|i| {
                    let mut s = session(i);
                    s.project_id = ProjectId::new(if i < 5 { "diri" } else { "anara" });
                    s
                })
                .collect(),
            projects: ["diri", "anara"]
                .into_iter()
                .map(|name| diri_proto::Project {
                    id: ProjectId::new(name),
                    root: format!("/work/{name}"),
                    name: name.into(),
                    pinned_order: None,
                    host: None,
                })
                .collect(),
        }
    }

    /// The zoom's first frame flies toward the layout's estimate of a card;
    /// every later frame toward where the card painted. The two must agree,
    /// and revealing a card must scroll it on screen.
    #[gpui::test]
    fn calm_card_estimates_match_painted_cards_and_reveal_scrolls(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(calm_fleet(14));
        let (view, cx) = cx.add_window_view(move |_, cx| OverviewHarness {
            surfaces: cx.new(|cx| {
                let mut surfaces = SessionSurfaces::new(runtime, None, cx);
                surfaces.overview_variant = OverviewVariant::Windows;
                surfaces.store.write().unwrap().toggle_overview();
                surfaces
            }),
            background_scrolls: Arc::new(AtomicUsize::new(0)),
        });
        cx.simulate_resize(size(px(1300.0), px(800.0)));
        let surfaces = view.read_with(cx, |h, _| h.surfaces.clone());
        let check = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let surfaces = surfaces.read(cx);
                let painted = surfaces.zoom.cards.borrow().clone();
                assert_eq!(painted.len(), 14);
                for (id, card) in painted {
                    let estimate = surfaces.calm_card_estimate(&id, window).unwrap();
                    for (a, b) in [
                        (card.x, estimate.x),
                        (card.y, estimate.y),
                        (card.width, estimate.width),
                        (card.height, estimate.height),
                    ] {
                        assert!(
                            (a - b).abs() < 0.51,
                            "{id:?}: painted {card:?}, estimate {estimate:?}"
                        );
                    }
                }
            });
        };
        check(cx);
        let last = session(13).id;
        cx.update(|window, cx| surfaces.read(cx).reveal_calm_card(&last, window, true));
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        check(cx);
        cx.update(|window, cx| {
            let card = surfaces.read(cx).zoom.cards.borrow()[&last];
            let height = f32::from(window.viewport_size().height);
            assert!(card.y >= 42.0 && card.y + card.height <= height, "{card:?}");
        });
    }

    /// The workbench a zoom flies out of: a pane (title bar over the grid,
    /// padded the way `TerminalPane` pads it) under the overview surfaces.
    /// Only the macOS pixel test builds it.
    #[cfg(target_os = "macos")]
    struct ZoomHarness {
        surfaces: Entity<SessionSurfaces>,
        page: TerminalElement,
    }

    #[cfg(target_os = "macos")]
    impl Render for ZoomHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let colors = self.surfaces.read(cx).colors();
            let theme = {
                let surfaces = self.surfaces.read(cx);
                let store = surfaces.store.read().unwrap();
                crate::app_theme::terminal_theme_in(&store)
            };
            let pane = div()
                .absolute()
                .left(px(240.0))
                .top(px(0.0))
                .w(px(1200.0))
                .h(px(900.0))
                .flex()
                .flex_col()
                .bg(theme.background)
                .child(
                    div()
                        .flex_none()
                        .h(px(diri_ui::Metrics::TITLE_BAR))
                        .flex()
                        .items_center()
                        .px(px(14.0))
                        .border_b_1()
                        .border_color(colors.primary.alpha(0.08))
                        .text_size(px(13.0))
                        .text_color(colors.secondary)
                        .child("Overflowing session 01 — feature/session-01"),
                )
                .child(
                    div().flex_1().pt(px(2.0)).pb(px(10.0)).px(px(12.0)).child(
                        self.page
                            .clone()
                            .font(crate::fonts::terminal_font("", true))
                            .font_size(px(13.0))
                            .theme(theme),
                    ),
                );
            div().size_full().bg(colors.background).child(pane).child(
                self.surfaces
                    .clone()
                    .cached(StyleRefinement::default().absolute().inset_0()),
            )
        }
    }

    /// The zoom flies a picture of the page and lands as the card itself:
    /// the flight never re-lays the terminal out, and at progress 1 it paints
    /// the calm overview pixel for pixel, so it ends without a swap. Set
    /// `DIRI_VISUAL_OUTPUT_DIR` to keep the frame strip.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "headless Metal pixel comparison; run explicitly on macOS"]
    fn calm_zoom_lands_as_the_card_pixel_for_pixel() {
        use crate::overview_fixture as fx;
        use crate::overview_zoom::Landing;
        use gpui::HeadlessAppContext;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let screens = [
            fx::codex_done(),
            fx::htop(),
            fx::git_graph(),
            fx::vim_review(),
        ];
        let window = cx
            .open_window(size(px(1440.0), px(900.0)), |_, cx| {
                let runtime = Arc::new(StoreRuntime::inert());
                let mut fleet = calm_fleet(screens.len());
                for record in &mut fleet.sessions {
                    record.status = SessionStatus::Idle;
                    record.last_turn_completed_at = Some(DateMillis(1.0));
                    record.last_seen_at = Some(DateMillis(2.0));
                }
                runtime.store.write().unwrap().hydrate(fleet);
                let buffers: Vec<_> = screens
                    .iter()
                    .map(|screen| Arc::new(RwLock::new(fx::screen(screen))))
                    .collect();
                let page = TerminalElement::new(buffers[1].clone()).focused(false);
                let surfaces = cx.new(|cx| {
                    let mut view = SessionSurfaces::new(runtime, None, cx);
                    view.overview_variant = OverviewVariant::Windows;
                    for (i, buffer) in buffers.iter().enumerate() {
                        view.set_resident_buffer(session(i).id, buffer.clone());
                    }
                    view.set_page_region(
                        crate::terminal_pane::TerminalViewport {
                            x: 240.0,
                            y: 0.0,
                            width: 1200.0,
                            height: 900.0,
                        },
                        42.0,
                    );
                    view.store.write().unwrap().select(session(1).id);
                    view
                });
                cx.new(|_| ZoomHarness { surfaces, page })
            })
            .unwrap();
        let surfaces = cx
            .update_window(window.into(), |root, _, cx| {
                root.downcast::<ZoomHarness>()
                    .unwrap()
                    .read(cx)
                    .surfaces
                    .clone()
            })
            .unwrap();
        let out = std::env::var("DIRI_VISUAL_OUTPUT_DIR")
            .ok()
            .map(std::path::PathBuf::from);
        let save = |image: &image::RgbaImage, name: &str| {
            if let Some(dir) = &out {
                image
                    .save(dir.join(format!("calm-zoom-{name}.png")))
                    .unwrap();
            }
        };
        // The page, then ⇧⌘O: the first overview frame takes its picture.
        cx.run_until_parked();
        let page = cx.capture_screenshot(window.into()).unwrap();
        save(&page, "0-page");
        cx.update(|cx| {
            surfaces.update(cx, |view, cx| {
                view.store.write().unwrap().toggle_overview();
                cx.notify();
            })
        });
        cx.run_until_parked();
        let cost = cx.update(|cx| {
            let view = surfaces.read(cx);
            assert!(view.zoom.zoom.is_flying(), "⇧⌘O zooms the page in");
            let snapshot = view.zoom.snapshot.as_ref().expect("the page was captured");
            assert_eq!(snapshot.source, Landing::Page);
            snapshot.cost
        });
        // Re-capture a few times for a steadier number than the first call.
        let repeat = cx
            .update_window(window.into(), |_, window, _| {
                let bounds =
                    gpui::Bounds::new(point(px(240.0), px(0.0)), size(px(1200.0), px(662.0)));
                let started = std::time::Instant::now();
                for _ in 0..10 {
                    window.capture_region(bounds, 4).unwrap();
                }
                started.elapsed() / 10
            })
            .unwrap();
        eprintln!("page snapshot: first capture {cost:?}, steady {repeat:?}");
        // Land, then take the resting grid.
        cx.update(|cx| {
            surfaces.update(cx, |view, cx| {
                view.zoom.zoom.reset(Landing::Overview);
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.capture_screenshot(window.into()).unwrap();
        let resting = cx.capture_screenshot(window.into()).unwrap();
        save(&resting, "resting");
        let card = cx.update(|cx| surfaces.read(cx).zoom.cards.borrow()[&session(1).id]);
        // The strip along the way: the page picture, scaled about its grid.
        let hold = |cx: &mut HeadlessAppContext, progress: f32| {
            cx.update(|cx| {
                surfaces.update(cx, |view, cx| {
                    let now = cx.background_executor().now();
                    view.zoom.session = Some(session(1).id);
                    view.zoom.crossfade = false;
                    view.zoom.grip = None;
                    let page = view.zoom.page_cache.clone();
                    view.zoom.set_snapshot(page);
                    view.zoom.zoom.reset(Landing::Page);
                    view.zoom.zoom.begin(Landing::Page, now);
                    let scale = 1.0 - progress * (1.0 - card.width / 1200.0);
                    view.zoom
                        .zoom
                        .pinch(scale - 1.0, now + std::time::Duration::from_millis(16));
                    assert!((view.zoom.zoom.progress() - progress).abs() < 1e-3);
                    assert!(view.zoom_painting());
                    cx.notify();
                })
            });
            cx.run_until_parked();
            cx.capture_screenshot(window.into()).unwrap()
        };
        for progress in [0.25_f32, 0.5, 0.75, 0.85, 0.9, 0.95] {
            let frame = hold(&mut cx, progress);
            save(&frame, &format!("{:03}", (progress * 100.0).round() as u32));
        }
        let landed = hold(&mut cx, 1.0);
        save(&landed, "100");
        // Pinching a card open with no current page picture grows the card's
        // own picture and hands over to the live pane near the end.
        cx.update(|cx| {
            surfaces.update(cx, |view, cx| {
                view.zoom.set_page_cache(None);
                view.zoom.zoom.reset(Landing::Overview);
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.update_window(window.into(), |root, window, cx| {
            let harness = root.downcast::<ZoomHarness>().unwrap();
            harness.read(cx).surfaces.clone().update(cx, |view, _| {
                assert!(view.take_return_snapshot(&session(1).id, card, window));
                let snapshot = view.zoom.snapshot.as_ref().unwrap();
                assert_eq!(snapshot.source, Landing::Overview);
            });
        })
        .unwrap();
        for progress in [0.5_f32, 0.08] {
            cx.update(|cx| {
                surfaces.update(cx, |view, cx| {
                    let now = cx.background_executor().now();
                    view.zoom.session = Some(session(1).id);
                    view.zoom.grip = None;
                    view.zoom.zoom.reset(Landing::Overview);
                    view.zoom.zoom.begin(Landing::Overview, now);
                    let scale = 1.0 - progress * (1.0 - card.width / 1200.0);
                    let from = card.width / 1200.0;
                    view.zoom.zoom.pinch(
                        scale / from - 1.0,
                        now + std::time::Duration::from_millis(16),
                    );
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let image = cx.capture_screenshot(window.into()).unwrap();
            save(
                &image,
                &format!("out-{:03}", (progress * 100.0).round() as u32),
            );
        }
        assert_eq!(resting.dimensions(), landed.dimensions());
        let scale = resting.width() as f32 / 1440.0;
        let inside = |x: u32, y: u32| {
            let (x, y) = (x as f32 / scale, y as f32 / scale);
            x >= card.x && x < card.x + card.width && y >= card.y && y < card.y + card.height
        };
        let mut card_pixels = 0;
        for (x, y, a) in resting.enumerate_pixels() {
            let b = landed.get_pixel(x, y);
            if inside(x, y) {
                card_pixels += 1;
                assert_eq!(a, b, "the landed card differs at ({x}, {y})");
            } else {
                // The card's soft shadow overlaps its neighbour's; above the
                // grid it blends in the other order, a level at most apart.
                let apart =
                    a.0.iter()
                        .zip(b.0.iter())
                        .map(|(a, b)| a.abs_diff(*b))
                        .max();
                assert!(
                    apart <= Some(1),
                    "the landed zoom moved pixels at ({x}, {y})"
                );
            }
        }
        assert!(card_pixels > 10_000, "the card was measured: {card:?}");
    }

    /// Phase-1 design exploration. `DIRI_VISUAL_VARIANT` = current | a | b | c.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes calm overview design screenshots"]
    fn render_calm_overview_screenshot() {
        use crate::overview_fixture as fx;
        use gpui::HeadlessAppContext;
        let output = std::env::var("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT");
        let light = std::env::var_os("DIRI_VISUAL_LIGHT").is_some();
        fx::LIGHT.store(light, std::sync::atomic::Ordering::Relaxed);
        let variant =
            OverviewVariant::from_env(&std::env::var("DIRI_VISUAL_VARIANT").unwrap_or_default());
        let dim = |key: &str, default: f32| {
            std::env::var(key)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        };
        let (width, height) = (
            dim("DIRI_VISUAL_WIDTH", 1440.0),
            dim("DIRI_VISUAL_HEIGHT", 900.0),
        );
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let fleet: Vec<(&str, &str, ProtoAgentKind, &str, String)> = vec![
            (
                "diri",
                "Make the session overview calm",
                ProtoAgentKind::CLAUDE_CODE,
                "working",
                fx::claude_working(),
            ),
            (
                "anara",
                "Fix the login redirect loop",
                ProtoAgentKind::CLAUDE_CODE,
                "input",
                fx::claude_permission(),
            ),
            (
                "diri",
                "Fix the flaky reconnect test",
                ProtoAgentKind::CODEX,
                "done",
                fx::codex_done(),
            ),
            (
                "anara",
                "Web dev server",
                ProtoAgentKind::SHELL,
                "working",
                fx::vite_server(),
            ),
            (
                "diri",
                "Release 0.8.9",
                ProtoAgentKind::SHELL,
                "working",
                fx::cargo_release(),
            ),
            (
                "diri",
                "System monitor",
                ProtoAgentKind::SHELL,
                "idle",
                fx::htop(),
            ),
            (
                "anara",
                "Onboarding copy pass",
                ProtoAgentKind::GEMINI,
                "asleep",
                fx::gemini_asleep(),
            ),
            (
                "diri",
                "Tidy branch history",
                ProtoAgentKind::CODEX,
                "idle",
                fx::git_graph(),
            ),
            (
                "diri",
                "overview_layout.rs",
                ProtoAgentKind::SHELL,
                "idle",
                fx::vim_review(),
            ),
        ];
        let window = cx
            .open_window(size(px(width), px(height)), |_, cx| {
                let runtime = Arc::new(StoreRuntime::inert());
                let sessions: Vec<_> = fleet
                    .iter()
                    .enumerate()
                    .map(|(i, (project, title, kind, status, _))| {
                        let mut s = session(i);
                        s.title = (*title).into();
                        s.kind = kind.clone();
                        s.project_id = ProjectId::new(*project);
                        s.cwd = format!("/work/{project}");
                        s.status = match *status {
                            "working" => SessionStatus::Working,
                            "input" => {
                                SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Permission)
                            }
                            _ => SessionStatus::Idle,
                        };
                        if *status == "done" {
                            s.last_turn_completed_at = Some(DateMillis(10.0));
                        }
                        if *status == "idle" {
                            s.last_turn_completed_at = Some(DateMillis(1.0));
                            s.last_seen_at = Some(DateMillis(2.0));
                        }
                        if *status == "asleep" {
                            s.hibernation = Some(diri_proto::HibernationInfo {
                                since: DateMillis(0.0),
                                reason: diri_proto::HibernationReason::Idle,
                                tree_pids: vec![],
                                tree_start_times: None,
                            });
                        }
                        s
                    })
                    .collect();
                {
                    let mut store = runtime.store.write().unwrap();
                    let projects = ["diri", "anara"]
                        .into_iter()
                        .map(|name| diri_proto::Project {
                            id: ProjectId::new(name),
                            root: format!("/work/{name}"),
                            name: name.into(),
                            pinned_order: None,
                            host: None,
                        })
                        .collect();
                    store.hydrate(SessionListResult { sessions, projects });
                    store
                        .update_preferences(|p| {
                            p.terminal_theme = if light {
                                "dirijor-light"
                            } else {
                                "dirijor-dark"
                            }
                            .into();
                            p.window_material = crate::store::WindowMaterial::Opaque;
                        })
                        .unwrap();
                }
                let surfaces = cx.new(|cx| {
                    let mut view = SessionSurfaces::new(runtime, None, cx);
                    view.overview_variant = variant;
                    {
                        let mut store = view.store.write().unwrap();
                        store.select(session(2).id);
                        store.toggle_overview();
                        if let Ok(query) = std::env::var("DIRI_VISUAL_QUERY") {
                            store.append_overview_query(&query);
                        }
                    }
                    for (i, (_, _, kind, _, screen)) in fleet.iter().enumerate() {
                        let buffer = if *kind == ProtoAgentKind::SHELL {
                            fx::screen(screen)
                        } else {
                            fx::screen_bottom(screen)
                        };
                        let text: String = buffer
                            .cells
                            .chunks(usize::from(buffer.cols))
                            .map(|row| {
                                row.iter()
                                    .map(|cell| {
                                        char::from_u32(cell.scalar)
                                            .filter(|c| *c != '\0')
                                            .unwrap_or(' ')
                                    })
                                    .collect::<String>()
                                    .trim_end()
                                    .to_owned()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        view.screens
                            .insert(session(i).id, ScreenPreview::Ready(screen_excerpt(&text)));
                        view.set_resident_buffer(session(i).id, Arc::new(RwLock::new(buffer)));
                    }
                    view
                });
                cx.new(|_| OverviewHarness {
                    surfaces,
                    background_scrolls: Arc::new(AtomicUsize::new(0)),
                })
            })
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }
}
