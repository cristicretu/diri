use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::commands::{NAVIGATION_CONTEXT, ToggleCommandPalette, ToggleQuickOpen};
use crate::fuzzy::{FuzzyMatcher, FuzzyQuery};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::palette::{self, PaletteAction, PaletteCommand, Ranked};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::quick_open::{
    self, DirectoryIndex, QuickOpenItem, QuickOpenSnapshot, RANK_DEBOUNCE, RESULT_LIMIT,
    RankedFolder,
};
use crate::store::{SessionStore, SpawnOptions, StoreRuntime};
use diri_proto::{AgentKind, AttentionLevel, SessionId, SessionRecord};
use diri_ui::{FloatingSurface, HairlineDivider, Palette, Radius, SemanticColors};
use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, FontWeight, HighlightStyle, KeyDownEvent,
    MouseButton, Pixels, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    StyledText, Task, Window, div, prelude::*, px, rgba,
};

/// The search field above the results, and the gap the surface keeps from the
/// window edges. Everything else is measured against the live viewport so the
/// list grows into a tall window and never overflows a short one.
const SEARCH_HEIGHT: f32 = 40.0;
const ROW_HEIGHT: f32 = 32.0;
const QUICK_ROW_HEIGHT: f32 = 34.0;
/// Quick Open rows that show a parent path stack two lines.
const QUICK_ROW_HEIGHT_WITH_PATH: f32 = 44.0;
const SECTION_HEADER_HEIGHT: f32 = 26.0;
const LIST_PADDING_X: f32 = 4.0;
const LIST_PADDING_Y: f32 = 2.0;
const ROW_PADDING_X: f32 = 10.0;
const COMMAND_SURFACE_WIDTH: f32 = 520.0;
const QUICK_OPEN_SURFACE_WIDTH: f32 = 580.0;
const MIN_LIST_HEIGHT: f32 = 96.0;
const COMMAND_MAX_LIST_HEIGHT: f32 = 440.0;
const QUICK_OPEN_MAX_LIST_HEIGHT: f32 = 640.0;
const CHAT_PREVIEW_LIMIT: usize = 7;
const MIN_TOP_INSET: f32 = 12.0;
const MAX_TOP_INSET: f32 = 96.0;
const BOTTOM_INSET: f32 = 24.0;

/// Where the overlay sits and how tall its list may grow in this window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OverlayLayout {
    top_inset: Pixels,
    width: Pixels,
    list_height: Pixels,
}

impl OverlayLayout {
    fn command_palette(viewport: gpui::Size<Pixels>) -> Self {
        let height = viewport.height.as_f32();
        let list = (height - 2.0 * MIN_TOP_INSET - SEARCH_HEIGHT - 1.0)
            .clamp(MIN_LIST_HEIGHT, COMMAND_MAX_LIST_HEIGHT);
        let surface_height = SEARCH_HEIGHT + 1.0 + list;
        Self {
            top_inset: px(((height - surface_height) / 2.0).max(MIN_TOP_INSET)),
            width: px(
                (viewport.width.as_f32() - 2.0 * BOTTOM_INSET).clamp(280.0, COMMAND_SURFACE_WIDTH)
            ),
            list_height: px(list),
        }
    }

