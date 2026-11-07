//! The Files surface of the trailing workbench: a file tree and workspace
//! search in a collapsible sidebar, beside the code editor.
//!
//! `code_intelligence` owns filesystem discovery, containment, loading and
//! saving; `file_tree` the tree model; `code_editor` the editor. This module
//! owns the surface: asynchronous opens, navigation history, drafts of
//! unsaved files, the sidebar's two modes (Files and Search), the quick
//! open picker, and the toolbar and breadcrumb.

use crate::tooltip_warmth::WarmTooltip;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, KeyBinding, KeyDownEvent,
    MouseButton, Pixels, Render, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, actions, canvas, div, prelude::*, px, uniform_list,
};

use crate::code_editor::find::FindOptions;
use crate::code_editor::search::{
    self as workspace_search, ResultRow, SearchFilters, SearchOutcome,
};
use crate::code_editor::{CodeEditor, Document, EditorEvent, EditorPalette};
use crate::code_intelligence::{
    CodeIntelligence, CodeIntelligenceError, SearchHit, SearchHitKind, SourceSnapshot,
};
use crate::file_tree::{FileTree, FilterResults, Move, TreeRow};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use diri_term::theme::TermTheme;
use diri_ui::{FloatingSurface, Radius, SemanticColors, Typo};

const TREE_ROW_HEIGHT: f32 = 22.0;
const SEARCH_ROW_HEIGHT: f32 = 22.0;
/// Below this width the sidebar floats over the editor as a drawer.
const DOCKED_MIN_WIDTH: f32 = 600.0;
/// Quiet time after typing before a workspace search or filter runs.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(220);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

actions!(diri_files, [QuickOpen, SearchWorkspace]);

pub(crate) const FILES_CONTEXT: &str = "DiriFiles";

/// The Files surface's keys and the editor's, in their own key contexts.
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("cmd-p", QuickOpen, Some(FILES_CONTEXT)),
        KeyBinding::new("cmd-shift-f", SearchWorkspace, Some(FILES_CONTEXT)),
    ];
    bindings.extend(crate::code_editor::key_bindings());
    bindings
}

#[derive(Clone)]
enum ViewerState {
    Empty,
    Loading { reference: String },
    Ready,
    Error { reference: String, message: String },
}

/// What the sidebar shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sidebar {
    Files,
    Search,
}

/// Which field the surface's own key handler types into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Tree,
    Filter,
    Query,
    Include,
    Exclude,
}

struct ExplorerTooltip(String, SemanticColors);

impl Render for ExplorerTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(420.0))
            .px(px(9.0))
            .py(px(6.0))
            .rounded(px(Radius::BADGE))
            .bg(self.1.sidebar_surface())
            .border_1()
            .border_color(self.1.primary.alpha(0.15))
            .text_size(px(11.0))
            .text_color(self.1.primary)
            .child(self.0.clone())
    }
}

/// Corner radius of the search popup; the menu radius like every dropdown.
const CODE_PICKER_RADIUS: f32 = crate::floating::MENU_RADIUS;

/// The search popup as a panel target (see `crate::floating::Target`).
const CODE_PICKER: crate::floating::Target<CodeViewer> = crate::floating::Target {
    key: "code-picker",
    radius: CODE_PICKER_RADIUS,
    content: CodeViewer::picker_panel_content,
    dismiss: |this, _, cx| {
        this.picker_open = false;
        cx.notify();
    },
};

/// The workspace search sidebar's state.
#[derive(Default)]
struct SearchPanel {
    query: QueryEditor,
    include: QueryEditor,
    exclude: QueryEditor,
    options: FindOptions,
    show_filters: bool,
    outcome: SearchOutcome,
    rows: Vec<ResultRow>,
    collapsed: HashSet<PathBuf>,
    pending: bool,
    error: Option<String>,
    selected: Option<usize>,
    generation: u64,
}

pub struct CodeViewer {
    /// Kept for the host's signature; blocking work runs on GPUI's
    /// background executor so it is ordered with the UI in tests too.
    _tokio: tokio::runtime::Handle,
    focus: FocusHandle,
    colors: SemanticColors,
    theme: Option<TermTheme>,
    font_family: SharedString,
    workspace_cwd: Option<PathBuf>,
    intelligence: Option<Arc<CodeIntelligence>>,
    state: ViewerState,
    editor: Entity<CodeEditor>,
    _editor_events: Subscription,
    /// Unsaved documents of files not on screen, by absolute path. They
    /// outlive workspace switches, so edits are never dropped silently.
    drafts: HashMap<PathBuf, Document>,
    generation: u64,
    _load_task: Option<Task<()>>,
    history: Vec<(PathBuf, String)>,
    history_index: usize,
    // Sidebar.
    sidebar_visible: bool,
    /// The sidebar floating over the editor in a narrow panel.
    drawer_open: bool,
    sidebar: Sidebar,
    field: Field,
    tree: FileTree,
    tree_generation: u64,
    tree_scroll: UniformListScrollHandle,
    filter: QueryEditor,
    filter_generation: u64,
    _filter_task: Option<Task<()>>,
    _git_task: Option<Task<()>>,
    search: SearchPanel,
    search_cancel: Arc<AtomicU64>,
    _search_task: Option<Task<()>>,
    search_scroll: UniformListScrollHandle,
    width: Pixels,
    // Quick open.
    picker_open: bool,
    query: QueryEditor,
    results: Vec<SearchHit>,
    highlighted_result: usize,
    picker_generation: u64,
    picker_pending: bool,
    picker_error: Option<String>,
    _picker_task: Option<Task<()>>,
    result_scroll: gpui::ScrollHandle,
    // Watching the open file for outside writes.
    _watcher: Option<notify::RecommendedWatcher>,
    _watch_task: Option<Task<()>>,
    watched: Option<PathBuf>,
    /// The file in the editor, for the tab title.
    open_file: Option<PathBuf>,
    /// Whether outside writes to the open file are watched (tests turn it off).
    watch_files: bool,
}

