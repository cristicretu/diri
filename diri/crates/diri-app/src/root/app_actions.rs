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
            match self.agent_confirmation(&request) {
                Some((title, message)) => {
                    self.confirm_agent_action(request, &title, &message, window, cx);
                }
                None => self.run_app_perform(request, window, cx),
            }
        }
    }

    /// The question for a destructive action an Agent asked for (an
    /// Agent's `dirijor do`, or a session-scoped one through its tools):
    /// the person approves it first. The person's own requests, a shell's
    /// and a note's carry no Agent, so they run as asked.
    fn agent_confirmation(&self, request: &AppPerformRequest) -> Option<(String, String)> {
        let actor = request.actor.as_ref()?;
        if !crate::app_actions::is_destructive(&request.action) {
            return None;
        }
        let store = self.window_store.read().expect("store");
        let agent = store
            .sessions()
            .get(actor)
            .filter(|session| {
                !session.is_note() && session.effective_kind() != &AgentKind::SHELL
            })
            .map(|session| crate::switcher::display_title_str(session).to_owned())?;
        let action = crate::app_actions::title(&request.action)
            .unwrap_or_else(|| request.action.clone());
        Some((
            crate::i18n::tf(
                "agent_confirm.title",
                &[("agent", &agent), ("action", &action)],
            ),
            crate::i18n::tf("agent_confirm.message", &[("agent", &agent)]),
        ))
    }

    /// Asks the person, then runs the action or answers that they declined.
    /// One question at a time: another waits for none, it is refused.
    fn confirm_agent_action(
        &mut self,
        request: AppPerformRequest,
        title: &str,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = request.request_id;
        if self.agent_action_prompt.is_some() {
            self.window_store.write().expect("store").reply_app_perform(
                id,
                Err(ControlError::new(
                    "action_declined",
                    "the person is already answering another agent's request; try again later",
                )),
            );
            return;
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            title,
            Some(message),
            &[
                gpui::PromptButton::ok(crate::i18n::t("agent_confirm.allow")),
                gpui::PromptButton::cancel(crate::i18n::t("agent_confirm.deny")),
            ],
            cx,
        );
        // Past the Engine's deadline the Agent was already told the app timed
        // out, so a late Allow must not run the action behind its back.
        let deadline = request
            .timeout_ms
            .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
        self.agent_action_prompt = Some(cx.spawn_in(window, async move |this, cx| {
            let allowed = answer.await.ok() == Some(0);
            let expired = deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline);
            let _ = crate::floating::update_in_owner(&this, cx, |root, window, cx| {
                root.agent_action_prompt = None;
                if expired {
                    // Nobody is waiting for the answer any more.
                } else if allowed {
                    root.run_app_perform(request, window, cx);
                } else {
                    root.window_store.write().expect("store").reply_app_perform(
                        id,
                        Err(ControlError::new(
                            "action_declined",
                            "the person declined this action",
                        )),
                    );
                }
            });
        }));
    }

    fn run_app_perform(
        &mut self,
        request: AppPerformRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = request.request_id;
        if crate::scripting::is_script_action(&request.action) {
                // A script command answers when its function returns, on
                // the script thread; the window does not wait for it.
                match crate::scripting::perform(&request) {
                    Ok(answer) => {
                        cx.spawn(async move |this, cx| {
                            let result = answer.await.unwrap_or_else(|_| {
                                Err(ControlError::new(
                                    "script_error",
                                    "the script stopped before it answered",
                                ))
                            });
                            let _ = this.update(cx, |this, _| {
                                this.window_store
                                    .write()
                                    .expect("store")
                                    .reply_app_perform(id, result);
                            });
                        })
                        .detach();
                    }
                    Err(error) => self
                        .window_store
                        .write()
                        .expect("store")
                        .reply_app_perform(id, Err(error)),
                }
                return;
            }
            let result = self.perform_app_action(&request, window, cx);
            self.window_store
                .write()
                .expect("store")
                .reply_app_perform(id, result);
    }

    /// Publishes the window's actions again: the script's commands changed.
    pub(crate) fn republish_app_actions(&mut self) {
        self.window_store
            .write()
            .expect("store")
            .request_app_actions_publish();
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
            Resolved::ListKeys => Ok(crate::scripting::keymap_listing()),
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
    use crate::sidebar::SidebarPreviewFixture;
    use serde_json::json;

    fn request(id: u64, action: &str, args: Value) -> AppPerformRequest {
        AppPerformRequest {
            request_id: id,
            action: action.into(),
            args,
            actor: None,
            timeout_ms: None,
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

    /// A destructive action an Agent asks for waits for the person: Allow
    /// runs it, Don't Allow refuses it, and an answer after the Engine gave
    /// up runs nothing. The same request from a shell tab or with no actor
    /// (the person, a script) runs without asking, and a safe one never asks.
    #[gpui::test]
    fn an_agents_destructive_action_asks_the_person_first(cx: &mut gpui::TestAppContext) {
        cx.update(crate::alerts::enable);
        let (runtime, mut effects) = crate::store::StoreRuntime::inert_with_effects();
        let mut services = Arc::try_unwrap(test_services())
            .ok()
            .expect("fresh test services");
        services.store = Arc::new(runtime);
        let services = Arc::new(services);
        let runtime = services.store.clone();
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            let base = store
                .sessions()
                .values()
                .next()
                .expect("a preview session")
                .as_ref()
                .clone();
            for (id, kind) in [("s_agent", AgentKind::CLAUDE_CODE), ("s_shell", AgentKind::SHELL)] {
                let mut record = base.clone();
                record.id = SessionId::new(id);
                record.kind = kind;
                record.note_id = None;
                record.archived_at = None;
                store.upsert_session(record);
            }
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Typical, window, cx)
        });
        let window_store = root.read_with(cx, |root, _| root.window_store.clone());
        let mut next = 0;
        let mut perform = |action: &str,
                           actor: Option<&str>,
                           timeout_ms: Option<u64>,
                           cx: &mut gpui::VisualTestContext| {
            next += 1;
            let mut params = json!({"requestId": next, "action": action});
            if let Some(actor) = actor {
                params["actor"] = json!(actor);
            }
            if let Some(timeout_ms) = timeout_ms {
                params["timeoutMs"] = json!(timeout_ms);
            }
            window_store
                .write()
                .unwrap()
                .handle_event(diri_client::EventEnvelope {
                    name: diri_proto::EventName::APP_PERFORM_REQUESTED.into(),
                    params,
                    seq: 0,
                });
            root.update_in(cx, |root, window, cx| root.perform_app_actions(window, cx));
            cx.run_until_parked();
        };

        // The window's answers to the Engine, by request id.
        let mut replies = || {
            let mut replies = Vec::new();
            while let Ok(effect) = effects.try_recv() {
                if let crate::store::StoreEffect::AppReply(reply) = effect {
                    replies.push((reply.request_id, reply.error.map(|error| error.code)));
                }
            }
            replies
        };
        let declined = Some("action_declined".to_string());

        perform("note-delete", Some("s_agent"), Some(30_000), cx);
        assert!(cx.has_pending_prompt(), "the person is asked first");
        assert!(replies().is_empty(), "nothing runs before the answer");
        cx.simulate_prompt_answer("Don't Allow");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert!(root.read_with(cx, |root, _| root.agent_action_prompt.is_none()));
        assert_eq!(replies(), vec![(1, declined.clone())]);

        perform("note-delete", Some("s_agent"), Some(30_000), cx);
        cx.simulate_prompt_answer("Allow");
        cx.run_until_parked();
        let allowed = replies();
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].0, 2);
        assert_ne!(allowed[0].1, declined, "Allow runs the action");

        // The Engine stopped waiting before the person answered: the Agent
        // was told the app timed out, so a late Allow runs nothing.
        perform("note-delete", Some("s_agent"), Some(0), cx);
        cx.simulate_prompt_answer("Allow");
        cx.run_until_parked();
        assert!(replies().is_empty(), "a late Allow runs nothing");
        assert!(root.read_with(cx, |root, _| root.agent_action_prompt.is_none()));

        perform("note-delete", Some("s_shell"), None, cx);
        assert!(!cx.has_pending_prompt(), "a shell tab is the person");
        perform("note-delete", None, None, cx);
        assert!(!cx.has_pending_prompt(), "no actor is the person or a script");
        perform("toggle-sidebar", Some("s_agent"), None, cx);
        assert!(!cx.has_pending_prompt(), "a safe action never asks");
    }
}
