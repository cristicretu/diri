use super::*;

impl LauncherOverlay {
    pub(super) fn reconcile_account(&mut self) {
        if let Some(id) = self.selected_account.as_deref().filter(|id| !id.is_empty())
            && !self.accounts.profiles.iter().any(|p| {
                p.id == id && p.agent == self.selected_harness.id() && p.host == self.selected_host
            })
        {
            self.selected_account = None;
        }
    }
    pub(super) fn validate_recipe_account(&self, recipe: &LaunchRecipe) -> Result<(), RecipeIssue> {
        if let Some(id) = recipe
            .account_profile_id
            .as_deref()
            .filter(|id| !id.is_empty())
            && !self
                .accounts
                .profiles
                .iter()
                .any(|p| p.id == id && p.agent == recipe.agent.id() && p.host == recipe.host)
        {
            return Err(RecipeIssue::AccountUnavailable);
        }
        Ok(())
    }
    pub(super) fn refresh_launcher_accounts(&mut self, cx: &mut Context<Self>) {
        if self.preview || self.accounts_loading {
            return;
        }
        self.accounts_loading = true;
        self.accounts_error = None;
        let client = Arc::clone(self.services.store.client());
        let runtime = Arc::clone(&self.services.tokio);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client.account_profiles().await
                })
                .await;
            let result = result
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.accounts_loading = false;
                match result {
                    Ok(catalog) => this.accounts = catalog,
                    Err(error) => this.accounts_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn account_choices(&self) -> Vec<(Option<String>, String)> {
        let profiles: Vec<_> = self
            .accounts
            .profiles
            .iter()
            .filter(|p| p.agent == self.selected_harness.id() && p.host == self.selected_host)
            .collect();
        let default = profiles
            .iter()
            .find(|p| p.is_default)
            .map_or("CLI account", |p| p.label.as_str());
        let mut choices = vec![
            (None, format!("Default · {default}")),
            (Some(String::new()), "CLI environment".into()),
        ];
        choices.extend(
            profiles
                .into_iter()
                .map(|p| (Some(p.id.clone()), p.label.clone())),
        );
        choices
    }

    pub(super) fn manage_accounts(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.picker = None;
        cx.emit(LauncherEvent::ManageAccounts);
        cx.notify();
    }

    pub(super) fn account_picker_button(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self
            .account_choices()
            .into_iter()
            .find(|(id, _)| *id == self.selected_account)
            .map(|(_, label)| label)
            .unwrap_or_else(|| "Unavailable account".into());
        div()
            .id("launcher-account-button")
            .role(Role::Button)
            .aria_label("Choose account profile")
            .h(px(CONTROL_SIZE))
            .max_w(px(170.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(CONTROL_RADIUS))
            .bg(Fill::subtle(colors))
            .text_size(px(11.0))
            .text_color(colors.secondary)
            .cursor_pointer()
            .hover(move |s| s.bg(colors.primary.alpha(0.09)))
            .active(move |s| s.bg(colors.primary.alpha(0.12)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_picker(Picker::Account);
                if this.accounts_error.is_some() {
                    this.refresh_launcher_accounts(cx);
                }
                cx.notify();
            }))
            .child(sf_symbol("account.circle", 12.0, colors.secondary))
            .child(
                div()
                    .min_w(px(0.0))
                    .text_ellipsis()
                    .child(if self.accounts_loading {
                        "Loading accounts…".into()
                    } else if self.accounts_error.is_some() {
                        "Accounts unavailable".into()
                    } else {
                        label
                    }),
            )
            .child(sf_symbol("chevron.down", 7.5, colors.tertiary))
            .into_any_element()
    }

    pub(super) fn render_account_picker(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = div()
            .id("launcher-account-list")
            .py(px(4.0))
            .w(px(280.0))
            .max_h(px(260.0))
            .overflow_y_scroll();
        if let Some(error) = &self.accounts_error {
            list = list.child(
                div()
                    .p(px(10.0))
                    .text_size(px(11.0))
                    .text_color(colors.secondary)
                    .child(error.clone()),
            );
        }
        for (index, (id, label)) in self.account_choices().into_iter().enumerate() {
            let selected = id == self.selected_account;
            list = list.child(
                div()
                    .id(format!("launcher-account-{index}"))
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .mx(px(6.0))
                    .h(px(34.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(8.0))
                    .text_size(px(12.0))
                    .text_color(colors.primary)
                    .when(self.highlight == index, |row| {
                        row.bg(colors.primary.alpha(0.08))
                    })
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_account = id.clone();
                        this.picker = None;
                        cx.notify();
                    }))
                    .child(div().min_w(px(0.0)).flex_1().text_ellipsis().child(label))
                    .when(selected, |row| {
                        row.child(sf_symbol("checkmark", 10.0, colors.secondary))
                    }),
            );
        }
        list = list.child(
            div()
                .id("launcher-manage-accounts")
                .role(Role::Button)
                .h(px(34.0))
                .mx(px(6.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(colors.secondary)
                .when(self.highlight == self.account_choices().len(), |row| {
                    row.bg(colors.primary.alpha(0.08))
                })
                .cursor_pointer()
                .hover(move |s| s.bg(colors.primary.alpha(0.06)))
                .on_click(cx.listener(|this, _, _, cx| this.manage_accounts(cx)))
                .child("Manage accounts…"),
        );
        FloatingSurface::new(colors, list).into_any_element()
    }
}
