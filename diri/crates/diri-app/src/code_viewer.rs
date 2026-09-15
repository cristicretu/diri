//! Native, read-only code viewer for the trailing workbench.
//!
//! `code_intelligence` owns filesystem discovery, containment and loading.
//! This module owns only the presentation state: asynchronous opens, source
//! history, line targeting, virtualization, and lightweight lexical color.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, FontWeight, HighlightStyle, KeyDownEvent,
    ListHorizontalSizingBehavior, MouseButton, Render, ScrollStrategy, SharedString, StyledText,
    Task, UniformListScrollHandle, Window, div, prelude::*, px, rgba, uniform_list,
};

use crate::code_intelligence::{
    CodeIntelligence, CodeIntelligenceError, DirectoryEntry, SearchHit, SearchHitKind,
    SourceSnapshot,
};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use diri_ui::{Appearance, FloatingSurface, Radius, SemanticColors, Typo};

#[cfg(test)]
use crate::code_intelligence::SourceTarget;

const SOURCE_ROW_HEIGHT: f32 = 20.0;
const SOURCE_GUTTER_WIDTH: f32 = 52.0;

#[derive(Clone)]
enum ViewerState {
    Empty,
    Loading { reference: String },
    Ready(Arc<SourceSnapshot>),
    Error { reference: String, message: String },
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

pub struct CodeViewer {
    tokio: tokio::runtime::Handle,
    focus: FocusHandle,
    colors: SemanticColors,
    workspace_cwd: Option<PathBuf>,
    intelligence: Option<Arc<CodeIntelligence>>,
    state: ViewerState,
    scroll: UniformListScrollHandle,
    generation: u64,
    _load_task: Option<Task<()>>,
    _search_task: Option<Task<()>>,
    search_generation: u64,
    picker_open: bool,
    query: QueryEditor,
    results: Vec<SearchHit>,
    highlighted_result: usize,
    history: Vec<(PathBuf, String)>,
    history_index: usize,
    tree_visible: bool,
    tree_focused: bool,
    tree_generation: u64,
    directories: HashMap<PathBuf, Vec<DirectoryEntry>>,
    expanded: HashSet<PathBuf>,
    tree_loading: HashSet<PathBuf>,
    tree_errors: HashMap<PathBuf, String>,
    tree_selected: Option<PathBuf>,
    tree_scroll: UniformListScrollHandle,
    content_search: bool,
    search_pending: bool,
    search_error: Option<String>,
    search_cancel: Arc<AtomicU64>,
    result_scroll: gpui::ScrollHandle,
}

impl CodeViewer {
    pub fn new(
        tokio: tokio::runtime::Handle,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            tokio,
            focus: cx.focus_handle(),
            colors,
            workspace_cwd: None,
            intelligence: None,
            state: ViewerState::Empty,
            scroll: UniformListScrollHandle::new(),
            generation: 0,
            _load_task: None,
            _search_task: None,
            search_generation: 0,
            picker_open: false,
            query: QueryEditor::default(),
            results: Vec::new(),
            highlighted_result: 0,
            history: Vec::new(),
            history_index: 0,
            tree_visible: true,
            tree_focused: false,
            tree_generation: 0,
            directories: HashMap::new(),
            expanded: HashSet::new(),
            tree_loading: HashSet::new(),
            tree_errors: HashMap::new(),
            tree_selected: None,
            tree_scroll: UniformListScrollHandle::new(),
            content_search: false,
            search_pending: false,
            search_error: None,
            search_cancel: Arc::new(AtomicU64::new(0)),
            result_scroll: gpui::ScrollHandle::new(),
        }
    }

    pub fn set_colors(&mut self, colors: SemanticColors, cx: &mut Context<Self>) {
        if self.colors == colors {
            return;
        }
        self.colors = colors;
        cx.notify();
    }

