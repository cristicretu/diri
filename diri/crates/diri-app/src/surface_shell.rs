use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::query_label;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::remote_access::{self, CompanionAccess};
use crate::settings::{HostDraft, SettingsTab, default_agent_label, theme};
use crate::store::{DefaultAgent, Prefs, SessionStore, StoreRuntime};
use crate::updates::{UpdateCommand, UpdateHandle, UpdatePhase};
use crate::worktrees::WorktreesSheet;
use diri_proto::{AgentKind as ProtoAgentKind, HistoryEntry, HostEntry, HostsConfig};
use diri_term::theme::TermTheme;
use diri_ui::{
    AgentLogo, Appearance, Fill, FloatingSurface, HairlineDivider, Ink, Metrics, Palette, Radius,
    SemanticColors, Space, Typo,
};
use gpui::{
    AnyElement, App, ClipboardItem, Context, CursorStyle, FocusHandle, Focusable, FontWeight,
    IntoElement, KeyDownEvent, MouseButton, Render, Rgba, SharedString, Task, Window, actions,
    deferred, div, prelude::*, px, rgba,
};
use tokio::runtime::Runtime;

const COLORS: SemanticColors = SemanticColors::dark();
const SETTINGS_COLORS: SemanticColors = SemanticColors::sidebar(Appearance::Dark);
const SETTINGS_WIDTH: f32 = 600.0;
const SETTINGS_HEIGHT: f32 = 420.0;
const SETTINGS_NAV_WIDTH: f32 = 150.0;
const SETTINGS_SECTION_GAP: f32 = 16.0;
const SETTINGS_ROW_HEIGHT: f32 = 50.0;
const RESULT_LIMIT: usize = 200;

