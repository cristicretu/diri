mod history_page;
#[cfg(test)]
mod page_tests;

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::commands::{
    CommandId, NAVIGATION_CONTEXT, ToggleCommandPalette, ToggleHistory, ToggleQuickOpen,
};
use crate::fuzzy::{FuzzyMatcher, FuzzyQuery};
use crate::icons::sf_symbol;
use crate::palette::{self, PaletteAction, PaletteCommand, Ranked};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::quick_open::{
    self, DirectoryIndex, QuickOpenItem, QuickOpenSnapshot, RANK_DEBOUNCE, RESULT_LIMIT,
    RankedFolder,
};
use crate::store::{SessionStore, SpawnOptions, StoreRuntime};
use diri_proto::{AgentKind, AttentionLevel, SessionId, SessionRecord};
use diri_term::theme::TermTheme;
use diri_ui::{
    Fill, FloatingSurface, HairlineDivider, Icon, IconName, Ink, LoadingIndicator, Palette, Radius,
    SemanticColors,
};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, Context, FocusHandle, Focusable, FontWeight,
    HighlightStyle, KeyDownEvent, MouseButton, Pixels, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement, StyledText, Task, UniformListScrollHandle, Window, canvas, div,
    ease_out_quint, fill, linear_color_stop, linear_gradient, point, prelude::*, px, rgba, size,
    uniform_list,
};

