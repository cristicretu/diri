//! Native trailing workbench inspector.
//!
//! The root knows only whether this view is mounted and how wide its dock is.
//! This module owns selection tracking, session/PR/artifact projections,
//! background Git refreshes, unified-diff snapshots, and diff virtualization.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::{
    AgentKind as ProtoAgentKind, ArtifactKind, PrCheck, PrDiscussionItem, PullRequestStatus,
    SessionArtifact, SessionDiffBase, SessionId, SessionRecord, SessionStatus,
};
use diri_ui::{
    AgentKind, AgentLogo, Fill, FloatingSurface, Ink, Metrics, Radius, SemanticColors, Typo,
};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyDownEvent, ListHorizontalSizingBehavior, MouseButton,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Task,
    UniformListScrollHandle, Window, div, ease_out_quint, point, prelude::*, px, rgba,
    uniform_list,
};

use crate::code_viewer::CodeViewer;
use crate::diff::{
    DiffFile, DiffHunk, DiffLayer, DiffRow, DiffRowKind, DiffSnapshot, load_local_diff,
    snapshot_from_read_diff,
};
use crate::git_review::{GitRepository, GitReviewError, PatchMutation, ReviewStatus};
use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::markdown::MarkdownDocument;
use crate::markdown_view::render_markdown;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::review_prompt::{ReviewEvidence, ReviewLayer, ReviewPrompt};
use crate::store::{InspectorTab, StoreRuntime};

const DIFF_ROW_HEIGHT: f32 = 20.0;
const GUTTER_WIDTH: f32 = 68.0;
const REFRESH_INTERVAL: Duration = Duration::from_millis(1400);
const SCROLLBAR_INSET: f32 = 4.0;
const SCROLLBAR_MIN_THUMB: f32 = 34.0;

#[derive(Clone, Copy)]
struct DraggedDiffScrollbar;

impl Render for DraggedDiffScrollbar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ScrollbarInteraction {
    dragging: bool,
    grab_offset: f32,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarMetrics {
    track_top: f32,
    track_height: f32,
    thumb_height: f32,
    thumb_top: f32,
}

#[derive(Clone, Copy, Debug)]
pub enum InspectorEvent {
    Close,
}

impl InspectorTab {
    const ALL: [Self; 4] = [Self::Info, Self::Changes, Self::Code, Self::Artifacts];

