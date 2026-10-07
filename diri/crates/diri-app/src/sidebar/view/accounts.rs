//! The accounts part of the bottom-left menu, kept to one line per account:
//! a click switches every open tab of its Agent to it, a checkmark marks the
//! login new tabs use. Who each account is and how much room it has live in
//! Settings › Accounts.
use super::*;
use diri_proto::{AgentAccountOverview, AgentAccountProfile, AgentKind};

/// What the account menu asks the Engine to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountRequest {
    /// Re-read the accounts (the menu opened).
    Refresh,
    /// Switch every open tab of the profile's Agent to it.
    Switch(String),
    /// Open the sign-in tab of a saved profile.
    SignIn(String),
}

#[derive(Default)]
pub(super) struct MenuAccounts {
    overview: AgentAccountOverview,
    loaded: bool,
    busy: bool,
    message: Option<String>,
    failed: bool,
}

impl MenuAccounts {
    #[cfg(test)]
    pub(super) fn overview_mut(&mut self) -> &mut AgentAccountOverview {
        &mut self.overview
    }

    pub(super) fn new(preview: bool) -> Self {
        let mut state = Self::default();
        if preview {
            state.loaded = true;
            state.overview = crate::usage::limits::preview_overview();
        }
        state
    }
}

enum Outcome {
    Switched(diri_proto::SwitchAccountResult),
    Opened(Box<diri_proto::SessionRecord>),
    Refreshed,
}

fn switchable(profile: &AgentAccountProfile) -> bool {
    profile.host.is_none()
        && matches!(
            profile.agent.as_str(),
            AgentKind::CODEX_ID | AgentKind::CLAUDE_CODE_ID
        )
}