actions!(
    diri,
    [
        ToggleHistory,
        OpenWorktrees,
        OpenSettings,
        CloseSurface,
        MoveUp,
        MoveDown,
        Activate
    ]
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Surface {
    #[default]
    None,
    History,
    Worktrees,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Remaining dropdowns are introduced incrementally.
enum SettingsMenu {
    DefaultAgent,
    TerminalTheme,
    HibernateAfter,
    MemoryLimit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HostFormField {
    #[default]
    Name,
    Ssh,
    DefaultCwd,
    NodeEndpoint,
    NodeTokenFile,
    NodeId,
}

impl HostFormField {
    const ALL: [Self; 6] = [
        Self::Name,
        Self::Ssh,
        Self::DefaultCwd,
        Self::NodeEndpoint,
        Self::NodeTokenFile,
        Self::NodeId,
    ];

    fn adjacent(self, backwards: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        let delta = if backwards { Self::ALL.len() - 1 } else { 1 };
        Self::ALL[(index + delta) % Self::ALL.len()]
    }
}

#[derive(Clone, Debug)]
struct HostEditor {
    original_id: Option<String>,
    name: QueryEditor,
    ssh: QueryEditor,
    default_cwd: QueryEditor,
    node_endpoint: QueryEditor,
    node_token_file: QueryEditor,
    node_id: QueryEditor,
    active_field: HostFormField,
    error: Option<String>,
    confirm_remove: bool,
}

impl HostEditor {
    fn adding() -> Self {
        Self::from_draft(HostDraft::new())
    }

    fn editing(host: &HostEntry) -> Self {
        Self::from_draft(HostDraft::editing(host))
    }

    fn from_draft(draft: HostDraft) -> Self {
        Self {
            original_id: draft.original_id,
            name: text_editor(&draft.name),
            ssh: text_editor(&draft.ssh),
            default_cwd: text_editor(&draft.default_cwd),
            node_endpoint: text_editor(&draft.node_endpoint),
            node_token_file: text_editor(&draft.node_token_file),
            node_id: text_editor(&draft.node_id),
            active_field: HostFormField::Name,
            error: None,
            confirm_remove: false,
        }
    }

    fn draft(&self) -> HostDraft {
        HostDraft {
            original_id: self.original_id.clone(),
            name: self.name.text().to_owned(),
            ssh: self.ssh.text().to_owned(),
            default_cwd: self.default_cwd.text().to_owned(),
            node_endpoint: self.node_endpoint.text().to_owned(),
            node_token_file: self.node_token_file.text().to_owned(),
            node_id: self.node_id.text().to_owned(),
        }
    }

    fn field_mut(&mut self) -> &mut QueryEditor {
        match self.active_field {
            HostFormField::Name => &mut self.name,
            HostFormField::Ssh => &mut self.ssh,
            HostFormField::DefaultCwd => &mut self.default_cwd,
            HostFormField::NodeEndpoint => &mut self.node_endpoint,
            HostFormField::NodeTokenFile => &mut self.node_token_file,
            HostFormField::NodeId => &mut self.node_id,
        }
    }
}

pub struct UtilitySurfaces {
    focus: FocusHandle,
    surface: Surface,
    history: Vec<HistoryEntry>,
    history_query: QueryEditor,
    history_highlight: usize,
    history_loading: bool,
    history_error: Option<String>,
    worktrees: WorktreesSheet,
    settings_tab: SettingsTab,
    settings_menu: Option<SettingsMenu>,
    home: PathBuf,
    hosts_path: PathBuf,
    hosts: Vec<HostEntry>,
    host_editor: Option<HostEditor>,
    prefs: Prefs,
    store: Arc<RwLock<SessionStore>>,
    companion_access: CompanionAccess,
    companion_busy: bool,
    companion_error: Option<String>,
    store_runtime: Arc<StoreRuntime>,
    runtime: Arc<Runtime>,
    updates: UpdateHandle,
    activity: String,
    _update_changes: Task<()>,
    _store_changes: Task<()>,
}

impl UtilitySurfaces {
    pub fn new(
        store_runtime: Arc<StoreRuntime>,
        runtime: Arc<Runtime>,
        updates: UpdateHandle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let hosts_path = diri_proto::paths::DirijorPaths::hosts_config_file(&home);
        let (prefs, hosts) = {
            let store = store_runtime
                .store
                .read()
                .expect("session store lock poisoned");
            (store.preferences().clone(), store.hosts().to_vec())
        };
        let companion_access = CompanionAccess::load(&home);
        let settings_preview = std::env::var("DIRI_SETTINGS_PREVIEW")
            .ok()
            .map(|value| value.to_ascii_lowercase());
        let settings_tab = match settings_preview.as_deref() {
            Some("terminal") => SettingsTab::Terminal,
            Some("resources") => SettingsTab::Resources,
            Some("remote") => SettingsTab::Remote,
            _ => SettingsTab::General,
        };
        // The Settings pane renders update state it does not own, so it has to
        // be woken when that state moves.
        let update_changes = {
            let mut states = updates.subscribe();
            cx.spawn(async move |this, cx| {
                while states.changed().await.is_ok() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            })
        };
        // This view is `.cached()` in RootView, so ambient window redraws no
        // longer reach it: store changes must notify it directly.
        let store_changes = {
            let mut changes = store_runtime.changes();
            cx.spawn(async move |this, cx| {
                loop {
                    match changes.recv().await {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if this.update(cx, |_, cx| cx.notify()).is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            })
        };
        Self {
            focus,
            surface: if settings_preview.is_some() {
                Surface::Settings
            } else {
                Surface::None
            },
            history: Vec::new(),
            history_query: QueryEditor::default(),
            history_highlight: 0,
            history_loading: false,
            history_error: None,
            worktrees: WorktreesSheet::default(),
            settings_tab,
            settings_menu: None,
            home,
            hosts_path,
            hosts,
            host_editor: None,
            prefs,
            store: Arc::clone(&store_runtime.store),
            companion_access,
            companion_busy: false,
            companion_error: None,
            store_runtime,
            runtime,
            updates,
            activity: "Connected client · shared daemon remains untouched".to_owned(),
            _update_changes: update_changes,
            _store_changes: store_changes,
        }
    }

    pub(crate) fn open_history(&mut self, cx: &mut Context<Self>) {
        self.surface = Surface::History;
        self.history_query.clear();
        self.history_highlight = 0;
        self.history_loading = true;
        self.history_error = None;
        cx.notify();

        let roots = crate::history::HistoryRoots::current_user();
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                let tracked = if client
                    .wait_until_connected(Duration::from_secs(5))
                    .await
                    .is_ok()
                {
                    client
                        .sessions()
                        .await
                        .map(|result| {
                            result
                                .sessions
                                .into_iter()
                                .filter_map(|session| session.agent_session_id)
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    HashSet::new()
                };
                tokio::task::spawn_blocking(move || crate::history::scan(&roots, &tracked))
                    .await
                    .map_err(|error| error.to_string())
            });
            let result = task
                .await
                .map_err(|error| error.to_string())
                .and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.history_loading = false;
                match result {
                    Ok(entries) => {
                        this.activity = format!("{} past conversations found", entries.len());
                        this.history = entries;
                    }
                    Err(error) => this.history_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn open_worktrees(&mut self, cx: &mut Context<Self>) {
        self.surface = Surface::Worktrees;
        self.refresh_worktrees(cx);
    }

    fn refresh_worktrees(&mut self, cx: &mut Context<Self>) {
        self.worktrees.begin_refresh();
        cx.notify();
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                client.worktree_overview().await
            });
            let result = match task.await {
                Ok(Ok(entries)) => Ok(entries),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                this.worktrees.finish_refresh(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn resume_history(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        let Some(params) = crate::history::resume_spawn(&entry) else {
            self.history_error = Some("The conversation folder is no longer available".to_owned());
            cx.notify();
            return;
        };
        self.history_loading = true;
        self.history_error = None;
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                client.spawn(params).await
            });
            let result = match task.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                this.history_loading = false;
                match result {
                    Ok(id) => {
                        this.surface = Surface::None;
                        this.activity = format!("Resumed conversation in session {id}");
                    }
                    Err(error) => this.history_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_cleanup(&mut self, cx: &mut Context<Self>) {
        let Some(params) = self.worktrees.confirm_cleanup() else {
            return;
        };
        self.worktrees.begin_refresh();
        let client = Arc::clone(self.store_runtime.client());
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                client.wait_until_connected(Duration::from_secs(5)).await?;
                client.worktree_remove(params).await?;
                client.worktree_overview().await
            });
            let result = match task.await {
                Ok(Ok(entries)) => Ok(entries),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                this.worktrees.finish_refresh(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn persist_prefs(&mut self) {
        self.prefs.normalize();
        let prefs = self.prefs.clone();
        if let Err(error) = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|shared| *shared = prefs)
        {
            self.activity = format!("Could not save settings: {error}");
        } else {
            self.activity = "Settings saved for diri".to_owned();
        }
    }

    fn reload_hosts(&mut self) {
        self.hosts = HostsConfig::load(&self.hosts_path).hosts;
        self.store
            .write()
            .expect("session store lock poisoned")
            .set_hosts(self.hosts.clone());
    }

    fn reload_companion_access(&mut self) {
        self.companion_access = CompanionAccess::load(&self.home);
        self.companion_error = None;
    }

    fn set_companion_access(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.companion_busy {
            return;
        }
        self.companion_busy = true;
        self.companion_error = None;
        self.activity = if enabled {
            "Enabling iPhone access…".to_owned()
        } else {
            "Disabling iPhone access…".to_owned()
        };
        cx.notify();

        let home = self.home.clone();
        let client = Arc::clone(self.store_runtime.client());
        let socket_path = client.socket_path().to_path_buf();
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let task = runtime.spawn(async move {
                let config_home = home.clone();
                let access = tokio::task::spawn_blocking(move || {
                    if enabled {
                        remote_access::enable(&config_home)
                    } else {
                        remote_access::disable(&config_home)
                    }
                })
                .await
                .map_err(|error| error.to_string())??;

                client
                    .wait_until_connected(Duration::from_secs(5))
                    .await
                    .map_err(|error| {
                        format!("Saved the setting, but the daemon is offline: {error}")
                    })?;
                client
                    .restart_daemon_for_remote_config()
                    .await
                    .map_err(|error| {
                        format!("Saved the setting, but could not reload the daemon: {error}")
                    })?;

                tokio::task::spawn_blocking(move || {
                    crate::daemon_launch::relaunch_after_remote_config_change(&socket_path);
                })
                .await
                .map_err(|error| error.to_string())?;
                Ok::<_, String>(access)
            });

            let result = task
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            let _ = this.update(cx, |this, cx| {
                this.companion_busy = false;
                match result {
                    Ok(access) => {
                        this.companion_access = access;
                        this.activity = if enabled {
                            "iPhone access is ready".to_owned()
                        } else {
                            "iPhone access disabled".to_owned()
                        };
                    }
                    Err(error) => {
                        // Reload in case writing the config succeeded but the
                        // explicit daemon restart failed.
                        this.companion_access = CompanionAccess::load(&this.home);
                        this.companion_error = Some(error.clone());
                        this.activity = error;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_pairing_link(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.companion_access.pairing_url() else {
            self.companion_error =
                Some("No reachable Tailscale endpoint is available for pairing yet.".to_owned());
            cx.notify();
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        self.activity = "Pairing link copied — open it on your iPhone".to_owned();
        self.companion_error = None;
        cx.notify();
    }

    fn begin_adding_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_menu = None;
        self.host_editor = Some(HostEditor::adding());
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn begin_editing_host(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|host| host.id == id) else {
            return;
        };
        self.settings_menu = None;
        self.host_editor = Some(HostEditor::editing(host));
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn select_host_field(
        &mut self,
        field: HostFormField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = &mut self.host_editor {
            editor.active_field = field;
            editor.error = None;
            editor.confirm_remove = false;
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    fn save_host(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.host_editor else {
            return;
        };
        let entry = match editor.draft().entry(&self.hosts) {
            Ok(entry) => entry,
            Err(error) => {
                if let Some(editor) = &mut self.host_editor {
                    editor.error = Some(error);
                    editor.confirm_remove = false;
                }
                cx.notify();
                return;
            }
        };
        let mut hosts = self.hosts.clone();
        if let Some(index) = hosts.iter().position(|host| host.id == entry.id) {
            hosts[index] = entry.clone();
        } else {
            hosts.push(entry.clone());
        }
        self.persist_hosts(hosts, format!("{} is ready", entry.display_name()), cx);
    }

    fn request_remove_host(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.host_editor else {
            return;
        };
        if !editor.confirm_remove {
            editor.confirm_remove = true;
            editor.error = None;
            cx.notify();
            return;
        }
        let Some(id) = editor.original_id.clone() else {
            return;
        };
        let in_use = self
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .values()
            .any(|session| session.host.as_deref() == Some(id.as_str()));
        if in_use {
            editor.confirm_remove = false;
            editor.error =
                Some("Move or close every session on this host before removing it.".to_owned());
            cx.notify();
            return;
        }
        let name = self
            .hosts
            .iter()
            .find(|host| host.id == id)
            .map_or_else(|| id.clone(), |host| host.display_name().to_owned());
        let hosts = self
            .hosts
            .iter()
            .filter(|host| host.id != id)
            .cloned()
            .collect();
        self.persist_hosts(hosts, format!("Removed {name}"), cx);
    }

    fn persist_hosts(&mut self, hosts: Vec<HostEntry>, activity: String, cx: &mut Context<Self>) {
        match (HostsConfig {
            hosts: hosts.clone(),
        })
        .save(&self.hosts_path)
        {
            Ok(()) => {
                self.hosts = hosts;
                self.store
                    .write()
                    .expect("session store lock poisoned")
                    .set_hosts(self.hosts.clone());
                self.host_editor = None;
                self.activity = activity;
            }
            Err(error) => {
                if let Some(editor) = &mut self.host_editor {
                    editor.error = Some(format!("Could not save hosts: {error}"));
                    editor.confirm_remove = false;
                }
            }
        }
        cx.notify();
    }

    fn edit_host_field(&mut self, edit: Edit, cx: &mut Context<Self>) {
        let Some(host_editor) = &mut self.host_editor else {
            return;
        };
        match edit {
            Edit::Local(local) => {
                host_editor.field_mut().apply(local);
            }
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(host_editor.field_mut(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Cut) => {
                query_editor::cut_selection(host_editor.field_mut(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Paste) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    host_editor.field_mut().insert(&text);
                }
            }
        }
        host_editor.error = None;
        host_editor.confirm_remove = false;
        cx.notify();
    }

    fn handle_host_editor_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.surface != Surface::Settings
            || self.settings_tab != SettingsTab::Remote
            || self.host_editor.is_none()
        {
            return false;
        }
        let key = &event.keystroke;
        match key.key.as_str() {
            "escape" => {
                if let Some(editor) = &mut self.host_editor
                    && editor.confirm_remove
                {
                    editor.confirm_remove = false;
                } else {
                    self.host_editor = None;
                }
                cx.notify();
            }
            "tab" => {
                if let Some(editor) = &mut self.host_editor {
                    editor.active_field = editor.active_field.adjacent(key.modifiers.shift);
                    editor.error = None;
                }
                cx.notify();
            }
            "enter" => self.save_host(cx),
            _ => {
                let Some(edit) = query_editor::edit_for(key) else {
                    return false;
                };
                self.edit_host_field(edit, cx);
            }
        }
        true
    }

    fn visible_history(&self) -> Vec<HistoryEntry> {
        self.history
            .iter()
            .filter(|entry| crate::history::matches_query(entry, self.history_query.text()))
            .take(RESULT_LIMIT)
            .cloned()
            .collect()
    }

    fn move_history(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.surface != Surface::History {
            return;
        }
        let count = self.visible_history().len();
        if count == 0 {
            return;
        }
        self.history_highlight =
            (self.history_highlight as isize + delta).rem_euclid(count as isize) as usize;
        cx.notify();
    }

    fn activate_history(&mut self, cx: &mut Context<Self>) {
        if self.surface != Surface::History {
            return;
        }
        if let Some(entry) = self.visible_history().get(self.history_highlight).cloned() {
            self.resume_history(entry, cx);
        }
    }

    fn close_surface(&mut self, cx: &mut Context<Self>) {
        if self.worktrees.pending_cleanup.is_some() {
            self.worktrees.cancel_cleanup();
        } else {
            self.surface = Surface::None;
            self.settings_menu = None;
            self.host_editor = None;
        }
        cx.notify();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.surface != Surface::None
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.prefs = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .clone();
        self.surface = Surface::Settings;
        self.settings_menu = None;
        self.host_editor = None;
        self.reload_companion_access();
        cx.notify();
    }

    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        if self.surface == Surface::History {
            self.close_surface(cx);
        } else {
            self.open_history(cx);
        }
    }

    pub(crate) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.surface == Surface::None {
            return;
        }
        if self.handle_host_editor_key(event, cx) {
            return;
        }
        let key = &event.keystroke;
        if key.key == "escape" && self.surface == Surface::Settings && self.settings_menu.is_some()
        {
            self.settings_menu = None;
            cx.notify();
        } else if key.key == "escape" {
            self.close_surface(cx);
        } else if key.key == "up" {
            self.move_history(-1, cx);
        } else if key.key == "down" {
            self.move_history(1, cx);
        } else if key.key == "enter" {
            self.activate_history(cx);
        } else if self.surface == Surface::History {
            let Some(edit) = query_editor::edit_for(key) else {
                return;
            };
            let changed = match edit {
                Edit::Local(local) => self.history_query.apply(local),
                Edit::Clipboard(ClipboardEdit::Copy) => {
                    query_editor::copy_selection(&self.history_query, cx);
                    false
                }
                Edit::Clipboard(ClipboardEdit::Cut) => {
                    query_editor::cut_selection(&mut self.history_query, cx)
                }
                Edit::Clipboard(ClipboardEdit::Paste) => cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .is_some_and(|text| self.history_query.insert(&text)),
            };
            if changed {
                self.history_highlight = 0;
            }
            cx.notify();
        }
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.visible_history();
        let empty = entries.is_empty() && !self.history_loading;
        let rows = entries.into_iter().enumerate().map(|(index, entry)| {
            let selected = index == self.history_highlight;
            let resumable = entry.cwd_exists;
            let folder = folder_name(&entry.cwd).to_owned();
            let parent = relative_parent(&entry.cwd);
            let title = entry
                .title
                .clone()
                .unwrap_or_else(|| "Untitled conversation".to_owned());
            let age = relative_time(entry.last_active_at.0);
            let agent = ui_agent(&entry.kind);
            div()
                .id(("history-row", index))
                .h(px(46.0))
                .px(px(10.0))
                .rounded(px(Radius::ROW))
                .flex()
                .items_center()
                .gap(px(9.0))
                .opacity(if resumable { 1.0 } else { 0.45 })
                .bg(Fill::selected(COLORS, selected))
                .cursor_pointer()
                .hover(|style| style.bg(Fill::hover(COLORS, true)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if resumable {
                        this.resume_history(entry.clone(), cx);
                    }
                }))
                .child(AgentLogo::new(agent, 18.0, COLORS))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(COLORS.primary)
                                .overflow_hidden()
                                .child(title),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(5.0))
                                .text_size(px(11.0))
                                .child(div().text_color(COLORS.secondary).child(folder))
                                .child(div().text_color(COLORS.tertiary).child(parent)),
                        ),
                )
                .child(chip(if !resumable {
                    "folder gone".to_owned()
                } else if selected {
                    "↵ resume".to_owned()
                } else {
                    age
                }))
        });

        FloatingSurface::new(
            COLORS,
            div()
                .w(px(560.0))
                .max_h(px(440.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(48.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .text_size(px(15.0))
                        .child(sf_symbol("magnifyingglass", 13.0, COLORS.tertiary))
                        .child(
                            div()
                                .flex_1()
                                .text_color(if self.history_query.is_empty() {
                                    COLORS.tertiary
                                } else {
                                    COLORS.primary
                                })
                                .child(if self.history_query.is_empty() {
                                    div().child("Search past conversations…").into_any_element()
                                } else {
                                    query_label(&self.history_query)
                                }),
                        )
                        .child(chip("esc".to_owned())),
                )
                .child(HairlineDivider::horizontal(COLORS))
                .child(
                    div()
                        .id("history-results")
                        .max_h(px(380.0))
                        .overflow_y_scroll()
                        .p(px(6.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .when(self.history_loading, |view| {
                            view.child(empty_label("Scanning Claude and Codex transcripts…"))
                        })
                        .when_some(self.history_error.clone(), |view, error| {
                            view.child(empty_label(&error))
                        })
                        .when(empty, |view| {
                            view.child(empty_label(if self.history_query.is_empty() {
                                "No past conversations"
                            } else {
                                "No matches"
                            }))
                        })
                        .children(rows),
                ),
        )
    }

    fn render_worktrees(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = self
            .worktrees
            .entries
            .iter()
            .map(|entry| {
                let path = entry.path.clone();
                let branch = entry
                    .branch
                    .clone()
                    .unwrap_or_else(|| "detached".to_owned());
                let project = folder_name(&entry.project_root).to_owned();
                let stale = entry.stale_suggestion;
                div()
                    .w(px(278.0))
                    .min_h(px(132.0))
                    .p(px(12.0))
                    .rounded(px(Radius::CARD))
                    .border_1()
                    .border_color(if stale {
                        Ink::ATTENTION.alpha(0.6)
                    } else {
                        COLORS.primary.alpha(0.06)
                    })
                    .bg(Fill::subtle(COLORS))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(sf_symbol("arrow.branch", 13.0, COLORS.secondary))
                            .child(branch),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(COLORS.tertiary)
                            .child(project),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(5.0))
                            .when(entry.dirty, |row| {
                                row.child(colored_badge("dirty", Ink::ATTENTION))
                            })
                            .when(entry.merged, |row| {
                                row.child(colored_badge("merged", Ink::FRESH))
                            })
                            .child(chip(format!("{}d", entry.age_days))),
                    )
                    .when(stale, |card| {
                        card.child(
                            div()
                                .id(SharedString::from(format!("cleanup-{path}")))
                                .text_size(px(11.0))
                                .text_color(Ink::DANGER)
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.worktrees.request_cleanup(&path);
                                    cx.notify();
                                }))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(sf_symbol_weighted(
                                    "trash",
                                    11.0,
                                    SymbolWeight::Regular,
                                    Ink::DANGER,
                                ))
                                .child("Clean Up"),
                        )
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let pending = self.worktrees.pending_cleanup.clone();
        FloatingSurface::new(
            COLORS,
            div()
                .w(px(620.0))
                .h(px(460.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(52.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(15.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Worktrees"),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.0))
                                .child(surface_button(
                                    "Refresh",
                                    "refresh-worktrees",
                                    cx,
                                    |this, cx| {
                                        this.refresh_worktrees(cx);
                                    },
                                ))
                                .child(surface_button("Done", "done-worktrees", cx, |this, cx| {
                                    this.close_surface(cx);
                                })),
                        ),
                )
                .child(HairlineDivider::horizontal(COLORS))
                .child(
                    div()
                        .id("worktree-cards")
                        .flex_1()
                        .overflow_y_scroll()
                        .p(px(16.0))
                        .flex()
                        .flex_wrap()
                        .content_start()
                        .gap(px(12.0))
                        .when(self.worktrees.loading, |view| {
                            view.child(empty_label("Refreshing worktrees…"))
                        })
                        .when_some(self.worktrees.error.clone(), |view, error| {
                            view.child(empty_label(&error))
                        })
                        .when(
                            self.worktrees.entries.is_empty() && !self.worktrees.loading,
                            |view| view.child(empty_label("No worktrees")),
                        )
                        .children(cards),
                )
                .when_some(pending, |sheet, entry| {
                    sheet.child(
                        div()
                            .absolute()
                            .inset_0()
                            .occlude()
                            .bg(rgba(0x00000088))
                            .flex()
                            .items_end()
                            .justify_center()
                            .pb(px(18.0))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.worktrees.cancel_cleanup();
                                    cx.notify();
                                    cx.stop_propagation();
                                }),
                            )
                            .child(FloatingSurface::new(
                                COLORS,
                                div()
                                    .w(px(560.0))
                                    .p(px(16.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(12.0))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.0))
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child("Remove worktree?"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.0))
                                                    .text_color(COLORS.secondary)
                                                    .child(format!(
                                                        "{} is merged and clean.",
                                                        entry.path
                                                    )),
                                            ),
                                    )
                                    .child(surface_button(
                                        "Cancel",
                                        "cancel-cleanup",
                                        cx,
                                        |this, cx| {
                                            this.worktrees.cancel_cleanup();
                                            cx.notify();
                                        },
                                    ))
                                    .child(danger_button("Remove Worktree", cx, |this, cx| {
                                        this.confirm_cleanup(cx);
                                    })),
                            )),
                    )
                }),
        )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = SettingsTab::ALL
            .into_iter()
            .map(|tab| {
                let selected = tab == self.settings_tab;
                div()
                    .id(SharedString::from(format!("settings-{}", tab.label())))
                    .h(px(30.0))
                    .px(px(8.0))
                    .rounded(px(Radius::ROW))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .bg(Fill::selected(SETTINGS_COLORS, selected))
                    .text_color(if selected {
                        SETTINGS_COLORS.primary
                    } else {
                        SETTINGS_COLORS.secondary
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(Fill::hover(SETTINGS_COLORS, true)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_tab = tab;
                        this.settings_menu = None;
                        this.host_editor = None;
                        if tab == SettingsTab::Remote {
                            this.reload_hosts();
                            this.reload_companion_access();
                        }
                        cx.notify();
                    }))
                    .child(sf_symbol(
                        tab.icon(),
                        13.0,
                        if selected {
                            SETTINGS_COLORS.primary
                        } else {
                            SETTINGS_COLORS.tertiary
                        },
                    ))
                    .child(
                        div()
                            .text_size(px(Typo::ROW.size))
                            .font_weight(if selected {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(tab.label()),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let pane = match self.settings_tab {
            SettingsTab::General => self.general_settings(cx).into_any_element(),
            SettingsTab::Terminal => self.terminal_settings(cx).into_any_element(),
            SettingsTab::Resources => self.resource_settings(cx).into_any_element(),
            SettingsTab::Remote => self.remote_settings(cx).into_any_element(),
        };
        FloatingSurface::new(
            SETTINGS_COLORS,
            div()
                .id("settings-dialog")
                .debug_selector(|| "settings-dialog".into())
                .w(px(SETTINGS_WIDTH))
                .h(px(SETTINGS_HEIGHT))
                .rounded(px(Radius::PANEL))
                .overflow_hidden()
                .flex()
                // The dialog owns clicks within its bounds. When a select is
                // open, any click elsewhere in Settings dismisses that select
                // before the clicked control runs its own action.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        if this.settings_menu.take().is_some() {
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .w(px(SETTINGS_NAV_WIDTH))
                        .h_full()
                        .bg(SETTINGS_COLORS.primary.alpha(0.018))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .h(px(Metrics::TITLE_BAR))
                                .px(px(Metrics::TOOLBAR_EDGE_INSET))
                                .flex()
                                .items_center()
                                .gap(px(Metrics::TOOLBAR_ITEM_GAP))
                                .child(sf_symbol("gearshape", 15.0, SETTINGS_COLORS.secondary))
                                .child(
                                    div()
                                        .text_size(px(Typo::TITLE.size))
                                        .font_weight(Typo::TITLE.weight)
                                        .child("Settings"),
                                ),
                        )
                        .child(
                            div()
                                .px(px(Space::INSET))
                                .pt(px(2.0))
                                .pb(px(10.0))
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .children(tabs),
                        )
                        .child(
                            div()
                                .px(px(Metrics::TOOLBAR_EDGE_INSET))
                                .pb(px(10.0))
                                .text_size(px(Typo::META.size))
                                .text_color(SETTINGS_COLORS.tertiary)
                                .child(format!("diri {}", crate::updates::CURRENT_VERSION)),
                        ),
                )
                .child(HairlineDivider::vertical(SETTINGS_COLORS))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w(px(0.0))
                        .h_full()
                        .child(
                            div()
                                .id("settings-pane")
                                .absolute()
                                .inset_0()
                                .overflow_y_scroll()
                                .child(pane),
                        )
                        .child(
                            div()
                                .id("close-settings")
                                .debug_selector(|| "close-settings".into())
                                .absolute()
                                .top(px(8.0))
                                .right(px(Metrics::TOOLBAR_EDGE_INSET))
                                .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                                .rounded(px(Radius::BADGE))
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .hover(|style| style.bg(Fill::subtle(SETTINGS_COLORS)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_surface(cx);
                                }))
                                .child(sf_symbol_weighted(
                                    "xmark",
                                    13.5,
                                    SymbolWeight::Bold,
                                    SETTINGS_COLORS.secondary,
                                )),
                        ),
                ),
        )
    }

    fn general_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let quick_open_roots = if self.prefs.quick_open_roots.is_empty() {
            "~/fun".to_owned()
        } else {
            self.prefs.quick_open_roots.clone()
        };
        settings_page(
            "General",
            div()
                .flex()
                .flex_col()
                .gap(px(SETTINGS_SECTION_GAP))
                .child(setting_section(
                    "New sessions",
                    setting_row(
                        "Default agent",
                        "Used by ⌘T and Quick Open.",
                        self.default_agent_dropdown(cx),
                    ),
                ))
                .child(setting_section(
                    "Behavior",
                    div()
                        .flex()
                        .flex_col()
                        .child(toggle_row(
                            "Start diri at login",
                            "Open diri automatically after you sign in.",
                            self.prefs.start_at_login,
                            "toggle-login",
                            cx,
                            |this, cx| {
                                this.prefs.start_at_login = !this.prefs.start_at_login;
                                this.persist_prefs();
                                cx.notify();
                            },
                        ))
                        .child(setting_divider())
                        .child(toggle_row(
                            "Confirm before closing a session",
                            "Ask before closing a session with a running process.",
                            self.prefs.confirm_before_closing_session,
                            "toggle-close-confirm",
                            cx,
                            |this, cx| {
                                this.prefs.confirm_before_closing_session =
                                    !this.prefs.confirm_before_closing_session;
                                this.persist_prefs();
                                cx.notify();
                            },
                        ))
                        .child(setting_divider())
                        .child(toggle_row(
                            "Chime on agent status changes",
                            "Play a sound when an agent needs input or finishes.",
                            self.prefs.status_sounds,
                            "toggle-status-sounds",
                            cx,
                            |this, cx| {
                                this.prefs.status_sounds = !this.prefs.status_sounds;
                                this.persist_prefs();
                                cx.notify();
                            },
                        )),
                ))
                .child(self.update_settings(cx))
                .child(setting_section(
                    "Quick Open",
                    div()
                        .p(px(12.0))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .child("Search roots"),
                        )
                        .child(
                            div()
                                .p(px(10.0))
                                .rounded(px(Radius::BADGE))
                                .bg(SETTINGS_COLORS.primary.alpha(0.055))
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(11.0))
                                .line_height(px(17.0))
                                .text_color(SETTINGS_COLORS.secondary)
                                .child(quick_open_roots),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(16.0))
                                .text_color(SETTINGS_COLORS.tertiary)
                                .child(if self.prefs.quick_open_roots.is_empty() {
                                    "Using the default folder plus project parent folders."
                                } else {
                                    "One folder per line, scanned four levels deep."
                                }),
                        ),
                )),
        )
    }

    fn default_agent_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.prefs.default_agent;
        let open = self.settings_menu == Some(SettingsMenu::DefaultAgent);
        let trigger = settings_select_button(
            default_agent_label(selected),
            "default-agent-dropdown",
            open,
            SettingsMenu::DefaultAgent,
            cx,
        );

        let mut control = div().relative().min_w(px(154.0)).child(trigger);
        if open {
            let mut options = div().p(px(4.0)).flex().flex_col();
            for (index, agent) in DefaultAgent::ALL.into_iter().enumerate() {
                let is_selected = agent == selected;
                options = options.child(
                    div()
                        .id(SharedString::from(format!("default-agent-option-{index}")))
                        .h(px(Metrics::ROW_HEIGHT))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(Radius::ROW))
                        .bg(Fill::selected(SETTINGS_COLORS, is_selected))
                        .cursor_pointer()
                        .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.08)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.prefs.default_agent = agent;
                            this.settings_menu = None;
                            this.persist_prefs();
                            cx.notify();
                        }))
                        .child(
                            AgentLogo::new(ui_default_agent(agent), 16.0, SETTINGS_COLORS)
                                .badged(false),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(Typo::ROW.size))
                                .text_color(SETTINGS_COLORS.primary)
                                .child(default_agent_label(agent)),
                        )
                        .when(is_selected, |row| {
                            row.child(sf_symbol_weighted(
                                "checkmark",
                                10.0,
                                SymbolWeight::Semibold,
                                SETTINGS_COLORS.secondary,
                            ))
                        }),
                );
            }
            control = control.child(settings_dropdown(options, 204.0));
        }
        control.into_any_element()
    }

    /// The UPDATES block in Settings → General.
    ///
    /// Only the *check* is automatic. Downloading and restarting stay behind
    /// the button here (and the sidebar pill), because diri holds live agent
    /// sessions and relaunching underneath them uninvited would be hostile.
    fn update_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.updates.state();
        let unsupported = matches!(state.phase, UpdatePhase::Unsupported(_));
        let action = match &state.phase {
            UpdatePhase::Available(_) => Some(("Download", UpdateCommand::Download)),
            UpdatePhase::Ready(_) => Some(("Restart", UpdateCommand::Install)),
            UpdatePhase::Checking | UpdatePhase::Downloading { .. } | UpdatePhase::Installing => {
                None
            }
            _ if unsupported => None,
            _ => Some((
                "Check Now",
                UpdateCommand::Check {
                    user_initiated: true,
                },
            )),
        };

        let summary = if unsupported {
            format!("diri {}", crate::updates::CURRENT_VERSION)
        } else {
            state.summary()
        };
        let action_control = div().when_some(action, |control, (label, command)| {
            control.child(surface_button(
                label,
                "update-action",
                cx,
                move |this, _| {
                    this.updates.send(command.clone());
                },
            ))
        });
        let mut rows = div().flex().flex_col().child(setting_row(
            summary,
            update_detail(&state, unsupported),
            action_control,
        ));

        // "Skip" only makes sense for a release that is offered but not yet
        // downloaded; once it is staged the bytes are already on disk.
        if let UpdatePhase::Available(release) = &state.phase {
            let version = release.version.clone();
            rows = rows.child(setting_divider()).child(setting_row(
                "Skip this version",
                "Hide this release until a newer version is available.",
                surface_button("Skip", "skip-update", cx, move |this, cx| {
                    this.prefs.skipped_update_version = version.clone();
                    this.persist_prefs();
                    this.updates.send(UpdateCommand::Skip);
                    cx.notify();
                }),
            ));
        }
        if !unsupported {
            rows = rows.child(setting_divider()).child(toggle_row(
                "Check automatically",
                "Look for new releases in the background.",
                self.prefs.automatic_updates,
                "toggle-automatic-updates",
                cx,
                |this, cx| {
                    this.prefs.automatic_updates = !this.prefs.automatic_updates;
                    this.persist_prefs();
                    this.updates
                        .send(UpdateCommand::SetAutomatic(this.prefs.automatic_updates));
                    cx.notify();
                },
            ));
        }
        setting_section("Software updates", rows)
    }

    fn terminal_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = theme(&self.prefs.terminal_theme);
        let font_control = div()
            .h(px(28.0))
            .rounded(px(Radius::BADGE))
            .border_1()
            .border_color(SETTINGS_COLORS.primary.alpha(0.11))
            .bg(SETTINGS_COLORS.primary.alpha(0.045))
            .flex()
            .items_center()
            .child(
                div()
                    .id("font-smaller")
                    .w(px(30.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(15.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.08)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.prefs.zoom_terminal(-1.0);
                        this.persist_prefs();
                        cx.notify();
                    }))
                    .child("−"),
            )
            .child(HairlineDivider::vertical(SETTINGS_COLORS))
            .child(
                div()
                    .w(px(52.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(11.0))
                    .text_color(SETTINGS_COLORS.secondary)
                    .child(format!("{:.0} pt", self.prefs.terminal_font_size)),
            )
            .child(HairlineDivider::vertical(SETTINGS_COLORS))
            .child(
                div()
                    .id("font-larger")
                    .w(px(30.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(15.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.08)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.prefs.zoom_terminal(1.0);
                        this.persist_prefs();
                        cx.notify();
                    }))
                    .child("+"),
            );

        settings_page(
            "Terminal",
            div()
                .flex()
                .flex_col()
                .gap(px(SETTINGS_SECTION_GAP))
                .child(setting_section(
                    "Appearance",
                    div()
                        .flex()
                        .flex_col()
                        .child(setting_row(
                            "Color theme",
                            "Applies immediately to every open terminal.",
                            self.terminal_theme_dropdown(cx),
                        ))
                        .child(setting_divider())
                        .child(div().p(px(12.0)).child(theme_preview(selected))),
                ))
                .child(setting_section(
                    "Text",
                    setting_row(
                        "Font size",
                        "Adjust terminal text without changing the rest of the app.",
                        font_control,
                    ),
                )),
        )
    }

    fn resource_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        settings_page(
            "Resources",
            div()
                .flex()
                .flex_col()
                .gap(px(SETTINGS_SECTION_GAP))
                .child(setting_section(
                    "Session limits",
                    div()
                        .flex()
                        .flex_col()
                        .child(setting_row(
                            "Hibernate idle sessions",
                            "Freeze inactive sessions after this amount of time.",
                            self.hibernate_dropdown(cx),
                        ))
                        .child(setting_divider())
                        .child(setting_row(
                            "Memory limit",
                            "Freeze an individual session when it reaches this size.",
                            self.memory_dropdown(cx),
                        )),
                ))
                .child(
                    div()
                        .p(px(12.0))
                        .rounded(px(Radius::ROW))
                        .border_1()
                        .border_color(SETTINGS_COLORS.primary.alpha(0.07))
                        .bg(SETTINGS_COLORS.primary.alpha(0.035))
                        .flex()
                        .items_start()
                        .gap(px(10.0))
                        .child(sf_symbol(
                            "moon.fill",
                            15.0,
                            SETTINGS_COLORS.tertiary,
                        ))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(17.0))
                                .text_color(SETTINGS_COLORS.secondary)
                                .child("Frozen sessions are never killed. Opening one wakes it immediately, exactly where you left it."),
                        ),
                ),
        )
    }

    fn remote_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let configured = self.companion_access.is_configured();
        let endpoint = self.companion_access.endpoint_label();
        let action = if self.companion_busy {
            div()
                .h(px(26.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(SETTINGS_COLORS.tertiary)
                .child("Applying…")
                .into_any_element()
        } else if configured {
            surface_button("Disable", "disable-companion", cx, |this, cx| {
                this.set_companion_access(false, cx);
            })
            .into_any_element()
        } else {
            surface_button("Enable", "enable-companion", cx, |this, cx| {
                this.set_companion_access(true, cx);
            })
            .into_any_element()
        };

        let mut companion_rows = div().flex().flex_col().child(setting_row(
            "Connect this iPhone",
            endpoint,
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(colored_badge(
                    if configured { "Ready" } else { "Off" },
                    if configured {
                        Ink::FRESH
                    } else {
                        SETTINGS_COLORS.secondary
                    },
                ))
                .child(action),
        ));

        if configured {
            companion_rows = companion_rows.child(setting_divider()).child(setting_row(
                "Pair this iPhone",
                "Copy the secure link, then open it on your phone. The token stays out of the UI.",
                surface_button("Copy Link", "copy-pairing-link", cx, |this, cx| {
                    this.copy_pairing_link(cx);
                }),
            ));
        }
        if let Some(error) = &self.companion_error {
            companion_rows = companion_rows.child(setting_divider()).child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .text_size(px(11.0))
                    .line_height(px(16.0))
                    .text_color(Ink::ATTENTION)
                    .child(sf_symbol("exclamationmark.triangle", 13.0, Ink::ATTENTION))
                    .child(error.clone()),
            );
        }

        settings_page(
            "Remote",
            div()
                .flex()
                .flex_col()
                .gap(px(SETTINGS_SECTION_GAP))
                .child(self.remote_hosts_section(cx))
                .child(setting_section("Companion access", companion_rows))
                .child(
                    div()
                        .p(px(12.0))
                        .rounded(px(Radius::ROW))
                        .border_1()
                        .border_color(Ink::ATTENTION.alpha(0.25))
                        .bg(Ink::ATTENTION.alpha(0.045))
                        .flex()
                        .items_start()
                        .gap(px(10.0))
                        .child(sf_symbol("exclamationmark.triangle", 15.0, Ink::ATTENTION))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(
                                    div()
                                        .text_size(px(Typo::TITLE.size))
                                        .font_weight(Typo::TITLE.weight)
                                        .child("Private by default"),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .line_height(px(17.0))
                                        .text_color(SETTINGS_COLORS.secondary)
                                        .child("The listener binds only to this Mac's Tailscale IPv4 address. Changing access briefly reloads the daemon while holder-owned sessions stay alive."),
                                ),
                        ),
                ),
        )
    }

    fn remote_hosts_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut catalog = div()
            .rounded(px(Radius::ROW))
            .border_1()
            .border_color(SETTINGS_COLORS.primary.alpha(0.065))
            .bg(SETTINGS_COLORS.primary.alpha(0.02))
            .overflow_hidden();

        if let Some(editor) = &self.host_editor {
            catalog = catalog.child(self.host_editor_panel(editor, cx));
        } else if self.hosts.is_empty() {
            catalog = catalog.child(
                div()
                    .px(px(14.0))
                    .py(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .size(px(34.0))
                            .rounded(px(10.0))
                            .bg(SETTINGS_COLORS.primary.alpha(0.065))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(sf_symbol("network", 17.0, SETTINGS_COLORS.tertiary)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("No execution hosts yet"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(SETTINGS_COLORS.tertiary)
                                    .child("Add any machine you can reach over SSH."),
                            ),
                    ),
            );
        } else {
            for (index, host) in self.hosts.iter().enumerate() {
                if index > 0 {
                    catalog = catalog.child(setting_divider());
                }
                let id = host.id.clone();
                let name = host.display_name().to_owned();
                let destination = host.ssh.clone();
                let folder = host.default_cwd.clone();
                let first_party = host.node.is_some();
                catalog = catalog.child(
                    div()
                        .id(SharedString::from(format!("remote-host-{id}")))
                        .min_h(px(SETTINGS_ROW_HEIGHT))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(11.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.055)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.begin_editing_host(&id, window, cx);
                        }))
                        .child(
                            div()
                                .size(px(30.0))
                                .rounded(px(9.0))
                                .bg(SETTINGS_COLORS.primary.alpha(0.06))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(sf_symbol("network", 15.0, SETTINGS_COLORS.secondary)),
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
                                        .flex()
                                        .items_center()
                                        .gap(px(7.0))
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(name)
                                        .when(first_party, |row| {
                                            row.child(
                                                div()
                                                    .px(px(6.0))
                                                    .py(px(2.0))
                                                    .rounded(px(Radius::BADGE))
                                                    .bg(Ink::FRESH.alpha(0.10))
                                                    .text_size(px(9.0))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(Ink::FRESH)
                                                    .child("NODE"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(7.0))
                                        .font_family(crate::fonts::mono_family())
                                        .text_size(px(10.5))
                                        .text_color(SETTINGS_COLORS.tertiary)
                                        .child(destination)
                                        .when_some(folder, |line, folder| {
                                            line.child(
                                                div()
                                                    .text_color(SETTINGS_COLORS.primary.alpha(0.22))
                                                    .child("·"),
                                            )
                                            .child(folder)
                                        }),
                                ),
                        )
                        .child(sf_symbol("chevron.right", 10.0, SETTINGS_COLORS.tertiary)),
                );
            }
        }

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .h(px(28.0))
                    .px(px(2.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Typo::SECTION_HEADER.size))
                            .font_weight(Typo::SECTION_HEADER.weight)
                            .text_color(SETTINGS_COLORS.tertiary)
                            .child("Execution hosts"),
                    )
                    .when(self.host_editor.is_none(), |header| {
                        header.child(settings_primary_button(
                            "Add Host",
                            "add-remote-host",
                            Some("plus"),
                            cx,
                            |this, window, cx| this.begin_adding_host(window, cx),
                        ))
                    }),
            )
            .child(catalog)
    }

    fn host_editor_panel(&self, editor: &HostEditor, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = editor.original_id.is_some();
        let title = if editing { "Edit host" } else { "Add a host" };
        let mut form = div()
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(Typo::TITLE.size))
                                    .font_weight(Typo::TITLE.weight)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(SETTINGS_COLORS.tertiary)
                                    .child("SSH aliases from ~/.ssh/config work too."),
                            ),
                    )
                    .child(
                        div()
                            .id("cancel-host-editor")
                            .size(px(24.0))
                            .rounded(px(Radius::BADGE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.08)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.host_editor = None;
                                cx.notify();
                            }))
                            .child(sf_symbol("xmark", 11.0, SETTINGS_COLORS.secondary)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(10.0))
                            .child(div().flex_1().child(self.host_text_field(
                                "Name",
                                "Forge",
                                &editor.name,
                                HostFormField::Name,
                                cx,
                            )))
                            .child(div().flex_1().child(self.host_text_field(
                                "SSH destination",
                                "you@forge",
                                &editor.ssh,
                                HostFormField::Ssh,
                                cx,
                            ))),
                    )
                    .child(self.host_text_field(
                        "Default folder",
                        "~/code (optional)",
                        &editor.default_cwd,
                        HostFormField::DefaultCwd,
                        cx,
                    ))
                    .child(
                        div()
                            .pt(px(2.0))
                            .text_size(px(10.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(SETTINGS_COLORS.secondary)
                            .child("First-party node (optional)"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(10.0))
                            .child(div().flex_1().child(self.host_text_field(
                                "Node endpoint",
                                "tcp://100.64.0.2:7337",
                                &editor.node_endpoint,
                                HostFormField::NodeEndpoint,
                                cx,
                            )))
                            .child(div().flex_1().child(self.host_text_field(
                                "Local token file",
                                "~/.config/dirijor/forge.token",
                                &editor.node_token_file,
                                HostFormField::NodeTokenFile,
                                cx,
                            ))),
                    )
                    .child(self.host_text_field(
                        "Pinned node ID",
                        "node-a1b2c3d4 (recommended after first hello)",
                        &editor.node_id,
                        HostFormField::NodeId,
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .line_height(px(15.0))
                            .text_color(SETTINGS_COLORS.tertiary)
                            .child("The token stays in that owner-only file. SSH remains available for install and recovery."),
                    ),
            );

        if let Some(error) = &editor.error {
            form = form.child(
                div()
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(Radius::BADGE))
                    .bg(Ink::DANGER.alpha(0.08))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_size(px(11.0))
                    .text_color(Ink::DANGER)
                    .child(sf_symbol("exclamationmark.triangle", 12.0, Ink::DANGER))
                    .child(error.clone()),
            );
        }

        if editor.confirm_remove {
            let name = if editor.name.is_empty() {
                "this host".to_owned()
            } else {
                editor.name.text().to_owned()
            };
            form = form.child(
                div()
                    .pt(px(2.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(SETTINGS_COLORS.secondary)
                            .child(format!(
                                "Remove {name}? Existing sessions must be moved first."
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(surface_button("Keep Host", "keep-host", cx, |this, cx| {
                                if let Some(editor) = &mut this.host_editor {
                                    editor.confirm_remove = false;
                                }
                                cx.notify();
                            }))
                            .child(settings_danger_button(
                                "Remove Host",
                                "confirm-remove-host",
                                cx,
                                |this, cx| this.request_remove_host(cx),
                            )),
                    ),
            );
        } else {
            form = form.child(
                div()
                    .pt(px(2.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().when(editing, |left| {
                        left.child(settings_danger_button(
                            "Remove",
                            "remove-host",
                            cx,
                            |this, cx| this.request_remove_host(cx),
                        ))
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(surface_button("Cancel", "cancel-host", cx, |this, cx| {
                                this.host_editor = None;
                                cx.notify();
                            }))
                            .child(settings_primary_button(
                                if editing { "Save Host" } else { "Add Host" },
                                "save-host",
                                None,
                                cx,
                                |this, _, cx| this.save_host(cx),
                            )),
                    ),
            );
        }
        form
    }

    fn host_text_field(
        &self,
        label: &'static str,
        placeholder: &'static str,
        editor: &QueryEditor,
        field: HostFormField,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self
            .host_editor
            .as_ref()
            .is_some_and(|host_editor| host_editor.active_field == field);
        let value = if editor.is_empty() {
            div()
                .flex()
                .items_center()
                .text_color(SETTINGS_COLORS.tertiary)
                .when(active, |value| value.child(query_label(editor)))
                .child(placeholder)
                .into_any_element()
        } else if active {
            query_label(editor)
        } else {
            div().child(editor.text().to_owned()).into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(SETTINGS_COLORS.secondary)
                    .child(label),
            )
            .child(
                div()
                    .id(SharedString::from(format!("host-field-{field:?}")))
                    .h(px(34.0))
                    .px(px(10.0))
                    .rounded(px(Radius::BADGE))
                    .border_1()
                    .border_color(
                        SETTINGS_COLORS
                            .primary
                            .alpha(if active { 0.26 } else { 0.11 }),
                    )
                    .bg(SETTINGS_COLORS
                        .primary
                        .alpha(if active { 0.075 } else { 0.04 }))
                    .flex()
                    .items_center()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(11.0))
                    .text_color(SETTINGS_COLORS.primary)
                    .cursor(CursorStyle::IBeam)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_host_field(field, window, cx);
                    }))
                    .child(value),
            )
    }

    fn terminal_theme_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = theme(&self.prefs.terminal_theme);
        let open = self.settings_menu == Some(SettingsMenu::TerminalTheme);
        let mut control = div()
            .relative()
            .min_w(px(158.0))
            .child(settings_select_button(
                selected.name,
                "terminal-theme-dropdown",
                open,
                SettingsMenu::TerminalTheme,
                cx,
            ));
        if open {
            let mut options = div().p(px(4.0)).flex().flex_col();
            for (index, candidate) in TermTheme::CATALOG.into_iter().enumerate() {
                let is_selected = candidate.id == selected.id;
                options = options.child(
                    div()
                        .id(SharedString::from(format!("terminal-theme-option-{index}")))
                        .h(px(Metrics::ROW_HEIGHT))
                        .px(px(8.0))
                        .rounded(px(Radius::ROW))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .bg(Fill::selected(SETTINGS_COLORS, is_selected))
                        .cursor_pointer()
                        .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.08)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.prefs.terminal_theme = candidate.id.to_owned();
                            this.settings_menu = None;
                            this.persist_prefs();
                            cx.notify();
                        }))
                        .child(
                            div()
                                .size(px(12.0))
                                .rounded(px(4.0))
                                .bg(candidate.background)
                                .border_1()
                                .border_color(SETTINGS_COLORS.primary.alpha(0.14)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(Typo::ROW.size))
                                .child(candidate.name),
                        )
                        .when(is_selected, |row| {
                            row.child(sf_symbol("checkmark", 10.0, SETTINGS_COLORS.secondary))
                        }),
                );
            }
            control = control.child(settings_dropdown(options, 218.0));
        }
        control.into_any_element()
    }

    fn hibernate_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        const OPTIONS: [(u32, &str); 5] = [
            (0, "Off"),
            (5, "5 minutes"),
            (15, "15 minutes"),
            (30, "30 minutes"),
            (60, "1 hour"),
        ];
        let selected_label = OPTIONS
            .into_iter()
            .find_map(|(value, label)| {
                (value == self.prefs.hibernate_after_minutes).then_some(label)
            })
            .unwrap_or("Off");
        let open = self.settings_menu == Some(SettingsMenu::HibernateAfter);
        let mut control = div()
            .relative()
            .min_w(px(132.0))
            .child(settings_select_button(
                selected_label,
                "hibernate-dropdown",
                open,
                SettingsMenu::HibernateAfter,
                cx,
            ));
        if open {
            let mut options = div().p(px(4.0)).flex().flex_col();
            for (index, (value, label)) in OPTIONS.into_iter().enumerate() {
                let is_selected = value == self.prefs.hibernate_after_minutes;
                options = options.child(settings_choice_row(
                    format!("hibernate-option-{index}"),
                    label,
                    is_selected,
                    cx,
                    move |this, cx| {
                        this.prefs.hibernate_after_minutes = value;
                        this.settings_menu = None;
                        this.persist_prefs();
                        cx.notify();
                    },
                ));
            }
            control = control.child(settings_dropdown(options, 172.0));
        }
        control.into_any_element()
    }

    fn memory_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        const OPTIONS: [u64; 4] = [2, 4, 6, 8];
        let open = self.settings_menu == Some(SettingsMenu::MemoryLimit);
        let mut control = div()
            .relative()
            .min_w(px(104.0))
            .child(settings_select_button(
                format!("{} GB", self.prefs.memory_hard_limit_gb),
                "memory-dropdown",
                open,
                SettingsMenu::MemoryLimit,
                cx,
            ));
        if open {
            let mut options = div().p(px(4.0)).flex().flex_col();
            for (index, value) in OPTIONS.into_iter().enumerate() {
                let is_selected = value == self.prefs.memory_hard_limit_gb;
                options = options.child(settings_choice_row(
                    format!("memory-option-{index}"),
                    format!("{value} GB"),
                    is_selected,
                    cx,
                    move |this, cx| {
                        this.prefs.memory_hard_limit_gb = value;
                        this.settings_menu = None;
                        this.persist_prefs();
                        cx.notify();
                    },
                ));
            }
            control = control.child(settings_dropdown(options, 132.0));
        }
        control.into_any_element()
    }
}

