//! GPUI rendering and event routing for T13 navigation surfaces.
//!
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::store::{SessionStore, StoreRuntime};
use crate::switcher::{
    OverviewArrow, OverviewFilter, OverviewLane, OverviewMode, SwitcherKey, display_title,
};
use diri_proto::{
    AgentKind as ProtoAgentKind, AttentionLevel, RiskHint, SessionId, SessionRecord, SessionStatus,
};
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

pub struct SessionSurfaces {
    store: Arc<RwLock<SessionStore>>,
    focus_handle: FocusHandle,
    resident_previews: HashMap<SessionId, TerminalElement>,
    status_glyphs: HashMap<(SessionId, u16, diri_ui::AgentKind), Entity<StatusGlyph>>,
    overview_board_scroll: ScrollHandle,
    overview_lane_scrolls: HashMap<OverviewLane, ScrollHandle>,
    overview_list_scroll: ScrollHandle,
    /// This view is `.cached()` in RootView, so ambient window redraws no
    /// longer reach it: store changes must notify it directly.
    _store_changes: Task<()>,
}

impl SessionSurfaces {
    pub fn new(runtime: Arc<StoreRuntime>, cx: &mut Context<Self>) -> Self {
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
            store: Arc::clone(&runtime.store),
            focus_handle: cx.focus_handle(),
            resident_previews: HashMap::new(),
            status_glyphs: HashMap::new(),
            overview_board_scroll: ScrollHandle::new(),
            overview_lane_scrolls: OverviewLane::ALL
                .into_iter()
                .map(|lane| (lane, ScrollHandle::new()))
                .collect(),
            overview_list_scroll: ScrollHandle::new(),
            _store_changes: store_changes,
        }
    }

    fn colors(&self) -> SemanticColors {
        let store = self.store.read().expect("session store lock poisoned");
        crate::app_theme::colors(&store.preferences().terminal_theme)
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

    pub(crate) fn open_overview(&mut self, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        if !store.overview_state().is_visible() {
            store.toggle_overview();
        }
        cx.notify();
    }
}