    fn quick_open(viewport: gpui::Size<Pixels>) -> Self {
        let height = viewport.height.as_f32();
        let chrome = SEARCH_HEIGHT + 1.0 + BOTTOM_INSET;
        // Float the surface a twelfth of the way down, but give the inset back
        // to the list before the list is allowed to fall below its minimum.
        let top = (height / 12.0)
            .clamp(MIN_TOP_INSET, MAX_TOP_INSET)
            .min((height - chrome - MIN_LIST_HEIGHT).max(MIN_TOP_INSET));
        let list = (height - top - chrome).clamp(MIN_LIST_HEIGHT, QUICK_OPEN_MAX_LIST_HEIGHT);
        Self {
            top_inset: px(top),
            width: px((viewport.width.as_f32() - 2.0 * BOTTOM_INSET)
                .clamp(280.0, QUICK_OPEN_SURFACE_WIDTH)),
            list_height: px(list),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    CommandPalette,
    QuickOpen,
}

#[derive(Clone)]
enum CommandSelection {
    Action(PaletteCommand),
    Session(SessionId),
}

pub struct NavigationOverlay {
    focus_handle: FocusHandle,
    store: Arc<RwLock<SessionStore>>,
    _runtime: Arc<StoreRuntime>,
    overlay: Option<Overlay>,
    query: QueryEditor,
    highlight: usize,
    /// Ranked once per keystroke, then read by hit-testing, keyboard
    /// navigation, and rendering alike — they must agree on what row 3 is.
    ranked_actions: Vec<Ranked<PaletteAction>>,
    ranked_sessions: Vec<Ranked<SessionRecord>>,
    matcher: FuzzyMatcher,
    directory_index: DirectoryIndex,
    quick_snapshot: QuickOpenSnapshot,
    ranked_items: Vec<RankedFolder>,
    /// Identity of the readiness facts `ranked_actions` was built from, so a
    /// store change that cannot have altered the Agent rows does not rebuild
    /// them. See `agent_actions_fingerprint`.
    agent_actions_fingerprint: u64,
    scroll_handle: ScrollHandle,
    /// Separate slots: the disk-cache load and the filesystem scan both start
    /// at launch, and neither may cancel the other by sharing a `Task` slot.
    cache_task: Option<Task<()>>,
    scan_task: Option<Task<()>>,
    rank_task: Option<Task<()>>,
    /// This view is `.cached()` in RootView, so ambient window redraws no
    /// longer reach it: store changes must rebuild an open command palette and
    /// notify it directly, or its catalog actions and session rows go stale.
    _store_changes: Option<Task<()>>,
}

impl NavigationOverlay {
    pub fn new(runtime: Arc<StoreRuntime>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let _ = window;
        let mut changes = runtime.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| this.handle_store_change(cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let mut overlay = Self {
            focus_handle,
            store: Arc::clone(&runtime.store),
            _runtime: runtime,
            overlay: None,
            query: QueryEditor::default(),
            highlight: 0,
            ranked_actions: Vec::new(),
            ranked_sessions: Vec::new(),
            matcher: FuzzyMatcher::text(),
            directory_index: DirectoryIndex::default(),
            quick_snapshot: QuickOpenSnapshot::default(),
            ranked_items: Vec::new(),
            agent_actions_fingerprint: 0,
            scroll_handle: ScrollHandle::new(),
            cache_task: None,
            scan_task: None,
            rank_task: None,
            _store_changes: Some(store_changes),
        };
        // Warm at launch, the way Zed's worktree scan does: the cache makes the
        // index usable immediately and the scan refreshes it behind that, so the
        // first ⌘P of a session never waits on `read_dir`.
        overlay.load_cached_index(cx);
        overlay.refresh_directory_index(cx);
        overlay
    }

    #[cfg(test)]
    fn opened_for_test(runtime: Arc<StoreRuntime>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            store: Arc::clone(&runtime.store),
            _runtime: runtime,
            overlay: Some(Overlay::CommandPalette),
            query: QueryEditor::default(),
            highlight: 0,
            ranked_actions: Vec::new(),
            ranked_sessions: Vec::new(),
            matcher: FuzzyMatcher::text(),
            directory_index: DirectoryIndex::default(),
            quick_snapshot: QuickOpenSnapshot::default(),
            ranked_items: Vec::new(),
            agent_actions_fingerprint: 0,
            scroll_handle: ScrollHandle::new(),
            cache_task: None,
            scan_task: None,
            rank_task: None,
            _store_changes: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.overlay.is_some()
    }

    /// Store changes broadcast on the UI publish tick, so an open palette gets
    /// one of these several times a second while any session is producing
    /// output. Rebuilding on each would take a write lock, clone every project
    /// and session record, and re-rank the whole list — reordering rows under a
    /// highlight index that is not re-anchored. Only readiness can change the
    /// Agent rows this handler exists for, so gate on exactly that.
    fn handle_store_change(&mut self, cx: &mut Context<Self>) {
        if self.overlay == Some(Overlay::CommandPalette) {
            let fingerprint = {
                let store = self.store.read().expect("session store lock poisoned");
                agent_actions_fingerprint(&store)
            };
            if fingerprint != self.agent_actions_fingerprint {
                let highlighted = self.highlighted_command();
                self.refresh_command_items();
                self.restore_highlight(highlighted.as_ref());
            }
        }
        cx.notify();
    }

    /// The row the user is on, so a rebuild can put the highlight back on it
    /// rather than on whatever inherits its index.
    fn highlighted_command(&self) -> Option<CommandSelection> {
        self.ranked_sessions.get(self.highlight).map_or_else(
            || {
                self.ranked_actions
                    .get(self.highlight.saturating_sub(self.ranked_sessions.len()))
                    .map(|ranked| CommandSelection::Action(ranked.item.command.clone()))
            },
            |session| Some(CommandSelection::Session(session.item.id.clone())),
        )
    }

    fn restore_highlight(&mut self, previous: Option<&CommandSelection>) {
        let found = match previous {
            Some(CommandSelection::Action(command)) => self
                .ranked_actions
                .iter()
                .position(|ranked| ranked.item.command == *command)
                .map(|index| index + self.ranked_sessions.len()),
            Some(CommandSelection::Session(id)) => self
                .ranked_sessions
                .iter()
                .position(|ranked| ranked.item.id == *id),
            None => None,
        };
        let count = self.ranked_actions.len() + self.ranked_sessions.len();
        self.highlight = found.unwrap_or(self.highlight).min(count.saturating_sub(1));
    }

    pub(crate) fn toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay == Some(Overlay::CommandPalette) {
            self.close_overlay(cx);
        } else {
            self.open_overlay(Overlay::CommandPalette, window, cx);
        }
    }

    pub(crate) fn toggle_quick_open(
        &mut self,
        _: &ToggleQuickOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay == Some(Overlay::QuickOpen) {
            self.close_overlay(cx);
        } else {
            self.open_overlay(Overlay::QuickOpen, window, cx);
            self.refresh_directory_index(cx);
        }
    }

    fn open_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = Some(overlay);
        self.query.clear();
        self.reset_selection();
        self.ranked_items.clear();
        if overlay == Overlay::CommandPalette {
            self.refresh_command_items();
        }
        let _ = window;
        cx.notify();
    }

    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        self.overlay = None;
        self.query.clear();
        self.highlight = 0;
        self.ranked_actions.clear();
        self.ranked_sessions.clear();
        self.rank_task = None;
        cx.notify();
    }

    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            self.close_overlay(cx);
        }
    }

    /// Back to the first row, scrolled back to the top of the list.
    fn reset_selection(&mut self) {
        self.highlight = 0;
        self.scroll_handle.set_offset(gpui::point(px(0.0), px(0.0)));
    }

    /// The roots to index, and where their cached index lives.
    fn index_roots(&mut self) -> (Vec<PathBuf>, Vec<PathBuf>, PathBuf) {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let projects = self.project_roots();
        let mut fallback = vec![PathBuf::from("~/fun")];
        fallback.extend(
            projects
                .iter()
                .filter_map(|(root, _)| root.parent().map(Path::to_path_buf)),
        );
        let quick_open_roots = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .quick_open_roots
            .clone();
        let roots = quick_open::resolve_roots(&quick_open_roots, &fallback, &home);
        let cache = quick_open::cache_file(&home);
        (roots, vec![home], cache)
    }

    /// Populate the index from the previous run's scan. Costs one file read, so
    /// the first ⌘P of a launch has results to show instead of "Scanning…".
    fn load_cached_index(&mut self, cx: &mut Context<Self>) {
        let (roots, _, cache) = self.index_roots();
        let (projects, cwds) = self.snapshot_inputs();
        self.cache_task = Some(cx.spawn(async move |this, cx| {
            let built = cx
                .background_spawn(async move {
                    let entries = quick_open::load_cache(&cache, &roots)?;
                    let snapshot = quick_open::build_snapshot(&entries, &projects, &cwds);
                    Some((entries, snapshot))
                })
                .await;
            let Some((entries, snapshot)) = built else {
                return;
            };
            this.update(cx, |this, cx| {
                this.directory_index.adopt_cached(entries);
                this.quick_snapshot = snapshot;
                cx.notify();
            })
            .ok();
        }));
    }

    fn refresh_directory_index(&mut self, cx: &mut Context<Self>) {
        if !self.directory_index.needs_scan(Instant::now()) || !self.directory_index.begin_scan() {
            return;
        }
        let (roots, standalone, cache) = self.index_roots();
        let (projects, cwds) = self.snapshot_inputs();

        self.scan_task = Some(cx.spawn(async move |this, cx| {
            // Scan, persist, and prepare 20 000 ranking candidates all on the
            // background executor: preparing them on the main thread cost ~13 ms,
            // which is a dropped frame on any display and most of two at 120 Hz.
            let (entries, snapshot) = cx
                .background_spawn(async move {
                    let entries = quick_open::scan(&roots, &standalone);
                    quick_open::store_cache(&cache, &roots, &entries);
                    let snapshot = quick_open::build_snapshot(&entries, &projects, &cwds);
                    (entries, snapshot)
                })
                .await;
            this.update(cx, |this, cx| {
                this.directory_index.finish_scan(entries, Instant::now());
                this.quick_snapshot = snapshot;
                if !this.query.text().trim().is_empty() {
                    this.schedule_rank(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The Recent section's contents: configured projects first, then session
    /// working directories in most-recently-updated order.
    fn snapshot_inputs(&mut self) -> (Vec<(PathBuf, String)>, Vec<PathBuf>) {
        let projects = self.project_roots();
        let store = self.store.read().expect("session store lock poisoned");
        let mut sessions: Vec<_> = store.sessions().values().collect();
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .partial_cmp(&left.updated_at)
                .unwrap_or(Ordering::Equal)
        });
        let cwds = sessions
            .into_iter()
            .map(|session| PathBuf::from(&session.cwd))
            .collect();
        (projects, cwds)
    }

    fn project_roots(&mut self) -> Vec<(PathBuf, String)> {
        self.store
            .write()
            .expect("session store lock poisoned")
            .sidebar_projection()
            .projects
            .iter()
            .map(|entry| {
                (
                    PathBuf::from(&entry.project.root),
                    entry.project.name.clone(),
                )
            })
            .collect()
    }

    fn schedule_rank(&mut self, cx: &mut Context<Self>) {
        self.rank_task = None;
        let query = self.query.text().trim().to_owned();
        if query.is_empty() {
            self.ranked_items.clear();
            cx.notify();
            return;
        }
        let pool = self.quick_snapshot.pool.clone();
        self.rank_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RANK_DEBOUNCE).await;
            let ranked = cx
                .background_spawn(async move { quick_open::rank(&query, &pool, RESULT_LIMIT) })
                .await;
            this.update(cx, |this, cx| {
                this.ranked_items = ranked;
                this.reset_selection();
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay.is_none() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "escape" => self.close_overlay(cx),
            "up" => self.move_highlight(-1, cx),
            "down" => self.move_highlight(1, cx),
            "p" if modifiers.control => self.move_highlight(-1, cx),
            "n" if modifiers.control => self.move_highlight(1, cx),
            "enter" => self.run_highlighted(modifiers.platform, window, cx),
            _ => self.edit_query(event, cx),
        }
        cx.stop_propagation();
    }

    /// Everything the search field itself handles, through the key map shared
    /// with Quick Open and the terminal's find bar.
    fn edit_query(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(edit) = query_editor::edit_for(&event.keystroke) else {
            return;
        };
        let changed = match edit {
            Edit::Local(local) => self.query.apply(local),
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(&self.query, cx);
                false
            }
            Edit::Clipboard(ClipboardEdit::Cut) => query_editor::cut_selection(&mut self.query, cx),
            Edit::Clipboard(ClipboardEdit::Paste) => cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| self.query.insert(&text)),
        };

        if changed {
            self.query_changed(cx);
        } else {
            // The caret or selection moved even when the text did not.
            cx.notify();
        }
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.reset_selection();
        if self.overlay == Some(Overlay::QuickOpen) {
            self.schedule_rank(cx);
        } else {
            self.refresh_command_items();
            cx.notify();
        }
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        self.highlight = (self.highlight as isize + delta).rem_euclid(count as isize) as usize;
        self.scroll_to_highlight();
        cx.notify();
    }

    /// Keyboard navigation must drag the viewport along with it; the list is
    /// taller than the window on any real machine.
    fn scroll_to_highlight(&self) {
        self.scroll_handle.scroll_to_item(self.highlight_child());
    }

    /// Index of the highlighted row among the scroll container's children,
    /// which include the section headers.
    fn highlight_child(&self) -> usize {
        match self.overlay {
            Some(Overlay::CommandPalette) => command_row_child_index(
                self.highlight,
                self.ranked_sessions.len(),
                self.quick_action_count(),
                self.ranked_actions.len(),
                self.query.text().trim().is_empty(),
            ),
            Some(Overlay::QuickOpen) if self.query.text().trim().is_empty() => {
                row_child_index(self.highlight, Some(self.quick_snapshot.recent.len()))
            }
            // A searched Quick Open list is one flat section, no headers.
            _ => row_child_index(self.highlight, None),
        }
    }

    fn visible_count(&self) -> usize {
        match self.overlay {
            Some(Overlay::CommandPalette) => self.ranked_actions.len() + self.ranked_sessions.len(),
            Some(Overlay::QuickOpen) if self.query.text().trim().is_empty() => {
                self.quick_snapshot.recent.len() + self.quick_snapshot.folders.len()
            }
            Some(Overlay::QuickOpen) => self.ranked_items.len(),
            None => 0,
        }
    }

    fn quick_action_count(&self) -> usize {
        if self.query.text().trim().is_empty() {
            self.ranked_actions
                .iter()
                .take_while(|ranked| is_quick_action(&ranked.item))
                .count()
        } else {
            0
        }
    }

    fn run_highlighted(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay {
            Some(Overlay::CommandPalette) => {
                let selection = if let Some(session) = self.ranked_sessions.get(self.highlight) {
                    Some(CommandSelection::Session(session.item.id.clone()))
                } else {
                    self.ranked_actions
                        .get(self.highlight.saturating_sub(self.ranked_sessions.len()))
                        .and_then(|action| {
                            action
                                .item
                                .enabled
                                .then(|| CommandSelection::Action(action.item.command.clone()))
                        })
                };
                if let Some(selection) = selection {
                    self.run_command_selection(selection, window, cx);
                }
            }
            Some(Overlay::QuickOpen) => {
                if let Some(item) = self.current_quick_item() {
                    let cwd = item.path.to_string_lossy().into_owned();
                    if secondary {
                        self.store
                            .write()
                            .expect("session store lock poisoned")
                            .spawn_shell(SpawnOptions {
                                cwd: Some(cwd.clone()),
                                ..SpawnOptions::default()
                            });
                    } else {
                        self.store
                            .write()
                            .expect("session store lock poisoned")
                            .spawn_default(SpawnOptions {
                                cwd: Some(cwd.clone()),
                                ..SpawnOptions::default()
                            });
                    }
                    self.close_overlay(cx);
                }
            }
            None => {}
        }
    }

    fn run_command_selection(
        &mut self,
        selection: CommandSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            CommandSelection::Session(id) => {
                self.store
                    .write()
                    .expect("session store lock poisoned")
                    .select(id);
                self.close_overlay(cx);
            }
            CommandSelection::Action(command) => self.run_palette_command(command, window, cx),
        }
    }

    fn run_palette_command(
        &mut self,
        command: PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PaletteCommand::Action(id) => {
                self.close_overlay(cx);
                window.dispatch_action(id.action(), cx);
            }
            PaletteCommand::SpawnAgent { agent, cwd, host } => {
                {
                    let mut store = self.store.write().expect("session store lock poisoned");
                    let mut options = SpawnOptions {
                        cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
                        host: host.clone(),
                        ..SpawnOptions::default()
                    };
                    // Repo-preserving spawn: when no explicit directory was
                    // chosen and the spawn targets a remote host (or the
                    // active session lives on one), keep the active REPO —
                    // the daemon resolves its checkout on the target host.
                    let selected = store.selected_session();
                    let active_host = selected.and_then(|session| session.host.clone());
                    if options.cwd.is_none() && (host.is_some() || active_host.is_some()) {
                        options.same_repo_as = selected.map(|session| session.id.clone());
                        if host.is_none() && active_host.is_some() {
                            // Remote session spawning locally: its remote cwd
                            // is useless as a local path.
                            options.cwd = Some(store.local_fallback_directory());
                        }
                    }
                    store.spawn_kind(agent, options);
                }
                self.close_overlay(cx);
            }
            PaletteCommand::MigrateSelected { target_host } => {
                {
                    let mut store = self.store.write().expect("session store lock poisoned");
                    if let Some(id) = store.selected_session_id().cloned() {
                        store.migrate_session(id, target_host);
                    }
                }
                self.close_overlay(cx);
            }
            PaletteCommand::SyncPrefs { host } => {
                self.store
                    .write()
                    .expect("session store lock poisoned")
                    .sync_prefs(host);
                self.close_overlay(cx);
            }
        }
    }

    /// Rebuild the palette's ranked rows for the current query. Cheap enough
    /// to run on every keystroke — a few hundred candidates against one
    /// matcher — and never run per frame.
    fn refresh_command_items(&mut self) {
        let (actions, sessions, fingerprint) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let projects: Vec<_> = store
                .sidebar_projection()
                .projects
                .iter()
                .map(|entry| palette::ProjectTarget {
                    project: entry.project.clone(),
                    host: entry.host.clone(),
                })
                .collect();
            let hosts = store.hosts().to_vec();
            let selected = store.selected_session().cloned();
            let default_host = store.default_spawn_host();
            let actions = palette::actions_for_catalogs(
                store.preferences().default_agent.clone(),
                &projects,
                &hosts,
                selected.as_ref(),
                default_host.as_deref(),
                store.agent_catalogs(),
            );
            let fingerprint = agent_actions_fingerprint(&store);
            (actions, store.ordered_sessions(), fingerprint)
        };
        self.agent_actions_fingerprint = fingerprint;
        let query = FuzzyQuery::new(self.query.text());
        let searching = !self.query.text().trim().is_empty();
        let mut ranked_actions = palette::rank_actions(actions, &query, &mut self.matcher);
        if !searching {
            // Empty-query browsing is intentionally curated: two frequent
            // actions stay visible directly below chats, while fuzzy
            // search remains score-ordered across every command.
            let (mut quick, commands): (Vec<_>, Vec<_>) = ranked_actions
                .into_iter()
                .partition(|ranked| is_quick_action(&ranked.item));
            quick.extend(commands);
            ranked_actions = quick;
        }
        self.ranked_actions = ranked_actions;
        self.ranked_sessions = palette::rank_sessions(sessions, &query, &mut self.matcher);
        if !searching {
            self.ranked_sessions.truncate(CHAT_PREVIEW_LIMIT);
        }
    }

    fn current_quick_item(&self) -> Option<QuickOpenItem> {
        if self.query.text().trim().is_empty() {
            self.quick_snapshot
                .recent
                .iter()
                .chain(&self.quick_snapshot.folders)
                .nth(self.highlight)
                .cloned()
        } else {
            self.ranked_items
                .get(self.highlight)
                .map(|folder| folder.item.clone())
        }
    }

    fn render_overlay(&mut self, layout: OverlayLayout, cx: &mut Context<Self>) -> AnyElement {
        let colors = {
            let store = self.store.read().expect("session store lock poisoned");
            crate::app_theme::colors(&store.preferences().terminal_theme)
        };
        let content = match self.overlay {
            Some(Overlay::CommandPalette) => self.render_command_palette(layout, colors, cx),
            Some(Overlay::QuickOpen) => self.render_quick_open(layout, colors, cx),
            None => return div().into_any_element(),
        };
        let surface = if self.overlay == Some(Overlay::CommandPalette) {
            // Command palettes are keyboard-triggered, so their frame is
            // immediate; a settled surface is faster to parse than a
            // decorative entrance animation.
            FloatingSurface::new(colors, content)
                .radius(20.0)
                .animate_entry(false)
        } else {
            FloatingSurface::new(colors, content)
        };
        div()
            .absolute()
            .inset_0()
            // A modal owns the entire wheel gesture, including the backdrop.
            // Without this, trackpad deltas hit the terminal underneath and its
            // precise-scroll accumulator releases them after the modal closes.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .flex()
            .items_start()
            .justify_center()
            .pt(layout.top_inset)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .bg(rgba(0x00000040))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.close_overlay(cx);
                        }),
                    ),
            )
            .child(
                div()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.close_overlay(cx);
                    }))
                    .child(surface),
            )
            .into_any_element()
    }

    fn render_search(&self, placeholder: &'static str, colors: SemanticColors) -> AnyElement {
        let field = div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(SEARCH_HEIGHT))
            // Line the query up with the rows' icon column below it.
            .px(px(LIST_PADDING_X + ROW_PADDING_X))
            .text_size(px(14.0));

        if self.query.is_empty() {
            return field
                .text_color(colors.tertiary)
                .child(placeholder)
                .into_any_element();
        }

        field
            .text_color(colors.primary)
            .child(query_label(&self.query))
            .into_any_element()
    }

    fn render_command_palette(
        &mut self,
        layout: OverlayLayout,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let actions = self.ranked_actions.clone();
        let sessions = self.ranked_sessions.clone();
        let action_count = actions.len();
        let session_count = sessions.len();
        let quick_count = self.quick_action_count();
        let mut results = div()
            .id("command-palette-results")
            .track_scroll(&self.scroll_handle)
            .flex()
            .flex_col()
            .max_h(layout.list_height)
            .overflow_y_scroll()
            .px(px(LIST_PADDING_X))
            .py(px(LIST_PADDING_Y));

        if !sessions.is_empty() {
            results = results.child(section_header("Chats", colors));
            for (index, session) in sessions.into_iter().enumerate() {
                results = results.child(self.render_session_row(session, index, colors, cx));
            }
        }
        if quick_count > 0 {
            results = results.child(section_header("Quick actions", colors));
            for (offset, action) in actions.iter().take(quick_count).cloned().enumerate() {
                results = results.child(self.render_action_row(
                    action,
                    session_count + offset,
                    colors,
                    cx,
                ));
            }
        }
        if action_count > quick_count {
            results = results.child(section_header("Commands", colors));
            for (offset, action) in actions.iter().skip(quick_count).cloned().enumerate() {
                results = results.child(self.render_action_row(
                    action,
                    session_count + quick_count + offset,
                    colors,
                    cx,
                ));
            }
        }
        if action_count == 0 && session_count == 0 {
            results = results.child(empty_label("No matches", colors));
        }

        div()
            .id("command-palette")
            .debug_selector(|| "command-palette".into())
            .w(layout.width)
            .p(px(4.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_color(colors.primary)
            .child(self.render_search("Search chats or run a command…", colors))
            .child(results)
            .into_any_element()
    }

    fn render_action_row(
        &mut self,
        ranked: Ranked<PaletteAction>,
        index: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let action = ranked.item;
        let command = action.command.clone();
        let enabled = action.enabled;
        let trailing = action
            .detail
            .clone()
            .map(SharedString::from)
            .or_else(|| action.shortcut.map(SharedString::from))
            .into_iter()
            .collect();
        palette_row(
            highlighted_label(action.title, &ranked.title_matches),
            sf_symbol(action.system_image, 12.5, colors.secondary),
            trailing,
            index == self.highlight,
            index,
            enabled,
            colors,
        )
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered {
                this.highlight = index;
                cx.notify();
            }
        }))
        .when(enabled, |row| {
            row.on_click(cx.listener(move |this, _, window, cx| {
                this.run_command_selection(CommandSelection::Action(command.clone()), window, cx);
            }))
        })
        .into_any_element()
    }

    fn render_session_row(
        &mut self,
        ranked: Ranked<SessionRecord>,
        index: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = ranked.item;
        let id = session.id.clone();
        let dot_color = attention_color(session.attention(), colors);
        let mut trailing = vec![SharedString::from(kind_label(session.effective_kind()))];
        if let Some(shortcut) = session_shortcut(index) {
            trailing.push(shortcut.into());
        }
        palette_row(
            highlighted_label(session.title, &ranked.title_matches),
            div()
                .flex_none()
                .size(px(7.0))
                .rounded_full()
                .bg(dot_color)
                .into_any_element(),
            trailing,
            index == self.highlight,
            index,
            true,
            colors,
        )
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered {
                this.highlight = index;
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.run_command_selection(CommandSelection::Session(id.clone()), window, cx);
        }))
        .into_any_element()
    }

    fn render_quick_open(
        &mut self,
        layout: OverlayLayout,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let searching = !self.query.text().trim().is_empty();
        let mut results = div()
            .id("quick-open-results")
            .track_scroll(&self.scroll_handle)
            .flex()
            .flex_col()
            .max_h(layout.list_height)
            .overflow_y_scroll()
            .px(px(LIST_PADDING_X))
            .py(px(LIST_PADDING_Y));

        if searching {
            let ranked = self.ranked_items.clone();
            for (index, folder) in ranked.iter().enumerate() {
                results = results.child(self.render_quick_row(
                    folder.item.clone(),
                    &folder.name_matches,
                    index,
                    colors,
                    cx,
                ));
            }
            if ranked.is_empty() {
                results = results.child(empty_label(
                    if self.directory_index.is_scanning() {
                        "Scanning…"
                    } else {
                        "No matches"
                    },
                    colors,
                ));
            }
        } else {
            let recent = self.quick_snapshot.recent.clone();
            let folders = self.quick_snapshot.folders.clone();
            if !recent.is_empty() {
                results = results.child(section_header("Recent", colors));
                for (index, item) in recent.iter().cloned().enumerate() {
                    results = results.child(self.render_quick_row(item, &[], index, colors, cx));
                }
            }
            if !folders.is_empty() {
                results = results.child(section_header("Folders", colors));
                for (offset, item) in folders.iter().cloned().enumerate() {
                    results = results.child(self.render_quick_row(
                        item,
                        &[],
                        recent.len() + offset,
                        colors,
                        cx,
                    ));
                }
            }
            if recent.is_empty() && folders.is_empty() {
                results = results.child(empty_label(
                    if self.directory_index.is_scanning() {
                        "Scanning…"
                    } else {
                        "No folders indexed"
                    },
                    colors,
                ));
            }
        }

        div()
            .w(layout.width)
            .text_color(colors.primary)
            .child(self.render_search("Jump to a project or folder…", colors))
            .child(HairlineDivider::horizontal(colors))
            .child(results)
            .into_any_element()
    }

    fn render_quick_row(
        &mut self,
        item: QuickOpenItem,
        name_matches: &[Range<usize>],
        index: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = item.path.clone();
        let parent = relative_parent(&item.path);
        let icon_color = if item.is_git_repo {
            Palette::CLAY
        } else {
            colors.secondary
        };
        let default_name = {
            let store = self.store.read().expect("session store lock poisoned");
            store.agent_catalog(None).map_or_else(
                || crate::agent_catalog::title_case_id(store.preferences().default_agent.id()),
                |catalog| {
                    crate::agent_catalog::display_name(&store.preferences().default_agent, catalog)
                },
            )
        };
        let row = div()
            .id(format!("quick-row-{index}"))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .h(px(if parent.is_empty() {
                QUICK_ROW_HEIGHT
            } else {
                QUICK_ROW_HEIGHT_WITH_PATH
            }))
            .px(px(ROW_PADDING_X))
            .rounded(px(Radius::ROW))
            .bg(if index == self.highlight {
                colors.primary.alpha(0.10)
            } else {
                colors.primary.alpha(0.0)
            })
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .w(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(sf_symbol_weighted(
                                if item.is_git_repo {
                                    "folder.fill"
                                } else {
                                    "folder"
                                },
                                13.0,
                                SymbolWeight::Regular,
                                icon_color,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w(px(0.0))
                            .text_size(px(13.0))
                            .child(highlighted_label(item.name.clone(), name_matches))
                            .when(!parent.is_empty(), |column| {
                                column.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(colors.tertiary)
                                        .child(parent.clone()),
                                )
                            }),
                    ),
            )
            .when(index == self.highlight, |row| {
                row.child(
                    div()
                        .flex()
                        .gap(px(5.0))
                        .child(chip(format!("⏎ {}", default_name.to_lowercase()), colors))
                        .child(chip(
                            format!("{} term", crate::commands::primary_shortcut_label("⏎")),
                            colors,
                        )),
                )
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.highlight = index;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                let cwd = path.to_string_lossy().into_owned();
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .spawn_default(SpawnOptions {
                        cwd: Some(cwd.clone()),
                        ..SpawnOptions::default()
                    });
                this.close_overlay(cx);
            }));
        row.into_any_element()
    }
}