    const fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Changes => "Review",
            Self::Code => "Code",
            Self::Artifacts => "Artifacts",
        }
    }

    const fn index(self) -> i8 {
        match self {
            Self::Info => 0,
            Self::Changes => 1,
            Self::Code => 2,
            Self::Artifacts => 3,
        }
    }

    const fn debug_selector(self) -> &'static str {
        match self {
            Self::Info => "INSPECTOR_TAB_INFO",
            Self::Changes => "INSPECTOR_TAB_CHANGES",
            Self::Code => "INSPECTOR_TAB_CODE",
            Self::Artifacts => "INSPECTOR_TAB_ARTIFACTS",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffContext {
    id: SessionId,
    cwd: PathBuf,
    remote: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LoadState {
    NoSession,
    Loading,
    Ready(Arc<DiffSnapshot>),
    Error(String),
}

#[derive(Clone, Debug)]
enum ReviewLoadState {
    NoSession,
    Remote,
    Loading,
    Ready(Arc<ReviewStatus>),
    Error(String),
}

#[derive(Clone, Debug)]
enum ReviewAction {
    Stage(Vec<PathBuf>),
    Unstage(Vec<PathBuf>),
    Discard(Vec<PathBuf>),
    Patch {
        patch: Vec<u8>,
        mutation: PatchMutation,
    },
    Commit(String),
}

#[derive(Clone, Debug)]
struct AskDraft {
    evidence: Vec<ReviewEvidence>,
    label: String,
}

pub struct WorkbenchInspector {
    runtime: Arc<StoreRuntime>,
    _tokio_owner: Arc<tokio::runtime::Runtime>,
    tokio: tokio::runtime::Handle,
    code_viewer: Entity<CodeViewer>,
    markdown_cache: HashMap<String, Arc<MarkdownDocument>>,
    focus: FocusHandle,
    visible: bool,
    selected_tab: InspectorTab,
    tab_direction: f32,
    tab_transition_generation: u64,
    context: Option<DiffContext>,
    state: LoadState,
    review_state: ReviewLoadState,
    review_generation: u64,
    review_task: Option<Task<()>>,
    review_action_task: Option<Task<()>>,
    review_action_busy: bool,
    review_feedback: Option<(bool, String)>,
    ask_draft: Option<AskDraft>,
    ask_query: QueryEditor,
    ask_task: Option<Task<()>>,
    ask_busy: bool,
    ask_feedback: Option<(bool, String)>,
    commit_open: bool,
    commit_query: QueryEditor,
    discard_armed: bool,
    armed_hunk: Option<u64>,
    diff_layer: DiffLayer,
    files_open: bool,
    comparison: SessionDiffBase,
    comparison_menu_open: bool,
    loading: bool,
    generation: u64,
    scroll: UniformListScrollHandle,
    scrollbar_interaction: ScrollbarInteraction,
    scrollbar_layout_primed: bool,
    refresh_task: Option<Task<()>>,
    _poll_task: Task<()>,
    _store_changes: Task<()>,
}

impl EventEmitter<InspectorEvent> for WorkbenchInspector {}

impl Focusable for WorkbenchInspector {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WorkbenchInspector {
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokio = tokio_owner.handle().clone();
        let code_viewer = cx.new(|cx| CodeViewer::new(tokio.clone(), cx));
        let focus = cx.focus_handle();
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        if this.visible {
                            this.refresh(false, cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut changes = runtime.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| this.refresh_if_context_changed(cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let selected_tab = runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .inspector_tab;
        Self {
            runtime,
            _tokio_owner: tokio_owner,
            tokio,
            code_viewer,
            markdown_cache: HashMap::new(),
            focus,
            visible: false,
            selected_tab,
            tab_direction: 1.0,
            tab_transition_generation: 0,
            context: None,
            state: LoadState::NoSession,
            review_state: ReviewLoadState::NoSession,
            review_generation: 0,
            review_task: None,
            review_action_task: None,
            review_action_busy: false,
            review_feedback: None,
            ask_draft: None,
            ask_query: QueryEditor::default(),
            ask_task: None,
            ask_busy: false,
            ask_feedback: None,
            commit_open: false,
            commit_query: QueryEditor::default(),
            discard_armed: false,
            armed_hunk: None,
            diff_layer: DiffLayer::Branch,
            files_open: false,
            comparison: SessionDiffBase::DefaultBranch,
            comparison_menu_open: false,
            loading: false,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            scrollbar_interaction: ScrollbarInteraction::default(),
            scrollbar_layout_primed: false,
            refresh_task: None,
            _poll_task: poll_task,
            _store_changes: store_changes,
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.refresh(true, cx);
        } else {
            self.comparison_menu_open = false;
            self.files_open = false;
            self.ask_draft = None;
            self.ask_feedback = None;
            self.ask_query.clear();
        }
        cx.notify();
    }

    /// Opens a terminal or diff-shaped file reference in the native code tab.
    /// The viewer owns resolution and loading; the inspector only preserves
    /// the workbench's spatial context and selects the destination tab.
    pub fn open_file_reference(
        &mut self,
        cwd: impl Into<PathBuf>,
        reference: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let cwd = cwd.into();
        let reference = reference.into();
        self.code_viewer.update(cx, |viewer, cx| {
            viewer.open_reference(cwd, reference, cx);
        });
        self.select_tab(InspectorTab::Code, cx);
    }

    fn selected_context(&self) -> Option<DiffContext> {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let session = store.selected_session()?;
        Some(DiffContext {
            id: session.id.clone(),
            cwd: PathBuf::from(&session.cwd),
            remote: session.host.is_some(),
        })
    }

    fn refresh_if_context_changed(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        if self.selected_context() != self.context {
            self.refresh(true, cx);
        } else {
            // Info and Artifacts are projections of the live session record,
            // so same-session store changes still need to repaint the panel.
            cx.notify();
        }
    }

    fn select_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        if self.selected_tab == tab {
            return;
        }
        self.tab_direction = if tab.index() > self.selected_tab.index() {
            1.0
        } else {
            -1.0
        };
        self.selected_tab = tab;
        if let Err(error) = self
            .runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.inspector_tab = tab)
        {
            eprintln!("diri: could not remember inspector tab: {error}");
        }
        self.comparison_menu_open = false;
        self.tab_transition_generation = self.tab_transition_generation.wrapping_add(1);
        if tab == InspectorTab::Changes {
            self.refresh(true, cx);
        }
        cx.notify();
    }

    fn select_comparison(&mut self, comparison: SessionDiffBase, cx: &mut Context<Self>) {
        self.comparison_menu_open = false;
        if self.comparison == comparison {
            cx.notify();
            return;
        }
        self.comparison = comparison;
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
        self.refresh(true, cx);
    }

    fn select_diff_layer(&mut self, layer: DiffLayer, cx: &mut Context<Self>) {
        self.files_open = false;
        self.armed_hunk = None;
        self.discard_armed = false;
        self.commit_open = false;
        if self.diff_layer == layer {
            cx.notify();
            return;
        }
        self.diff_layer = layer;
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
        self.refresh(true, cx);
    }

    fn jump_to_diff_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.files_open = false;
        self.scroll.scroll_to_item(row, ScrollStrategy::Top);
        cx.notify();
    }

    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.visible || (self.loading && !force) {
            return;
        }
        let Some(context) = self.selected_context() else {
            self.context = None;
            self.state = LoadState::NoSession;
            self.review_state = ReviewLoadState::NoSession;
            self.code_viewer
                .update(cx, |viewer, cx| viewer.set_workspace(None, cx));
            cx.notify();
            return;
        };
        let context_changed = self.context.as_ref() != Some(&context);
        if context_changed {
            self.scroll = UniformListScrollHandle::new();
            self.scrollbar_interaction = ScrollbarInteraction::default();
            self.scrollbar_layout_primed = false;
            self.files_open = false;
            self.armed_hunk = None;
            self.ask_draft = None;
            self.ask_feedback = None;
            self.ask_query.clear();
            let workspace = (!context.remote).then(|| context.cwd.clone());
            self.code_viewer
                .update(cx, |viewer, cx| viewer.set_workspace(workspace, cx));
        }
        self.context = Some(context.clone());
        self.refresh_review(&context, force, cx);
        if !force && !context_changed && matches!(self.state, LoadState::NoSession) {
            return;
        }

        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if context_changed || !matches!(self.state, LoadState::Ready(_)) {
            self.state = LoadState::Loading;
            cx.notify();
        }
        let cwd = context.cwd;
        let session_id = context.id;
        let remote = context.remote;
        let comparison = self.comparison;
        let layer = self.diff_layer;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = if remote {
                tokio
                    .spawn(async move { client.read_diff(&session_id, comparison).await })
                    .await
                    .map_err(|error| format!("Diff request stopped: {error}"))
                    .and_then(|result| result.map_err(|error| error.to_string()))
                    .map(snapshot_from_read_diff)
                    .map(Arc::new)
            } else {
                tokio
                    .spawn_blocking(move || load_local_diff(&cwd, layer))
                    .await
                    .map_err(|error| format!("Diff worker stopped: {error}"))
                    .and_then(|result| result.map_err(|error| error.to_string()))
                    .map(Arc::new)
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                let next = match result {
                    Ok(snapshot) => LoadState::Ready(snapshot),
                    Err(error) => LoadState::Error(error),
                };
                if this.state != next {
                    this.state = next;
                    this.scrollbar_layout_primed = false;
                    cx.notify();
                }
            });
        }));
    }

    fn refresh_review(&mut self, context: &DiffContext, force: bool, cx: &mut Context<Self>) {
        if context.remote {
            self.review_state = ReviewLoadState::Remote;
            return;
        }
        if self.review_action_busy && !force {
            return;
        }
        self.review_generation = self.review_generation.wrapping_add(1);
        let generation = self.review_generation;
        let cwd = context.cwd.clone();
        if !matches!(self.review_state, ReviewLoadState::Ready(_)) {
            self.review_state = ReviewLoadState::Loading;
        }
        let tokio = self.tokio.clone();
        self.review_task = Some(cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn_blocking(move || {
                    let repository = GitRepository::discover(&cwd)?;
                    repository.status()
                })
                .await
                .map_err(|error| format!("Git status worker stopped: {error}"))
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                if this.review_generation != generation {
                    return;
                }
                this.review_state = match result {
                    Ok(status) => ReviewLoadState::Ready(Arc::new(status)),
                    Err(error) => ReviewLoadState::Error(error),
                };
                cx.notify();
            });
        }));
    }

    fn run_review_action(&mut self, action: ReviewAction, cx: &mut Context<Self>) {
        if self.review_action_busy {
            return;
        }
        let Some(context) = self.context.clone().filter(|context| !context.remote) else {
            return;
        };
        self.review_action_busy = true;
        self.review_feedback = None;
        self.discard_armed = false;
        self.armed_hunk = None;
        cx.notify();
        let tokio = self.tokio.clone();
        self.review_action_task = Some(cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn_blocking(move || -> Result<String, GitReviewError> {
                    let repository = GitRepository::discover(&context.cwd)?;
                    match action {
                        ReviewAction::Stage(paths) => {
                            repository.stage_paths(&paths)?;
                            Ok("Changes staged".to_owned())
                        }
                        ReviewAction::Unstage(paths) => {
                            repository.unstage_paths(&paths)?;
                            Ok("Changes moved back to the working tree".to_owned())
                        }
                        ReviewAction::Discard(paths) => {
                            repository.discard_unstaged(&paths)?;
                            Ok("Unstaged edits discarded".to_owned())
                        }
                        ReviewAction::Patch { patch, mutation } => {
                            repository.apply_patch(&patch, mutation)?;
                            Ok(match mutation {
                                PatchMutation::Stage => "Hunk staged",
                                PatchMutation::Unstage => "Hunk moved back to the working tree",
                                PatchMutation::Discard => "Hunk discarded",
                            }
                            .to_owned())
                        }
                        ReviewAction::Commit(message) => {
                            let commit = repository.commit(&message)?;
                            Ok(format!("Committed {} · {}", commit.oid, commit.summary))
                        }
                    }
                })
                .await
                .map_err(|error| format!("Git action worker stopped: {error}"))
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.review_action_busy = false;
                match result {
                    Ok(message) => {
                        this.review_feedback = Some((true, message));
                        this.commit_open = false;
                        this.commit_query.clear();
                    }
                    Err(message) => this.review_feedback = Some((false, message)),
                }
                this.refresh(true, cx);
                cx.notify();
            });
        }));
    }

    fn submit_commit(&mut self, cx: &mut Context<Self>) {
        let message = self.commit_query.text().trim().to_owned();
        if message.is_empty() {
            self.review_feedback = Some((false, "Write a commit message first".to_owned()));
            cx.notify();
            return;
        }
        self.run_review_action(ReviewAction::Commit(message), cx);
    }

    fn open_ask(
        &mut self,
        evidence: Vec<ReviewEvidence>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = if evidence.len() == 1 {
            evidence[0].label()
        } else {
            format!("{} review contexts", evidence.len())
        };
        self.ask_draft = Some(AskDraft { evidence, label });
        self.ask_feedback = None;
        self.ask_query.clear();
        self.ask_query
            .insert("Review this for correctness, regressions, and missing tests.");
        self.ask_query.select_all();
        self.commit_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn set_ask_question(&mut self, question: &str, cx: &mut Context<Self>) {
        self.ask_query.clear();
        self.ask_query.insert(question);
        self.ask_query.select_all();
        cx.notify();
    }

    fn submit_ask(&mut self, cx: &mut Context<Self>) {
        if self.ask_busy {
            return;
        }
        let Some(draft) = self.ask_draft.clone() else {
            return;
        };
        let question = self.ask_query.text().trim().to_owned();
        let prompt = match ReviewPrompt::compose(&draft.evidence, &question) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.ask_feedback = Some((false, error.to_string()));
                cx.notify();
                return;
            }
        };
        let Some(session) = self.selected_session() else {
            self.ask_feedback = Some((false, "Select an agent first".to_owned()));
            cx.notify();
            return;
        };

        self.ask_busy = true;
        self.ask_feedback = None;
        let subject = prompt.subject_label.clone();
        let session_id = session.id;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        self.ask_task = Some(cx.spawn(async move |this, cx| {
            let result = tokio
                .spawn(async move { client.send_text(&session_id, prompt.text, true).await })
                .await
                .map_err(|error| format!("Agent send stopped: {error}"))
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.ask_busy = false;
                match result {
                    Ok(()) => {
                        this.ask_feedback = Some((true, format!("Sent · {subject}")));
                        this.ask_query.clear();
                    }
                    Err(error) => this.ask_feedback = Some((false, error)),
                }
                cx.notify();
            });
        }));
    }

    fn selected_session(&self) -> Option<SessionRecord> {
        self.runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned()
    }

    fn markdown_document(&mut self, source: &str) -> Arc<MarkdownDocument> {
        if let Some(document) = self.markdown_cache.get(source) {
            return Arc::clone(document);
        }
        if self.markdown_cache.len() >= 24 {
            self.markdown_cache.clear();
        }
        let document = Arc::new(MarkdownDocument::parse(source));
        self.markdown_cache
            .insert(source.to_owned(), Arc::clone(&document));
        document
    }

    fn render_header(
        &self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let changes_count = match &self.state {
            LoadState::Ready(snapshot) if snapshot.files > 0 => Some(snapshot.files),
            _ => None,
        };
        let artifacts_count = session.map(artifact_count).filter(|count| *count > 0);
        let selected_tab = self.selected_tab;
        let mut tabs = div()
            .min_w(px(0.0))
            .flex_1()
            .flex()
            .items_center()
            .gap(px(2.0));

        for tab in InspectorTab::ALL {
            let count = match tab {
                InspectorTab::Info => None,
                InspectorTab::Changes => changes_count,
                InspectorTab::Code => None,
                InspectorTab::Artifacts => artifacts_count,
            };
            let active = tab == selected_tab;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("inspector-tab-{}", tab.label())))
                    .debug_selector(move || tab.debug_selector().to_owned())
                    .h(px(28.0))
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .bg(if active {
                        colors.primary.alpha(0.09)
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .hover(move |button| {
                        button.bg(colors.primary.alpha(if active { 0.11 } else { 0.055 }))
                    })
                    .text_size(px(12.0))
                    .font_weight(if active {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .text_color(if active {
                        colors.primary
                    } else {
                        colors.secondary
                    })
                    .child(tab.label())
                    // Counts are useful context once a destination is open,
                    // but four always-visible badges make the 300pt compact
                    // inspector overlap its close control.
                    .when_some(count.filter(|_| active), |tab, count| {
                        tab.child(
                            div()
                                .min_w(px(16.0))
                                .h(px(16.0))
                                .px(px(4.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(colors.primary.alpha(if active { 0.10 } else { 0.06 }))
                                .text_size(px(9.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if active {
                                    colors.secondary
                                } else {
                                    colors.tertiary
                                })
                                .child(count.to_string()),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_tab(tab, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        div()
            .h(px(Metrics::TITLE_BAR))
            .flex_none()
            .pl(px(8.0))
            .pr(px(Metrics::TOOLBAR_EDGE_INSET))
            .flex()
            .items_center()
            .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
            .child(tabs)
            .child(
                div()
                    .id("close-inspector")
                    .debug_selector(|| "INSPECTOR_CLOSE".to_owned())
                    .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .hover(move |button| button.bg(Fill::subtle(colors)))
                    .child(sf_symbol_weighted(
                        "xmark",
                        13.5,
                        SymbolWeight::Bold,
                        colors.secondary,
                    ))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(InspectorEvent::Close);
                        cx.stop_propagation();
                    })),
            )
    }

    fn render_info(
        &mut self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = session else {
            return self
                .render_message(
                    colors,
                    "sidebar.left",
                    "Select a session",
                    "Info follows the active agent.",
                )
                .into_any_element();
        };

        let (project_name, host_name) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            let project_name = store
                .projects()
                .get(&session.project_id)
                .map(|project| project.name.clone())
                .unwrap_or_else(|| folder_name(&session.cwd));
            let host_name = session
                .host
                .as_deref()
                .map(|host| store.host_display_name(host));
            (project_name, host_name)
        };
        let kind = ui_agent_kind(session.effective_kind());
        let (status_label, status_color) = session_status(session, colors);
        let artifact_total = artifact_count(session);

        let hero = div()
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .rounded(px(Radius::CARD))
            .bg(colors.primary.alpha(0.035))
            .border_1()
            .border_color(colors.primary.alpha(0.065))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(11.0))
                    .child(AgentLogo::new(kind, 36.0, colors))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(Typo::DISPLAY_TITLE.size))
                                    .font_weight(Typo::DISPLAY_TITLE.weight)
                                    .text_color(colors.primary)
                                    .child(session.title.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    .child(kind.label())
                                    .child("·")
                                    .child(project_name.clone()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .text_size(px(Typo::META.size))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(status_color)
                            .child(div().size(px(7.0)).rounded_full().bg(status_color))
                            .child(status_label),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(format!("Updated {}", relative_time(session.updated_at.0))),
                    ),
            );

        let mut content = div()
            .id("inspector-info-scroll")
            .size_full()
            .min_h(px(0.0))
            .px(px(12.0))
            .pt(px(8.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .overflow_y_scroll()
            .child(hero);

        if let Some(detail) = &session.needs_input {
            let risk_color = if detail.risk_hint == diri_proto::RiskHint::Destructive {
                Ink::DANGER
            } else {
                Ink::ATTENTION
            };
            content = content.child(
                div()
                    .p(px(12.0))
                    .flex()
                    .items_start()
                    .gap(px(9.0))
                    .rounded(px(Radius::CARD))
                    .bg(risk_color.alpha(0.10))
                    .border_1()
                    .border_color(risk_color.alpha(0.22))
                    .child(sf_symbol("questionmark.bubble", 15.0, risk_color))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(Typo::ROW_EMPHASIZED.size))
                                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                                    .text_color(colors.primary)
                                    .child("Needs your input"),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.secondary)
                                    .child(detail.summary.clone()),
                            ),
                    ),
            );
        }

        content = content
            .child(section_label("Git status", colors))
            .child(self.render_git_summary(colors, cx));

        if let Some(pull_requests) = session.pull_requests.as_deref()
            && !pull_requests.is_empty()
        {
            content = content.child(section_label(
                if pull_requests.len() == 1 {
                    "Pull request"
                } else {
                    "Pull requests"
                },
                colors,
            ));
            let inspector = cx.entity();
            for pull_request in pull_requests.iter().take(2) {
                let body = pull_request
                    .body
                    .as_deref()
                    .filter(|body| !body.trim().is_empty())
                    .map(|body| self.markdown_document(body));
                content = content.child(render_pull_request(
                    pull_request,
                    colors,
                    inspector.clone(),
                    body,
                ));
            }
        }

        if artifact_total > 0 {
            content = content.child(section_label("Artifacts", colors)).child(
                div()
                    .id("inspector-artifacts-summary")
                    .h(px(44.0))
                    .px(px(11.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(Radius::ROW))
                    .bg(colors.primary.alpha(0.035))
                    .border_1()
                    .border_color(colors.primary.alpha(0.06))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.065)))
                    .child(sf_symbol("shippingbox", 14.0, colors.secondary))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .text_size(px(Typo::ROW.size))
                            .text_color(colors.primary)
                            .child(format!(
                                "{artifact_total} {} discovered",
                                if artifact_total == 1 {
                                    "artifact"
                                } else {
                                    "artifacts"
                                }
                            )),
                    )
                    .child(sf_symbol("chevron.right", 11.0, colors.tertiary))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.select_tab(InspectorTab::Artifacts, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        let mut details = div()
            .rounded(px(Radius::CARD))
            .bg(colors.primary.alpha(0.025))
            .border_1()
            .border_color(colors.primary.alpha(0.055))
            .overflow_hidden()
            .child(detail_row("Project", project_name, false, colors))
            .child(detail_row("Directory", session.cwd.clone(), true, colors));
        if let Some(branch) = &session.git_branch {
            details = details.child(detail_row("Branch", branch.clone(), true, colors));
        }
        if let Some(host) = host_name {
            details = details.child(detail_row("Host", host, false, colors));
        }
        if let Some(bytes) = session.memory_bytes {
            details = details.child(detail_row("Memory", format_bytes(bytes), false, colors));
        }
        content
            .child(section_label("Details", colors))
            .child(details)
            .into_any_element()
    }

    fn render_git_summary(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let (symbol, title, detail, accent, can_open) = match &self.state {
            LoadState::Ready(snapshot) if snapshot.files > 0 => (
                "arrow.left.arrow.right",
                format!(
                    "{} {} changed",
                    snapshot.files,
                    if snapshot.files == 1 { "file" } else { "files" }
                ),
                format!("+{}  −{}", snapshot.additions, snapshot.deletions),
                rgba(0x4f8ef7ff),
                true,
            ),
            LoadState::Ready(snapshot) => (
                "checkmark.circle.fill",
                "No changes".to_owned(),
                format!(
                    "Matches {}",
                    snapshot
                        .base_ref
                        .as_deref()
                        .unwrap_or(match self.comparison {
                            SessionDiffBase::DefaultBranch => "default branch",
                            SessionDiffBase::Head => "HEAD",
                        })
                ),
                Ink::FRESH,
                true,
            ),
            LoadState::Loading => (
                "ellipsis.circle",
                "Reading working tree".to_owned(),
                "Git status is updating…".to_owned(),
                colors.secondary,
                false,
            ),
            LoadState::Error(error) => (
                "exclamationmark.triangle.fill",
                "Git status unavailable".to_owned(),
                error.clone(),
                Ink::ATTENTION,
                false,
            ),
            LoadState::NoSession => (
                "minus.circle",
                "No session selected".to_owned(),
                "Select an agent to inspect its working tree.".to_owned(),
                colors.tertiary,
                false,
            ),
        };
        div()
            .id("inspector-git-summary")
            .min_h(px(52.0))
            .px(px(11.0))
            .py(px(9.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(Radius::CARD))
            .bg(colors.primary.alpha(0.035))
            .border_1()
            .border_color(colors.primary.alpha(0.06))
            .when(can_open, |row| {
                row.cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.065)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.select_tab(InspectorTab::Changes, cx);
                        cx.stop_propagation();
                    }))
            })
            .child(sf_symbol(symbol, 15.0, accent))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                            .text_color(colors.primary)
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(detail),
                    ),
            )
            .when(can_open, |row| {
                row.child(sf_symbol("chevron.right", 11.0, colors.tertiary))
            })
            .into_any_element()
    }

    fn render_artifacts(
        &mut self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = session else {
            return self
                .render_message(
                    colors,
                    "sidebar.left",
                    "Select a session",
                    "Artifacts follow the active agent.",
                )
                .into_any_element();
        };
        if artifact_count(session) == 0 {
            return self
                .render_message(
                    colors,
                    "shippingbox",
                    "No artifacts yet",
                    "Pull requests, previews, Linear issues, links, and local ports appear here as they’re discovered.",
                )
                .into_any_element();
        }

        let mut content = div()
            .id("inspector-artifacts-scroll")
            .size_full()
            .min_h(px(0.0))
            .px(px(12.0))
            .pt(px(8.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .overflow_y_scroll();

        if let Some(pull_requests) = session.pull_requests.as_deref() {
            let inspector = cx.entity();
            for pull_request in pull_requests {
                let body = pull_request
                    .body
                    .as_deref()
                    .filter(|body| !body.trim().is_empty())
                    .map(|body| self.markdown_document(body));
                content = content.child(render_pull_request(
                    pull_request,
                    colors,
                    inspector.clone(),
                    body,
                ));
            }
        }
        if let Some(artifacts) = session.artifacts.as_deref() {
            for artifact in artifacts {
                let represented_by_status = artifact.kind == ArtifactKind::PullRequest
                    && session.pull_requests.as_deref().is_some_and(|statuses| {
                        statuses.iter().any(|status| status.url == artifact.url)
                    });
                if !represented_by_status {
                    content = content.child(render_artifact_row(artifact, colors));
                }
            }
        }
        if let Some(ports) = session.listening_ports.as_deref() {
            for port in ports {
                let url = format!("http://localhost:{}", port.port);
                let activation = url.clone();
                content = content.child(
                    div()
                        .id(SharedString::from(format!("inspector-port-{}", port.port)))
                        .min_h(px(54.0))
                        .px(px(11.0))
                        .py(px(9.0))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .rounded(px(Radius::ROW))
                        .bg(colors.primary.alpha(0.035))
                        .border_1()
                        .border_color(colors.primary.alpha(0.06))
                        .cursor_pointer()
                        .hover(move |row| row.bg(colors.primary.alpha(0.065)))
                        .child(artifact_icon("network", colors))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                                        .text_color(colors.primary)
                                        .child(format!("localhost:{}", port.port)),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.tertiary)
                                        .child(port.process_name.clone()),
                                ),
                        )
                        .child(sf_symbol("arrow.up.right", 11.0, colors.tertiary))
                        .on_click(move |_, _, cx| cx.open_url(&activation)),
                );
            }
        }
        content.into_any_element()
    }

    fn scrollbar_metrics(&self) -> Option<ScrollbarMetrics> {
        let base = self.scroll.0.borrow().base_handle.clone();
        let bounds = base.bounds();
        let viewport_height = f32::from(bounds.size.height);
        let max_offset = f32::from(base.max_offset().y).max(0.0);
        if max_offset <= 0.0 || viewport_height <= SCROLLBAR_MIN_THUMB {
            return None;
        }

        let track_height = (viewport_height - SCROLLBAR_INSET * 2.0).max(0.0);
        let content_height = viewport_height + max_offset;
        let thumb_height = (track_height * viewport_height / content_height)
            .max(SCROLLBAR_MIN_THUMB)
            .min(track_height);
        let thumb_travel = (track_height - thumb_height).max(0.0);
        let progress = (-f32::from(base.offset().y) / max_offset).clamp(0.0, 1.0);

        Some(ScrollbarMetrics {
            track_top: f32::from(bounds.origin.y) + SCROLLBAR_INSET,
            track_height,
            thumb_height,
            thumb_top: thumb_travel * progress,
        })
    }

    fn set_scrollbar_offset(&mut self, pointer_y: f32, cx: &mut Context<Self>) {
        let Some(metrics) = self.scrollbar_metrics() else {
            return;
        };
        let thumb_travel = (metrics.track_height - metrics.thumb_height).max(0.0);
        if thumb_travel <= 0.0 {
            return;
        }

        let thumb_top = (pointer_y - metrics.track_top - self.scrollbar_interaction.grab_offset)
            .clamp(0.0, thumb_travel);
        let base = self.scroll.0.borrow().base_handle.clone();
        let max_offset = f32::from(base.max_offset().y).max(0.0);
        let current = base.offset();
        base.set_offset(point(
            current.x,
            px(-(max_offset * thumb_top / thumb_travel)),
        ));
        cx.notify();
    }

    fn finish_scrollbar_drag(&mut self, cx: &mut Context<Self>) {
        if self.scrollbar_interaction.dragging {
            self.scrollbar_interaction.dragging = false;
            cx.notify();
        }
    }

    fn render_scrollbar(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let metrics = self.scrollbar_metrics()?;
        let dragging = self.scrollbar_interaction.dragging;

        let thumb = div()
            .id("diff-scrollbar-thumb")
            .absolute()
            .top(px(metrics.thumb_top))
            .left(px(3.0))
            .right(px(3.0))
            .h(px(metrics.thumb_height))
            .rounded(px(3.0))
            .bg(colors.primary.alpha(if dragging { 0.46 } else { 0.24 }))
            .group_hover("diff-scrollbar", move |style| {
                style.bg(colors.primary.alpha(0.40))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.scrollbar_interaction.dragging = true;
                    this.scrollbar_interaction.grab_offset =
                        (f32::from(event.position.y) - metrics.track_top - metrics.thumb_top)
                            .clamp(0.0, metrics.thumb_height);
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .on_drag(DraggedDiffScrollbar, |value, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| *value)
            });

        Some(
            div()
                .id("diff-scrollbar-track")
                .group("diff-scrollbar")
                .absolute()
                .top(px(SCROLLBAR_INSET))
                .bottom(px(SCROLLBAR_INSET))
                .right(px(2.0))
                .w(px(12.0))
                .rounded(px(6.0))
                .occlude()
                .hover(move |style| style.bg(colors.primary.alpha(0.055)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.scrollbar_interaction.dragging = true;
                        this.scrollbar_interaction.grab_offset = metrics.thumb_height / 2.0;
                        this.set_scrollbar_offset(f32::from(event.position.y), cx);
                        cx.stop_propagation();
                    }),
                )
                .on_drag(DraggedDiffScrollbar, |value, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *value)
                })
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_scrollbar_drag(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.finish_scrollbar_drag(cx)),
                )
                .child(thumb)
                .into_any_element(),
        )
    }

    fn render_diff(
        &mut self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row_count = snapshot.rows.len();
        let content_width =
            (GUTTER_WIDTH + 28.0 + snapshot.max_text_columns as f32 * 7.1).clamp(320.0, 3700.0);
        let inspector = cx.entity();
        let repo_root = snapshot.repo_root.clone();
        let armed_hunk = self.armed_hunk;
        let list = uniform_list("inspector-diff", row_count, move |range, _, _| {
            render_rows(
                &snapshot,
                range,
                content_width,
                colors,
                inspector.clone(),
                &repo_root,
                armed_hunk,
            )
        })
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&self.scroll)
        .size_full();
        let scrollbar = self.render_scrollbar(colors, cx);

        // The list's scroll bounds are available after its first layout pass.
        // Re-render once on the next frame so the fixed overlay can size itself.
        if !self.scrollbar_layout_primed {
            self.scrollbar_layout_primed = true;
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedDiffScrollbar>, _, cx| {
                    this.set_scrollbar_offset(f32::from(event.event.position.y), cx);
                },
            ))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_scrollbar_drag(cx)),
            )
            .child(list)
            .when_some(scrollbar, |body, scrollbar| body.child(scrollbar))
    }

    fn comparison_label(&self) -> String {
        if let LoadState::Ready(snapshot) = &self.state
            && let Some(base_ref) = snapshot.base_ref.as_deref()
        {
            return base_ref.to_owned();
        }
        match self.comparison {
            SessionDiffBase::DefaultBranch => "default branch".to_owned(),
            SessionDiffBase::Head => "HEAD".to_owned(),
        }
    }

    fn render_comparison_option(
        &self,
        comparison: SessionDiffBase,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, detail, selector) = match comparison {
            SessionDiffBase::DefaultBranch => (
                "Default branch",
                "Committed and working changes",
                "INSPECTOR_COMPARE_DEFAULT",
            ),
            SessionDiffBase::Head => ("HEAD", "Uncommitted changes only", "INSPECTOR_COMPARE_HEAD"),
        };
        let selected = self.comparison == comparison;
        div()
            .id(SharedString::from(format!("compare-option-{title}")))
            .debug_selector(move || selector.to_owned())
            .min_h(px(48.0))
            .px(px(10.0))
            .py(px(7.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .cursor_pointer()
            .bg(if selected {
                colors.primary.alpha(0.075)
            } else {
                colors.primary.alpha(0.0)
            })
            .hover(move |row| row.bg(colors.primary.alpha(0.09)))
            .child(
                div()
                    .w(px(14.0))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .when(selected, |slot| {
                        slot.child(sf_symbol("checkmark", 10.5, colors.primary))
                    }),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(Typo::ROW.size))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(detail),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_comparison(comparison, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_layer_option(
        &self,
        layer: DiffLayer,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, selector) = match layer {
            DiffLayer::Branch => ("Branch", "INSPECTOR_LAYER_BRANCH"),
            DiffLayer::Working => ("Working", "INSPECTOR_LAYER_WORKING"),
            DiffLayer::Staged => ("Staged", "INSPECTOR_LAYER_STAGED"),
        };
        let selected = self.diff_layer == layer;
        div()
            .id(SharedString::from(format!("review-layer-{label}")))
            .debug_selector(move || selector.to_owned())
            .h(px(25.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(Radius::CHIP))
            .bg(if selected {
                colors.primary.alpha(0.11)
            } else {
                colors.primary.alpha(0.0)
            })
            .cursor_pointer()
            .hover(move |button| button.bg(colors.primary.alpha(0.085)))
            .text_size(px(9.5))
            .font_weight(if selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(if selected {
                colors.primary
            } else {
                colors.tertiary
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_diff_layer(layer, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_file_navigator(
        &self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut files = div()
            .id("review-file-navigator-list")
            .max_h(px(390.0))
            .py(px(4.0))
            .overflow_y_scroll();
        for (index, file) in snapshot.file_diffs.iter().enumerate() {
            let row = file.row_range.start;
            files = files.child(
                div()
                    .id(("review-file-navigator-row", index))
                    .debug_selector(move || format!("INSPECTOR_REVIEW_FILE_{index}"))
                    .min_h(px(38.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .hover(move |item| item.bg(colors.primary.alpha(0.065)))
                    .child(sf_symbol(
                        "chevron.left.forwardslash.chevron.right",
                        11.5,
                        colors.tertiary,
                    ))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(10.0))
                            .text_color(colors.secondary)
                            .child(file.path.to_string_lossy().into_owned()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(9.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Ink::FRESH)
                            .child(format!("+{}", file.additions)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(9.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Ink::DANGER)
                            .child(format!("−{}", file.deletions)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_diff_row(row, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        div()
            .absolute()
            .inset_0()
            .child(div().absolute().inset_0().occlude().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.files_open = false;
                    cx.notify();
                    cx.stop_propagation();
                }),
            ))
            .child(
                div()
                    .id("review-file-navigator")
                    .debug_selector(|| "INSPECTOR_FILE_NAVIGATOR".to_owned())
                    .absolute()
                    .top(px(40.0))
                    .right(px(9.0))
                    .w(px(330.0))
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.files_open = false;
                        cx.notify();
                    }))
                    .child(FloatingSurface::new(colors, files)),
            )
            .into_any_element()
    }

    fn render_review_controls(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ReviewLoadState::Ready(status) = &self.review_state else {
            let (symbol, label) = match &self.review_state {
                ReviewLoadState::NoSession => ("minus.circle", "Select an agent to review"),
                ReviewLoadState::Remote => (
                    "network",
                    "Remote changes are view-only until Git actions move into the daemon",
                ),
                ReviewLoadState::Loading => ("ellipsis", "Reading index and working tree…"),
                ReviewLoadState::Error(error) => {
                    return div()
                        .px(px(10.0))
                        .py(px(7.0))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .border_b_1()
                        .border_color(colors.primary.alpha(0.06))
                        .text_size(px(Typo::META.size))
                        .text_color(Ink::ATTENTION)
                        .child(sf_symbol("exclamationmark.triangle", 11.0, Ink::ATTENTION))
                        .child(error.clone())
                        .into_any_element();
                }
                ReviewLoadState::Ready(_) => unreachable!(),
            };
            return div()
                .h(px(36.0))
                .flex_none()
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(sf_symbol(symbol, 11.0, colors.tertiary))
                .child(label)
                .into_any_element();
        };
        let status = Arc::clone(status);
        let staged_paths: Vec<_> = status
            .staged
            .iter()
            .map(|change| change.path.clone())
            .collect();
        // Conflicted paths are deliberately excluded. `git add` on a file that
        // still carries conflict markers both stages the markers and collapses
        // index stages 1/2/3, after which `git checkout --merge` can no longer
        // reconstruct the conflict. Resolving stays an explicit, per-file act.
        let mut stage_paths: Vec<_> = status
            .unstaged
            .iter()
            .chain(status.untracked.iter())
            .map(|change| change.path.clone())
            .collect();
        stage_paths.sort();
        stage_paths.dedup();
        let discard_paths: Vec<_> = status
            .unstaged
            .iter()
            .map(|change| change.path.clone())
            .collect();
        let staged_count = status.staged.len();
        let working_count = status.unstaged.len() + status.untracked.len();
        let conflicted_count = status.conflicted.len();
        let branch = status
            .branch
            .name
            .clone()
            .unwrap_or_else(|| "Detached HEAD".to_owned());
        let busy = self.review_action_busy;
        let commit_open = self.commit_open;
        let discard_armed = self.discard_armed;

        let mut actions = div().flex().items_center().gap(px(5.0));
        if self.diff_layer == DiffLayer::Working && !stage_paths.is_empty() {
            let paths = stage_paths;
            actions = actions.child(
                div()
                    .id("review-stage-all")
                    .h(px(25.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .bg(if staged_count == 0 {
                        rgba(0xd9775724)
                    } else {
                        colors.primary.alpha(0.055)
                    })
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if staged_count == 0 {
                        rgba(0xe89a7cff)
                    } else {
                        colors.secondary
                    })
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_review_action(ReviewAction::Stage(paths.clone()), cx);
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol("plus", 9.0, colors.secondary))
                    .child("Stage all"),
            );
        }
        if self.diff_layer == DiffLayer::Staged && !staged_paths.is_empty() {
            let paths = staged_paths;
            actions = actions.child(
                div()
                    .id("review-unstage-all")
                    .h(px(25.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::BADGE))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.tertiary)
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_review_action(ReviewAction::Unstage(paths.clone()), cx);
                                cx.stop_propagation();
                            }))
                    })
                    .child("Unstage"),
            );
            actions = actions.child(
                div()
                    .id("review-open-commit")
                    .h(px(25.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .bg(rgba(0xd9775730))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgba(0xf0aa8fff))
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(|button| button.bg(rgba(0xd9775744)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_open = !this.commit_open;
                                this.discard_armed = false;
                                this.ask_draft = None;
                                this.ask_feedback = None;
                                this.ask_query.clear();
                                if this.commit_open {
                                    window.focus(&this.focus, cx);
                                }
                                cx.notify();
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol("checkmark", 9.5, rgba(0xf0aa8fff)))
                    .child(if commit_open { "Cancel" } else { "Commit" }),
            );
        }
        if self.diff_layer == DiffLayer::Working && !discard_paths.is_empty() {
            let paths = discard_paths;
            actions = actions.child(
                div()
                    .id("review-discard-all")
                    .h(px(25.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .rounded(px(Radius::BADGE))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if discard_armed {
                        Ink::DANGER
                    } else {
                        colors.tertiary
                    })
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(Ink::DANGER.alpha(0.09)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.discard_armed {
                                    this.run_review_action(
                                        ReviewAction::Discard(paths.clone()),
                                        cx,
                                    );
                                } else {
                                    this.discard_armed = true;
                                    this.commit_open = false;
                                    cx.notify();
                                }
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol(
                        "trash",
                        9.5,
                        if discard_armed {
                            Ink::DANGER
                        } else {
                            colors.tertiary
                        },
                    ))
                    .child(if discard_armed { "Discard?" } else { "Discard" }),
            );
        }

        let branch_detail = match (status.branch.ahead, status.branch.behind) {
            (0, 0) => None,
            (ahead, 0) => Some(format!("↑{ahead}")),
            (0, behind) => Some(format!("↓{behind}")),
            (ahead, behind) => Some(format!("↑{ahead} ↓{behind}")),
        };
        let counts = format!(
            "{staged_count} staged · {working_count} working{}",
            if conflicted_count > 0 {
                format!(" · {conflicted_count} conflicted")
            } else {
                String::new()
            }
        );
        let mut panel = div()
            .flex_none()
            .px(px(10.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .border_b_1()
            .border_color(colors.primary.alpha(0.06))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(sf_symbol("arrow.branch", 11.0, colors.secondary))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.secondary)
                            .child(branch),
                    )
                    .when_some(branch_detail, |row, detail| {
                        row.child(
                            div()
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(9.5))
                                .text_color(colors.tertiary)
                                .child(detail),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(if conflicted_count > 0 {
                                Ink::DANGER
                            } else {
                                colors.tertiary
                            })
                            .child(counts),
                    ),
            )
            .when(self.diff_layer != DiffLayer::Branch, |panel| {
                panel.child(actions)
            })
            .when(self.diff_layer == DiffLayer::Branch, |panel| {
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(9.5))
                        .text_color(colors.tertiary)
                        .child(sf_symbol("scope", 9.5, colors.tertiary))
                        .child("Overview only · choose Working or Staged to mutate hunks"),
                )
            });

        if self.commit_open {
            let empty = self.commit_query.is_empty();
            panel = panel.child(
                div()
                    .p(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .rounded(px(Radius::ROW))
                    .bg(colors.primary.alpha(0.035))
                    .border_1()
                    .border_color(colors.primary.alpha(0.08))
                    .child(
                        div()
                            .id("review-commit-message")
                            .min_w(px(0.0))
                            .h(px(27.0))
                            .flex_1()
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::CHIP))
                            .bg(colors.background)
                            .border_1()
                            .border_color(colors.primary.alpha(0.10))
                            .cursor_text()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(10.5))
                            .text_color(colors.primary)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(if empty {
                                div()
                                    .text_color(colors.tertiary)
                                    .child("Commit message…")
                                    .into_any_element()
                            } else {
                                crate::navigation::query_label(&self.commit_query)
                            }),
                    )
                    .child(
                        div()
                            .id("review-submit-commit")
                            .h(px(27.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::CHIP))
                            .bg(if empty {
                                colors.primary.alpha(0.035)
                            } else {
                                rgba(0xd9775730)
                            })
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if empty {
                                colors.primary.alpha(0.25)
                            } else {
                                rgba(0xf0aa8fff)
                            })
                            .when(!empty && !busy, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgba(0xd9775744)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_commit(cx);
                                        cx.stop_propagation();
                                    }))
                            })
                            .child("Commit"),
                    ),
            );
        }
        if let Some((success, message)) = &self.review_feedback {
            let accent = if *success { Ink::FRESH } else { Ink::DANGER };
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(10.0))
                    .text_color(accent)
                    .child(sf_symbol(
                        if *success {
                            "checkmark.circle.fill"
                        } else {
                            "exclamationmark.circle.fill"
                        },
                        10.5,
                        accent,
                    ))
                    .child(message.clone()),
            );
        }
        let _ = window;
        panel.into_any_element()
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ask_draft.is_some() {
            match event.keystroke.key.as_str() {
                "escape" => {
                    self.ask_draft = None;
                    self.ask_feedback = None;
                    self.ask_query.clear();
                    cx.notify();
                }
                "enter" => self.submit_ask(cx),
                _ => {
                    let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                        return;
                    };
                    match edit {
                        Edit::Local(local) => {
                            self.ask_query.apply(local);
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&self.ask_query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(&mut self.ask_query, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                self.ask_query.insert(&text);
                            }
                        }
                    }
                    cx.notify();
                }
            }
            cx.stop_propagation();
            return;
        }
        if !self.commit_open {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.commit_open = false;
                cx.notify();
            }
            "enter" => self.submit_commit(cx),
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return;
                };
                match edit {
                    Edit::Local(local) => {
                        self.commit_query.apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(&self.commit_query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(&mut self.commit_query, cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            self.commit_query.insert(&text);
                        }
                    }
                }
                cx.notify();
            }
        }
        cx.stop_propagation();
    }

    fn render_changes(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self.comparison_label();
        let remote = self.context.as_ref().is_some_and(|context| context.remote);
        let empty_detail = match self.diff_layer {
            DiffLayer::Branch => format!("This branch matches {label}."),
            DiffLayer::Working => "The working tree matches the index.".to_owned(),
            DiffLayer::Staged => "The index matches HEAD.".to_owned(),
        };
        let body = match self.state.clone() {
            LoadState::Ready(snapshot) if snapshot.rows.is_empty() => self
                .render_message(colors, "checkmark.circle", "No changes", empty_detail)
                .into_any_element(),
            LoadState::Ready(snapshot) => self
                .render_diff(snapshot, colors, window, cx)
                .into_any_element(),
            LoadState::Loading => self
                .render_message(
                    colors,
                    "ellipsis",
                    "Loading changes",
                    "Reading the working tree…",
                )
                .into_any_element(),
            LoadState::NoSession => self
                .render_message(
                    colors,
                    "sidebar.left",
                    "Select a session",
                    "Changes follow the active agent.",
                )
                .into_any_element(),
            LoadState::Error(error) => self
                .render_message(
                    colors,
                    "exclamationmark.triangle",
                    "Couldn't load changes",
                    error,
                )
                .into_any_element(),
        };
        let comparison_open = remote && self.comparison_menu_open;
        let snapshot = match &self.state {
            LoadState::Ready(snapshot) => Some(Arc::clone(snapshot)),
            _ => None,
        };
        let file_count = snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.file_diffs.len());
        let menu = if !remote && self.files_open {
            snapshot.map(|snapshot| self.render_file_navigator(snapshot, colors, cx))
        } else if comparison_open {
            Some(
                div()
                    .absolute()
                    .inset_0()
                    .child(div().absolute().inset_0().occlude().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.comparison_menu_open = false;
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    ))
                    .child(
                        div()
                            .id("inspector-comparison-menu")
                            .absolute()
                            .top(px(40.0))
                            .right(px(10.0))
                            .w(px(230.0))
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.comparison_menu_open = false;
                                cx.notify();
                            }))
                            .child(FloatingSurface::new(
                                colors,
                                div()
                                    .py(px(4.0))
                                    .rounded(px(Radius::PANEL))
                                    .overflow_hidden()
                                    .child(self.render_comparison_option(
                                        SessionDiffBase::DefaultBranch,
                                        colors,
                                        cx,
                                    ))
                                    .child(self.render_comparison_option(
                                        SessionDiffBase::Head,
                                        colors,
                                        cx,
                                    )),
                            )),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

        let toolbar = if remote {
            div()
                .h(px(38.0))
                .flex_none()
                .px(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.tertiary)
                        .child("Remote branch"),
                )
                .child(
                    div()
                        .id("inspector-comparison-button")
                        .debug_selector(|| "INSPECTOR_COMPARE_BUTTON".to_owned())
                        .max_w(px(184.0))
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors
                            .primary
                            .alpha(if comparison_open { 0.10 } else { 0.055 }))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                        .child(sf_symbol("arrow.branch", 11.0, colors.secondary))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(Typo::META.size))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.secondary)
                                .child(format!("vs {label}")),
                        )
                        .child(sf_symbol("chevron.down", 9.0, colors.tertiary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.comparison_menu_open = !this.comparison_menu_open;
                            cx.notify();
                            cx.stop_propagation();
                        })),
                )
                .into_any_element()
        } else {
            div()
                .h(px(38.0))
                .flex_none()
                .px(px(9.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .child(
                    div()
                        .h(px(29.0))
                        .p(px(2.0))
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors.primary.alpha(0.035))
                        .border_1()
                        .border_color(colors.primary.alpha(0.055))
                        .child(self.render_layer_option(DiffLayer::Branch, colors, cx))
                        .child(self.render_layer_option(DiffLayer::Working, colors, cx))
                        .child(self.render_layer_option(DiffLayer::Staged, colors, cx)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("review-files-button")
                        .debug_selector(|| "INSPECTOR_REVIEW_FILES".to_owned())
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors
                            .primary
                            .alpha(if self.files_open { 0.10 } else { 0.045 }))
                        .text_size(px(9.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if file_count > 0 {
                            colors.secondary
                        } else {
                            colors.primary.alpha(0.28)
                        })
                        .when(file_count > 0, |button| {
                            button
                                .cursor_pointer()
                                .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.files_open = !this.files_open;
                                    cx.notify();
                                    cx.stop_propagation();
                                }))
                        })
                        .child(sf_symbol("list.bullet", 10.5, colors.tertiary))
                        .child(format!("{file_count} files"))
                        .child(sf_symbol("chevron.down", 8.5, colors.tertiary)),
                )
                .into_any_element()
        };

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(self.render_review_controls(colors, window, cx))
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(body))
            .when_some(menu, |panel, menu| panel.child(menu))
            .into_any_element()
    }

    fn render_ask_preset(
        &self,
        id: &'static str,
        label: &'static str,
        question: &'static str,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .h(px(21.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .rounded_full()
            .bg(colors.primary.alpha(0.045))
            .border_1()
            .border_color(colors.primary.alpha(0.065))
            .cursor_pointer()
            .hover(move |button| button.bg(colors.primary.alpha(0.085)))
            .text_size(px(9.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(colors.secondary)
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_ask_question(question, cx);
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_ask_composer(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let draft = self.ask_draft.as_ref()?;
        let empty = self.ask_query.text().trim().is_empty();
        let busy = self.ask_busy;
        let label = draft.label.clone();

        let mut composer = div()
            .id("inspector-ask-composer")
            .debug_selector(|| "INSPECTOR_ASK_COMPOSER".to_owned())
            .flex_none()
            .px(px(11.0))
            .py(px(9.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .border_t_1()
            .border_color(colors.primary.alpha(0.09))
            .bg(rgba(0x17191ef8))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(sf_symbol_weighted(
                        "sparkles",
                        11.5,
                        SymbolWeight::Semibold,
                        rgba(0xe9a381ff),
                    ))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.primary)
                            .child("Ask active agent"),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .text_size(px(9.5))
                            .text_color(colors.tertiary)
                            .child(label),
                    )
                    .child(
                        div()
                            .id("inspector-ask-close")
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::CHIP))
                            .cursor_pointer()
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .child(sf_symbol("xmark", 9.5, colors.tertiary))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ask_draft = None;
                                this.ask_feedback = None;
                                this.ask_query.clear();
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(self.render_ask_preset(
                        "ask-preset-review",
                        "Review",
                        "Review this for correctness, regressions, and missing tests.",
                        colors,
                        cx,
                    ))
                    .child(self.render_ask_preset(
                        "ask-preset-risks",
                        "Find risks",
                        "Find the highest-risk behavior changes and explain why they matter.",
                        colors,
                        cx,
                    ))
                    .child(self.render_ask_preset(
                        "ask-preset-tests",
                        "Suggest tests",
                        "Identify missing tests and propose concrete cases for this context.",
                        colors,
                        cx,
                    )),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .id("inspector-ask-input")
                            .min_w(px(0.0))
                            .h_full()
                            .flex_1()
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::BADGE))
                            .bg(colors.primary.alpha(0.045))
                            .border_1()
                            .border_color(colors.primary.alpha(0.075))
                            .text_size(px(10.5))
                            .text_color(colors.primary)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(if empty {
                                div()
                                    .text_color(colors.tertiary)
                                    .child("Ask a follow-up…")
                                    .into_any_element()
                            } else {
                                crate::navigation::query_label(&self.ask_query)
                            }),
                    )
                    .child(
                        div()
                            .id("inspector-ask-send")
                            .debug_selector(|| "INSPECTOR_ASK_SEND".to_owned())
                            .h_full()
                            .px(px(11.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(Radius::BADGE))
                            .bg(if empty || busy {
                                colors.primary.alpha(0.04)
                            } else {
                                rgba(0xd97757d9)
                            })
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if empty || busy {
                                colors.primary.alpha(0.28)
                            } else {
                                rgba(0xffffffff)
                            })
                            .when(!empty && !busy, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|button| button.bg(rgba(0xe38563ff)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_ask(cx);
                                        cx.stop_propagation();
                                    }))
                            })
                            .child(if busy { "Sending…" } else { "Send" })
                            .child(sf_symbol(
                                "arrow.up",
                                9.0,
                                if empty || busy {
                                    colors.primary.alpha(0.28)
                                } else {
                                    rgba(0xffffffff)
                                },
                            )),
                    ),
            );
        if let Some((success, message)) = &self.ask_feedback {
            let accent = if *success { Ink::FRESH } else { Ink::DANGER };
            composer = composer.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(9.5))
                    .text_color(accent)
                    .child(sf_symbol(
                        if *success {
                            "checkmark.circle.fill"
                        } else {
                            "exclamationmark.circle.fill"
                        },
                        10.0,
                        accent,
                    ))
                    .child(message.clone()),
            );
        }
        Some(composer.into_any_element())
    }

    fn render_message(
        &self,
        colors: SemanticColors,
        symbol: &'static str,
        title: &'static str,
        body: impl Into<SharedString>,
    ) -> impl IntoElement {
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
                    .text_color(colors.primary.alpha(0.86))
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(280.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(body.into()),
            )
    }
}

