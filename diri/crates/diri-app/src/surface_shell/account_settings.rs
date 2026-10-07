//! Settings › Accounts: every Claude Code and Codex account on this Mac, who
//! it is (email, plan), how much of each plan window it has used and when
//! that resets, and one click to switch, sign in, rename or remove it. Remote
//! and separate-directory profiles keep the full profile editor below.
use super::*;
use crate::tooltip_warmth::WarmTooltip;
use crate::usage::limits::AccountLimits;
#[cfg(test)]
use diri_proto::AgentAccountCatalog;
use diri_proto::{AgentAccountOverview, AgentAccountProfile, AgentKind};
use gpui::{Div, Hsla, Role};

#[derive(Default)]
pub(super) struct AccountsState {
    overview: AgentAccountOverview,
    loaded: bool,
    busy: bool,
    error: Option<String>,
    notice: Option<String>,
    editor: Option<ProfileEditor>,
    /// An account being renamed in place: its id and the name typed so far.
    renaming: Option<(String, QueryEditor)>,
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
    /// Save the login an Agent uses now.
    Adopt(String),
}

/// What opens a sign-in tab: a saved profile, or a new account of an Agent.
enum SignIn {
    Profile(AgentAccountProfile),
    New(&'static str),
}

fn agent_title(agent: &str) -> &'static str {
    if agent == AgentKind::CODEX_ID {
        "Codex"
    } else {
        "Claude Code"
    }
}

/// "max" → "Max", as the provider sells it.
fn plan_label(plan: &str) -> String {
    let mut chars = plan.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// "5-hour limit" → "5-hour"; the account above says what is limited.
fn window_title(label: &str) -> &str {
    label.strip_suffix(" limit").unwrap_or(label)
}

/// Compact time until a window resets: "3d 4h", "1h 12m", "9m".
fn until(seconds: i64) -> String {
    let minutes = (seconds / 60).max(1);
    let (days, hours) = (seconds / 86_400, seconds % 86_400 / 3_600);
    match (days, minutes / 60, minutes % 60) {
        (1.., _, _) if hours == 0 => format!("{days}d"),
        (1.., _, _) => format!("{days}d {hours}h"),
        (0, h @ 1.., 0) => format!("{h}h"),
        (0, h @ 1.., m) => format!("{h}h {m}m"),
        _ => format!("{minutes}m"),
    }
}

/// How much more room (percentage points) another account must have before
/// it is pointed out.
const ROOM_MARGIN: f64 = 20.0;

fn provider(agent: &str) -> &'static str {
    if agent == AgentKind::CODEX_ID {
        "Codex"
    } else {
        "Claude"
    }
}

/// The organization worth naming: not the "you@…'s Organization" that Claude
/// gives every personal account.
fn organization(identity: &diri_proto::AgentAccountIdentity) -> Option<String> {
    let organization = identity.organization.clone()?;
    let personal = identity
        .email
        .as_deref()
        .is_some_and(|email| organization.contains(email));
    (!personal).then_some(organization)
}

/// The leading slot: a checkmark on the account new tabs use.
fn account_check(live: bool, colors: SemanticColors) -> AnyElement {
    div()
        .flex_none()
        .w(px(14.0))
        .flex()
        .justify_center()
        .when(live, |slot| {
            slot.child(sf_symbol("checkmark", 10.0, colors.secondary))
        })
        .into_any_element()
}

/// A short status beside an account's name: "In use", "Most room", …
fn account_tag(label: &'static str, color: Hsla, colors: SemanticColors) -> AnyElement {
    div()
        .flex_none()
        .h(px(17.0))
        .px(px(6.0))
        .rounded(px(Radius::BADGE))
        .flex()
        .items_center()
        .bg(colors.primary.alpha(0.06))
        .text_size(px(10.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
        .into_any_element()
}

/// Every plan window of an account on one line: name, meter, use, and when
/// it resets. An idle account's last answer is dated.
fn plan_windows(usage: &AccountLimits, live: bool, now: i64, colors: SemanticColors) -> AnyElement {
    let mut line = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(14.0))
        .gap_y(px(4.0))
        .pt(px(2.0));
    for window in &usage.windows {
        let reset = window.resets_at.filter(|reset| *reset > now);
        let used = if window.resets_at.is_some_and(|reset| reset <= now) {
            0.0
        } else {
            window.used_percent
        };
        let fill = if used >= 90.0 {
            Hsla::from(Ink::DANGER)
        } else if used >= 75.0 {
            Hsla::from(Ink::ATTENTION)
        } else if live {
            Hsla::from(Palette::GEMINI_BLUE)
        } else {
            colors.tertiary.into()
        };
        line = line.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(window_title(&window.label).to_owned())
                .child(
                    div()
                        .w(px(44.0))
                        .h(px(3.0))
                        .rounded_full()
                        .bg(colors.primary.alpha(0.09))
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative((used / 100.0).clamp(0.0, 1.0) as f32))
                                .rounded_full()
                                .bg(fill),
                        ),
                )
                .child(
                    div()
                        .font_family(crate::fonts::mono_family())
                        .text_color(colors.secondary)
                        .child(format!("{used:.0}%")),
                )
                .when_some(reset, |row, reset| {
                    row.child(tf(
                        "settings.accounts.resets_in",
                        &[("time", &until(reset - now))],
                    ))
                }),
        );
    }
    if now - usage.checked_at > 600 {
        line = line.child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(tf(
                    "settings.accounts.as_of",
                    &[("time", &until(now - usage.checked_at))],
                )),
        );
    }
    line.into_any_element()
}