impl Focusable for UtilitySurfaces {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for UtilitySurfaces {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let overlay = match self.surface {
            Surface::None => None,
            Surface::History => Some(self.render_history(cx).into_any_element()),
            Surface::Worktrees => Some(self.render_worktrees(cx).into_any_element()),
            Surface::Settings => Some(self.render_settings(cx).into_any_element()),
        };
        let root = div()
            .id("utility-surfaces")
            .track_focus(&self.focus)
            .key_context("Diri")
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &ToggleHistory, _, cx| {
                if this.surface == Surface::History {
                    this.close_surface(cx);
                } else {
                    this.open_history(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenWorktrees, _, cx| {
                this.open_worktrees(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.open_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSurface, _, cx| this.close_surface(cx)))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_history(-1, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_history(1, cx)))
            .on_action(cx.listener(|this, _: &Activate, _, cx| this.activate_history(cx)))
            .absolute()
            .text_color(COLORS.primary);
        if let Some(overlay) = overlay {
            root.inset_0().child(
                div()
                    .absolute()
                    .inset_0()
                    // The modal backdrop dismisses the topmost surface while
                    // still protecting terminal selection and scrollback.
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.close_surface(cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .bg(rgba(0x00000040))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(overlay),
                    ),
            )
        } else {
            root.size(px(0.0))
        }
    }
}

fn surface_button(
    label: impl Into<SharedString>,
    id: impl Into<SharedString>,
    cx: &mut Context<UtilitySurfaces>,
    handler: impl Fn(&mut UtilitySurfaces, &mut Context<UtilitySurfaces>) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .h(px(26.0))
        .px(px(9.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(COLORS.primary.alpha(0.10))
        .bg(COLORS.primary.alpha(0.04))
        .flex()
        .items_center()
        .text_size(px(11.0))
        .cursor_pointer()
        .hover(|style| style.bg(COLORS.primary.alpha(0.09)))
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .child(label.into())
}

fn settings_primary_button(
    label: &'static str,
    id: &'static str,
    icon: Option<&'static str>,
    cx: &mut Context<UtilitySurfaces>,
    handler: impl Fn(&mut UtilitySurfaces, &mut Window, &mut Context<UtilitySurfaces>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(10.0))
        .rounded(px(Radius::BADGE))
        .bg(Palette::CLAY.alpha(0.90))
        .text_color(rgba(0x190f0bff))
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|style| style.bg(Palette::CLAY))
        .on_click(cx.listener(move |this, _, window, cx| handler(this, window, cx)))
        .when_some(icon, |button, icon| {
            button.child(sf_symbol(icon, 10.0, rgba(0x190f0bff)))
        })
        .child(label)
}

fn settings_danger_button(
    label: &'static str,
    id: &'static str,
    cx: &mut Context<UtilitySurfaces>,
    handler: impl Fn(&mut UtilitySurfaces, &mut Context<UtilitySurfaces>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(26.0))
        .px(px(9.0))
        .rounded(px(Radius::BADGE))
        .bg(Ink::DANGER.alpha(0.11))
        .text_color(Ink::DANGER)
        .flex()
        .items_center()
        .text_size(px(11.0))
        .cursor_pointer()
        .hover(|style| style.bg(Ink::DANGER.alpha(0.18)))
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .child(label)
}

fn text_editor(value: &str) -> QueryEditor {
    let mut editor = QueryEditor::default();
    editor.insert(value);
    editor
}

fn danger_button(
    label: &'static str,
    cx: &mut Context<UtilitySurfaces>,
    handler: impl Fn(&mut UtilitySurfaces, &mut Context<UtilitySurfaces>) + 'static,
) -> impl IntoElement {
    div()
        .id("confirm-cleanup")
        .h(px(26.0))
        .px(px(9.0))
        .rounded(px(Radius::BADGE))
        .bg(Ink::DANGER.alpha(0.16))
        .text_color(Ink::DANGER)
        .flex()
        .items_center()
        .text_size(px(11.0))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .child(label)
}

fn toggle_row(
    label: &'static str,
    detail: &'static str,
    enabled: bool,
    id: &'static str,
    cx: &mut Context<UtilitySurfaces>,
    handler: impl Fn(&mut UtilitySurfaces, &mut Context<UtilitySurfaces>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .min_h(px(SETTINGS_ROW_HEIGHT))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .cursor_pointer()
        .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.025)))
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(SETTINGS_COLORS.primary)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .line_height(px(14.0))
                        .text_color(SETTINGS_COLORS.tertiary)
                        .child(detail),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(30.0))
                .h(px(18.0))
                .p(px(2.0))
                .rounded(px(9.0))
                .bg(if enabled {
                    Ink::FRESH.alpha(0.72)
                } else {
                    SETTINGS_COLORS.primary.alpha(0.14)
                })
                .flex()
                .justify_end()
                .when(!enabled, |toggle| toggle.justify_start())
                .child(
                    div()
                        .size(px(14.0))
                        .rounded(px(7.0))
                        .bg(SETTINGS_COLORS.primary),
                ),
        )
}

