use super::*;
use diri_proto::{AgentAccountCatalog, AgentAccountProfile};
use gpui::Role;

#[derive(Default)]
pub(super) struct AccountsState {
    catalog: AgentAccountCatalog,
    loaded: bool,
    busy: bool,
    error: Option<String>,
    editor: Option<ProfileEditor>,
    sequence: u64,
    continue_session: Option<diri_proto::SessionId>,
    continue_highlight: usize,
    continuing: bool,
}

struct ProfileEditor {
    profile: AgentAccountProfile,
    name: QueryEditor,
    path: QueryEditor,
    path_active: bool,
}

enum AccountAction {
    Refresh,
    Save(AgentAccountProfile),
    Remove(String),
}

impl UtilitySurfaces {
    pub(crate) fn open_account_continuation(
        &mut self,
        id: diri_proto::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.accounts.continuing {
            return;
        }
        self.open_settings(cx);
        self.accounts.continue_session = Some(id);
        self.accounts.continue_highlight = 0;
        self.accounts.editor = None;
        self.open_settings_tab(SettingsTab::Accounts, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn clear_account_continuation(&mut self) {
        self.accounts.continue_session = None;
    }

    fn continuation_source(&self) -> Option<Arc<diri_proto::SessionRecord>> {
        let id = self.accounts.continue_session.as_ref()?;
        self.store.read().ok()?.sessions().get(id).cloned()
    }

    fn continuation_choices(&self) -> Vec<AgentAccountProfile> {
        let Some(source) = self.continuation_source() else {
            return Vec::new();
        };
        self.accounts
            .catalog
            .profiles
            .iter()
            .filter(|profile| {
                profile.agent == "claude-code"
                    && profile.host == source.host
                    && source
                        .account_profile
                        .as_ref()
                        .is_none_or(|current| current.id != profile.id)
            })
            .cloned()
            .collect()
    }

    fn continue_account(&mut self, profile_id: String, cx: &mut Context<Self>) {
        if self.accounts.busy {
            return;
        }
        let Some(id) = self.accounts.continue_session.clone() else {
            return;
        };
        self.accounts.busy = true;
        self.accounts.continuing = true;
        self.accounts.error = None;
        let runtime = Arc::clone(&self.runtime);
        let client = Arc::clone(self.store_runtime.client());
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client.continue_with_account(&id, profile_id).await
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.accounts.busy = false;
                this.accounts.continuing = false;
                match result {
                    Ok(record) => {
                        let id = record.id.clone();
                        let return_to_session =
                            this.accounts.continue_session.as_ref() == Some(&id);
                        {
                            let mut store =
                                this.store.write().expect("session store lock poisoned");
                            store.upsert_session(record);
                            if return_to_session {
                                store.select(id);
                            }
                        }
                        this.store_runtime.publish_local_change();
                        if return_to_session {
                            this.close_surface(cx);
                        }
                    }
                    Err(error) => this.accounts.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn continue_account_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.settings_colors();
        let source = self.continuation_source();
        let mut content = div().flex().flex_col().gap(px(16.0))
            .child(div().text_size(px(14.0)).child(source.as_ref().map_or("Session unavailable".to_owned(), |s| s.title.clone())))
            .child(div().text_size(px(12.0)).text_color(colors.secondary).child("Choose a signed-in Claude account on the same machine. Diri restarts Claude with this conversation and keeps your working files in place."));
        if let Some(source) = &source {
            content = content.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(format!(
                        "Current account: {}",
                        source
                            .account_profile
                            .as_ref()
                            .map_or("CLI account", |p| p.label.as_str())
                    )),
            );
        }
        if let Some(error) = &self.accounts.error {
            content = content.child(
                div()
                    .id("account-continuation-error")
                    .text_size(px(12.0))
                    .text_color(Ink::DANGER)
                    .child(error.clone()),
            );
        }
        if self.accounts.busy {
            content = content.child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(if self.accounts.continuing {
                        "Saving conversation and switching account…"
                    } else {
                        "Loading accounts…"
                    }),
            );
        }
        let choices = self.continuation_choices();
        if choices.is_empty() && self.accounts.loaded && !self.accounts.busy {
            content = content.child(div().text_size(px(12.0)).child("No other Claude account is set up for this machine. Add a profile and sign in through Open Agent, then return here."));
        }
        for (index, profile) in choices.into_iter().enumerate() {
            let id = profile.id.clone();
            content = content.child(
                div()
                    .p(px(14.0))
                    .rounded(px(Radius::PANEL))
                    .border_1()
                    .border_color(colors.primary.alpha(
                        if self.accounts.continue_highlight == index {
                            0.25
                        } else {
                            0.09
                        },
                    ))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(14.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(div().text_size(px(13.0)).child(profile.label.clone()))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(colors.tertiary)
                                    .text_ellipsis()
                                    .child(profile.config_home),
                            ),
                    )
                    .child(self.account_button(
                        format!("continue-account-{id}"),
                        format!("Continue with {}", profile.label),
                        cx,
                        move |this, _, cx| this.continue_account(id.clone(), cx),
                    )),
            );
        }
        content = content.child(
            div()
                .flex()
                .gap(px(8.0))
                .child(self.account_button(
                    "manage-continuation-accounts",
                    "Manage accounts",
                    cx,
                    |this, _, cx| {
                        this.clear_account_continuation();
                        this.accounts.error = None;
                        this.refresh_accounts(cx);
                        cx.notify();
                    },
                ))
                .child(self.account_button(
                    "cancel-account-continuation",
                    "Back to session",
                    cx,
                    |this, _, cx| this.close_surface(cx),
                )),
        );
        settings_page("Continue with another account", content, colors).into_any_element()
    }

