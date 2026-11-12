//! Computer use setup, as Settings › Agents shows it: Cua Driver's install
//! and macOS grants, polled from the Engine only while someone is looking
//! (the section is on screen, or a permission guide is open).
use super::*;

/// How often the status is re-read while watched: quick enough that a
/// switch flipped in System Settings shows up before the hand leaves it.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub(super) struct ComputerSetupState {
    status: Option<diri_proto::ComputerStatus>,
    /// The Engine could not be asked (old Engine, disconnected).
    error: Option<String>,
    watch_until: Option<Instant>,
    polling: bool,
    /// An action was sent and its answer has not arrived.
    busy: bool,
    /// The person clicked Set up: once installed, guide them through the
    /// grants without another click.
    guide_after_install: bool,
    /// Sessions that may use foreground computer use now, for their
    /// sidebar marks. Kept apart from `status`, which is only polled while
    /// Settings shows it.
    foreground_sessions: HashSet<SessionId>,
}

impl SessionStore {
    pub fn computer_status(&self) -> Option<&diri_proto::ComputerStatus> {
        self.computer.status.as_ref()
    }

    pub fn computer_status_error(&self) -> Option<&str> {
        self.computer.error.as_deref()
    }

    pub fn computer_busy(&self) -> bool {
        self.computer.busy || self.computer.status.as_ref().is_some_and(|s| s.installing)
    }

    /// Keeps the status fresh for `span` more. Cheap to call every frame.
    pub fn watch_computer(&mut self, span: Duration) {
        let until = Instant::now() + span;
        if self.computer.watch_until.is_none_or(|current| current < until) {
            self.computer.watch_until = Some(until);
        }
        if !self.computer.polling {
            self.computer.polling = true;
            self.emit(StoreEffect::WatchComputer);
        }
    }

    pub fn computer_setup(&mut self, action: diri_proto::ComputerSetupAction) {
        if self.computer.busy {
            return;
        }
        if action == diri_proto::ComputerSetupAction::Install {
            self.computer.guide_after_install = true;
        }
        self.computer.busy = true;
        self.watch_computer(Duration::from_secs(30));
        self.emit(StoreEffect::ComputerSetup(action));
    }

    /// True once, right after an install the person started from Set up
    /// succeeded: the caller opens the first permission guide.
    pub fn take_guide_after_install(&mut self) -> bool {
        let installed = self
            .computer
            .status
            .as_ref()
            // Grants known: right after an install the daemon may still be
            // starting, and a guide for grants already given would flash by.
            .is_some_and(|status| {
                status.installed() && !status.installing && status.accessibility.is_some()
            });
        let failed = self
            .computer
            .status
            .as_ref()
            .is_some_and(|status| status.install_error.is_some());
        if failed {
            self.computer.guide_after_install = false;
        }
        if self.computer.guide_after_install && installed && !self.computer.busy {
            self.computer.guide_after_install = false;
            return true;
        }
        false
    }

    /// Whether this session may bring windows forward for computer use.
    pub fn computer_foreground_held(&self, id: &SessionId) -> bool {
        self.computer.foreground_sessions.contains(id)
    }

    /// The sessions that may bring windows forward, in sidebar order.
    pub fn computer_foreground_sessions(&self) -> Vec<SessionId> {
        let mut ids: Vec<SessionId> = self.computer.foreground_sessions.iter().cloned().collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        ids
    }

    /// Settings › Agents › Computer use › Foreground actions.
    pub fn set_computer_foreground(&mut self, foreground: diri_proto::ComputerForeground) {
        if let Some(status) = self.computer.status.as_mut() {
            status.foreground = foreground;
        }
        self.emit(StoreEffect::ConfigureComputer(
            diri_proto::ComputerConfigureParams { foreground },
        ));
    }

    /// Takes foreground back from one session, or every session.
    pub fn revoke_computer_foreground(&mut self, id: Option<SessionId>) {
        match &id {
            Some(id) => {
                self.computer.foreground_sessions.remove(id);
            }
            None => self.computer.foreground_sessions.clear(),
        }
        self.emit(StoreEffect::RevokeComputerForeground(id));
    }

    /// The person answered an Agent's foreground request.
    pub fn answer_computer_foreground(&mut self, session_id: SessionId, allow: bool) {
        self.emit(StoreEffect::AnswerComputerForeground(
            diri_proto::ComputerForegroundAnswerParams { session_id, allow },
        ));
    }