fn setting_section(title: &'static str, content: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .px(px(1.0))
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .text_color(SETTINGS_COLORS.tertiary)
                .child(title),
        )
        .child(
            div()
                .rounded(px(Radius::ROW))
                .border_1()
                .border_color(SETTINGS_COLORS.primary.alpha(0.065))
                .bg(SETTINGS_COLORS.primary.alpha(0.02))
                .overflow_hidden()
                .child(content),
        )
}

fn setting_row(
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
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
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(SETTINGS_COLORS.primary)
                        .child(label.into()),
                )
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .line_height(px(14.0))
                        .text_color(SETTINGS_COLORS.tertiary)
                        .child(detail.into()),
                ),
        )
        .child(control)
}

fn settings_page(title: &'static str, content: impl IntoElement) -> impl IntoElement {
    div()
        .w_full()
        .px(px(20.0))
        .pt(px(13.0))
        .pb(px(22.0))
        .flex()
        .flex_col()
        .gap(px(16.0))
        .child(
            div()
                .pr(px(34.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .text_size(px(Typo::DISPLAY_TITLE.size))
                .font_weight(Typo::DISPLAY_TITLE.weight)
                .text_color(SETTINGS_COLORS.primary)
                .child(title),
        )
        .child(content)
}

fn setting_divider() -> impl IntoElement {
    div()
        .h(px(1.0))
        .mx(px(12.0))
        .bg(SETTINGS_COLORS.primary.alpha(0.055))
}

fn settings_select_button(
    label: impl Into<SharedString>,
    id: impl Into<SharedString>,
    open: bool,
    menu: SettingsMenu,
    cx: &mut Context<UtilitySurfaces>,
) -> impl IntoElement {
    div()
        .id(id.into())
        .h(px(28.0))
        .px(px(9.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(
            SETTINGS_COLORS
                .primary
                .alpha(if open { 0.20 } else { 0.10 }),
        )
        .bg(SETTINGS_COLORS
            .primary
            .alpha(if open { 0.08 } else { 0.04 }))
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.075)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.settings_menu = if this.settings_menu == Some(menu) {
                None
            } else {
                Some(menu)
            };
            cx.notify();
        }))
        .child(
            div()
                .flex_1()
                .text_size(px(Typo::META.size))
                .font_weight(Typo::META.weight)
                .text_color(SETTINGS_COLORS.primary)
                .child(label.into()),
        )
        .child(sf_symbol_weighted(
            if open { "chevron.up" } else { "chevron.down" },
            8.0,
            SymbolWeight::Semibold,
            SETTINGS_COLORS.tertiary,
        ))
}