impl Render for SessionSurfaces {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (overview_visible, switcher_visible) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                store.overview_state().is_visible(),
                store.switcher_state().is_visible(),
            )
        };
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
        if overview_visible {
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
const OVERVIEW_LANE_WIDTH: f32 = 272.0;

impl SessionSurfaces {
    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if event.keystroke.key == "o" && modifiers.platform && modifiers.shift {
            store.toggle_overview();
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if !store.overview_state().is_visible() {
            if event.keystroke.key == "escape" && !store.sidebar_selection().is_empty() {
                // Match Swift: clear Finder-style sidebar gathering, but do not
                // swallow Esc because the focused terminal still needs it.
                store.clear_sidebar_selection();
                cx.notify();
            }
            return;
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
            cx.stop_propagation();
            cx.notify();
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
            .child(self.mode_button(OverviewMode::Board, state.mode(), "Board", colors, cx))
            .child(self.mode_button(OverviewMode::List, state.mode(), "List", colors, cx));

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .h(px(50.0))
            .px(px(20.0))
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.primary)
                    .child("All Sessions"),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.tertiary)
                    .child(summary),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child("⌘ click to select"),
            )
            .child(mode_selector)
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
            .px(px(20.0))
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
        if !state.query().is_empty() {
            filters = filters.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(5.0))
                    .h(px(22.0))
                    .px(px(9.0))
                    .rounded_full()
                    .bg(colors.primary.alpha(0.10))
                    .border_1()
                    .border_color(colors.primary.alpha(0.16))
                    .text_size(px(11.0))
                    .text_color(colors.primary)
                    .child(sf_symbol("magnifyingglass", 11.0, colors.secondary))
                    .child(state.query().to_owned())
                    .child(div().text_color(colors.tertiary).child("⌫")),
            );
        }

        let visible_count = state.visible_sessions(&sessions).count();
        let body = if visible_count == 0 {
            self.overview_empty_state(&state, colors)
        } else if state.mode() == OverviewMode::Board {
            self.overview_board(&sessions, &state, colors, window, cx)
        } else {
            self.overview_list(&sessions, &state, colors, window, cx)
        };

        let chrome = div()
            .flex()
            .flex_none()
            .flex_col()
            .bg(rgba(0x171921ff))
            .child(header)
            .child(filters)
            .child(HairlineDivider::horizontal(colors));

        let content = div()
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
            .child(chrome)
            .child(body)
            .when(!state.selection().is_empty(), |content| {
                content.child(self.bulk_close_bar(state.selection().len(), visible_count, cx))
            });

        div()
            .id("overview-scrim")
            .absolute()
            .inset_0()
            .size_full()
            .bg(rgba(0x000000ff))
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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .set_overview_mode(mode);
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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .set_overview_filter(filter);
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
            .into_any_element()
    }

    fn overview_board(
        &mut self,
        sessions: &[SessionRecord],
        state: &crate::switcher::SessionOverviewState,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lanes: Vec<_> = match state.filter() {
            OverviewFilter::All => OverviewLane::ALL.to_vec(),
            OverviewFilter::Lane(lane) => vec![lane],
        };
        let bottom_padding = if state.selection().is_empty() {
            18.0
        } else {
            82.0
        };
        let mut board = div()
            .id("overview-board")
            .debug_selector(|| "OVERVIEW_BOARD".into())
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .items_stretch()
            .gap(px(12.0))
            .px(px(20.0))
            .pt(px(14.0))
            .pb(px(bottom_padding))
            .track_scroll(&self.overview_board_scroll)
            .overflow_x_scroll();
        // Do not reinterpret a vertical wheel as horizontal board movement;
        // the lane under the pointer owns that axis.
        board.style().restrict_scroll_to_axis = Some(true);
        for lane in lanes {
            let lane_sessions = state.lane_sessions(lane, sessions);
            let count = lane_sessions.len();
            let lane_scroll = self
                .overview_lane_scrolls
                .get(&lane)
                .expect("every overview lane has a scroll handle")
                .clone();
            let mut cards = div()
                .id(SharedString::from(format!(
                    "overview-lane-{}",
                    lane.label()
                )))
                .debug_selector(|| format!("OVERVIEW_LANE_{}", lane.label().to_uppercase()))
                .flex()
                .flex_1()
                .flex_col()
                .min_h_0()
                .gap(px(10.0))
                .p(px(10.0))
                .pt(px(8.0))
                .track_scroll(&lane_scroll)
                .overflow_y_scroll();
            cards.style().restrict_scroll_to_axis = Some(true);
            for session in lane_sessions {
                cards = cards.child(self.overview_card(session, state, colors, window, cx));
            }
            board = board.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(OVERVIEW_LANE_WIDTH))
                    .min_h_0()
                    .overflow_hidden()
                    .rounded(px(Radius::PANEL))
                    .bg(colors.primary.alpha(0.026))
                    .border_1()
                    .border_color(colors.primary.alpha(0.065))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(6.0))
                            .h(px(36.0))
                            .px(px(11.0))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.secondary)
                            .child(lane.label())
                            .child(div().flex_1())
                            .child(
                                div()
                                    .min_w(px(20.0))
                                    .h(px(20.0))
                                    .px(px(6.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(colors.primary.alpha(0.055))
                                    .text_color(colors.tertiary)
                                    .child(count.to_string()),
                            ),
                    )
                    .child(HairlineDivider::horizontal(colors))
                    .child(cards),
            );
        }
        board.into_any_element()
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
        let preview = self.render_grid_or_logo(session, 34.0, 6.5, colors);

        let mut thumbnail = div()
            .relative()
            .w_full()
            .h(px(112.0))
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
                    .bg(rgba(0x20222bd9))
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
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(state_badge_color(session, colors))
                            .child(state_badge(session)),
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
            .h(px(68.0))
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
                    .w(px(88.0))
                    .h(px(50.0))
                    .rounded(px(Radius::BADGE))
                    .overflow_hidden()
                    .bg(colors.background)
                    .child(self.render_grid_or_logo(session, 24.0, 4.5, colors)),
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
                    .bg(rgba(0x2a2c35f5))
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

