//! GPUI rendering and event routing for T13 navigation surfaces.
//!
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::store::{SessionStore, StoreRuntime};
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
    Animation, AnimationExt, AnyElement, BoxShadow, ClickEvent, Context, Entity, FocusHandle,
    FontWeight, KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, MouseButton, Render, ScrollHandle,
    SharedString, Task, Window, div, ease_out_quint, point, prelude::*, px, rgba,
};

#[path = "tab_peek_surface.rs"]
mod tab_peek_surface;

pub struct SessionSurfaces {
    peek: crate::tab_peek::TabPeek,
    peek_left: f32,
    peek_top: f32,
    peek_width: f32,
    peek_scroll: ScrollHandle,
    peek_previous_focus: Option<FocusHandle>,
    live_previews: crate::tab_preview::PreviewSet<crate::tab_preview::LivePreview>,
    store: Arc<RwLock<SessionStore>>,
    focus_handle: FocusHandle,
    resident_previews: HashMap<SessionId, TerminalElement>,
    status_glyphs: HashMap<(SessionId, u16, diri_ui::AgentKind), Entity<StatusGlyph>>,
    overview_grid_scroll: ScrollHandle,
    client: Arc<diri_client::DaemonClient>,
    tokio: Option<tokio::runtime::Handle>,
    screens: HashMap<SessionId, ScreenPreview>,
    screen_requests: HashMap<SessionId, ScreenRequest>,
    overview_was_visible: bool,
    overview_generation: usize,
    overview_list_scroll: ScrollHandle,
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
            peek_previous_focus: None,
            live_previews: Default::default(),
            store: Arc::clone(&runtime.store),
            focus_handle: cx.focus_handle(),
            resident_previews: HashMap::new(),
            status_glyphs: HashMap::new(),
            overview_grid_scroll: ScrollHandle::new(),
            client: Arc::clone(runtime.client()),
            tokio,
            screens: HashMap::new(),
            screen_requests: HashMap::new(),
            overview_was_visible: false,
            overview_generation: 0,
            overview_list_scroll: ScrollHandle::new(),
            _store_changes: store_changes,
        }
    }

    fn colors(&self) -> SemanticColors {
        let store = self.store.read().expect("session store lock poisoned");
        crate::app_theme::colors(store.theme_id())
    }

    /// T11 supplies the same resident buffer used by the mounted terminal. A
    /// separate painter/cache renders it into switcher and overview thumbnails
    /// without reading back the onscreen Metal layer.
    pub(crate) fn set_resident_buffer(&mut self, id: SessionId, buffer: SharedGridBuffer) {
        self.resident_previews
            .insert(id, TerminalElement::new(buffer).focused(false));
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
        self.dismiss_tab_peek();
        let mut store = self.store.write().expect("session store lock poisoned");
        store.cancel_switcher();
        store.dismiss_overview();
        drop(store);
        cx.notify();
    }
}

impl Render for SessionSurfaces {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_tab_peek_focus(window, cx);
        if !self.peek.visible() {
            self.live_previews.clear();
        }
        let (overview_visible, switcher_visible) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                store.overview_state().is_visible(),
                store.switcher_state().is_visible(),
            )
        };
        if !overview_visible && self.overview_was_visible {
            self.screen_requests.clear();
            self.screens.clear();
        }
        if overview_visible && !self.overview_was_visible {
            self.overview_generation = self.overview_generation.wrapping_add(1);
            self.overview_grid_scroll.scroll_to_item(0);
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
        if self.peek.visible() {
            root.inset_0().child(self.render_tab_peek(window, cx))
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

        store.set_overview_columns(overview_columns(f32::from(window.viewport_size().width)));
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
            .pt(px(42.0))
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

        let content = if cx.reduce_motion() {
            content.into_any_element()
        } else {
            content
                .with_animation(
                    ("overview-entry", self.overview_generation),
                    Animation::new(std::time::Duration::from_millis(120))
                        .with_easing(ease_out_quint()),
                    |view, value| view.opacity(value),
                )
                .into_any_element()
        };

        div()
            .id("overview-scrim")
            .absolute()
            .inset_0()
            .size_full()
            .bg(colors.background)
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

    fn request_screen(&mut self, id: SessionId, cx: &mut Context<Self>) {
        if !self.store.read().unwrap().overview_state().is_visible()
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
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                client.read_screen(&request_id),
            )
            .await
        });
        let abort = request.abort_handle();
        let task_id = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let preview = match request.await {
                Ok(Ok(Ok(screen))) => {
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
                if this.store.read().unwrap().overview_state().is_visible() {
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
            .bg(colors.background);
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
        if self.screens.contains_key(&id) || self.screen_requests.contains_key(&id) {
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
                .bg(colors.background)
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
        _ => AgentKind::Generic,
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
        SessionRecord {
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
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
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
        runtime.store.write().unwrap().toggle_overview();
        let background_scrolls = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&background_scrolls);
        let (view, cx) = cx.add_window_view(move |_, cx| OverviewHarness {
            surfaces: cx.new(|cx| SessionSurfaces::new(runtime, None, cx)),
            background_scrolls: probe,
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));
        let surfaces = view.read_with(cx, |h, _| h.surfaces.clone());
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

    #[gpui::test]
    fn overview_keeps_boundary_keys_away_from_the_terminal(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: vec![session(0)],
            projects: vec![],
        });
        runtime.store.write().unwrap().toggle_overview();
        let escaped = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&escaped);
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let surfaces = cx.new(|cx| SessionSurfaces::new(runtime, None, cx));
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
            sessions: (0..4).map(session).collect(),
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
        let store = runtime.store.clone();
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
        cx.simulate_keystrokes("right enter");
        assert_eq!(
            store.read().unwrap().selected_session_id(),
            Some(&session(1).id)
        );
        assert!(!surfaces.read_with(cx, |s, _| s.tab_peek_visible()));
        assert_eq!(
            (grid.read().unwrap().cols, grid.read().unwrap().rows),
            (100, 40)
        );
    }

    #[gpui::test]
    fn tab_peek_streams_inactive_cards_with_one_resident_and_drops_on_escape(
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
        let executor = tokio::runtime::Runtime::new().unwrap();
        let listener = {
            let _entered = executor.enter();
            tokio::net::UnixListener::bind(&socket).unwrap()
        };
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let server_opened = opened.clone();
        let server_closed = closed.clone();
        let server = executor.spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let opened = server_opened.clone();
                let closed = server_closed.clone();
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
                    };
                    stream
                        .get_mut()
                        .write_all(&FrameCodec::encode(&Frame::grid(&update).unwrap()).unwrap())
                        .await
                        .unwrap();
                    opened.fetch_add(1, Ordering::SeqCst);
                    let mut effects = Vec::new();
                    stream.read_to_end(&mut effects).await.unwrap();
                    assert!(effects.is_empty());
                    closed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        let runtime = Arc::new(StoreRuntime::inert());
        runtime.store.write().unwrap().hydrate(SessionListResult {
            sessions: (0..4).map(session).collect(),
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
              store.hydrate(SessionListResult{sessions:(0..4).map(session).collect(),projects:vec![]});
              store.select(session(0).id);
              if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() { store.update_preferences(|p|p.terminal_theme="dirijor-light".into()).unwrap(); }
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
}
