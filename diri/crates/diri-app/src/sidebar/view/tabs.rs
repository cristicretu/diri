use super::*;
use crate::store::TabOrientation;
use crate::tab_navigation::{TAB_STRIP_HEIGHT, selected_project_tabs};

const TAB_WIDTH: f32 = 164.0;
const TAB_GAP: f32 = 4.0;

impl Sidebar {
    pub(super) fn navigation_sessions(&self, store: &mut SessionStore) -> Vec<Arc<SessionRecord>> {
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
            .h_full()
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
                    .bg(if active {
                        colors.primary.alpha(0.10)
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .hover(move |row| {
                        row.bg(colors.primary.alpha(if active { 0.13 } else { 0.06 }))
                    })
                    .child(sf_symbol(
                        crate::agent_catalog::system_image(&session.kind),
                        12.0,
                        colors.secondary,
                    ))
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
            .gap(px(8.0))
            .pl(px(if cfg!(target_os = "macos") && !self.ui.visible {
                84.0
            } else {
                10.0
            }))
            .pr(px(10.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.07))
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            .child(
                div()
                    .w(px(120.0))
                    .flex_none()
                    .child(self.workspace_control(colors, cx)),
            )
            .child(
                div()
                    .id("horizontal-tab-project")
                    .debug_selector(|| "horizontal-tab-project".into())
                    .role(Role::Button)
                    .aria_label(format!("Browse projects, current project {}", tabs.label))
                    .h(px(30.0))
                    .max_w(px(140.0))
                    .px(px(8.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(SIDEBAR_ROW_RADIUS))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(Typo::META.size))
                            .child(tabs.label),
                    )
                    .child(sf_symbol("chevron.down", 8.0, colors.tertiary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.peek(window, cx);
                        this.focus_handle.focus(window, cx);
                    })),
            )
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