    pub(crate) fn tab_label(&self) -> Option<String> {
        match &self.state {
            ViewerState::Ready(snapshot) => snapshot
                .relative_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            _ => None,
        }
    }

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
        let snapshot = intelligence
            .open_reference("diri/crates/diri-app/src/code_intelligence.rs:65")
            .unwrap();
        self.workspace_cwd = Some(cwd);
        for parent in snapshot.relative_path.ancestors().skip(1) {
            self.expanded.insert(parent.to_path_buf());
            self.directories.insert(
                parent.to_path_buf(),
                intelligence.directory_entries(parent).unwrap(),
            );
        }
        self.tree_selected = Some(snapshot.relative_path.clone());
        self.state = ViewerState::Ready(Arc::new(snapshot));
        self.intelligence = Some(intelligence);
        self.scroll.scroll_to_item(55, ScrollStrategy::Top);
        if let Some(index) = self
            .tree_rows()
            .iter()
            .position(|(entry, _)| Some(&entry.relative_path) == self.tree_selected.as_ref())
        {
            self.tree_scroll
                .scroll_to_item(index.saturating_sub(5), ScrollStrategy::Top);
        }
        if std::env::var_os("DIRI_VISUAL_SEARCH").is_some() {
            self.picker_open = true;
            self.query.insert("directory");
            self.results = self.intelligence.as_ref().unwrap().search("directory", 200);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn appearance(&self) -> diri_ui::Appearance {
        self.colors.appearance
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        if self.picker_open {
            window.focus(&self.focus, cx);
            self.schedule_search(cx);
        } else {
            self.search_cancel.fetch_add(1, Ordering::Relaxed);
            self._search_task = None;
            self.query.clear();
            self.results.clear();
            self.highlighted_result = 0;
        }
        cx.notify();
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_cancel.store(generation, Ordering::Relaxed);
        let cancel = self.search_cancel.clone();
        let intelligence = self.intelligence.clone();
        let Some(cwd) = self.workspace_cwd.clone() else {
            self.results.clear();
            self.search_pending = false;
            return;
        };
        self.search_pending = true;
        self.search_error = None;
        self.results.clear();
        self.highlighted_result = 0;
        let query = self.query.text().to_owned();
        let content_search = self.content_search;
        let tokio = self.tokio.clone();
        self._search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let result = tokio
                .spawn_blocking(move || {
                    let intelligence = match intelligence {
                        Some(intelligence) => intelligence,
                        None => Arc::new(CodeIntelligence::for_session(cwd)?),
                    };
                    let results = if content_search {
                        intelligence.search_content(&query, 201, || {
                            cancel.load(Ordering::Relaxed) != generation
                        })
                    } else {
                        intelligence.search(&query, 201)
                    };
                    Ok::<_, CodeIntelligenceError>((intelligence, results))
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.search_generation != generation || !this.picker_open {
                    return;
                }
                this.search_pending = false;
                match result {
                    Ok((intelligence, results)) => {
                        this.intelligence = Some(intelligence);
                        this.results = results;
                    }
                    Err(error) => this.search_error = Some(error),
                }
                cx.notify();
            });
        }));
    }

    fn load_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.tree_loading.contains(&path) {
            return;
        }
        let Some(cwd) = self.workspace_cwd.clone() else {
            return;
        };
        self.tree_loading.insert(path.clone());
        self.tree_errors.remove(&path);
        let generation = self.tree_generation;
        let intelligence = self.intelligence.clone();
        let tokio = self.tokio.clone();
        cx.spawn(async move |this, cx| {
            let requested = path.clone();
            let result = tokio
                .spawn_blocking(move || {
                    let intelligence = match intelligence {
                        Some(intelligence) => intelligence,
                        None => Arc::new(CodeIntelligence::for_session(cwd)?),
                    };
                    let entries = intelligence.directory_entries(&requested)?;
                    Ok::<_, CodeIntelligenceError>((intelligence, entries))
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.tree_generation != generation {
                    return;
                }
                this.tree_loading.remove(&path);
                match result {
                    Ok((intelligence, entries)) => {
                        this.intelligence = Some(intelligence);
                        this.directories.insert(path, entries);
                    }
                    Err(error) => {
                        this.tree_errors.insert(path, error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn tree_rows(&self) -> Vec<(DirectoryEntry, usize)> {
        fn visit(
            this: &CodeViewer,
            path: &std::path::Path,
            depth: usize,
            rows: &mut Vec<(DirectoryEntry, usize)>,
        ) {
            if let Some(entries) = this.directories.get(path) {
                for entry in entries {
                    rows.push((entry.clone(), depth));
                    if entry.is_dir && this.expanded.contains(&entry.relative_path) {
                        visit(this, &entry.relative_path, depth + 1, rows);
                    }
                }
            }
        }
        let mut rows = Vec::new();
        visit(self, std::path::Path::new(""), 0, &mut rows);
        rows
    }

    fn activate_tree_entry(&mut self, entry: DirectoryEntry, cx: &mut Context<Self>) {
        self.tree_selected = Some(entry.relative_path.clone());
        if entry.is_dir {
            if !self.expanded.remove(&entry.relative_path) {
                self.expanded.insert(entry.relative_path.clone());
                if !self.directories.contains_key(&entry.relative_path) {
                    self.load_directory(entry.relative_path, cx);
                }
            }
        } else if let Some(intelligence) = &self.intelligence {
            let cwd = intelligence.workspace_root().to_path_buf();
            if let Ok(uri) = url::Url::from_file_path(cwd.join(entry.relative_path)) {
                self.open_reference_inner(cwd, uri.to_string(), true, cx);
            }
        }
        cx.notify();
    }

    fn handle_tree_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let rows = self.tree_rows();
        if rows.is_empty() {
            return false;
        }
        let index = rows
            .iter()
            .position(|(entry, _)| Some(&entry.relative_path) == self.tree_selected.as_ref())
            .unwrap_or(0);
        let entry = rows[index].0.clone();
        let next = match key {
            "up" => index.saturating_sub(1),
            "down" => (index + 1).min(rows.len() - 1),
            "home" => 0,
            "end" => rows.len() - 1,
            "enter" | "space" => {
                self.activate_tree_entry(entry, cx);
                return true;
            }
            "right" => {
                if !entry.is_dir {
                    return true;
                }
                if entry.is_dir && !self.expanded.contains(&entry.relative_path) {
                    self.activate_tree_entry(entry, cx);
                    return true;
                }
                (index + 1).min(rows.len() - 1)
            }
            "left" => {
                if self.expanded.remove(&entry.relative_path) {
                    cx.notify();
                    return true;
                }
                rows.iter()
                    .position(|(candidate, _)| {
                        Some(candidate.relative_path.as_path()) == entry.relative_path.parent()
                    })
                    .unwrap_or(index)
            }
            _ => return false,
        };
        self.tree_selected = Some(rows[next].0.relative_path.clone());
        self.tree_scroll.scroll_to_item(next, ScrollStrategy::Top);
        cx.notify();
        true
    }

    fn render_tree(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.tree_rows();
        let label = self
            .intelligence
            .as_ref()
            .map(|index| index.workspace_root())
            .or(self.workspace_cwd.as_deref())
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "EXPLORER".into());
        let mut tree = div()
            .w(px(170.0))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(colors.primary.alpha(0.08))
            .child(
                div()
                    .h(px(30.0))
                    .flex_none()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(10.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.secondary)
                            .child(label),
                    )
                    .child(
                        div()
                            .id("refresh-file-tree")
                            .size(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::BADGE))
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                            .child(sf_symbol("arrow.clockwise.circle", 10.0, colors.secondary))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tree_generation = this.tree_generation.wrapping_add(1);
                                this.tree_loading.clear();
                                this.intelligence = None;
                                this.directories.clear();
                                this.tree_errors.clear();
                                this.load_directory(PathBuf::new(), cx);
                                for path in this.expanded.clone() {
                                    this.load_directory(path, cx);
                                }
                                if this.picker_open {
                                    this.schedule_search(cx);
                                }
                                cx.notify();
                            })),
                    ),
            );
        if rows.is_empty() {
            let message = if self.workspace_cwd.is_none() {
                "Select a local session to browse its files."
            } else if self.tree_loading.contains(&PathBuf::new()) {
                "Loading files…"
            } else if let Some(error) = self.tree_errors.get(&PathBuf::new()) {
                error.as_str()
            } else {
                "This folder is empty."
            };
            tree = tree.child(
                div()
                    .p(px(12.0))
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(message.to_owned()),
            );
        } else {
            let viewer = cx.entity().downgrade();
            tree = tree.child(
                uniform_list("workspace-file-tree", rows.len(), move |range, _, cx| {
                    viewer
                        .update(cx, |this, cx| {
                            range
                                .map(|index| {
                                    let (entry, depth) = &rows[index];
                                    let entry = entry.clone();
                                    let tooltip = this
                                        .tree_errors
                                        .get(&entry.relative_path)
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            entry.relative_path.to_string_lossy().into_owned()
                                        });
                                    let selected =
                                        this.tree_selected.as_ref() == Some(&entry.relative_path);
                                    let expanded = this.expanded.contains(&entry.relative_path);
                                    let name = entry
                                        .relative_path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned();
                                    let status = if this.tree_loading.contains(&entry.relative_path)
                                    {
                                        " …"
                                    } else if this.tree_errors.contains_key(&entry.relative_path) {
                                        " !"
                                    } else {
                                        ""
                                    };
                                    div()
                                        .id(("file-tree-row", index))
                                        .h(px(24.0))
                                        .pl(px(6.0 + *depth as f32 * 12.0))
                                        .pr(px(6.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .overflow_hidden()
                                        .bg(colors.primary.alpha(if selected { 0.10 } else { 0.0 }))
                                        .cursor_pointer()
                                        .hover(move |row| row.bg(colors.primary.alpha(0.07)))
                                        .child(div().w(px(8.0)).flex_none().when(
                                            entry.is_dir,
                                            |icon| {
                                                icon.child(sf_symbol(
                                                    if expanded {
                                                        "chevron.down"
                                                    } else {
                                                        "chevron.right"
                                                    },
                                                    8.0,
                                                    colors.tertiary,
                                                ))
                                            },
                                        ))
                                        .child(sf_symbol(
                                            if entry.is_dir {
                                                if expanded { "folder.fill" } else { "folder" }
                                            } else {
                                                "doc.text"
                                            },
                                            11.0,
                                            colors.secondary,
                                        ))
                                        .child(
                                            div()
                                                .min_w(px(0.0))
                                                .flex_1()
                                                .truncate()
                                                .text_size(px(11.0))
                                                .text_color(colors.primary)
                                                .child(format!("{name}{status}")),
                                        )
                                        .tooltip(move |_, cx| {
                                            cx.new(|_| ExplorerTooltip(tooltip.clone(), colors))
                                                .into()
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.tree_focused = true;
                                            window.focus(&this.focus, cx);
                                            this.activate_tree_entry(entry.clone(), cx);
                                        }))
                                        .into_any_element()
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .track_scroll(&self.tree_scroll)
                .flex_1()
                .min_h(px(0.0)),
            );
        }
        tree.into_any_element()
    }

    /// Selects the local workspace represented by the active agent. The file
    /// index remains lazy, but the picker can now be used before a source file
    /// has been opened. Switching agents clears stale source and history.
    pub fn set_workspace(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.workspace_cwd == cwd {
            return;
        }
        self.workspace_cwd = cwd;
        self.tree_generation = self.tree_generation.wrapping_add(1);
        self.directories.clear();
        self.expanded.clear();
        self.tree_loading.clear();
        self.tree_errors.clear();
        self.tree_selected = None;
        self.tree_scroll = UniformListScrollHandle::new();
        self.intelligence = None;
        self.state = ViewerState::Empty;
        self.scroll = UniformListScrollHandle::new();
        self.generation = self.generation.wrapping_add(1);
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_cancel
            .store(self.search_generation, Ordering::Relaxed);
        self._search_task = None;
        self.picker_open = false;
        self.query.clear();
        self.results.clear();
        self.highlighted_result = 0;
        self.history.clear();
        self.history_index = 0;
        cx.notify();
    }

    fn open_highlighted(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.results.get(self.highlighted_result).cloned() else {
            return;
        };
        let Some(intelligence) = &self.intelligence else {
            return;
        };
        let cwd = intelligence.workspace_root().to_path_buf();
        let Ok(mut reference) = url::Url::from_file_path(cwd.join(&hit.relative_path)) else {
            return;
        };
        if let Some(line) = hit.line {
            reference.set_fragment(Some(&format!("L{line}")));
        }
        let reference = reference.to_string();
        self.picker_open = false;
        self.query.clear();
        self.results.clear();
        self.open_reference_inner(cwd, reference, true, cx);
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.platform && matches!(event.keystroke.key.as_str(), "f" | "p") {
            self.content_search = event.keystroke.key == "f";
            self.picker_open = false;
            self.toggle_picker(window, cx);
            cx.stop_propagation();
            return;
        }
        if !self.picker_open {
            if self.tree_focused && self.handle_tree_key(&event.keystroke.key, cx) {
                cx.stop_propagation();
            }
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.search_cancel.fetch_add(1, Ordering::Relaxed);
                self._search_task = None;
                self.picker_open = false;
                self.query.clear();
                self.results.clear();
                cx.notify();
            }
            "up" => {
                self.highlighted_result = self.highlighted_result.saturating_sub(1);
                self.result_scroll.scroll_to_item(self.highlighted_result);
                cx.notify();
            }
            "down" => {
                self.highlighted_result = (self.highlighted_result + 1)
                    .min(self.results.len().min(200).saturating_sub(1));
                self.result_scroll.scroll_to_item(self.highlighted_result);
                cx.notify();
            }
            "enter" => self.open_highlighted(cx),
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return;
                };
                let changed = match edit {
                    Edit::Local(local) => self.query.apply(local),
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.query, cx);
                        false
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.query, cx)
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .is_some_and(|text| self.query.insert(&text)),
                };
                if changed {
                    self.schedule_search(cx);
                }
                cx.notify();
            }
        }
        cx.stop_propagation();
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

    fn open_reference_inner(
        &mut self,
        cwd: PathBuf,
        reference: String,
        record_history: bool,
        cx: &mut Context<Self>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.scroll = UniformListScrollHandle::new();
        self.state = ViewerState::Loading {
            reference: reference.clone(),
        };
        cx.notify();

        let tokio = self.tokio.clone();
        let history_cwd = cwd.clone();
        let intelligence = self.intelligence.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let task_reference = reference.clone();
            let result = tokio
                .spawn_blocking(move || -> Result<_, CodeIntelligenceError> {
                    let intelligence = match intelligence {
                        Some(intelligence) => intelligence,
                        None => Arc::new(CodeIntelligence::for_session(&cwd)?),
                    };
                    let snapshot = intelligence.open_reference(&task_reference)?;
                    Ok((intelligence, snapshot))
                })
                .await
                .map_err(|error| format!("Code viewer stopped: {error}"))
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((intelligence, snapshot)) => {
                        this.intelligence = Some(intelligence);
                        let target_line = snapshot.target.map(|target| target.line);
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
                        this.tree_selected = Some(snapshot.relative_path.clone());
                        for parent in snapshot.relative_path.ancestors().skip(1) {
                            this.expanded.insert(parent.to_path_buf());
                            if !this.directories.contains_key(parent) {
                                this.load_directory(parent.to_path_buf(), cx);
                            }
                        }
                        this.state = ViewerState::Ready(Arc::new(snapshot));
                        if let Some(line) = target_line {
                            this.scroll
                                .scroll_to_item(line.saturating_sub(1), ScrollStrategy::Center);
                        }
                    }
                    Err(message) => {
                        this.state = ViewerState::Error { reference, message };
                    }
                }
                cx.notify();
            });
        }));
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

    fn render_toolbar(
        &self,
        snapshot: Option<&SourceSnapshot>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let can_back = !self.history.is_empty() && self.history_index > 0;
        let can_forward = self.history_index + 1 < self.history.len();
        let picker_open = self.picker_open;
        let (path, location) = snapshot.map_or_else(
            || ("No file open".to_owned(), None),
            |snapshot| {
                (
                    snapshot.relative_path.to_string_lossy().into_owned(),
                    snapshot.target.map(|target| {
                        if target.column > 1 {
                            format!("{}:{}", target.line, target.column)
                        } else {
                            target.line.to_string()
                        }
                    }),
                )
            },
        );
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
                .text_color(if enabled {
                    colors.secondary
                } else {
                    colors.primary.alpha(0.20)
                })
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
            .h(px(38.0))
            .flex_none()
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(3.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.06))
            .child(
                div()
                    .id("toggle-file-tree")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                    .child(sf_symbol("sidebar.left", 12.0, colors.secondary))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tree_visible = !this.tree_visible;
                        cx.notify();
                    })),
            )
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
            .child(
                div()
                    .id("code-open-file-picker")
                    .size(px(24.0))
                    .ml(px(2.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE))
                    .bg(if picker_open {
                        colors.primary.alpha(0.09)
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                    .child(sf_symbol("magnifyingglass", 10.5, colors.secondary))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_picker(window, cx);
                        cx.stop_propagation();
                    })),
            )
            .child(
                div()
                    .ml(px(2.0))
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(sf_symbol(
                        "doc.text",
                        11.5,
                        if snapshot.is_some() {
                            colors.secondary
                        } else {
                            colors.tertiary
                        },
                    ))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(Typo::META.size))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.secondary)
                            .child(path),
                    ),
            )
            .when_some(location, |bar, location| {
                bar.child(
                    div()
                        .px(px(6.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .bg(colors.primary.alpha(0.055))
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(9.5))
                        .text_color(colors.tertiary)
                        .child(location),
                )
            })
            .into_any_element()
    }

    fn render_source(&self, snapshot: Arc<SourceSnapshot>, colors: SemanticColors) -> AnyElement {
        let rows = snapshot.lines.len();
        let target = snapshot.target.map(|target| target.line);
        let extension = snapshot
            .relative_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_owned();
        let content_width = snapshot
            .lines
            .iter()
            .map(|line| snapshot.text[line.range.clone()].trim_end().chars().count())
            .max()
            .unwrap_or(0) as f32
            * 7.1
            + SOURCE_GUTTER_WIDTH
            + 24.0;
        uniform_list("code-viewer-source", rows, move |range, _, _| {
            range
                .map(|index| {
                    source_row(
                        &snapshot,
                        index,
                        target == Some(index + 1),
                        &extension,
                        content_width.max(320.0),
                        colors,
                    )
                })
                .collect()
        })
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    fn render_picker(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                        } else if self.search_pending {
                            "Searching…"
                        } else if let Some(error) = self.search_error.as_deref() {
                            error
                        } else if query_empty && self.content_search {
                            "Search for text across the workspace"
                        } else {
                            "No matches"
                        }
                        .to_owned(),
                    ),
            );
        } else {
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
                            if symbol {
                                code_palette(colors).keyword
                            } else {
                                colors.secondary
                            },
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
                .child(if self.content_search {
                    "Search text…"
                } else {
                    "Search files and symbols…"
                })
                .into_any_element()
        } else {
            crate::navigation::query_label(&self.query)
        };
        Some(
            div()
                .absolute()
                .top(px(40.0))
                .left(px(8.0))
                .right(px(8.0))
                .occlude()
                .rounded(px(Radius::PANEL))
                .bg(colors.sidebar_surface().alpha(1.0))
                .child(FloatingSurface::new(
                    colors,
                    div()
                        .rounded(px(Radius::PANEL))
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
                                        window.focus(&this.focus, cx);
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
                        .child(
                            div()
                                .h(px(30.0))
                                .px(px(8.0))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .children(
                                    [(false, "Files & symbols"), (true, "Text")]
                                        .into_iter()
                                        .map(|(content, label)| {
                                            let active = self.content_search == content;
                                            div()
                                                .id(("search-mode", usize::from(content)))
                                                .px(px(7.0))
                                                .py(px(3.0))
                                                .rounded(px(Radius::BADGE))
                                                .text_size(px(10.0))
                                                .text_color(if active {
                                                    colors.primary
                                                } else {
                                                    colors.tertiary
                                                })
                                                .bg(colors.primary.alpha(if active {
                                                    0.09
                                                } else {
                                                    0.0
                                                }))
                                                .cursor_pointer()
                                                .child(label)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.content_search = content;
                                                        window.focus(&this.focus, cx);
                                                        this.schedule_search(cx);
                                                        cx.notify();
                                                    },
                                                ))
                                        }),
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
                                    "{}{} results · ↑↓ select · ↵ open · Esc close{}",
                                    self.results.len().min(200),
                                    if self.results.len() > 200 { "+" } else { "" },
                                    if self.content_search {
                                        " · smart case · 32 MB scan limit"
                                    } else {
                                        ""
                                    }
                                )),
                        ),
                ))
                .into_any_element(),
        )
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