impl CodeViewer {
    pub fn new(
        tokio: tokio::runtime::Handle,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Self {
        let palette = EditorPalette::from_theme(&fallback_theme(colors));
        let editor = cx.new(|cx| CodeEditor::new(palette, colors, cx));
        let editor_events = cx.subscribe(&editor, Self::on_editor_event);
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        Self {
            _tokio: tokio,
            focus: cx.focus_handle(),
            colors,
            theme: None,
            font_family: crate::fonts::mono_family().into(),
            workspace_cwd: None,
            intelligence: None,
            state: ViewerState::Empty,
            editor,
            _editor_events: editor_events,
            drafts: HashMap::new(),
            generation: 0,
            _load_task: None,
            history: Vec::new(),
            history_index: 0,
            sidebar_visible: true,
            drawer_open: false,
            sidebar: Sidebar::Files,
            field: Field::Tree,
            tree: FileTree::default(),
            tree_generation: 0,
            tree_scroll: UniformListScrollHandle::new(),
            filter: QueryEditor::default(),
            filter_generation: 0,
            _filter_task: None,
            _git_task: None,
            search: SearchPanel::default(),
            search_cancel: Arc::new(AtomicU64::new(0)),
            _search_task: None,
            search_scroll: UniformListScrollHandle::new(),
            width: px(0.0),
            picker_open: false,
            query: QueryEditor::default(),
            results: Vec::new(),
            highlighted_result: 0,
            picker_generation: 0,
            picker_pending: false,
            picker_error: None,
            _picker_task: None,
            result_scroll: gpui::ScrollHandle::new(),
            _watcher: None,
            _watch_task: None,
            watched: None,
            open_file: None,
            watch_files: true,
        }
    }

    pub fn set_colors(&mut self, colors: SemanticColors, cx: &mut Context<Self>) {
        if self.colors == colors {
            return;
        }
        self.colors = colors;
        self.restyle(cx);
        cx.notify();
    }

    /// The terminal theme and font the editor paints with, so code reads
    /// like the agent's terminal beside it.
    pub fn set_terminal_style(
        &mut self,
        theme: TermTheme,
        font_family: &str,
        cx: &mut Context<Self>,
    ) {
        let family: SharedString = crate::fonts::terminal_family(font_family).to_owned().into();
        if self.theme == Some(theme) && self.font_family == family {
            return;
        }
        self.theme = Some(theme);
        self.font_family = family;
        self.restyle(cx);
        cx.notify();
    }

    fn restyle(&mut self, cx: &mut Context<Self>) {
        let theme = self.theme.unwrap_or_else(|| fallback_theme(self.colors));
        let palette = EditorPalette::from_theme(&theme);
        let (colors, family) = (self.colors, self.font_family.clone());
        self.editor.update(cx, |editor, cx| {
            editor.set_style(palette, colors, family, cx)
        });
    }

    pub(crate) fn tab_label(&self) -> Option<String> {
        self.open_file
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
    }

    #[cfg(test)]
    pub(crate) fn appearance(&self) -> diri_ui::Appearance {
        self.colors.appearance
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> Entity<CodeEditor> {
        self.editor.clone()
    }

    /// Opens this repository's `code_intelligence.rs` synchronously, for
    /// screenshots and tests.
    #[cfg(test)]
    pub(crate) fn seed_explorer_preview(&mut self, cx: &mut Context<Self>) {
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let intelligence = Arc::new(CodeIntelligence::for_session(&cwd).unwrap());
        let line = std::env::var("DIRI_VISUAL_LINE").unwrap_or_else(|_| "65".into());
        let snapshot = intelligence
            .open_reference(&format!(
                "diri/crates/diri-app/src/code_intelligence.rs:{line}"
            ))
            .unwrap();
        self.workspace_cwd = Some(cwd);
        for parent in snapshot.relative_path.ancestors().skip(1) {
            self.tree.finish_load(
                parent.to_path_buf(),
                intelligence
                    .directory_entries(parent)
                    .map_err(|error| error.to_string()),
            );
        }
        self.tree.set_git(intelligence.git_snapshot());
        self.intelligence = Some(intelligence.clone());
        self.attach_intelligence(cx);
        self.show_snapshot(snapshot, cx);
        self.drawer_open = std::env::var_os("DIRI_VISUAL_DRAWER").is_some();
        if std::env::var_os("DIRI_VISUAL_FIND").is_some() {
            self.editor.update(cx, |editor, cx| {
                editor.find.open = true;
                editor.find.replace_open = true;
                editor.find.query.insert("workspace");
                editor.find.replacement.insert("root");
                editor.find_changed(cx);
            });
        }
        if std::env::var_os("DIRI_VISUAL_COMPLETION").is_some() {
            self.editor.update(cx, |editor, cx| {
                use crate::code_editor::intel::{Candidate, CandidateKind};
                let head = editor.primary().head;
                editor.completion = Some(crate::code_editor::Completion {
                    word: head..head,
                    items: vec![
                        Candidate {
                            label: "workspace_root".into(),
                            detail: None,
                            kind: CandidateKind::Word,
                        },
                        Candidate {
                            label: "workspace_index".into(),
                            detail: Some("fn workspace_index(&self) -> &WorkspaceIndex".into()),
                            kind: CandidateKind::Symbol,
                        },
                        Candidate {
                            label: "WorkspaceIndex".into(),
                            detail: Some("struct WorkspaceIndex".into()),
                            kind: CandidateKind::Symbol,
                        },
                    ],
                    selected: 1,
                });
                cx.notify();
            });
        }
        if std::env::var_os("DIRI_VISUAL_SEARCH").is_some() {
            self.sidebar = Sidebar::Search;
            self.search.query.insert("directory_entries");
            self.search.outcome = workspace_search::run(
                &intelligence,
                "directory_entries",
                FindOptions::default(),
                &SearchFilters::default(),
                || false,
            )
            .unwrap_or_default();
            self.search.rows =
                workspace_search::result_rows(&self.search.outcome, &self.search.collapsed);
        }
        cx.notify();
    }

    fn attach_intelligence(&mut self, cx: &mut Context<Self>) {
        let intelligence = self.intelligence.clone();
        self.editor
            .update(cx, |editor, _| editor.set_intelligence(intelligence));
    }

    /// Blocking work against the workspace's intelligence, creating the
    /// intelligence on first use; callers run it on GPUI's background
    /// executor, never on the main thread.
    fn with_intelligence<T, W>(
        &self,
        work: W,
    ) -> Option<
        impl FnOnce() -> Result<(Arc<CodeIntelligence>, T), String> + Send + 'static + use<T, W>,
    >
    where
        T: Send + 'static,
        W: FnOnce(&Arc<CodeIntelligence>) -> Result<T, CodeIntelligenceError> + Send + 'static,
    {
        let cwd = self.workspace_cwd.clone()?;
        let intelligence = self.intelligence.clone();
        Some(move || {
            let intelligence = match intelligence {
                Some(intelligence) => intelligence,
                None => {
                    Arc::new(CodeIntelligence::for_session(cwd).map_err(|error| error.to_string())?)
                }
            };
            let value = work(&intelligence).map_err(|error| error.to_string())?;
            Ok((intelligence, value))
        })
    }

    fn adopt_intelligence(&mut self, intelligence: Arc<CodeIntelligence>, cx: &mut Context<Self>) {
        if self
            .intelligence
            .as_ref()
            .is_none_or(|known| !Arc::ptr_eq(known, &intelligence))
        {
            let first = self.intelligence.is_none();
            self.intelligence = Some(intelligence);
            self.attach_intelligence(cx);
            if first {
                self.refresh_git(cx);
            }
        }
    }

    // ---- tree --------------------------------------------------------------

    fn load_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.tree.begin_load(&path) {
            return;
        }
        let requested = path.clone();
        let Some(work) =
            self.with_intelligence(move |intelligence| intelligence.directory_entries(&requested))
        else {
            self.tree.finish_load(path, Err("No workspace".into()));
            return;
        };
        let generation = self.tree_generation;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.tree_generation != generation {
                    return;
                }
                match result {
                    Ok((intelligence, entries)) => {
                        this.adopt_intelligence(intelligence, cx);
                        this.tree.finish_load(path, Ok(entries));
                    }
                    Err(error) => this.tree.finish_load(path, Err(error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let Some(work) = self.with_intelligence(|intelligence| Ok(intelligence.git_snapshot()))
        else {
            return;
        };
        let generation = self.tree_generation;
        self._git_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.tree_generation != generation {
                    return;
                }
                if let Ok((_, git)) = result {
                    this.tree.set_git(git);
                    cx.notify();
                }
            });
        }));
    }

    /// Reloads every open folder, Git state, and the open file when clean.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.tree_generation = self.tree_generation.wrapping_add(1);
        self.intelligence = None;
        self.tree.forget_loaded();
        self.load_directory(PathBuf::new(), cx);
        for path in self.tree.expanded().cloned().collect::<Vec<_>>() {
            self.load_directory(path, cx);
        }
        self.refresh_git(cx);
        self.reload_open_file_if_clean(cx);
        if !self.filter.is_empty() {
            self.schedule_filter(cx);
        }
        cx.notify();
    }

    fn activate_tree_row(&mut self, row: &TreeRow, cx: &mut Context<Self>) {
        self.tree.select(Some(row.path.clone()));
        if row.is_dir {
            let open = !self.tree.is_expanded(&row.path);
            if self.tree.set_expanded(&row.path, open) {
                self.load_directory(row.path.clone(), cx);
            }
        } else {
            self.open_path(row.path.clone(), None, cx);
        }
        cx.notify();
    }

    fn tree_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let rows = self.tree.rows();
        let at = self
            .tree
            .selected()
            .and_then(|selected| rows.iter().position(|row| row.path == selected))
            .unwrap_or(0);
        let page = 12;
        let Some(step) = crate::file_tree::step(&rows, at, key, page) else {
            return false;
        };
        match step {
            Move::To(next) => {
                self.tree.select(Some(rows[next].path.clone()));
                self.tree_scroll
                    .scroll_to_item(next, ScrollStrategy::Nearest);
            }
            Move::Expand(path) => {
                if self.tree.set_expanded(&path, true) {
                    self.load_directory(path, cx);
                }
            }
            Move::Collapse(path) => {
                self.tree.set_expanded(&path, false);
            }
            Move::Activate(index) => {
                let row = rows[index].clone();
                self.activate_tree_row(&row, cx);
            }
        }
        cx.notify();
        true
    }

    /// Scrolls the tree to the open file and selects it.
    fn reveal_active(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.editor.read(cx).relative_path().map(Path::to_path_buf) else {
            return;
        };
        if self.tree.filter().is_some() {
            self.filter.clear();
            self.tree.set_filter(None);
        }
        for missing in self.tree.reveal(&path) {
            self.load_directory(missing, cx);
        }
        if let Some(index) = self.tree.rows().iter().position(|row| row.path == path) {
            self.tree_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn schedule_filter(&mut self, cx: &mut Context<Self>) {
        self.filter_generation = self.filter_generation.wrapping_add(1);
        let generation = self.filter_generation;
        let query = self.filter.text().trim().to_owned();
        if query.is_empty() {
            self._filter_task = None;
            self.tree.set_filter(None);
            cx.notify();
            return;
        }
        let lookup = query.clone();
        let Some(work) =
            self.with_intelligence(move |intelligence| Ok(intelligence.search_files(&lookup, 300)))
        else {
            return;
        };
        self._filter_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(90))
                .await;
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.filter_generation != generation {
                    return;
                }
                if let Ok((intelligence, hits)) = result {
                    this.adopt_intelligence(intelligence, cx);
                    this.tree.set_filter(Some(FilterResults {
                        query,
                        paths: hits.into_iter().map(|hit| hit.relative_path).collect(),
                    }));
                    this.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
                cx.notify();
            });
        }));
    }

    // ---- workspace search --------------------------------------------------

    fn schedule_search(&mut self, immediate: bool, cx: &mut Context<Self>) {
        self.search.generation = self.search.generation.wrapping_add(1);
        let generation = self.search.generation;
        self.search_cancel.store(generation, Ordering::Relaxed);
        let cancel = self.search_cancel.clone();
        let query = self.search.query.text().to_owned();
        if query.is_empty() {
            self._search_task = None;
            self.search.pending = false;
            self.search.error = None;
            self.search.outcome = SearchOutcome::default();
            self.search.rows.clear();
            cx.notify();
            return;
        }
        let options = self.search.options;
        let filters = SearchFilters {
            include: self.search.include.text().to_owned(),
            exclude: self.search.exclude.text().to_owned(),
        };
        let Some(cwd) = self.workspace_cwd.clone() else {
            return;
        };
        let intelligence = self.intelligence.clone();
        self.search.pending = true;
        cx.notify();
        self._search_task = Some(cx.spawn(async move |this, cx| {
            if !immediate {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    let intelligence = match intelligence {
                        Some(intelligence) => intelligence,
                        None => Arc::new(
                            CodeIntelligence::for_session(cwd)
                                .map_err(|error| error.to_string())?,
                        ),
                    };
                    let outcome =
                        workspace_search::run(&intelligence, &query, options, &filters, || {
                            cancel.load(Ordering::Relaxed) != generation
                        })?;
                    Ok::<_, String>((intelligence, outcome))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.search.generation != generation {
                    return;
                }
                this.search.pending = false;
                match result {
                    Ok((intelligence, outcome)) => {
                        this.adopt_intelligence(intelligence, cx);
                        this.search.error = None;
                        this.search.outcome = outcome;
                        this.search.collapsed.clear();
                        this.search.selected = None;
                        this.search.rows = workspace_search::result_rows(
                            &this.search.outcome,
                            &this.search.collapsed,
                        );
                        this.search_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                    Err(error) => {
                        this.search.error = Some(error);
                        this.search.outcome = SearchOutcome::default();
                        this.search.rows.clear();
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Opens the search sidebar, seeded with `text` when given.
    fn open_search(
        &mut self,
        text: Option<String>,
        whole_word: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_sidebar();
        self.sidebar = Sidebar::Search;
        self.field = Field::Query;
        if let Some(text) = text.filter(|text| !text.is_empty() && !text.contains('\n')) {
            self.search.query.clear();
            self.search.query.insert(&text);
            self.search.options.word = whole_word;
            self.schedule_search(true, cx);
        }
        self.search.query.select_all();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn open_search_hit(&mut self, file: usize, hit: usize, cx: &mut Context<Self>) {
        let Some(found) = self.search.outcome.files.get(file) else {
            return;
        };
        let Some(line) = found.hits.get(hit) else {
            return;
        };
        let path = found.relative_path.clone();
        self.open_path(path, Some((line.line, line.column)), cx);
    }

    // ---- opening files -----------------------------------------------------

    /// Selects the local workspace represented by the active agent. Switching
    /// workspaces drops the previous one's tree, search and history; an open
    /// file with unsaved edits is kept as a draft.
    pub fn set_workspace(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.workspace_cwd == cwd {
            return;
        }
        self.workspace_cwd = cwd;
        self.tree_generation = self.tree_generation.wrapping_add(1);
        self.tree.clear();
        self.tree_scroll = UniformListScrollHandle::new();
        self.filter.clear();
        self._filter_task = None;
        self._git_task = None;
        self.intelligence = None;
        self.attach_intelligence(cx);
        self.state = ViewerState::Empty;
        let previous = self
            .editor
            .update(cx, |editor, cx| editor.set_document(None, cx));
        if let Some(previous) = previous.filter(Document::is_dirty) {
            self.drafts.insert(previous.absolute_path.clone(), previous);
        }
        self.generation = self.generation.wrapping_add(1);
        self.search_cancel.fetch_add(1, Ordering::Relaxed);
        self.search = SearchPanel::default();
        self._search_task = None;
        self.picker_open = false;
        self.query.clear();
        self.results.clear();
        self.highlighted_result = 0;
        self.history.clear();
        self.history_index = 0;
        self.open_file = None;
        self.unwatch();
        cx.notify();
    }

    /// Opens a terminal-shaped reference relative to a session cwd. All path
    /// safety and parsing stay behind `CodeIntelligence`'s interface.
    pub fn open_reference(
        &mut self,
        cwd: impl Into<PathBuf>,
        reference: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let cwd = cwd.into();
        let reference = reference.into();
        if self.workspace_cwd.as_ref() != Some(&cwd) {
            self.set_workspace(Some(cwd.clone()), cx);
        }
        self.open_reference_inner(cwd, reference, true, cx);
    }

    /// Opens a workspace-relative path, optionally at a one-based line and column.
    fn open_path(&mut self, path: PathBuf, at: Option<(usize, usize)>, cx: &mut Context<Self>) {
        let Some(root) = self
            .intelligence
            .as_ref()
            .map(|intelligence| intelligence.workspace_root().to_path_buf())
            .or_else(|| self.workspace_cwd.clone())
        else {
            return;
        };
        let Ok(mut reference) = url::Url::from_file_path(root.join(&path)) else {
            return;
        };
        if let Some((line, column)) = at {
            reference.set_fragment(Some(&format!("L{line}C{column}")));
        }
        let cwd = self.workspace_cwd.clone().unwrap_or(root);
        self.open_reference_inner(cwd, reference.to_string(), true, cx);
    }

    fn open_reference_inner(
        &mut self,
        cwd: PathBuf,
        reference: String,
        record_history: bool,
        cx: &mut Context<Self>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if self.editor.read(cx).document().is_none() {
            self.state = ViewerState::Loading {
                reference: reference.clone(),
            };
        }
        cx.notify();
        let intelligence = self.intelligence.clone();
        let history_cwd = cwd.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let task_reference = reference.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let intelligence = match intelligence {
                        Some(intelligence) => intelligence,
                        None => Arc::new(CodeIntelligence::for_session(&cwd)?),
                    };
                    let snapshot = intelligence.open_reference(&task_reference)?;
                    Ok::<_, CodeIntelligenceError>((intelligence, snapshot))
                })
                .await
                .map_err(|error| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((intelligence, snapshot)) => {
                        this.adopt_intelligence(intelligence, cx);
                        if record_history {
                            if this.history_index + 1 < this.history.len() {
                                this.history.truncate(this.history_index + 1);
                            }
                            let should_push =
                                this.history.last().is_none_or(|(current, current_ref)| {
                                    current != &history_cwd || current_ref != &reference
                                });
                            if should_push {
                                this.history.push((history_cwd, reference));
                                this.history_index = this.history.len().saturating_sub(1);
                            }
                        }
                        this.show_snapshot(snapshot, cx);
                    }
                    Err(message) => {
                        this.state = ViewerState::Error { reference, message };
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Puts a loaded file in the editor. The open file keeps its unsaved
    /// edits when opened again; a clean one takes the fresh text from disk;
    /// another file's draft is restored; the file left behind becomes a
    /// draft when it has unsaved edits.
    fn show_snapshot(&mut self, snapshot: SourceSnapshot, cx: &mut Context<Self>) {
        let path = snapshot.relative_path.clone();
        let target = snapshot.target;
        let current = self
            .editor
            .read(cx)
            .document()
            .map(|doc| (doc.absolute_path.clone(), doc.is_dirty()));
        let same_dirty = matches!(&current, Some((open, true)) if *open == snapshot.absolute_path);
        if !same_dirty {
            let doc = self
                .drafts
                .remove(&snapshot.absolute_path)
                .unwrap_or_else(|| Document::from_snapshot(&snapshot));
            let previous = self
                .editor
                .update(cx, |editor, cx| editor.set_document(Some(doc), cx));
            if let Some(previous) = previous
                && previous.is_dirty()
                && previous.absolute_path != snapshot.absolute_path
            {
                self.drafts.insert(previous.absolute_path.clone(), previous);
            }
        }
        if let Some(target) = target {
            self.editor.update(cx, |editor, cx| {
                editor.go_to(target.line, target.column, cx)
            });
        }
        self.state = ViewerState::Ready;
        self.open_file = Some(path.clone());
        for missing in self.tree.reveal(&path) {
            self.load_directory(missing, cx);
        }
        if let Some(index) = self.tree.rows().iter().position(|row| row.path == path) {
            self.tree_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        self.sync_dirty(cx);
        self.watch(snapshot.absolute_path.clone(), cx);
        cx.notify();
    }

    /// Marks the tree's files with unsaved edits: drafts in this workspace
    /// and the open file.
    fn sync_dirty(&mut self, cx: &App) {
        let root = self
            .intelligence
            .as_ref()
            .map(|intelligence| intelligence.workspace_root().to_path_buf());
        let mut dirty: HashSet<PathBuf> = self
            .drafts
            .values()
            .filter(|doc| doc.is_dirty())
            .filter(|doc| {
                root.as_ref()
                    .is_some_and(|root| doc.absolute_path.starts_with(root))
            })
            .map(|doc| doc.relative_path.clone())
            .collect();
        let editor = self.editor.read(cx);
        if editor.is_dirty()
            && let Some(path) = editor.relative_path()
        {
            dirty.insert(path.to_path_buf());
        }
        self.tree.set_dirty(dirty);
    }

    fn on_editor_event(
        &mut self,
        editor: Entity<CodeEditor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::DirtyChanged | EditorEvent::Saved => {
                self.sync_dirty(cx);
                if *event == EditorEvent::Saved {
                    self.refresh_git(cx);
                }
            }
            EditorEvent::SavedAway(saved) => {
                // The save finished after its file was switched away: settle
                // it on that file's draft, never on the file now shown.
                if let Some(draft) = self.drafts.get_mut(&saved.absolute_path)
                    && draft.apply_saved_away(saved)
                    && !draft.is_dirty()
                {
                    self.drafts.remove(&saved.absolute_path);
                }
                self.sync_dirty(cx);
                self.refresh_git(cx);
            }
            EditorEvent::OpenLocation { path, line } => {
                self.open_path(path.clone(), Some((*line, 1)), cx)
            }
            EditorEvent::SearchWorkspace(text) => {
                self.show_sidebar();
                self.sidebar = Sidebar::Search;
                self.field = Field::Query;
                self.search.query.clear();
                self.search.query.insert(text);
                self.search.options.word = true;
                self.schedule_search(true, cx);
            }
            EditorEvent::Reload => {
                let path = editor.read(cx).relative_path().map(Path::to_path_buf);
                if let Some(path) = path {
                    let line = editor.read(cx).position();
                    editor.update(cx, |editor, cx| {
                        editor.set_document(None, cx);
                    });
                    self.open_path(path, Some(line), cx);
                }
            }
        }
        cx.notify();
    }

    fn navigate(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let next = self
            .history_index
            .saturating_add_signed(delta)
            .min(self.history.len() - 1);
        if next == self.history_index {
            return;
        }
        self.history_index = next;
        let (cwd, reference) = self.history[next].clone();
        self.open_reference_inner(cwd, reference, false, cx);
    }

    // ---- watching the open file ----------------------------------------------

    fn unwatch(&mut self) {
        self._watcher = None;
        self._watch_task = None;
        self.watched = None;
    }

    /// Watches the open file's folder (writes often land by rename) so an
    /// outside change reloads a clean file and flags a dirty one.
    fn watch(&mut self, absolute: PathBuf, cx: &mut Context<Self>) {
        if !self.watch_files || self.watched.as_ref() == Some(&absolute) {
            return;
        }
        use notify::{EventKind, RecursiveMode, Watcher};
        let Some(folder) = absolute.parent().map(Path::to_path_buf) else {
            return;
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let file = absolute.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                return;
            };
            if matches!(event.kind, EventKind::Access(_)) || !event.paths.contains(&file) {
                return;
            }
            let _ = tx.send(());
        })
        .ok()
        .and_then(|mut watcher| {
            watcher
                .watch(&folder, RecursiveMode::NonRecursive)
                .ok()
                .map(|()| watcher)
        });
        self._watcher = watcher;
        self.watched = Some(absolute);
        self._watch_task = Some(cx.spawn(async move |this, cx| {
            while rx.recv().await.is_some() {
                cx.background_executor().timer(WATCH_DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                if this
                    .update(cx, |this, cx| this.on_outside_write(cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn on_outside_write(&mut self, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let Some(doc) = editor.document() else {
            return;
        };
        let modified = std::fs::metadata(&doc.absolute_path)
            .and_then(|metadata| metadata.modified())
            .ok();
        if modified.is_none() || modified == doc.modified {
            return;
        }
        if doc.is_dirty() {
            self.editor.update(cx, |editor, cx| {
                editor.conflict = true;
                cx.notify();
            });
        } else {
            self.reload_open_file_if_clean(cx);
        }
        self.refresh_git(cx);
    }

    /// Takes the open file's current text from disk when it has no unsaved
    /// edits, keeping the caret's line and the scroll position.
    fn reload_open_file_if_clean(&mut self, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let Some(doc) = editor.document() else {
            return;
        };
        if doc.is_dirty() {
            return;
        }
        let relative = doc.relative_path.clone();
        let (line, column) = editor.position();
        let Some(work) =
            self.with_intelligence(move |intelligence| intelligence.open_file(&relative, None))
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let Ok((_, snapshot)) = cx.background_executor().spawn(async move { work() }).await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.editor.update(cx, |editor, cx| {
                    let Some(current) = editor.document() else {
                        return;
                    };
                    if current.relative_path != snapshot.relative_path
                        || current.is_dirty()
                        || current.text() == snapshot.text
                    {
                        if let Some(doc) = editor.doc.as_mut() {
                            doc.modified = snapshot.modified;
                        }
                        return;
                    }
                    let scroll = editor.top_row();
                    let mut doc = Document::from_snapshot(&snapshot);
                    doc.scroll_row = scroll;
                    editor.set_document(Some(doc), cx);
                    editor.go_to(line, column, cx);
                    editor.scroll.scroll_to_item(scroll, ScrollStrategy::Top);
                });
            });
        })
        .detach();
    }

    // ---- quick open ----------------------------------------------------------

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        if self.picker_open {
            window.focus(&self.focus, cx);
            self.schedule_picker(cx);
        } else {
            self._picker_task = None;
            self.query.clear();
            self.results.clear();
            self.highlighted_result = 0;
        }
        cx.notify();
    }

    fn schedule_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_generation = self.picker_generation.wrapping_add(1);
        let generation = self.picker_generation;
        let query = self.query.text().to_owned();
        let Some(work) =
            self.with_intelligence(move |intelligence| Ok(intelligence.search(&query, 201)))
        else {
            self.results.clear();
            self.picker_pending = false;
            return;
        };
        self.picker_pending = true;
        self.picker_error = None;
        self.results.clear();
        self.highlighted_result = 0;
        self._picker_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(90))
                .await;
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.picker_generation != generation || !this.picker_open {
                    return;
                }
                this.picker_pending = false;
                match result {
                    Ok((intelligence, results)) => {
                        this.adopt_intelligence(intelligence, cx);
                        this.results = results;
                    }
                    Err(error) => this.picker_error = Some(error),
                }
                cx.notify();
            });
        }));
    }

    fn open_highlighted(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.results.get(self.highlighted_result).cloned() else {
            return;
        };
        self.picker_open = false;
        self.query.clear();
        self.results.clear();
        self.open_path(hit.relative_path, hit.line.map(|line| (line, 1)), cx);
    }

    // ---- keys ------------------------------------------------------------------

    /// Applies a query-field edit to `field`; answers whether its text changed.
    fn edit_field(
        field: &mut QueryEditor,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let edit = query_editor::edit_for(&event.keystroke)?;
        Some(match edit {
            Edit::Local(local) => field.apply(local),
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(field, cx);
                false
            }
            Edit::Clipboard(ClipboardEdit::Cut) => query_editor::cut_selection(field, cx),
            Edit::Clipboard(ClipboardEdit::Paste) => cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| field.insert(&text.replace('\n', " "))),
        })
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keys the editor (or its find bar) left unhandled bubble here; only
        // keys aimed at the surface itself are ours.
        if !self.focus.is_focused(window) {
            return;
        }
        let keystroke = &event.keystroke;
        let key = keystroke.key.as_str();
        if keystroke.modifiers.platform && key == "f" && !keystroke.modifiers.shift {
            if self.editor.read(cx).document().is_some() && self.field == Field::Tree {
                let editor = self.editor.clone();
                editor.update(cx, |editor, cx| editor.open_find(false, window, cx));
            } else {
                self.open_search(None, false, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.picker_open {
            match key {
                "escape" => {
                    self.picker_open = false;
                    self.query.clear();
                    self.results.clear();
                }
                "up" => {
                    self.highlighted_result = self.highlighted_result.saturating_sub(1);
                    self.result_scroll.scroll_to_item(self.highlighted_result);
                }
                "down" => {
                    self.highlighted_result = (self.highlighted_result + 1)
                        .min(self.results.len().min(200).saturating_sub(1));
                    self.result_scroll.scroll_to_item(self.highlighted_result);
                }
                "enter" => self.open_highlighted(cx),
                _ => {
                    if Self::edit_field(&mut self.query, event, cx) == Some(true) {
                        self.schedule_picker(cx);
                    }
                }
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let handled = match self.field {
            Field::Tree => {
                if key == "escape" && self.tree.filter().is_some() {
                    self.filter.clear();
                    self.schedule_filter(cx);
                    true
                } else if self.tree_key(key, cx) {
                    true
                } else if keystroke.key_char.as_ref().is_some_and(|ch| {
                    ch.chars()
                        .all(|ch| ch.is_alphanumeric() || "._-/".contains(ch))
                }) && !keystroke.modifiers.platform
                    && !keystroke.modifiers.control
                {
                    // Typing in the tree starts filtering it.
                    self.field = Field::Filter;
                    self.filter.clear();
                    Self::edit_field(&mut self.filter, event, cx);
                    self.schedule_filter(cx);
                    true
                } else {
                    false
                }
            }
            Field::Filter => match key {
                "escape" => {
                    self.filter.clear();
                    self.field = Field::Tree;
                    self.schedule_filter(cx);
                    true
                }
                "down" | "enter" | "up" => {
                    self.field = Field::Tree;
                    let rows = self.tree.rows();
                    if self
                        .tree
                        .selected()
                        .is_none_or(|selected| !rows.iter().any(|row| row.path == selected))
                    {
                        let first = rows
                            .iter()
                            .find(|row| !row.is_dir)
                            .or(rows.first())
                            .map(|row| row.path.clone());
                        self.tree.select(first);
                    }
                    if key == "enter" {
                        self.tree_key("enter", cx);
                    }
                    true
                }
                _ => {
                    if Self::edit_field(&mut self.filter, event, cx) == Some(true) {
                        self.schedule_filter(cx);
                    }
                    true
                }
            },
            Field::Query | Field::Include | Field::Exclude => match key {
                "escape" => {
                    self.field = Field::Tree;
                    self.sidebar = Sidebar::Files;
                    true
                }
                "enter" => {
                    self.schedule_search(true, cx);
                    true
                }
                "tab" => {
                    self.field = match (self.field, self.search.show_filters) {
                        (Field::Query, true) => Field::Include,
                        (Field::Include, _) => Field::Exclude,
                        _ => Field::Query,
                    };
                    true
                }
                "c" if keystroke.modifiers.alt => {
                    self.search.options.case = !self.search.options.case;
                    self.schedule_search(true, cx);
                    true
                }
                "w" if keystroke.modifiers.alt => {
                    self.search.options.word = !self.search.options.word;
                    self.schedule_search(true, cx);
                    true
                }
                "r" if keystroke.modifiers.alt => {
                    self.search.options.regex = !self.search.options.regex;
                    self.schedule_search(true, cx);
                    true
                }
                _ => {
                    let field = match self.field {
                        Field::Include => &mut self.search.include,
                        Field::Exclude => &mut self.search.exclude,
                        _ => &mut self.search.query,
                    };
                    if Self::edit_field(field, event, cx) == Some(true) {
                        self.schedule_search(false, cx);
                    }
                    true
                }
            },
        };
        if handled {
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn focus_field(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        self.field = field;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    // ---- rendering ---------------------------------------------------------------

    fn docked(&self) -> bool {
        self.width >= px(DOCKED_MIN_WIDTH)
    }

    /// Docked, the sidebar shows until hidden. As a drawer it shows when
    /// asked for, or while there is no file to look at instead.
    fn sidebar_shown(&self, cx: &App) -> bool {
        if self.docked() {
            self.sidebar_visible
        } else {
            self.drawer_open
                || (self.workspace_cwd.is_some() && self.editor.read(cx).document().is_none())
        }
    }

    fn show_sidebar(&mut self) {
        self.sidebar_visible = true;
        self.drawer_open = true;
    }

    fn sidebar_width(&self) -> Pixels {
        if self.docked() {
            (self.width * 0.32).clamp(px(200.0), px(300.0))
        } else {
            (self.width * 0.86).min(px(300.0)).max(px(180.0))
        }
    }

    fn icon_button(
        &self,
        id: &'static str,
        symbol: &'static str,
        active: bool,
        tooltip: &'static str,
        cx: &mut Context<Self>,
        action: fn(&mut CodeViewer, &mut Window, &mut Context<CodeViewer>),
    ) -> AnyElement {
        let colors = self.colors;
        div()
            .id(id)
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::BADGE))
            .bg(colors.primary.alpha(if active { 0.09 } else { 0.0 }))
            .cursor_pointer()
            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
            .child(sf_symbol(
                symbol,
                11.0,
                if active {
                    colors.primary
                } else {
                    colors.secondary
                },
            ))
            .warm_tooltip(move |_, cx| {
                cx.new(|_| ExplorerTooltip(tooltip.to_owned(), colors))
                    .into()
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors;
        let can_back = !self.history.is_empty() && self.history_index > 0;
        let can_forward = self.history_index + 1 < self.history.len();
        let editor = self.editor.read(cx);
        let dirty = editor.is_dirty();
        let has_doc = editor.document().is_some();
        let read_only = editor.document().is_some_and(|doc| doc.read_only);
        let position = has_doc.then(|| editor.position());
        let nav_button = |id: &'static str,
                          symbol: &'static str,
                          enabled: bool,
                          delta: isize,
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::BADGE))
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate(delta, cx);
                            cx.stop_propagation();
                        }))
                })
                .child(sf_symbol_weighted(
                    symbol,
                    10.0,
                    SymbolWeight::Semibold,
                    if enabled {
                        colors.secondary
                    } else {
                        colors.primary.alpha(0.20)
                    },
                ))
        };
        div()
            .h(px(36.0))
            .flex_none()
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.06))
            .child(self.icon_button(
                "toggle-file-sidebar",
                "sidebar.left",
                self.sidebar_shown(cx),
                "Toggle sidebar",
                cx,
                |this, _, cx| {
                    if this.docked() {
                        this.sidebar_visible = !this.sidebar_visible;
                    } else {
                        this.drawer_open = !this.sidebar_shown(cx);
                    }
                    cx.notify();
                },
            ))
            .child(nav_button(
                "code-history-back",
                "chevron.left",
                can_back,
                -1,
                cx,
            ))
            .child(nav_button(
                "code-history-forward",
                "chevron.right",
                can_forward,
                1,
                cx,
            ))
            .child(self.render_breadcrumb(cx))
            .when(read_only, |bar| {
                bar.child(
                    div()
                        .px(px(6.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .rounded(px(Radius::CHIP))
                        .bg(colors.primary.alpha(0.055))
                        .text_size(px(9.5))
                        .text_color(colors.tertiary)
                        .child(sf_symbol("lock", 9.0, colors.tertiary))
                        .child("Read-only"),
                )
            })
            .when(dirty, |bar| {
                bar.child(
                    div()
                        .id("save-file")
                        .h(px(20.0))
                        .px(px(7.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .rounded(px(Radius::CHIP))
                        .bg(colors.primary.alpha(0.07))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.12)))
                        .text_size(px(10.0))
                        .text_color(colors.secondary)
                        .child(
                            div()
                                .size(px(6.0))
                                .rounded_full()
                                .bg(colors.primary.alpha(0.75)),
                        )
                        .child("Save ⌘S")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.editor.update(cx, |editor, cx| editor.save(false, cx));
                        })),
                )
            })
            .when_some(position, |bar, (line, column)| {
                bar.child(
                    div()
                        .px(px(6.0))
                        .h(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(9.5))
                        .text_color(colors.tertiary)
                        .child(format!("{line}:{column}")),
                )
            })
            .child(self.icon_button(
                "code-open-file-picker",
                "magnifyingglass",
                self.picker_open,
                "Go to file or symbol  ⌘P",
                cx,
                |this, window, cx| this.toggle_picker(window, cx),
            ))
            .into_any_element()
    }

    /// The open file's path, then the blocks around the caret.
    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors;
        let editor = self.editor.read(cx);
        let Some(doc) = editor.document() else {
            return div()
                .flex_1()
                .min_w(px(0.0))
                .px(px(6.0))
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(if self.workspace_cwd.is_some() {
                    "No file open"
                } else {
                    "No workspace"
                })
                .into_any_element();
        };
        let line = editor.position().0.saturating_sub(1);
        let crumbs =
            crate::code_editor::intel::breadcrumb(&doc.buffer, editor.folds(), doc.language, line);
        let segments: Vec<String> = doc
            .relative_path
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        let separator = || {
            div().flex_none().px(px(1.0)).child(sf_symbol(
                "chevron.right",
                7.0,
                colors.primary.alpha(0.25),
            ))
        };
        let mut row = div()
            .id("code-breadcrumb")
            .flex_1()
            .min_w(px(0.0))
            .ml(px(4.0))
            .flex()
            .items_center()
            .gap(px(3.0))
            .overflow_x_scroll()
            .font_family(crate::fonts::ui_family())
            .text_size(px(11.0))
            .whitespace_nowrap();
        let last = segments.len().saturating_sub(1);
        for (ix, segment) in segments.into_iter().enumerate() {
            if ix > 0 {
                row = row.child(separator());
            }
            let file = ix == last;
            // Leading folders collapse to an ellipsis in a narrow panel.
            if !file && ix + 1 < last && self.width < px(520.0) {
                if ix == 0 {
                    row = row.child(div().text_color(colors.tertiary).child("…"));
                }
                continue;
            }
            row = row.child(
                div()
                    .id(("crumb-path", ix))
                    .flex_none()
                    .px(px(2.0))
                    .rounded(px(Radius::CHIP))
                    .text_color(if file {
                        colors.primary
                    } else {
                        colors.secondary
                    })
                    .when(file, |crumb| crumb.font_weight(FontWeight::MEDIUM))
                    .cursor_pointer()
                    .hover(move |crumb| crumb.bg(colors.primary.alpha(0.06)))
                    .child(segment)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_sidebar();
                        this.sidebar = Sidebar::Files;
                        this.reveal_active(cx);
                    })),
            );
        }
        for (ix, crumb) in crumbs.into_iter().enumerate() {
            let target = crumb.line + 1;
            row = row.child(separator()).child(
                div()
                    .id(("crumb-symbol", ix))
                    .flex_none()
                    .px(px(2.0))
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .rounded(px(Radius::CHIP))
                    .text_color(colors.secondary)
                    .cursor_pointer()
                    .hover(move |crumb| crumb.bg(colors.primary.alpha(0.06)))
                    .child(sf_symbol("curlybraces", 9.0, colors.tertiary))
                    .child(crumb.label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let editor = this.editor.clone();
                        editor.update(cx, |editor, cx| {
                            editor.go_to(target, 1, cx);
                            window.focus(&editor.focus_handle(cx), cx);
                        });
                    })),
            );
        }
        row.into_any_element()
    }

    fn field_box(
        &self,
        id: &'static str,
        field: Field,
        editor: &QueryEditor,
        placeholder: &'static str,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = self.colors;
        let active = focused && self.field == field;
        let content = if editor.is_empty() && !active {
            div()
                .text_color(colors.tertiary)
                .child(placeholder)
                .into_any_element()
        } else if active {
            crate::navigation::query_label(editor)
        } else {
            div().child(editor.text().to_owned()).into_any_element()
        };
        div()
            .id(id)
            .h(px(24.0))
            .min_w(px(0.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .rounded(px(Radius::CHIP))
            .bg(colors.primary.alpha(0.045))
            .border_1()
            .border_color(colors.primary.alpha(if active { 0.22 } else { 0.07 }))
            .font_family(crate::fonts::mono_family())
            .text_size(px(11.0))
            .text_color(colors.primary)
            .cursor_text()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(content),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.focus_field(field, window, cx);
                    cx.stop_propagation();
                }),
            )
    }

    fn render_sidebar(&self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors;
        let tab =
            |id: &'static str, label: &'static str, sidebar: Sidebar, cx: &mut Context<Self>| {
                let active = self.sidebar == sidebar;
                div()
                    .id(id)
                    .h(px(22.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::CHIP))
                    .text_size(px(10.5))
                    .font_weight(if active {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if active {
                        colors.primary
                    } else {
                        colors.tertiary
                    })
                    .bg(colors.primary.alpha(if active { 0.08 } else { 0.0 }))
                    .cursor_pointer()
                    .hover(move |tab| tab.text_color(colors.primary))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.sidebar = sidebar;
                        this.focus_field(
                            if sidebar == Sidebar::Search {
                                Field::Query
                            } else {
                                Field::Tree
                            },
                            window,
                            cx,
                        );
                    }))
            };
        let header = div()
            .h(px(32.0))
            .flex_none()
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(tab("sidebar-files", "Files", Sidebar::Files, cx))
            .child(tab("sidebar-search", "Search", Sidebar::Search, cx))
            .child(div().flex_1())
            .when(self.sidebar == Sidebar::Files, |header| {
                header
                    .child(self.icon_button(
                        "reveal-active-file",
                        "arrow.turn.down.right",
                        false,
                        "Reveal open file",
                        cx,
                        |this, _, cx| this.reveal_active(cx),
                    ))
                    .child(self.icon_button(
                        "toggle-ignored",
                        "archivebox",
                        self.tree.show_ignored(),
                        "Show ignored files",
                        cx,
                        |this, _, cx| {
                            let show = !this.tree.show_ignored();
                            this.tree.set_show_ignored(show);
                            cx.notify();
                        },
                    ))
                    .child(self.icon_button(
                        "collapse-file-tree",
                        "arrow.down.right.and.arrow.up.left",
                        false,
                        "Collapse folders",
                        cx,
                        |this, _, cx| {
                            this.tree.collapse_all();
                            cx.notify();
                        },
                    ))
                    .child(self.icon_button(
                        "refresh-file-tree",
                        "arrow.clockwise.circle",
                        false,
                        "Refresh",
                        cx,
                        |this, _, cx| this.refresh(cx),
                    ))
            });
        let body = match self.sidebar {
            Sidebar::Files => self.render_tree(focused, cx),
            Sidebar::Search => self.render_search(focused, cx),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_tree(&self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors;
        let rows = self.tree.rows();
        let filter = div().px(px(6.0)).pb(px(5.0)).flex_none().child(
            self.field_box(
                "tree-filter",
                Field::Filter,
                &self.filter,
                "Filter files…",
                focused,
                cx,
            )
            .when(!self.filter.is_empty(), |field| {
                field.child(
                    div()
                        .id("clear-tree-filter")
                        .cursor_pointer()
                        .child(sf_symbol("xmark.circle.fill", 10.0, colors.tertiary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.filter.clear();
                            this.field = Field::Tree;
                            this.schedule_filter(cx);
                            cx.stop_propagation();
                        })),
                )
            }),
        );
        let mut tree = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(filter);
        if rows.is_empty() {
            let message = if self.workspace_cwd.is_none() {
                "Select a local session to browse its files.".to_owned()
            } else if self.tree.filter().is_some() {
                "No files match.".to_owned()
            } else if self.tree.is_loading(Path::new("")) || !self.tree.has_loaded_anything() {
                "Loading files…".to_owned()
            } else if let Some(error) = self.tree.root_error() {
                error.to_owned()
            } else {
                "This folder is empty.".to_owned()
            };
            return tree
                .child(
                    div()
                        .p(px(12.0))
                        .text_size(px(11.0))
                        .text_color(colors.tertiary)
                        .child(message),
                )
                .into_any_element();
        }
        let viewer = cx.entity().downgrade();
        let tree_focused = focused && self.field == Field::Tree;
        let selected = self.tree.selected().map(Path::to_path_buf);
        let palette = self.palette();
        tree = tree.child(
            uniform_list("workspace-file-tree", rows.len(), move |range, _, cx| {
                viewer
                    .update(cx, |this, cx| {
                        range
                            .map(|index| {
                                this.tree_row(
                                    &rows[index],
                                    index,
                                    selected.as_deref(),
                                    tree_focused,
                                    palette,
                                    cx,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .track_scroll(&self.tree_scroll)
            .flex_1()
            .min_h(px(0.0)),
        );
        tree.into_any_element()
    }

    fn palette(&self) -> EditorPalette {
        EditorPalette::from_theme(&self.theme.unwrap_or_else(|| fallback_theme(self.colors)))
    }

    fn tree_row(
        &self,
        row: &TreeRow,
        index: usize,
        selected: Option<&Path>,
        tree_focused: bool,
        palette: EditorPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors;
        let is_selected = selected == Some(row.path.as_path());
        let open = self.editor.read(cx).relative_path() == Some(row.path.as_path());
        let dirty = self.tree.is_dirty(&row.path);
        let status_color = row.status.map(|status| match status {
            crate::file_tree::GitStatus::Added | crate::file_tree::GitStatus::Untracked => {
                palette.added
            }
            crate::file_tree::GitStatus::Modified | crate::file_tree::GitStatus::Renamed => {
                palette.modified
            }
            crate::file_tree::GitStatus::Deleted | crate::file_tree::GitStatus::Conflicted => {
                palette.deleted
            }
        });
        let name_color = if row.ignored {
            colors.tertiary
        } else if let Some(color) = status_color {
            color.alpha(0.95)
        } else {
            colors.primary.alpha(0.88)
        };
        let tooltip = row.error.clone().unwrap_or_else(|| match row.status {
            Some(status) => format!("{} · {}", row.path.display(), status.word()),
            None => row.path.to_string_lossy().into_owned(),
        });
        let label = if row.matched.is_empty() {
            div().child(row.name.clone()).into_any_element()
        } else {
            crate::navigation::highlighted_label(row.name.clone(), &row.matched)
        };
        let indent = 12.0;
        let entry = row.clone();
        let guides = (0..row.depth).map(move |level| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(10.0 + level as f32 * indent + 3.5))
                .w(px(1.0))
                .bg(colors.primary.alpha(0.06))
        });
        div()
            .id(("file-tree-row", index))
            .relative()
            .h(px(TREE_ROW_HEIGHT))
            .mx(px(4.0))
            .pl(px(6.0 + row.depth as f32 * indent))
            .pr(px(6.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .overflow_hidden()
            .rounded(px(Radius::CHIP))
            .bg(if is_selected {
                colors.primary.alpha(if tree_focused { 0.12 } else { 0.08 })
            } else if open {
                colors.primary.alpha(0.04)
            } else {
                colors.primary.alpha(0.0)
            })
            .cursor_pointer()
            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
            .children(guides)
            .child(
                div()
                    .w(px(9.0))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .when(row.is_dir, |icon| {
                        icon.child(sf_symbol(
                            if row.loading {
                                "ellipsis"
                            } else if row.expanded {
                                "chevron.down"
                            } else {
                                "chevron.right"
                            },
                            8.0,
                            colors.tertiary,
                        ))
                    }),
            )
            .child(sf_symbol(
                if row.is_dir { "folder" } else { "doc.text" },
                11.0,
                if row.is_dir {
                    palette.function.alpha(0.85)
                } else {
                    colors.secondary
                },
            ))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(name_color)
                    .when(open, |name| name.font_weight(FontWeight::MEDIUM))
                    .child(label),
            )
            .when(dirty, |row| {
                row.child(
                    div()
                        .flex_none()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(colors.primary.alpha(0.7)),
                )
            })
            .when_some(row.error.as_ref(), |row, _| {
                row.child(sf_symbol("exclamationmark.triangle", 9.0, palette.warning))
            })
            .when_some(row.status.zip(status_color), |element, (status, color)| {
                element.child(if row.is_dir {
                    div()
                        .flex_none()
                        .size(px(5.0))
                        .rounded_full()
                        .bg(color.alpha(0.85))
                        .into_any_element()
                } else {
                    div()
                        .flex_none()
                        .w(px(10.0))
                        .text_center()
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(9.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(status.letter())
                        .into_any_element()
                })
            })
            .warm_tooltip(move |_, cx| cx.new(|_| ExplorerTooltip(tooltip.clone(), colors)).into())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.field = Field::Tree;
                window.focus(&this.focus, cx);
                this.activate_tree_row(&entry, cx);
                if !entry.is_dir && !this.docked() {
                    // A drawer gets out of the way once a file is chosen.
                    this.drawer_open = false;
                }
            }))
            .into_any_element()
    }

    fn render_search(&self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors;
        let palette = self.palette();
        let options = self.search.options;
        let toggle = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      flip: fn(&mut FindOptions),
                      cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(20.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::CHIP))
                .font_family(crate::fonts::mono_family())
                .text_size(px(10.0))
                .text_color(if on {
                    palette.function
                } else {
                    colors.tertiary
                })
                .bg(if on {
                    palette.function.alpha(0.16)
                } else {
                    colors.primary.alpha(0.0)
                })
                .cursor_pointer()
                .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    flip(&mut this.search.options);
                    this.schedule_search(true, cx);
                    cx.stop_propagation();
                }))
        };
        let query = self
            .field_box(
                "search-query",
                Field::Query,
                &self.search.query,
                "Search",
                focused,
                cx,
            )
            .flex_1()
            .child(toggle(
                "search-case",
                "Aa",
                options.case,
                |options| options.case = !options.case,
                cx,
            ))
            .child(toggle(
                "search-word",
                "ab",
                options.word,
                |options| options.word = !options.word,
                cx,
            ))
            .child(toggle(
                "search-regex",
                ".*",
                options.regex,
                |options| options.regex = !options.regex,
                cx,
            ));
        let show_filters = self.search.show_filters;
        let mut controls =
            div()
                .px(px(6.0))
                .pb(px(5.0))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(div().flex().items_center().gap(px(3.0)).child(query).child(
                    self.icon_button(
                        "toggle-search-filters",
                        "ellipsis",
                        show_filters,
                        "Files to include and exclude",
                        cx,
                        |this, _, cx| {
                            this.search.show_filters = !this.search.show_filters;
                            cx.notify();
                        },
                    ),
                ));
        if show_filters {
            controls = controls
                .child(self.field_box(
                    "search-include",
                    Field::Include,
                    &self.search.include,
                    "Include, e.g. *.rs, src/**",
                    focused,
                    cx,
                ))
                .child(self.field_box(
                    "search-exclude",
                    Field::Exclude,
                    &self.search.exclude,
                    "Exclude, e.g. tests, *.md",
                    focused,
                    cx,
                ));
        }
        let outcome = &self.search.outcome;
        let summary = if self.search.pending {
            "Searching…".to_owned()
        } else if let Some(error) = &self.search.error {
            error.clone()
        } else if self.search.query.is_empty() {
            "Search across the workspace. ⇧⌘F".to_owned()
        } else if outcome.total == 0 {
            "No results".to_owned()
        } else {
            format!(
                "{}{} result{} in {} file{}",
                outcome.total,
                if outcome.truncated { "+" } else { "" },
                if outcome.total == 1 { "" } else { "s" },
                outcome.files.len(),
                if outcome.files.len() == 1 { "" } else { "s" }
            )
        };
        let error = self.search.error.is_some();
        controls = controls.child(
            div()
                .px(px(2.0))
                .text_size(px(10.0))
                .text_color(if error {
                    palette.deleted
                } else {
                    colors.tertiary
                })
                .child(summary),
        );
        let rows = self.search.rows.clone();
        let viewer = cx.entity().downgrade();
        let selected = self.search.selected;
        let list = (!rows.is_empty()).then(|| {
            uniform_list(
                "workspace-search-results",
                rows.len(),
                move |range, _, cx| {
                    viewer
                        .update(cx, |this, cx| {
                            range
                                .map(|index| {
                                    this.search_row(
                                        rows[index],
                                        index,
                                        selected == Some(index),
                                        palette,
                                        cx,
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                },
            )
            .track_scroll(&self.search_scroll)
            .flex_1()
            .min_h(px(0.0))
        });
        div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(controls)
            .children(list)
            .into_any_element()
    }

    fn search_row(
        &self,
        row: ResultRow,
        index: usize,
        selected: bool,
        palette: EditorPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors;
        let base = div()
            .id(("search-row", index))
            .h(px(SEARCH_ROW_HEIGHT))
            .mx(px(4.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .overflow_hidden()
            .rounded(px(Radius::CHIP))
            .bg(colors.primary.alpha(if selected { 0.10 } else { 0.0 }))
            .cursor_pointer()
            .hover(move |row| row.bg(colors.primary.alpha(0.06)));
        match row {
            ResultRow::File(file) => {
                let Some(found) = self.search.outcome.files.get(file) else {
                    return base.into_any_element();
                };
                let path = found.relative_path.clone();
                let collapsed = self.search.collapsed.contains(&path);
                let (name, folder) = workspace_search::split_path(&found.relative_path);
                base.child(sf_symbol(
                    if collapsed {
                        "chevron.right"
                    } else {
                        "chevron.down"
                    },
                    8.0,
                    colors.tertiary,
                ))
                .child(sf_symbol("doc.text", 10.5, colors.secondary))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.primary)
                        .child(name),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(colors.tertiary)
                        .child(folder),
                )
                .child(
                    div()
                        .flex_none()
                        .px(px(5.0))
                        .rounded_full()
                        .bg(colors.primary.alpha(0.07))
                        .text_size(px(9.5))
                        .text_color(colors.secondary)
                        .child(found.hits.len().to_string()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.search.collapsed.remove(&path) {
                        this.search.collapsed.insert(path.clone());
                    }
                    this.search.rows =
                        workspace_search::result_rows(&this.search.outcome, &this.search.collapsed);
                    this.search.selected = None;
                    cx.notify();
                }))
                .into_any_element()
            }
            ResultRow::Hit(file, hit) => {
                let Some(line) = self
                    .search
                    .outcome
                    .files
                    .get(file)
                    .and_then(|found| found.hits.get(hit))
                else {
                    return base.into_any_element();
                };
                let styles: Vec<_> = line
                    .ranges
                    .iter()
                    .map(|range| {
                        (
                            range.clone(),
                            gpui::HighlightStyle {
                                background_color: Some(palette.find_match.into()),
                                color: Some(colors.primary.into()),
                                ..gpui::HighlightStyle::default()
                            },
                        )
                    })
                    .collect();
                base.pl(px(26.0))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(22.0))
                            .text_right()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(9.5))
                            .text_color(colors.tertiary)
                            .child(line.line.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(self.font_family.clone())
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(
                                gpui::StyledText::new(SharedString::from(line.preview.clone()))
                                    .with_highlights(styles),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.search.selected = Some(index);
                        this.open_search_hit(file, hit, cx);
                        if !this.docked() {
                            this.drawer_open = false;
                        }
                    }))
                    .into_any_element()
            }
        }
    }

    /// The search popup's input and results, without host chrome.
    fn picker_content(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.picker_open {
            return None;
        }
        let query_empty = self.query.is_empty();
        let mut results = div()
            .id("code-search-results")
            .max_h(px(330.0))
            .overflow_y_scroll()
            .track_scroll(&self.result_scroll)
            .py(px(4.0));
        if self.results.is_empty() {
            results = results.child(
                div()
                    .h(px(72.0))
                    .px(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(
                        if self.workspace_cwd.is_none() {
                            "Select a local session to search its files"
                        } else if self.picker_pending {
                            "Searching…"
                        } else {
                            self.picker_error.as_deref().unwrap_or("No matches")
                        }
                        .to_owned(),
                    ),
            );
        } else {
            let keyword = self.palette().keyword;
            for (index, hit) in self.results.iter().take(200).enumerate() {
                let selected = index == self.highlighted_result;
                let path = match hit.line {
                    Some(line) => format!("{}:{line}", hit.relative_path.display()),
                    None => hit.relative_path.to_string_lossy().into_owned(),
                };
                let preview = hit.preview.clone();
                let symbol = hit.kind == SearchHitKind::Symbol;
                results = results.child(
                    div()
                        .id(("code-search-result", index))
                        .min_h(px(39.0))
                        .px(px(9.0))
                        .py(px(5.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(Radius::ROW))
                        .bg(if selected {
                            colors.primary.alpha(0.085)
                        } else {
                            colors.primary.alpha(0.0)
                        })
                        .cursor_pointer()
                        .hover(move |row| row.bg(colors.primary.alpha(0.07)))
                        .child(sf_symbol(
                            if symbol { "curlybraces" } else { "doc.text" },
                            11.5,
                            if symbol { keyword } else { colors.secondary },
                        ))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .child(
                                    div()
                                        .truncate()
                                        .font_family(crate::fonts::mono_family())
                                        .text_size(px(10.5))
                                        .font_weight(if symbol {
                                            FontWeight::MEDIUM
                                        } else {
                                            FontWeight::NORMAL
                                        })
                                        .text_color(colors.primary)
                                        .child(preview),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .font_family(crate::fonts::mono_family())
                                        .text_size(px(9.0))
                                        .text_color(colors.tertiary)
                                        .child(path),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.highlighted_result = index;
                            this.open_highlighted(cx);
                            cx.stop_propagation();
                        })),
                );
            }
        }
        let query = if query_empty {
            div()
                .text_color(colors.tertiary)
                .child("Go to file or symbol…  (path:line jumps)")
                .into_any_element()
        } else {
            crate::navigation::query_label(&self.query)
        };
        Some(
            div()
                .rounded(px(CODE_PICKER_RADIUS))
                .overflow_hidden()
                .child(
                    div()
                        .id("code-search-input")
                        .h(px(38.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .border_b_1()
                        .border_color(colors.primary.alpha(0.08))
                        .cursor_text()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.in_main_window(window, cx, |this, window, cx| {
                                    window.focus(&this.focus, cx);
                                });
                                cx.stop_propagation();
                            }),
                        )
                        .child(sf_symbol("magnifyingglass", 11.0, colors.tertiary))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(11.0))
                                .text_color(colors.primary)
                                .child(query),
                        ),
                )
                .child(results)
                .child(
                    div()
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_size(px(9.0))
                        .text_color(colors.tertiary)
                        .child(format!(
                            "{}{} results · ↑↓ select · ↵ open · Esc close · ⇧⌘F search text",
                            self.results.len().min(200),
                            if self.results.len() > 200 { "+" } else { "" },
                        )),
                )
                .into_any_element(),
        )
    }

    fn render_picker(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let content = self.picker_content(colors, cx)?;
        if crate::floating::uses_panels(false, colors, cx) {
            return Some(
                crate::floating::host_here(
                    CODE_PICKER,
                    crate::floating::surface_full(colors, CODE_PICKER_RADIUS, content)
                        .into_any_element(),
                    None,
                    gpui::Anchor::TopLeft,
                    8.0,
                    cx,
                )
                .absolute()
                .top(px(40.0))
                .left(px(8.0))
                .right(px(8.0))
                .h(px(0.0))
                .into_any_element(),
            );
        }
        Some(
            div()
                .absolute()
                .top(px(40.0))
                .left(px(8.0))
                .right(px(8.0))
                .occlude()
                .rounded(px(CODE_PICKER_RADIUS))
                .bg(colors.sidebar_surface().alpha(1.0))
                .child(FloatingSurface::new(colors, content).radius(CODE_PICKER_RADIUS))
                .into_any_element(),
        )
    }

    /// The search popup's pixels for its floating panel.
    fn picker_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = self.colors;
        let content = self.picker_content(colors, cx)?;
        Some(crate::floating::surface_full(colors, CODE_PICKER_RADIUS, content).into_any_element())
    }

    /// Runs `f` against the viewer's own window even from a panel handler.
    fn in_main_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        crate::floating::in_main_window(self, window, cx, f);
    }

    fn render_message(
        &self,
        colors: SemanticColors,
        symbol: &'static str,
        title: impl Into<SharedString>,
        body: impl Into<SharedString>,
    ) -> AnyElement {
        div()
            .size_full()
            .px(px(28.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .text_center()
            .child(sf_symbol(symbol, 28.0, colors.tertiary))
            .child(
                div()
                    .text_size(px(Typo::ROW_EMPHASIZED.size))
                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                    .text_color(colors.primary.alpha(0.88))
                    .child(title.into()),
            )
            .child(
                div()
                    .max_w(px(300.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(body.into()),
            )
            .into_any_element()
    }
}

/// A terminal theme matching chrome colors, for surfaces that have not been
/// told the terminal theme yet: the catalog theme with this background,
/// else the default of the same appearance.
fn fallback_theme(colors: SemanticColors) -> TermTheme {
    TermTheme::CATALOG
        .into_iter()
        .find(|theme| theme.background == colors.background && theme.foreground == colors.primary)
        .unwrap_or(match colors.appearance {
            diri_ui::Appearance::Dark => TermTheme::DIRIJOR_DARK,
            diri_ui::Appearance::Light => TermTheme::DIRIJOR_LIGHT,
        })
}

impl Focusable for CodeViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CodeViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("files");
        if self.workspace_cwd.is_some() && !self.tree.has_loaded_anything() {
            self.load_directory(PathBuf::new(), cx);
        }
        let colors = self.colors;
        let focused = self.focus.is_focused(window);
        let body = match &self.state {
            ViewerState::Ready => self.editor.clone().into_any_element(),
            ViewerState::Empty => self.render_message(
                colors,
                "cursorarrow.click.2",
                "Explore your workspace",
                "Choose a file in the sidebar, press ⌘P to go to a file or symbol, or ⇧⌘F to search text.",
            ),
            ViewerState::Loading { reference } => {
                self.render_message(colors, "ellipsis", "Opening file", format!("Resolving {reference}…"))
            }
            ViewerState::Error { reference, message } => self.render_message(
                colors,
                "exclamationmark.triangle",
                format!("Couldn’t open {reference}"),
                message.clone(),
            ),
        };
        let docked = self.docked();
        let sidebar = self.sidebar_shown(cx).then(|| {
            let width = self.sidebar_width();
            let content = self.render_sidebar(focused, cx);
            if docked {
                div()
                    .w(width)
                    .flex_none()
                    .h_full()
                    .border_r_1()
                    .border_color(colors.primary.alpha(0.07))
                    .child(content)
                    .into_any_element()
            } else {
                div()
                    .id("files-drawer")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(width)
                    .occlude()
                    .bg(colors.floating_surface())
                    .border_r_1()
                    .border_color(colors.floating_stroke())
                    .shadow(vec![gpui::BoxShadow {
                        color: gpui::hsla(
                            0.0,
                            0.0,
                            0.0,
                            if colors.appearance == diri_ui::Appearance::Light {
                                0.10
                            } else {
                                0.35
                            },
                        ),
                        offset: gpui::point(px(2.0), px(0.0)),
                        blur_radius: px(16.0),
                        spread_radius: px(0.0),
                        inset: false,
                    }])
                    .child(content)
                    .into_any_element()
            }
        });
        let (docked_sidebar, drawer) = if docked {
            (sidebar, None)
        } else {
            (None, sidebar)
        };
        let scrim = (drawer.is_some() && self.drawer_open).then(|| {
            div()
                .id("files-drawer-scrim")
                .absolute()
                .inset_0()
                .bg(gpui::hsla(0.0, 0.0, 0.0, 0.12))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.drawer_open = false;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
        });
        let picker = self.render_picker(colors, cx);
        let measure = cx.entity();
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(colors.background)
            .key_context(FILES_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_action(cx.listener(|this, _: &QuickOpen, window, cx| {
                this.picker_open = false;
                this.toggle_picker(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SearchWorkspace, window, cx| {
                let selected = {
                    let editor = this.editor.read(cx);
                    let primary = editor.primary();
                    (!primary.is_empty()).then(|| editor.text()[primary.range()].to_owned())
                };
                this.open_search(selected, false, window, cx);
            }))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        measure.update(cx, |this, cx| {
                            if this.width != bounds.size.width {
                                this.width = bounds.size.width;
                                cx.notify();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .children(docked_sidebar)
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .child(body),
                    )
                    .children(scrim)
                    .children(drawer),
            )
            .when_some(picker, |viewer, picker| viewer.child(picker))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn viewer(tokio: &tokio::runtime::Runtime, cx: &mut Context<CodeViewer>) -> CodeViewer {
        let mut viewer = CodeViewer::new(tokio.handle().clone(), SemanticColors::dark(), cx);
        viewer.watch_files = false;
        viewer
    }

    /// A workspace with a fake `.git` so it is its own root.
    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        for (path, contents) in files {
            let path = workspace.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        workspace
    }

    fn open_now(
        viewer: &mut CodeViewer,
        root: &Path,
        reference: &str,
        cx: &mut Context<CodeViewer>,
    ) {
        if viewer.workspace_cwd.as_deref() != Some(root) {
            viewer.set_workspace(Some(root.to_path_buf()), cx);
        }
        let intelligence = viewer
            .intelligence
            .clone()
            .unwrap_or_else(|| Arc::new(CodeIntelligence::for_session(root).unwrap()));
        let snapshot = intelligence.open_reference(reference).unwrap();
        viewer.intelligence = Some(intelligence);
        viewer.attach_intelligence(cx);
        viewer.show_snapshot(snapshot, cx);
    }

    /// Drives GPUI until `done` holds, letting worker threads finish.
    fn wait_until(
        cx: &mut gpui::VisualTestContext,
        mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) {
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition never held");
    }

    #[gpui::test]
    fn explorer_keyboard_navigates_and_collapses_directories(cx: &mut TestAppContext) {
        let tokio = runtime();
        let (viewer, cx) = cx.add_window_view(|window, cx| {
            let mut viewer = viewer(&tokio, cx);
            viewer.seed_explorer_preview(cx);
            viewer.field = Field::Tree;
            window.focus(&viewer.focus, cx);
            viewer
        });
        cx.simulate_resize(gpui::size(px(700.0), px(500.0)));
        cx.run_until_parked();
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(
                viewer.tree.selected(),
                Some(Path::new("diri/crates/diri-app/src/code_intelligence.rs")),
                "the open file is revealed"
            );
        });
        cx.simulate_keystrokes("left");
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(
                viewer.tree.selected(),
                Some(Path::new("diri/crates/diri-app/src"))
            )
        });
        cx.simulate_keystrokes("left");
        viewer.read_with(cx, |viewer, _| {
            assert!(
                !viewer
                    .tree
                    .is_expanded(Path::new("diri/crates/diri-app/src"))
            )
        });
        cx.simulate_keystrokes("right");
        viewer.read_with(cx, |viewer, _| {
            assert!(
                viewer
                    .tree
                    .is_expanded(Path::new("diri/crates/diri-app/src"))
            )
        });
        cx.simulate_keystrokes("down");
        viewer.read_with(cx, |viewer, _| {
            assert_ne!(
                viewer.tree.selected(),
                Some(Path::new("diri/crates/diri-app/src"))
            )
        });
    }

    #[gpui::test]
    fn unsaved_edits_survive_switching_files_as_drafts(cx: &mut TestAppContext) {
        let tokio = runtime();
        let first = workspace(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let root = first.path().canonicalize().unwrap();
        let (viewer, cx) = cx.add_window_view(|_, cx| viewer(&tokio, cx));
        cx.update(|_, cx| cx.bind_keys(key_bindings()));
        viewer.update(cx, |viewer, cx| open_now(viewer, &root, "a.rs:1", cx));
        let editor = viewer.read_with(cx, |viewer, _| viewer.editor());
        editor.update(cx, |editor, cx| editor.insert("// edited\n", false, cx));
        cx.run_until_parked();
        viewer.update(cx, |viewer, cx| open_now(viewer, &root, "b.rs", cx));
        viewer.read_with(cx, |viewer, cx| {
            assert_eq!(viewer.tab_label().as_deref(), Some("b.rs"));
            assert!(viewer.drafts.contains_key(&root.join("a.rs")));
            assert!(
                viewer.tree.is_dirty(Path::new("a.rs")),
                "the tree marks the draft"
            );
            assert!(!viewer.editor.read(cx).is_dirty());
        });
        viewer.update(cx, |viewer, cx| open_now(viewer, &root, "a.rs", cx));
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.text().to_owned()),
            "// edited\nfn a() {}\n",
            "the draft comes back, not the file on disk"
        );
        viewer.read_with(cx, |viewer, _| assert!(viewer.drafts.is_empty()));

        // Switching to another session's workspace keeps the edit too.
        let other = workspace(&[("c.rs", "")]);
        let other_root = other.path().canonicalize().unwrap();
        viewer.update(cx, |viewer, cx| open_now(viewer, &other_root, "c.rs", cx));
        viewer.read_with(cx, |viewer, _| {
            assert!(viewer.drafts.contains_key(&root.join("a.rs")));
            assert!(
                !viewer.tree.is_dirty(Path::new("a.rs")),
                "another workspace's draft is not marked in this tree"
            );
        });
        viewer.update(cx, |viewer, cx| open_now(viewer, &root, "a.rs", cx));
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.text().to_owned()),
            "// edited\nfn a() {}\n"
        );
    }

    #[gpui::test]
    fn a_save_finishing_after_a_switch_settles_its_own_document(cx: &mut TestAppContext) {
        let tokio = runtime();
        let workspace = workspace(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let root = workspace.path().canonicalize().unwrap();
        let file = root.join("a.rs");
        let (viewer, cx) = cx.add_window_view(|_, cx| viewer(&tokio, cx));
        viewer.update(cx, |viewer, cx| open_now(viewer, &root, "a.rs", cx));
        let editor = viewer.read_with(cx, |viewer, _| viewer.editor());
        // Save a.rs, then switch to b.rs and make the same number of edits
        // there before the save completes: the buffer versions collide.
        viewer.update(cx, |viewer, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert("// one\n", false, cx);
                editor.save(false, cx);
            });
            open_now(viewer, &root, "b.rs", cx);
            editor.update(cx, |editor, cx| editor.insert("// two\n", false, cx));
        });
        wait_until(cx, |_| {
            std::fs::read_to_string(&file).unwrap() == "// one\nfn a() {}\n"
        });
        wait_until(cx, |cx| {
            viewer.read_with(cx, |viewer, _| viewer.drafts.is_empty())
        });
        viewer.read_with(cx, |viewer, cx| {
            let editor = viewer.editor.read(cx);
            assert!(editor.is_dirty(), "b.rs was never saved");
            assert_eq!(editor.text(), "// two\nfn b() {}\n");
            assert!(!viewer.tree.is_dirty(Path::new("a.rs")));
        });
    }

    #[gpui::test]
    fn save_writes_the_file_and_refuses_an_outside_change(cx: &mut TestAppContext) {
        let tokio = runtime();
        let workspace = workspace(&[("a.rs", "fn a() {}\n")]);
        let root = workspace.path().canonicalize().unwrap();
        let file = root.join("a.rs");
        let (viewer, cx) = cx.add_window_view(|_, cx| viewer(&tokio, cx));
        viewer.update(cx, |viewer, cx| open_now(viewer, &root, "a.rs", cx));
        let editor = viewer.read_with(cx, |viewer, _| viewer.editor());
        editor.update(cx, |editor, cx| {
            editor.insert("// one\n", false, cx);
            editor.save(false, cx);
        });
        wait_until(cx, |_| {
            std::fs::read_to_string(&file).unwrap() == "// one\nfn a() {}\n"
        });
        wait_until(cx, |cx| {
            !editor.read_with(cx, |editor, _| editor.is_dirty())
        });

        // Someone else writes the file; a save now must not clobber it.
        let outside = std::fs::File::options().write(true).open(&file).unwrap();
        std::io::Write::write_all(&mut &outside, b"// theirs\n").unwrap();
        outside
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        drop(outside);
        editor.update(cx, |editor, cx| {
            editor.insert("// two\n", false, cx);
            editor.save(false, cx);
        });
        wait_until(cx, |cx| editor.read_with(cx, |editor, _| editor.conflict));
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .starts_with("// theirs")
        );
        editor.update(cx, |editor, cx| editor.save(true, cx));
        wait_until(cx, |_| {
            std::fs::read_to_string(&file)
                .unwrap()
                .starts_with("// one\n// two\n")
        });
    }

    #[gpui::test]
    fn the_sidebar_docks_when_wide_and_floats_when_narrow(cx: &mut TestAppContext) {
        let tokio = runtime();
        let (viewer, cx) = cx.add_window_view(|_, cx| viewer(&tokio, cx));
        cx.simulate_resize(gpui::size(px(800.0), px(500.0)));
        cx.run_until_parked();
        viewer.read_with(cx, |viewer, _| {
            assert!(viewer.docked());
            assert!(viewer.sidebar_width() <= px(300.0));
        });
        cx.simulate_resize(gpui::size(px(360.0), px(500.0)));
        cx.run_until_parked();
        cx.run_until_parked();
        viewer.read_with(cx, |viewer, _| {
            assert!(!viewer.docked());
            assert!(viewer.sidebar_width() < px(360.0));
        });
    }

    #[gpui::test]
    fn workspace_search_runs_off_thread_and_opens_hits(cx: &mut TestAppContext) {
        let tokio = runtime();
        let workspace = workspace(&[("src/a.rs", "fn needle() {}\n"), ("notes.md", "a needle\n")]);
        let root = workspace.path().canonicalize().unwrap();
        let (viewer, cx) = cx.add_window_view(|_, cx| viewer(&tokio, cx));
        viewer.update(cx, |viewer, cx| {
            viewer.set_workspace(Some(root.clone()), cx);
            viewer.search.query.insert("needle");
            viewer.search.include.insert("*.rs");
            viewer.schedule_search(true, cx);
        });
        wait_until(cx, |cx| {
            viewer.read_with(cx, |viewer, _| !viewer.search.pending)
        });
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(viewer.search.outcome.total, 1);
            assert_eq!(
                viewer.search.rows,
                vec![ResultRow::File(0), ResultRow::Hit(0, 0)]
            );
        });
        viewer.update(cx, |viewer, cx| viewer.open_search_hit(0, 0, cx));
        wait_until(cx, |cx| {
            viewer.read_with(cx, |viewer, _| {
                viewer.tab_label().as_deref() == Some("a.rs")
            })
        });
        let editor = viewer.read_with(cx, |viewer, _| viewer.editor());
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.position()),
            (1, 4),
            "the caret lands on the hit"
        );
    }

    #[gpui::test]
    fn the_tree_filter_narrows_to_matching_files(cx: &mut TestAppContext) {
        let tokio = runtime();
        let workspace = workspace(&[
            ("src/button.rs", ""),
            ("src/menu.rs", ""),
            ("README.md", ""),
        ]);
        let root = workspace.path().canonicalize().unwrap();
        let (viewer, cx) = cx.add_window_view(|_, cx| viewer(&tokio, cx));
        viewer.update(cx, |viewer, cx| {
            viewer.set_workspace(Some(root.clone()), cx);
            viewer.filter.insert("btn");
            viewer.schedule_filter(cx);
        });
        wait_until(cx, |cx| {
            viewer.read_with(cx, |viewer, _| viewer.tree.filter().is_some())
        });
        viewer.read_with(cx, |viewer, _| {
            let rows = viewer.tree.rows();
            let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
            assert_eq!(names, ["src", "button.rs"]);
        });
    }

    #[test]
    fn the_fallback_theme_matches_the_chrome() {
        let colors = crate::app_theme::colors("dracula");
        assert_eq!(fallback_theme(colors).id, "dracula");
        assert_eq!(
            fallback_theme(SemanticColors::light()).id,
            TermTheme::DIRIJOR_LIGHT.id
        );
    }
}
