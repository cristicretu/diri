//! Settings › Sync & Migration: keep the portable part of a Diri setup in a
//! private Git repository or a synced folder, and move everything (sessions
//! and their conversations included) to another Mac in one backup file.
//!
//! The Engine does the work (`sync.*`, `migrate.*`); this page only edits
//! the configuration, shows one line of status, and walks an import through
//! its dry run. Preferences that arrive wait beside `prefs.json` until the
//! app merges them, which happens here right after a sync or import and once
//! at launch (`SessionStore::load`).
use super::*;
use diri_client::ClientError;
use diri_proto::portable::{
    MigrateExportParams, MigrateImportParams, MigrateImportPlan, PathMapping, PlannedProject,
    ProjectMatch, SyncBackendKind, SyncConfig, SyncStatus,
};

const FOLDER_PLACEHOLDER: &str = "~/Library/Mobile Documents/com~apple~CloudDocs/Diri";
const GIT_PLACEHOLDER: &str = "git@github.com:you/diri-profile.git";
const LOCATION_FIELD_WIDTH: f32 = 300.0;
const PASSPHRASE_FIELD_WIDTH: f32 = 160.0;

#[derive(Default)]
pub(super) struct SyncSettingsState {
    status: Option<SyncStatus>,
    /// The last request that failed, in one quiet line.
    error: Option<String>,
    busy: Option<SyncBusy>,
    /// The form, which follows `status` until it is edited.
    backend: SyncBackendKind,
    location: QueryEditor,
    auto: bool,
    export_passphrase: QueryEditor,
    exporting: bool,
    export_line: Option<String>,
    import: Option<ImportDraft>,
    import_line: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncBusy {
    Loading,
    Saving,
    Syncing,
}

/// A backup being imported: the file, the user's answers, and the Engine's
/// latest plan for them.
pub(super) struct ImportDraft {
    path: PathBuf,
    maps: Vec<PathMapping>,
    passphrase: QueryEditor,
    /// The bundle is sealed: the passphrase field stays up.
    locked: bool,
    plan: Option<MigrateImportPlan>,
    error: Option<String>,
    running: bool,
}

impl ImportDraft {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            maps: Vec::new(),
            passphrase: QueryEditor::default(),
            locked: false,
            plan: None,
            error: None,
            running: false,
        }
    }

    fn params(&self, dry_run: bool) -> MigrateImportParams {
        let passphrase = self.passphrase.text();
        MigrateImportParams {
            path: self.path.display().to_string(),
            passphrase: (!passphrase.is_empty()).then(|| passphrase.to_owned()),
            dry_run,
            maps: self.maps.clone(),
        }
    }
}

/// What a failed request shows: the Engine's own message, or a way out when
/// the Engine predates sync.
fn error_line(error: &ClientError) -> String {
    match error {
        ClientError::Control(error)
            if error.code == "not_found" && error.message.contains("method") =>
        {
            t("settings.sync.engine_too_old").to_owned()
        }
        ClientError::Control(error) => error.message.clone(),
        other => other.to_string(),
    }
}

/// A sealed bundle opened without (or with the wrong) passphrase.
fn needs_passphrase(error: &ClientError) -> bool {
    let message = match error {
        ClientError::Control(error) => format!("{} {}", error.code, error.message),
        other => other.to_string(),
    }
    .to_lowercase();
    message.contains("passphrase") || message.contains("encrypted") || message.contains("decrypt")
}

fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0)
}

/// "Last synced 2 min ago · 3 in, 1 out · 1 conflict kept local".
pub(super) fn sync_status_line(status: &SyncStatus, now: f64) -> String {
    if status.config.is_none() {
        return t("settings.sync.not_set_up").to_owned();
    }
    if status.running {
        return t("settings.sync.syncing").to_owned();
    }
    if let Some(error) = &status.last_error {
        return error.clone();
    }
    let Some(at) = status.last_synced_at else {
        return t("settings.sync.never").to_owned();
    };
    let seconds = ((now - at.0).max(0.0) / 1000.0) as u64;
    let n = |unit: u64| (seconds / unit).to_string();
    let mut parts = vec![match seconds {
        0..=59 => t("settings.sync.just_now").to_owned(),
        60..=3_599 => tf("settings.sync.minutes", &[("n", &n(60))]),
        3_600..=86_399 => tf("settings.sync.hours", &[("n", &n(3_600))]),
        _ => tf("settings.sync.days", &[("n", &n(86_400))]),
    }];
    if let Some(summary) = &status.last_summary {
        if summary.pulled > 0 || summary.pushed > 0 {
            parts.push(tf(
                "settings.sync.moved",
                &[
                    ("pulled", &summary.pulled.to_string()),
                    ("pushed", &summary.pushed.to_string()),
                ],
            ));
        }
        let conflicts = summary.conflicts.len();
        if conflicts > 0 {
            parts.push(tf(
                if conflicts == 1 {
                    "settings.sync.conflicts_one"
                } else {
                    "settings.sync.conflicts_other"
                },
                &[("count", &conflicts.to_string())],
            ));
        }
    }
    parts.join(" · ")
}

