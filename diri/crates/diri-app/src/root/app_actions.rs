//! Running an app action the Engine routed to this window: what an Agent's
//! `perform_action`, a script or `dirijor do` asked for.
//!
//! A command runs through the same GPUI action its key binding and palette
//! row dispatch, so the result is what the person would get by pressing the
//! key. The window is never brought forward: an Agent changes what the window
//! shows, not where the person is working.

use super::*;
use crate::app_actions::{Resolved, resolve, unavailable};
use diri_proto::ControlError;
use diri_proto::app_actions::AppPerformRequest;
use serde_json::{Value, json};

impl RootView {
    pub(super) fn perform_app_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for request in self.window_store.take_app_performs() {
            let id = request.request_id;
            let result = self.perform_app_action(&request, window, cx);
            self.window_store
                .write()
                .expect("store")
                .reply_app_perform(id, result);
        }
    }

    fn perform_app_action(
        &mut self,
        request: &AppPerformRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, ControlError> {
        let known = |this: &Self, id: &SessionId| {
            this.window_store
                .read()
                .expect("store")
                .sessions()
                .contains_key(id)
        };
        let missing = |id: &SessionId| ControlError::not_found(format!("no session {}", id.0));
        match resolve(request)? {
            Resolved::Command(command) => {
                let action = command.action();
                // Where a key press would land: the focused element, or the
                // window's root when nothing in it has focus yet.
                let focused = window.focused(cx).is_some();
                let available = if focused {
                    window.is_action_available(action.as_ref(), cx)
                } else {
                    window.is_action_available_in(action.as_ref(), &self.focus)
                };
                if !available && !crate::app_actions::handled_globally(command) {
                    return Err(unavailable(&request.action));
                }
                crate::telemetry::action(action.name(), "agent");
                if focused {
                    window.dispatch_action(action, cx);
                } else {
                    let root = self.focus.clone();
                    window.defer(cx, move |window, cx| {
                        root.dispatch_action(action.as_ref(), window, cx)
                    });
                }
                Ok(json!({"dispatched": request.action}))
            }
            Resolved::ShowSession(id) => {
                if !known(self, &id) {
                    return Err(missing(&id));
                }
                self.open_workspace_launch_session(id.clone(), window, cx);
                Ok(json!({"shown": id.0}))
            }
            Resolved::CloseSession(id) => {
                if !known(self, &id) {
                    return Err(missing(&id));
                }
                // The same request ⌘W makes, including its confirmation when
                // the session is busy.
                self.window_store
                    .write()
                    .expect("store")
                    .request_close(vec![id.clone()]);
                Ok(json!({"closing": id.0}))
            }
            Resolved::ArchiveSession(id) => {
                if !known(self, &id) {
                    return Err(missing(&id));
                }
                self.window_store
                    .write()
                    .expect("store")
                    .archive_sessions(vec![id.clone()]);
                Ok(json!({"archived": id.0}))
            }
            Resolved::SetTheme(theme) => {
                self.window_store
                    .write()
                    .expect("store")
                    .update_preferences(|prefs| {
                        prefs.follow_system_theme = false;
                        prefs.terminal_theme = theme.clone();
                    })
                    .map_err(|error| ControlError::internal(error.to_string()))?;
                self.services.store.publish_local_change();
                Ok(json!({"theme": theme}))
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::root::tests::test_services;
    use serde_json::json;

    fn request(id: u64, action: &str, args: Value) -> AppPerformRequest {
        AppPerformRequest {
            request_id: id,
            action: action.into(),
            args,
            actor: None,
        }
    }

    #[gpui::test]
    fn an_action_runs_what_its_key_runs_and_reports_what_it_could_not(
        cx: &mut gpui::TestAppContext,
    ) {
        let services = test_services();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let mut root = RootView::new(services, true, PreviewScenario::Typical, window, cx);
            root.preview = true;
            root
        });
        cx.simulate_resize(gpui::size(px(1000.0), px(700.0)));
        cx.run_until_parked();
        let visible = root.read_with(cx, |root, cx| root.sidebar.read(cx).is_visible());

        // The same command ⌘B-style keys dispatch.
        let toggled = root.update_in(cx, |root, window, cx| {
            root.perform_app_action(&request(1, "toggle-sidebar", json!({})), window, cx)
        });
        assert_eq!(toggled, Ok(json!({"dispatched": "toggle-sidebar"})));
        cx.run_until_parked();
        assert_ne!(
            root.read_with(cx, |root, cx| root.sidebar.read(cx).is_visible()),
            visible
        );

        // A session this window does not know is refused, not guessed.
        let missing = root.update_in(cx, |root, window, cx| {
            root.perform_app_action(
                &request(2, "show-session", json!({"session_id": "s_gone"})),
                window,
                cx,
            )
        });
        assert_eq!(missing.unwrap_err().code, "not_found");

        // A hidden command is not an action at all.
        let paste = root.update_in(cx, |root, window, cx| {
            root.perform_app_action(&request(3, "terminal-paste", json!({})), window, cx)
        });
        assert_eq!(paste.unwrap_err().code, "unknown_action");

        // Queued requests are drained and leave nothing behind.
        root.update_in(cx, |root, window, cx| {
            root.window_store
                .write()
                .expect("store")
                .handle_event(diri_client::EventEnvelope {
                    name: diri_proto::EventName::APP_PERFORM_REQUESTED.into(),
                    params: json!({"requestId": 4, "action": "toggle-sidebar"}),
                    seq: 0,
                });
            root.perform_app_actions(window, cx);
            assert!(
                !root
                    .window_store
                    .read()
                    .expect("store")
                    .has_pending_ui_request()
            );
        });
    }
}