impl Focusable for NavigationOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NavigationOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = if self.overlay == Some(Overlay::CommandPalette) {
            OverlayLayout::command_palette(window.viewport_size())
        } else {
            OverlayLayout::quick_open(window.viewport_size())
        };
        let overlay = self.overlay.map(|_| self.render_overlay(layout, cx));
        let root = div()
            .id("navigation-overlay")
            .key_context(NAVIGATION_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_quick_open))
            .on_key_down(cx.listener(Self::on_key_down))
            .absolute()
            // Cached entity roots are laid out independently, so insets alone
            // leave this absolute root without a definite size and its height
            // collapses to its in-flow content, which is nothing.
            .size_full();
        if let Some(overlay) = overlay {
            root.inset_0().child(overlay)
        } else {
            root.size(px(0.0))
        }
    }
}

fn section_header(title: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .h(px(SECTION_HEADER_HEIGHT))
        .flex()
        .flex_none()
        .items_end()
        .px(px(ROW_PADDING_X))
        .pb(px(3.0))
        .text_size(px(11.0))
        .text_color(colors.tertiary)
        .child(title)
        .into_any_element()
}

fn empty_label(text: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .h(px(ROW_HEIGHT))
        .flex()
        .flex_none()
        .items_center()
        .px(px(ROW_PADDING_X))
        .text_size(px(13.0))
        .text_color(colors.tertiary)
        .child(text)
        .into_any_element()
}