impl Render for WorkbenchInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = SemanticColors::sidebar(diri_ui::Appearance::from_window(window.appearance()));
        let session = self.selected_session();
        let body = match self.selected_tab {
            InspectorTab::Info => self.render_info(session.as_ref(), colors, cx),
            InspectorTab::Artifacts => self.render_artifacts(session.as_ref(), colors, cx),
            InspectorTab::Changes => self.render_changes(colors, window, cx),
            InspectorTab::Code => self.code_viewer.clone().into_any_element(),
        };
        let transition_id = SharedString::from(format!(
            "inspector-tab-transition-{}",
            self.tab_transition_generation
        ));
        let direction = self.tab_direction;
        let ask_composer = self.render_ask_composer(colors, cx);
        div()
            .id("workbench-inspector")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            .child(self.render_header(session.as_ref(), colors, cx))
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(
                div().relative().size_full().child(body).with_animation(
                    transition_id,
                    Animation::new(Duration::from_millis(190)).with_easing(ease_out_quint()),
                    move |body, delta| {
                        body.left(px(direction * (1.0 - delta) * 8.0))
                            .opacity(0.70 + 0.30 * delta)
                    },
                ),
            ))
            .when_some(ask_composer, |panel, composer| panel.child(composer))
    }
}

fn section_label(label: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .px(px(2.0))
        .text_size(px(Typo::SECTION_HEADER.size))
        .font_weight(Typo::SECTION_HEADER.weight)
        .text_color(colors.tertiary)
        .child(label)
        .into_any_element()
}

