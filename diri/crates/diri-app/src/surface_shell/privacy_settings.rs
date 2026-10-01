//! Settings > General > Privacy: whether diagnostics upload, the name that
//! goes with them, the Support ID a report quotes, and the local folder they
//! are recorded to. The files behind it belong to `diri-telemetry`; the
//! Engine's uploader re-reads them every cycle, so changes apply next cycle.
use super::*;
use crate::telemetry::PrivacySettings;

#[derive(Default)]
pub(super) struct PrivacyState {
    settings: PrivacySettings,
    name: QueryEditor,
    name_active: bool,
    copied: bool,
    save_error: Option<&'static str>,
    send: SendState,
}

/// The "Send now" button: idle, waiting on the Engine, or the last outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SendState {
    #[default]
    Idle,
    Sending,
    Done(&'static str),
}

impl UtilitySurfaces {
    /// Re-reads the files each time Settings opens: the Support ID may have
    /// been created by the Engine since the last look.
    pub(super) fn reload_privacy(&mut self) {
        self.set_privacy_settings(PrivacySettings::load());
    }

    pub(super) fn set_privacy_settings(&mut self, settings: PrivacySettings) {
        self.privacy.name = text_editor(&settings.name_text());
        self.privacy.settings = settings;
        self.privacy.name_active = false;
        self.privacy.copied = false;
        self.privacy.save_error = None;
        if self.privacy.send != SendState::Sending {
            self.privacy.send = SendState::Idle;
        }
    }

    pub(super) fn deactivate_privacy_name(&mut self) {
        self.privacy.name_active = false;
    }

