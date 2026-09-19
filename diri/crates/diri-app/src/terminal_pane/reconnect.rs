//! Explicit recovery of the existing remote owner. This view never retries input.
use super::*;
use diri_proto::RemoteConnectionState;

#[derive(Default)]
pub(super) struct ReconnectUi {
    session: Option<SessionId>,
    request: u64,
    pending: bool,
    error: Option<String>,
}
impl ReconnectUi {
    fn select(&mut self, id: &SessionId) {
        if self.session.as_ref() != Some(id) {
            self.session = Some(id.clone());
            self.request = self.request.wrapping_add(1);
            self.pending = false;
            self.error = None;
        }
    }
    fn begin(&mut self, id: &SessionId) -> Option<u64> {
        self.select(id);
        if self.pending {
            return None;
        }
        self.pending = true;
        self.error = None;
        self.request = self.request.wrapping_add(1);
        Some(self.request)
    }
    fn finish(&mut self, id: &SessionId, request: u64, error: Option<String>) -> bool {
        if self.session.as_ref() != Some(id) || self.request != request {
            return false;
        }
        self.pending = false;
        self.error = error;
        true
    }
}

fn remote_state(session: &SessionRecord) -> Option<RemoteConnectionState> {
    if matches!(session.status, SessionStatus::Exited(_)) {
        return None;
    }
    session.remote_connection.map(|connection| connection.state)
}