fn settings_dropdown(content: impl IntoElement, width: f32) -> impl IntoElement {
    deferred(
        div()
            .absolute()
            .top(px(32.0))
            .right_0()
            .w(px(width))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(FloatingSurface::new(SETTINGS_COLORS, content)),
    )
    .with_priority(5)
}

fn settings_choice_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &mut Context<UtilitySurfaces>,
    handler: impl Fn(&mut UtilitySurfaces, &mut Context<UtilitySurfaces>) + 'static,
) -> impl IntoElement {
    div()
        .id(id.into())
        .h(px(Metrics::ROW_HEIGHT))
        .px(px(8.0))
        .rounded(px(Radius::ROW))
        .flex()
        .items_center()
        .gap(px(8.0))
        .bg(Fill::selected(SETTINGS_COLORS, selected))
        .cursor_pointer()
        .hover(|style| style.bg(SETTINGS_COLORS.primary.alpha(0.08)))
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .child(
            div()
                .flex_1()
                .text_size(px(Typo::ROW.size))
                .child(label.into()),
        )
        .when(selected, |row| {
            row.child(sf_symbol("checkmark", 10.0, SETTINGS_COLORS.secondary))
        })
}

fn theme_preview(theme: TermTheme) -> impl IntoElement {
    div()
        .p(px(10.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(SETTINGS_COLORS.primary.alpha(0.10))
        .bg(theme.background)
        .flex()
        .flex_col()
        .gap(px(6.0))
        .font_family(crate::fonts::mono_family())
        .text_size(px(11.0))
        .child(
            div()
                .flex()
                .child(div().text_color(theme.ansi[2]).child("❯ "))
                .child(div().text_color(theme.foreground).child("cargo test"))
                .child(div().text_color(theme.cursor).child("█")),
        )
        .child(div().text_color(theme.ansi[8]).child("test result: ok"))
        .child(
            div().flex().gap(px(3.0)).children(
                theme
                    .ansi
                    .into_iter()
                    .map(|color| div().flex_1().h(px(10.0)).rounded(px(2.0)).bg(color)),
            ),
        )
}

fn chip(label: String) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(2.0))
        .rounded(px(Radius::CHIP))
        .bg(COLORS.primary.alpha(0.09))
        .text_size(px(11.0))
        .text_color(COLORS.secondary)
        .child(label)
}