    /// Uploads everything recorded so far, now, even with sharing off: the
    /// click is the consent. The Engine does the upload; this waits for it.
    fn send_diagnostics_now(&mut self, cx: &mut Context<Self>) {
        if self.privacy.send == SendState::Sending {
            return;
        }
        self.privacy.send = SendState::Sending;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { crate::telemetry::upload_now_blocking() })
                .await;
            let summary = crate::telemetry::upload_now_summary(&result);
            let _ = this.update(cx, |this, cx| {
                this.privacy.send = SendState::Done(summary);
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_diagnostics_upload(&mut self, cx: &mut Context<Self>) {
        let upload = !self.privacy.settings.config.upload;
        let mut config = self.privacy.settings.config.clone();
        config.upload = upload;
        if self.save_privacy_config(config) {
            diri_telemetry::event!("privacy.upload_changed", upload = upload);
        }
        cx.notify();
    }

    /// Stores what the field says. Leaving the login name untouched keeps
    /// following the login name; an empty field is an anonymous install.
    fn store_privacy_name(&mut self) {
        let typed = self.privacy.name.text().trim().to_owned();
        let settings = &mut self.privacy.settings;
        let name = if settings.config.name.is_none()
            && settings.login_name.as_deref() == Some(typed.as_str())
        {
            None
        } else {
            Some(typed)
        };
        if settings.config.name != name {
            let mut config = settings.config.clone();
            config.name = name;
            self.save_privacy_config(config);
        }
    }

    fn save_privacy_config(&mut self, config: diri_telemetry::Config) -> bool {
        match self.privacy.settings.save_config(config) {
            Ok(()) => {
                self.privacy.save_error = None;
                true
            }
            Err(error) => {
                self.privacy.save_error = Some(
                    "Could not save. Your previous privacy settings are still active. Try again.",
                );
                diri_telemetry::error_event!(
                    "settings.privacy_save_failed",
                    io = diri_telemetry::io_error(&error)
                );
                false
            }
        }
    }

    pub(super) fn handle_privacy_name_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.surface != Surface::Settings
            || self.settings_tab != SettingsTab::General
            || !self.privacy.name_active
        {
            return false;
        }
        let key = &event.keystroke;
        match key.key.as_str() {
            "escape" | "enter" | "tab" => {
                self.store_privacy_name();
                self.privacy.name_active = false;
            }
            _ => {
                let Some(edit) = query_editor::edit_for(key) else {
                    return false;
                };
                match edit {
                    Edit::Local(local) => {
                        self.privacy.name.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.privacy.name, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.privacy.name, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.privacy
                                .name
                                .insert(text.lines().next().unwrap_or_default());
                        }
                    }
                }
                self.store_privacy_name();
            }
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(super) fn privacy_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let privacy = &self.privacy;
        let support_id = privacy
            .settings
            .support_id
            .clone()
            .unwrap_or_else(|| "Unavailable".to_owned());
        setting_section(
            "Privacy",
            div()
                .flex()
                .flex_col()
                .when_some(privacy.save_error, |column, error| {
                    column.child(div().id("privacy-save-error").px(px(12.0)).py(px(8.0))
                        .text_size(px(12.0)).text_color(Ink::DANGER).child(error))
                })
                .child(toggle_row(
                    "Share diagnostics to help fix bugs",
                    "Crashes, hangs, errors and timings. Never terminal contents, prompts or files.",
                    privacy.settings.config.upload,
                    "toggle-share-diagnostics",
                    colors,
                    cx,
                    |this, cx| this.toggle_diagnostics_upload(cx),
                ))
                .child(setting_divider(colors))
                .child(setting_row(
                    "Name for bug reports",
                    "Sent with diagnostics so a report can be found. Leave it empty to stay anonymous.",
                    self.privacy_name_field(cx),
                    colors,
                ))
                .child(setting_divider(colors))
                .child(setting_row(
                    "Support ID",
                    "Quote it when you report a problem.",
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .debug_selector(|| "privacy-support-id".into())
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(11.0))
                                .text_color(colors.secondary)
                                .child(support_id.clone()),
                        )
                        .when(privacy.settings.support_id.is_some(), |row| {
                            row.child(surface_button(
                                if privacy.copied { "Copied" } else { "Copy" },
                                "copy-support-id",
                                colors,
                                cx,
                                move |this, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        support_id.clone(),
                                    ));
                                    this.privacy.copied = true;
                                    cx.notify();
                                },
                            ))
                        }),
                    colors,
                ))
                .child(setting_divider(colors))
                .child(setting_row(
                    "Send diagnostics now",
                    match privacy.send {
                        SendState::Done(summary) => summary,
                        _ => "Uploads what's been recorded so far, even with sharing off.",
                    },
                    surface_button(
                        if privacy.send == SendState::Sending {
                            "Sending…"
                        } else {
                            "Send now"
                        },
                        "send-diagnostics-now",
                        colors,
                        cx,
                        |this, cx| this.send_diagnostics_now(cx),
                    ),
                    colors,
                ))
                .child(setting_divider(colors))
                .child(setting_row(
                    format!("Diagnostics on {}", crate::platform::local_machine_label_lowercase()),
                    "What diri has recorded, before anything is shared.",
                    surface_button(
                        crate::platform::reveal_in_file_manager_label(),
                        "show-diagnostics-folder",
                        colors,
                        cx,
                        |this, cx| {
                            if let Some(folder) = &this.privacy.settings.folder {
                                let _ = std::fs::create_dir_all(folder);
                                cx.reveal_path(folder);
                            }
                        },
                    ),
                    colors,
                )),
            colors,
        )
    }

    fn privacy_name_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let active = self.privacy.name_active;
        let content = if active {
            query_label(&self.privacy.name)
        } else if self.privacy.name.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child("Anonymous")
                .into_any_element()
        } else {
            div()
                .child(self.privacy.name.text().to_owned())
                .into_any_element()
        };
        div()
            .id("privacy-name")
            .debug_selector(|| "privacy-name".into())
            .flex_none()
            .w(px(180.0))
            .h(px(26.0))
            .px(px(8.0))
            .rounded(px(Radius::BADGE))
            .border_1()
            .border_color(colors.primary.alpha(if active { 0.26 } else { 0.10 }))
            .bg(colors.primary.alpha(if active { 0.075 } else { 0.04 }))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(11.0))
            .text_color(colors.primary)
            .cursor(CursorStyle::IBeam)
            .on_click(cx.listener(|this, _, window, cx| {
                this.privacy.name_active = true;
                this.privacy.name.select_all();
                this.roots_editor_active = false;
                this.include_editor_active = false;
                this.settings_search_active = false;
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .child(content)
    }
}