impl Focusable for CodeViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CodeViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.tree_visible
            && self.workspace_cwd.is_some()
            && self.directories.is_empty()
            && self.tree_loading.is_empty()
            && self.tree_errors.is_empty()
        {
            self.load_directory(PathBuf::new(), cx);
        }
        let colors = self.colors;
        let snapshot = match &self.state {
            ViewerState::Ready(snapshot) => Some(Arc::clone(snapshot)),
            _ => None,
        };
        let body = match &self.state {
            ViewerState::Empty => self.render_message(
                colors,
                "cursorarrow.click.2",
                "Explore your workspace",
                "Choose a file in the tree, or search for a file, symbol, or text.",
            ),
            ViewerState::Loading { reference } => self.render_message(
                colors,
                "ellipsis",
                "Opening file",
                format!("Resolving {reference}…"),
            ),
            ViewerState::Ready(snapshot) => self.render_source(Arc::clone(snapshot), colors),
            ViewerState::Error { reference, message } => self.render_message(
                colors,
                "exclamationmark.triangle",
                format!("Couldn’t open {reference}"),
                message.clone(),
            ),
        };
        let picker = self.render_picker(colors, cx);
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(colors.background)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(self.render_toolbar(snapshot.as_deref(), colors, cx))
            .child(
                div()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .when(self.tree_visible, |row| {
                        row.child(self.render_tree(colors, cx))
                    })
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .child(body),
                    ),
            )
            .when_some(picker, |viewer, picker| viewer.child(picker))
    }
}