fn detail_row(
    label: &'static str,
    value: String,
    monospaced: bool,
    colors: SemanticColors,
) -> AnyElement {
    div()
        .min_h(px(38.0))
        .px(px(11.0))
        .flex()
        .items_center()
        .gap(px(12.0))
        .border_b_1()
        .border_color(colors.primary.alpha(0.05))
        .child(
            div()
                .w(px(64.0))
                .flex_none()
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(label),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .truncate()
                .when(monospaced, |value| {
                    value.font_family(crate::fonts::mono_family())
                })
                .text_size(px(if monospaced {
                    Typo::META_MONO.size
                } else {
                    Typo::META.size
                }))
                .text_color(colors.secondary)
                .child(value),
        )
        .into_any_element()
}

fn render_pull_request(
    pull_request: &PullRequestStatus,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    body: Option<Arc<MarkdownDocument>>,
) -> AnyElement {
    let number = if pull_request.number > 0 {
        format!("PR #{}", pull_request.number)
    } else {
        "Pull request".to_owned()
    };
    let title = pull_request.title.clone().unwrap_or_else(|| number.clone());
    let author = pull_request.author.as_deref().unwrap_or("contributor");
    let (state_label, state_color) = pull_request_state(pull_request, colors);
    let checks_total =
        pull_request.checks_passed + pull_request.checks_failed + pull_request.checks_pending;
    let discussion_total = pull_request.comment_count + pull_request.review_count;
    let can_merge = pull_request_can_merge(pull_request);
    let view_url = pull_request.url.clone();
    let merge_url = pull_request.url.clone();
    let checks = sorted_pr_checks(pull_request);
    let discussion = pull_request.discussion.as_deref().unwrap_or_default();
    let ask_evidence = ReviewEvidence::PullRequest {
        url: pull_request.url.clone(),
        title: title.clone(),
        body: body.as_ref().map(|document| document.plain_text()),
        base: pull_request.base_ref_name.clone(),
        head: pull_request.head_ref_name.clone(),
    };
    let ask_inspector = inspector.clone();

    let mut surface = div()
        .id(SharedString::from(format!(
            "inspector-pr-{}",
            pull_request.url
        )))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .rounded(px(Radius::CARD))
        .bg(colors.primary.alpha(0.022))
        .border_1()
        .border_color(colors.primary.alpha(0.075))
        .overflow_hidden()
        .child(
            div()
                .p(px(13.0))
                .pb(px(12.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(9.0))
                        .child(
                            div()
                                .size(px(30.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(state_color.alpha(0.12))
                                .child(sf_symbol_weighted(
                                    "arrow.triangle.pull",
                                    13.0,
                                    SymbolWeight::Semibold,
                                    state_color,
                                )),
                        )
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(
                                    div()
                                        .line_height(px(17.0))
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(colors.primary)
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.tertiary)
                                        .child(format!("{author} opened {number}")),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "inspector-pr-ask-{}",
                                    pull_request.number
                                )))
                                .debug_selector(|| "INSPECTOR_PR_ASK".to_owned())
                                .h(px(24.0))
                                .px(px(8.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .rounded(px(Radius::CHIP))
                                .bg(rgba(0xd9775717))
                                .cursor_pointer()
                                .hover(|button| button.bg(rgba(0xd9775728)))
                                .text_size(px(9.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgba(0xe9a381ff))
                                .child(sf_symbol("sparkles", 9.5, rgba(0xe9a381ff)))
                                .child("Ask")
                                .on_click(move |_, window, cx| {
                                    ask_inspector.update(cx, |inspector, cx| {
                                        inspector.open_ask(vec![ask_evidence.clone()], window, cx);
                                    });
                                    cx.stop_propagation();
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .px(px(7.0))
                                .h(px(21.0))
                                .flex()
                                .items_center()
                                .rounded_full()
                                .bg(state_color.alpha(0.12))
                                .text_size(px(10.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(state_color)
                                .child(state_label),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "inspector-pr-open-{}",
                                    pull_request.number
                                )))
                                .debug_selector(|| "INSPECTOR_PR_OPEN".to_owned())
                                .size(px(24.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::CHIP))
                                .cursor_pointer()
                                .hover(move |button| button.bg(colors.primary.alpha(0.06)))
                                .child(sf_symbol("arrow.up.right", 10.5, colors.tertiary))
                                .on_click(move |_, _, cx| cx.open_url(&view_url)),
                        ),
                )
                .when(
                    pull_request.head_ref_name.is_some() || pull_request.base_ref_name.is_some(),
                    |header| {
                        let head = pull_request
                            .head_ref_name
                            .clone()
                            .unwrap_or_else(|| "head".to_owned());
                        let base = pull_request
                            .base_ref_name
                            .clone()
                            .unwrap_or_else(|| "base".to_owned());
                        header.child(
                            div()
                                .h(px(24.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(branch_badge(base, colors))
                                .child(sf_symbol("arrow.left", 9.5, colors.tertiary))
                                .child(branch_badge(head, colors)),
                        )
                    },
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(diff_stat(
                            format!("+{}", pull_request.additions),
                            Ink::FRESH,
                        ))
                        .child(diff_stat(
                            format!("−{}", pull_request.deletions),
                            Ink::DANGER,
                        ))
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.tertiary)
                                .child(format!(
                                    "{} changed {}",
                                    pull_request.changed_files,
                                    if pull_request.changed_files == 1 {
                                        "file"
                                    } else {
                                        "files"
                                    }
                                )),
                        )
                        .when_some(pull_request.total_threads, |stats, total| {
                            stats.child(
                                div()
                                    .ml_auto()
                                    .text_size(px(10.5))
                                    .text_color(colors.tertiary)
                                    .child(format!(
                                        "{}/{} resolved",
                                        pull_request.resolved_threads.unwrap_or(0),
                                        total
                                    )),
                            )
                        }),
                )
                .when_some(body, |header, body| {
                    header.child(
                        div()
                            .mt(px(1.0))
                            .p(px(11.0))
                            .rounded(px(Radius::BADGE))
                            .bg(colors.primary.alpha(0.035))
                            .border_1()
                            .border_color(colors.primary.alpha(0.055))
                            .child(render_markdown(&body, colors)),
                    )
                }),
        );

    if checks_total > 0 {
        let (checks_label, checks_color) = checks_rollup(pull_request);
        let mut check_rows = div()
            .rounded(px(Radius::BADGE))
            .border_1()
            .border_color(colors.primary.alpha(0.07))
            .overflow_hidden();
        for (index, check) in checks.iter().enumerate() {
            check_rows = check_rows.child(render_pr_check(
                check,
                index,
                checks.len(),
                pull_request.number,
                colors,
                inspector.clone(),
            ));
        }
        surface = surface.child(
            div()
                .px(px(13.0))
                .flex()
                .flex_col()
                .gap(px(7.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(section_label("Checks", colors))
                        .child(
                            div()
                                .ml_auto()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(checks_color)
                                .child(checks_label),
                        ),
                )
                .child(check_rows),
        );
    }

    if discussion_total > 0 {
        let mut conversation = div().px(px(13.0)).flex().flex_col().gap(px(8.0)).child(
            div()
                .flex()
                .items_center()
                .child(section_label("Conversation", colors))
                .child(
                    div()
                        .ml_auto()
                        .text_size(px(10.5))
                        .text_color(colors.tertiary)
                        .child(format!("{discussion_total} items")),
                ),
        );
        if discussion.is_empty() {
            conversation = conversation.child(render_discussion_fallback(pull_request, colors));
        } else {
            for (index, item) in discussion.iter().enumerate() {
                conversation = conversation.child(render_discussion_item(
                    item,
                    index,
                    discussion.len(),
                    pull_request.number,
                    colors,
                ));
            }
        }
        surface = surface.child(conversation);
    }

    if pull_request.state == "OPEN" {
        let (merge_detail, merge_color) = if can_merge {
            ("Ready to merge", Ink::FRESH)
        } else {
            (merge_blocker_label(pull_request), Ink::ATTENTION)
        };
        surface = surface.child(
            div()
                .mt(px(1.0))
                .p(px(13.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .border_t_1()
                .border_color(colors.primary.alpha(0.07))
                .bg(merge_color.alpha(0.045))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .child(merge_detail),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(colors.tertiary)
                                .child("Review and confirm on GitHub"),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "inspector-pr-merge-{}",
                            pull_request.number
                        )))
                        .debug_selector(|| "INSPECTOR_PR_MERGE".to_owned())
                        .h(px(30.0))
                        .px(px(10.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(Radius::BADGE))
                        .cursor_pointer()
                        .bg(merge_color.alpha(if can_merge { 0.86 } else { 0.13 }))
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if can_merge {
                            rgba(0xffffffff)
                        } else {
                            merge_color
                        })
                        .hover(move |button| {
                            button.bg(merge_color.alpha(if can_merge { 1.0 } else { 0.19 }))
                        })
                        .child("Merge pull request")
                        .child(sf_symbol(
                            "arrow.up.right",
                            9.0,
                            if can_merge {
                                rgba(0xffffffff)
                            } else {
                                merge_color
                            },
                        ))
                        .on_click(move |_, _, cx| cx.open_url(&merge_url)),
                ),
        );
    }

    surface.pb(px(13.0)).into_any_element()
}

fn branch_badge(branch: String, colors: SemanticColors) -> AnyElement {
    div()
        .min_w(px(0.0))
        .max_w(px(158.0))
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .bg(colors.primary.alpha(0.045))
        .font_family(crate::fonts::mono_family())
        .text_size(px(10.0))
        .text_color(colors.secondary)
        .truncate()
        .child(branch)
        .into_any_element()
}

fn diff_stat(label: String, color: gpui::Rgba) -> AnyElement {
    div()
        .px(px(7.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .bg(color.alpha(0.09))
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(label)
        .into_any_element()
}

fn render_pr_check(
    check: &PrCheck,
    index: usize,
    total: usize,
    pr_number: i64,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
) -> AnyElement {
    let (symbol, color, status) = match check.result.as_str() {
        "pass" => ("checkmark.circle.fill", Ink::FRESH, "Passed"),
        "fail" => ("xmark.circle.fill", Ink::DANGER, "Failed"),
        "pending" => ("clock.fill", Ink::ATTENTION, "Running"),
        _ => ("circle", colors.tertiary, "Unknown"),
    };
    let detail = check
        .detail
        .as_deref()
        .map(humanize_github_state)
        .filter(|detail| detail != status)
        .unwrap_or_else(|| status.to_owned());
    let url = check.url.clone();
    let ask_evidence = ReviewEvidence::Check {
        name: check.name.clone(),
        result: check.result.clone(),
        detail: check.detail.clone(),
    };
    div()
        .id(SharedString::from(format!(
            "inspector-pr-{pr_number}-check-{index}"
        )))
        .debug_selector(move || format!("INSPECTOR_PR_CHECK_{index}"))
        .min_h(px(34.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .bg(colors.primary.alpha(if check.result == "pending" {
            0.025
        } else {
            0.0
        }))
        .when(index + 1 < total, |row| {
            row.border_b_1().border_color(colors.primary.alpha(0.055))
        })
        .when(url.is_some(), |row| {
            row.cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.045)))
        })
        .child(sf_symbol(symbol, 12.0, color))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .truncate()
                .text_size(px(Typo::META.size))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.secondary)
                .child(check.name.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(10.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .child(detail),
        )
        .child(
            div()
                .id(("ask-pr-check", index))
                .size(px(20.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::CHIP))
                .cursor_pointer()
                .hover(move |button| button.bg(rgba(0xd9775722)))
                .child(sf_symbol("sparkles", 8.5, rgba(0xe9a381ff)))
                .on_click(move |_, window, cx| {
                    inspector.update(cx, |inspector, cx| {
                        inspector.open_ask(vec![ask_evidence.clone()], window, cx);
                    });
                    cx.stop_propagation();
                }),
        )
        .when_some(url, |row, url| {
            row.child(sf_symbol("arrow.up.right", 9.0, colors.tertiary))
                .on_click(move |_, _, cx| cx.open_url(&url))
        })
        .into_any_element()
}

fn render_discussion_item(
    item: &PrDiscussionItem,
    index: usize,
    total: usize,
    pr_number: i64,
    colors: SemanticColors,
) -> AnyElement {
    let author = item.author.clone();
    let initial = author
        .chars()
        .next()
        .map(|character| character.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_owned());
    let is_review = item.kind == "review";
    let (review_label, review_color) = discussion_state(item, colors);
    let body = MarkdownDocument::parse(&item.body);
    let body_fallback = if item.body.trim().is_empty() {
        review_label
            .clone()
            .unwrap_or_else(|| "Commented".to_owned())
    } else {
        String::new()
    };
    let time = item.created_at.as_ref().map(|date| relative_time(date.0));
    let url = item.url.clone();

    div()
        .id(SharedString::from(format!(
            "inspector-pr-{pr_number}-comment-{index}"
        )))
        .debug_selector(move || format!("INSPECTOR_PR_COMMENT_{index}"))
        .flex()
        .items_stretch()
        .gap(px(8.0))
        .child(
            div()
                .w(px(26.0))
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .child(
                    div()
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(if is_review {
                            review_color.alpha(0.13)
                        } else {
                            colors.primary.alpha(0.075)
                        })
                        .text_size(px(9.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if is_review {
                            review_color
                        } else {
                            colors.secondary
                        })
                        .child(initial),
                )
                .when(index + 1 < total, |rail| {
                    rail.child(
                        div()
                            .mt(px(4.0))
                            .w(px(1.0))
                            .flex_1()
                            .min_h(px(10.0))
                            .bg(colors.primary.alpha(0.08)),
                    )
                }),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "inspector-pr-{pr_number}-comment-card-{index}"
                )))
                .min_w(px(0.0))
                .flex_1()
                .mb(px(if index + 1 < total { 2.0 } else { 0.0 }))
                .rounded(px(Radius::BADGE))
                .border_1()
                .border_color(colors.primary.alpha(0.07))
                .bg(colors.primary.alpha(0.025))
                .when(url.is_some(), |card| {
                    card.cursor_pointer()
                        .hover(move |card| card.bg(colors.primary.alpha(0.05)))
                })
                .child(
                    div()
                        .min_h(px(29.0))
                        .px(px(9.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .border_b_1()
                        .border_color(colors.primary.alpha(0.055))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .child(author),
                        )
                        .when_some(review_label, |header, label| {
                            header.child(
                                div()
                                    .px(px(5.0))
                                    .h(px(17.0))
                                    .flex()
                                    .items_center()
                                    .rounded_full()
                                    .bg(review_color.alpha(0.11))
                                    .text_size(px(9.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(review_color)
                                    .child(label),
                            )
                        })
                        .when_some(time, |header, time| {
                            header.child(
                                div()
                                    .ml_auto()
                                    .text_size(px(9.5))
                                    .text_color(colors.tertiary)
                                    .child(time),
                            )
                        }),
                )
                .child(
                    div()
                        .px(px(9.0))
                        .py(px(8.0))
                        .child(if body_fallback.is_empty() {
                            render_markdown(&body, colors)
                        } else {
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(colors.secondary)
                                .child(body_fallback)
                                .into_any_element()
                        }),
                )
                .when_some(url, |card, url| {
                    card.on_click(move |_, _, cx| cx.open_url(&url))
                }),
        )
        .into_any_element()
}

fn render_discussion_fallback(
    pull_request: &PullRequestStatus,
    colors: SemanticColors,
) -> AnyElement {
    let discussion = pull_request_discussion(pull_request)
        .unwrap_or_else(|| "Open the conversation on GitHub".to_owned());
    let url = pull_request.url.clone();
    div()
        .id(SharedString::from(format!(
            "inspector-pr-discussion-{}",
            pull_request.number
        )))
        .min_h(px(38.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Radius::BADGE))
        .border_1()
        .border_color(colors.primary.alpha(0.07))
        .bg(colors.primary.alpha(0.025))
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.05)))
        .child(sf_symbol(
            "bubble.left.and.bubble.right",
            12.0,
            colors.secondary,
        ))
        .child(
            div()
                .flex_1()
                .text_size(px(Typo::META.size))
                .text_color(colors.secondary)
                .child(discussion),
        )
        .child(sf_symbol("arrow.up.right", 9.0, colors.tertiary))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

fn sorted_pr_checks(pull_request: &PullRequestStatus) -> Vec<PrCheck> {
    let mut checks = pull_request.checks.clone().unwrap_or_default();
    checks.sort_by_key(|check| match check.result.as_str() {
        "fail" => 0,
        "pending" => 1,
        "pass" => 2,
        _ => 3,
    });
    checks
}

fn checks_rollup(pull_request: &PullRequestStatus) -> (String, gpui::Rgba) {
    if pull_request.checks_failed > 0 {
        return (
            format!("{} failed", pull_request.checks_failed),
            Ink::DANGER,
        );
    }
    if pull_request.checks_pending > 0 {
        return (
            format!("{} running", pull_request.checks_pending),
            Ink::ATTENTION,
        );
    }
    ("All passed".to_owned(), Ink::FRESH)
}

fn discussion_state(
    item: &PrDiscussionItem,
    colors: SemanticColors,
) -> (Option<String>, gpui::Rgba) {
    match item.state.as_deref() {
        Some("APPROVED") => (Some("Approved".to_owned()), Ink::FRESH),
        Some("CHANGES_REQUESTED") => (Some("Requested changes".to_owned()), Ink::DANGER),
        Some("COMMENTED") => (Some("Reviewed".to_owned()), colors.secondary),
        Some(state) => (Some(humanize_github_state(state)), colors.secondary),
        None => (None, colors.secondary),
    }
}

fn pull_request_can_merge(pull_request: &PullRequestStatus) -> bool {
    pull_request.state == "OPEN"
        && !pull_request.is_draft
        && pull_request.mergeable.as_deref() != Some("CONFLICTING")
        && pull_request.checks_failed == 0
        && pull_request.checks_pending == 0
        && !matches!(
            pull_request.review_decision.as_deref(),
            Some("CHANGES_REQUESTED") | Some("REVIEW_REQUIRED")
        )
        && !matches!(
            pull_request.merge_state_status.as_deref(),
            Some("BLOCKED") | Some("DIRTY") | Some("DRAFT")
        )
}

fn merge_blocker_label(pull_request: &PullRequestStatus) -> &'static str {
    if pull_request.checks_failed > 0 {
        "Checks are failing"
    } else if pull_request.checks_pending > 0 {
        "Checks are still running"
    } else if pull_request.mergeable.as_deref() == Some("CONFLICTING") {
        "Resolve merge conflicts"
    } else if pull_request.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
        "Changes were requested"
    } else if pull_request.review_decision.as_deref() == Some("REVIEW_REQUIRED") {
        "Review is required"
    } else {
        "GitHub is blocking the merge"
    }
}