fn is_quick_action(action: &PaletteAction) -> bool {
    let default_shortcut =
        crate::commands::command(crate::commands::CommandId::NewDefaultSession).shortcut_label();
    matches!(
        action.command,
        PaletteCommand::Action(crate::commands::CommandId::ToggleQuickOpen)
    ) || action.shortcut.as_deref() == default_shortcut.as_deref()
}

fn session_shortcut(index: usize) -> Option<String> {
    use crate::commands::CommandId;

    let command = match index {
        0 => CommandId::SelectSession1,
        1 => CommandId::SelectSession2,
        2 => CommandId::SelectSession3,
        3 => CommandId::SelectSession4,
        4 => CommandId::SelectSession5,
        5 => CommandId::SelectSession6,
        6 => CommandId::SelectSession7,
        7 => CommandId::SelectSession8,
        _ => return None,
    };
    crate::commands::command(command).shortcut_label()
}

/// The command palette has up to three visible sections: recent chats, two
/// curated quick actions, then the remaining commands. Convert a row index in
/// that keyboard order into the scroll container's child index.
const fn command_row_child_index(
    row: usize,
    session_count: usize,
    quick_count: usize,
    action_count: usize,
    show_quick_section: bool,
) -> usize {
    let mut child = row;
    if session_count > 0 {
        child += 1;
    }
    if row >= session_count {
        if show_quick_section && quick_count > 0 {
            child += 1;
        } else if !show_quick_section && action_count > 0 {
            child += 1;
        }
        if show_quick_section && row >= session_count + quick_count && action_count > quick_count {
            child += 1;
        }
    }
    child
}

