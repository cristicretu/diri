use super::*;
use diri_proto::{AgentAccountCatalog, AgentAccountProfile};

#[derive(Default)]
pub(super) struct MenuAccounts {
    catalog: AgentAccountCatalog,
    loaded: bool,
    busy: bool,
    message: Option<String>,
    failed: bool,
}

impl MenuAccounts {
    pub(super) fn new(preview: bool) -> Self {
        let mut state = Self::default();
        if preview {
            state.loaded = true;
            state.catalog.profiles = ["Personal", "Work"]
                .into_iter()
                .enumerate()
                .map(|(i, label)| AgentAccountProfile {
                    id: format!("preview-{i}"),
                    label: label.into(),
                    agent: "codex".into(),
                    host: None,
                    config_home: String::new(),
                    is_default: i == 0,
                    login_store: None,
                })
                .collect();
        }
        state
    }
}

impl Sidebar {
    pub(crate) fn account_menu_action(
        &mut self,
        profile: Option<String>,
        services: Arc<crate::AppServices>,
        cx: &mut Context<Self>,
    ) {
        if self.accounts.busy || self.preview {
            return;
        }
        self.accounts.busy = true;
        if profile.is_some() {
            self.accounts.failed = false;
            self.accounts.message = Some("Switching login and resuming conversations…".into());
        }
        let client = services.store.client().clone();
        let runtime = services.tokio.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client
                        .wait_until_connected(Duration::from_secs(5))
                        .await
                        .map_err(|e| e.to_string())?;
                    let switched = if let Some(id) = profile {
                        Some(
                            client
                                .switch_all_accounts(id)
                                .await
                                .map_err(|e| e.to_string())?,
                        )
                    } else {
                        None
                    };
                    // A catalog refresh failure must not hide a completed switch.
                    let catalog = client.account_profiles().await.map_err(|e| e.to_string());
                    Ok::<_, String>((switched, catalog))
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.accounts.busy = false;
                match result {
                    Ok((switched, catalog)) => {
                        if let Some(result) = switched {
                            let count = result.switched.len();
                            let unchanged = result.unchanged.len();
                            let mut store =
                                this.store.write().expect("session store lock poisoned");
                            for record in result.switched {
                                store.upsert_session(record);
                            }
                            let deferred = result.deferred.len();
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
                            if let Some(error) = result.default_error {
                                errors.push(error);
                            }
                            this.accounts.failed = !errors.is_empty() || !result.default_changed;
                            this.accounts.message = Some(if this.accounts.failed {
                                format!("{count} conversations switched. {}", errors.join("\n"))
                            } else {
                                if deferred > 0 {
                                    format!("Switched {count} conversations; {unchanged} separate-home tabs unchanged; {deferred} unidentified tab(s) keep the previous login until restarted. Default account updated.")
                                } else {
                                    format!("Switched {count} conversations; {unchanged} separate-home tabs unchanged. Default account updated.")
                                }
                            });
                            drop(store);
                            services.store.publish_local_change();
                            cx.emit(SidebarEvent::RefreshUsageLimits);
                        }
                        match catalog {
                            Ok(catalog) => {
                                this.accounts.catalog = catalog;
                                this.accounts.loaded = true;
                            }
                            Err(error) => {
                                this.accounts.failed = true;
                                let message = this.accounts.message.get_or_insert_default();
                                message.push_str(&format!("\nCould not refresh accounts: {error}"));
                            }
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

    pub(super) fn account_switch_menu(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = div().id("account-switcher").debug_selector(|| "account-switcher".into()).flex().flex_col().py(px(3.0))
            .child(div().px(px(14.0)).text_size(px(Typo::META.size)).text_color(colors.tertiary).child("Switch open conversations"))
            .child(div().px(px(14.0)).py(px(4.0)).text_size(px(Typo::META.size)).text_color(colors.tertiary).child("Open Claude and Codex tabs on this Mac resume with the selected login. Local MCP setup stays; hosted connectors require per-account connections."));
        for profile in self
            .accounts
            .catalog
            .profiles
            .iter()
            .filter(|p| p.host.is_none() && matches!(p.agent.as_str(), "codex" | "claude-code"))
        {
            let id = profile.id.clone();
            let subtitle = format!(
                "{} · {}{}",
                if profile.agent == "codex" {
                    "Codex"
                } else {
                    "Claude"
                },
                profile.host.as_deref().unwrap_or("This Mac"),
                if profile.is_default {
                    " · Default"
                } else {
                    ""
                }
            );
            let busy = self.accounts.busy;
            section = section.child(
                div()
                    .id(SharedString::from(format!("switch-account-{id}")))
                    .debug_selector(move || format!("switch-account-{id}"))
                    .mx(px(6.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(SIDEBAR_MENU_ROW_RADIUS))
                    .flex()
                    .flex_col()
                    .when(!busy, |row| {
                        row.cursor_pointer()
                            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    })
                    .text_color(if busy {
                        colors.tertiary
                    } else {
                        colors.primary
                    })
                    .child(
                        div()
                            .text_size(px(Typo::ROW.size))
                            .child(profile.label.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(subtitle),
                    )
                    .on_click(cx.listener({
                        let id = profile.id.clone();
                        move |this, _, _, cx| {
                            if !this.accounts.busy {
                                cx.emit(SidebarEvent::AccountAction(Some(id.clone())));
                            }
                        }
                    })),
            );
        }
        if !self.accounts.loaded && !self.accounts.failed {
            section = section.child(
                div()
                    .px(px(14.0))
                    .py(px(5.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child("Loading accounts…"),
            );
        }
        if let Some(message) = &self.accounts.message {
            section = section.child(
                div()
                    .id("account-switch-status")
                    .px(px(14.0))
                    .py(px(5.0))
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
                    .py(px(7.0))
                    .rounded(px(SIDEBAR_MENU_ROW_RADIUS))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.secondary)
                    .child("Add or manage accounts…")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.popover = None;
                        cx.emit(SidebarEvent::ManageAccounts);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}