fn state_badge(session: &SessionRecord) -> String {
    if session.hibernation.is_some() {
        "asleep".to_owned()
    } else if matches!(session.status, SessionStatus::Exited(_)) {
        "ended".to_owned()
    } else if let Some(bytes) = session
        .memory_bytes
        .filter(|bytes| *bytes > 2 * 1_073_741_824)
    {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else {
        session
            .git_branch
            .as_deref()
            .map(clamp_branch)
            .unwrap_or_default()
    }
}

fn state_badge_color(session: &SessionRecord, colors: SemanticColors) -> gpui::Rgba {
    if session
        .hibernation
        .as_ref()
        .is_some_and(|info| info.reason == diri_proto::HibernationReason::MemoryPressure)
        || session
            .memory_bytes
            .is_some_and(|bytes| bytes > 6 * 1_073_741_824)
    {
        Ink::ATTENTION
    } else {
        colors.tertiary
    }
}

fn clamp_branch(branch: &str) -> String {
    let characters: Vec<_> = branch.chars().collect();
    if characters.len() <= 18 {
        branch.to_owned()
    } else {
        format!(
            "{}…{}",
            characters[..8].iter().collect::<String>(),
            characters[characters.len() - 8..]
                .iter()
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use diri_proto::{
        AgentKind as ProtoAgentKind, DateMillis, Project, ProjectId, Resumability,
        SessionListResult, TitleSource,
    };
    use gpui::{ScrollDelta, ScrollWheelEvent, StyleRefinement, TestAppContext, size};

    struct OverviewHarness {
        surfaces: Entity<SessionSurfaces>,
        background_scrolls: Arc<AtomicUsize>,
    }

    impl Render for OverviewHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let background_scrolls = Arc::clone(&self.background_scrolls);
            div()
                .size_full()
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
            id: SessionId::new(format!("running-{index:02}")),
            kind: ProtoAgentKind::CODEX,
            cwd: "/work/overview".into(),
            project_id: ProjectId::new("overview"),
            worktree_path: None,
            git_branch: Some(format!("feature/session-{index:02}")),
            title: format!("Overflowing session {index:02}"),
            title_source: TitleSource::AgentProvided,
            agent_session_id: None,
            transcript_path: None,
            status: SessionStatus::Working,
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::Live,
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
    fn overflowing_overview_lane_scrolls_without_reaching_the_background(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .hydrate(SessionListResult {
                sessions: (0..18).map(session).collect(),
                projects: vec![Project {
                    id: ProjectId::new("overview"),
                    root: "/work/overview".into(),
                    name: "Overview".into(),
                    pinned_order: None,
                }],
            });
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .toggle_overview();

        let background_scrolls = Arc::new(AtomicUsize::new(0));
        let background_probe = Arc::clone(&background_scrolls);
        let (view, cx) = cx.add_window_view(move |_window, cx| OverviewHarness {
            surfaces: cx.new(|cx| SessionSurfaces::new(runtime, cx)),
            background_scrolls: background_probe,
        });
        cx.simulate_resize(size(px(1100.0), px(700.0)));

        let surfaces = view.read_with(cx, |harness, _| harness.surfaces.clone());
        let lane_bounds = cx
            .debug_bounds("OVERVIEW_LANE_RUNNING")
            .expect("running lane should render");
        let board_bounds = cx
            .debug_bounds("OVERVIEW_BOARD")
            .expect("overview board should render");
        let content_bounds = cx
            .debug_bounds("OVERVIEW_CONTENT")
            .expect("overview content should render");
        assert_eq!(
            content_bounds.size,
            size(px(1100.0), px(700.0)),
            "the opaque overview surface must cover the full cached viewport"
        );
        assert!(
            lane_bounds.size.height > px(300.0),
            "the lane viewport must receive the available window height"
        );
        let max_offset = surfaces.read_with(cx, |surfaces, _| {
            surfaces
                .overview_lane_scrolls
                .get(&OverviewLane::Running)
                .expect("running scroll handle")
                .max_offset()
        });
        assert!(
            max_offset.y > px(0.0),
            "overflowing lane must have a bounded, scrollable viewport"
        );
        assert!(
            surfaces
                .read_with(cx, |surfaces, _| surfaces
                    .overview_board_scroll
                    .max_offset())
                .x
                > px(0.0),
            "the five-lane board should expose horizontal overflow"
        );

        cx.simulate_event(ScrollWheelEvent {
            position: lane_bounds.center(),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-80.0))),
            ..ScrollWheelEvent::default()
        });

        let offset = surfaces.read_with(cx, |surfaces, _| {
            surfaces
                .overview_lane_scrolls
                .get(&OverviewLane::Running)
                .expect("running scroll handle")
                .offset()
        });
        assert!(
            offset.y < px(0.0),
            "wheel input should move the overview lane (content: {content_bounds:?}, board: {board_bounds:?}, lane: {lane_bounds:?}, offset: {offset:?}, max: {max_offset:?}, background events: {})",
            background_scrolls.load(Ordering::Relaxed),
        );
        assert_eq!(
            surfaces
                .read_with(cx, |surfaces, _| surfaces.overview_board_scroll.offset())
                .x,
            px(0.0),
            "vertical lane scrolling must not shift the board sideways"
        );

        cx.simulate_event(ScrollWheelEvent {
            position: lane_bounds.center(),
            delta: ScrollDelta::Pixels(point(px(-80.0), px(0.0))),
            ..ScrollWheelEvent::default()
        });
        assert!(
            surfaces
                .read_with(cx, |surfaces, _| surfaces.overview_board_scroll.offset())
                .x
                < px(0.0),
            "horizontal trackpad input should move the lane board"
        );
        assert_eq!(
            background_scrolls.load(Ordering::Relaxed),
            0,
            "overview wheel input must not leak to the terminal behind it"
        );
    }
}