fn humanize_github_state(value: &str) -> String {
    let lower = value.replace('_', " ").to_ascii_lowercase();
    let mut chars = lower.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn render_artifact_row(artifact: &SessionArtifact, colors: SemanticColors) -> AnyElement {
    let (symbol, kind_label) = match artifact.kind {
        ArtifactKind::PullRequest => ("arrow.triangle.pull", "Pull request"),
        ArtifactKind::LinearIssue => ("checklist", "Linear issue"),
        ArtifactKind::Preview => ("network", "Preview"),
        ArtifactKind::Link | ArtifactKind::Unknown => ("link", "Link"),
    };
    let title = artifact_title(artifact);
    let url = artifact.url.clone();
    div()
        .id(SharedString::from(format!(
            "inspector-artifact-{}",
            artifact.url
        )))
        .min_h(px(54.0))
        .px(px(11.0))
        .py(px(9.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(Radius::ROW))
        .bg(colors.primary.alpha(0.035))
        .border_1()
        .border_color(colors.primary.alpha(0.06))
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.065)))
        .child(artifact_icon(symbol, colors))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::ROW_EMPHASIZED.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(colors.primary)
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(kind_label),
                ),
        )
        .child(sf_symbol("arrow.up.right", 11.0, colors.tertiary))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

fn artifact_icon(symbol: &'static str, colors: SemanticColors) -> AnyElement {
    div()
        .size(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle(colors))
        .child(sf_symbol(symbol, 13.0, colors.secondary))
        .into_any_element()
}