/// The search field above the results, and the gap the surface keeps from the
/// window edges. Everything else is measured against the live viewport so the
/// list grows into a tall window and never overflows a short one.
const SEARCH_HEIGHT: f32 = 48.0;
const ROW_HEIGHT: f32 = 36.0;
const LIST_HEIGHT: f32 = ROW_HEIGHT * 9.0;
const SURFACE_WIDTH: f32 = 600.0;
const KEYCAP_WIDTH: f32 = 28.0;
const KEYCAP_HEIGHT: f32 = 20.0;
const CHAT_PREVIEW_LIMIT: usize = 7;
const PAGE_DURATION: Duration = Duration::from_millis(140);

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
        let top = (height / 6.0).clamp(12.0, 96.0);
        Self {
            top_inset: px(top),
            width: px((viewport.width.as_f32() - 32.0).clamp(0.0, SURFACE_WIDTH)),
            list_height: px((height - top - SEARCH_HEIGHT - 25.0).clamp(0.0, LIST_HEIGHT)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    CommandPalette,
    QuickOpen,
    History,
    Settings,
    Themes,
}

#[derive(Clone)]
enum CommandSelection {
    Action(PaletteCommand),
    Session(SessionId),
}

struct PageState {
    page: Overlay,
    query: QueryEditor,
    highlight: usize,
    scroll: UniformListScrollHandle,
}

pub struct NavigationOverlay {
    focus_handle: FocusHandle,
    previous_focus_handle: Option<FocusHandle>,
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
    list_scroll: UniformListScrollHandle,
    tokio: Arc<tokio::runtime::Runtime>,
    history: Vec<diri_proto::HistoryEntry>,
    history_loading: bool,
    history_error: Option<String>,
    history_scanner: Option<crate::history::HistoryScanner>,
    history_search: crate::history::HistorySearch,
    history_matches: Vec<usize>,
    history_resuming: Option<String>,
    back_stack: Vec<PageState>,
    page_generation: u64,
    page_direction: f32,
    previous_page_rows: usize,
    last_theme_id: String,
    theme_matches: Vec<TermTheme>,
    theme_error: Option<String>,
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
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio: Arc<tokio::runtime::Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this, _| this.cancel_theme_preview())
            .detach();
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
            previous_focus_handle: None,
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
            list_scroll: UniformListScrollHandle::new(),
            tokio,
            history: Vec::new(),
            history_loading: false,
            history_error: None,
            history_scanner: Some(crate::history::HistoryScanner::default()),
            history_search: crate::history::HistorySearch::default(),
            history_matches: Vec::new(),
            history_resuming: None,
            back_stack: Vec::new(),
            page_generation: 0,
            page_direction: 1.0,
            previous_page_rows: 9,
            last_theme_id: String::new(),
            theme_matches: Vec::new(),
            theme_error: None,
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
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        Self {
            focus_handle: cx.focus_handle(),
            previous_focus_handle: None,
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
            list_scroll: UniformListScrollHandle::new(),
            tokio,
            history: Vec::new(),
            history_loading: false,
            history_error: None,
            history_scanner: Some(crate::history::HistoryScanner::default()),
            history_search: crate::history::HistorySearch::default(),
            history_matches: Vec::new(),
            history_resuming: None,
            back_stack: Vec::new(),
            page_generation: 0,
            page_direction: 1.0,
            previous_page_rows: 9,
            last_theme_id: String::new(),
            theme_matches: Vec::new(),
            theme_error: None,
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
        let mut changed = {
            let store = self.store.read().expect("session store lock poisoned");
            if self.last_theme_id != store.theme_id() {
                self.last_theme_id = store.theme_id().to_owned();
                true
            } else {
                false
            }
        };
        if self.overlay == Some(Overlay::CommandPalette) {
            let fingerprint = {
                let store = self.store.read().expect("session store lock poisoned");
                agent_actions_fingerprint(&store)
            };
            if fingerprint != self.agent_actions_fingerprint {
                let highlighted = self.highlighted_command();
                self.refresh_command_items();
                self.restore_highlight(highlighted.as_ref());
                changed = true;
            }
        }
        if changed && self.is_open() {
            cx.notify();
        }
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
            self.close_overlay(window, cx);
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
            self.close_overlay(window, cx);
        } else {
            self.open_overlay(Overlay::QuickOpen, window, cx);
            self.refresh_directory_index(cx);
        }
    }

    fn open_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_none() {
            self.previous_focus_handle = window
                .focused(cx)
                .filter(|handle| handle != &self.focus_handle);
        }
        self.back_stack.clear();
        self.previous_page_rows = self.page_rows();
        self.page_generation = if self.is_open() {
            self.page_generation + 1
        } else {
            0
        };
        self.prepare_page(overlay, cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn clear_overlay(&mut self, cx: &mut Context<Self>) {
        self.cancel_theme_preview();
        self.back_stack.clear();
        self.overlay = None;
        self.query.clear();
        self.highlight = 0;
        self.ranked_actions.clear();
        self.ranked_sessions.clear();
        self.rank_task = None;
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous_focus = self.previous_focus_handle.take();
        self.clear_overlay(cx);
        if let Some(previous_focus) = previous_focus {
            previous_focus.focus(window, cx);
        }
    }

    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            self.previous_focus_handle = None;
            self.clear_overlay(cx);
        }
    }

    /// Back to the first row, scrolled back to the top of the list.
    fn reset_selection(&mut self) {
        self.highlight = 0;
        self.list_scroll = UniformListScrollHandle::new();
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
                if this.overlay == Some(Overlay::QuickOpen) && !this.query.text().trim().is_empty()
                {
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
        let expected_query = query.clone();
        self.rank_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RANK_DEBOUNCE).await;
            let ranked = cx
                .background_spawn(async move { quick_open::rank(&query, &pool, RESULT_LIMIT) })
                .await;
            this.update(cx, |this, cx| {
                if this.overlay != Some(Overlay::QuickOpen)
                    || this.query.text().trim() != expected_query
                {
                    return;
                }
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
            "escape" => self.close_overlay(window, cx),
            "[" if modifiers.platform => self.back(window, cx),
            "backspace" if self.query.is_empty() => self.back(window, cx),
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
        match self.overlay {
            Some(Overlay::QuickOpen) => self.schedule_rank(cx),
            Some(Overlay::History) => self.filter_history(),
            Some(Overlay::Themes) => {
                self.filter_themes();
                self.preview_highlighted_theme();
            }
            Some(Overlay::CommandPalette) => self.refresh_command_items(),
            _ => {}
        }
        cx.notify();
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        self.highlight = (self.highlight as isize + delta).rem_euclid(count as isize) as usize;
        self.scroll_to_highlight();
        self.preview_highlighted_theme();
        cx.notify();
    }

    fn scroll_to_highlight(&self) {
        self.list_scroll
            .scroll_to_item(self.highlight, ScrollStrategy::Nearest);
    }

    fn visible_count(&self) -> usize {
        match self.overlay {
            Some(Overlay::CommandPalette) => self.ranked_actions.len() + self.ranked_sessions.len(),
            Some(Overlay::QuickOpen) if self.query.text().trim().is_empty() => {
                self.quick_snapshot.recent.len() + self.quick_snapshot.folders.len()
            }
            Some(Overlay::QuickOpen) => self.ranked_items.len(),
            Some(Overlay::History) => self.history_matches.len(),
            Some(Overlay::Settings) => self.settings_items().len(),
            Some(Overlay::Themes) => self.theme_matches.len(),
            None => 0,
        }
    }

    #[cfg(test)]
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
                    self.close_overlay(window, cx);
                }
            }
            Some(Overlay::History) => {
                if let Some(entry) = self.highlighted_history().cloned() {
                    self.resume_history(entry, window, cx);
                }
            }
            Some(Overlay::Settings) => match self.settings_items().get(self.highlight).copied() {
                Some(0) => self.push_page(Overlay::Themes, window, cx),
                Some(_) => {
                    self.close_overlay(window, cx);
                    window.dispatch_action(Box::new(crate::commands::OpenSettings), cx);
                }
                None => {}
            },
            Some(Overlay::Themes) => self.commit_theme(window, cx),
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
                self.close_overlay(window, cx);
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
            PaletteCommand::Themes => self.push_page(Overlay::Themes, window, cx),
            PaletteCommand::Action(CommandId::ToggleQuickOpen) => {
                self.push_page(Overlay::QuickOpen, window, cx)
            }
            PaletteCommand::Action(CommandId::ToggleHistory) => {
                self.push_page(Overlay::History, window, cx)
            }
            PaletteCommand::Action(CommandId::OpenSettings) => {
                self.push_page(Overlay::Settings, window, cx)
            }
            PaletteCommand::Action(id) => {
                self.close_overlay(window, cx);
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
                self.close_overlay(window, cx);
            }
            PaletteCommand::MigrateSelected { target_host } => {
                {
                    let mut store = self.store.write().expect("session store lock poisoned");
                    if let Some(id) = store.selected_session_id().cloned() {
                        store.migrate_session(id, target_host);
                    }
                }
                self.close_overlay(window, cx);
            }
            PaletteCommand::SyncPrefs { host } => {
                self.store
                    .write()
                    .expect("session store lock poisoned")
                    .sync_prefs(host);
                self.close_overlay(window, cx);
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

    pub(crate) fn toggle_history(
        &mut self,
        _: &ToggleHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay == Some(Overlay::History) {
            self.close_overlay(window, cx);
        } else {
            self.open_overlay(Overlay::History, window, cx);
        }
    }

    fn cancel_theme_preview(&mut self) {
        if self
            .store
            .write()
            .expect("session store lock poisoned")
            .preview_theme(None)
        {
            self._runtime.publish_local_change();
        }
    }

    fn prepare_page(&mut self, page: Overlay, cx: &mut Context<Self>) {
        self.rank_task = None;
        self.cancel_theme_preview();
        self.overlay = Some(page);
        self.theme_error = None;
        self.query.clear();
        self.reset_selection();
        self.ranked_items.clear();
        match page {
            Overlay::CommandPalette => self.refresh_command_items(),
            Overlay::QuickOpen => self.refresh_directory_index(cx),
            Overlay::History => {
                self.filter_history();
                self.refresh_history(cx);
            }
            Overlay::Settings => {}
            Overlay::Themes => {
                self.filter_themes();
                let saved = self
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .preferences()
                    .terminal_theme
                    .clone();
                self.highlight = self
                    .theme_matches
                    .iter()
                    .position(|theme| theme.id == saved)
                    .unwrap_or(0);
                self.scroll_to_highlight();
            }
        }
        cx.notify();
    }

    fn push_page(&mut self, page: Overlay, _window: &mut Window, cx: &mut Context<Self>) {
        self.previous_page_rows = self.page_rows();
        if let Some(current) = self.overlay {
            self.back_stack.push(PageState {
                page: current,
                query: self.query.clone(),
                highlight: self.highlight,
                scroll: self.list_scroll.clone(),
            });
        }
        self.prepare_page(page, cx);
        self.page_generation += 1;
        self.page_direction = 1.0;
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.previous_page_rows = self.page_rows();
        if let Some(previous) = self.back_stack.pop() {
            self.prepare_page(previous.page, cx);
            self.query = previous.query;
            self.query_changed(cx);
            self.highlight = previous
                .highlight
                .min(self.visible_count().saturating_sub(1));
            self.list_scroll = previous.scroll;
            self.page_generation += 1;
            self.page_direction = -1.0;
        } else if self.overlay != Some(Overlay::CommandPalette) {
            self.prepare_page(Overlay::CommandPalette, cx);
            self.page_generation += 1;
            self.page_direction = -1.0;
        } else {
            self.close_overlay(window, cx);
        }
    }

    fn settings_items(&self) -> Vec<usize> {
        let query = self.query.text().trim().to_lowercase();
        [
            "Color theme appearance dark light",
            "All settings preferences shortcuts",
        ]
        .iter()
        .enumerate()
        .filter_map(|(index, label)| label.to_lowercase().contains(&query).then_some(index))
        .collect()
    }

    fn filter_themes(&mut self) {
        let query = self.query.text().trim().to_lowercase();
        self.theme_matches = TermTheme::CATALOG
            .into_iter()
            .filter(|theme| theme.name.to_lowercase().contains(&query))
            .collect();
    }

    fn preview_highlighted_theme(&mut self) {
        if self.overlay != Some(Overlay::Themes) {
            return;
        }
        let theme = self
            .theme_matches
            .get(self.highlight)
            .map(|theme| theme.id.to_owned());
        if self
            .store
            .write()
            .expect("session store lock poisoned")
            .preview_theme(theme)
        {
            self._runtime.publish_local_change();
        }
    }

    fn commit_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(theme) = self.theme_matches.get(self.highlight).copied() else {
            return;
        };
        let result = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.terminal_theme = theme.id.to_owned());
        match result {
            Ok(()) => {
                self.close_overlay(window, cx);
                self._runtime.publish_local_change();
            }
            Err(error) => {
                self.theme_error = Some(format!("Could not save theme: {error}"));
                cx.notify();
            }
        }
    }

    fn page_rows(&self) -> usize {
        match self.overlay {
            Some(Overlay::Settings) => 2,
            Some(Overlay::History) => 7,
            _ => 9,
        }
    }

    fn colors(&self) -> SemanticColors {
        crate::app_theme::sidebar_colors(
            self.store
                .read()
                .expect("session store lock poisoned")
                .theme_id(),
        )
    }

    fn render_overlay(&mut self, layout: OverlayLayout, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors();
        let list_height = layout
            .list_height
            .min(px(self.page_rows() as f32 * ROW_HEIGHT));
        let previous_list_height = layout
            .list_height
            .min(px(self.previous_page_rows as f32 * ROW_HEIGHT));
        let count = self.visible_count();
        let entity = cx.entity();
        let page = self.overlay.expect("open palette");
        let placeholder = match page {
            Overlay::CommandPalette => "Search chats or run a command…",
            Overlay::QuickOpen => "Open project…",
            Overlay::History => "Search chats…",
            Overlay::Settings => "Settings…",
            Overlay::Themes => "Color theme…",
        };
        let error = if page == Overlay::History {
            self.history_error.clone()
        } else {
            self.theme_error.clone()
        };
        let content = div()
            .id("palette-page")
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(SEARCH_HEIGHT))
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("palette-back")
                            .debug_selector(|| "palette-back".into())
                            .size(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .when(page != Overlay::CommandPalette, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(move |style| style.bg(Fill::hover(colors, true)))
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| PaletteTooltip("Back · ⌘[".into(), colors))
                                            .into()
                                    })
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.back(window, cx)),
                                    )
                            })
                            .child(sf_symbol(
                                if page == Overlay::CommandPalette {
                                    "magnifyingglass"
                                } else {
                                    "chevron.left"
                                },
                                13.0,
                                colors.secondary,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(13.0))
                            .text_color(if self.query.is_empty() {
                                colors.secondary
                            } else {
                                colors.primary
                            })
                            .child(if self.query.is_empty() {
                                div().child(placeholder).into_any_element()
                            } else {
                                query_label(&self.query)
                            }),
                    )
                    .when(page == Overlay::History, |header| {
                        header.child(
                            div()
                                .id("refresh-history")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::CHIP))
                                .cursor_pointer()
                                .hover(move |style| style.bg(Fill::hover(colors, true)))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| PaletteTooltip("Refresh chats".into(), colors))
                                        .into()
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.refresh_history(cx)))
                                .child(if self.history_loading {
                                    LoadingIndicator::new(
                                        "history-refreshing",
                                        12.0,
                                        colors.secondary,
                                    )
                                    .into_any_element()
                                } else {
                                    sf_symbol("arrow.triangle.2.circlepath", 12.0, colors.secondary)
                                }),
                        )
                    })
                    .child(
                        keycap(colors)
                            .id("close-palette")
                            .debug_selector(|| "palette-escape".into())
                            .cursor_pointer()
                            .hover(move |style| style.bg(Fill::hover(colors, true)))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_overlay(window, cx)),
                            )
                            .child("esc"),
                    ),
            )
            .child(HairlineDivider::horizontal(colors))
            .when_some(error, |view, error| {
                view.child(
                    div()
                        .px(px(16.0))
                        .py(px(6.0))
                        .text_size(px(12.0))
                        .text_color(Ink::DANGER)
                        .child(error),
                )
            })
            .child(
                div()
                    .relative()
                    .my(px(6.0))
                    .h(list_height)
                    .overflow_hidden()
                    .when(count > 0, |view| {
                        view.child(
                            uniform_list("palette-results", count, move |range, _, cx| {
                                entity.update(cx, |this, cx| {
                                    range
                                        .map(|index| this.render_result(index, colors, cx))
                                        .collect()
                                })
                            })
                            .track_scroll(&self.list_scroll)
                            .size_full(),
                        )
                        .child(scroll_fades(self.list_scroll.clone(), colors))
                    })
                    .when(count == 0, |view| {
                        view.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(13.0))
                                .text_color(colors.secondary)
                                .child(if page == Overlay::History && self.history_loading {
                                    "Finding chats…"
                                } else if page == Overlay::QuickOpen
                                    && self.directory_index.is_scanning()
                                {
                                    "Finding projects…"
                                } else {
                                    "No matches"
                                }),
                        )
                    }),
            );
        // Only page changes animate. Typing, selection, and theme previews have
        // stable IDs and do not restart motion or schedule idle frames.
        let direction = self.page_direction;
        let content = if self.page_generation > 0 && !cx.reduce_motion() {
            content
                .with_animation(
                    ("palette-page", self.page_generation),
                    Animation::new(PAGE_DURATION).with_easing(ease_out_quint()),
                    move |view, value| {
                        view.opacity(value)
                            .h(px(SEARCH_HEIGHT + 13.0)
                                + previous_list_height
                                + (list_height - previous_list_height) * value)
                            .overflow_hidden()
                            .relative()
                            .left(px((1.0 - value) * 8.0 * direction))
                    },
                )
                .into_any_element()
        } else {
            content.into_any_element()
        };
        let surface = FloatingSurface::new(
            colors,
            div()
                .id("command-palette")
                .debug_selector(|| "command-palette".into())
                .w(layout.width)
                .text_color(colors.primary)
                .child(content),
        )
        .radius(Radius::PANEL);
        div()
            .absolute()
            .inset_0()
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
                    .bg(rgba(0x00000030))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_overlay(window, cx)),
                    ),
            )
            .child(
                div()
                    // Consume hit tests inside the surface before the dismiss
                    // backdrop sees mouse-down, including header controls.
                    .occlude()
                    .on_mouse_down_out(
                        cx.listener(|this, _, window, cx| this.close_overlay(window, cx)),
                    )
                    .child(surface),
            )
            .into_any_element()
    }

    fn render_result(
        &mut self,
        index: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.overlay == Some(Overlay::History) {
            return self.render_history_row(index, cx);
        }
        let row = match self.overlay {
            Some(Overlay::CommandPalette) => {
                if index < self.ranked_sessions.len() {
                    self.render_session_row(self.ranked_sessions[index].clone(), index, colors, cx)
                } else {
                    self.render_action_row(
                        self.ranked_actions[index - self.ranked_sessions.len()].clone(),
                        index,
                        colors,
                        cx,
                    )
                }
            }
            Some(Overlay::QuickOpen) => {
                if self.query.text().trim().is_empty() {
                    let item = self
                        .quick_snapshot
                        .recent
                        .iter()
                        .chain(&self.quick_snapshot.folders)
                        .nth(index)
                        .expect("visible project")
                        .clone();
                    self.render_quick_row(item, &[], index, colors, cx)
                } else {
                    let ranked = self.ranked_items[index].clone();
                    self.render_quick_row(ranked.item, &ranked.name_matches, index, colors, cx)
                }
            }
            Some(Overlay::Settings | Overlay::Themes) => self.render_setting_row(index, colors, cx),
            _ => div().into_any_element(),
        };
        div()
            .h(px(ROW_HEIGHT))
            .px(px(6.0))
            .py(px(2.0))
            .child(row)
            .into_any_element()
    }

    fn render_setting_row(
        &mut self,
        index: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = (self.overlay == Some(Overlay::Themes)).then(|| self.theme_matches[index]);
        let title = theme.map_or_else(
            || {
                if self.settings_items()[index] == 0 {
                    "Color theme"
                } else {
                    "All settings"
                }
            },
            |theme| theme.name,
        );
        let leading = theme.map_or_else(
            || {
                sf_symbol(
                    if self.settings_items()[index] == 0 {
                        "moon.fill"
                    } else {
                        "gearshape"
                    },
                    13.0,
                    colors.secondary,
                )
            },
            |theme| {
                div()
                    .size(px(16.0))
                    .rounded(px(5.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(colors.primary.alpha(0.2))
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(px(6.0)).rounded_full().bg(theme.ansi[4]))
                    .into_any_element()
            },
        );
        palette_row(
            div().child(title).into_any_element(),
            leading,
            Vec::new(),
            index == self.highlight,
            index,
            true,
            colors,
        )
        .when(
            theme.is_some_and(|theme| {
                theme.id
                    == self
                        .store
                        .read()
                        .expect("session store lock poisoned")
                        .preferences()
                        .terminal_theme
            }),
            |row| row.child(sf_symbol("checkmark", 12.0, colors.secondary)),
        )
        .when(theme.is_none(), |row| {
            row.child(sf_symbol("chevron.right", 11.0, colors.tertiary))
        })
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.highlight != index {
                this.highlight = index;
                this.preview_highlighted_theme();
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.highlight = index;
            this.run_highlighted(false, window, cx);
        }))
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
        let opens_page = matches!(
            command,
            PaletteCommand::Themes
                | PaletteCommand::Action(
                    CommandId::ToggleQuickOpen | CommandId::ToggleHistory | CommandId::OpenSettings
                )
        );
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
        .when(opens_page, |row| {
            row.child(sf_symbol("chevron.right", 11.0, colors.tertiary))
        })
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.highlight != index {
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
            if *hovered && this.highlight != index {
                this.highlight = index;
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.run_command_selection(CommandSelection::Session(id.clone()), window, cx);
        }))
        .into_any_element()
    }

    fn render_quick_row(
        &mut self,
        item: QuickOpenItem,
        matches: &[Range<usize>],
        index: usize,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let parent = relative_parent(&item.path);
        let title = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .child(highlighted_label(item.name, matches)),
            )
            .when(index == self.highlight, |title| {
                title.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(colors.tertiary)
                        .child(parent),
                )
            });
        let detail = format!(
            "{}\nEnter to open · {} for a terminal",
            item.path.display(),
            crate::commands::primary_shortcut_label("Enter")
        );
        palette_row(
            title.into_any_element(),
            sf_symbol(
                if item.is_git_repo {
                    "folder.fill"
                } else {
                    "folder"
                },
                13.0,
                colors.secondary,
            ),
            Vec::new(),
            index == self.highlight,
            index,
            true,
            colors,
        )
        .tooltip(move |_, cx| cx.new(|_| PaletteTooltip(detail.clone(), colors)).into())
        .when(index == self.highlight, |row| {
            row.child(keycap(colors).child(Icon::new(IconName::Return, 14.0, colors.secondary)))
        })
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.highlight != index {
                this.highlight = index;
                cx.notify();
            }
        }))
        .on_click(
            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                this.highlight = index;
                this.run_highlighted(event.modifiers().platform, window, cx);
            }),
        )
        .into_any_element()
    }
}