/// Rows and section headers are siblings in the scroll container, so scrolling
/// to row N means scrolling to child N plus every header above it. `sections`
/// is the size of the first section, or `None` for a headerless flat list.
const fn row_child_index(row: usize, first_section: Option<usize>) -> usize {
    let Some(first) = first_section else {
        return row;
    };
    // Each non-empty section above the row contributes one header child.
    row + (first > 0) as usize + (row >= first) as usize
}

/// A static caret. Blinking would need an autonomous frame timer, which is
/// exactly what PERF.md's idle-CPU budget forbids; the terminal cursor is
/// static for the same reason.
pub(crate) const CARET: &str = "▏";

/// Draw a query field's contents: caret at the cursor, or the selection washed
/// in the brand accent. Shared by the palette, Quick Open, and the find bar so
/// all three fields look like the same control.
pub fn query_label(editor: &QueryEditor) -> AnyElement {
    let (text, selection) = editor.display(CARET);
    highlighted_label_styled(
        text,
        selection.as_slice(),
        HighlightStyle {
            background_color: Some(Palette::CLAY.alpha(0.35).into()),
            ..HighlightStyle::default()
        },
    )
}

/// Paint the characters the query actually matched in the brand accent, so a
/// glance at the list explains why each row is there and in that order.
fn highlighted_label(text: impl Into<SharedString>, matches: &[Range<usize>]) -> AnyElement {
    highlighted_label_styled(
        text,
        matches,
        HighlightStyle {
            color: Some(Palette::CLAY.into()),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..HighlightStyle::default()
        },
    )
}