fn artifact_count(session: &SessionRecord) -> usize {
    let artifacts = session.artifacts.as_deref().unwrap_or_default();
    let ports = session.listening_ports.as_deref().unwrap_or_default();
    let status_only_pull_requests = session
        .pull_requests
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|status| {
            !artifacts.iter().any(|artifact| {
                artifact.kind == ArtifactKind::PullRequest && artifact.url == status.url
            })
        })
        .count();
    artifacts.len() + ports.len() + status_only_pull_requests
}

fn ui_agent_kind(kind: &ProtoAgentKind) -> AgentKind {
    match kind.id() {
        ProtoAgentKind::CLAUDE_CODE_ID => AgentKind::ClaudeCode,
        ProtoAgentKind::CODEX_ID => AgentKind::Codex,
        ProtoAgentKind::CURSOR_ID => AgentKind::Cursor,
        ProtoAgentKind::GEMINI_ID => AgentKind::Gemini,
        ProtoAgentKind::SHELL_ID => AgentKind::Shell,
        _ => AgentKind::Generic,
    }
}

fn session_status(session: &SessionRecord, colors: SemanticColors) -> (&'static str, gpui::Rgba) {
    if session.hibernation.is_some() {
        return ("Sleeping", colors.secondary);
    }
    match session.status {
        SessionStatus::Starting => (
            "Starting",
            Ink::working(ui_agent_kind(session.effective_kind()), colors),
        ),
        SessionStatus::Working => (
            "Working",
            Ink::working(ui_agent_kind(session.effective_kind()), colors),
        ),
        SessionStatus::NeedsInput(_) => {
            let destructive = session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == diri_proto::RiskHint::Destructive);
            (
                "Needs input",
                if destructive {
                    Ink::DANGER
                } else {
                    Ink::ATTENTION
                },
            )
        }
        SessionStatus::Idle if session.attention() == diri_proto::AttentionLevel::DoneUnseen => {
            ("Finished", Ink::FRESH)
        }
        SessionStatus::Idle => ("Idle", colors.secondary),
        SessionStatus::Exited(_) => ("Ended", colors.tertiary),
        SessionStatus::Unknown => ("Unknown", colors.tertiary),
    }
}