impl Focusable for NavigationOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NavigationOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = OverlayLayout::command_palette(window.viewport_size());
        let overlay = self.overlay.map(|_| self.render_overlay(layout, cx));
        let root = div()
            .id("navigation-overlay")
            .key_context(NAVIGATION_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_quick_open))
            .on_action(cx.listener(Self::toggle_history))
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
        .debug_selector(move || format!("palette-row-{index}"))
        .flex()
        // Without this the rows are shrinkable flex children: a list taller
        // than its container squeezes every row toward min-content instead of
        // scrolling, and 40pt rows render as ~21pt of crammed text.
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h_full()
        .px(px(10.0))
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
                .flex_1()
                .flex()
                .items_center()
                .gap(px(6.0))
                .min_w_0()
                .child(
                    div()
                        .w(px(28.0))
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

/// Keyboard hints share a footprint and border, so the header and every row
/// end on the same vertical axis regardless of their text or icon contents.
fn keycap(colors: SemanticColors) -> gpui::Div {
    div()
        .flex_none()
        .w(px(KEYCAP_WIDTH))
        .h(px(KEYCAP_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .border_1()
        .border_color(colors.floating_stroke())
        .text_size(px(11.0))
        .text_color(colors.secondary)
}

struct PaletteTooltip(String, SemanticColors);

impl Render for PaletteTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(440.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(Radius::ROW))
            .bg(self.1.floating_surface())
            .border_1()
            .border_color(self.1.floating_stroke())
            .text_size(px(11.0))
            .text_color(self.1.primary)
            .child(self.0.clone())
    }
}

/// Paint after the virtual list has laid out: both wheel scrolling and deferred
/// keyboard selection then use this frame's offset. A canvas adds no hitbox, so
/// the fade never intercepts clicks or scrolling at the edges.
fn scroll_fades(scroll: UniformListScrollHandle, colors: SemanticColors) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let handle = &scroll.0.borrow().base_handle;
            let scrolled = f32::from(handle.offset().y).min(0.0).abs();
            let remaining = (f32::from(handle.max_offset().y) - scrolled).max(0.0);
            for (distance, angle, top) in [(scrolled, 180.0, true), (remaining, 0.0, false)] {
                let strength = (distance / 14.0).min(1.0);
                if strength <= 0.01 {
                    continue;
                }
                let height = px(16.0);
                let origin = if top {
                    bounds.origin
                } else {
                    point(bounds.left(), bounds.bottom() - height)
                };
                let color: gpui::Hsla = colors.floating_surface().alpha(strength).into();
                window.paint_quad(fill(
                    Bounds::new(origin, size(bounds.size.width, height)),
                    linear_gradient(
                        angle,
                        linear_color_stop(color, 0.0),
                        linear_color_stop(color.opacity(0.0), 1.0),
                    ),
                ));
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full()
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

    struct OverlayFocusHarness {
        previous_focus: FocusHandle,
        overlay: Entity<NavigationOverlay>,
    }

    impl Render for OverlayFocusHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .id("previous-focus-surface")
                        .track_focus(&self.previous_focus),
                )
                .child(crate::root::cached_window_overlay(self.overlay.clone()))
        }
    }

    /// Mounted only by the screenshot fixture, which is macOS-only.
    #[cfg(target_os = "macos")]
    struct CommandPalettePreviewHarness {
        overlay: Entity<NavigationOverlay>,
    }

    #[cfg(target_os = "macos")]
    impl Render for CommandPalettePreviewHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(self.overlay.read(cx).colors().background)
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
    fn palette_fits_small_windows_and_keeps_one_geometry_for_every_page() {
        for (width, height) in [(1100.0, 700.0), (600.0, 360.0), (320.0, 180.0)] {
            let layout = OverlayLayout::command_palette(size(px(width), px(height)));
            assert!(layout.width <= px(width));
            assert!(layout.top_inset + px(SEARCH_HEIGHT + 13.0) + layout.list_height <= px(height));
            assert!(layout.list_height <= px(LIST_HEIGHT));
        }
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

    #[gpui::test]
    fn opening_command_palette_claims_input_from_the_previous_surface(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let runtime_for_view = Arc::clone(&runtime);
        let (view, cx) = cx.add_window_view(move |window, cx| {
            let previous_focus = cx.focus_handle();
            previous_focus.focus(window, cx);
            let overlay = cx.new(|cx| {
                let mut overlay = NavigationOverlay::opened_for_test(runtime_for_view, cx);
                overlay.clear_overlay(cx);
                overlay
            });
            OverlayFocusHarness {
                previous_focus,
                overlay,
            }
        });
        let overlay = view.read_with(cx, |view, _| view.overlay.clone());

        overlay.update_in(cx, |overlay, window, cx| {
            overlay.open_overlay(Overlay::CommandPalette, window, cx);
        });
        cx.simulate_keystrokes("x");

        overlay.read_with(cx, |overlay, _| {
            assert_eq!(
                overlay.query.text(),
                "x",
                "the first palette keystroke must not remain trapped in the previous surface"
            );
        });

        cx.simulate_keystrokes("escape");
        assert!(
            !overlay.read_with(cx, |overlay, _| overlay.is_open()),
            "the first Escape should close the command palette"
        );
        view.update_in(cx, |view, window, _| {
            assert!(
                view.previous_focus.is_focused(window),
                "closing the palette should return keyboard input to its previous surface"
            );
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
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });

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
                    if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                        overlay
                            .store
                            .write()
                            .unwrap()
                            .update_preferences(|prefs| {
                                prefs.terminal_theme = "dirijor-light".into()
                            })
                            .unwrap();
                    }
                    overlay.refresh_command_items();
                    match std::env::var("DIRI_VISUAL_PAGE").as_deref() {
                        Ok("history") => super::page_tests::seed_history(&mut overlay),
                        Ok("projects") => {
                            overlay.overlay = Some(Overlay::QuickOpen);
                            overlay.quick_snapshot.recent = [
                                "diri",
                                "anara",
                                "website",
                                "design-system",
                                "docs",
                                "experiments",
                                "mobile",
                                "playground",
                                "research",
                                "archive",
                            ]
                            .into_iter()
                            .map(|name| QuickOpenItem {
                                name: name.into(),
                                path: PathBuf::from(format!("/Users/demo/fun/{name}")),
                                is_git_repo: true,
                            })
                            .collect();
                        }
                        Ok("settings") => overlay.overlay = Some(Overlay::Settings),
                        Ok("themes") => {
                            overlay.overlay = Some(Overlay::Themes);
                            overlay.filter_themes();
                            overlay.highlight = overlay
                                .theme_matches
                                .iter()
                                .position(|theme| {
                                    theme.id == overlay.store.read().unwrap().theme_id()
                                })
                                .unwrap_or(0);
                            overlay.scroll_to_highlight();
                        }
                        _ => {}
                    }
                    if let Ok(query) = std::env::var("DIRI_VISUAL_QUERY") {
                        overlay.query.insert(&query);
                        overlay.query_changed(cx);
                    }
                    overlay
                });
                cx.new(|_| CommandPalettePreviewHarness { overlay })
            })
            .expect("open headless command-palette window");
        cx.run_until_parked();
        cx.update_window(window.into(), |view, window, cx| {
            view.downcast::<CommandPalettePreviewHarness>()
                .unwrap()
                .update(cx, |view, cx| {
                    view.overlay.update(cx, |_, cx| cx.notify());
                });
            window.refresh();
        })
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