fn colored_badge(label: &'static str, color: Rgba) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(2.0))
        .rounded(px(Radius::CHIP))
        .bg(color.alpha(0.12))
        .text_size(px(11.0))
        .text_color(color)
        .child(label)
}

fn empty_label(label: &str) -> impl IntoElement {
    div()
        .h(px(42.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .text_size(px(13.0))
        .text_color(COLORS.tertiary)
        .child(label.to_owned())
}

fn ui_agent(kind: &ProtoAgentKind) -> diri_ui::AgentKind {
    // Brand vocabulary, not a protocol type: a manifest agent the client has
    // no hand-drawn mark for falls back to the generic terminal treatment.
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => diri_ui::AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => diri_ui::AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => diri_ui::AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => diri_ui::AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => diri_ui::AgentKind::Shell,
        _ => diri_ui::AgentKind::Generic,
    }
}

fn ui_default_agent(agent: DefaultAgent) -> diri_ui::AgentKind {
    match agent {
        DefaultAgent::ClaudeCode => diri_ui::AgentKind::ClaudeCode,
        DefaultAgent::Codex => diri_ui::AgentKind::Codex,
        DefaultAgent::Cursor => diri_ui::AgentKind::Cursor,
        DefaultAgent::Gemini => diri_ui::AgentKind::Gemini,
    }
}

fn folder_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn relative_parent(path: &str) -> String {
    let Some(parent) = Path::new(path).parent() else {
        return String::new();
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.as_deref() == Some(parent) {
        return "~".to_owned();
    }
    if let Some(home) = home
        && let Ok(relative) = parent.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    parent.to_string_lossy().into_owned()
}

/// Second line under the update summary: why updates are off, or when the last
/// check ran.
fn update_detail(state: &crate::updates::UpdateState, unsupported: bool) -> String {
    if unsupported {
        // Verbatim reason ("not running from an app bundle", "not signed with
        // a Developer ID") so a dev build explains itself instead of looking
        // broken.
        return match &state.phase {
            UpdatePhase::Unsupported(reason) => format!("Updates off — {reason}"),
            _ => "Updates off for this build".to_owned(),
        };
    }
    let Some(checked) = state.last_checked_unix else {
        return "Not checked yet".to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(checked);
    let seconds = now.saturating_sub(checked);
    let ago = match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    };
    format!("Last checked {ago}")
}

fn relative_time(milliseconds: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
    let seconds = ((now - milliseconds).max(0.0) / 1000.0) as u64;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{
        Entity, Modifiers, MouseDownEvent, ScrollDelta, ScrollWheelEvent, TestAppContext, point,
    };

    struct SettingsModalHarness {
        surfaces: Entity<UtilitySurfaces>,
        background_events: Arc<AtomicUsize>,
    }

    impl Render for SettingsModalHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mouse_events = Arc::clone(&self.background_events);
            let scroll_events = Arc::clone(&self.background_events);
            div()
                .size_full()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .on_mouse_down(MouseButton::Left, move |_, _, _| {
                            mouse_events.fetch_add(1, Ordering::Relaxed);
                        })
                        .on_scroll_wheel(move |_, _, _| {
                            scroll_events.fetch_add(1, Ordering::Relaxed);
                        }),
                )
                .child(self.surfaces.clone())
        }
    }

    #[gpui::test]
    fn settings_modal_blocks_background_selection_and_scroll(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let updates = crate::updates::inert();
        let background_events = Arc::new(AtomicUsize::new(0));
        let event_probe = Arc::clone(&background_events);
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let surfaces = cx.new(|cx| {
                let mut surfaces = UtilitySurfaces::new(runtime, tokio, updates, window, cx);
                surfaces.open_settings(cx);
                surfaces
            });
            SettingsModalHarness {
                surfaces,
                background_events: event_probe,
            }
        });

        let surfaces = view.read_with(cx, |harness, _| harness.surfaces.clone());
        let outside_panel = point(px(8.0), px(320.0));
        cx.simulate_event(MouseDownEvent {
            position: outside_panel,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });

        assert_eq!(background_events.load(Ordering::Relaxed), 0);
        assert_eq!(
            surfaces.read_with(cx, |surfaces, _| surfaces.surface),
            Surface::None
        );

        surfaces.update(cx, |surfaces, cx| surfaces.open_settings(cx));
        cx.simulate_event(ScrollWheelEvent {
            position: outside_panel,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
            ..ScrollWheelEvent::default()
        });

        assert_eq!(background_events.load(Ordering::Relaxed), 0);
        assert_eq!(
            surfaces.read_with(cx, |surfaces, _| surfaces.surface),
            Surface::Settings
        );
    }

    #[gpui::test]
    fn clicking_inside_settings_but_outside_a_dropdown_closes_only_the_dropdown(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let updates = crate::updates::inert();
        let background_events = Arc::new(AtomicUsize::new(0));
        let event_probe = Arc::clone(&background_events);
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let surfaces = cx.new(|cx| {
                let mut surfaces = UtilitySurfaces::new(runtime, tokio, updates, window, cx);
                surfaces.open_settings(cx);
                surfaces.settings_menu = Some(SettingsMenu::DefaultAgent);
                surfaces
            });
            SettingsModalHarness {
                surfaces,
                background_events: event_probe,
            }
        });

        let dialog = cx
            .debug_bounds("settings-dialog")
            .expect("settings dialog should render");
        cx.simulate_click(
            point(dialog.center().x, dialog.top() + px(29.0)),
            Modifiers::default(),
        );

        let surfaces = view.read_with(cx, |harness, _| harness.surfaces.clone());
        assert_eq!(
            surfaces.read_with(cx, |surfaces, _| surfaces.settings_menu),
            None
        );
        assert_eq!(
            surfaces.read_with(cx, |surfaces, _| surfaces.surface),
            Surface::Settings
        );
        assert_eq!(background_events.load(Ordering::Relaxed), 0);
    }

    #[gpui::test]
    fn settings_content_close_control_dismisses_the_surface(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let updates = crate::updates::inert();
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let surfaces = cx.new(|cx| {
                let mut surfaces = UtilitySurfaces::new(runtime, tokio, updates, window, cx);
                surfaces.open_settings(cx);
                surfaces
            });
            SettingsModalHarness {
                surfaces,
                background_events: Arc::new(AtomicUsize::new(0)),
            }
        });

        let surfaces = view.read_with(cx, |harness, _| harness.surfaces.clone());
        let close = cx
            .debug_bounds("close-settings")
            .expect("settings close control should render");
        cx.simulate_click(close.center(), Modifiers::default());

        assert_eq!(
            surfaces.read_with(cx, |surfaces, _| surfaces.surface),
            Surface::None
        );
    }
}