/// `Diri-2026-11-23.diribundle`, dated in UTC.
fn default_bundle_name() -> String {
    let days = (now_millis() / 86_400_000.0) as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "Diri-{year:04}-{month:02}-{day:02}.{}",
        diri_proto::portable::BUNDLE_EXTENSION
    )
}

/// The editor with every character drawn as a bullet, caret and selection
/// where they are in the real text.
fn masked(editor: &QueryEditor) -> QueryEditor {
    let text = editor.text();
    let chars_before = |offset: usize| text[..offset].chars().count();
    let bullet = '•'.len_utf8();
    let cursor = editor.cursor();
    let anchor = match editor.selection() {
        Some(range) if range.end == cursor => range.start,
        Some(range) => range.end,
        None => cursor,
    };
    let mut masked = QueryEditor::default();
    masked.insert(&"•".repeat(text.chars().count()));
    masked.set_cursor(chars_before(anchor) * bullet, false);
    masked.set_cursor(chars_before(cursor) * bullet, true);
    masked
}

impl UtilitySurfaces {
    fn sync_editor_mut(&mut self, field: SettingsField) -> Option<&mut QueryEditor> {
        match field {
            SettingsField::SyncLocation => Some(&mut self.sync.location),
            SettingsField::SyncExportPassphrase => Some(&mut self.sync.export_passphrase),
            SettingsField::SyncImportPassphrase => {
                self.sync.import.as_mut().map(|draft| &mut draft.passphrase)
            }
            _ => None,
        }
    }

    pub(super) fn release_sync_fields(&mut self) {
        for field in [
            SettingsField::SyncLocation,
            SettingsField::SyncExportPassphrase,
            SettingsField::SyncImportPassphrase,
        ] {
            self.active_field.release(field);
        }
    }

    /// Takes the Engine's status, and the form with it unless the form has
    /// edits the user has not saved.
    fn set_sync_status(&mut self, status: SyncStatus, cx: &mut Context<Self>) {
        let editing = self.active_field.is(SettingsField::SyncLocation);
        let saved = self.sync.status.as_ref().and_then(|status| status.config.as_ref());
        let pristine = saved.map_or(self.sync.location.is_empty(), |config| {
            config.location == self.sync.location.text() && config.backend == self.sync.backend
        });
        if !editing && pristine {
            let config = status.config.clone().unwrap_or_default();
            self.sync.backend = config.backend;
            self.sync.location = text_editor(&config.location);
            self.sync.auto = status.config.as_ref().is_some_and(|config| config.auto);
        }
        let prefs_pending = status
            .last_summary
            .as_ref()
            .is_some_and(|summary| summary.prefs_pending);
        self.sync.status = Some(status);
        self.sync.error = None;
        if prefs_pending {
            self.merge_pending_portable_prefs(cx);
        }
    }

    pub(super) fn refresh_sync_status(&mut self, cx: &mut Context<Self>) {
        if self.sync.busy.is_some() {
            return;
        }
        self.sync.busy = Some(SyncBusy::Loading);
        self.sync_request(|client| async move { client.sync_status().await }, cx);
    }