    pub(super) fn set_computer_foreground_sessions(&mut self, sessions: Vec<SessionId>) {
        self.computer.foreground_sessions = sessions.into_iter().collect();
    }

    /// An Agent's first foreground action waits for the person.
    pub(super) fn computer_foreground_requested(
        &mut self,
        event: diri_proto::ComputerForegroundRequestedEvent,
    ) {
        let toast = foreground_toast(&event);
        let title = toast.message.clone();
        self.emit(StoreEffect::StatusTransition(StatusTransition {
            dismiss: Vec::new(),
            sound: None,
            // The person is likely in another app: the one being driven.
            notification: Some(crate::notifications::NotificationRequest {
                session_event: false,
                guard: None,
                identifier: format!("computer-foreground:{}", event.session_id.0),
                title,
                body: crate::i18n::t("notification.computer_foreground_body").to_owned(),
                thread_identifier: Some(event.session_id.0.clone()),
                action_data: None,
                use_system_sound: false,
                reply: false,
            }),
            in_app_banner: Some(toast),
        }));
    }

    #[cfg(test)]
    pub fn seed_computer_status(&mut self, status: diri_proto::ComputerStatus) {
        self.set_computer_status(Ok(status));
    }

    fn set_computer_status(&mut self, status: Result<diri_proto::ComputerStatus, String>) -> bool {
        let mut changed = false;
        if let Ok(status) = &status {
            let sessions: HashSet<SessionId> = status.foreground_sessions.iter().cloned().collect();
            changed = sessions != self.computer.foreground_sessions;
            self.computer.foreground_sessions = sessions;
        }
        let (status, error) = match status {
            Ok(status) => (Some(status), None),
            Err(error) => (self.computer.status.clone(), Some(error)),
        };
        let changed =
            changed || status != self.computer.status || error != self.computer.error;
        self.computer.status = status;
        self.computer.error = error;
        changed
    }

    /// An Agent asked for the computer before setup was done.
    pub(super) fn computer_setup_needed(&mut self, event: diri_proto::ComputerSetupNeededEvent) {
        let installed = self
            .computer
            .status
            .as_ref()
            .is_some_and(diri_proto::ComputerStatus::installed);
        let title = crate::i18n::tf("toast.computer_setup_needed", &[("agent", &event.badge)]);
        let detail = if installed {
            crate::i18n::t("toast.computer_setup_needed_permissions")
        } else {
            crate::i18n::t("toast.computer_setup_needed_install")
        };
        self.emit(StoreEffect::StatusTransition(StatusTransition {
            dismiss: Vec::new(),
            sound: None,
            notification: None,
            in_app_banner: Some(
                crate::toast::Toast::new(crate::toast::ToastTone::Warning, title)
                    .detail(detail)
                    .action(
                        crate::i18n::t("toast.computer_setup_action"),
                        crate::toast::ToastCommand::OpenComputerSetup,
                    ),
            ),
        }));
    }
}

