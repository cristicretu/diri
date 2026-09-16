use super::*;
use crate::store::TabOrientation;
use crate::tab_navigation::{TAB_STRIP_HEIGHT, selected_project_tabs};

const TAB_WIDTH: f32 = 164.0;
const TAB_GAP: f32 = 4.0;

impl Sidebar {
    pub(super) fn agent_tab_icon(kind: &ProtoAgentKind, colors: SemanticColors) -> AnyElement {
        match ui_agent_kind(kind).brand_mark() {
            Some(mark) => diri_ui::BrandMark::solid(mark, 16.0, colors.secondary)
                .inset(0.08)
                .into_any_element(),
            None => sf_symbol("terminal", 16.0, colors.secondary),
        }
    }

    pub(super) fn navigation_sessions(
        &self,
        store: &mut crate::store::WindowWrite<'_>,
    ) -> Vec<Arc<SessionRecord>> {
        if store.preferences().tab_orientation == TabOrientation::Horizontal {
            selected_project_tabs(store).sessions
        } else if !self.filter_query.text().trim().is_empty() {
            // Numeric shortcuts follow the displayed filtered rows, including
            // disclosed archives and the current project/recency grouping.
            self.focus_rows_for_store(store)
                .iter()
                .filter_map(|row| store.sessions().get(&row.id).cloned())
                .collect()
        } else {
            super::super::filter::filter_projection(
                store.sidebar_projection(),
                self.filter_query.text(),
            )
            .ordered_sessions
            .clone()
        }
    }

    pub fn tab_orientation(&self) -> TabOrientation {
        self.store
            .read()
            .expect("store")
            .preferences()
            .tab_orientation
    }

    pub fn horizontal_tabs_visible(&self) -> bool {
        let store = self.store.read().expect("store");
        store.preferences().tab_orientation == TabOrientation::Horizontal
            && store.preferences().horizontal_tabs_visible
    }

    pub fn toggle_horizontal_tabs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        self.store
            .write()
            .expect("store")
            .update_preferences(|prefs| {
                prefs.horizontal_tabs_visible = !prefs.horizontal_tabs_visible;
            })?;
        if !self.horizontal_tabs_visible() && self.project_picker_active() {
            if self.project_picker.new_agent {
                self.ui.popover = None;
                self.project_picker.new_agent = false;
            }
            self.dismiss_project_picker(window, cx);
        }
        cx.notify();
        Ok(())
    }

    /// Commit the presentation preference before changing the visible chrome.
    /// Selection and all terminal entities stay owned by their existing views.
    pub fn set_tab_orientation(
        &mut self,
        orientation: TabOrientation,
        cx: &mut Context<Self>,
    ) -> std::io::Result<()> {
        let visible = orientation == TabOrientation::Vertical;
        self.store
            .write()
            .expect("store")
            .update_preferences(|prefs| {
                prefs.tab_orientation = orientation;
                prefs.sidebar_visible = visible;
            })?;
        self.ui.visible = visible;
        self.last_tab_selection = None;
        self.peek_open = false;
        self.peek_close = None;
        self.dismiss_hover_card(cx);
        cx.emit(SidebarEvent::VisibilityChanged);
        cx.notify();
        Ok(())
    }

    pub(super) fn render_project_tab_rows(
        &mut self,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let (tabs, selected) = {
            let mut store = self.store.write().expect("store");
            let selected = store.selected_session_id().cloned();
            (selected_project_tabs(&mut store), selected)
        };
        let mut rows = div()
            .id(SharedString::from(format!(
                "horizontal-tab-list-{}",
                tabs.project.as_ref().map_or("empty", |id| id.0.as_str())
            )))
            .flex()
            .items_center()
            .gap(px(TAB_GAP))
            .flex_1()
            .min_w(px(0.0))
            .h(px(30.0))
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll);
        if self.last_tab_selection != selected || self.last_tab_available_width != available_width {
            if let Some(index) = tabs
                .sessions
                .iter()
                .position(|session| Some(&session.id) == selected.as_ref())
            {
                // Fixed-width tabs have a known content position before the
                // first layout. GPUI clamps this offset to the final viewport.
                self.tab_scroll
                    .set_offset(point(px(-(index as f32) * (TAB_WIDTH + TAB_GAP)), px(0.0)));
            }
            self.last_tab_selection = selected.clone();
            self.last_tab_available_width = available_width;
        }
        for session in tabs.sessions {
            let id = session.id.clone();
            let active = selected.as_ref() == Some(&id);
            let title = display_title(&session);
            let debug_id = id.0.clone();
            let close_id = id.clone();
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("horizontal-tab-{}", id.0)))
                    .debug_selector(move || format!("horizontal-tab-{}", debug_id))
                    .role(Role::Tab)
                    .aria_label(title.clone())
                    .aria_selected(active)
                    .flex_none()
                    .w(px(TAB_WIDTH))
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(SIDEBAR_ROW_RADIUS))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(colors.primary.alpha(0.0))
                    .glass_pill(colors, active)
                    .hover(move |row| {
                        if active {
                            row
                        } else {
                            row.bg(colors.primary.alpha(0.06))
                        }
                    })
                    .child(Self::agent_tab_icon(session.effective_kind(), colors))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(Typo::ROW.size))
                            .text_color(if active {
                                colors.primary
                            } else {
                                colors.secondary
                            })
                            .child(title),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "close-horizontal-tab-{}",
                                close_id.0
                            )))
                            .role(Role::Button)
                            .aria_label("Close session")
                            .size(px(18.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                            .child(sf_symbol("xmark", 8.0, colors.tertiary))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_sessions(vec![close_id.clone()], cx);
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.commit_rename();
                        this.store.write().expect("store").select(id.clone());
                        cx.emit(SidebarEvent::SessionActivated);
                        cx.notify();
                    })),
            );
        }
        rows.into_any_element()
    }

    pub fn render_horizontal_tabs(
        &mut self,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        if self.workspace_nav.active.is_some() {
            self.workspace_nav.available_width = available_width;
            return self.workspace_strip(colors, cx);
        }
        let rows = self.render_project_tab_rows(available_width, cx);
        div()
            .id("horizontal-tabs")
            .debug_selector(|| "horizontal-tabs".into())
            .role(Role::TabList)
            .aria_label("Project sessions")
            .flex_none()
            .h(px(TAB_STRIP_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .relative()
            .py(px(6.0))
            .gap(px(0.0))
            .pl(px(if cfg!(target_os = "macos") && !self.ui.visible {
                92.0
            } else {
                10.0
            }))
            .pr(px(10.0))
            .child(
                div().absolute().left(px(0.0)).right(px(0.0)).bottom(px(0.0))
                    .h(px(1.0)).bg(colors.primary.alpha(0.07)),
            )
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            .child(self.project_control(colors, cx))
            .child(rows)
            .child(
                div()
                    .id("horizontal-peek-tabs")
                    .debug_selector(|| "horizontal-peek-tabs".into())
                    .role(Role::Button)
                    .aria_label("Peek tabs")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("square.grid.2x2", 12.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::commands::ToggleTabPeek), cx)
                    }),
            )
            .child(
                div()
                    .id("horizontal-new-tab")
                    .role(Role::Button)
                    .aria_label("New session")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("plus", 12.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::commands::NewDefaultSession), cx)
                    }),
            )
            .into_any_element()
    }
}