    #[cfg(test)]
    pub(super) fn seed_account_preview(&mut self, editor: bool) {
        let profile = AgentAccountProfile {
            id: "work".into(),
            label: "Work".into(),
            agent: "codex".into(),
            host: None,
            config_home: "~/.codex-work".into(),
            is_default: true,
        };
        self.accounts = AccountsState {
            loaded: true,
            catalog: AgentAccountCatalog {
                profiles: vec![profile.clone()],
            },
            ..Default::default()
        };
        if editor {
            self.accounts.editor = Some(ProfileEditor {
                name: text_editor(&profile.label),
                path: text_editor(&profile.config_home),
                profile,
                path_active: false,
            });
        }
    }

    #[cfg(test)]
    pub(super) fn seed_account_handoff_preview(&mut self) {
        let mut source =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical)
                .list
                .sessions
                .into_iter()
                .find(|s| s.kind == diri_proto::AgentKind::CLAUDE_CODE)
                .unwrap();
        source.title = "Finish account settings".into();
        source.agent_session_id = Some("preview-conversation".into());
        source.host = None;
        let work = AgentAccountProfile {
            id: "work".into(),
            label: "Work".into(),
            agent: "claude-code".into(),
            host: None,
            config_home: "~/.claude-work".into(),
            is_default: true,
        };
        let personal = AgentAccountProfile {
            id: "personal".into(),
            label: "Personal".into(),
            config_home: "~/.claude-personal".into(),
            is_default: false,
            ..work.clone()
        };
        source.account_profile = Some(work.clone());
        self.accounts = AccountsState {
            loaded: true,
            continue_session: Some(source.id.clone()),
            catalog: AgentAccountCatalog {
                profiles: vec![work, personal],
            },
            ..Default::default()
        };
        self.store.write().unwrap().upsert_session(source);
    }

    pub(super) fn refresh_accounts(&mut self, cx: &mut Context<Self>) {
        if !self.accounts.busy {
            self.account_action(AccountAction::Refresh, cx);
        }
    }

    fn account_action(&mut self, action: AccountAction, cx: &mut Context<Self>) {
        if self.accounts.busy {
            return;
        }
        self.accounts.busy = true;
        self.accounts.error = None;
        self.accounts.sequence += 1;
        let sequence = self.accounts.sequence;
        let close_editor = !matches!(action, AccountAction::Refresh);
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    match action {
                        AccountAction::Refresh => client.account_profiles().await,
                        AccountAction::Save(profile) => client.save_account_profile(&profile).await,
                        AccountAction::Remove(id) => client.remove_account_profile(id).await,
                    }
                })
                .await;
            let result = result
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.accounts.sequence != sequence {
                    return;
                }
                this.accounts.busy = false;
                match result {
                    Ok(catalog) => {
                        this.accounts.catalog = catalog;
                        this.accounts.loaded = true;
                        if close_editor {
                            this.accounts.editor = None;
                        }
                    }
                    Err(error) => this.accounts.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn edit_account(
        &mut self,
        profile: Option<AgentAccountProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.accounts.busy {
            return;
        }
        let profile = profile.unwrap_or_else(|| AgentAccountProfile {
            id: format!(
                "profile-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            label: String::new(),
            agent: "codex".into(),
            host: None,
            config_home: "~/.codex-work".into(),
            is_default: false,
        });
        self.accounts.editor = Some(ProfileEditor {
            name: text_editor(&profile.label),
            path: text_editor(&profile.config_home),
            profile,
            path_active: false,
        });
        self.accounts.error = None;
        self.settings_search_active = false;
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn save_account(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.accounts.editor else {
            return;
        };
        let mut profile = editor.profile.clone();
        profile.label = editor.name.text().trim().into();
        profile.config_home = editor.path.text().trim().into();
        self.account_action(AccountAction::Save(profile), cx);
    }

    pub(super) fn handle_account_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.surface != Surface::Settings
            || self.settings_tab != SettingsTab::Accounts
            || self.settings_search_active
        {
            return false;
        }
        if self.accounts.continue_session.is_some() {
            if self.accounts.busy {
                return true;
            }
            let choices = self.continuation_choices();
            let count = choices.len();
            match event.keystroke.key.as_str() {
                "escape" => self.close_surface(cx),
                "down" | "tab" if count > 0 && !event.keystroke.modifiers.shift => {
                    self.accounts.continue_highlight =
                        (self.accounts.continue_highlight + 1) % count
                }
                "up" | "tab" if count > 0 => {
                    self.accounts.continue_highlight =
                        (self.accounts.continue_highlight + count - 1) % count
                }
                "enter" => {
                    if let Some(profile) = choices.get(self.accounts.continue_highlight) {
                        self.continue_account(profile.id.clone(), cx);
                    }
                }
                _ => return false,
            }
            cx.notify();
            return true;
        }
        if self.accounts.editor.is_none() {
            return false;
        }
        if self.accounts.busy {
            return true;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.accounts.editor = None,
            "tab" => {
                let editor = self.accounts.editor.as_mut().unwrap();
                editor.path_active = !editor.path_active;
            }
            "enter" => {
                self.save_account(cx);
                return true;
            }
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return false;
                };
                let editor = self.accounts.editor.as_mut().unwrap();
                let input = if editor.path_active {
                    &mut editor.path
                } else {
                    &mut editor.name
                };
                match edit {
                    Edit::Local(local) => {
                        input.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => query_editor::copy_selection(input, cx),
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(input, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            input.insert(&text);
                        }
                    }
                }
            }
        }
        cx.notify();
        true
    }

    pub(super) fn accounts_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.accounts.continue_session.is_some() {
            return self.continue_account_settings(cx);
        }
        let colors = self.settings_colors();
        let mut content = div().flex().flex_col().gap(px(16.0))
            .child(div().text_size(px(12.0)).text_color(colors.secondary).child("Choose which Claude or Codex account each new session uses. Profiles keep their own provider configuration on the machine where the Agent runs."))
            .child(div().flex().items_center().justify_between()
                .child(div().text_size(px(12.0)).text_color(colors.secondary).child(if self.accounts.busy { "Updating accounts…" } else { "Saved profiles" }))
                .child(self.account_button("add-account", "Add profile", cx, |this, window, cx| this.edit_account(None, window, cx))));
        if let Some(error) = &self.accounts.error {
            content = content.child(
                div()
                    .text_size(px(12.0))
                    .text_color(Ink::DANGER)
                    .child(error.clone())
                    .child(
                        self.account_button("retry-accounts", "Retry", cx, |this, _, cx| {
                            this.refresh_accounts(cx)
                        }),
                    ),
            );
        }
        if let Some(editor) = &self.accounts.editor {
            let mut form = div()
                .p(px(14.0))
                .rounded(px(Radius::PANEL))
                .border_1()
                .border_color(colors.primary.alpha(0.12))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::MEDIUM)
                        .child("Account profile"),
                );
            form = form.child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(self.account_button(
                        "profile-codex",
                        if editor.profile.agent == "codex" {
                            "✓ Codex"
                        } else {
                            "Codex"
                        },
                        cx,
                        |this, _, cx| {
                            if let Some(editor) = &mut this.accounts.editor {
                                editor.profile.agent = "codex".into();
                                if editor.path.text() == "~/.claude-work" {
                                    editor.path = text_editor("~/.codex-work");
                                }
                            }
                            cx.notify();
                        },
                    ))
                    .child(self.account_button(
                        "profile-claude",
                        if editor.profile.agent == "claude-code" {
                            "✓ Claude Code"
                        } else {
                            "Claude Code"
                        },
                        cx,
                        |this, _, cx| {
                            if let Some(editor) = &mut this.accounts.editor {
                                editor.profile.agent = "claude-code".into();
                                if editor.path.text() == "~/.codex-work" {
                                    editor.path = text_editor("~/.claude-work");
                                }
                            }
                            cx.notify();
                        },
                    )),
            );
            for (path, label, input) in [
                (false, "Name", &editor.name),
                (true, "Account directory", &editor.path),
            ] {
                let active = path == editor.path_active;
                form = form.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(colors.secondary)
                                .child(label),
                        )
                        .child(
                            div()
                                .id(if path { "account-path" } else { "account-name" })
                                .role(Role::TextInput)
                                .aria_label(label)
                                .h(px(34.0))
                                .px(px(10.0))
                                .rounded(px(Radius::BADGE))
                                .border_1()
                                .border_color(colors.primary.alpha(if active { 0.3 } else { 0.1 }))
                                .bg(colors.primary.alpha(0.04))
                                .flex()
                                .items_center()
                                .overflow_hidden()
                                .text_size(px(12.0))
                                .cursor(CursorStyle::IBeam)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(editor) = &mut this.accounts.editor {
                                        editor.path_active = path;
                                    }
                                    this.focus.focus(window, cx);
                                    cx.notify();
                                }))
                                .child(if active {
                                    query_label(input).into_any_element()
                                } else {
                                    div()
                                        .text_ellipsis()
                                        .child(input.text().to_owned())
                                        .into_any_element()
                                }),
                        ),
                );
            }
            let host_label = editor
                .profile
                .host
                .as_deref()
                .map(|id| {
                    self.hosts
                        .iter()
                        .find(|h| h.id == id)
                        .map_or(id, |h| h.display_name())
                })
                .unwrap_or("This Mac")
                .to_owned();
            form = form.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child("Run on"),
            );
            let mut hosts = div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .child(
                    self.account_button("account-local", "This Mac", cx, |this, _, cx| {
                        if let Some(editor) = &mut this.accounts.editor {
                            editor.profile.host = None;
                        }
                        cx.notify();
                    }),
                );
            for host in &self.hosts {
                let id = host.id.clone();
                hosts = hosts.child(self.account_button(
                    format!("account-host-{id}"),
                    host.display_name().to_owned(),
                    cx,
                    move |this, _, cx| {
                        if let Some(editor) = &mut this.accounts.editor {
                            editor.profile.host = Some(id.clone());
                        }
                        cx.notify();
                    },
                ));
            }
            form = form.child(hosts).child(div().text_size(px(11.0)).text_color(colors.secondary).child(format!("Selected: {host_label}. The directory is on this machine.")))
                .child(self.account_button("account-default", if editor.profile.is_default { "✓ Default for this Agent on this host" } else { "Use by default for this Agent on this host" }, cx, |this, _, cx| {
                    if let Some(editor) = &mut this.accounts.editor { editor.profile.is_default = !editor.profile.is_default; } cx.notify();
                }))
                .child(div().text_size(px(11.0)).text_color(colors.tertiary).child("Choose an existing account directory, or a new one for a separate login. Open Agent to complete provider sign-in. Diri stores the directory and label; credentials stay with the provider."))
                .child(div().flex().gap(px(8.0))
                    .child(self.account_button("save-account", "Save profile", cx, |this, _, cx| this.save_account(cx)))
                    .child(self.account_button("cancel-account", "Cancel", cx, |this, _, cx| { this.accounts.editor = None; cx.notify(); })));
            content = content.child(form);
        }
        if self.accounts.loaded
            && self.accounts.catalog.profiles.is_empty()
            && self.accounts.editor.is_none()
        {
            content = content.child(div().p(px(20.0)).rounded(px(Radius::PANEL)).bg(colors.primary.alpha(0.025)).flex().flex_col().gap(px(8.0))
                .child(div().text_size(px(14.0)).child("Work and personal, side by side"))
                .child(div().text_size(px(12.0)).text_color(colors.secondary).child("Add a named profile to select an account in the launcher. Without a profile, Diri uses your CLI’s current environment.")));
        }
        for profile in &self.accounts.catalog.profiles {
            let edit = profile.clone();
            let open = profile.clone();
            let remove = profile.id.clone();
            let host = profile
                .host
                .as_deref()
                .map(|id| {
                    self.hosts
                        .iter()
                        .find(|h| h.id == id)
                        .map_or(id, |h| h.display_name())
                })
                .unwrap_or("This Mac");
            content = content.child(
                div()
                    .p(px(14.0))
                    .border_1()
                    .border_color(colors.primary.alpha(0.09))
                    .rounded(px(Radius::PANEL))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!(
                                "{}{}",
                                profile.label,
                                if profile.is_default {
                                    " · Default"
                                } else {
                                    ""
                                }
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(format!(
                                "{} · {}",
                                if profile.agent == "codex" {
                                    "Codex"
                                } else {
                                    "Claude Code"
                                },
                                host
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(colors.tertiary)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(profile.config_home.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(self.account_button(
                                format!("open-{}", profile.id),
                                "Open Agent",
                                cx,
                                move |this, _, cx| {
                                    let kind = diri_proto::AgentKind::new(&open.agent);
                                    let mut store =
                                        this.store.write().expect("session store lock poisoned");
                                    let cwd = open
                                        .host
                                        .is_none()
                                        .then(|| store.local_fallback_directory());
                                    store.spawn_kind(
                                        kind,
                                        crate::store::SpawnOptions {
                                            account_profile_id: Some(open.id.clone()),
                                            host: open.host.clone(),
                                            cwd,
                                            ..Default::default()
                                        },
                                    );
                                    drop(store);
                                    this.close_surface(cx);
                                },
                            ))
                            .child(self.account_button(
                                format!("edit-{}", profile.id),
                                "Edit",
                                cx,
                                move |this, window, cx| {
                                    this.edit_account(Some(edit.clone()), window, cx)
                                },
                            ))
                            .child(self.account_button(
                                format!("remove-{}", profile.id),
                                "Remove profile",
                                cx,
                                move |this, _, cx| {
                                    this.account_action(AccountAction::Remove(remove.clone()), cx)
                                },
                            )),
                    ),
            );
        }
        content = content.child(div().text_size(px(11.0)).text_color(colors.tertiary).child("Editing or removing a profile affects future launches. Existing sessions retain their account. Removing a profile leaves the provider’s files and credentials intact."));
        settings_page("Accounts", content, colors).into_any_element()
    }

    fn account_button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = self.settings_colors();
        div()
            .id(id.into())
            .role(Role::Button)
            .h(px(30.0))
            .px(px(10.0))
            .rounded(px(Radius::BADGE))
            .text_size(px(11.0))
            .flex()
            .items_center()
            .bg(colors.primary.alpha(0.055))
            .text_color(if self.accounts.busy {
                colors.tertiary
            } else {
                colors.secondary
            })
            .when(!self.accounts.busy, |button| {
                button
                    .cursor_pointer()
                    .hover(move |s| s.bg(colors.primary.alpha(0.1)))
                    .active(move |s| s.bg(colors.primary.alpha(0.14)))
                    .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
            .child(label.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn continuation_picker_filters_agent_host_and_current_account(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (surfaces, cx) = cx.add_window_view(move |window, cx| {
            let mut surfaces =
                UtilitySurfaces::new(runtime, tokio, crate::updates::inert(), window, cx);
            surfaces.open_settings(cx);
            surfaces.settings_tab = SettingsTab::Accounts;
            surfaces.seed_account_handoff_preview();
            surfaces
        });
        surfaces.update_in(cx, |surfaces, _, cx| {
            let personal = surfaces.accounts.catalog.profiles[1].clone();
            surfaces.accounts.catalog.profiles.extend([
                AgentAccountProfile {
                    id: "remote".into(),
                    host: Some("server".into()),
                    ..personal.clone()
                },
                AgentAccountProfile {
                    id: "codex".into(),
                    agent: "codex".into(),
                    ..personal.clone()
                },
                AgentAccountProfile {
                    id: "third".into(),
                    ..personal
                },
            ]);
            let choices = surfaces.continuation_choices();
            assert_eq!(
                choices.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
                ["personal", "third"]
            );
            let key = |name| KeyDownEvent {
                keystroke: gpui::Keystroke::parse(name).unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            surfaces.handle_account_key(&key("tab"), cx);
            assert_eq!(surfaces.accounts.continue_highlight, 1);
            surfaces.handle_account_key(&key("shift-tab"), cx);
            assert_eq!(surfaces.accounts.continue_highlight, 0);
            surfaces.accounts.busy = true;
            surfaces.handle_account_key(&key("escape"), cx);
            assert!(surfaces.accounts.continue_session.is_some());
            surfaces.accounts.busy = false;
            surfaces.handle_account_key(&key("escape"), cx);
            assert!(surfaces.accounts.continue_session.is_none());
            assert_eq!(surfaces.surface, Surface::None);
        });
    }

    #[gpui::test]
    fn account_editor_keeps_unsaved_changes_local_and_blocks_input_during_save(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (surfaces, cx) = cx.add_window_view(move |window, cx| {
            let mut surfaces =
                UtilitySurfaces::new(runtime, tokio, crate::updates::inert(), window, cx);
            surfaces.open_settings(cx);
            surfaces.settings_tab = SettingsTab::Accounts;
            surfaces.seed_account_preview(true);
            surfaces
        });
        surfaces.update_in(cx, |surfaces, _, cx| {
            let key = |name| KeyDownEvent {
                keystroke: gpui::Keystroke::parse(name).unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            assert!(surfaces.handle_account_key(&key("tab"), cx));
            assert!(surfaces.accounts.editor.as_ref().unwrap().path_active);
            surfaces
                .accounts
                .editor
                .as_mut()
                .unwrap()
                .path
                .insert("-changed");
            assert_eq!(
                surfaces.accounts.catalog.profiles[0].config_home,
                "~/.codex-work"
            );
            surfaces.accounts.busy = true;
            surfaces.handle_account_key(&key("escape"), cx);
            assert!(surfaces.accounts.editor.is_some());
            surfaces.accounts.busy = false;
            surfaces.handle_account_key(&key("escape"), cx);
            assert!(surfaces.accounts.editor.is_none());
            assert_eq!(
                surfaces.accounts.catalog.profiles[0].config_home,
                "~/.codex-work"
            );
        });
    }
}