/// Polls while watched, then stops: nothing runs when nobody is looking.
pub(super) async fn watch(
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
) {
    loop {
        let status = client
            .computer_status(true)
            .await
            .map_err(|error| error.to_string());
        let keep_going = {
            let mut locked = store.write().expect("session store lock poisoned");
            let installing = status.as_ref().is_ok_and(|status| status.installing);
            let changed = locked.set_computer_status(status);
            let watched = locked
                .computer
                .watch_until
                .is_some_and(|until| until > Instant::now());
            // An install keeps itself watched, so its end is never missed.
            let keep_going = watched || installing || locked.computer.busy;
            if !keep_going {
                locked.computer.polling = false;
                locked.computer.watch_until = None;
            }
            drop(locked);
            if changed {
                let _ = change_tx.send(());
            }
            keep_going
        };
        if !keep_going {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The prompt an Agent's first foreground action shows under Ask.
pub fn foreground_toast(event: &diri_proto::ComputerForegroundRequestedEvent) -> crate::toast::Toast {
    crate::toast::Toast::new(
        crate::toast::ToastTone::Warning,
        crate::i18n::tf("toast.computer_foreground", &[("agent", &event.badge)]),
    )
    .detail(crate::i18n::t("toast.computer_foreground_detail"))
    .action(
        crate::i18n::t("toast.computer_foreground_allow"),
        crate::toast::ToastCommand::AnswerComputerForeground {
            session_id: event.session_id.clone(),
            allow: true,
        },
    )
    .action(
        crate::i18n::t("toast.computer_foreground_deny"),
        crate::toast::ToastCommand::AnswerComputerForeground {
            session_id: event.session_id.clone(),
            allow: false,
        },
    )
    // Shown until the Agent stops waiting.
    .hold(Duration::from_millis(event.timeout_ms))
}

/// Applies a status the Engine answered with outside the poller.
pub(super) fn apply_status(
    status: Result<diri_proto::ComputerStatus, String>,
    store: &Arc<RwLock<SessionStore>>,
    change_tx: &broadcast::Sender<()>,
) {
    let changed = store
        .write()
        .expect("session store lock poisoned")
        .set_computer_status(status);
    if changed {
        let _ = change_tx.send(());
    }
}

pub(super) async fn setup(
    action: diri_proto::ComputerSetupAction,
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
) {
    let status = client
        .computer_setup(action)
        .await
        .map_err(|error| error.to_string());
    let mut locked = store.write().expect("session store lock poisoned");
    locked.computer.busy = false;
    if status.is_err() {
        locked.computer.guide_after_install = false;
    }
    locked.set_computer_status(status);
    drop(locked);
    let _ = change_tx.send(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(installed: bool, grants: (bool, bool)) -> diri_proto::ComputerStatus {
        diri_proto::ComputerStatus {
            supported: true,
            version: installed.then(|| "0.36.1".to_owned()),
            tested_version: "0.36.1".to_owned(),
            accessibility: installed.then_some(grants.0),
            screen_recording: installed.then_some(grants.1),
            ..Default::default()
        }
    }

    fn effects(rx: &mut mpsc::UnboundedReceiver<StoreEffect>) -> Vec<StoreEffect> {
        let mut out = Vec::new();
        while let Ok(effect) = rx.try_recv() {
            out.push(effect);
        }
        out
    }

    #[test]
    fn watching_starts_one_poller_and_set_up_guides_once_installed() {
        let (mut store, mut rx) = SessionStore::headless(Prefs::default());
        store.watch_computer(Duration::from_secs(4));
        store.watch_computer(Duration::from_secs(4));
        let polls = effects(&mut rx)
            .into_iter()
            .filter(|effect| matches!(effect, StoreEffect::WatchComputer))
            .count();
        assert_eq!(polls, 1, "one poller however often the section paints");

        store.set_computer_status(Ok(status(false, (false, false))));
        store.computer_setup(diri_proto::ComputerSetupAction::Install);
        assert!(effects(&mut rx).iter().any(|effect| matches!(
            effect,
            StoreEffect::ComputerSetup(diri_proto::ComputerSetupAction::Install)
        )));
        assert!(store.computer_busy());
        // A second click while the first is answered sends nothing.
        store.computer_setup(diri_proto::ComputerSetupAction::Install);
        assert!(effects(&mut rx).is_empty());

        // Installing: no guide yet.
        store.computer.busy = false;
        let mut installing = status(false, (false, false));
        installing.installing = true;
        store.set_computer_status(Ok(installing));
        assert!(!store.take_guide_after_install());
        // Installed, daemon still starting: wait for the grants.
        let mut starting = status(true, (false, false));
        starting.accessibility = None;
        starting.screen_recording = None;
        store.set_computer_status(Ok(starting));
        assert!(!store.take_guide_after_install());
        // Installed: the guide opens once.
        store.set_computer_status(Ok(status(true, (false, false))));
        assert!(store.take_guide_after_install());
        assert!(!store.take_guide_after_install());
    }

    #[test]
    fn a_failed_install_opens_no_guide() {
        let (mut store, _rx) = SessionStore::headless(Prefs::default());
        store.computer_setup(diri_proto::ComputerSetupAction::Install);
        store.computer.busy = false;
        let mut failed = status(false, (false, false));
        failed.install_error = Some("Couldn't reach the download.".into());
        store.set_computer_status(Ok(failed));
        assert!(!store.take_guide_after_install());
        // Installed later by other means: still no surprise guide.
        store.set_computer_status(Ok(status(true, (false, false))));
        assert!(!store.take_guide_after_install());
    }

    #[test]
    fn an_unreachable_engine_keeps_the_last_status() {
        let (mut store, _rx) = SessionStore::headless(Prefs::default());
        store.set_computer_status(Ok(status(true, (true, true))));
        assert!(store.set_computer_status(Err("disconnected".into())));
        assert!(store.computer_status().is_some_and(diri_proto::ComputerStatus::ready));
        assert_eq!(store.computer_status_error(), Some("disconnected"));
    }

    #[test]
    fn an_agent_asking_early_gets_a_set_up_toast() {
        let (mut store, mut rx) = SessionStore::headless(Prefs::default());
        store.handle_event(EventEnvelope {
            name: diri_proto::EventName::COMPUTER_SETUP_NEEDED.into(),
            params: serde_json::json!({"sessionID": "s1", "badge": "Codex · signup flow"}),
            seq: 1,
        });
        let toast = effects(&mut rx)
            .into_iter()
            .find_map(|effect| match effect {
                StoreEffect::StatusTransition(transition) => transition.in_app_banner,
                _ => None,
            })
            .expect("a toast");
        assert!(toast.message.contains("Codex · signup flow"), "{}", toast.message);
        assert_eq!(
            toast.actions[0].command,
            crate::toast::ToastCommand::OpenComputerSetup
        );
    }

    #[test]
    fn a_foreground_request_asks_and_the_answer_reaches_the_engine() {
        let (mut store, mut rx) = SessionStore::headless(Prefs::default());
        store.handle_event(EventEnvelope {
            name: diri_proto::EventName::COMPUTER_FOREGROUND_REQUESTED.into(),
            params: serde_json::json!({"sessionID": "s1", "badge": "Codex · mail", "timeoutMs": 60000}),
            seq: 1,
        });
        let transition = effects(&mut rx)
            .into_iter()
            .find_map(|effect| match effect {
                StoreEffect::StatusTransition(transition) => Some(transition),
                _ => None,
            })
            .expect("a prompt");
        assert!(transition.notification.is_some(), "the person may be in another app");
        let toast = transition.in_app_banner.expect("a toast");
        assert!(toast.message.contains("Codex · mail"), "{}", toast.message);
        assert_eq!(toast.hold, Some(Duration::from_secs(60)));
        let session_id = SessionId("s1".into());
        assert_eq!(
            toast.actions[0].command,
            crate::toast::ToastCommand::AnswerComputerForeground {
                session_id: session_id.clone(),
                allow: true
            }
        );
        assert_eq!(
            toast.actions[1].command,
            crate::toast::ToastCommand::AnswerComputerForeground {
                session_id: session_id.clone(),
                allow: false
            }
        );
        store.answer_computer_foreground(session_id, true);
        assert!(effects(&mut rx).iter().any(|effect| matches!(
            effect,
            StoreEffect::AnswerComputerForeground(params) if params.allow
        )));
    }

    #[test]
    fn foreground_marks_follow_the_engine_and_revoke_clears_them() {
        let (mut store, mut rx) = SessionStore::headless(Prefs::default());
        let a = SessionId("a".into());
        let b = SessionId("b".into());
        store.handle_event(EventEnvelope {
            name: diri_proto::EventName::COMPUTER_FOREGROUND_CHANGED.into(),
            params: serde_json::json!({"sessions": ["b", "a"]}),
            seq: 1,
        });
        assert!(store.computer_foreground_held(&a));
        assert_eq!(store.computer_foreground_sessions(), [a.clone(), b.clone()]);

        store.revoke_computer_foreground(Some(a.clone()));
        assert!(!store.computer_foreground_held(&a));
        assert!(store.computer_foreground_held(&b));
        store.revoke_computer_foreground(None);
        assert!(store.computer_foreground_sessions().is_empty());
        let revokes: Vec<Option<SessionId>> = effects(&mut rx)
            .into_iter()
            .filter_map(|effect| match effect {
                StoreEffect::RevokeComputerForeground(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(revokes, [Some(a), None]);

        // A status read (on connect, or from Settings) replaces the marks.
        let mut status = status(true, (true, true));
        status.foreground = diri_proto::ComputerForeground::Ask;
        status.foreground_sessions = vec![b.clone()];
        assert!(store.set_computer_status(Ok(status)));
        assert_eq!(store.computer_foreground_sessions(), [b]);

        store.set_computer_foreground(diri_proto::ComputerForeground::Always);
        assert_eq!(
            store.computer_status().map(|status| status.foreground),
            Some(diri_proto::ComputerForeground::Always)
        );
        assert!(effects(&mut rx).iter().any(|effect| matches!(
            effect,
            StoreEffect::ConfigureComputer(params)
                if params.foreground == diri_proto::ComputerForeground::Always
        )));
    }
}
