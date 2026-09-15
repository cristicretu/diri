use super::*;
use crate::tab_peek::{GestureFrame, card_rect, terminal_offset};
use gpui::App;

impl SessionSurfaces {
    pub(crate) fn sync_tab_peek_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.peek.visible() {
            if self.peek_previous_focus.is_none() && !self.focus_handle.is_focused(window) {
                self.peek_previous_focus = window.focused(cx);
            }
            window.focus(&self.focus_handle, cx);
        } else if let Some(previous) = self.peek_previous_focus.take()
            && self.focus_handle.is_focused(window)
        {
            window.focus(&previous, cx);
        }
    }

    pub(crate) fn tab_peek_visible(&self) -> bool {
        self.peek.visible()
    }
    pub(crate) fn tab_peek_offset(&self, cx: &App) -> f32 {
        terminal_offset(&self.peek, cx.reduce_motion())
    }
    pub(crate) fn set_tab_peek_region(&mut self, left: f32, width: f32, cx: &mut Context<Self>) {
        if self.peek_left != left || self.peek_width != width {
            self.peek_left = left;
            self.peek_width = width;
            if self.peek.visible() {
                cx.notify();
            }
        }
    }
    pub(crate) fn tab_gesture(&mut self, frame: GestureFrame, cx: &mut Context<Self>) {
        if matches!(frame, GestureFrame::Cancelled) {
            self.peek.dismiss();
            cx.notify();
            return;
        }
        if self.peek.sessions.is_empty() {
            let mut store = self.store.write().unwrap();
            if store.overview_state().is_visible() || store.switcher_state().is_visible() {
                return;
            }
            let selected = store.selected_session_id().cloned();
            let project = selected
                .as_ref()
                .and_then(|id| store.sessions().get(id))
                .map(|s| s.project_id.clone());
            let projection = store.sidebar_projection();
            let sessions = projection
                .projects
                .iter()
                .find(|group| Some(&group.project.id) == project.as_ref())
                .map(|group| {
                    group
                        .active
                        .iter()
                        .chain(
                            group
                                .archived
                                .iter()
                                .filter(|session| Some(&session.id) == selected.as_ref()),
                        )
                        .map(|session| session.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            self.peek.begin(sessions, selected.as_ref());
            self.peek_scroll.set_offset(point(px(0.0), px(0.0)));
        }
        self.peek.update(frame);
        cx.notify();
    }
    pub(crate) fn toggle_tab_peek(&mut self, cx: &mut Context<Self>) {
        if self.peek.visible() {
            self.peek.dismiss();
            cx.notify();
        } else {
            self.tab_gesture(GestureFrame::Released(140.0), cx);
        }
    }
    fn commit_tab_peek(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let mut store = self.store.write().unwrap();
        if store.sessions().get(&id).is_some() {
            store.select(id);
        }
        self.peek.dismiss();
        cx.notify();
    }
    pub(super) fn handle_tab_peek_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.peek.visible() {
            return false;
        }
        if crate::commands::matches_keystroke(
            crate::commands::CommandId::ToggleTabPeek,
            &event.keystroke,
        ) {
            self.peek.dismiss();
            self.sync_tab_peek_focus(window, cx);
            cx.notify();
            cx.stop_propagation();
            return true;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.peek.dismiss(),
            "left" => self.peek.advance(-1),
            "right" | "tab" => self.peek.advance(if event.keystroke.modifiers.shift {
                -1
            } else {
                1
            }),
            "up" => self
                .peek
                .advance(if self.peek_width < 620.0 { -1 } else { -2 }),
            "down" => self
                .peek
                .advance(if self.peek_width < 620.0 { 1 } else { 2 }),
            "enter" => {
                if let Some(id) = self.peek.selected() {
                    self.commit_tab_peek(id, cx);
                }
            }
            _ => {}
        }
        if self.peek.visible() && self.peek.overview() > 0.5 {
            let height = f32::from(window.viewport_size().height);
            let bounds = card_rect(
                self.peek.focused,
                self.peek.sessions.len(),
                self.peek_width,
                height,
                &self.peek,
                cx.reduce_motion(),
            );
            let current = f32::from(self.peek_scroll.offset().y);
            let target = if bounds.y + current < 48.0 {
                48.0 - bounds.y
            } else if bounds.y + bounds.height + current > height {
                height - bounds.y - bounds.height - 12.0
            } else {
                current
            };
            self.peek_scroll.set_offset(point(px(0.0), px(target)));
        }
        self.sync_tab_peek_focus(window, cx);
        cx.stop_propagation();
        cx.notify();
        true
    }
    pub(super) fn render_tab_peek(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        let reduced = cx.reduce_motion();
        let width = if self.peek_width > 0.0 {
            self.peek_width
        } else {
            f32::from(window.viewport_size().width)
        };
        let height = f32::from(window.viewport_size().height);
        let blend = self.peek.overview();
        let mut body = div().id("tab-peek-cards").relative().w_full().min_h_full();
        let sessions: Vec<_> = {
            let store = self.store.read().unwrap();
            self.peek
                .sessions
                .iter()
                .filter_map(|id| store.sessions().get(id).cloned())
                .collect()
        };
        let theme = crate::app_theme::terminal_theme(self.store.read().unwrap().theme_id());
        let mut content_height = height;
        for (index, session) in sessions.iter().enumerate() {
            let bounds = card_rect(index, sessions.len(), width, height, &self.peek, reduced);
            content_height = content_height.max(bounds.y + bounds.height + 24.0);
            let selected = self.peek.focused == index;
            let id = session.id.clone();
            let resident = self.resident_previews.get(&id);
            let preview = if let Some(preview) = resident {
                let font_size = ((bounds.width - 12.0)
                    / (f32::from(preview.grid_cols().max(1)) * 0.65))
                    .min((bounds.height - 36.0) / (f32::from(preview.grid_rows().max(1)) * 1.5))
                    .max(1.0);
                preview
                    .clone()
                    .font(gpui::font(crate::fonts::mono_family()))
                    .font_size(px(font_size))
                    .theme(theme)
                    .into_any_element()
            } else {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(colors.secondary)
                    .child(
                        AgentLogo::new(ui_agent_kind(session.effective_kind()), 22.0, colors)
                            .badged(false),
                    )
                    .child("Preview unavailable")
                    .into_any_element()
            };
            let title = display_title(session);
            body = body.child(
                div()
                    .id(SharedString::from(format!("tab-peek-{}", id.0)))
                    .debug_selector(move || format!("TAB_PEEK_CARD_{index}"))
                    .absolute()
                    .left(px(bounds.x))
                    .top(px(bounds.y))
                    .w(px(bounds.width))
                    .h(px(bounds.height))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(px(Radius::CARD))
                    .border_1()
                    .border_color(if selected {
                        colors.primary.alpha(0.65)
                    } else {
                        colors.primary.alpha(0.15)
                    })
                    .bg(colors.background)
                    .cursor_pointer()
                    .hover(|s| s.border_color(colors.primary.alpha(0.65)))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .p(px(6.0))
                            .overflow_hidden()
                            .child(preview),
                    )
                    .child(
                        div()
                            .h(px(28.0))
                            .rounded_bl(px(Radius::CARD - 1.0))
                            .rounded_br(px(Radius::CARD - 1.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .bg(colors.floating_surface())
                            .text_size(px(11.0))
                            .text_color(colors.primary)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(title),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.commit_tab_peek(id.clone(), cx);
                        cx.stop_propagation();
                    })),
            );
        }
        body = body.h(px(content_height));
        div()
            .id("tab-peek")
            .debug_selector(|| "TAB_PEEK".into())
            .absolute()
            .left(px(self.peek_left))
            .top_0()
            .w(px(width))
            .h_full()
            .occlude()
            .overflow_hidden()
            .bg(colors
                .background
                .alpha(if reduced { 1.0 } else { blend * 0.98 }))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.peek.dismiss();
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .id("tab-peek-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.peek_scroll)
                    .child(body),
            )
            .child(
                div()
                    .absolute()
                    .top(px(12.0))
                    .right(px(16.0))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(if blend > 0.5 {
                        "Tab overview"
                    } else {
                        "Tab peek"
                    })
                    .child(
                        div()
                            .id("tab-peek-expand")
                            .cursor_pointer()
                            .child(if blend > 0.5 { "Collapse" } else { "Show all" })
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.peek.update(GestureFrame::Released(if blend > 0.5 {
                                    140.0
                                } else {
                                    380.0
                                }));
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id("tab-peek-close")
                            .cursor_pointer()
                            .child("Esc  ×")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.peek.dismiss();
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    ),
            )
            .into_any_element()
    }
}