    /// Runs one `sync.*` call and shows what it returns.
    fn sync_request<F>(
        &mut self,
        call: impl FnOnce(Arc<diri_client::DaemonClient>) -> F + Send + 'static,
        cx: &mut Context<Self>,
    ) where
        F: std::future::Future<Output = Result<SyncStatus, ClientError>> + Send + 'static,
    {
        cx.notify();
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                call(client).await
            });
            let result = match task.await {
                Ok(result) => result.map_err(|error| error_line(&error)),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                this.sync.busy = None;
                match result {
                    Ok(status) => this.set_sync_status(status, cx),
                    Err(error) => this.sync.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The form as a configuration, or `None` when it has no location.
    fn sync_form_config(&self) -> Option<SyncConfig> {
        let location = self.sync.location.text().trim();
        (!location.is_empty()).then(|| SyncConfig {
            backend: self.sync.backend,
            location: location.to_owned(),
            auto: self.sync.auto,
        })
    }

    fn save_sync_config(&mut self, cx: &mut Context<Self>) {
        self.active_field.release(SettingsField::SyncLocation);
        let Some(config) = self.sync_form_config() else {
            return;
        };
        let saved = self.sync.status.as_ref().and_then(|status| status.config.as_ref());
        if saved == Some(&config) || self.sync.busy.is_some() {
            return;
        }
        self.sync.busy = Some(SyncBusy::Saving);
        self.sync_request(
            move |client| async move { client.sync_configure(Some(config)).await },
            cx,
        );
    }

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.active_field.release(SettingsField::SyncLocation);
        if self.sync.busy.is_some() {
            return;
        }
        let Some(config) = self.sync_form_config() else {
            self.sync.error = Some(t("settings.sync.location_required").to_owned());
            cx.notify();
            return;
        };
        let saved = self
            .sync
            .status
            .as_ref()
            .and_then(|status| status.config.clone());
        self.sync.busy = Some(SyncBusy::Syncing);
        self.sync_request(
            move |client| async move {
                // Unsaved edits first, so "Sync Now" syncs what the form says.
                if saved.as_ref() != Some(&config) {
                    client.sync_configure(Some(config)).await?;
                }
                client.sync_now().await
            },
            cx,
        );
    }

    fn turn_sync_off(&mut self, cx: &mut Context<Self>) {
        if self.sync.busy.is_some() {
            return;
        }
        self.sync.busy = Some(SyncBusy::Saving);
        self.sync.location.clear();
        self.sync.auto = false;
        self.sync_request(|client| async move { client.sync_configure(None).await }, cx);
    }

    fn toggle_sync_auto(&mut self, cx: &mut Context<Self>) {
        self.sync.auto = !self.sync.auto;
        let configured = self
            .sync
            .status
            .as_ref()
            .is_some_and(|status| status.config.is_some());
        if configured {
            self.save_sync_config(cx);
        }
        cx.notify();
    }

    fn choose_sync_backend(&mut self, backend: SyncBackendKind, cx: &mut Context<Self>) {
        if self.sync.backend == backend {
            return;
        }
        self.sync.backend = backend;
        // A folder is not a repository: the other back end starts empty.
        self.sync.location.clear();
        self.active_field = SettingsField::SyncLocation;
        cx.notify();
    }

    /// Applies preferences a sync or import left for the app, as a change
    /// made in Settings would be.
    fn merge_pending_portable_prefs(&mut self, cx: &mut Context<Self>) {
        let language = self.prefs.ui_language;
        let result = self
            .store
            .write()
            .expect("session store lock poisoned")
            .apply_pending_portable_prefs();
        match result {
            Ok(true) => {
                self.prefs = self
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .preferences()
                    .clone();
                self.store_runtime.publish_local_change();
                if self.prefs.ui_language != language {
                    crate::i18n::apply_live(self.prefs.ui_language, cx);
                }
                cx.refresh_windows();
            }
            Ok(false) => {}
            Err(error) => {
                self.sync.error = Some(tf(
                    "settings.general.save_failed",
                    &[("error", &error.to_string())],
                ));
            }
        }
    }

    fn export_backup(&mut self, cx: &mut Context<Self>) {
        if self.sync.exporting {
            return;
        }
        self.release_sync_fields();
        let directory = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Downloads"))
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let path = cx.prompt_for_new_path(&directory, Some(&default_bundle_name()));
        let passphrase = self.sync.export_passphrase.text().to_owned();
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.sync.exporting = true;
                this.sync.export_line = None;
                cx.notify();
            });
            let params = MigrateExportParams {
                path: path.display().to_string(),
                passphrase: (!passphrase.is_empty()).then_some(passphrase),
                skip_transcripts: false,
            };
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                client.migrate_export(&params).await
            });
            let line = match task.await {
                Ok(Ok(result)) => {
                    let file = Path::new(&result.path)
                        .file_name()
                        .map_or(result.path.clone(), |name| {
                            name.to_string_lossy().into_owned()
                        });
                    tf(
                        "settings.sync.exported",
                        &[
                            ("sessions", &result.counts.sessions.to_string()),
                            ("conversations", &result.counts.transcripts.to_string()),
                            ("file", &file),
                        ],
                    )
                }
                Ok(Err(error)) => error_line(&error),
                Err(error) => error.to_string(),
            };
            let _ = this.update(cx, |this, cx| {
                this.sync.exporting = false;
                this.sync.export_line = Some(line);
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_import_backup(&mut self, cx: &mut Context<Self>) {
        self.release_sync_fields();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings.sync.import_button").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.sync.import_line = None;
                this.sync.import = Some(ImportDraft::new(path));
                this.run_import(true, cx);
            });
        })
        .detach();
    }

    fn map_project(&mut self, from: String, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("settings.sync.choose_prompt").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(to) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(draft) = this.sync.import.as_mut() else {
                    return;
                };
                draft.maps.retain(|map| map.from != from);
                draft.maps.push(PathMapping {
                    from,
                    to: to.display().to_string(),
                });
                this.run_import(true, cx);
            });
        })
        .detach();
    }

    fn cancel_import(&mut self, cx: &mut Context<Self>) {
        self.sync.import = None;
        self.active_field.release(SettingsField::SyncImportPassphrase);
        cx.notify();
    }

    /// Plans (`dry_run`) or performs the import with the draft's answers.
    fn run_import(&mut self, dry_run: bool, cx: &mut Context<Self>) {
        self.active_field.release(SettingsField::SyncImportPassphrase);
        let Some(draft) = self.sync.import.as_mut() else {
            return;
        };
        if draft.running {
            return;
        }
        draft.running = true;
        draft.error = None;
        let params = draft.params(dry_run);
        cx.notify();
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                client.migrate_import(&params).await
            });
            let result = match task.await {
                Ok(result) => result.map_err(|error| (needs_passphrase(&error), error_line(&error))),
                Err(error) => Err((false, error.to_string())),
            };
            let _ = this.update(cx, |this, cx| {
                this.finish_import(dry_run, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_import(
        &mut self,
        dry_run: bool,
        result: Result<MigrateImportPlan, (bool, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.sync.import.as_mut() else {
            return;
        };
        draft.running = false;
        match result {
            Ok(plan) if dry_run => {
                draft.locked = draft.locked || plan.source.encrypted;
                draft.plan = Some(plan);
            }
            Ok(plan) => {
                let count = plan.added.sessions + plan.updated;
                self.sync.import_line = Some(tf(
                    if count == 1 {
                        "settings.sync.imported_one"
                    } else {
                        "settings.sync.imported_other"
                    },
                    &[("count", &count.to_string())],
                ));
                self.sync.import = None;
                if plan.prefs_pending {
                    self.merge_pending_portable_prefs(cx);
                }
            }
            Err((locked, error)) => {
                if locked {
                    draft.locked = true;
                    draft.plan = None;
                    self.active_field = SettingsField::SyncImportPassphrase;
                }
                draft.error = Some(error);
            }
        }
    }

    pub(super) fn handle_sync_field_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.surface != Surface::Settings || self.settings_tab != SettingsTab::Sync {
            return false;
        }
        let field = self.active_field;
        if self.sync_editor_mut(field).is_none() {
            return false;
        }
        let key = &event.keystroke;
        match key.key.as_str() {
            "enter" => match field {
                SettingsField::SyncLocation => self.save_sync_config(cx),
                SettingsField::SyncImportPassphrase => self.run_import(true, cx),
                _ => self.active_field.release(field),
            },
            "escape" | "tab" => {
                if field == SettingsField::SyncLocation && key.key == "escape" {
                    // Back to what is saved.
                    let saved = self
                        .sync
                        .status
                        .as_ref()
                        .and_then(|status| status.config.clone())
                        .unwrap_or_default();
                    if saved.location.is_empty() || saved.backend == self.sync.backend {
                        self.sync.location = text_editor(&saved.location);
                    }
                }
                self.active_field.release(field);
            }
            _ => {
                let Some(edit) = query_editor::edit_for(key) else {
                    return false;
                };
                let secret = field != SettingsField::SyncLocation;
                let editor = self.sync_editor_mut(field).expect("active sync field");
                match edit {
                    Edit::Local(local) => {
                        editor.apply(local);
                    }
                    // A passphrase never goes to the pasteboard.
                    Edit::Clipboard(ClipboardEdit::Copy) if !secret => {
                        query_editor::copy_selection(editor, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) if !secret => {
                        query_editor::cut_selection(editor, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy | ClipboardEdit::Cut) => {}
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                        {
                            let editor = self.sync_editor_mut(field).expect("active sync field");
                            editor.insert(text.lines().next().unwrap_or_default());
                        }
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    /// A one-line text field in the settings style. `secret` draws bullets.
    fn sync_text_field(
        &self,
        field: SettingsField,
        id: &'static str,
        placeholder: &'static str,
        width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = self.settings_colors();
        let active = self.active_field.is(field);
        let secret = field != SettingsField::SyncLocation;
        let editor = match field {
            SettingsField::SyncLocation => &self.sync.location,
            SettingsField::SyncExportPassphrase => &self.sync.export_passphrase,
            _ => match &self.sync.import {
                Some(draft) => &draft.passphrase,
                None => &self.sync.export_passphrase,
            },
        };
        let shown = if secret { masked(editor) } else { editor.clone() };
        let content = if active {
            query_label(&shown)
        } else if shown.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child(placeholder)
                .into_any_element()
        } else {
            div().child(shown.text().to_owned()).into_any_element()
        };
        div()
            .id(id)
            .debug_selector(move || id.into())
            .flex_none()
            .w(px(width))
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
            .when(!secret, |field| field.font_family(crate::fonts::mono_family()))
            .text_size(px(11.0))
            .text_color(colors.primary)
            .cursor(CursorStyle::IBeam)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.active_field = field;
                if let Some(editor) = this.sync_editor_mut(field) {
                    let end = editor.text().len();
                    editor.set_cursor(end, false);
                }
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .child(content)
    }

    pub(super) fn sync_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        settings_page(
            t("settings.tab.sync"),
            div()
                .flex()
                .flex_col()
                .gap(px(SETTINGS_SECTION_GAP))
                .child(self.profile_sync_section(cx))
                .child(self.migration_section(cx)),
            colors,
        )
    }

    fn profile_sync_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let sync = &self.sync;
        let configured = sync
            .status
            .as_ref()
            .is_some_and(|status| status.config.is_some());
        let backend = sync.backend;
        let segment = |id: &'static str, label: &'static str, value: SyncBackendKind| {
            let selected = backend == value;
            div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(24.0))
                .px(px(9.0))
                .rounded(px(Radius::CHIP))
                .flex()
                .items_center()
                .text_size(px(Typo::META.size))
                .text_color(if selected {
                    colors.primary
                } else {
                    colors.secondary
                })
                .when(selected, |segment| segment.bg(colors.primary.alpha(0.10)))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.choose_sync_backend(value, cx)))
                .child(label)
        };
        let backend_control = div()
            .flex_none()
            .p(px(2.0))
            .rounded(px(Radius::BADGE))
            .bg(colors.primary.alpha(0.05))
            .flex()
            .gap(px(2.0))
            .child(segment(
                "sync-backend-git",
                t("settings.sync.backend_git"),
                SyncBackendKind::Git,
            ))
            .child(segment(
                "sync-backend-folder",
                t("settings.sync.backend_folder"),
                SyncBackendKind::Folder,
            ));
        let (location_detail, placeholder) = match backend {
            SyncBackendKind::Git => (t("settings.sync.location_git_detail"), GIT_PLACEHOLDER),
            SyncBackendKind::Folder => (
                t("settings.sync.location_folder_detail"),
                FOLDER_PLACEHOLDER,
            ),
        };
        let status_line = match (sync.busy, &sync.status, &sync.error) {
            (Some(SyncBusy::Syncing), _, _) => t("settings.sync.syncing").to_owned(),
            (_, _, Some(error)) => error.clone(),
            (_, Some(status), None) => sync_status_line(status, now_millis()),
            (_, None, None) => t("settings.sync.not_set_up").to_owned(),
        };
        let failed = sync.error.is_some()
            || sync
                .status
                .as_ref()
                .is_some_and(|status| status.config.is_some() && status.last_error.is_some());
        let busy = sync.busy.is_some();
        let buttons = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .when(configured, |row| {
                row.child(surface_button(
                    t("settings.sync.turn_off"),
                    "sync-turn-off",
                    colors,
                    cx,
                    |this, cx| this.turn_sync_off(cx),
                ))
            })
            .child(
                div()
                    .when(busy, |button| button.opacity(0.5))
                    .child(surface_button(
                        if sync.busy == Some(SyncBusy::Syncing) {
                            t("settings.sync.syncing")
                        } else {
                            t("settings.sync.sync_now")
                        },
                        "sync-now",
                        colors,
                        cx,
                        |this, cx| this.sync_now(cx),
                    )),
            );
        let status_row = div()
            .debug_selector(|| "sync-status".into())
            .min_h(px(SETTINGS_ROW_HEIGHT))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(6.0))
                            .rounded(px(3.0))
                            .bg(if failed {
                                Ink::DANGER.alpha(0.85)
                            } else if configured {
                                Ink::FRESH.alpha(0.85)
                            } else {
                                colors.primary.alpha(0.18)
                            }),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .whitespace_normal()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .child(wrappable_setting_copy(status_line.into())),
                    ),
            )
            .child(buttons);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(setting_section(
                t("settings.sync.profile"),
                div()
                    .flex()
                    .flex_col()
                    .child(setting_row(
                        t("settings.sync.backend"),
                        t("settings.sync.backend_detail"),
                        backend_control,
                        colors,
                    ))
                    .child(setting_divider(colors))
                    .child(setting_row(
                        t("settings.sync.location"),
                        location_detail,
                        self.sync_text_field(
                            SettingsField::SyncLocation,
                            "sync-location",
                            placeholder,
                            LOCATION_FIELD_WIDTH,
                            cx,
                        ),
                        colors,
                    ))
                    .child(setting_divider(colors))
                    .child(toggle_row(
                        t("settings.sync.auto"),
                        t("settings.sync.auto_detail"),
                        sync.auto,
                        "sync-auto",
                        colors,
                        cx,
                        |this, cx| this.toggle_sync_auto(cx),
                    ))
                    .child(setting_divider(colors))
                    .child(status_row),
                colors,
            ))
            .child(
                div()
                    .px(px(1.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(t("settings.sync.footnote")),
            )
    }

    fn migration_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let sync = &self.sync;
        let export_detail = if sync.exporting {
            t("settings.sync.exporting").to_owned()
        } else {
            sync.export_line
                .clone()
                .unwrap_or_else(|| t("settings.sync.export_detail").to_owned())
        };
        let import_detail = match &sync.import {
            Some(draft) if draft.running && draft.plan.is_none() => {
                t("settings.sync.reading").to_owned()
            }
            Some(draft) => draft
                .path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            None => sync
                .import_line
                .clone()
                .unwrap_or_else(|| t("settings.sync.import_detail").to_owned()),
        };
        let mut card = div()
            .flex()
            .flex_col()
            .child(setting_row(
                t("settings.sync.export"),
                export_detail,
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(self.sync_text_field(
                        SettingsField::SyncExportPassphrase,
                        "sync-export-passphrase",
                        t("settings.sync.passphrase_optional"),
                        PASSPHRASE_FIELD_WIDTH,
                        cx,
                    ))
                    .child(surface_button(
                        t("settings.sync.export_button"),
                        "sync-export",
                        colors,
                        cx,
                        |this, cx| this.export_backup(cx),
                    )),
                colors,
            ))
            .child(setting_divider(colors))
            .child(setting_row(
                t("settings.sync.import"),
                import_detail,
                surface_button(
                    t("settings.sync.import_button"),
                    "sync-import",
                    colors,
                    cx,
                    |this, cx| this.choose_import_backup(cx),
                ),
                colors,
            ));
        if let Some(draft) = &sync.import {
            card = card.child(self.import_draft(draft, cx));
        }
        setting_section(t("settings.sync.move"), card, colors)
    }

    /// The dry run under Import Backup: what comes over, where each project
    /// lands, and the buttons that act on it.
    fn import_draft(&self, draft: &ImportDraft, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.settings_colors();
        let mut column = div().flex().flex_col();
        if draft.locked && draft.plan.is_none() {
            column = column.child(setting_divider(colors)).child(setting_row(
                t("settings.sync.passphrase"),
                draft
                    .error
                    .clone()
                    .unwrap_or_else(|| t("settings.sync.passphrase_needed").to_owned()),
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(self.sync_text_field(
                        SettingsField::SyncImportPassphrase,
                        "sync-import-passphrase",
                        t("settings.sync.passphrase"),
                        PASSPHRASE_FIELD_WIDTH,
                        cx,
                    ))
                    .child(surface_button(
                        t("settings.sync.unlock"),
                        "sync-import-unlock",
                        colors,
                        cx,
                        |this, cx| this.run_import(true, cx),
                    )),
                colors,
            ));
        } else if let Some(error) = &draft.error {
            column = column.child(setting_divider(colors)).child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(wrappable_setting_copy(error.clone().into())),
            );
        }
        let Some(plan) = &draft.plan else {
            return column.into_any_element();
        };
        let mut projects = div().flex().flex_col();
        for (index, project) in plan.projects.iter().enumerate() {
            if index > 0 {
                projects = projects.child(setting_divider(colors));
            }
            projects = projects.child(self.planned_project_row(index, project, cx));
        }
        let footer = div()
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .min_w(px(0.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(match &draft.error {
                        Some(error) => error.clone(),
                        None if draft.running => t("settings.sync.importing").to_owned(),
                        None => tf(
                            "settings.sync.from",
                            &[("device", &plan.source.device)],
                        ),
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(surface_button(
                        t("settings.sync.cancel"),
                        "sync-import-cancel",
                        colors,
                        cx,
                        |this, cx| this.cancel_import(cx),
                    ))
                    .child(settings_primary_button(
                        t("settings.sync.import_confirm"),
                        "sync-import-confirm",
                        None,
                        cx,
                        |this, _, cx| this.run_import(false, cx),
                    )),
            );
        column
            .child(setting_divider(colors))
            .child(import_counts(plan, colors))
            .when(!plan.projects.is_empty(), |column| {
                column.child(setting_divider(colors)).child(projects)
            })
            .child(setting_divider(colors))
            .child(footer)
            .into_any_element()
    }

    fn planned_project_row(
        &self,
        index: usize,
        project: &PlannedProject,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = self.settings_colors();
        let unresolved = project.how == ProjectMatch::Unresolved;
        let how = match project.how {
            ProjectMatch::Same => t("settings.sync.match_same"),
            ProjectMatch::Mapped => t("settings.sync.match_mapped"),
            ProjectMatch::Origin => t("settings.sync.match_origin"),
            ProjectMatch::Unresolved => t("settings.sync.match_unresolved"),
        };
        let path = |text: String, color: Rgba| {
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.0))
                .text_color(color)
                .child(text)
        };
        let from = project.from.clone();
        div()
            .id(SharedString::from(format!("sync-project-{index}")))
            .debug_selector(|| "sync-project".into())
            .h(px(36.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(path(project.from.clone(), colors.secondary))
            .child(sf_symbol("arrow.right", 9.0, colors.tertiary))
            .child(path(
                project.to.clone(),
                if unresolved {
                    colors.tertiary
                } else {
                    colors.primary
                },
            ))
            .child(
                div()
                    .flex_none()
                    .w(px(170.0))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(how),
                    )
                    .when(unresolved, |cell| {
                        cell.child(surface_button(
                            t("settings.sync.choose"),
                            SharedString::from(format!("sync-project-choose-{index}")),
                            colors,
                            cx,
                            move |this, cx| this.map_project(from.clone(), cx),
                        ))
                    }),
            )
    }

    /// Seeds the page for the headless screenshot fixtures.
    #[cfg(test)]
    pub(super) fn seed_sync_preview(&mut self, with_plan: bool) {
        use diri_proto::portable::{BundleSource, PortableCounts, SyncSummary};
        let config = SyncConfig {
            backend: SyncBackendKind::Git,
            location: "git@github.com:alice/diri-profile.git".into(),
            auto: true,
        };
        self.sync.backend = config.backend;
        self.sync.location = text_editor(&config.location);
        self.sync.auto = config.auto;
        self.sync.status = Some(SyncStatus {
            config: Some(config),
            device: "Alice's MacBook Pro".into(),
            last_synced_at: Some(diri_proto::model::DateMillis(now_millis() - 2.0 * 60_000.0)),
            last_summary: Some(SyncSummary {
                pulled: 3,
                pushed: 1,
                ..SyncSummary::default()
            }),
            last_error: None,
            running: false,
        });
        if !with_plan {
            return;
        }
        let project = |from: &str, to: &str, how, origin: Option<&str>, sessions| PlannedProject {
            from: from.into(),
            to: to.into(),
            how,
            git_origin: origin.map(str::to_owned),
            sessions,
        };
        let mut draft = ImportDraft::new(PathBuf::from(
            "/Users/bob/Downloads/Diri-Alices-MacBook-Pro-2026-11-23.diribundle",
        ));
        draft.plan = Some(MigrateImportPlan {
            source: BundleSource {
                device: "Alice's MacBook Pro".into(),
                home: "/Users/alice".into(),
                exported_at: None,
                diri_version: "0.12.0".into(),
                encrypted: false,
            },
            dry_run: true,
            bundle: PortableCounts {
                sessions: 42,
                transcripts: 38,
                notes: 12,
                skills: 3,
                recipes: 4,
                schedules: 2,
                projects: 3,
            },
            added: PortableCounts {
                sessions: 40,
                transcripts: 38,
                notes: 12,
                skills: 3,
                recipes: 4,
                schedules: 2,
                projects: 3,
            },
            updated: 0,
            kept: 2,
            conflicts: Vec::new(),
            projects: vec![
                project(
                    "~/work/repo",
                    "/Users/bob/code/repo",
                    ProjectMatch::Origin,
                    Some("github.com/acme/repo"),
                    24,
                ),
                project(
                    "~/work/site",
                    "/Users/bob/work/site",
                    ProjectMatch::Same,
                    None,
                    15,
                ),
                project(
                    "~/scratch/tmp",
                    "/Users/bob/scratch/tmp",
                    ProjectMatch::Unresolved,
                    None,
                    3,
                ),
            ],
            sessions: Vec::new(),
            skipped: Vec::new(),
            prefs_pending: false,
        });
        self.sync.import = Some(draft);
    }
}

/// "40 sessions to add · 2 already here · 38 conversations · …", each count
/// over its label; things the import does not bring are left out.
fn import_counts(plan: &MigrateImportPlan, colors: SemanticColors) -> impl IntoElement {
    let added = &plan.added;
    let already = plan
        .bundle
        .sessions
        .saturating_sub(added.sessions + plan.updated);
    let plural = |count: u32, one: &'static str, other: &'static str| {
        if count == 1 { t(one) } else { t(other) }
    };
    let stats = [
        (
            added.sessions + plan.updated,
            plural(
                added.sessions + plan.updated,
                "settings.sync.count_sessions_one",
                "settings.sync.count_sessions_other",
            ),
            true,
        ),
        (already, t("settings.sync.count_already"), already > 0),
        (
            added.transcripts,
            plural(
                added.transcripts,
                "settings.sync.count_conversations_one",
                "settings.sync.count_conversations_other",
            ),
            added.transcripts > 0,
        ),
        (
            added.notes,
            plural(
                added.notes,
                "settings.sync.count_notes_one",
                "settings.sync.count_notes_other",
            ),
            added.notes > 0,
        ),
        (
            added.skills,
            plural(
                added.skills,
                "settings.sync.count_skills_one",
                "settings.sync.count_skills_other",
            ),
            added.skills > 0,
        ),
        (
            added.recipes,
            plural(
                added.recipes,
                "settings.sync.count_recipes_one",
                "settings.sync.count_recipes_other",
            ),
            added.recipes > 0,
        ),
        (
            added.schedules,
            plural(
                added.schedules,
                "settings.sync.count_schedules_one",
                "settings.sync.count_schedules_other",
            ),
            added.schedules > 0,
        ),
    ];
    div()
        .debug_selector(|| "sync-import-counts".into())
        .px(px(12.0))
        .py(px(12.0))
        .flex()
        .flex_wrap()
        .gap_x(px(22.0))
        .gap_y(px(10.0))
        .children(stats.into_iter().filter(|(_, _, shown)| *shown).map(
            |(count, label, _)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.primary)
                            .child(count.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(label),
                    )
            },
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::model::DateMillis;
    use diri_proto::portable::SyncSummary;

    #[test]
    fn status_line_reads_like_a_sentence() {
        let now = 1_000_000_000_000.0;
        let mut status = SyncStatus::default();
        assert_eq!(sync_status_line(&status, now), "Not set up");
        status.config = Some(SyncConfig::default());
        assert_eq!(sync_status_line(&status, now), "Not synced yet");
        status.last_synced_at = Some(DateMillis(now - 2.0 * 60_000.0));
        status.last_summary = Some(SyncSummary {
            pulled: 3,
            pushed: 1,
            conflicts: vec!["notes/a.md".into()],
            ..SyncSummary::default()
        });
        assert_eq!(
            sync_status_line(&status, now),
            "Last synced 2 min ago · 3 in, 1 out · 1 conflict kept local"
        );
        status.last_error = Some("push rejected".into());
        assert_eq!(sync_status_line(&status, now), "push rejected");
    }

    #[test]
    fn masked_fields_keep_caret_and_selection() {
        let mut editor = text_editor("héllo");
        editor.set_cursor(2, false);
        let shown = masked(&editor);
        assert_eq!(shown.text(), "•••••");
        assert_eq!(shown.cursor(), '•'.len_utf8());
        editor.select_all();
        assert_eq!(masked(&editor).selection(), Some(0..5 * '•'.len_utf8()));
    }

    #[test]
    fn bundle_names_carry_a_date() {
        let name = default_bundle_name();
        assert!(name.starts_with("Diri-20") && name.ends_with(".diribundle"), "{name}");
    }
}
