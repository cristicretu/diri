use super::*;
use crate::tab_peek::{
    GestureFrame, card_rect, preview_reveal_offset, terminal_offset, visible_card_indices,
};
use gpui::App;

/// A local preview connection is not proof that the remote Holder is reachable.
fn preview_caption(
    session: &SessionRecord,
    source: Option<crate::tab_preview::PreviewState>,
) -> Option<&'static str> {
    use crate::tab_preview::PreviewState;
    use diri_proto::{RemoteConnectionState, SessionStatus};
    let remote = session
        .host
        .as_ref()
        .and(session.remote_connection.as_ref())
        .map(|connection| connection.state);
    if matches!(session.status, SessionStatus::Exited(_))
        || remote == Some(RemoteConnectionState::Exited)
    {
        return Some("Exited");
    }
    if session.hibernation.is_some() {
        return Some("Paused");
    }
    if source == Some(PreviewState::Disconnected) {
        return Some("Disconnected");
    }
    session.host.as_ref()?;
    Some(match remote {
        Some(RemoteConnectionState::Connecting) => "Connecting",
        Some(RemoteConnectionState::Connected) => "Connected",
        Some(RemoteConnectionState::Reconnecting) => "Reconnecting",
        Some(RemoteConnectionState::Failed) => "Connection failed",
        Some(RemoteConnectionState::Exited) => "Exited",
        Some(RemoteConnectionState::Unknown) | None => "Last received",
    })
}

impl SessionSurfaces {
    pub(super) fn dismiss_tab_peek(&mut self, cx: &mut Context<Self>) {
        if self.peek.is_closing() {
            return;
        }
        self.closing_previews = self
            .peek
            .sessions
            .iter()
            .filter_map(|id| {
                self.live_previews
                    .get(id)
                    .map(|preview| (id.clone(), preview.element.clone()))
            })
            .collect();
        self.live_previews.clear();
        self.peek
            .animate_to(0.0, cx.background_executor().now(), cx.reduce_motion());
    }