fn source_row(
    snapshot: &SourceSnapshot,
    index: usize,
    targeted: bool,
    extension: &str,
    content_width: f32,
    colors: SemanticColors,
) -> AnyElement {
    let palette = code_palette(colors);
    let line = &snapshot.lines[index];
    let source = snapshot.text[line.range.clone()]
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    let styled = highlighted_source(source, extension, palette);
    div()
        .id(index)
        .h(px(SOURCE_ROW_HEIGHT))
        .min_w(px(content_width))
        .w_full()
        .flex()
        .items_center()
        .bg(if targeted {
            rgba(0xd977571c)
        } else {
            colors.background
        })
        .when(targeted, |row| {
            row.border_l_2().border_color(rgba(0xd97757ff))
        })
        .child(
            div()
                .w(px(SOURCE_GUTTER_WIDTH))
                .h_full()
                .flex_none()
                .pr(px(10.0))
                .flex()
                .items_center()
                .justify_end()
                .border_r_1()
                .border_color(colors.primary.alpha(0.055))
                .font_family(crate::fonts::mono_family())
                .text_size(px(10.0))
                .text_color(if targeted {
                    rgba(0xd97757ff)
                } else {
                    colors.primary.alpha(0.26)
                })
                .child(line.number.to_string()),
        )
        .child(
            div()
                .h_full()
                .min_w(px(0.0))
                .pl(px(10.0))
                .flex()
                .items_center()
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.5))
                .text_color(palette.foreground)
                .child(styled),
        )
        .into_any_element()
}