impl TerminalPane {
    fn reconnect_remote(&mut self, id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_id().as_ref() != Some(&id) {
            return;
        }
        let failed = self
            .runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(&id)
            .is_some_and(|session| remote_state(session) == Some(RemoteConnectionState::Failed));
        if !failed {
            return;
        }
        let Some(request) = self.reconnect.begin(&id) else {
            return;
        };
        let client = self.runtime.client().clone();
        let target = id.clone();
        let job = self
            .tokio
            .spawn(async move { client.reconnect(&target).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = job
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                // Session events remain authoritative. A delayed RPC response
                // must not overwrite a newer Connected/Exited projection.
                if this.selected_id().as_ref() != Some(&id) {
                    return;
                }
                let error = result.as_ref().err().cloned();
                if this.reconnect.finish(&id, request, error) {
                    if result.is_ok_and(|result| result.uncertain_input_discarded) {
                        this.show_terminal_feedback(
                            "Previous queued input was discarded; delivery was not confirmed.",
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn seed_reconnect_fixture(&mut self, id: &SessionId) {
        match std::env::var("DIRI_WORKSPACE_REMOTE_FAILURE").as_deref() {
            Ok("pending") => {
                self.reconnect.begin(id);
            }
            Ok("error") => {
                if let Some(request) = self.reconnect.begin(id) {
                    self.reconnect.finish(id, request, Some("The existing remote owner could not be reached. Try again when the host is available.".into()));
                }
            }
            _ => {}
        }
    }

    pub(super) fn render_remote_connection(
        &mut self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.reconnect.select(&session.id);
        let state = remote_state(session)?;
        let failed = state == RemoteConnectionState::Failed;
        let message = if self.reconnect.pending || state == RemoteConnectionState::Reconnecting {
            "Reconnecting… Last received screen"
        } else if state == RemoteConnectionState::Connecting {
            "Connecting…"
        } else if failed {
            "Connection lost · Last received screen"
        } else {
            return None;
        };
        let id = session.id.clone();
        let short = self
            .viewport
            .is_some_and(|viewport| viewport.height < 220.0);
        let details = self
            .reconnect
            .error
            .as_ref()
            .map_or_else(|| message.to_owned(), |error| format!("{message}\n{error}"));
        let label = if short && failed && !self.reconnect.pending {
            "Offline"
        } else if short && state == RemoteConnectionState::Connecting {
            "Connecting…"
        } else if short {
            "Reconnecting…"
        } else {
            message
        };
        let mut content = div()
            .flex_1()
            .min_w(px(0.0))
            .text_size(px(11.0))
            .child(label);
        if !short && let Some(error) = &self.reconnect.error {
            content = content.child(
                div()
                    .text_size(px(10.0))
                    .text_color(colors.secondary)
                    .child(error.clone()),
            );
        }
        let pending = self.reconnect.pending;
        let compact = !short && self.viewport.is_some_and(|viewport| viewport.width < 360.0);
        Some(
            div()
                .id("remote-connection-status")
                .debug_selector(|| "remote-connection-status".into())
                .absolute()
                .bottom(px(if short { 6.0 } else { 18.0 }))
                .left(px(if short { 6.0 } else { 12.0 }))
                .right(px(if short { 6.0 } else { 12.0 }))
                .rounded(px(8.0))
                .p(px(if short { 4.0 } else { 8.0 }))
                .tooltip(move |_, cx| {
                    cx.new(|_| crate::palette_chrome::PaletteTooltip(details.clone(), colors))
                        .into()
                })
                .bg(colors.floating_surface())
                .border_1()
                .border_color(colors.floating_stroke())
                .text_color(colors.secondary)
                .flex()
                .items_center()
                .gap(px(8.0))
                .when(compact, |panel| panel.flex_col().items_start())
                .child(content.when(compact, |content| content.w_full()))
                .when(failed, |panel| {
                    panel.child(
                        div()
                            .id("reconnect-remote-session")
                            .debug_selector(|| "reconnect-remote-session".into())
                            .role(Role::Button)
                            .aria_label(if pending {
                                "Reconnecting remote session"
                            } else {
                                "Reconnect remote session"
                            })
                            .flex_none()
                            .rounded(px(5.0))
                            .px(px(8.0))
                            .py(px(5.0))
                            .text_size(px(11.0))
                            .text_color(colors.primary)
                            .bg(colors.primary.alpha(0.08))
                            .opacity(if pending { 0.5 } else { 1.0 })
                            .when(!pending, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(move |button| button.bg(colors.primary.alpha(0.14)))
                            })
                            .child(if pending && short {
                                "Wait"
                            } else if pending {
                                "Reconnecting…"
                            } else {
                                "Reconnect"
                            })
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if !pending {
                                    this.reconnect_remote(id.clone(), window, cx);
                                }
                                cx.stop_propagation();
                            })),
                    )
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn reconnect_button_keeps_last_grid_and_coalesces_pending_clicks(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let mut session = crate::terminal_pane::tests::fixture_session();
        session.host = Some("test-host".into());
        session.remote_connection = Some(diri_proto::RemoteConnection {
            state: RemoteConnectionState::Failed,
            since: diri_proto::DateMillis(0.0),
        });
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(session.clone());
            store.select(id.clone());
        }
        let services = runtime.clone();
        let (pane, cx) =
            cx.add_window_view(move |window, cx| TerminalPane::new(services, tokio, window, cx));
        cx.simulate_resize(gpui::size(px(520.0), px(400.0)));
        cx.run_until_parked();
        let grid = pane.read_with(cx, |pane, _| pane.residents[&id].element.buffer());
        {
            let mut grid = grid.write().unwrap();
            *grid = GridBuffer::new(80, 24);
            grid.cells[0].scalar = 'X' as u32;
        }
        let button = cx.debug_bounds("reconnect-remote-session").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        let request = pane.read_with(cx, |pane, _| {
            assert!(pane.reconnect.pending);
            pane.reconnect.request
        });
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        pane.update(cx, |pane, cx| {
            assert_eq!(pane.reconnect.request, request);
            assert!(
                pane.reconnect
                    .finish(&id, request, Some("remote_owner_unavailable".into()))
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("reconnect-remote-session").is_some());
        cx.simulate_resize(gpui::size(px(280.0), px(400.0)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("remote-connection-status").unwrap();
        let action = cx.debug_bounds("reconnect-remote-session").unwrap();
        assert!(panel.contains(&action.center()));
        assert!(panel.size.height < px(200.0));
        pane.update(cx, |pane, cx| {
            pane.set_viewport(
                TerminalViewport {
                    width: 160.0,
                    height: 120.0,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(160.0), px(120.0)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("remote-connection-status").unwrap();
        let action = cx.debug_bounds("reconnect-remote-session").unwrap();
        assert!(panel.contains(&action.center()));
        assert!(panel.size.height < px(52.0));
        assert!(panel.top() >= px(0.0) && panel.bottom() <= px(120.0));
        pane.read_with(cx, |pane, _| {
            assert!(Arc::ptr_eq(&grid, &pane.residents[&id].element.buffer()));
            assert_eq!(grid.read().unwrap().cells[0].scalar, 'X' as u32);
            assert!(!pane.reconnect.pending);
        });
        session.remote_connection.as_mut().unwrap().state = RemoteConnectionState::Connected;
        runtime.store.write().unwrap().upsert_session(session);
        pane.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-connection-status").is_none());
    }

    #[test]
    fn reconnect_completion_is_scoped_and_duplicate_clicks_are_coalesced() {
        let mut state = ReconnectUi::default();
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        let request = state.begin(&a).unwrap();
        assert!(state.begin(&a).is_none());
        state.select(&b);
        assert!(!state.finish(&a, request, Some("old error".into())));
        assert!(state.error.is_none());
        let request = state.begin(&b).unwrap();
        assert!(state.finish(&b, request, Some("inspection failed".into())));
        assert!(!state.pending);
        assert_eq!(state.error.as_deref(), Some("inspection failed"));
        assert!(state.begin(&b).is_some());
        assert!(state.error.is_none());
    }
}