fn pull_request_state(
    pull_request: &PullRequestStatus,
    colors: SemanticColors,
) -> (&'static str, gpui::Rgba) {
    if pull_request.state == "MERGED" {
        return ("Merged", rgba(0xaf7cf7ff));
    }
    if pull_request.state == "CLOSED" {
        return ("Closed", Ink::DANGER);
    }
    if pull_request.is_draft {
        return ("Draft", colors.secondary);
    }
    if pull_request.mergeable.as_deref() == Some("CONFLICTING") {
        return ("Conflicts", Ink::DANGER);
    }
    match pull_request.review_decision.as_deref() {
        Some("APPROVED") => ("Approved", Ink::FRESH),
        Some("CHANGES_REQUESTED") => ("Needs work", Ink::DANGER),
        Some("REVIEW_REQUIRED") => ("Review needed", Ink::ATTENTION),
        _ => ("Open", colors.secondary),
    }
}

fn pull_request_discussion(pull_request: &PullRequestStatus) -> Option<String> {
    let mut parts = Vec::new();
    if pull_request.comment_count > 0 {
        parts.push(format!(
            "{} {}",
            pull_request.comment_count,
            if pull_request.comment_count == 1 {
                "comment"
            } else {
                "comments"
            }
        ));
    }
    if pull_request.review_count > 0 {
        parts.push(format!(
            "{} {}",
            pull_request.review_count,
            if pull_request.review_count == 1 {
                "review"
            } else {
                "reviews"
            }
        ));
    }
    if let Some(total) = pull_request.total_threads.filter(|total| *total > 0) {
        parts.push(format!(
            "{} of {total} threads resolved",
            pull_request.resolved_threads.unwrap_or(0)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn artifact_title(artifact: &SessionArtifact) -> String {
    match artifact.kind {
        ArtifactKind::PullRequest => pr_number(&artifact.url)
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| "Pull request".to_owned()),
        ArtifactKind::LinearIssue => {
            linear_key(&artifact.url).unwrap_or_else(|| "Linear issue".to_owned())
        }
        ArtifactKind::Preview => url_authority(&artifact.url),
        ArtifactKind::Link | ArtifactKind::Unknown => url_authority(&artifact.url),
    }
}

fn pr_number(url: &str) -> Option<String> {
    let parts = url
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if let Some(index) = parts.iter().position(|part| *part == "pull") {
        return parts
            .get(index + 1)
            .map(|part| part.chars().take_while(char::is_ascii_digit).collect())
            .filter(|part: &String| !part.is_empty());
    }
    parts
        .last()
        .filter(|part| part.chars().all(|character| character.is_ascii_digit()))
        .map(|part| (*part).to_owned())
}

fn linear_key(url: &str) -> Option<String> {
    let parts = url.split('/').collect::<Vec<_>>();
    let index = parts.iter().position(|part| *part == "issue")?;
    parts.get(index + 1).map(|part| (*part).to_owned())
}

fn url_authority(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, remainder)| remainder)
        .split('/')
        .next()
        .filter(|authority| !authority.is_empty())
        .unwrap_or(url)
        .to_owned()
}

fn folder_name(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1_073_741_824.0;
    const MIB: f64 = 1_048_576.0;
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MB", bytes as f64 / MIB)
    }
}