impl Sidebar {
    pub(crate) fn account_menu_action(
        &mut self,
        request: AccountRequest,
        services: Arc<crate::AppServices>,
        cx: &mut Context<Self>,
    ) {
        if self.accounts.busy || self.preview {
            return;
        }
        self.accounts.busy = true;
        if request != AccountRequest::Refresh {
            self.accounts.failed = false;
            self.accounts.message = match &request {
                AccountRequest::Switch(_) => Some(t("sidebar.account.switching").into()),
                _ => None,
            };
        }
        let client = services.store.client().clone();
        let runtime = services.tokio.clone();
        let previous = self.accounts.overview.clone();
        cx.spawn(async move |this, cx| {
            let request_kind = request.clone();
            let result = runtime
                .spawn(async move {
                    client
                        .wait_until_connected(Duration::from_secs(5))
                        .await
                        .map_err(|e| e.to_string())?;
                    let outcome = match request {
                        AccountRequest::Refresh => Outcome::Refreshed,
                        AccountRequest::Switch(id) => Outcome::Switched(
                            client
                                .switch_all_accounts(id)
                                .await
                                .map_err(|e| e.to_string())?,
                        ),
                        AccountRequest::SignIn(id) => {
                            let claude = previous
                                .catalog
                                .profiles
                                .iter()
                                .any(|p| p.id == id && p.agent == AgentKind::CLAUDE_CODE_ID);
                            Outcome::Opened(Box::new(
                                if claude {
                                    client.login_claude_account(id).await
                                } else {
                                    client.login_codex_account(id).await
                                }
                                .map_err(|e| e.to_string())?,
                            ))
                        }
                    };
                    // A refresh failure must not hide a completed action.
                    let overview = client.account_overview().await.map_err(|e| e.to_string());
                    Ok::<_, String>((outcome, overview))
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.accounts.busy = false;
                match result {
                    Ok((outcome, overview)) => {
                        this.account_outcome(outcome, &services, cx);
                        match overview {
                            Ok(overview) => {
                                this.accounts.overview = overview;
                                this.accounts.loaded = true;
                            }
                            Err(error) => {
                                this.accounts.failed = true;
                                let message = this.accounts.message.get_or_insert_default();
                                if !message.is_empty() {
                                    message.push('\n');
                                }
                                message.push_str(&tf(
                                    "sidebar.account.refresh_failed",
                                    &[("error", &error)],
                                ));
                            }
                        }
                        if request_kind != AccountRequest::Refresh {
                            cx.emit(SidebarEvent::RefreshUsageLimits);
                        }
                    }
                    Err(error) => {
                        this.accounts.failed = true;
                        this.accounts.message = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn account_outcome(
        &mut self,
        outcome: Outcome,
        services: &Arc<crate::AppServices>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Outcome::Refreshed => {}
            Outcome::Switched(result) => {
                let count = result.switched.len();
                let unchanged = result.unchanged.len();
                let deferred = result.deferred.len();
                let mut store = self.store.write().expect("session store lock poisoned");
                for record in result.switched {
                    store.upsert_session(record);
                }
                let mut errors = result
                    .failures
                    .iter()
                    .map(|failure| {
                        let label = store
                            .sessions()
                            .get(&failure.session_id)
                            .map(|s| s.title.as_str())
                            .unwrap_or(&failure.session_id.0);
                        format!("{label}: {}", failure.message)
                    })
                    .collect::<Vec<_>>();
                drop(store);
                if let Some(error) = result.default_error {
                    errors.push(error);
                }
                self.accounts.failed = !errors.is_empty() || !result.default_changed;
                self.accounts.message = Some(if self.accounts.failed {
                    tf(
                        "sidebar.account.switched_with_errors",
                        &[("count", &count), ("errors", &errors.join("\n"))],
                    )
                } else if deferred > 0 {
                    tf(
                        "sidebar.account.switched_deferred",
                        &[
                            ("count", &count),
                            ("unchanged", &unchanged),
                            ("deferred", &deferred),
                        ],
                    )
                } else {
                    tf(
                        "sidebar.account.switched",
                        &[("count", &count), ("unchanged", &unchanged)],
                    )
                });
                services.store.publish_local_change();
            }
            Outcome::Opened(record) => {
                // The sign-in tab: show it and let the user type in it.
                let id = record.id.clone();
                {
                    let mut store = self.store.write().expect("session store lock poisoned");
                    store.upsert_session(*record);
                    store.select(id);
                }
                services.store.publish_local_change();
                self.accounts.message = None;
                self.ui.popover = None;
                cx.emit(SidebarEvent::SessionActivated);
            }
        }
    }

    /// Whether the account list has been read since launch.
    pub(super) fn accounts_loaded(&self) -> bool {
        self.accounts.loaded
    }

    /// One line per saved local account, then managing them.
    pub(super) fn account_switch_menu(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.accounts.busy;
        let overview = &self.accounts.overview;
        let mut section = div()
            .id("account-switcher")
            .debug_selector(|| "account-switcher".into())
            .flex()
            .flex_col()
            .py(px(3.0));
        for profile in overview.catalog.profiles.iter().filter(|p| switchable(p)) {
            let id = profile.id.clone();
            let agent = if profile.agent == AgentKind::CODEX_ID {
                "Codex"
            } else {
                "Claude"
            };
            let live = overview.is_live(profile);
            // A click switches to the account, or signs in one with no login.
            let action = (!live).then(|| {
                if overview.login(&profile.id).is_some_and(|l| l.signed_in) {
                    AccountRequest::Switch(profile.id.clone())
                } else {
                    AccountRequest::SignIn(profile.id.clone())
                }
            });
            section = section.child(
                div()
                    .id(SharedString::from(format!("switch-account-{id}")))
                    .debug_selector(move || format!("switch-account-{id}"))
                    .mx(px(6.0))
                    .px(px(8.0))
                    .h(px(ACCOUNT_MENU_ACTION_ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(SIDEBAR_MENU_ROW_RADIUS))
                    .when(!busy && action.is_some(), |row| {
                        row.cursor_pointer().glass_menu_row(colors, false)
                    })
                    .text_size(px(Typo::ROW.size))
                    .text_color(if busy {
                        colors.tertiary
                    } else {
                        colors.primary
                    })
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(profile.label.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(agent),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.0))
                            .flex()
                            .justify_center()
                            .when(live, |slot| {
                                slot.child(sf_symbol("checkmark", 9.0, colors.secondary))
                            }),
                    )
                    .when_some(action.filter(|_| !busy), |row, action| {
                        row.on_click(cx.listener(move |this, _, _, cx| {
                            if !this.accounts.busy {
                                cx.emit(SidebarEvent::AccountAction(action.clone()));
                            }
                        }))
                    }),
            );
        }
        if !self.accounts.loaded && !self.accounts.failed {
            section = section.child(menu_note(t("sidebar.account.loading"), colors));
        }
        if let Some(message) = &self.accounts.message {
            section = section.child(
                div()
                    .id("account-switch-status")
                    .px(px(14.0))
                    .py(px(3.0))
                    .text_size(px(Typo::META.size))
                    .text_color(if self.accounts.failed {
                        Ink::DANGER
                    } else {
                        colors.secondary
                    })
                    .child(message.clone()),
            );
        }
        section
            .child(
                div()
                    .id("manage-accounts")
                    .debug_selector(|| "manage-accounts".into())
                    .mx(px(6.0))
                    .px(px(8.0))
                    .h(px(ACCOUNT_MENU_ACTION_ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .rounded(px(SIDEBAR_MENU_ROW_RADIUS))
                    .cursor_pointer()
                    .glass_menu_row(colors, false)
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.secondary)
                    .child(t("sidebar.account.manage"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.popover = None;
                        cx.emit(SidebarEvent::ManageAccounts);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}