fn highlighted_label_styled(
    text: impl Into<SharedString>,
    matches: &[Range<usize>],
    style: HighlightStyle,
) -> AnyElement {
    let text = text.into();
    if matches.is_empty() {
        return div().child(text).into_any_element();
    }
    StyledText::new(text)
        .with_highlights(matches.iter().map(|range| (range.clone(), style)))
        .into_any_element()
}

fn palette_row(
    title: AnyElement,
    leading: AnyElement,
    // Owned: agent chips and shortcut hints are not compile-time literals.
    trailing: Vec<SharedString>,
    highlighted: bool,
    index: usize,
    enabled: bool,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(format!("palette-row-{index}"))
        .flex()
        // Without this the rows are shrinkable flex children: a list taller
        // than its container squeezes every row toward min-content instead of
        // scrolling, and 40pt rows render as ~21pt of crammed text.
        .flex_none()
        .items_center()
        .justify_between()
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING_X))
        .rounded(px(Radius::ROW))
        .bg(if highlighted {
            colors.primary.alpha(0.10)
        } else {
            colors.primary.alpha(0.0)
        })
        .opacity(if enabled { 1.0 } else { 0.48 })
        .when(enabled, |row| row.cursor_pointer())
        .text_size(px(13.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(9.0))
                .min_w_0()
                .child(
                    div()
                        .w(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(leading),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(title),
                ),
        )
        .when(!trailing.is_empty(), |row| {
            row.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .children(trailing.into_iter().map(|trailing| chip(trailing, colors))),
            )
        })
}