    pub(crate) fn cancel_tab_peek_immediately(&mut self, cx: &mut Context<Self>) {
        self.peek.dismiss();
        self.live_previews.clear();
        self.closing_previews.clear();
        cx.notify();
    }

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
    pub(crate) fn set_tab_peek_region(
        &mut self,
        left: f32,
        top: f32,
        width: f32,
        cx: &mut Context<Self>,
    ) {
        if self.peek_left != left || self.peek_top != top || self.peek_width != width {
            self.peek_left = left;
            self.peek_top = top;
            self.peek_width = width;
            if self.peek.visible() {
                cx.notify();
            }
        }
    }
    pub(crate) fn tab_gesture(&mut self, frame: GestureFrame, cx: &mut Context<Self>) {
        if matches!(frame, GestureFrame::Cancelled) {
            self.dismiss_tab_peek(cx);
            cx.notify();
            return;
        }
        if self.peek.sessions.is_empty() {
            let mut store = self.store.write().unwrap();
            if store.overview_state().is_visible() || store.switcher_state().is_visible() {
                return;
            }
            let selected = store.selected_session_id().cloned();
            let sessions = crate::tab_navigation::selected_project_tabs(&mut store)
                .sessions
                .iter()
                .map(|session| session.id.clone())
                .collect();
            self.peek.begin(sessions, selected.as_ref());
            self.peek_scroll.set_offset(point(px(0.0), px(0.0)));
        }
        if matches!(frame, GestureFrame::Tracking(_)) {
            self.closing_previews.clear();
        }
        self.peek
            .update_animated(frame, cx.background_executor().now(), cx.reduce_motion());
        if !self.peek.visible() {
            self.live_previews.clear();
        }
        cx.notify();
    }
    pub(crate) fn toggle_tab_peek(&mut self, cx: &mut Context<Self>) {
        if self.peek.visible() {
            self.dismiss_tab_peek(cx);
            cx.notify();
        } else {
            self.tab_gesture(GestureFrame::Tracking(0.0), cx);
            self.peek
                .animate_to(140.0, cx.background_executor().now(), cx.reduce_motion());
            cx.notify();
        }
    }
    fn commit_tab_peek(&mut self, id: SessionId, cx: &mut Context<Self>) {
        if !self.peek.visible() {
            return;
        }
        let mut store = self.store.write().unwrap();
        let activate = store.sessions().get(&id).is_some();
        if activate {
            store.select(id);
        }
        drop(store);
        self.dismiss_tab_peek(cx);
        if activate {
            cx.emit(TabPeekActivated);
        }
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
            self.dismiss_tab_peek(cx);
            self.sync_tab_peek_focus(window, cx);
            cx.notify();
            cx.stop_propagation();
            return true;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_tab_peek(cx),
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
            let height = (f32::from(window.viewport_size().height) - self.peek_top).max(0.0);
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
        let height = (f32::from(window.viewport_size().height) - self.peek_top).max(0.0);
        let blend = self.peek.overview();
        let mut body = div().id("tab-peek-cards").relative().w_full().min_h_full();
        let sessions: Vec<_> = {
            let store = self.store.read().unwrap();
            let focused = self.peek.selected();
            let previous_count = self.peek.sessions.len();
            self.peek
                .sessions
                .retain(|id| store.sessions().get(id).is_some());
            if self.peek.sessions.len() != previous_count {
                cx.notify();
            }
            self.peek.focused = focused
                .as_ref()
                .and_then(|id| {
                    self.peek
                        .sessions
                        .iter()
                        .position(|candidate| candidate == id)
                })
                .unwrap_or_else(|| {
                    self.peek
                        .focused
                        .min(self.peek.sessions.len().saturating_sub(1))
                });
            self.peek
                .sessions
                .iter()
                .filter_map(|id| store.sessions().get(id).cloned())
                .collect()
        };
        let visible = visible_card_indices(
            &self.peek,
            width,
            height,
            f32::from(self.peek_scroll.offset().y),
            reduced,
        );
        let mut wanted: Vec<_> = visible
            .iter()
            .filter_map(|index| sessions.get(*index))
            .filter(|session| {
                self.peek.visible()
                    && !self.resident_previews.contains_key(&session.id)
                    && !matches!(session.status, diri_proto::SessionStatus::Exited(_))
            })
            .map(|session| session.id.clone())
            .collect();
        let focused = self.peek.selected();
        wanted.sort_by_key(|id| Some(id) != focused.as_ref());
        if let Some(runtime) = &self.tokio {
            let socket = self.client.socket_path().to_path_buf();
            self.live_previews.sync(wanted, |id| {
                crate::tab_preview::LivePreview::open(runtime, socket.clone(), id, cx)
            });
        }
        let theme = crate::app_theme::terminal_theme(self.store.read().unwrap().theme_id());
        let mut content_height = height;
        for (index, session) in sessions.iter().enumerate() {
            let bounds = card_rect(index, sessions.len(), width, height, &self.peek, reduced);
            content_height = content_height.max(bounds.y + bounds.height + 24.0);
            if !visible.contains(&index) {
                continue;
            }
            let selected = self.peek.focused == index;
            let id = session.id.clone();
            let live = self.live_previews.get(&id);
            let state = live.map(|preview| *preview.state.borrow());
            let resident = self.resident_previews.get(&id);
            let grid = resident
                .or_else(|| self.closing_previews.get(&id))
                .or_else(|| {
                    live.filter(|preview| preview.element.grid_cols() > 0)
                        .map(|preview| &preview.element)
                });
            let preview = if let Some(preview) = grid {
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
                    .child(
                        if matches!(session.status, diri_proto::SessionStatus::Exited(_)) {
                            "Session exited"
                        } else {
                            match state {
                                Some(crate::tab_preview::PreviewState::Loading) => {
                                    "Loading preview…"
                                }
                                _ => "Preview unavailable",
                            }
                        },
                    )
                    .into_any_element()
            };
            let title = display_title(session);
            let status = preview_caption(session, state);
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
                            .child(div().flex_1().min_w(px(0.0)).truncate().child(title))
                            .when_some(status, |row, status| {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .debug_selector(move || format!("TAB_PEEK_STATUS_{index}"))
                                        .ml(px(6.0))
                                        .text_size(px(9.0))
                                        .text_color(colors.secondary)
                                        .child(status),
                                )
                            }),
                    )
                    .when(self.peek.visible(), |element| {
                        element.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.peek.visible() {
                            this.commit_tab_peek(id.clone(), cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        body = body
            .h(px(content_height))
            .when(self.peek.is_closing(), |body| {
                // Preserve the last scrolled pose after scroll interaction is disabled.
                body.top(self.peek_scroll.offset().y)
            });
        div()
            .id("tab-peek")
            .debug_selector(|| "TAB_PEEK".into())
            .absolute()
            .left(px(self.peek_left))
            .top(px(self.peek_top))
            .w(px(width))
            .h(px(height))
            .when(self.peek.visible(), |surface| surface.occlude())
            .opacity(self.peek.reveal())
            .overflow_hidden()
            .bg(colors
                .background
                .alpha(if reduced { 1.0 } else { blend * 0.98 }))
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                if this.peek.visible() {
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.peek.visible() {
                        this.dismiss_tab_peek(cx);
                        cx.notify();
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .id("tab-peek-scroll")
                    .size_full()
                    .when(self.peek.visible(), |element| {
                        element.overflow_y_scroll().track_scroll(&self.peek_scroll)
                    })
                    .child(body),
            )
            .child(
                div()
                    .absolute()
                    .top(px(12.0 + preview_reveal_offset(&self.peek, reduced)))
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
                            .when(self.peek.visible(), |element| {
                                element.on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.peek.visible() {
                                    return;
                                }
                                this.peek.animate_to(
                                    if blend > 0.5 { 140.0 } else { 380.0 },
                                    cx.background_executor().now(),
                                    cx.reduce_motion(),
                                );
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id("tab-peek-close")
                            .cursor_pointer()
                            .child("Esc  ×")
                            .when(self.peek.visible(), |element| {
                                element.on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.peek.visible() {
                                    this.dismiss_tab_peek(cx);
                                    cx.notify();
                                    cx.stop_propagation();
                                }
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sidebar::{PreviewScenario, SidebarPreviewFixture},
        tab_preview::PreviewState,
    };
    use diri_proto::{
        DateMillis, ExitInfo, ExitReason, HibernationInfo, HibernationReason, RemoteConnection,
        RemoteConnectionState, SessionStatus,
    };

    fn remote_session() -> SessionRecord {
        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .remove(0);
        session.host = Some("fixture-host".into());
        session.status = SessionStatus::Working;
        session.hibernation = None;
        session.remote_connection = None;
        session
    }

    #[test]
    fn local_preview_delivery_never_invents_remote_connectivity() {
        let mut session = remote_session();
        assert_eq!(
            preview_caption(&session, Some(PreviewState::Live)),
            Some("Last received")
        );
        for (state, expected) in [
            (RemoteConnectionState::Unknown, "Last received"),
            (RemoteConnectionState::Connecting, "Connecting"),
            (RemoteConnectionState::Connected, "Connected"),
            (RemoteConnectionState::Reconnecting, "Reconnecting"),
            (RemoteConnectionState::Failed, "Connection failed"),
            (RemoteConnectionState::Exited, "Exited"),
        ] {
            session.remote_connection = Some(RemoteConnection {
                state,
                since: DateMillis(1.0),
            });
            assert_eq!(
                preview_caption(&session, Some(PreviewState::Live)),
                Some(expected)
            );
            // Transition time is not terminal output age and does not change the label.
            session.remote_connection.as_mut().unwrap().since = DateMillis(9_999_999.0);
            assert_eq!(
                preview_caption(&session, Some(PreviewState::Live)),
                Some(expected)
            );
        }
    }

    #[test]
    fn authoritative_exit_pause_and_local_delivery_loss_have_explicit_precedence() {
        let mut session = remote_session();
        session.remote_connection = Some(RemoteConnection {
            state: RemoteConnectionState::Connected,
            since: DateMillis(1.0),
        });
        assert_eq!(
            preview_caption(&session, Some(PreviewState::Disconnected)),
            Some("Disconnected")
        );
        session.hibernation = Some(HibernationInfo {
            since: DateMillis(2.0),
            reason: HibernationReason::Manual,
            tree_pids: vec![],
            tree_start_times: None,
        });
        assert_eq!(
            preview_caption(&session, Some(PreviewState::Disconnected)),
            Some("Paused")
        );
        session.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: None,
        });
        assert_eq!(
            preview_caption(&session, Some(PreviewState::Disconnected)),
            Some("Exited")
        );
        session.status = SessionStatus::Working;
        session.hibernation = None;
        session.host = None;
        assert_eq!(preview_caption(&session, Some(PreviewState::Live)), None);
    }
    struct CaptionHarness(Entity<SessionSurfaces>);
    impl Render for CaptionHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().relative().child(self.0.clone())
        }
    }

    #[gpui::test]
    fn remote_caption_stays_inside_a_narrow_card_with_a_long_title(cx: &mut gpui::TestAppContext) {
        let mut session = remote_session();
        session.title = "A long task title that must leave the transport caption readable".into();
        session.remote_connection = Some(RemoteConnection {
            state: RemoteConnectionState::Failed,
            since: DateMillis(1.0),
        });
        let id = session.id.clone();
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .unwrap()
            .hydrate(diri_proto::SessionListResult {
                sessions: vec![session],
                projects: vec![],
            });
        runtime.store.write().unwrap().select(id);
        let (_, cx) = cx.add_window_view(move |_, cx| {
            CaptionHarness(cx.new(|cx| {
                let mut surface = SessionSurfaces::new(runtime, None, cx);
                surface.tab_gesture(GestureFrame::Tracking(140.0), cx);
                surface.tab_gesture(GestureFrame::Released(140.0), cx);
                surface
            }))
        });
        cx.simulate_resize(gpui::size(px(250.0), px(700.0)));
        let card = cx.debug_bounds("TAB_PEEK_CARD_0").unwrap();
        let status = cx.debug_bounds("TAB_PEEK_STATUS_0").unwrap();
        assert!(status.left() >= card.left());
        assert!(status.right() <= card.right());
        assert!(status.bottom() <= card.bottom());
        assert!(status.size.width > px(40.0));
    }
}