fn relative_time(milliseconds: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
    let seconds = ((now - milliseconds).max(0.0) / 1000.0) as u64;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[derive(Clone)]
struct DiffRowRenderContext {
    content_width: f32,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    repo_root: PathBuf,
    layer: DiffLayer,
    armed_hunk: Option<u64>,
}

fn render_rows(
    snapshot: &DiffSnapshot,
    range: Range<usize>,
    content_width: f32,
    colors: SemanticColors,
    inspector: Entity<WorkbenchInspector>,
    repo_root: &Path,
    armed_hunk: Option<u64>,
) -> Vec<AnyElement> {
    let context = DiffRowRenderContext {
        content_width,
        colors,
        inspector,
        repo_root: repo_root.to_path_buf(),
        layer: snapshot.layer,
        armed_hunk,
    };
    range
        .map(|index| {
            let owning_file = snapshot
                .file_diffs
                .iter()
                .find(|file| file.row_range.contains(&index));
            let file = (snapshot.rows[index].kind == DiffRowKind::File)
                .then(|| owning_file.cloned())
                .flatten();
            let hunk = (snapshot.rows[index].kind == DiffRowKind::Hunk)
                .then(|| {
                    owning_file.and_then(|file| {
                        file.hunks
                            .iter()
                            .find(|hunk| hunk.row_range.start == index)
                            .cloned()
                            .map(|hunk| (file.path.clone(), hunk))
                    })
                })
                .flatten();
            render_row(index, &snapshot.rows[index], &context, file, hunk)
        })
        .collect()
}

fn prompt_layer(layer: DiffLayer) -> ReviewLayer {
    match layer {
        DiffLayer::Branch => ReviewLayer::Branch,
        DiffLayer::Staged => ReviewLayer::Staged,
        DiffLayer::Working => ReviewLayer::Working,
    }
}

fn patch_creates_file(patch: &[u8]) -> bool {
    patch
        .windows(b"--- /dev/null".len())
        .any(|window| window == b"--- /dev/null")
}

fn render_row(
    index: usize,
    row: &DiffRow,
    context: &DiffRowRenderContext,
    file: Option<DiffFile>,
    hunk: Option<(PathBuf, DiffHunk)>,
) -> AnyElement {
    let content_width = context.content_width;
    let colors = context.colors;
    let inspector = context.inspector.clone();
    let repo_root = &context.repo_root;
    let layer = context.layer;
    let armed_hunk = context.armed_hunk;
    let (background, foreground, marker) = match row.kind {
        DiffRowKind::Addition => (rgba(0x2f7d4a24), rgba(0xc7ebd2ff), "+"),
        DiffRowKind::Deletion => (rgba(0x9f3a4424), rgba(0xf0c4c8ff), "−"),
        DiffRowKind::Hunk => (rgba(0x4675a31c), rgba(0x9bbde0ff), ""),
        DiffRowKind::File => (rgba(0xffffff09), colors.primary, ""),
        DiffRowKind::Context => (rgba(0x00000000), rgba(0xffffffb8), ""),
        DiffRowKind::Meta => (rgba(0x00000000), rgba(0xffffff66), ""),
    };
    let line_number = |line: Option<u32>| line.map_or_else(String::new, |line| line.to_string());
    let text = if row.kind == DiffRowKind::File {
        SharedString::from(row.text.clone())
    } else {
        SharedString::from(format!("{marker}{}", row.text))
    };

    let reference = row.text.clone();
    let cwd = repo_root.to_path_buf();
    let open_inspector = inspector.clone();
    let mut actions = div()
        .absolute()
        .right(px(6.0))
        .top(px(2.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .gap(px(2.0))
        .rounded(px(Radius::CHIP))
        .bg(colors.background.alpha(0.96))
        .border_1()
        .border_color(colors.primary.alpha(0.10));

    if let Some(file) = file.as_ref() {
        let ask_inspector = inspector.clone();
        let evidence = ReviewEvidence::File {
            path: file.path.clone(),
            layer: prompt_layer(layer),
            patch: file
                .hunks
                .iter()
                .map(|hunk| String::from_utf8_lossy(&hunk.patch))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        actions = actions.child(
            div()
                .id(("ask-diff-file", index))
                .h_full()
                .px(px(5.0))
                .flex()
                .items_center()
                .gap(px(3.0))
                .rounded(px(Radius::CHIP))
                .cursor_pointer()
                .hover(move |button| button.bg(rgba(0xd9775722)))
                .text_size(px(8.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgba(0xe9a381ff))
                .child(sf_symbol("sparkles", 8.0, rgba(0xe9a381ff)))
                .child("Ask")
                .on_click(move |_, window, cx| {
                    ask_inspector.update(cx, |inspector, cx| {
                        inspector.open_ask(vec![evidence.clone()], window, cx);
                    });
                    cx.stop_propagation();
                }),
        );
        match layer {
            DiffLayer::Working => {
                let stage_inspector = inspector.clone();
                let path = file.path.clone();
                actions = actions.child(
                    div()
                        .id(("stage-diff-file", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.secondary)
                        .child("Stage")
                        .on_click(move |_, _, cx| {
                            stage_inspector.update(cx, |inspector, cx| {
                                inspector
                                    .run_review_action(ReviewAction::Stage(vec![path.clone()]), cx);
                            });
                            cx.stop_propagation();
                        }),
                );
            }
            DiffLayer::Staged => {
                let unstage_inspector = inspector.clone();
                let path = file.path.clone();
                actions = actions.child(
                    div()
                        .id(("unstage-diff-file", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.tertiary)
                        .child("Unstage")
                        .on_click(move |_, _, cx| {
                            unstage_inspector.update(cx, |inspector, cx| {
                                inspector.run_review_action(
                                    ReviewAction::Unstage(vec![path.clone()]),
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }),
                );
            }
            DiffLayer::Branch => {}
        }
    }

    if let Some((path, hunk)) = hunk.as_ref() {
        let ask_inspector = inspector.clone();
        let evidence = ReviewEvidence::Hunk {
            path: path.clone(),
            layer: prompt_layer(layer),
            header: hunk.header.clone(),
            patch: String::from_utf8_lossy(&hunk.patch).into_owned(),
        };
        actions = actions.child(
            div()
                .id(("ask-diff-hunk", index))
                .h_full()
                .px(px(5.0))
                .flex()
                .items_center()
                .gap(px(3.0))
                .rounded(px(Radius::CHIP))
                .cursor_pointer()
                .hover(move |button| button.bg(rgba(0xd9775722)))
                .text_size(px(8.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgba(0xe9a381ff))
                .child(sf_symbol("sparkles", 8.0, rgba(0xe9a381ff)))
                .child("Ask")
                .on_click(move |_, window, cx| {
                    ask_inspector.update(cx, |inspector, cx| {
                        inspector.open_ask(vec![evidence.clone()], window, cx);
                    });
                    cx.stop_propagation();
                }),
        );
        let patch = hunk.patch.clone();
        match layer {
            DiffLayer::Working => {
                let stage_inspector = inspector.clone();
                let stage_patch = patch.clone();
                actions = actions.child(
                    div()
                        .id(("stage-diff-hunk", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.secondary)
                        .child("Stage")
                        .on_click(move |_, _, cx| {
                            stage_inspector.update(cx, |inspector, cx| {
                                inspector.run_review_action(
                                    ReviewAction::Patch {
                                        patch: stage_patch.clone(),
                                        mutation: PatchMutation::Stage,
                                    },
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }),
                );
                if !patch_creates_file(&patch) {
                    let discard_inspector = inspector.clone();
                    let discard_patch = patch;
                    let fingerprint = hunk.fingerprint;
                    let armed = armed_hunk == Some(fingerprint);
                    actions = actions.child(
                        div()
                            .id(("discard-diff-hunk", index))
                            .h_full()
                            .px(px(5.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::CHIP))
                            .cursor_pointer()
                            .bg(if armed {
                                Ink::DANGER.alpha(0.12)
                            } else {
                                colors.primary.alpha(0.0)
                            })
                            .hover(move |button| button.bg(Ink::DANGER.alpha(0.13)))
                            .text_size(px(8.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Ink::DANGER)
                            .child(if armed { "Confirm" } else { "Discard" })
                            .on_click(move |_, _, cx| {
                                discard_inspector.update(cx, |inspector, cx| {
                                    if inspector.armed_hunk == Some(fingerprint) {
                                        inspector.run_review_action(
                                            ReviewAction::Patch {
                                                patch: discard_patch.clone(),
                                                mutation: PatchMutation::Discard,
                                            },
                                            cx,
                                        );
                                    } else {
                                        inspector.armed_hunk = Some(fingerprint);
                                        inspector.review_feedback = Some((
                                            false,
                                            "Click Confirm to discard this hunk".to_owned(),
                                        ));
                                        cx.notify();
                                    }
                                });
                                cx.stop_propagation();
                            }),
                    );
                }
            }
            DiffLayer::Staged => {
                let unstage_inspector = inspector.clone();
                actions = actions.child(
                    div()
                        .id(("unstage-diff-hunk", index))
                        .h_full()
                        .px(px(5.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.09)))
                        .text_size(px(8.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.tertiary)
                        .child("Unstage")
                        .on_click(move |_, _, cx| {
                            unstage_inspector.update(cx, |inspector, cx| {
                                inspector.run_review_action(
                                    ReviewAction::Patch {
                                        patch: patch.clone(),
                                        mutation: PatchMutation::Unstage,
                                    },
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        }),
                );
            }
            DiffLayer::Branch => {}
        }
    }

    let has_actions = file.is_some() || hunk.is_some();
    div()
        .id(index)
        .relative()
        .h(px(DIFF_ROW_HEIGHT))
        .min_w(px(content_width))
        .w_full()
        .flex()
        .items_center()
        .bg(background)
        .when(row.kind == DiffRowKind::File, |line| {
            line.border_t_1()
                .border_color(colors.primary.alpha(0.08))
                .cursor_pointer()
                .hover(move |line| line.bg(colors.primary.alpha(0.07)))
                .on_click(move |_, _, cx| {
                    open_inspector.update(cx, |inspector, cx| {
                        inspector.open_file_reference(cwd.clone(), reference.clone(), cx);
                    });
                    cx.stop_propagation();
                })
        })
        .child(
            div()
                .w(px(GUTTER_WIDTH))
                .h_full()
                .flex_none()
                .pr(px(7.0))
                .flex()
                .items_center()
                .justify_end()
                .gap(px(7.0))
                .border_r_1()
                .border_color(colors.primary.alpha(0.055))
                .font_family(crate::fonts::mono_family())
                .text_size(px(10.5))
                .text_color(colors.primary.alpha(0.25))
                .child(line_number(row.old_line))
                .child(line_number(row.new_line)),
        )
        .child(
            div()
                .h_full()
                .flex()
                .items_center()
                .pl(px(if row.kind == DiffRowKind::File {
                    10.0
                } else {
                    8.0
                }))
                .gap(px(6.0))
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.5))
                .font_weight(if row.kind == DiffRowKind::File {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(foreground)
                .when(row.kind == DiffRowKind::File, |content| {
                    content.child(sf_symbol(
                        "chevron.left.forwardslash.chevron.right",
                        13.0,
                        colors.secondary,
                    ))
                })
                .child(text),
        )
        .when(has_actions, |line| line.child(actions))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use diri_proto::DateMillis;
    use gpui::{Entity, Modifiers, TestAppContext};

    struct InspectorHarness {
        inspector: Entity<WorkbenchInspector>,
    }

    impl Render for InspectorHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.0))
                .h_full()
                .overflow_hidden()
                .child(self.inspector.clone())
        }
    }

    #[test]
    fn inspector_tabs_have_stable_spatial_order() {
        assert!(InspectorTab::Info.index() < InspectorTab::Changes.index());
        assert!(InspectorTab::Changes.index() < InspectorTab::Code.index());
        assert!(InspectorTab::Code.index() < InspectorTab::Artifacts.index());
    }

    #[test]
    fn artifact_titles_extract_the_useful_destination() {
        let pull_request = SessionArtifact {
            kind: ArtifactKind::PullRequest,
            url: "https://github.com/acme/diri/pull/42".to_owned(),
            first_seen_at: DateMillis(0.0),
        };
        let issue = SessionArtifact {
            kind: ArtifactKind::LinearIssue,
            url: "https://linear.app/acme/issue/DIR-19/polish-inspector".to_owned(),
            first_seen_at: DateMillis(0.0),
        };
        let preview = SessionArtifact {
            kind: ArtifactKind::Preview,
            url: "https://feature-dirijor.vercel.app/build".to_owned(),
            first_seen_at: DateMillis(0.0),
        };

        assert_eq!(artifact_title(&pull_request), "PR #42");
        assert_eq!(artifact_title(&issue), "DIR-19");
        assert_eq!(artifact_title(&preview), "feature-dirijor.vercel.app");
    }

    #[test]
    fn merge_gate_waits_for_checks_and_review_blockers() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Artifacts);
        let pull_request = fixture.list.sessions[0].pull_requests.as_ref().unwrap()[0].clone();
        assert!(!pull_request_can_merge(&pull_request));
        assert_eq!(
            merge_blocker_label(&pull_request),
            "Checks are still running"
        );

        let mut ready = pull_request;
        ready.checks_pending = 0;
        ready.checks_passed = 3;
        for check in ready.checks.as_mut().unwrap() {
            check.result = "pass".to_owned();
        }
        assert!(pull_request_can_merge(&ready));
    }

    #[gpui::test]
    fn tabs_fit_and_switch_at_the_minimum_inspector_width(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Artifacts);
        let selected = fixture.selected_session_id.clone();
        if let Some(session) = fixture
            .list
            .sessions
            .iter_mut()
            .find(|session| Some(&session.id) == selected.as_ref())
        {
            session.artifacts = Some(vec![SessionArtifact {
                kind: ArtifactKind::Preview,
                url: "https://preview.example.com".to_owned(),
                first_seen_at: DateMillis(0.0),
            }]);
            session.listening_ports = Some(vec![diri_proto::PortInfo {
                port: 3000,
                process_name: "node".to_owned(),
            }]);
        }
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            if let Some(selected) = selected {
                store.select(selected);
            }
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| {
                let mut inspector = WorkbenchInspector::new(inspector_runtime, tokio, cx);
                inspector.state = LoadState::Ready(Arc::new(DiffSnapshot {
                    files: 88,
                    additions: 556,
                    deletions: 19,
                    ..DiffSnapshot::default()
                }));
                inspector
            });
            InspectorHarness { inspector }
        });
        cx.run_until_parked();

        let info = cx.debug_bounds("INSPECTOR_TAB_INFO").expect("Info tab");
        let changes = cx
            .debug_bounds("INSPECTOR_TAB_CHANGES")
            .expect("Changes tab");
        let code = cx.debug_bounds("INSPECTOR_TAB_CODE").expect("Code tab");
        let artifacts = cx
            .debug_bounds("INSPECTOR_TAB_ARTIFACTS")
            .expect("Artifacts tab");
        let close = cx.debug_bounds("INSPECTOR_CLOSE").expect("close button");

        assert!(info.right() <= changes.left());
        assert!(changes.right() <= code.left());
        assert!(code.right() <= artifacts.left());
        assert!(artifacts.right() <= close.left());
        assert!(close.right() <= px(300.0));

        cx.simulate_click(changes.center(), Modifiers::none());
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.selected_tab),
            InspectorTab::Changes
        );
        cx.run_until_parked();

        let working = cx
            .debug_bounds("INSPECTOR_LAYER_WORKING")
            .expect("working-tree layer");
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.diff_layer),
            DiffLayer::Branch
        );
        cx.simulate_click(working.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.diff_layer),
            DiffLayer::Working
        );

        cx.simulate_click(artifacts.center(), Modifiers::none());
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.selected_tab),
            InspectorTab::Artifacts
        );
        assert_eq!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .inspector_tab,
            InspectorTab::Artifacts
        );

        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_PR_MERGE").is_some());
        assert!(cx.debug_bounds("INSPECTOR_PR_CHECK_0").is_some());
        assert!(cx.debug_bounds("INSPECTOR_PR_COMMENT_0").is_some());
        let ask = cx.debug_bounds("INSPECTOR_PR_ASK").expect("PR ask action");
        cx.simulate_click(ask.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_ASK_COMPOSER").is_some());
        assert!(cx.debug_bounds("INSPECTOR_ASK_SEND").is_some());
    }
}