fn chip(text: impl Into<gpui::SharedString>, colors: SemanticColors) -> AnyElement {
    div()
        .px(px(5.0))
        .py(px(2.0))
        .rounded(px(Radius::CHIP))
        .bg(colors.primary.alpha(0.06))
        .text_size(px(11.0))
        .text_color(colors.tertiary)
        .child(text.into())
        .into_any_element()
}

fn attention_color(attention: AttentionLevel, colors: SemanticColors) -> gpui::Rgba {
    match attention {
        AttentionLevel::NeedsInput => gpui::rgb(0xf59e0b),
        AttentionLevel::DoneUnseen => gpui::rgb(0x3b82f6),
        AttentionLevel::Working => colors.secondary,
        _ => colors.tertiary,
    }
}

/// Compact label for the navigator's kind column. The manifest id is already a
/// short lowercase word for every agent, so only the two non-agent kinds and
/// Claude's hyphenated id need shortening.
fn kind_label(kind: &AgentKind) -> String {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "claude".to_owned(),
        AgentKind::GENERIC_ID => "term".to_owned(),
        other => other.to_owned(),
    }
}

/// Identity of everything the palette's Agent rows are derived from: the saved
/// default, the target it spawns on, and each target's readiness facts. Session
/// and project churn is deliberately excluded — it moves on every UI tick and
/// cannot change which Agents a target can launch.
fn agent_actions_fingerprint(store: &SessionStore) -> u64 {
    let mut hasher = DefaultHasher::new();
    store.preferences().default_agent.id().hash(&mut hasher);
    store.default_spawn_host().hash(&mut hasher);
    let mut targets: Vec<_> = store.agent_catalogs().iter().collect();
    targets.sort_by_key(|(target, _)| *target);
    for (target, catalog) in targets {
        target.hash(&mut hasher);
        for agent in &catalog.agents {
            agent.kind.id().hash(&mut hasher);
            agent.available().hash(&mut hasher);
            agent.show_in_quick_create.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn relative_parent(path: &Path) -> String {
    let Some(parent) = path.parent() else {
        return String::new();
    };
    let parent = parent.to_string_lossy().into_owned();
    if parent.is_empty() || parent == "/" {
        return parent;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return parent;
    };
    let home = PathBuf::from(home);
    if parent == home.to_string_lossy() {
        return "~".into();
    }
    parent
        .strip_prefix(&format!("{}/", home.to_string_lossy()))
        .map_or(parent.clone(), |suffix| format!("~/{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use crate::commands::CommandId;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use diri_proto::{
        AgentDescriptor, AgentPathSource, AgentReadinessItem, AgentReadinessResult, HostEntry,
    };
    #[cfg(target_os = "macos")]
    use gpui::HeadlessAppContext;
    use gpui::{Entity, ScrollDelta, ScrollWheelEvent, TestAppContext, point};

    struct CommandPalettePreviewHarness {
        overlay: Entity<NavigationOverlay>,
    }

    impl Render for CommandPalettePreviewHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(gpui::rgb(0x191b20))
                .child(crate::root::cached_window_overlay(self.overlay.clone()))
        }
    }

    #[test]
    fn relative_parent_abbreviates_home_like_swift() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert_eq!(relative_parent(&home.join("project")), "~");
        assert_eq!(relative_parent(&home.join("fun/project")), "~/fun");
        assert_eq!(relative_parent(Path::new("/tmp/project")), "/tmp");
    }

    #[test]
    fn debounce_is_the_swift_value() {
        assert_eq!(RANK_DEBOUNCE, std::time::Duration::from_millis(25));
    }

    #[test]
    fn scrolling_to_a_row_counts_the_section_headers_above_it() {
        // Three chats, two quick actions, then three commands.
        assert_eq!(command_row_child_index(0, 3, 2, 5, true), 1);
        assert_eq!(command_row_child_index(2, 3, 2, 5, true), 3);
        assert_eq!(command_row_child_index(3, 3, 2, 5, true), 5);
        assert_eq!(command_row_child_index(4, 3, 2, 5, true), 6);
        assert_eq!(command_row_child_index(5, 3, 2, 5, true), 8);
        // Search collapses actions into one Commands section.
        assert_eq!(command_row_child_index(3, 3, 0, 5, false), 5);
        // A searched Quick Open list has no headers at all.
        assert_eq!(row_child_index(3, None), 3);
    }

    fn command_layout(width: f32, height: f32) -> OverlayLayout {
        OverlayLayout::command_palette(gpui::size(px(width), px(height)))
    }

    fn quick_open_layout(width: f32, height: f32) -> OverlayLayout {
        OverlayLayout::quick_open(gpui::size(px(width), px(height)))
    }

    #[test]
    fn overlay_never_grows_past_the_window_it_floats_in() {
        for (width, height) in [
            (1100.0, 700.0),
            (1800.0, 1100.0),
            (900.0, 495.0),
            (600.0, 360.0),
        ] {
            for layout in [
                command_layout(width, height),
                quick_open_layout(width, height),
            ] {
                let total = layout.top_inset + px(SEARCH_HEIGHT + 1.0) + layout.list_height;
                assert!(
                    total <= px(height),
                    "{width}x{height} overflows by {:?}",
                    total - px(height)
                );
                assert!(layout.width <= px(width));
            }
        }
    }

    #[test]
    fn the_list_uses_the_height_the_window_actually_has() {
        let tall = command_layout(1400.0, 1100.0);
        assert_eq!(tall.list_height, px(COMMAND_MAX_LIST_HEIGHT));
        assert_eq!(
            tall.top_inset,
            px((1100.0 - SEARCH_HEIGHT - 1.0 - COMMAND_MAX_LIST_HEIGHT) / 2.0)
        );
        assert!(command_layout(900.0, 360.0).list_height < px(400.0));
        assert_eq!(
            quick_open_layout(1600.0, 3000.0).list_height,
            px(QUICK_OPEN_MAX_LIST_HEIGHT)
        );
        let cramped = command_layout(800.0, 150.0);
        assert_eq!(cramped.list_height, px(MIN_LIST_HEIGHT));
        assert_eq!(cramped.top_inset, px(MIN_TOP_INSET));
    }

    #[gpui::test]
    fn empty_command_palette_begins_with_chats_then_quick_actions(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            if let Some(selected) = fixture.selected_session_id {
                store.select(selected);
            }
        }
        let runtime_for_view = Arc::clone(&runtime);
        let (overlay, cx) = cx.add_window_view(move |_, cx| {
            let mut overlay = NavigationOverlay::opened_for_test(runtime_for_view, cx);
            overlay.refresh_command_items();
            overlay
        });

        overlay.read_with(cx, |overlay, _| {
            assert!(!overlay.ranked_sessions.is_empty());
            assert!(overlay.ranked_sessions.len() <= CHAT_PREVIEW_LIMIT);
            assert_eq!(overlay.quick_action_count(), 2);
            assert!(
                overlay.ranked_actions[..2]
                    .iter()
                    .all(|ranked| is_quick_action(&ranked.item))
            );
            assert!(
                overlay.ranked_actions[2..]
                    .iter()
                    .all(|ranked| !is_quick_action(&ranked.item))
            );
            assert!(matches!(
                overlay.highlighted_command(),
                Some(CommandSelection::Session(_))
            ));
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the deterministic command-palette screenshot artifact"]
    fn render_command_palette_preview_screenshot() {
        let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("set DIRI_VISUAL_OUTPUT to the target PNG path");
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));

        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            if let Some(selected) = fixture.selected_session_id {
                store.select(selected);
            }
        }
        let window = cx
            .open_window(gpui::size(px(1100.0), px(700.0)), move |_, cx| {
                let overlay = cx.new(|cx| {
                    let mut overlay = NavigationOverlay::opened_for_test(runtime, cx);
                    overlay.refresh_command_items();
                    overlay
                });
                cx.new(|_| CommandPalettePreviewHarness { overlay })
            })
            .expect("open headless command-palette window");
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .expect("refresh command-palette window");
        cx.run_until_parked();
        let screenshot = cx
            .capture_screenshot(window.into())
            .expect("capture command-palette screenshot");
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).expect("create screenshot directory");
        }
        screenshot
            .save(output)
            .expect("save command-palette screenshot");
    }

    #[gpui::test]
    fn an_open_palette_rebuilds_its_agent_rows_when_readiness_arrives(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.set_hosts(vec![HostEntry {
                id: "forge".into(),
                name: Some("Forge".into()),
                ssh: "forge.example".into(),
                default_cwd: None,
                node: None,
            }]);
            store.set_default_spawn_host(Some("forge".into()));
        }
        let runtime_for_view = Arc::clone(&runtime);
        let (overlay, cx) = cx.add_window_view(move |_window, cx| {
            let mut overlay = NavigationOverlay::opened_for_test(runtime_for_view, cx);
            overlay.refresh_command_items();
            overlay
        });

        // Forge has not been scanned, so no Agent is advertised as launchable
        // there — but ⌘T still belongs to the saved preference.
        assert!(overlay.read_with(cx, |overlay, _| {
            overlay.ranked_actions.iter().any(|ranked| {
                ranked.item.title == "New Claude Code on Forge"
                    && ranked.item.command == PaletteCommand::Action(CommandId::NewDefaultSession)
            })
        }));

        // A store change that cannot have moved readiness must not rebuild the
        // list: these arrive on the UI tick, and re-ranking under a fixed
        // highlight index moves rows out from under the user's selection.
        let before = overlay.read_with(cx, |overlay, _| overlay.agent_actions_fingerprint);
        overlay.update(cx, |overlay, cx| {
            overlay.highlight = 1;
            overlay.handle_store_change(cx);
        });
        assert_eq!(
            overlay.read_with(cx, |overlay, _| (
                overlay.agent_actions_fingerprint,
                overlay.highlight
            )),
            (before, 1)
        );

        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .set_agent_catalog(AgentReadinessResult {
                host: Some("forge".into()),
                agents: vec![AgentReadinessItem {
                    kind: AgentKind::CODEX,
                    binary: "codex".into(),
                    path: Some("/usr/bin/codex".into()),
                    detected_path: Some("/usr/bin/codex".into()),
                    path_source: Some(AgentPathSource::SystemPath),
                    show_in_quick_create: true,
                    descriptor: Some(AgentDescriptor {
                        id: AgentKind::CODEX_ID.into(),
                        display_name: "Codex".into(),
                        first_class: true,
                        ..AgentDescriptor::default()
                    }),
                    ..AgentReadinessItem::default()
                }],
                ..AgentReadinessResult::default()
            });
        overlay.update(cx, |overlay, cx| overlay.handle_store_change(cx));

        assert!(overlay.read_with(cx, |overlay, _| {
            !overlay.ranked_actions.iter().any(|ranked| {
                ranked.item.title == "New Terminal on Forge"
                    && ranked.item.command == PaletteCommand::Action(CommandId::NewDefaultSession)
            }) && overlay.ranked_actions.iter().any(|ranked| {
                ranked.item.title == "New Codex on Forge"
                    && ranked.item.command
                        == PaletteCommand::SpawnAgent {
                            agent: AgentKind::CODEX,
                            cwd: None,
                            host: Some("forge".into()),
                        }
            })
        }));
    }

    struct WheelHarness {
        overlay: Entity<NavigationOverlay>,
        background_scrolls: Arc<AtomicUsize>,
    }

    impl Render for WheelHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let background_scrolls = Arc::clone(&self.background_scrolls);
            div()
                .size_full()
                .child(div().absolute().inset_0().on_scroll_wheel(move |_, _, _| {
                    background_scrolls.fetch_add(1, AtomicOrdering::Relaxed);
                }))
                .child(crate::root::cached_window_overlay(self.overlay.clone()))
        }
    }

    #[gpui::test]
    fn modal_backdrop_consumes_wheel_events(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let background_scrolls = Arc::new(AtomicUsize::new(0));
        let scroll_probe = Arc::clone(&background_scrolls);
        let (_view, cx) = cx.add_window_view(move |_window, cx| {
            let overlay = cx.new(|cx| NavigationOverlay::opened_for_test(runtime, cx));
            WheelHarness {
                overlay,
                background_scrolls: scroll_probe,
            }
        });

        cx.simulate_event(ScrollWheelEvent {
            position: point(px(8.0), px(320.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
            ..ScrollWheelEvent::default()
        });

        assert_eq!(background_scrolls.load(AtomicOrdering::Relaxed), 0);
    }
}