/// A local profile that shares the provider's home and switches only its
/// login: every open tab of that Agent follows it.
fn shares_login(profile: &AgentAccountProfile) -> bool {
    profile.host.is_none() && matches!(profile.agent.as_str(), "codex" | "claude-code")
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
            .overview
            .catalog
            .profiles
            .iter()
            .filter(|profile| {
                profile.agent == source.kind.id()
                    && profile.host == source.host
                    && source
                        .account_profile
                        .as_ref()
                        .is_none_or(|current| current.id != profile.id)
            })
            .cloned()
            .collect()
    }

    /// Open a sign-in tab and take the user there.
    fn login_account(&mut self, sign_in: SignIn, cx: &mut Context<Self>) {
        if self.accounts.busy {
            return;
        }
        self.accounts.busy = true;
        self.accounts.error = None;
        let runtime = Arc::clone(&self.runtime);
        let client = Arc::clone(self.store_runtime.client());
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    match sign_in {
                        SignIn::New(agent) => client.add_account(agent.into()).await,
                        SignIn::Profile(profile) if profile.agent == AgentKind::CLAUDE_CODE_ID => {
                            client.login_claude_account(profile.id).await
                        }
                        SignIn::Profile(profile) => client.login_codex_account(profile.id).await,
                    }
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.accounts.busy = false;
                match result {
                    Ok(record) => {
                        let mut store = this.store.write().expect("session store lock poisoned");
                        let id = record.id.clone();
                        store.upsert_session(record);
                        store.select(id);
                        drop(store);
                        this.store_runtime.publish_local_change();
                        this.close_surface(cx);
                        cx.emit(UtilitySurfacesEvent::AccountLoginOpened);
                    }
                    Err(e) => this.accounts.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn continue_claude_account(&mut self, profile_id: String, cx: &mut Context<Self>) {
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

    fn continue_account(&mut self, profile_id: String, cx: &mut Context<Self>) {
        if self
            .continuation_source()
            .is_some_and(|s| s.kind == diri_proto::AgentKind::CLAUDE_CODE)
        {
            self.continue_claude_account(profile_id, cx);
            return;
        }
        if self.accounts.busy {
            return;
        }
        self.accounts.busy = true;
        self.accounts.continuing = true;
        self.accounts.error = None;
        self.accounts.notice = None;
        let runtime = Arc::clone(&self.runtime);
        let client = Arc::clone(self.store_runtime.client());
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client.switch_all_accounts(profile_id).await
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.accounts.busy = false;
                this.accounts.continuing = false;
                match result {
                    Ok(result) => {
                        let switched = result.switched.len();
                        let unchanged = result.unchanged.len();
                        {
                            let mut store =
                                this.store.write().expect("session store lock poisoned");
                            for record in result.switched {
                                store.upsert_session(record);
                            }
                        }
                        this.store_runtime.publish_local_change();
                        if result.failures.is_empty() && result.default_changed {
                            this.accounts.error = None;
                            let mut notice = tf(
                                "settings.accounts.switched_notice",
                                &[("switched", &switched), ("unchanged", &unchanged)],
                            );
                            if !result.deferred.is_empty() {
                                notice.push_str(&tf(
                                    "settings.accounts.switched_deferred",
                                    &[("count", &result.deferred.len())],
                                ));
                            }
                            this.accounts.notice = Some(notice);
                            this.accounts.continue_session = None;
                            this.account_action(AccountAction::Refresh, cx);
                        } else {
                            let store = this.store.read().expect("session store lock poisoned");
                            let mut details = result
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
                                .collect::<Vec<_>>()
                                .join("\n");
                            if let Some(error) = result.default_error {
                                details.push('\n');
                                details.push_str(&tf(
                                    "settings.accounts.default_save_failed",
                                    &[("error", &error)],
                                ));
                            }
                            this.accounts.error = Some(format!(
                                "{}\n{details}",
                                tf(
                                    "settings.accounts.switched_partial",
                                    &[("switched", &switched)]
                                )
                            ));
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
        let heading = source.as_ref().map_or_else(
            || t("settings.accounts.session_unavailable").to_owned(),
            |source| {
                let count = self
                    .store
                    .read()
                    .map(|store| {
                        let open = store
                            .workspace_catalog()
                            .snapshot()
                            .map(|s| s.open_session_ids())
                            .unwrap_or_default();
                        store
                            .sessions()
                            .values()
                            .filter(|session| {
                                session.kind == source.kind
                                    && session.host == source.host
                                    && !session.is_archived()
                                    && open.contains(&session.id)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                let agent = if source.kind == diri_proto::AgentKind::CODEX {
                    "Codex"
                } else {
                    "Claude"
                };
                tf(
                    if count == 1 {
                        "settings.accounts.conversations_one"
                    } else {
                        "settings.accounts.conversations_other"
                    },
                    &[("count", &count), ("agent", &agent)],
                )
            },
        );
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(div().text_size(px(14.0)).child(heading))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(t("settings.accounts.continue_detail")),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(t("settings.accounts.continue_warning")),
            );
        if let Some(source) = &source {
            content = content.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(tf(
                        "settings.accounts.current_account",
                        &[(
                            "account",
                            &source
                                .account_profile
                                .as_ref()
                                .map_or(t("settings.accounts.cli_account"), |p| p.label.as_str()),
                        )],
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
                        t("settings.accounts.switching")
                    } else {
                        t("settings.accounts.loading")
                    }),
            );
        }
        let choices = self.continuation_choices();
        if choices.is_empty() && self.accounts.loaded && !self.accounts.busy {
            content = content.child(
                div()
                    .text_size(px(12.0))
                    .child(t("settings.accounts.no_other_account")),
            );
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
                        tf(
                            "settings.accounts.switch_to",
                            &[("account", &profile.label)],
                        ),
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
                    t("settings.accounts.manage"),
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
                    t("settings.accounts.back_to_session"),
                    cx,
                    |this, _, cx| this.close_surface(cx),
                )),
        );
        settings_page(t("settings.accounts.switch_title"), content, colors).into_any_element()
    }

    /// Two Claude and two Codex accounts with limits, plus a remote profile
    /// (opened in the editor when `editor`).
    #[cfg(test)]
    pub(super) fn seed_account_preview(&mut self, editor: bool) {
        let remote = AgentAccountProfile {
            id: "build-box".into(),
            label: "Build box".into(),
            agent: "codex".into(),
            host: Some("devbox".into()),
            config_home: "~/.codex".into(),
            is_default: true,
            login_store: None,
        };
        let mut overview = crate::usage::limits::preview_overview();
        overview.catalog.profiles.push(remote.clone());
        self.accounts = AccountsState {
            loaded: true,
            overview,
            ..Default::default()
        };
        self.usage.limits = crate::usage::limits::preview();
        if editor {
            self.accounts.editor = Some(ProfileEditor {
                name: text_editor(&remote.label),
                path: text_editor(&remote.config_home),
                profile: remote,
                path_active: false,
            });
        }
    }

    /// Every state an account can be in besides "fine": its login revoked,
    /// a sign-in tab still open, never signed in, a full window that reset
    /// since, and an idle account showing its last answer.
    #[cfg(test)]
    pub(super) fn seed_account_states_preview(&mut self) {
        let now = crate::usage::Clock::read(&crate::usage::SystemClock).unix_seconds;
        self.seed_account_preview(false);
        let profiles = &mut self.accounts.overview.catalog.profiles;
        profiles.retain(|p| p.host.is_none());
        let template = profiles[1].clone();
        for (id, label, agent) in [
            ("preview-4", "Client", "claude-code"),
            ("preview-5", "Weekend", "codex"),
        ] {
            profiles.push(AgentAccountProfile {
                id: id.into(),
                label: label.into(),
                agent: agent.into(),
                is_default: false,
                ..template.clone()
            });
        }
        let logins = &mut self.accounts.overview.logins;
        let mut client = logins[1].clone();
        client.profile_id = "preview-4".into();
        client.identity.email = Some("alex@client.example".into());
        client.signing_in = true;
        client.signed_in = false;
        let mut weekend = logins[3].clone();
        weekend.profile_id = "preview-5".into();
        weekend.identity = Default::default();
        weekend.signed_in = false;
        logins.extend([client, weekend]);
        // Revoked: the provider refused a token that had not expired.
        let personal = &mut self.usage.limits[1];
        personal.windows.clear();
        personal.error = Some(crate::usage::limits::SIGN_IN_AGAIN);
        // Idle since this morning: its full 5-hour window has reset since.
        let mut windows = self.usage.limits[2].windows.clone();
        windows[0].used_percent = 100.0;
        windows[0].resets_at = Some(now - 1_800);
        windows[1].used_percent = 62.0;
        let side = &mut self.usage.limits[3];
        side.checked_at = now - 4 * 3_600;
        side.windows = windows;
    }

    /// First run: nothing saved in Diri yet, both logins in use.
    #[cfg(test)]
    pub(super) fn seed_account_first_run_preview(&mut self) {
        let mut overview = crate::usage::limits::first_run_overview();
        overview.catalog.profiles.clear();
        self.accounts = AccountsState {
            loaded: true,
            overview,
            ..Default::default()
        };
        self.usage.limits = crate::usage::limits::preview()
            .into_iter()
            .filter(|l| l.live)
            .map(|mut l| {
                l.profile_id = None;
                l
            })
            .collect();
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
            login_store: None,
        };
        let personal = AgentAccountProfile {
            id: "personal".into(),
            label: "Personal".into(),
            config_home: "~/.claude-personal".into(),
            is_default: false,
            login_store: None,
            ..work.clone()
        };
        source.account_profile = Some(work.clone());
        self.accounts = AccountsState {
            loaded: true,
            continue_session: Some(source.id.clone()),
            overview: AgentAccountOverview {
                catalog: AgentAccountCatalog {
                    profiles: vec![work, personal],
                },
                ..Default::default()
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
        let adopting = match &action {
            AccountAction::Adopt(agent) => Some(agent.clone()),
            _ => None,
        };
        if !matches!(action, AccountAction::Refresh) {
            self.accounts.notice = None;
        }
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    match action {
                        AccountAction::Refresh => {}
                        AccountAction::Save(profile) => {
                            client.save_account_profile(&profile).await?;
                        }
                        AccountAction::Remove(id) => {
                            client.remove_account_profile(id).await?;
                        }
                        AccountAction::Adopt(agent) => return client.adopt_account(agent).await,
                    }
                    client.account_overview().await
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
                    Ok(overview) => {
                        if let Some(agent) = adopting
                            && let Some(label) = overview
                                .live(&agent)
                                .and_then(|l| l.profile_id.as_deref())
                                .and_then(|id| {
                                    overview.catalog.profiles.iter().find(|p| p.id == id)
                                })
                                .map(|p| p.label.clone())
                        {
                            this.accounts.notice =
                                Some(tf("settings.accounts.saved_as", &[("account", &label)]));
                        }
                        this.accounts.overview = overview;
                        this.accounts.loaded = true;
                        if close_editor {
                            this.accounts.editor = None;
                            this.accounts.renaming = None;
                        }
                    }
                    Err(error) => this.accounts.error = Some(error),
                }
                // Limits follow the accounts: ask for every one of them.
                cx.emit(UtilitySurfacesEvent::RefreshUsageLimits);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_rename(
        &mut self,
        profile: &AgentAccountProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.accounts.busy {
            return;
        }
        self.accounts.renaming = Some((profile.id.clone(), text_editor(&profile.label)));
        self.accounts.editor = None;
        self.accounts.error = None;
        self.settings_search_active = false;
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn finish_rename(&mut self, cx: &mut Context<Self>) {
        let Some((id, editor)) = self.accounts.renaming.take() else {
            return;
        };
        let label = editor.text().trim().to_owned();
        let Some(mut profile) = self
            .accounts
            .overview
            .catalog
            .profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
        else {
            return;
        };
        if label.is_empty() {
            self.accounts.renaming = Some((id, editor));
            self.accounts.error = Some(t("settings.accounts.name_required").into());
            cx.notify();
            return;
        }
        if label == profile.label {
            cx.notify();
            return;
        }
        profile.label = label;
        self.account_action(AccountAction::Save(profile), cx);
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
        let profile = profile.unwrap_or_else(|| {
            let id = format!(
                "profile-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            AgentAccountProfile {
                config_home: "~/.codex".into(),
                id,
                label: String::new(),
                agent: "codex".into(),
                host: None,
                is_default: false,
                login_store: None,
            }
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
        // The Engine rejects a nameless profile; say so here, without a round trip.
        if profile.label.is_empty() {
            self.accounts.error = Some(t("settings.accounts.name_required").into());
            if let Some(editor) = self.accounts.editor.as_mut() {
                editor.path_active = false;
            }
            cx.notify();
            return;
        }
        if shares_login(&profile) {
            profile.is_default = self
                .accounts
                .overview
                .catalog
                .profiles
                .iter()
                .find(|p| p.id == profile.id)
                .is_some_and(|p| p.is_default);
        }
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
        if self.accounts.renaming.is_some() {
            if self.accounts.busy {
                return true;
            }
            match event.keystroke.key.as_str() {
                "escape" => {
                    self.accounts.renaming = None;
                    self.accounts.error = None;
                }
                "enter" => {
                    self.finish_rename(cx);
                    return true;
                }
                _ => {
                    let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                        return false;
                    };
                    let (_, input) = self.accounts.renaming.as_mut().unwrap();
                    match edit {
                        Edit::Local(local) => {
                            input.apply(local);
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(input, cx)
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(input, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                input.insert(&text);
                            }
                        }
                    }
                }
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
        let now = crate::usage::Clock::read(&crate::usage::SystemClock).unix_seconds;
        let mut content = div().flex().flex_col().gap(px(16.0)).child(
            div()
                .text_size(px(12.0))
                .text_color(colors.secondary)
                .child(t("settings.accounts.intro")),
        );
        if let Some(notice) = &self.accounts.notice {
            content = content.child(
                div()
                    .id("account-notice")
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(notice.clone()),
            );
        }
        if self.accounts.busy {
            content = content.child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.tertiary)
                    .child(t("settings.accounts.updating")),
            );
        }
        if let Some(error) = &self.accounts.error {
            content = content.child(
                div()
                    .id("account-error")
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(12.0))
                    .text_color(Ink::DANGER)
                    .child(div().flex_1().child(error.clone()))
                    .child(self.account_button(
                        "retry-accounts",
                        t("settings.accounts.retry"),
                        cx,
                        |this, _, cx| this.refresh_accounts(cx),
                    )),
            );
        }
        for agent in [AgentKind::CLAUDE_CODE_ID, AgentKind::CODEX_ID] {
            content = content.child(self.agent_accounts(agent, now, cx));
        }
        content = content.child(
            div()
                .flex()
                .items_end()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(Typo::SECTION_HEADER.size))
                                .font_weight(Typo::SECTION_HEADER.weight)
                                .text_color(colors.tertiary)
                                .child(t("settings.accounts.other_profiles")),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .child(t("settings.accounts.other_detail")),
                        ),
                )
                .child(self.account_button(
                    "add-account",
                    t("settings.accounts.add_profile"),
                    cx,
                    |this, window, cx| this.edit_account(None, window, cx),
                )),
        );
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
                        .child(t("settings.accounts.account_profile")),
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
                                if editor.profile.host.is_none()
                                    && matches!(editor.path.text(), "~/.claude" | "")
                                {
                                    editor.path = text_editor("~/.codex");
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
                                if editor.profile.host.is_none()
                                    && matches!(editor.path.text(), "~/.codex" | "")
                                {
                                    editor.path = text_editor("~/.claude");
                                }
                            }
                            cx.notify();
                        },
                    )),
            );
            for (path, label, input) in [
                (false, t("settings.accounts.name"), &editor.name),
                (true, t("settings.accounts.directory"), &editor.path),
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
                .unwrap_or(t("settings.accounts.this_mac"))
                .to_owned();
            form = form.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(t("settings.accounts.run_on")),
            );
            let mut hosts = div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .child(self.account_button(
                    "account-local",
                    t("settings.accounts.this_mac"),
                    cx,
                    |this, _, cx| {
                        if let Some(editor) = &mut this.accounts.editor {
                            editor.profile.host = None;
                        }
                        cx.notify();
                    },
                ));
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
            form = form
                .child(hosts)
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child(tf(
                            "settings.accounts.selected_host",
                            &[("host", &host_label)],
                        )),
                )
                .when(!shares_login(&editor.profile), |form| {
                    form.child(self.account_button(
                        "account-default",
                        if editor.profile.is_default {
                            t("settings.accounts.is_default")
                        } else {
                            t("settings.accounts.use_default")
                        },
                        cx,
                        |this, _, cx| {
                            if let Some(editor) = &mut this.accounts.editor {
                                editor.profile.is_default = !editor.profile.is_default;
                            }
                            cx.notify();
                        },
                    ))
                })
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.tertiary)
                        .child(t("settings.accounts.editor_hint")),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(self.account_button(
                            "save-account",
                            t("settings.accounts.save_profile"),
                            cx,
                            |this, _, cx| this.save_account(cx),
                        ))
                        .child(self.account_button(
                            "cancel-account",
                            t("settings.accounts.cancel"),
                            cx,
                            |this, _, cx| {
                                this.accounts.editor = None;
                                cx.notify();
                            },
                        )),
                );
            content = content.child(form);
        }
        // Remote and separate-directory profiles: chosen per launch.
        for profile in self
            .accounts
            .overview
            .catalog
            .profiles
            .iter()
            .filter(|p| !shares_login(p))
        {
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
                .unwrap_or(t("settings.accounts.this_mac"));
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
                            .child(if profile.is_default {
                                tf(
                                    "settings.accounts.profile_default",
                                    &[("profile", &profile.label)],
                                )
                            } else {
                                profile.label.clone()
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(format!("{} · {}", agent_title(&profile.agent), host)),
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
                                t("settings.accounts.open_agent"),
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
                                t("settings.accounts.edit"),
                                cx,
                                move |this, window, cx| {
                                    this.edit_account(Some(edit.clone()), window, cx)
                                },
                            ))
                            .child(self.account_button(
                                format!("remove-{}", profile.id),
                                t("settings.accounts.remove_profile"),
                                cx,
                                move |this, _, cx| {
                                    this.account_action(AccountAction::Remove(remove.clone()), cx)
                                },
                            )),
                    ),
            );
        }
        let others = self
            .accounts
            .overview
            .catalog
            .profiles
            .iter()
            .any(|p| !shares_login(p));
        if others || self.accounts.editor.is_some() {
            content = content.child(
                div()
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(t("settings.accounts.footer")),
            );
        }
        settings_page(t("settings.accounts.title"), content, colors).into_any_element()
    }

    /// One Agent's accounts as a settings group: each account, the login in
    /// use when no account holds it yet, and adding another.
    fn agent_accounts(&self, agent: &'static str, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.settings_colors();
        let overview = &self.accounts.overview;
        let limits = &self.usage.limits;
        let suggested = crate::usage::limits::most_room(overview, limits, agent, ROOM_MARGIN, now);
        let mut rows = div().flex().flex_col();
        let mut first = true;
        let mut divided = |rows: Div, row: AnyElement| {
            let rows = if first {
                rows
            } else {
                rows.child(setting_divider(colors))
            };
            first = false;
            rows.child(row)
        };
        for profile in overview
            .catalog
            .profiles
            .iter()
            .filter(|p| p.agent == agent && shares_login(p))
        {
            let usage = limits
                .iter()
                .find(|l| l.profile_id.as_deref() == Some(profile.id.as_str()));
            let row = self.account_row(
                profile,
                usage,
                suggested.as_deref() == Some(profile.id.as_str()),
                now,
                cx,
            );
            rows = divided(rows, row);
        }
        if let Some(live) = overview.live(agent).filter(|l| l.profile_id.is_none()) {
            let usage = limits
                .iter()
                .find(|l| l.live && l.profile_id.is_none() && l.provider == provider(agent));
            let row = self.unsaved_account_row(agent, &live.identity, usage, now, cx);
            rows = divided(rows, row);
        }
        let add =
            div()
                .id(SharedString::from(format!("add-{agent}-account")))
                .debug_selector(move || format!("add-{agent}-account"))
                .h(px(38.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(Typo::ROW.size))
                .text_color(colors.secondary)
                .when(!self.accounts.busy, |row| {
                    row.cursor_pointer()
                        .hover(move |s| s.bg(colors.primary.alpha(0.035)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.login_account(SignIn::New(agent), cx)
                        }))
                })
                .child(div().w(px(14.0)).flex().justify_center().child(sf_symbol(
                    "plus",
                    10.0,
                    colors.secondary,
                )))
                .child(tf(
                    "settings.accounts.add_agent",
                    &[("agent", &agent_title(agent))],
                ))
                .into_any_element();
        rows = divided(rows, add);
        setting_section(agent_title(agent), rows, colors).into_any_element()
    }

    /// An account: who it is, its plan windows, and what can be done with it.
    fn account_row(
        &self,
        profile: &AgentAccountProfile,
        usage: Option<&AccountLimits>,
        suggested: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.settings_colors();
        let overview = &self.accounts.overview;
        let live = overview.is_live(profile);
        let login = overview.login(&profile.id);
        let signed_in = login.is_some_and(|l| l.signed_in);
        let signing_in = login.is_some_and(|l| l.signing_in);
        let needs_sign_in = usage.is_some_and(AccountLimits::needs_sign_in);
        let identity = login.map(|l| &l.identity);
        let plan = usage
            .and_then(|u| u.plan.clone())
            .or_else(|| identity.and_then(|i| i.plan.as_deref()).map(plan_label));
        let detail = [
            identity.and_then(|i| i.email.clone()),
            identity.and_then(organization),
            plan,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let tag = if signing_in {
            Some((t("settings.accounts.signing_in"), colors.secondary.into()))
        } else if !signed_in {
            Some((
                t("settings.accounts.not_signed_in"),
                colors.secondary.into(),
            ))
        } else if needs_sign_in {
            Some((
                t("settings.accounts.needs_sign_in"),
                Hsla::from(Ink::DANGER),
            ))
        } else if live {
            Some((t("settings.accounts.in_use"), colors.secondary.into()))
        } else if usage.is_some_and(|u| u.ready_again(now)) {
            Some((t("settings.accounts.ready"), Hsla::from(Ink::FRESH)))
        } else if suggested {
            Some((t("settings.accounts.most_room"), Hsla::from(Ink::FRESH)))
        } else {
            None
        };
        let renaming = self
            .accounts
            .renaming
            .as_ref()
            .filter(|(id, _)| id == &profile.id)
            .map(|(_, input)| input);
        let id = profile.id.clone();
        let mut actions = div().flex_none().flex().items_center().gap(px(6.0));
        if renaming.is_some() {
            actions = actions
                .child(self.account_button(
                    "rename-save",
                    t("settings.accounts.save"),
                    cx,
                    |this, _, cx| this.finish_rename(cx),
                ))
                .child(self.account_button(
                    "rename-cancel",
                    t("settings.accounts.cancel"),
                    cx,
                    |this, _, cx| {
                        this.accounts.renaming = None;
                        this.accounts.error = None;
                        cx.notify();
                    },
                ));
        } else {
            if (!signed_in || needs_sign_in) && !signing_in {
                let sign_in = profile.clone();
                actions = actions.child(self.account_button(
                    format!("sign-in-{id}"),
                    t("settings.accounts.sign_in"),
                    cx,
                    move |this, _, cx| this.login_account(SignIn::Profile(sign_in.clone()), cx),
                ));
            } else if signed_in && !live {
                let switch = profile.id.clone();
                actions = actions.child(self.account_button(
                    format!("switch-{id}"),
                    t("settings.accounts.switch"),
                    cx,
                    move |this, _, cx| this.continue_account(switch.clone(), cx),
                ));
            }
            let rename = profile.clone();
            let remove = profile.id.clone();
            actions = actions
                .child(self.account_icon_button(
                    format!("rename-{id}"),
                    "pencil",
                    t("settings.accounts.rename"),
                    false,
                    cx,
                    move |this, window, cx| this.start_rename(&rename, window, cx),
                ))
                .child(self.account_icon_button(
                    format!("remove-{id}"),
                    "trash",
                    t("settings.accounts.remove"),
                    true,
                    cx,
                    move |this, _, cx| {
                        this.account_action(AccountAction::Remove(remove.clone()), cx)
                    },
                ));
        }
        let title = match renaming {
            Some(input) => div()
                .id("account-rename")
                .role(Role::TextInput)
                .aria_label(t("settings.accounts.name"))
                .h(px(24.0))
                .w(px(220.0))
                .px(px(7.0))
                .rounded(px(Radius::BADGE))
                .border_1()
                .border_color(colors.primary.alpha(0.3))
                .bg(colors.primary.alpha(0.04))
                .flex()
                .items_center()
                .overflow_hidden()
                .text_size(px(Typo::ROW.size))
                .cursor(CursorStyle::IBeam)
                .child(query_label(input))
                .into_any_element(),
            None => div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .min_w(px(0.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(colors.primary)
                        .child(profile.label.clone()),
                )
                .when_some(tag, |row, (tag, color)| {
                    row.child(account_tag(tag, color, colors))
                })
                .into_any_element(),
        };
        let windows = match usage {
            _ if !signed_in || needs_sign_in => None,
            Some(usage) if !usage.windows.is_empty() => {
                Some(plan_windows(usage, live, now, colors))
            }
            // Its token expired before Diri asked: say when numbers come.
            _ => Some(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(t("settings.accounts.not_checked"))
                    .into_any_element(),
            ),
        };
        div()
            .id(SharedString::from(format!("account-{id}")))
            .debug_selector(move || format!("account-{id}"))
            .min_h(px(SETTINGS_ROW_HEIGHT))
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .self_start()
                    .pt(px(4.0))
                    .child(account_check(live, colors)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(title)
                    .when(!detail.is_empty(), |column| {
                        column.child(
                            div()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .child(detail),
                        )
                    })
                    .children(windows),
            )
            .child(actions)
            .into_any_element()
    }

    /// The login an Agent uses now that no account holds: one click keeps it.
    fn unsaved_account_row(
        &self,
        agent: &'static str,
        identity: &diri_proto::AgentAccountIdentity,
        usage: Option<&AccountLimits>,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.settings_colors();
        let detail = [organization(identity), usage.and_then(|u| u.plan.clone())]
            .into_iter()
            .flatten()
            .chain(std::iter::once(t("settings.accounts.not_saved").to_owned()))
            .collect::<Vec<_>>()
            .join(" · ");
        div()
            .id(SharedString::from(format!("unsaved-{agent}")))
            .debug_selector(move || format!("unsaved-{agent}"))
            .min_h(px(SETTINGS_ROW_HEIGHT))
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .self_start()
                    .pt(px(4.0))
                    .child(account_check(true, colors)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                            .text_color(colors.primary)
                            .child(
                                identity
                                    .email
                                    .clone()
                                    .unwrap_or_else(|| t("settings.accounts.signed_in").into()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(detail),
                    )
                    .children(usage.map(|u| plan_windows(u, true, now, colors))),
            )
            .child(self.account_button(
                format!("save-{agent}-login"),
                t("settings.accounts.save"),
                cx,
                move |this, _, cx| this.account_action(AccountAction::Adopt(agent.into()), cx),
            ))
            .into_any_element()
    }

    /// A quiet icon action with its name on hover.
    fn account_icon_button(
        &self,
        id: impl Into<SharedString>,
        icon: &'static str,
        label: &'static str,
        destructive: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = self.settings_colors();
        let busy = self.accounts.busy;
        div()
            .id(id.into())
            .role(Role::Button)
            .aria_label(label)
            .size(px(26.0))
            .flex_none()
            .rounded(px(Radius::BADGE))
            .flex()
            .items_center()
            .justify_center()
            .when(!busy, |button| {
                button
                    .cursor_pointer()
                    .hover(move |s| {
                        s.bg(if destructive {
                            Ink::DANGER.alpha(0.10)
                        } else {
                            colors.primary.alpha(0.08)
                        })
                    })
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| crate::palette_chrome::PaletteTooltip(label.into(), colors))
                            .into()
                    })
                    .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
            .child(sf_symbol(icon, 10.0, colors.tertiary))
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

    #[test]
    fn claudes_default_personal_organization_is_not_repeated() {
        let identity = |email: &str, organization: &str| diri_proto::AgentAccountIdentity {
            email: Some(email.into()),
            organization: Some(organization.into()),
            plan: None,
        };
        assert_eq!(
            organization(&identity(
                "me@example.test",
                "me@example.test's Organization"
            )),
            None,
            "it only repeats the email"
        );
        assert_eq!(
            organization(&identity("me@corp.test", "Corp Engineering")).as_deref(),
            Some("Corp Engineering")
        );
    }

    #[gpui::test]
    fn renaming_in_place_keeps_a_name_and_escape_leaves_it_unchanged(
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
            surfaces.seed_account_preview(false);
            surfaces
        });
        let key = |name| KeyDownEvent {
            keystroke: gpui::Keystroke::parse(name).unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        surfaces.update_in(cx, |surfaces, window, cx| {
            let personal = surfaces.accounts.overview.catalog.profiles[1].clone();
            surfaces.start_rename(&personal, window, cx);
            surfaces.accounts.renaming.as_mut().unwrap().1.select_all();
            assert!(surfaces.handle_account_key(&key("backspace"), cx));
            assert!(surfaces.handle_account_key(&key("enter"), cx));
            assert_eq!(
                surfaces.accounts.error.as_deref(),
                Some(t("settings.accounts.name_required")),
                "an account keeps a name"
            );
            assert!(!surfaces.accounts.busy, "nothing was sent");
            assert!(surfaces.handle_account_key(&key("escape"), cx));
            assert!(surfaces.accounts.renaming.is_none());
            assert_eq!(
                surfaces.accounts.overview.catalog.profiles[1].label,
                "Personal"
            );
        });
    }

    #[gpui::test]
    fn saving_a_nameless_profile_asks_for_a_name_without_calling_the_engine(
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
            surfaces
        });
        surfaces.update_in(cx, |surfaces, window, cx| {
            surfaces.edit_account(None, window, cx);
            let enter = KeyDownEvent {
                keystroke: gpui::Keystroke::parse("enter").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            assert!(surfaces.handle_account_key(&enter, cx));
            assert!(!surfaces.accounts.busy, "nothing may be sent");
            assert_eq!(
                surfaces.accounts.error.as_deref(),
                Some(t("settings.accounts.name_required"))
            );
            assert!(surfaces.accounts.editor.is_some(), "the form stays open");
        });
    }
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
            let personal = surfaces.accounts.overview.catalog.profiles[1].clone();
            surfaces.accounts.overview.catalog.profiles.extend([
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

    /// The remote profile the preview opens in the editor, as saved.
    fn saved(surfaces: &UtilitySurfaces) -> &AgentAccountProfile {
        surfaces
            .accounts
            .overview
            .catalog
            .profiles
            .iter()
            .find(|p| p.id == "build-box")
            .unwrap()
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
            assert_eq!(saved(surfaces).config_home, "~/.codex");
            surfaces.accounts.busy = true;
            surfaces.handle_account_key(&key("escape"), cx);
            assert!(surfaces.accounts.editor.is_some());
            surfaces.accounts.busy = false;
            surfaces.handle_account_key(&key("escape"), cx);
            assert!(surfaces.accounts.editor.is_none());
            assert_eq!(saved(surfaces).config_home, "~/.codex");
        });
    }
}