#[derive(Clone, Copy)]
struct CodePalette {
    foreground: gpui::Rgba,
    comment: gpui::Rgba,
    string: gpui::Rgba,
    keyword: gpui::Rgba,
}

fn code_palette(colors: SemanticColors) -> CodePalette {
    match colors.appearance {
        Appearance::Dark => CodePalette {
            foreground: rgba(0xd8dee9ff),
            comment: rgba(0x718096ff),
            string: rgba(0xd7ba7dff),
            keyword: rgba(0xc792eaff),
        },
        Appearance::Light => CodePalette {
            foreground: rgba(0x2f3337ff),
            comment: rgba(0x5f6368ff),
            string: rgba(0x7a4d00ff),
            keyword: rgba(0x6f42c1ff),
        },
    }
}

fn highlighted_source(source: String, extension: &str, palette: CodePalette) -> AnyElement {
    let ranges = lexical_highlights(&source, extension, palette);
    if ranges.is_empty() {
        return div().child(source).into_any_element();
    }
    StyledText::new(source)
        .with_highlights(ranges)
        .into_any_element()
}

fn lexical_highlights(
    source: &str,
    extension: &str,
    palette: CodePalette,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut ranges = Vec::new();
    let comment_start = match extension {
        "py" | "rb" | "sh" | "bash" | "zsh" | "fish" | "toml" | "yaml" | "yml" => source.find('#'),
        "sql" => source.find("--"),
        _ => source.find("//"),
    };
    let code_end = comment_start.unwrap_or(source.len());
    if let Some(start) = comment_start {
        ranges.push((
            start..source.len(),
            HighlightStyle {
                color: Some(palette.comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..HighlightStyle::default()
            },
        ));
    }

    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < code_end {
        let quote = bytes[cursor];
        if quote != b'"' && quote != b'\'' && quote != b'`' {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < code_end {
            if bytes[cursor] == b'\\' {
                cursor = (cursor + 2).min(code_end);
            } else if bytes[cursor] == quote {
                cursor += 1;
                break;
            } else {
                cursor += 1;
            }
        }
        ranges.push((
            start..cursor,
            HighlightStyle {
                color: Some(palette.string.into()),
                ..HighlightStyle::default()
            },
        ));
    }

    let keywords = match extension {
        "rs" => RUST_KEYWORDS,
        "swift" => SWIFT_KEYWORDS,
        "py" => PYTHON_KEYWORDS,
        "js" | "jsx" | "ts" | "tsx" => JS_KEYWORDS,
        _ => COMMON_KEYWORDS,
    };
    for keyword in keywords {
        for (start, _) in source[..code_end].match_indices(keyword) {
            let end = start + keyword.len();
            let left_ok = start == 0 || !is_ident(source.as_bytes()[start - 1]);
            let right_ok = end == code_end || !is_ident(source.as_bytes()[end]);
            if left_ok && right_ok && !ranges.iter().any(|(range, _)| range.contains(&start)) {
                ranges.push((
                    start..end,
                    HighlightStyle {
                        color: Some(palette.keyword.into()),
                        font_weight: Some(FontWeight::MEDIUM),
                        ..HighlightStyle::default()
                    },
                ));
            }
        }
    }
    ranges.sort_by_key(|(range, _)| range.start);
    ranges
}

const fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const SWIFT_KEYWORDS: &[&str] = &[
    "actor",
    "async",
    "await",
    "case",
    "class",
    "defer",
    "else",
    "enum",
    "extension",
    "false",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "init",
    "let",
    "nil",
    "protocol",
    "return",
    "self",
    "static",
    "struct",
    "switch",
    "throw",
    "true",
    "try",
    "var",
    "while",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "not", "or", "pass", "raise", "return", "True", "try", "while", "with",
    "yield",
];
const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "while",
    "yield",
];
const COMMON_KEYWORDS: &[&str] = &[
    "class", "const", "else", "enum", "false", "for", "function", "if", "import", "let", "null",
    "return", "static", "struct", "true", "type", "var", "while",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_luminance(color: gpui::Rgba) -> f32 {
        fn linear(channel: f32) -> f32 {
            if channel <= 0.03928 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn contrast(left: gpui::Rgba, right: gpui::Rgba) -> f32 {
        let left = relative_luminance(left);
        let right = relative_luminance(right);
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }

    #[gpui::test]
    fn explorer_keyboard_navigates_and_collapses_directories(cx: &mut gpui::TestAppContext) {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (viewer, cx) = cx.add_window_view(|window, cx| {
            let mut viewer = CodeViewer::new(tokio.handle().clone(), SemanticColors::dark(), cx);
            viewer.seed_explorer_preview(cx);
            viewer.tree_focused = true;
            window.focus(&viewer.focus, cx);
            viewer
        });
        cx.simulate_resize(gpui::size(px(600.0), px(500.0)));
        cx.run_until_parked();
        cx.simulate_keystrokes("left");
        viewer.read_with(cx, |viewer, _| {
            assert_eq!(
                viewer.tree_selected.as_deref(),
                Some(std::path::Path::new("diri/crates/diri-app/src"))
            )
        });
        cx.simulate_keystrokes("left");
        viewer.read_with(cx, |viewer, _| {
            assert!(
                !viewer
                    .expanded
                    .contains(std::path::Path::new("diri/crates/diri-app/src"))
            )
        });
        cx.simulate_keystrokes("right");
        viewer.read_with(cx, |viewer, _| {
            assert!(
                viewer
                    .expanded
                    .contains(std::path::Path::new("diri/crates/diri-app/src"))
            )
        });
        cx.simulate_keystrokes("down");
        viewer.read_with(cx, |viewer, _| {
            assert_ne!(
                viewer.tree_selected.as_deref(),
                Some(std::path::Path::new("diri/crates/diri-app/src"))
            )
        });
    }

    #[test]
    fn highlights_keywords_strings_and_comments_without_overlapping() {
        let source = "pub fn main() { let value = \"hello\"; // note";
        let ranges = lexical_highlights(source, "rs", code_palette(SemanticColors::dark()));
        assert!(
            ranges
                .iter()
                .any(|(range, _)| &source[range.clone()] == "pub")
        );
        assert!(
            ranges
                .iter()
                .any(|(range, _)| &source[range.clone()] == "fn")
        );
        assert!(
            ranges
                .iter()
                .any(|(range, _)| &source[range.clone()] == "\"hello\"")
        );
        assert!(
            ranges
                .iter()
                .any(|(range, _)| &source[range.clone()] == "// note")
        );
    }

    #[test]
    fn keyword_boundaries_do_not_color_identifiers() {
        let source = "format for before";
        let ranges = lexical_highlights(source, "rs", code_palette(SemanticColors::dark()));
        let words: Vec<_> = ranges
            .iter()
            .map(|(range, _)| &source[range.clone()])
            .collect();
        assert_eq!(words, vec!["for"]);
    }

    #[test]
    fn light_code_palette_keeps_source_tokens_readable() {
        for theme in ["dirijor-light", "solarized-light", "github-light"] {
            let colors = crate::app_theme::colors(theme);
            let palette = code_palette(colors);

            for (role, foreground) in [
                ("source", palette.foreground),
                ("comment", palette.comment),
                ("string", palette.string),
                ("keyword", palette.keyword),
            ] {
                assert!(
                    contrast(foreground, colors.background) >= 4.5,
                    "{role} contrast must remain readable with {theme}"
                );
            }
        }
    }

    #[test]
    fn target_type_is_one_based() {
        let target = SourceTarget {
            line: 12,
            column: 4,
        };
        assert_eq!((target.line, target.column), (12, 4));
    }
}
