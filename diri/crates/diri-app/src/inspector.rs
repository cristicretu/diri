//! Native trailing workbench inspector.
//!
//! The root knows only whether this view is mounted and how wide its dock is.
//! This module owns selection tracking, session/PR/artifact projections,
//! background Git refreshes, unified-diff snapshots, and diff virtualization.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::{
    AgentKind as ProtoAgentKind, ArtifactKind, PrCheck, PrDiscussionItem, PullRequestStatus,
    SessionArtifact, SessionDiffBase, SessionId, SessionRecord, SessionStatus,
};
use diri_ui::{
    AgentKind, AgentLogo, Fill, FloatingSurface, Ink, LoadingIndicator, Metrics, Radius,
    SemanticColors, Typo,
};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, DragMoveEvent, EventEmitter, FontWeight,
    ListHorizontalSizingBehavior, MouseButton, Render, SharedString, StatefulInteractiveElement,
    Task, UniformListScrollHandle, Window, div, ease_out_quint, point, prelude::*, px, rgba,
    uniform_list,
};

use crate::diff::{
    DiffRow, DiffRowKind, DiffSnapshot, load_worktree_diff_against, snapshot_from_read_diff,
};
use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
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
    const ALL: [Self; 3] = [Self::Info, Self::Changes, Self::Artifacts];

    const fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Changes => "Changes",
            Self::Artifacts => "Artifacts",
        }
    }

    const fn index(self) -> i8 {
        match self {
            Self::Info => 0,
            Self::Changes => 1,
            Self::Artifacts => 2,
        }
    }

    const fn debug_selector(self) -> &'static str {
        match self {
            Self::Info => "INSPECTOR_TAB_INFO",
            Self::Changes => "INSPECTOR_TAB_CHANGES",
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

pub struct WorkbenchInspector {
    runtime: Arc<StoreRuntime>,
    _tokio_owner: Arc<tokio::runtime::Runtime>,
    tokio: tokio::runtime::Handle,
    visible: bool,
    selected_tab: InspectorTab,
    tab_direction: f32,
    tab_transition_generation: u64,
    context: Option<DiffContext>,
    state: LoadState,
    comparison: SessionDiffBase,
    comparison_menu_open: bool,
    loading: bool,
    generation: u64,
    scroll: UniformListScrollHandle,
    scrollbar_interaction: ScrollbarInteraction,
    scrollbar_layout_primed: bool,
    refresh_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _store_changes: Task<()>,
}

impl EventEmitter<InspectorEvent> for WorkbenchInspector {}

impl WorkbenchInspector {
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokio = tokio_owner.handle().clone();
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
            visible: false,
            selected_tab,
            tab_direction: 1.0,
            tab_transition_generation: 0,
            context: None,
            state: LoadState::NoSession,
            comparison: SessionDiffBase::DefaultBranch,
            comparison_menu_open: false,
            loading: false,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            scrollbar_interaction: ScrollbarInteraction::default(),
            scrollbar_layout_primed: false,
            refresh_task: None,
            poll_task: None,
            _store_changes: store_changes,
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            if self.selected_tab == InspectorTab::Changes {
                self.refresh(true, cx);
            } else {
                self.context = self.selected_context();
            }
        } else {
            self.comparison_menu_open = false;
        }
        self.reconcile_diff_polling(cx);
        cx.notify();
    }

    fn reconcile_diff_polling(&mut self, cx: &mut Context<Self>) {
        let should_poll = self.visible && self.selected_tab == InspectorTab::Changes;
        if !should_poll {
            // Dropping a GPUI Task cancels its timer/future. Info and Artifacts
            // therefore perform no periodic Git work and have no idle wakeup.
            self.poll_task = None;
            return;
        }
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        if this.visible && this.selected_tab == InspectorTab::Changes {
                            this.refresh(false, cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
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
        if self.selected_tab != InspectorTab::Changes {
            self.context = self.selected_context();
            cx.notify();
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
        self.reconcile_diff_polling(cx);
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

    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.visible || (self.loading && !force) {
            return;
        }
        let Some(context) = self.selected_context() else {
            self.context = None;
            self.state = LoadState::NoSession;
            cx.notify();
            return;
        };
        let context_changed = self.context.as_ref() != Some(&context);
        if context_changed {
            self.scroll = UniformListScrollHandle::new();
            self.scrollbar_interaction = ScrollbarInteraction::default();
            self.scrollbar_layout_primed = false;
        }
        self.context = Some(context.clone());
        if !force && !context_changed && matches!(self.state, LoadState::NoSession) {
            return;
        }

        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if should_show_blocking_git_loading(context_changed, &self.state) {
            self.state = LoadState::Loading;
            cx.notify();
        }
        let cwd = context.cwd;
        let session_id = context.id;
        let remote = context.remote;
        let comparison = self.comparison;
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
                    .spawn_blocking(move || load_worktree_diff_against(&cwd, comparison))
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

    fn selected_session(&self) -> Option<SessionRecord> {
        self.runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned()
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
                InspectorTab::Artifacts => artifacts_count,
            };
            let active = tab == selected_tab;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("inspector-tab-{}", tab.label())))
                    .debug_selector(move || tab.debug_selector().to_owned())
                    .h(px(28.0))
                    .px(px(9.0))
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
                    .when_some(count, |tab, count| {
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
        &self,
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
            for pull_request in pull_requests.iter().take(2) {
                content = content.child(render_pull_request(pull_request, colors));
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
                Some("arrow.left.arrow.right"),
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
                Some("checkmark.circle.fill"),
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
                None,
                "Reading working tree".to_owned(),
                "Git status is updating…".to_owned(),
                colors.secondary,
                false,
            ),
            LoadState::Error(error) if git_is_not_a_repository(error) => (
                Some("folder"),
                "Not a Git repository".to_owned(),
                "This folder has no Git working tree.".to_owned(),
                colors.tertiary,
                false,
            ),
            LoadState::Error(error) if git_is_not_installed(error) => (
                Some("terminal"),
                "Git unavailable".to_owned(),
                "Git is not installed on this host.".to_owned(),
                colors.tertiary,
                false,
            ),
            LoadState::Error(error) => (
                Some("exclamationmark.triangle.fill"),
                "Git status unavailable".to_owned(),
                error.clone(),
                Ink::ATTENTION,
                false,
            ),
            LoadState::NoSession => (
                Some("minus.circle"),
                "No session selected".to_owned(),
                "Select an agent to inspect its working tree.".to_owned(),
                colors.tertiary,
                false,
            ),
        };
        let status_mark = symbol.map_or_else(
            || LoadingIndicator::new("inspector-git-loading", 16.0, accent).into_any_element(),
            |symbol| sf_symbol(symbol, 15.0, accent),
        );
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
            .child(status_mark)
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
        &self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
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
            for pull_request in pull_requests {
                content = content.child(render_pull_request(pull_request, colors));
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
        let list = uniform_list("inspector-diff", row_count, move |range, _, _| {
            render_rows(&snapshot, range, content_width, colors)
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

    fn render_changes(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self.comparison_label();
        let empty_detail = format!("This branch matches {label}.");
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
        let menu_open = self.comparison_menu_open;
        let menu = if menu_open {
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

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(
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
                            .child("Compare changes"),
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
                            .bg(colors.primary.alpha(if menu_open { 0.10 } else { 0.055 }))
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
                    ),
            )
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(body))
            .when_some(menu, |panel, menu| panel.child(menu))
            .into_any_element()
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

fn git_is_not_a_repository(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("not a git repository")
        || error.contains("session cwd is not inside a git repository")
}

fn git_is_not_installed(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("git is not installed")
        || error.contains("git: command not found")
        || error.contains("git: not found")
}

fn should_show_blocking_git_loading(context_changed: bool, state: &LoadState) -> bool {
    context_changed || matches!(state, LoadState::NoSession)
}

impl Render for WorkbenchInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::sidebar_colors(&store.preferences().terminal_theme)
        };
        let session = self.selected_session();
        let body = match self.selected_tab {
            InspectorTab::Info => self.render_info(session.as_ref(), colors, cx),
            InspectorTab::Artifacts => self.render_artifacts(session.as_ref(), colors),
            InspectorTab::Changes => self.render_changes(colors, window, cx),
        };
        let transition_id = SharedString::from(format!(
            "inspector-tab-transition-{}",
            self.tab_transition_generation
        ));
        let direction = self.tab_direction;
        div()
            .id("workbench-inspector")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
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

fn render_pull_request(pull_request: &PullRequestStatus, colors: SemanticColors) -> AnyElement {
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
    let body = pull_request
        .body
        .as_deref()
        .map(compact_markdown)
        .filter(|body| !body.is_empty());

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
                            .p(px(9.0))
                            .rounded(px(Radius::BADGE))
                            .bg(colors.primary.alpha(0.035))
                            .line_height(px(16.0))
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .child(body),
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
    let compact_body = compact_markdown(&item.body);
    let body = if compact_body.is_empty() {
        review_label
            .clone()
            .unwrap_or_else(|| "Commented".to_owned())
    } else {
        compact_body
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
                        .line_height(px(16.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.secondary)
                        .child(body),
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

fn compact_markdown(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
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

fn render_rows(
    snapshot: &DiffSnapshot,
    range: Range<usize>,
    content_width: f32,
    colors: SemanticColors,
) -> Vec<AnyElement> {
    range
        .map(|index| render_row(index, &snapshot.rows[index], content_width, colors))
        .collect()
}

fn render_row(
    index: usize,
    row: &DiffRow,
    content_width: f32,
    colors: SemanticColors,
) -> AnyElement {
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

    div()
        .id(index)
        .h(px(DIFF_ROW_HEIGHT))
        .min_w(px(content_width))
        .w_full()
        .flex()
        .items_center()
        .bg(background)
        .when(row.kind == DiffRowKind::File, |line| {
            line.border_t_1().border_color(colors.primary.alpha(0.08))
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
        assert!(InspectorTab::Changes.index() < InspectorTab::Artifacts.index());
    }

    #[test]
    fn background_git_refresh_keeps_the_last_settled_surface() {
        assert!(!should_show_blocking_git_loading(
            false,
            &LoadState::Error("not a git repository".to_owned())
        ));
        assert!(!should_show_blocking_git_loading(
            false,
            &LoadState::Ready(Arc::new(DiffSnapshot::default()))
        ));
        assert!(should_show_blocking_git_loading(
            true,
            &LoadState::Error("old project".to_owned())
        ));
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
        let artifacts = cx
            .debug_bounds("INSPECTOR_TAB_ARTIFACTS")
            .expect("Artifacts tab");
        let close = cx.debug_bounds("INSPECTOR_CLOSE").expect("close button");

        assert!(info.right() <= changes.left());
        assert!(changes.right() <= artifacts.left());
        assert!(artifacts.right() <= close.left());
        assert!(close.right() <= px(300.0));

        cx.simulate_click(changes.center(), Modifiers::none());
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.selected_tab),
            InspectorTab::Changes
        );
        cx.run_until_parked();

        let compare = cx
            .debug_bounds("INSPECTOR_COMPARE_BUTTON")
            .expect("comparison button");
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.comparison),
            SessionDiffBase::DefaultBranch
        );
        cx.simulate_click(compare.center(), Modifiers::none());
        cx.run_until_parked();
        let head = cx
            .debug_bounds("INSPECTOR_COMPARE_HEAD")
            .expect("HEAD comparison option");
        cx.simulate_click(head.center(), Modifiers::none());
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.comparison),
            SessionDiffBase::Head
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
    }

    #[test]
    fn ordinary_remote_git_absence_is_rendered_as_compatibility_state() {
        assert!(git_is_not_a_repository(
            "internal: fatal: not a git repository (or any parent)"
        ));
        assert!(git_is_not_installed(
            "internal: git is not installed on this host"
        ));
        assert!(!git_is_not_a_repository("ssh connection timed out"));
    }
}
