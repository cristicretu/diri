//! Native trailing workbench inspector.
//!
//! The root knows only whether this view is mounted and how wide its dock is.
//! This module owns selection tracking, session/PR/artifact projections,
//! background Git refreshes, unified-diff snapshots, and diff virtualization.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_proto::{
    AgentKind as ProtoAgentKind, ArtifactKind, PullRequestStatus, SessionArtifact, SessionDiffBase,
    SessionId, SessionRecord, SessionStatus,
};
use diri_ui::{
    AgentKind, AgentLogo, Fill, FloatingSurface, GlassMenuRow, IconName, Ink, LoadingIndicator,
    Metrics, Radius, SemanticColors, Typo,
};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyDownEvent, ListHorizontalSizingBehavior, MouseButton,
    Render, Rgba, ScrollStrategy, SharedString, StatefulInteractiveElement, Task,
    UniformListScrollHandle, Window, canvas, div, ease_out_quint, point, prelude::*, px, rgba,
    uniform_list,
};

use crate::code_viewer::CodeViewer;
use crate::details_ui;
use crate::diff::{
    DiffLayer, DiffSelection, DiffSnapshot, load_commit_diff, load_local_diff,
    snapshot_from_read_diff,
};
use crate::git_review::{
    COMMIT_HISTORY_LIMIT, GitRepository, GitReviewError, PatchMutation, ReviewStatus,
};
use crate::git_ui::diff_view::{
    self, DiffHandlers, DiffViewProps, FileAction, HunkAction, OmittedAction,
};
use crate::git_ui::{
    CommitDiffLoad, DiffLayout, DiffPalette, HistoryLoad, LoadedHistory, ReviewMode, ReviewUi,
};
use crate::i18n::{t, tf};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::markdown::MarkdownDocument;
use crate::markdown_view::render_markdown;
use crate::pr_card::{PrCardActions, PrCardState, PullRequestCard};
#[cfg(test)]
use crate::pr_card::{merge_blocker_label, pull_request_can_merge};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::quote::{Quote, QuoteSource};
use crate::review_prompt::{ReviewEvidence, ReviewLayer, ReviewPrompt};
use crate::store::{InspectorTab, StoreRuntime};
use crate::terminal_pane::TerminalPane;
use crate::transcript::{TranscriptDocument, TranscriptVersion, load as load_transcript};

/// Omitted file names are indented a few columns under their notice.
const OMITTED_PATH_INDENT_COLUMNS: usize = 3;
const REFRESH_INTERVAL: Duration = Duration::from_millis(1400);
const TRANSCRIPT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(120);
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

#[derive(Clone, Debug)]
pub enum InspectorEvent {
    Close,
    SessionChanged,
    WorkspaceChanged(WorkspaceSurface),
    /// Restore a conversation's inspector without moving keyboard focus into it.
    WorkspaceRestored(WorkspaceSurface),
    WorkspaceClosed {
        surface: WorkspaceSurface,
        id: u64,
        /// Slot of a closed terminal tab. The tab is already gone when the
        /// event is handled, so the shell binding has to travel with it.
        terminal_slot: Option<usize>,
    },
    RequestTerminal,
    Browser(BrowserAction),
    /// Start a new Agent in a fresh worktree from the default branch of this
    /// Session's repository (the worktree chip's "behind main" action).
    NewAgentFromDefaultBranch(SessionId),
}

#[derive(Clone, Debug)]
pub enum BrowserAction {
    Navigate(String),
    Back,
    Forward,
    Reload,
    OpenExternal(String),
}

/// The native WebKit view owns navigation. This compact projection lets the
/// GPUI chrome accurately reflect redirects, in-page links, and history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserState {
    pub url: Option<String>,
    pub title: Option<String>,
    pub favicon: Option<Arc<gpui::Image>>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub is_loading: bool,
    pub error: Option<String>,
}

impl InspectorTab {
    const DETAILS: [Self; 2] = [Self::Info, Self::Artifacts];

    fn label(self) -> &'static str {
        t(match self {
            Self::Info => "panel.tab.info",
            Self::Changes => "panel.tab.review",
            Self::Code => "panel.tab.code",
            Self::Artifacts => "panel.tab.artifacts",
        })
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

/// A workspace surface is deliberately separate from `InspectorTab`: the
/// latter is persisted user state for the existing agent details views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSurface {
    Details,
    Browser,
    Terminal,
    Files,
    Review,
    /// HTTP requests (`crate::api_client`).
    Api,
}

impl WorkspaceSurface {
    const CATALOG: [Self; 6] = [
        Self::Browser,
        Self::Terminal,
        Self::Files,
        Self::Review,
        Self::Api,
        Self::Details,
    ];

    fn label(self) -> &'static str {
        t(match self {
            Self::Details => "panel.surface.details",
            Self::Browser => "panel.surface.browser",
            Self::Terminal => "panel.surface.terminal",
            Self::Files => "panel.surface.files",
            Self::Review => "panel.tab.review",
            Self::Api => "panel.surface.api",
        })
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Details => "square.stack.3d.up",
            Self::Browser => "network",
            Self::Terminal => "terminal",
            Self::Files => "folder",
            Self::Review => "checklist",
            Self::Api => "server.rack",
        }
    }
}

/// Identity belongs to the tab instance, never to its surface kind.
struct WorkspaceTab {
    id: u64,
    surface: WorkspaceSurface,
    viewer: Option<Entity<CodeViewer>>,
    api: Option<Entity<crate::api_client::ApiClient>>,
    terminal_slot: Option<usize>,
    details_tab: InspectorTab,
    scroll: UniformListScrollHandle,
    diff_layer: DiffLayer,
    comparison: SessionDiffBase,
    browser_query: QueryEditor,
    browser_state: BrowserState,
}

impl WorkspaceTab {
    fn new(id: u64, surface: WorkspaceSurface) -> Self {
        Self {
            id,
            surface,
            viewer: None,
            api: None,
            terminal_slot: None,
            details_tab: InspectorTab::Info,
            scroll: UniformListScrollHandle::new(),
            diff_layer: DiffLayer::Branch,
            comparison: SessionDiffBase::DefaultBranch,
            browser_query: QueryEditor::default(),
            browser_state: BrowserState::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffContext {
    id: SessionId,
    /// The checkout the panel shows: the one the Agent works in, which is the
    /// launch directory unless it moved (`crate::workspace_follow`).
    cwd: PathBuf,
    /// `SessionRecord.cwd`, which transcripts are validated against.
    launch_cwd: PathBuf,
    remote: bool,
    agent_session_id: Option<String>,
    transcript_path: Option<PathBuf>,
    kind: ProtoAgentKind,
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
enum TranscriptLoadState {
    Unavailable,
    Loading,
    Ready(Arc<TranscriptDocument>),
    Error,
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

#[derive(Clone, Debug)]
struct SelectedTurn {
    key: String,
    quote: Quote,
}

struct SessionWorkspace {
    tabs: Vec<WorkspaceTab>,
    active: Option<u64>,
    visible: bool,
    next_terminal_slot: usize,
}

/// The review file navigator as a panel target (see `crate::floating::Target`).
const INSPECTOR_FILES_MENU: crate::floating::Target<WorkbenchInspector> = crate::floating::Target {
    key: "inspector-files",
    radius: crate::floating::MENU_RADIUS,
    content: WorkbenchInspector::files_panel_content,
    dismiss: |this, _, cx| {
        this.files_open = false;
        cx.notify();
    },
};

/// The panel header's + menu as a panel target.
const INSPECTOR_ADD_MENU: crate::floating::Target<WorkbenchInspector> = crate::floating::Target {
    key: "inspector-add-surface",
    radius: crate::floating::MENU_RADIUS,
    content: WorkbenchInspector::add_menu_panel_content,
    dismiss: |this, _, cx| {
        this.workspace_chooser_open = false;
        cx.notify();
    },
};
const ADD_MENU_WIDTH: f32 = 184.0;

/// The comparison base menu as a panel target.
const INSPECTOR_COMPARISON_MENU: crate::floating::Target<WorkbenchInspector> =
    crate::floating::Target {
        key: "inspector-comparison",
        radius: crate::floating::MENU_RADIUS,
        content: WorkbenchInspector::comparison_panel_content,
        dismiss: |this, _, cx| {
            this.comparison_menu_open = false;
            cx.notify();
        },
    };

pub struct WorkbenchInspector {
    runtime: Arc<StoreRuntime>,
    _tokio_owner: Arc<tokio::runtime::Runtime>,
    tokio: tokio::runtime::Handle,
    code_viewer: Entity<CodeViewer>,
    terminal_surface: Option<Entity<TerminalPane>>,
    markdown_cache: HashMap<String, Arc<MarkdownDocument>>,
    focus: FocusHandle,
    visible: bool,
    selected_tab: InspectorTab,
    details_tab: InspectorTab,
    workspace_session: Option<SessionId>,
    // None follows legacy selection; Some(None) is an empty saved workspace.
    session_context: Option<Option<SessionId>>,
    session_workspaces: HashMap<Option<SessionId>, SessionWorkspace>,
    workspace_tabs: Vec<WorkspaceTab>,
    workspace_active: Option<u64>,
    workspace_tab_scroll: gpui::ScrollHandle,
    workspace_tab_width: std::rc::Rc<std::cell::Cell<f32>>,
    next_workspace_id: u64,
    next_terminal_slot: usize,
    workspace_selected: Option<WorkspaceSurface>,
    workspace_chooser_open: bool,
    tab_direction: f32,
    tab_transition_generation: u64,
    browser_query: QueryEditor,
    browser_address_focused: bool,
    browser_state: BrowserState,
    /// Where the API surface keeps each project's requests.
    api_store_root: PathBuf,
    #[cfg(target_os = "macos")]
    native_browser: Option<std::rc::Rc<std::cell::RefCell<crate::macos::browser::NativeBrowser>>>,
    context: Option<DiffContext>,
    /// Which checkout the panel shows: see `crate::workspace_follow`.
    follow: crate::workspace_follow::FollowController,
    state: LoadState,
    review_state: ReviewLoadState,
    review_generation: u64,
    review_task: Option<Task<()>>,
    transcript_state: TranscriptLoadState,
    transcript_version: Option<TranscriptVersion>,
    transcript_generation: u64,
    transcript_task: Option<Task<()>>,
    transcript_home: PathBuf,
    review_action_task: Option<Task<()>>,
    review_action_busy: bool,
    review_feedback: Option<(bool, String)>,
    status_evidence_open: bool,
    /// Pull request card folds the user changed, by URL.
    pr_cards: PrCardState,
    ask_draft: Option<AskDraft>,
    ask_query: QueryEditor,
    ask_task: Option<Task<()>>,
    ask_busy: bool,
    ask_feedback: Option<(bool, String)>,
    commit_open: bool,
    commit_query: QueryEditor,
    discard_armed: bool,
    armed_hunk: Option<u64>,
    /// Whether the "not shown" notice is expanded into the omitted file names.
    omitted_untracked_open: bool,
    diff_selection: DiffSelection,
    /// Review presentation: layout, collapsed files, commit history.
    review_ui: ReviewUi,
    selected_turn: Option<SelectedTurn>,
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
    poll_task: Option<Task<()>>,
    _store_changes: Task<()>,
}

impl EventEmitter<InspectorEvent> for WorkbenchInspector {}

impl Focusable for WorkbenchInspector {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The terminal theme and font family the Files surface's editor paints
/// with, so code reads in the same palette as the terminal beside it.
fn code_style(store: &crate::store::SessionStore) -> (diri_term::theme::TermTheme, String) {
    (
        crate::app_theme::terminal_theme(store.theme_id()),
        store.preferences().terminal_font_family.clone(),
    )
}

impl WorkbenchInspector {
    pub fn new(
        runtime: Arc<StoreRuntime>,
        tokio_owner: Arc<tokio::runtime::Runtime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokio = tokio_owner.handle().clone();
        let (selected_tab, code_colors, workspace_session, code_style) = {
            let store = runtime.store.read().expect("session store lock poisoned");
            (
                store.preferences().inspector_tab,
                crate::app_theme::sidebar_colors_in(&store),
                store.selected_session_id().cloned(),
                code_style(&store),
            )
        };
        let code_viewer = cx.new(|cx| {
            let mut viewer = CodeViewer::new(tokio.clone(), code_colors, cx);
            viewer.set_terminal_style(code_style.0, &code_style.1, cx);
            viewer
        });
        cx.observe(&code_viewer, |_, _, cx| cx.notify()).detach();
        let focus = cx.focus_handle();
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
        let initial_surface = match selected_tab {
            InspectorTab::Changes => WorkspaceSurface::Review,
            InspectorTab::Code => WorkspaceSurface::Files,
            InspectorTab::Info | InspectorTab::Artifacts => WorkspaceSurface::Details,
        };
        let mut workspace_tabs = vec![WorkspaceTab::new(0, WorkspaceSurface::Details)];
        if initial_surface != WorkspaceSurface::Details {
            workspace_tabs.push(WorkspaceTab::new(1, initial_surface));
        }
        if initial_surface == WorkspaceSurface::Files {
            workspace_tabs.last_mut().unwrap().viewer = Some(code_viewer.clone());
        }
        workspace_tabs[0].details_tab = if selected_tab == InspectorTab::Artifacts {
            InspectorTab::Artifacts
        } else {
            InspectorTab::Info
        };
        let workspace_active = workspace_tabs.last().map(|tab| tab.id);
        Self {
            runtime,
            _tokio_owner: tokio_owner,
            tokio,
            code_viewer,
            terminal_surface: None,
            markdown_cache: HashMap::new(),
            focus,
            visible: false,
            selected_tab,
            details_tab: if selected_tab == InspectorTab::Artifacts {
                InspectorTab::Artifacts
            } else {
                InspectorTab::Info
            },
            workspace_session,
            session_context: None,
            session_workspaces: HashMap::new(),
            workspace_tabs,
            workspace_active,
            workspace_tab_scroll: gpui::ScrollHandle::new(),
            workspace_tab_width: Default::default(),
            next_workspace_id: 2,
            next_terminal_slot: 0,
            workspace_selected: Some(initial_surface),
            workspace_chooser_open: false,
            tab_direction: 1.0,
            tab_transition_generation: 0,
            browser_query: QueryEditor::default(),
            browser_address_focused: false,
            browser_state: BrowserState::default(),
            api_store_root: crate::api_client::storage::default_root(),
            #[cfg(target_os = "macos")]
            native_browser: None,
            context: None,
            follow: Default::default(),
            state: LoadState::NoSession,
            review_state: ReviewLoadState::NoSession,
            review_generation: 0,
            review_task: None,
            transcript_state: TranscriptLoadState::Unavailable,
            transcript_version: None,
            transcript_generation: 0,
            transcript_task: None,
            transcript_home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            review_action_task: None,
            review_action_busy: false,
            review_feedback: None,
            status_evidence_open: false,
            pr_cards: PrCardState::default(),
            ask_draft: None,
            ask_query: QueryEditor::default(),
            ask_task: None,
            ask_busy: false,
            ask_feedback: None,
            commit_open: false,
            commit_query: QueryEditor::default(),
            discard_armed: false,
            armed_hunk: None,
            omitted_untracked_open: false,
            diff_selection: DiffSelection::default(),
            review_ui: ReviewUi::default(),
            selected_turn: None,
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
            poll_task: None,
            _store_changes: store_changes,
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.seed_default_workspace(cx);
            self.sync_workspace_follow(true, cx);
            // One-shot, every tab. Info renders the Git summary and the header
            // renders the Changes badge, so becoming visible always needs one
            // settled read of the working tree — what stays tab-gated is the
            // *periodic* poll below, not this edge-triggered refresh.
            self.refresh(true, cx);
            if self.is_terminal_tab() {
                cx.emit(InspectorEvent::RequestTerminal);
            }
        } else {
            self.comparison_menu_open = false;
            self.files_open = false;
            self.ask_draft = None;
            self.ask_feedback = None;
            self.ask_query.clear();
            self.release_hidden_state();
        }
        self.reconcile_diff_polling(cx);
        cx.notify();
    }

    /// A hidden panel paints none of this, and the diff and transcript are
    /// its largest allocations. Becoming visible forces a full refresh, so
    /// holding them only kept megabytes alive for a closed panel.
    fn release_hidden_state(&mut self) {
        self.refresh_task = None;
        self.review_task = None;
        self.transcript_task = None;
        self.loading = false;
        // Row indices and the transcript version describe the dropped
        // snapshots; a kept version would turn the reload into a no-op.
        self.diff_selection.clear();
        self.review_ui.reset_for_context();
        self.selected_turn = None;
        self.transcript_version = None;
        self.state = LoadState::NoSession;
        self.review_state = ReviewLoadState::NoSession;
        self.transcript_state = TranscriptLoadState::Unavailable;
        self.markdown_cache = HashMap::new();
    }

    /// The transcript is only painted by Details → Info.
    fn transcript_showing(&self) -> bool {
        self.workspace_selected == Some(WorkspaceSurface::Details)
            && self.selected_tab == InspectorTab::Info
    }

    pub fn set_terminal_surface(
        &mut self,
        terminal: Option<Entity<TerminalPane>>,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_surface != terminal {
            self.terminal_surface = terminal;
            cx.notify();
        }
    }

    #[must_use]
    pub fn is_terminal_tab(&self) -> bool {
        self.workspace_selected == Some(WorkspaceSurface::Terminal)
    }

    #[must_use]
    pub(crate) fn has_active_workspace(&self) -> bool {
        self.workspace_selected.is_some()
    }

    /// Land keyboard focus on the tab already selected. A blank browser gets
    /// its address field; every other non-terminal surface uses this view.
    pub(crate) fn focus_active_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_selected == Some(WorkspaceSurface::Browser)
            && self.browser_state.url.is_none()
        {
            self.focus_browser_address(window, cx);
            return;
        }
        window.focus(&self.focus, cx);
    }

    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn is_browser_tab(&self) -> bool {
        self.workspace_selected == Some(WorkspaceSurface::Browser)
    }

    #[cfg(target_os = "macos")]
    pub fn set_native_browser(
        &mut self,
        browser: std::rc::Rc<std::cell::RefCell<crate::macos::browser::NativeBrowser>>,
    ) {
        self.native_browser = Some(browser);
    }

    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn blocks_native_browser(&self) -> bool {
        self.workspace_chooser_open
            || self.comparison_menu_open
            || self.files_open
            || self.status_evidence_open
            || self.ask_draft.is_some()
            || self.commit_open
    }

    #[must_use]
    pub fn workspace_needs_terminal(&self) -> bool {
        self.workspace_tabs
            .iter()
            .any(|tab| tab.surface == WorkspaceSurface::Terminal)
    }

    #[cfg(target_os = "macos")]
    pub fn set_browser_state(&mut self, state: BrowserState, cx: &mut Context<Self>) {
        let blurred = self.browser_address_focused
            && self
                .native_browser
                .as_ref()
                .is_some_and(|browser| browser.borrow().has_focus());
        if blurred {
            self.browser_address_focused = false;
        }
        if self.browser_state == state && !blurred {
            return;
        }
        let update_address = !self.browser_address_focused;
        if (self.browser_state.title != state.title || self.browser_state.favicon != state.favicon)
            && let Some(index) = self
                .workspace_tabs
                .iter()
                .position(|tab| Some(tab.id) == self.workspace_active)
        {
            self.workspace_tab_scroll.scroll_to_item(index);
        }
        self.browser_state = state;
        if update_address {
            self.browser_query.clear();
            if let Some(url) = self.browser_state.url.as_deref() {
                self.browser_query.insert(url);
            }
        }
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub fn set_browser_tab_state(&mut self, id: u64, state: BrowserState, cx: &mut Context<Self>) {
        if self.workspace_active == Some(id) {
            self.set_browser_state(state, cx);
            return;
        }
        for tab in self.workspace_tabs.iter_mut().chain(
            self.session_workspaces
                .values_mut()
                .flat_map(|workspace| workspace.tabs.iter_mut()),
        ) {
            if tab.id == id && tab.browser_state != state {
                tab.browser_state = state;
                cx.notify();
                return;
            }
        }
    }

    #[must_use]
    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Returns the selection owned by the inspector's active surface.
    #[must_use]
    pub fn quote_selection(&self) -> Option<Quote> {
        match self.workspace_selected {
            Some(WorkspaceSurface::Review) => {
                let snapshot = self.displayed_diff()?;
                let session_id = self.context.as_ref()?.id.clone();
                self.diff_selection.quote(snapshot, session_id)
            }
            Some(WorkspaceSurface::Details) => match self.selected_tab {
                InspectorTab::Info | InspectorTab::Artifacts => {
                    self.selected_turn.as_ref().map(|turn| turn.quote.clone())
                }
                InspectorTab::Changes => {
                    let snapshot = self.displayed_diff()?;
                    let session_id = self.context.as_ref()?.id.clone();
                    self.diff_selection.quote(snapshot, session_id)
                }
                InspectorTab::Code => None,
            },
            _ => None,
        }
    }

    fn select_diff_row(
        &mut self,
        row: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.displayed_diff().cloned() else {
            return;
        };
        self.selected_turn = None;
        self.diff_selection.select(&snapshot, row, extend);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn select_turn(
        &mut self,
        key: String,
        source: QuoteSource,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(quote) = Quote::new(source, content) else {
            return;
        };
        self.diff_selection.clear();
        self.selected_turn = Some(SelectedTurn { key, quote });
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn selected_turn_key(&self) -> Option<&str> {
        self.selected_turn
            .as_ref()
            .map(|selection| selection.key.as_str())
    }

    fn reconcile_diff_polling(&mut self, cx: &mut Context<Self>) {
        let should_poll = self.visible
            && (self.workspace_selected == Some(WorkspaceSurface::Review)
                || (self.workspace_selected == Some(WorkspaceSurface::Details)
                    && self.selected_tab == InspectorTab::Changes));
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
                        if this.visible
                            && (this.workspace_selected == Some(WorkspaceSurface::Review)
                                || (this.workspace_selected == Some(WorkspaceSurface::Details)
                                    && this.selected_tab == InspectorTab::Changes))
                        {
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
        self.select_tab(InspectorTab::Code, cx);
        self.code_viewer.update(cx, |viewer, cx| {
            viewer.open_reference(cwd, reference, cx);
        });
    }

    #[cfg(test)]
    pub(crate) fn workspace_tab_count(&self) -> usize {
        self.workspace_tabs.len()
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn session_id_for_test(&self) -> Option<SessionId> {
        self.selected_session().map(|session| session.id)
    }

    fn selected_context(&self) -> Option<DiffContext> {
        let session = self.selected_session()?;
        Some(DiffContext {
            id: session.id.clone(),
            cwd: self.follow.directory_for(&session),
            launch_cwd: PathBuf::from(&session.cwd),
            remote: session.host.is_some(),
            agent_session_id: session.agent_session_id.clone(),
            transcript_path: session.transcript_path.as_deref().map(PathBuf::from),
            kind: session.effective_kind().clone(),
        })
    }

    fn refresh_if_context_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        self.sync_workspace_follow(false, cx);
        let (colors, (theme, font)) = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            (
                crate::app_theme::sidebar_colors_in(&store),
                code_style(&store),
            )
        };
        self.code_viewer.update(cx, |viewer, cx| {
            viewer.set_colors(colors, cx);
            viewer.set_terminal_style(theme, &font, cx);
        });
        for tab in &self.workspace_tabs {
            if let Some(viewer) = &tab.viewer {
                viewer.update(cx, |viewer, cx| {
                    viewer.set_colors(colors, cx);
                    viewer.set_terminal_style(theme, &font, cx);
                });
            }
        }
        self.sync_api_colors(cx);
        if !self.visible {
            return;
        }
        // Edge-triggered on a real context change, on every tab: a store change
        // that moves the selection must not leave Info showing the previous
        // session's counts. This is not periodic work — an idle Info tab makes
        // no Git calls because `reconcile_diff_polling` installs no timer.
        if self.selected_context() != self.context {
            self.refresh(true, cx);
            if self.is_terminal_tab() {
                cx.emit(InspectorEvent::RequestTerminal);
            }
        } else {
            // Info and Artifacts are projections of the live session record,
            // so same-session store changes repaint and schedule one bounded
            // transcript mtime check without installing an idle poll. Other
            // tabs skip it: activating Info performs its own version check.
            if self.transcript_showing()
                && let Some(context) = self.context.clone()
            {
                self.refresh_transcript(&context, true, cx);
            }
            cx.notify();
        }
    }

    fn save_active_workspace(&mut self) {
        if let Some(tab) = self
            .workspace_tabs
            .iter_mut()
            .find(|tab| Some(tab.id) == self.workspace_active)
        {
            tab.details_tab = self.details_tab;
            tab.scroll = self.scroll.clone();
            tab.diff_layer = self.diff_layer;
            tab.comparison = self.comparison;
            tab.browser_query = self.browser_query.clone();
            tab.browser_state = self.browser_state.clone();
        }
    }

    fn prune_session_workspaces(&mut self) {
        {
            let store = self.runtime.store.read().expect("store");
            self.session_workspaces.retain(|id, workspace| {
                // Archived records stay in the store, so membership alone
                // would keep their viewers, indexes and web pages for good.
                // Unarchiving starts from a fresh workspace.
                let keep = id.as_ref().is_none_or(|id| {
                    store
                        .sessions()
                        .get(id)
                        .is_some_and(|session| !session.is_archived())
                });
                if !keep {
                    #[cfg(target_os = "macos")]
                    if let Some(browser) = &self.native_browser {
                        for tab in &workspace.tabs {
                            if tab.surface == WorkspaceSurface::Browser {
                                browser.borrow_mut().close_tab(tab.id);
                            }
                        }
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = workspace;
                }
                keep
            });
        }
    }

    /// Session identity owns tabs, including hidden ones. Tab IDs stay unique
    /// across sessions so native WebKit pages cannot alias one another.
    pub(crate) fn sync_workspace_session(&mut self, cx: &mut Context<Self>) {
        self.prune_session_workspaces();
        let session = self.selected_context().map(|context| context.id);
        if self.workspace_session == session {
            return;
        }
        self.save_active_workspace();
        let previous = SessionWorkspace {
            tabs: std::mem::take(&mut self.workspace_tabs),
            active: self.workspace_active.take(),
            visible: self.visible,
            next_terminal_slot: self.next_terminal_slot,
        };
        self.session_workspaces
            .insert(self.workspace_session.take(), previous);
        self.prune_session_workspaces();
        self.workspace_session = session.clone();
        let next = self.session_workspaces.remove(&session).unwrap_or_else(|| {
            let id = self.next_workspace_id;
            self.next_workspace_id += 1;
            SessionWorkspace {
                tabs: vec![WorkspaceTab::new(id, WorkspaceSurface::Details)],
                active: Some(id),
                visible: self.visible,
                next_terminal_slot: 0,
            }
        });
        self.workspace_tabs = next.tabs;
        self.next_terminal_slot = next.next_terminal_slot;
        self.visible = next.visible;
        self.workspace_selected = None;
        self.workspace_chooser_open = false;
        self.comparison_menu_open = false;
        self.files_open = false;
        self.status_evidence_open = false;
        self.commit_open = false;
        self.ask_draft = None;
        self.ask_feedback = None;
        self.ask_query.clear();
        self.commit_query.clear();
        self.discard_armed = false;
        self.armed_hunk = None;
        self.omitted_untracked_open = false;

        self.browser_address_focused = false;
        self.browser_query.clear();
        self.browser_state = BrowserState::default();
        self.terminal_surface = None;
        self.context = None;
        self.refresh_task = None;
        self.review_task = None;
        self.transcript_task = None;
        self.review_action_task = None;
        self.ask_task = None;
        self.review_action_busy = false;
        self.ask_busy = false;
        self.review_feedback = None;
        self.loading = false;
        self.state = LoadState::NoSession;
        self.review_state = ReviewLoadState::NoSession;
        self.transcript_state = TranscriptLoadState::Unavailable;
        if let Some(id) = next.active
            && let Some(surface) = self.load_workspace(id, cx)
        {
            cx.emit(InspectorEvent::WorkspaceRestored(surface));
        }
        if self.visible {
            self.seed_default_workspace(cx);
        }
        self.reconcile_diff_polling(cx);
        cx.emit(InspectorEvent::SessionChanged);
        cx.notify();
    }

    fn select_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        if tab == InspectorTab::Changes {
            self.select_workspace(WorkspaceSurface::Review, cx);
            return;
        }
        if tab == InspectorTab::Code {
            self.select_workspace(WorkspaceSurface::Files, cx);
            return;
        }
        if self.workspace_selected != Some(WorkspaceSurface::Details) {
            self.select_workspace(WorkspaceSurface::Details, cx);
        }
        self.details_tab = tab;
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
        self.diff_selection.clear();
        self.selected_turn = None;
        self.tab_transition_generation = self.tab_transition_generation.wrapping_add(1);
        if tab == InspectorTab::Changes {
            self.refresh(true, cx);
        } else if tab == InspectorTab::Info
            && let Some(context) = self.context.clone()
        {
            // Info can have been hidden while the provider appended turns.
            // Activation performs one version check; it does not start a
            // transcript poll.
            self.refresh_transcript(&context, false, cx);
        }
        self.reconcile_diff_polling(cx);
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn active_workspace_id(&self) -> Option<u64> {
        self.workspace_active
    }

    pub(crate) fn terminal_slot(&self) -> usize {
        self.workspace_tabs
            .iter()
            .find(|tab| Some(tab.id) == self.workspace_active)
            .and_then(|tab| tab.terminal_slot)
            .unwrap_or(0)
    }

    /// ⌘W while this inspector is focused closes the tab in front, same as its X.
    /// A focused browser page counts too: its web view is not this focus handle.
    pub(crate) fn close_focused_workspace(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.visible {
            return false;
        }
        #[cfg(target_os = "macos")]
        let browser_focused = self
            .native_browser
            .as_ref()
            .is_some_and(|browser| browser.borrow().has_focus());
        #[cfg(not(target_os = "macos"))]
        let browser_focused = false;
        if !self.is_focused(window) && !browser_focused && !self.api_focused(window, cx) {
            return false;
        }
        self.close_active_workspace(cx)
    }

    /// ⌘W on the focused shell closes this tab. Leaving it open shows an empty
    /// t("panel.select_session") pane that cannot select anything.
    pub(crate) fn close_active_terminal(&mut self, cx: &mut Context<Self>) -> bool {
        if self.workspace_selected != Some(WorkspaceSurface::Terminal) {
            return false;
        }
        self.close_active_workspace(cx)
    }

    /// ⌘W while this inspector is focused closes the tab in front, same as its X.
    pub(crate) fn close_active_workspace(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.workspace_active else {
            return false;
        };
        self.close_workspace(id, cx);
        true
    }

    pub(crate) fn select_workspace(&mut self, surface: WorkspaceSurface, cx: &mut Context<Self>) {
        self.sync_workspace_session(cx);
        if self.workspace_selected == Some(surface) && self.workspace_active.is_some() {
            self.workspace_chooser_open = false;
            cx.notify();
            return;
        }
        if let Some(tab) = self
            .workspace_tabs
            .iter()
            .find(|tab| tab.surface == surface)
        {
            self.activate_workspace(tab.id, cx);
        } else {
            self.add_workspace(surface, cx);
        }
    }

    /// A fresh tab with its own identity and, for Files, its own viewer.
    fn new_workspace_tab(
        &mut self,
        surface: WorkspaceSurface,
        cx: &mut Context<Self>,
    ) -> WorkspaceTab {
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;
        let mut tab = WorkspaceTab::new(id, surface);
        if surface == WorkspaceSurface::Files {
            let (colors, (theme, font)) = {
                let store = self.runtime.store.read().expect("store");
                (
                    crate::app_theme::sidebar_colors_in(&store),
                    code_style(&store),
                )
            };
            let viewer = cx.new(|cx| {
                let mut viewer = CodeViewer::new(self.tokio.clone(), colors, cx);
                viewer.set_terminal_style(theme, &font, cx);
                viewer
            });
            cx.observe(&viewer, |_, _, cx| cx.notify()).detach();
            let cwd = self
                .selected_context()
                .filter(|context| !context.remote)
                .map(|context| context.cwd);
            viewer.update(cx, |viewer, cx| viewer.set_workspace(cwd, cx));
            tab.viewer = Some(viewer);
        }
        tab
    }

    fn add_workspace(&mut self, surface: WorkspaceSurface, cx: &mut Context<Self>) {
        let mut tab = self.new_workspace_tab(surface, cx);
        let id = tab.id;
        if surface == WorkspaceSurface::Terminal {
            tab.terminal_slot = Some(self.next_terminal_slot);
            self.next_terminal_slot += 1;
        }
        if surface == WorkspaceSurface::Api {
            let project = self.selected_session().map(|session| session.project_id.0);
            tab.api = Some(self.new_api_client(project.as_deref(), cx));
        }
        self.workspace_tabs.push(tab);
        self.activate_workspace(id, cx);
    }

    fn activate_workspace(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(surface) = self.load_workspace(id, cx) {
            cx.emit(InspectorEvent::WorkspaceChanged(surface));
        }
    }

    fn load_workspace(&mut self, id: u64, cx: &mut Context<Self>) -> Option<WorkspaceSurface> {
        self.workspace_chooser_open = false;
        if self.workspace_active == Some(id) {
            cx.notify();
            return None;
        }
        let index = self.workspace_tabs.iter().position(|tab| tab.id == id)?;
        let previous_index = self
            .workspace_tabs
            .iter()
            .position(|tab| Some(tab.id) == self.workspace_active);
        self.save_active_workspace();
        self.tab_direction = if previous_index.is_none_or(|previous| index >= previous) {
            1.0
        } else {
            -1.0
        };
        let tab = &self.workspace_tabs[index];
        let surface = tab.surface;
        if let Some(viewer) = &tab.viewer {
            self.code_viewer = viewer.clone();
        }
        self.details_tab = tab.details_tab;
        self.scroll = tab.scroll.clone();
        self.diff_layer = tab.diff_layer;
        self.comparison = tab.comparison;
        self.browser_query = tab.browser_query.clone();
        self.browser_state = tab.browser_state.clone();
        self.workspace_active = Some(id);
        self.workspace_tab_scroll.scroll_to_item(index);
        self.diff_selection.clear();
        self.selected_turn = None;
        self.tab_transition_generation = self.tab_transition_generation.wrapping_add(1);
        self.workspace_selected = Some(surface);
        self.comparison_menu_open = false;
        self.files_open = false;
        self.status_evidence_open = false;
        self.commit_open = false;
        self.ask_draft = None;
        self.browser_address_focused = false;
        let preference_tab = match surface {
            WorkspaceSurface::Files => Some(InspectorTab::Code),
            WorkspaceSurface::Review => Some(InspectorTab::Changes),
            WorkspaceSurface::Details => Some(self.details_tab),
            WorkspaceSurface::Browser | WorkspaceSurface::Terminal | WorkspaceSurface::Api => None,
        };
        if let Some(tab) = preference_tab {
            self.selected_tab = tab;
            let _ = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned")
                .update_preferences(|prefs| prefs.inspector_tab = tab);
        }
        if surface == WorkspaceSurface::Review {
            self.refresh(true, cx);
        }
        if surface == WorkspaceSurface::Terminal {
            cx.emit(InspectorEvent::RequestTerminal);
        }
        if surface == WorkspaceSurface::Details
            && self.details_tab == InspectorTab::Info
            && let Some(context) = self.context.clone()
        {
            self.refresh_transcript(&context, false, cx);
        }
        self.reconcile_diff_polling(cx);
        cx.notify();
        Some(surface)
    }

    fn close_workspace(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.workspace_tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let tab = self.workspace_tabs.remove(index);
        self.workspace_chooser_open = false;
        if self.workspace_active == Some(id) {
            self.workspace_active = None;
            self.workspace_selected = None;
            if let Some(next) = self
                .workspace_tabs
                .get(index.min(self.workspace_tabs.len().saturating_sub(1)))
            {
                self.activate_workspace(next.id, cx);
            }
        }
        self.reconcile_diff_polling(cx);
        cx.emit(InspectorEvent::WorkspaceClosed {
            surface: tab.surface,
            id,
            terminal_slot: tab.terminal_slot,
        });
        // An empty panel has nothing to offer, so the last tab takes the panel
        // with it. The next open seeds a default tab (`seed_default_workspace`).
        if self.workspace_tabs.is_empty() && self.visible {
            cx.emit(InspectorEvent::Close);
        }
        cx.notify();
    }

    /// Gives an emptied panel a tab to open on: the user's last details
    /// destination, so reopening never lands on an empty chooser. Restored
    /// rather than activated, so it does not pull keyboard focus.
    fn seed_default_workspace(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_tabs.is_empty() {
            return;
        }
        let surface = crate::right_panel::default_surface(self.selected_tab);
        let mut tab = self.new_workspace_tab(surface, cx);
        let id = tab.id;
        if surface == WorkspaceSurface::Details {
            tab.details_tab = self.details_tab;
        }
        self.workspace_tabs.push(tab);
        if let Some(surface) = self.load_workspace(id, cx) {
            cx.emit(InspectorEvent::WorkspaceRestored(surface));
        }
    }

    fn select_comparison(&mut self, comparison: SessionDiffBase, cx: &mut Context<Self>) {
        self.comparison_menu_open = false;
        if self.comparison == comparison {
            cx.notify();
            return;
        }
        self.comparison = comparison;
        self.omitted_untracked_open = false;
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
        self.refresh(true, cx);
    }

    fn select_diff_layer(&mut self, layer: DiffLayer, cx: &mut Context<Self>) {
        self.files_open = false;
        self.armed_hunk = None;
        self.diff_selection.clear();
        self.selected_turn = None;
        self.discard_armed = false;
        self.commit_open = false;
        if self.diff_layer == layer {
            cx.notify();
            return;
        }
        self.diff_layer = layer;
        self.omitted_untracked_open = false;
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
        self.refresh(true, cx);
    }

    /// Expands or collapses the omitted-untracked notice. The open state
    /// survives a Git refresh so staging one listed file keeps the list open.
    fn toggle_omitted_untracked(&mut self, cx: &mut Context<Self>) {
        self.omitted_untracked_open = !self.omitted_untracked_open;
        self.scrollbar_layout_primed = false;
        cx.notify();
    }

    fn jump_to_diff_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.files_open = false;
        if let Some(position) = self.diff_position(row) {
            self.scroll.scroll_to_item(position, ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// The list position showing snapshot row `row` in the diff on screen.
    fn diff_position(&mut self, row: usize) -> Option<usize> {
        let snapshot = self.displayed_diff()?.clone();
        let omitted_open =
            self.omitted_untracked_open && snapshot.omitted_untracked_notice_row().is_some();
        let built = self.review_ui.rows_for(&snapshot, omitted_open);
        crate::git_ui::rows::position_of(&built.rows, row)
    }

    /// The diff the review is showing: the picked commit's in Commits, the
    /// selected lane's otherwise.
    fn displayed_diff(&self) -> Option<&Arc<DiffSnapshot>> {
        if self.review_ui.mode == ReviewMode::Commits {
            return self.review_ui.commit_snapshot();
        }
        match &self.state {
            LoadState::Ready(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    fn reset_diff_scroll(&mut self) {
        self.scroll = UniformListScrollHandle::new();
        self.scrollbar_interaction = ScrollbarInteraction::default();
        self.scrollbar_layout_primed = false;
    }

    fn set_review_mode(&mut self, mode: ReviewMode, cx: &mut Context<Self>) {
        self.files_open = false;
        self.comparison_menu_open = false;
        if self.review_ui.mode == mode {
            cx.notify();
            return;
        }
        self.review_ui.mode = mode;
        self.diff_selection.clear();
        self.armed_hunk = None;
        self.discard_armed = false;
        self.commit_open = false;
        self.reset_diff_scroll();
        if mode == ReviewMode::Commits {
            self.load_commit_history(false, cx);
        }
        cx.notify();
    }

    fn set_diff_layout(&mut self, layout: DiffLayout, cx: &mut Context<Self>) {
        if self.review_ui.layout == layout {
            return;
        }
        // The same rows sit at other list positions in the other layout, so
        // keep the selection (or the top) in view across the switch.
        let anchor = self.diff_selection.head();
        self.review_ui.layout = layout;
        self.scrollbar_layout_primed = false;
        if let Some(position) = anchor.and_then(|row| self.diff_position(row)) {
            self.scroll.scroll_to_item(position, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Reloads the history while Commits is showing and HEAD has moved (a new
    /// commit, a rebase, a checkout). Rides the review's status poll, so an
    /// idle branch runs no extra Git.
    fn reconcile_commit_history(&mut self, cx: &mut Context<Self>) {
        if self.review_ui.mode != ReviewMode::Commits || !self.visible {
            return;
        }
        let ReviewLoadState::Ready(status) = &self.review_state else {
            return;
        };
        let head = status.branch.oid.clone();
        let stale = match &self.review_ui.history {
            HistoryLoad::Ready(loaded) => loaded.history.head != head,
            HistoryLoad::Idle => true,
            HistoryLoad::Loading | HistoryLoad::Error(_) => false,
        };
        if stale {
            self.load_commit_history(true, cx);
        }
    }

    fn load_commit_history(&mut self, background: bool, cx: &mut Context<Self>) {
        let Some(context) = self.context.clone() else {
            return;
        };
        if context.remote {
            self.review_ui.history = HistoryLoad::Idle;
            return;
        }
        if !background || !matches!(self.review_ui.history, HistoryLoad::Ready(_)) {
            self.review_ui.history = HistoryLoad::Loading;
        }
        self.review_ui.history_generation = self.review_ui.history_generation.wrapping_add(1);
        let generation = self.review_ui.history_generation;
        let cwd = context.cwd;
        self.review_ui.history_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let repository = GitRepository::discover(&cwd)?;
                    repository
                        .commit_history(COMMIT_HISTORY_LIMIT)
                        .map(LoadedHistory::new)
                })
                .await
                .map_err(|error: GitReviewError| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.review_ui.history_generation != generation {
                    return;
                }
                match result {
                    Ok(loaded) => {
                        // A picked commit that left the branch (rebased or
                        // reset away) is let go with its diff.
                        let picked_gone =
                            this.review_ui
                                .selected_commit
                                .as_ref()
                                .is_some_and(|picked| {
                                    !loaded.history.commits.iter().any(|c| &c.oid == picked)
                                });
                        if picked_gone {
                            this.review_ui.clear_commit();
                            this.diff_selection.clear();
                        }
                        this.review_ui.history = HistoryLoad::Ready(Arc::new(loaded));
                    }
                    Err(error) => this.review_ui.history = HistoryLoad::Error(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Shows commit `index`'s diff below the list; picking it again closes it.
    fn select_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(loaded) = self.review_ui.loaded_history().cloned() else {
            return;
        };
        let Some(commit) = loaded.history.commits.get(index) else {
            return;
        };
        self.diff_selection.clear();
        self.armed_hunk = None;
        self.reset_diff_scroll();
        if self.review_ui.selected_commit.as_deref() == Some(commit.oid.as_str()) {
            self.review_ui.clear_commit();
            cx.notify();
            return;
        }
        let Some(context) = self.context.clone().filter(|context| !context.remote) else {
            return;
        };
        let oid = commit.oid.clone();
        let parent = commit.parents.first().cloned();
        self.review_ui.clear_commit();
        self.review_ui
            .commit_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        self.review_ui.selected_commit = Some(oid.clone());
        self.review_ui.commit_diff = Some(CommitDiffLoad::Loading);
        let generation = self.review_ui.commit_diff_generation;
        let cwd = context.cwd;
        self.review_ui.commit_diff_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_commit_diff(&cwd, &oid, parent.as_deref()) })
                .await
                .map_err(|error| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.review_ui.commit_diff_generation != generation {
                    return;
                }
                this.review_ui.commit_diff = Some(match result {
                    Ok(snapshot) => CommitDiffLoad::Ready(Arc::new(snapshot)),
                    Err(error) => CommitDiffLoad::Error(error),
                });
                this.scrollbar_layout_primed = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn diff_file_action(
        &mut self,
        snapshot: &DiffSnapshot,
        file: usize,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = snapshot.file_diffs.get(file) else {
            return;
        };
        let path = file.path.clone();
        match action {
            FileAction::Toggle => {
                self.review_ui.toggle_collapsed(path);
                self.scrollbar_layout_primed = false;
                cx.notify();
            }
            FileAction::Open => self.open_file_reference(
                snapshot.repo_root.clone(),
                path.to_string_lossy().into_owned(),
                cx,
            ),
            FileAction::Ask => {
                let evidence = ReviewEvidence::File {
                    path,
                    layer: prompt_layer(snapshot.layer),
                    patch: file
                        .hunks
                        .iter()
                        .map(|hunk| String::from_utf8_lossy(&hunk.patch))
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                self.open_ask(vec![evidence], window, cx);
            }
            FileAction::Stage => self.run_review_action(ReviewAction::Stage(vec![path]), cx),
            FileAction::Unstage => self.run_review_action(ReviewAction::Unstage(vec![path]), cx),
        }
    }

    fn diff_hunk_action(
        &mut self,
        snapshot: &DiffSnapshot,
        (file, hunk): (usize, usize),
        action: HunkAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = snapshot.file_diffs.get(file) else {
            return;
        };
        let Some(hunk) = file.hunks.get(hunk) else {
            return;
        };
        match action {
            HunkAction::Ask => {
                let evidence = ReviewEvidence::Hunk {
                    path: file.path.clone(),
                    layer: prompt_layer(snapshot.layer),
                    header: hunk.header.clone(),
                    patch: String::from_utf8_lossy(&hunk.patch).into_owned(),
                };
                self.open_ask(vec![evidence], window, cx);
            }
            HunkAction::Stage => self.run_review_action(
                ReviewAction::Patch {
                    patch: hunk.patch.clone(),
                    mutation: PatchMutation::Stage,
                },
                cx,
            ),
            HunkAction::Unstage => self.run_review_action(
                ReviewAction::Patch {
                    patch: hunk.patch.clone(),
                    mutation: PatchMutation::Unstage,
                },
                cx,
            ),
            HunkAction::Discard => {
                if self.armed_hunk == Some(hunk.fingerprint) {
                    self.run_review_action(
                        ReviewAction::Patch {
                            patch: hunk.patch.clone(),
                            mutation: PatchMutation::Discard,
                        },
                        cx,
                    );
                } else {
                    self.armed_hunk = Some(hunk.fingerprint);
                    self.review_feedback =
                        Some((false, t("panel.review.confirm_discard_hint").to_owned()));
                    cx.notify();
                }
            }
        }
    }

    fn omitted_path_action(
        &mut self,
        snapshot: &DiffSnapshot,
        ordinal: usize,
        action: OmittedAction,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = snapshot.omitted_untracked_paths.get(ordinal).cloned() else {
            return;
        };
        match action {
            OmittedAction::Open => self.open_file_reference(
                snapshot.repo_root.clone(),
                path.to_string_lossy().into_owned(),
                cx,
            ),
            OmittedAction::Stage => self.run_review_action(ReviewAction::Stage(vec![path]), cx),
        }
    }

    /// The review's palette and code font: the terminal theme's diff tints
    /// over the panel's semantic colors, in the terminal's font family.
    fn review_look(&self, colors: SemanticColors) -> (DiffPalette, SharedString) {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        let theme = crate::app_theme::terminal_theme_in(&store);
        let font = crate::fonts::terminal_family(&store.preferences().terminal_font_family);
        (
            DiffPalette::new(colors, &theme),
            SharedString::from(font.to_owned()),
        )
    }

    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.visible || (self.loading && !force) {
            return;
        }
        let Some(context) = self.selected_context() else {
            self.context = None;
            self.state = LoadState::NoSession;
            self.review_state = ReviewLoadState::NoSession;
            self.transcript_state = TranscriptLoadState::Unavailable;
            self.transcript_version = None;
            self.transcript_task = None;
            for tab in &self.workspace_tabs {
                if let Some(viewer) = &tab.viewer {
                    viewer.update(cx, |viewer, cx| viewer.set_workspace(None, cx));
                }
            }
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
            self.omitted_untracked_open = false;
            self.diff_selection.clear();
            self.review_ui.reset_for_context();
            self.selected_turn = None;
            self.ask_draft = None;
            self.ask_feedback = None;
            self.ask_query.clear();
            self.status_evidence_open = false;
            self.transcript_version = None;
            let workspace = (!context.remote).then(|| context.cwd.clone());
            for tab in &self.workspace_tabs {
                if let Some(viewer) = &tab.viewer {
                    viewer.update(cx, |viewer, cx| viewer.set_workspace(workspace.clone(), cx));
                }
            }
        }
        self.context = Some(context.clone());
        if context_changed || force {
            self.refresh_transcript(&context, false, cx);
        }
        if !context.remote {
            crate::workspace_follow::refresh_staleness(self, &context.id, context.cwd.clone(), cx);
        }
        self.refresh_review(&context, force, cx);
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
        let layer = self.diff_layer;
        let client = Arc::clone(self.runtime.client());
        let tokio = self.tokio.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = if remote {
                let read = tokio
                    .spawn(async move { client.read_diff(&session_id, comparison).await })
                    .await
                    .map_err(|error| tf("panel.review.diff_stopped", &[("error", &error)]))
                    .and_then(|result| result.map_err(|error| error.to_string()));
                // Parsing also marks changed words; keep it off the UI thread.
                cx.background_spawn(async move { read.map(snapshot_from_read_diff) })
                    .await
                    .map(Arc::new)
            } else {
                cx.background_spawn(async move { load_local_diff(&cwd, layer) })
                    .await
                    .map_err(|error| error.to_string())
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
                    // Row indices are only meaningful for the snapshot they
                    // came from. Clearing avoids silently quoting a different
                    // hunk after a live Git refresh inserts or removes rows.
                    // A commit's diff is not the snapshot being replaced.
                    if this.review_ui.mode == ReviewMode::Changes {
                        this.diff_selection.clear();
                    }
                    this.scrollbar_layout_primed = false;
                    cx.notify();
                }
            });
        }));
    }

    fn refresh_transcript(
        &mut self,
        context: &DiffContext,
        debounce: bool,
        cx: &mut Context<Self>,
    ) {
        self.transcript_task = None;
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        let generation = self.transcript_generation;
        let supported = matches!(
            context.kind.id(),
            ProtoAgentKind::CLAUDE_CODE_ID | ProtoAgentKind::CODEX_ID
        );
        let Some((path, agent_id)) = context
            .transcript_path
            .clone()
            .zip(context.agent_session_id.clone())
            .filter(|_| !context.remote && supported)
        else {
            self.transcript_state = TranscriptLoadState::Unavailable;
            self.transcript_version = None;
            return;
        };
        let kind = context.kind.clone();
        let cwd = context.launch_cwd.to_string_lossy().into_owned();
        let home = self.transcript_home.clone();
        let previous = self.transcript_version;
        if previous.is_none() {
            self.transcript_state = TranscriptLoadState::Loading;
        }
        self.transcript_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(TRANSCRIPT_REFRESH_DEBOUNCE)
                    .await;
            }
            let result = cx
                .background_spawn(async move {
                    load_transcript(&home, &path, &kind, &agent_id, &cwd, previous)
                })
                .await
                .map_err(|_| ());
            let _ = this.update(cx, |this, cx| {
                if this.transcript_generation != generation {
                    return;
                }
                match result {
                    Ok(Some(snapshot)) => {
                        this.transcript_version = Some(snapshot.version);
                        this.transcript_state =
                            TranscriptLoadState::Ready(Arc::new(snapshot.document));
                    }
                    Ok(None) => {}
                    Err(()) => {
                        this.transcript_version = None;
                        this.transcript_state = TranscriptLoadState::Error;
                    }
                }
                cx.notify();
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
        self.review_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let repository = GitRepository::discover(&cwd)?;
                    repository.status()
                })
                .await
                .map_err(|error: GitReviewError| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if this.review_generation != generation {
                    return;
                }
                this.review_state = match result {
                    Ok(status) => ReviewLoadState::Ready(Arc::new(status)),
                    Err(error) => ReviewLoadState::Error(error),
                };
                this.reconcile_commit_history(cx);
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
        let is_commit = matches!(action, ReviewAction::Commit(_));
        cx.notify();
        self.review_action_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let repository = GitRepository::discover(&context.cwd)?;
                    match action {
                        ReviewAction::Stage(paths) => {
                            repository.stage_paths(&paths)?;
                            Ok(t("panel.review.staged").to_owned())
                        }
                        ReviewAction::Unstage(paths) => {
                            repository.unstage_paths(&paths)?;
                            Ok(t("panel.review.unstaged").to_owned())
                        }
                        ReviewAction::Discard(paths) => {
                            repository.discard_unstaged(&paths)?;
                            Ok(t("panel.review.discarded").to_owned())
                        }
                        ReviewAction::Patch { patch, mutation } => {
                            repository.apply_patch(&patch, mutation)?;
                            Ok(t(match mutation {
                                PatchMutation::Stage => "panel.review.hunk_staged",
                                PatchMutation::Unstage => "panel.review.hunk_unstaged",
                                PatchMutation::Discard => "panel.review.hunk_discarded",
                            })
                            .to_owned())
                        }
                        ReviewAction::Commit(message) => {
                            let commit = repository.commit(&message)?;
                            Ok(tf(
                                "panel.review.committed",
                                &[("oid", &commit.oid), ("summary", &commit.summary)],
                            ))
                        }
                    }
                })
                .await
                .map_err(|error: GitReviewError| error.to_string());
            let _ = this.update(cx, |this, cx| {
                this.review_action_busy = false;
                match result {
                    Ok(message) => {
                        this.review_feedback = Some((true, message));
                        // Staging, unstaging, and discarding share this path
                        // with the composer open; only a landed commit
                        // consumes the draft message.
                        if is_commit {
                            this.commit_open = false;
                            this.commit_query.clear();
                        }
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
            self.review_feedback = Some((false, t("panel.review.need_message").to_owned()));
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
            tf("panel.review.contexts", &[("count", &evidence.len())])
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
            self.ask_feedback = Some((false, t("panel.ask.select_agent").to_owned()));
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
                .map_err(|error| tf("panel.ask.send_stopped", &[("error", &error)]))
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.ask_busy = false;
                match result {
                    Ok(()) => {
                        this.ask_feedback =
                            Some((true, tf("panel.ask.sent", &[("subject", &subject)])));
                        this.ask_query.clear();
                    }
                    Err(error) => this.ask_feedback = Some((false, error)),
                }
                cx.notify();
            });
        }));
    }

    /// A saved workspace supplies this window's focused pane; it never rewrites
    /// the shared session selection used by other windows.
    pub(crate) fn set_session_context(
        &mut self,
        context: Option<Option<SessionId>>,
        cx: &mut Context<Self>,
    ) {
        if self.session_context != context {
            self.session_context = context;
            self.refresh_if_context_changed(cx);
            cx.notify();
        }
    }

    /// Resolves which checkout the selected Session's Agent works in when
    /// its evidence changed; `reveal` also samples its process directory and
    /// re-reads staleness. See `crate::workspace_follow`.
    fn sync_workspace_follow(&mut self, reveal: bool, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let reveal = reveal
            || (self.visible
                && self
                    .context
                    .as_ref()
                    .is_some_and(|context| context.id != session.id));
        if reveal {
            self.follow.forget_staleness(&session.id);
        }
        let runtime = Arc::clone(&self.runtime);
        let tokio = self.tokio.clone();
        crate::workspace_follow::sync(self, &session, reveal && self.visible, &runtime, &tokio, cx);
    }

    fn render_worktree_follow(
        &self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        matches!(
            self.workspace_selected,
            Some(WorkspaceSurface::Details | WorkspaceSurface::Review | WorkspaceSurface::Files)
        )
        .then(|| crate::workspace_follow::render_bar(&self.follow, session, colors, cx))
        .flatten()
    }

    fn selected_session(&self) -> Option<SessionRecord> {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        match &self.session_context {
            Some(id) => id
                .as_ref()
                .and_then(|id| store.sessions().get(id))
                .map(AsRef::as_ref),
            None => store.selected_session(),
        }
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
        let mut track = details_ui::segmented_track(colors);

        for tab in InspectorTab::DETAILS {
            let count = match tab {
                InspectorTab::Info => None,
                InspectorTab::Changes => changes_count,
                InspectorTab::Code => None,
                InspectorTab::Artifacts => artifacts_count,
            };
            track = track.child(
                details_ui::segment(
                    SharedString::from(format!("inspector-tab-{}", tab.label())),
                    tab.label(),
                    count,
                    tab == selected_tab,
                    colors,
                )
                .debug_selector(move || tab.debug_selector().to_owned())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_tab(tab, cx);
                    cx.stop_propagation();
                })),
            );
        }

        div()
            .h(px(40.0))
            .flex_none()
            .px(px(details_ui::CONTENT_INSET))
            .flex()
            .items_center()
            .child(track)
    }

    /// What an emptied panel shows if it is ever open with nothing in it.
    /// Closing the last tab closes the panel and reopening seeds a default
    /// tab, so this is a fallback rather than a destination: one quiet line
    /// and a row of plain choices.
    fn render_surface_chooser(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let mut choices = div()
            .max_w(px(260.0))
            .flex()
            .flex_wrap()
            .justify_center()
            .gap(px(2.0));
        for surface in WorkspaceSurface::CATALOG {
            choices = choices.child(
                div()
                    .id(SharedString::from(format!(
                        "workspace-open-{}",
                        surface.label()
                    )))
                    .debug_selector(move || format!("workspace-open-{}", surface.label()))
                    .h(px(crate::right_panel::TAB_HEIGHT))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.secondary)
                    .hover(move |choice| {
                        choice
                            .bg(crate::right_panel::tab_hover_fill(colors))
                            .text_color(colors.primary)
                    })
                    .child(sf_symbol(surface.icon(), 11.0, colors.tertiary))
                    .child(surface.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_workspace(surface, cx);
                        cx.stop_propagation();
                    })),
            );
        }
        div()
            .id("workspace-empty")
            .debug_selector(|| "workspace-empty".into())
            .size_full()
            .p(px(20.0))
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(t("panel.no_open_tabs")),
            )
            .child(choices)
            .into_any_element()
    }

    /// One row of the + menu.
    fn workspace_catalog_items(&self, colors: SemanticColors, cx: &mut Context<Self>) -> gpui::Div {
        let mut items = div().p(px(4.0)).flex().flex_col().gap(px(1.0));
        for surface in WorkspaceSurface::CATALOG {
            items = items.child(
                div()
                    .id(SharedString::from(format!(
                        "workspace-catalog-{}",
                        surface.label()
                    )))
                    .debug_selector(move || format!("workspace-catalog-{}", surface.label()))
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.08)))
                    .child(
                        div()
                            .w(px(14.0))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(sf_symbol(surface.icon(), 11.5, colors.secondary)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Typo::ROW.size))
                            .text_color(colors.primary)
                            .child(surface.label()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_chooser_open = false;
                        this.add_workspace(surface, cx);
                        cx.stop_propagation();
                    })),
            );
        }
        items
    }

    /// The + menu's pixels for its floating panel.
    fn add_menu_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.workspace_chooser_open {
            return None;
        }
        let colors = self.panel_colors();
        let items = self.workspace_catalog_items(colors, cx);
        Some(
            crate::floating::surface(colors, crate::floating::MENU_RADIUS, ADD_MENU_WIDTH, items)
                .into_any_element(),
        )
    }

    /// The + menu, dropped from its button. A scrim over the panel and a
    /// click anywhere else both dismiss it, so the menu never outlives the
    /// pointer leaving it.
    fn render_add_menu(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let top = Metrics::TITLE_BAR - 4.0;
        // Right-aligned under the + button, which sits just before the toggle.
        let right = Metrics::TOOLBAR_EDGE_INSET
            + Metrics::TOOLBAR_CONTROL_SIZE
            + Metrics::TOOLBAR_COMPACT_GAP;
        let scrim = div()
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.workspace_chooser_open = false;
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.workspace_chooser_open = false;
                cx.notify();
            }));
        let menu = if crate::floating::uses_panels(false, colors, cx) {
            crate::floating::host_here(
                INSPECTOR_ADD_MENU,
                crate::floating::surface(
                    colors,
                    crate::floating::MENU_RADIUS,
                    ADD_MENU_WIDTH,
                    self.workspace_catalog_items(colors, cx),
                )
                .into_any_element(),
                Some(ADD_MENU_WIDTH),
                gpui::Anchor::TopRight,
                8.0,
                cx,
            )
            .absolute()
            .top(px(top))
            .right(px(right))
            .w(px(0.0))
            .h(px(0.0))
            .into_any_element()
        } else {
            div()
                .id("workspace-surface-catalog")
                .debug_selector(|| "workspace-surface-catalog".into())
                .absolute()
                .top(px(top))
                .right(px(right))
                .w(px(ADD_MENU_WIDTH))
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    FloatingSurface::new(colors, self.workspace_catalog_items(colors, cx))
                        .radius(crate::floating::MENU_RADIUS),
                )
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .child(scrim)
            .child(menu)
            .into_any_element()
    }

    /// The panel's title bar: the workspace tabs, then + and the toggle that
    /// hides the panel. The toggle lands exactly where the session pane's
    /// toggle stands while the panel is closed, so the control does not move
    /// as the panel slides.
    fn render_workspace_header(
        &self,
        colors: SemanticColors,
        held_hint: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.workspace_active;
        let active_index = self
            .workspace_tabs
            .iter()
            .position(|tab| Some(tab.id) == selected);
        let scroll = self.workspace_tab_scroll.clone();
        let previous_width = self.workspace_tab_width.clone();
        let inspector = cx.entity().downgrade();
        let mut tabs = div()
            .on_children_prepainted(move |_, window, _| {
                let width = f32::from(scroll.bounds().size.width);
                if previous_width.replace(width) != width
                    && let Some(tab) = active_index.and_then(|index| scroll.bounds_for_item(index))
                {
                    // The first reveal can precede measured scroll bounds.
                    // Reconcile after layout, including dock resizes, without
                    // snapping back during ordinary horizontal scrolling.
                    let viewport = scroll.bounds();
                    let mut offset = scroll.offset();
                    if tab.left() + offset.x < viewport.left() {
                        offset.x = viewport.left() - tab.left();
                    } else if tab.right() + offset.x > viewport.right() {
                        offset.x = viewport.right() - tab.right();
                    }
                    if offset != scroll.offset() {
                        scroll.set_offset(offset);
                        let inspector = inspector.clone();
                        window.on_next_frame(move |_, cx| {
                            let _ = inspector.update(cx, |_, cx| cx.notify());
                        });
                        window.request_animation_frame();
                    }
                }
            })
            .id("workspace-surface-tabs")
            .overflow_x_scroll()
            .track_scroll(&self.workspace_tab_scroll)
            .min_w(px(0.0))
            .flex_1()
            .flex()
            .items_center()
            .gap(px(2.0));

        for tab in &self.workspace_tabs {
            let surface = tab.surface;
            let id = tab.id;
            let active = selected == Some(id);
            let label = if surface == WorkspaceSurface::Browser {
                (if active {
                    &self.browser_state
                } else {
                    &tab.browser_state
                })
                .title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| surface.label().into())
            } else if let Some(label) = tab
                .viewer
                .as_ref()
                .and_then(|viewer| viewer.read(cx).tab_label())
            {
                label
            } else {
                let count = self
                    .workspace_tabs
                    .iter()
                    .filter(|other| other.surface == surface)
                    .count();
                let ordinal = self
                    .workspace_tabs
                    .iter()
                    .filter(|other| other.surface == surface)
                    .position(|other| other.id == id)
                    .unwrap_or(0)
                    + 1;
                if count > 1 {
                    format!("{} {ordinal}", surface.label())
                } else {
                    surface.label().into()
                }
            };
            let group = SharedString::from(format!("workspace-tab-group-{id}"));
            let tint = if active {
                colors.primary
            } else {
                colors.tertiary
            };
            let glyph = {
                let state = if active {
                    &self.browser_state
                } else {
                    &tab.browser_state
                };
                if surface == WorkspaceSurface::Browser && state.is_loading {
                    sf_symbol("arrow.triangle.2.circlepath", 10.5, tint)
                } else if surface == WorkspaceSurface::Browser
                    && let Some(favicon) = &state.favicon
                {
                    use gpui::StyledImage;
                    gpui::img(favicon.clone())
                        .size(px(13.0))
                        .flex_none()
                        .with_fallback(move || sf_symbol("network", 10.5, tint))
                        .into_any_element()
                } else {
                    sf_symbol(surface.icon(), 10.5, tint)
                }
            };
            // The close slot is always laid out, so a tab never changes width
            // as its close control comes and goes; only its visibility follows
            // selection and the pointer.
            let close = div()
                .id(SharedString::from(format!("close-workspace-{}", id)))
                .debug_selector(move || format!("close-workspace-{id}"))
                .role(gpui::Role::Button)
                .aria_label(t("panel.close_tab"))
                .size(px(crate::right_panel::TAB_CLOSE_SIZE))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::BADGE))
                .when(
                    !crate::right_panel::tab_close_visible(active, false),
                    |close| {
                        close
                            .invisible()
                            .group_hover(group.clone(), |close| close.visible())
                    },
                )
                .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_workspace(id, cx);
                    cx.stop_propagation();
                }))
                .child(sf_symbol("xmark", 8.0, colors.secondary));
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("workspace-tab-{}", id)))
                    .debug_selector(move || format!("workspace-tab-{id}"))
                    .group(group)
                    .role(gpui::Role::Tab)
                    .aria_selected(active)
                    .aria_label(label.clone())
                    .h(px(crate::right_panel::TAB_HEIGHT))
                    .flex_none()
                    .max_w(px(crate::right_panel::TAB_MAX_WIDTH))
                    .min_w(px(0.0))
                    .pl(px(8.0))
                    .pr(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .when(active, |tab| {
                        tab.bg(crate::right_panel::tab_active_fill(colors))
                    })
                    .text_color(if active {
                        colors.primary
                    } else {
                        colors.secondary
                    })
                    .cursor_pointer()
                    .when(!active, |tab| {
                        tab.hover(move |tab| {
                            tab.bg(crate::right_panel::tab_hover_fill(colors))
                                .text_color(colors.primary)
                        })
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(glyph)
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .text_size(px(11.5))
                            // One weight for every state: a bolder selected
                            // label would widen the tab and shove its
                            // neighbours whenever the selection moved.
                            .font_weight(FontWeight::MEDIUM)
                            .child(label),
                    )
                    .child(close)
                    // A middle click closes, as in every tabbed editor.
                    .on_aux_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        if event.is_middle_click() {
                            this.close_workspace(id, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activate_workspace(id, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        let menu_open = self.workspace_chooser_open;
        div()
            .id("workspace-surface-header")
            .relative()
            .h(px(Metrics::TITLE_BAR))
            .flex_none()
            .pl(px(7.0))
            .pr(px(Metrics::TOOLBAR_EDGE_INSET))
            .flex()
            .items_center()
            .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
            .border_b_1()
            .border_color(crate::right_panel::panel_divider(colors))
            .child(tabs)
            .child(
                div()
                    .id("workspace-add-surface")
                    .debug_selector(|| "workspace-add-surface".into())
                    .role(gpui::Role::Button)
                    .aria_label(t("panel.new_tab"))
                    .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::BADGE))
                    .cursor_pointer()
                    .when(menu_open, |button| button.bg(Fill::subtle(colors)))
                    .hover(move |button| button.bg(Fill::subtle(colors)))
                    .child(sf_symbol("plus", 13.0, colors.secondary))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_chooser_open = !this.workspace_chooser_open;
                        cx.notify();
                        cx.stop_propagation();
                    })),
            )
            .child(crate::right_panel::toggle_button(
                "INSPECTOR_TOGGLE",
                true,
                colors,
                held_hint,
                cx.listener(|_, _: &gpui::ClickEvent, _, cx| cx.emit(InspectorEvent::Close)),
            ))
            .into_any_element()
    }

    fn browser_url(&self) -> Option<String> {
        let typed = self.browser_query.text().trim();
        if typed.is_empty() {
            return None;
        }
        crate::agent_catalog::normal_web_url(typed).or_else(|| {
            let candidate = url::Url::parse(&format!("https://{typed}")).ok()?;
            let local = candidate.host_str().is_some_and(|host| {
                host == "localhost"
                    || host == "[::1]"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            crate::agent_catalog::normal_web_url(&format!(
                "{}://{typed}",
                if local { "http" } else { "https" }
            ))
        })
    }

    pub(crate) fn focus_browser_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser_address_focused = true;
        self.browser_query.select_all();
        window.focus(&self.focus, cx);
        #[cfg(target_os = "macos")]
        if let Some(browser) = &self.native_browser {
            browser.borrow().focus_chrome();
        }
        cx.notify();
    }

    pub(crate) fn browser_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.visible || self.workspace_selected != Some(WorkspaceSurface::Browser) {
            return false;
        }
        let key = &event.keystroke;
        if !key.modifiers.platform || key.modifiers.control || key.modifiers.alt {
            return false;
        }
        if key.key.eq_ignore_ascii_case("l") {
            self.focus_browser_address(window, cx);
            return true;
        }
        let focused = self.focus.is_focused(window);
        #[cfg(target_os = "macos")]
        let focused = focused
            || self
                .native_browser
                .as_ref()
                .is_some_and(|browser| browser.borrow().has_focus());
        if !focused || key.modifiers.shift {
            return false;
        }
        match key.key.as_str() {
            "t" => {
                self.add_workspace(WorkspaceSurface::Browser, cx);
                // WorkspaceChanged resets root focus; defer until it has run.
                cx.defer_in(window, |this, window, cx| {
                    this.focus_browser_address(window, cx)
                });
            }
            "w" => {
                if let Some(id) = self.workspace_active {
                    self.close_workspace(id, cx);
                }
            }
            "r" => cx.emit(InspectorEvent::Browser(BrowserAction::Reload)),
            "[" => cx.emit(InspectorEvent::Browser(BrowserAction::Back)),
            "]" => cx.emit(InspectorEvent::Browser(BrowserAction::Forward)),
            _ => return false,
        }
        true
    }

    fn navigate_browser(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.browser_url() else {
            return;
        };
        self.browser_query.clear();
        self.browser_query.insert(&url);
        self.browser_address_focused = false;
        cx.emit(InspectorEvent::Browser(BrowserAction::Navigate(url)));
        cx.notify();
    }

    fn apply_browser_edit(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.browser_address_focused = false;
                self.browser_query.clear();
                if let Some(url) = &self.browser_state.url {
                    self.browser_query.insert(url);
                }
            }
            "enter" => self.navigate_browser(cx),
            _ => match query_editor::edit_for(&event.keystroke) {
                Some(Edit::Local(edit)) => {
                    self.browser_query.apply(edit);
                }
                Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                    query_editor::copy_selection(&self.browser_query, cx);
                }
                Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                    query_editor::cut_selection(&mut self.browser_query, cx);
                }
                Some(Edit::Clipboard(ClipboardEdit::Paste)) => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.browser_query.insert(&text);
                    }
                }
                None => return false,
            },
        }
        cx.notify();
        true
    }

    fn render_browser(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let has_url = self.browser_state.url.is_some() || self.browser_url().is_some();
        let nav_button = |id: &'static str,
                          symbol: &'static str,
                          action: BrowserAction,
                          enabled: bool,
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(26.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::BADGE))
                .text_color(if enabled {
                    colors.secondary
                } else {
                    colors.primary.alpha(0.24)
                })
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        .on_click(cx.listener(move |_this, _, _, cx| {
                            cx.emit(InspectorEvent::Browser(action.clone()));
                            cx.stop_propagation();
                        }))
                })
                .child(sf_symbol_weighted(
                    symbol,
                    10.5,
                    SymbolWeight::Semibold,
                    if enabled {
                        colors.secondary
                    } else {
                        colors.primary.alpha(0.24)
                    },
                ))
        };
        let url_label = if self.browser_query.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child(t("panel.browser.placeholder"))
                .into_any_element()
        } else {
            crate::navigation::query_label(&self.browser_query)
        };
        div()
            .id("workspace-browser")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(42.0))
                    .flex_none()
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .border_b_1()
                    .border_color(colors.primary.alpha(0.065))
                    .child(nav_button(
                        "browser-back",
                        "chevron.left",
                        BrowserAction::Back,
                        self.browser_state.can_go_back,
                        cx,
                    ))
                    .child(nav_button(
                        "browser-forward",
                        "chevron.right",
                        BrowserAction::Forward,
                        self.browser_state.can_go_forward,
                        cx,
                    ))
                    .child(nav_button(
                        "browser-reload",
                        "arrow.triangle.2.circlepath",
                        BrowserAction::Reload,
                        has_url,
                        cx,
                    ))
                    .child(
                        div()
                            .id("browser-address")
                            .debug_selector(|| "browser-address".into())
                            .min_w(px(0.0))
                            .flex_1()
                            .h(px(28.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .rounded(px(Radius::BADGE))
                            .bg(colors.primary.alpha(0.045))
                            .border_1()
                            .border_color(if self.browser_address_focused {
                                rgba(0x4f83f1cc)
                            } else {
                                colors.primary.alpha(0.075)
                            })
                            .text_size(px(10.5))
                            .text_color(colors.primary)
                            .cursor_text()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.focus_browser_address(window, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(url_label),
                    )
                    .child(
                        div()
                            .id("browser-open-external")
                            .size(px(26.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::BADGE))
                            .text_color(if has_url {
                                colors.secondary
                            } else {
                                colors.primary.alpha(0.24)
                            })
                            .when(has_url, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(url) = this
                                            .browser_state
                                            .url
                                            .clone()
                                            .or_else(|| this.browser_url())
                                        {
                                            cx.emit(InspectorEvent::Browser(
                                                BrowserAction::OpenExternal(url),
                                            ));
                                        }
                                        cx.stop_propagation();
                                    }))
                            })
                            .child(sf_symbol(
                                "link",
                                10.5,
                                if has_url {
                                    colors.secondary
                                } else {
                                    colors.primary.alpha(0.24)
                                },
                            )),
                    ),
            )
            .child(
                div()
                    .id("workspace-browser-loading-space")
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .text_center()
                    .text_color(colors.tertiary)
                    .when(!has_url, |body| {
                        body.child(sf_symbol("network", 26.0, colors.tertiary))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.secondary)
                                    .child(t("panel.browser.empty_title")),
                            )
                            .child(
                                div()
                                    .max_w(px(230.0))
                                    .text_size(px(11.0))
                                    .line_height(px(17.0))
                                    .child(t("panel.browser.empty_detail")),
                            )
                    })
                    .when_some(self.browser_state.error.clone(), |body, error| {
                        body.child(div().max_w(px(260.0)).text_size(px(12.0)).child(error))
                    })
                    .when(self.browser_state.is_loading, |body| {
                        body.child(div().text_size(px(10.0)).child(t("panel.loading")))
                    })
                    .map(|body| {
                        #[cfg(target_os = "macos")]
                        let body = body.when_some(self.native_browser.clone(), |body, browser| {
                            body.child(crate::macos::browser::NativeBrowser::surface(browser))
                        });
                        body
                    }),
            )
            .into_any_element()
    }

    fn render_terminal(&self, colors: SemanticColors) -> AnyElement {
        self.terminal_surface.clone().map_or_else(
            || {
                self.render_message(
                    colors,
                    "terminal",
                    t("panel.select_session"),
                    t("panel.terminal.empty"),
                )
                .into_any_element()
            },
            |terminal| {
                div()
                    .id("workspace-terminal")
                    .size_full()
                    .child(terminal)
                    .into_any_element()
            },
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
                    t("panel.select_session"),
                    t("panel.info.empty"),
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

        // Hero: who this is and what it is doing, without a box around it.
        let hero = div()
            .flex_none()
            .flex()
            .items_start()
            .gap(px(10.0))
            .child(AgentLogo::new(kind, 30.0, colors))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .line_height(px(19.0))
                            .text_size(px(14.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.primary)
                            .child(session.title.clone()),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(px(Typo::META.size))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.tertiary)
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(status_color)
                                    .child(div().size(px(6.0)).rounded_full().bg(status_color))
                                    .child(status_label),
                            )
                            .child("·")
                            .child(div().flex_none().child(kind.label()))
                            .child("·")
                            .child(div().min_w(px(0.0)).truncate().child(project_name.clone())),
                    ),
            );

        let mut content = div()
            .id("inspector-info-scroll")
            .size_full()
            .min_h(px(0.0))
            .px(px(details_ui::CONTENT_INSET))
            .pt(px(6.0))
            .pb(px(24.0))
            .flex()
            .flex_col()
            .gap(px(details_ui::SECTION_GAP))
            .overflow_y_scroll()
            .child(hero);

        if let Some(detail) = &session.needs_input {
            let destructive = detail.risk_hint == diri_proto::RiskHint::Destructive;
            let risk_color = if destructive {
                Ink::DANGER
            } else {
                Ink::ATTENTION
            };
            content = content.child(
                div()
                    .flex_none()
                    .px(px(11.0))
                    .py(px(9.0))
                    .flex()
                    .items_start()
                    .gap(px(9.0))
                    .rounded(px(Radius::CARD))
                    .bg(risk_color.alpha(0.09))
                    .border_1()
                    .border_color(risk_color.alpha(0.20))
                    .child(div().pt(px(1.0)).child(details_ui::icon(
                        if destructive {
                            IconName::Warning
                        } else {
                            IconName::Bell
                        },
                        14.0,
                        risk_color,
                    )))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.primary)
                                    .child(t("panel.info.needs_input")),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .font_weight(FontWeight::NORMAL)
                                    .line_height(px(15.0))
                                    .text_color(colors.secondary)
                                    .child(detail.summary.clone()),
                            ),
                    ),
            );
        }

        content = content.child(details_ui::section(
            t("panel.info.changes"),
            None,
            self.render_git_summary(colors, cx),
            colors,
        ));

        if let Some(transcript) = self.render_transcript(session, colors, cx) {
            content = content.child(transcript);
        }

        if let Some(pull_requests) = session.pull_requests.as_deref()
            && !pull_requests.is_empty()
        {
            let mut cards = div().flex().flex_col().gap(px(10.0));
            for pull_request in pull_requests.iter().take(2) {
                cards = cards.child(self.pull_request_card(pull_request, &session.id, colors, cx));
            }
            content = content.child(details_ui::section(
                if pull_requests.len() == 1 {
                    t("panel.pull_request")
                } else {
                    t("panel.pull_requests")
                },
                (pull_requests.len() > 1)
                    .then(|| details_ui::count_label(pull_requests.len(), colors)),
                cards,
                colors,
            ));
        }

        if artifact_total > 0 {
            let kinds = artifact_kind_summary(session);
            content = content.child(details_ui::section(
                t("panel.tab.artifacts"),
                None,
                details_ui::card(colors).child(
                    details_ui::list_row(
                        "inspector-artifacts-summary",
                        details_ui::icon_tile(IconName::Stack, colors.secondary, colors),
                        tf(
                            if artifact_total == 1 {
                                "panel.artifacts.count_one"
                            } else {
                                "panel.artifacts.count_other"
                            },
                            &[("count", &artifact_total)],
                        ),
                        (!kinds.is_empty()).then_some(kinds),
                        Some(details_ui::icon(
                            IconName::ChevronRight,
                            12.0,
                            colors.tertiary,
                        )),
                        colors,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.select_tab(InspectorTab::Artifacts, cx);
                        cx.stop_propagation();
                    })),
                ),
                colors,
            ));
        }

        let mut details = details_ui::DescriptionList::new()
            .text(t("panel.info.project"), project_name, false, colors)
            .text(t("panel.info.directory"), session.cwd.clone(), true, colors);
        if let Some(branch) = &session.git_branch {
            details = details.text(t("panel.info.branch"), branch.clone(), true, colors);
        }
        if let Some(host) = host_name {
            details = details.text(t("panel.info.host"), host, false, colors);
        }
        if let Some(bytes) = session.memory_bytes {
            details = details.text(t("panel.info.memory"), format_bytes(bytes), false, colors);
        }
        details = details.text(
            t("panel.info.updated"),
            details_ui::relative_time(session.updated_at.0),
            false,
            colors,
        );
        content
            .child(details_ui::section(
                t("panel.surface.details"),
                None,
                details.render(colors),
                colors,
            ))
            .child(self.render_status_evidence(session, colors, cx))
            .into_any_element()
    }

    fn render_transcript(
        &mut self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let TranscriptLoadState::Ready(document) = &self.transcript_state else {
            return None;
        };
        if document.turns.is_empty() {
            return None;
        }
        let turns = Arc::clone(document);
        let first = turns.turns.len().saturating_sub(8);
        let shown = turns.turns.len() - first;
        let selected_key = self.selected_turn_key().map(str::to_owned);
        let inspector = cx.entity();
        let kind = ui_agent_kind(session.effective_kind());
        let mut list = div().flex().flex_col().gap(px(2.0));
        for (index, turn) in turns.turns.iter().enumerate().skip(first) {
            let key = format!("transcript:{}:{}", session.id.0, turn.line);
            let selected = selected_key.as_deref() == Some(key.as_str());
            let source = QuoteSource::Transcript {
                session_id: session.id.clone(),
                turn: format!("{} turn near line {}", turn.role, turn.line),
            };
            let selection_content = turn.text.clone();
            let selection_inspector = inspector.clone();
            let document = self.markdown_document(&turn.text);
            let from_person = turn.role == "You";
            list = list.child(
                div()
                    .id(("transcript-turn", index))
                    .debug_selector(move || format!("INSPECTOR_TRANSCRIPT_TURN_{index}"))
                    .mx(px(-6.0))
                    .px(px(6.0))
                    .py(px(6.0))
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .rounded(px(Radius::BADGE))
                    .border_1()
                    .border_color(if selected {
                        details_ui::selection_stroke()
                    } else {
                        colors.primary.alpha(0.0)
                    })
                    .when(selected, |turn| turn.bg(details_ui::selection_fill()))
                    .cursor_pointer()
                    .hover(|turn| turn.bg(details_ui::selection_hover()))
                    .child(div().pt(px(1.0)).flex_none().child(if from_person {
                        details_ui::avatar("You", 18.0)
                    } else {
                        AgentLogo::new(kind, 18.0, colors).into_any_element()
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .h(px(18.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_size(px(Typo::META.size))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(colors.primary)
                                            .child(if from_person {
                                                t("panel.transcript.you")
                                            } else {
                                                turn.role
                                            }),
                                    )
                                    .child(
                                        div()
                                            .ml_auto()
                                            .font_weight(FontWeight::NORMAL)
                                            .text_color(colors.tertiary)
                                            .child(tf(
                                                "panel.transcript.line",
                                                &[("line", &turn.line)],
                                            )),
                                    ),
                            )
                            .child(render_markdown(&document, colors)),
                    )
                    .on_click(move |_, window, cx| {
                        selection_inspector.update(cx, |inspector, cx| {
                            inspector.select_turn(
                                key.clone(),
                                source.clone(),
                                selection_content.clone(),
                                window,
                                cx,
                            );
                        });
                        cx.stop_propagation();
                    }),
            );
        }
        Some(
            details_ui::section(
                t("panel.transcript.title"),
                Some(details_ui::count_label(shown, colors)),
                list,
                colors,
            )
            .into_any_element(),
        )
    }

    fn render_status_evidence(
        &self,
        session: &SessionRecord,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let evidence = session
            .status_evidence
            .as_ref()
            .filter(|evidence| evidence.status == session.status);
        let open = self.status_evidence_open;
        let mut disclosure = div().flex_none().flex().flex_col().child(
            div()
                .id("toggle-status-evidence")
                .debug_selector(|| "STATUS_EVIDENCE_TOGGLE".to_owned())
                .mx(px(-6.0))
                .h(px(28.0))
                .px(px(6.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(Radius::BADGE))
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.045)))
                .child(details_ui::icon(
                    if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                    12.0,
                    colors.tertiary,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(Typo::SECTION_HEADER.size))
                        .font_weight(Typo::SECTION_HEADER.weight)
                        .text_color(colors.secondary)
                        .child(t("panel.evidence.title")),
                )
                .child(
                    div()
                        .min_w(px(0.0))
                        .ml_auto()
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .font_weight(FontWeight::NORMAL)
                        .text_color(colors.tertiary)
                        .child(evidence.map_or(t("panel.evidence.none"), |evidence| {
                            crate::status_debug::source_name(evidence.source)
                        })),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.status_evidence_open = !this.status_evidence_open;
                    cx.notify();
                    cx.stop_propagation();
                })),
        );

        if !open {
            return disclosure.into_any_element();
        }

        let explanation = evidence.map_or(t("panel.evidence.predates"), |evidence| {
            status_evidence_explanation(evidence.source)
        });
        let mut details = div()
            .pt(px(4.0))
            .pl(px(18.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .font_weight(FontWeight::NORMAL)
                    .line_height(px(16.0))
                    .text_color(colors.secondary)
                    .child(explanation),
            );
        if let Some(evidence) = evidence {
            let mut facts = details_ui::DescriptionList::new().text(
                t("panel.evidence.signal"),
                details_ui::relative_time(evidence.signal_at.0),
                false,
                colors,
            );
            if let Some(manifest) =
                crate::status_debug::safe_identifier(evidence.manifest_id.as_deref())
            {
                let version =
                    crate::status_debug::safe_identifier(evidence.manifest_version.as_deref());
                facts = facts.text(
                    t("panel.evidence.manifest"),
                    version.map_or(manifest.clone(), |version| format!("{manifest}@{version}")),
                    true,
                    colors,
                );
            }
            if let Some(rule) =
                crate::status_debug::safe_identifier(evidence.matched_rule_id.as_deref())
            {
                facts = facts.text(t("panel.evidence.rule"), rule, true, colors);
            }
            if evidence.startup_grace_active {
                facts = facts.text(
                    t("panel.evidence.startup"),
                    t("panel.evidence.startup_detail"),
                    false,
                    colors,
                );
            }
            if evidence.anti_flicker_active {
                facts = facts.text(
                    t("panel.evidence.flicker"),
                    t("panel.evidence.flicker_detail"),
                    false,
                    colors,
                );
            }
            if let Some(reason) = evidence.fallback_reason {
                facts = facts.text(
                    t("panel.evidence.fallback"),
                    crate::status_debug::fallback_name(reason),
                    false,
                    colors,
                );
            }
            details = details.child(facts.render(colors));
        }

        let report = crate::status_debug::StatusDebugInfo::from_session(session)
            .as_str()
            .to_owned();
        details = details.child(
            details_ui::ghost_button(
                "copy-status-debug-info",
                IconName::File,
                Some(t("panel.evidence.copy")),
                colors.secondary,
                colors.primary.alpha(0.09),
            )
            .self_start()
            .bg(colors.primary.alpha(0.05))
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(report.clone()));
                cx.stop_propagation();
            }),
        );
        disclosure = disclosure.child(details);
        disclosure.into_any_element()
    }

    fn render_git_summary(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let default_base = || match self.comparison {
            SessionDiffBase::DefaultBranch => t("panel.git.the_default_branch"),
            SessionDiffBase::Head => "HEAD",
        };
        let comparison = |base: Option<&str>| {
            tf(
                "panel.git.against",
                &[("base", &base.unwrap_or(default_base()))],
            )
        };
        let (glyph, title, detail, accent, can_open): (
            Option<IconName>,
            String,
            AnyElement,
            gpui::Rgba,
            bool,
        ) = match &self.state {
            LoadState::Ready(snapshot) if snapshot.files > 0 => (
                Some(IconName::Branch),
                tf(
                    if snapshot.files == 1 {
                        "panel.git.files_changed_one"
                    } else {
                        "panel.git.files_changed_other"
                    },
                    &[("count", &snapshot.files)],
                ),
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(details_ui::diff_stat(
                        snapshot.additions as u64,
                        snapshot.deletions as u64,
                        colors,
                    ))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Typo::META.size))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.tertiary)
                            .child(comparison(snapshot.base_ref.as_deref())),
                    )
                    .into_any_element(),
                colors.secondary,
                true,
            ),
            LoadState::Ready(snapshot) => (
                Some(IconName::CheckCircle),
                t("panel.git.no_changes").to_owned(),
                meta_text(
                    tf(
                        "panel.git.matches",
                        &[(
                            "base",
                            &snapshot.base_ref.as_deref().unwrap_or(default_base()),
                        )],
                    ),
                    colors,
                ),
                Ink::FRESH,
                true,
            ),
            LoadState::Loading => (
                None,
                t("panel.git.reading").to_owned(),
                meta_text(t("panel.git.updating").to_owned(), colors),
                colors.secondary,
                false,
            ),
            LoadState::Error(error) if git_is_not_a_repository(error) => (
                Some(IconName::Folder),
                t("panel.git.not_repo").to_owned(),
                meta_text(t("panel.git.not_repo_detail").to_owned(), colors),
                colors.tertiary,
                false,
            ),
            LoadState::Error(error) if git_is_not_installed(error) => (
                Some(IconName::Terminal),
                t("panel.git.unavailable").to_owned(),
                meta_text(t("panel.git.not_installed").to_owned(), colors),
                colors.tertiary,
                false,
            ),
            LoadState::Error(error) => (
                Some(IconName::Warning),
                t("panel.git.status_unavailable").to_owned(),
                meta_text(error.clone(), colors),
                Ink::ATTENTION,
                false,
            ),
            LoadState::NoSession => (
                Some(IconName::Info),
                t("panel.git.no_session").to_owned(),
                meta_text(t("panel.git.no_session_detail").to_owned(), colors),
                colors.tertiary,
                false,
            ),
        };
        let tile = match glyph {
            Some(glyph) => details_ui::icon_tile(glyph, accent, colors),
            None => div()
                .size(px(26.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(LoadingIndicator::new("inspector-git-loading", 14.0, accent))
                .into_any_element(),
        };
        details_ui::card(colors)
            .child(
                div()
                    .id("inspector-git-summary")
                    .min_h(px(48.0))
                    .px(px(9.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .when(can_open, |row| {
                        row.cursor_pointer()
                            .hover(move |row| row.bg(colors.primary.alpha(0.045)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.select_tab(InspectorTab::Changes, cx);
                                cx.stop_propagation();
                            }))
                    })
                    .child(tile)
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
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary)
                                    .child(title),
                            )
                            .child(detail),
                    )
                    .when(can_open, |row| {
                        row.child(details_ui::icon(
                            IconName::ChevronRight,
                            12.0,
                            colors.tertiary,
                        ))
                    }),
            )
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
                    t("panel.select_session"),
                    t("panel.artifacts.empty_session"),
                )
                .into_any_element();
        };
        if artifact_count(session) == 0 {
            return self
                .render_message(
                    colors,
                    "square.stack.3d.up",
                    t("panel.artifacts.empty"),
                    t("panel.artifacts.empty_detail"),
                )
                .into_any_element();
        }

        let mut content = div()
            .id("inspector-artifacts-scroll")
            .size_full()
            .min_h(px(0.0))
            .px(px(details_ui::CONTENT_INSET))
            .pt(px(6.0))
            .pb(px(24.0))
            .flex()
            .flex_col()
            .gap(px(details_ui::SECTION_GAP))
            .overflow_y_scroll();

        if let Some(pull_requests) = session.pull_requests.as_deref()
            && !pull_requests.is_empty()
        {
            let mut cards = div().flex().flex_col().gap(px(10.0));
            for pull_request in pull_requests {
                cards = cards.child(self.pull_request_card(pull_request, &session.id, colors, cx));
            }
            content = content.child(details_ui::section(
                if pull_requests.len() == 1 {
                    t("panel.pull_request")
                } else {
                    t("panel.pull_requests")
                },
                (pull_requests.len() > 1)
                    .then(|| details_ui::count_label(pull_requests.len(), colors)),
                cards,
                colors,
            ));
        }

        let links: Vec<&SessionArtifact> = session
            .artifacts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|artifact| {
                !(artifact.kind == ArtifactKind::PullRequest
                    && session.pull_requests.as_deref().is_some_and(|statuses| {
                        statuses.iter().any(|status| status.url == artifact.url)
                    }))
            })
            .collect();
        if !links.is_empty() {
            let mut group = details_ui::card(colors).flex().flex_col();
            for (index, artifact) in links.iter().enumerate() {
                if index > 0 {
                    group = group.child(details_ui::hairline(colors));
                }
                group = group.child(render_artifact_row(artifact, colors));
            }
            content = content.child(details_ui::section(
                t("panel.artifacts.links"),
                Some(details_ui::count_label(links.len(), colors)),
                group,
                colors,
            ));
        }

        if let Some(ports) = session.listening_ports.as_deref()
            && !ports.is_empty()
        {
            let mut group = details_ui::card(colors).flex().flex_col();
            for (index, port) in ports.iter().enumerate() {
                if index > 0 {
                    group = group.child(details_ui::hairline(colors));
                }
                let url = format!("http://localhost:{}", port.port);
                group = group.child(
                    details_ui::list_row(
                        SharedString::from(format!("inspector-port-{}", port.port)),
                        details_ui::icon_tile(IconName::Network, colors.secondary, colors),
                        format!("localhost:{}", port.port),
                        Some(port.process_name.clone()),
                        Some(details_ui::icon(
                            IconName::ExternalLink,
                            12.0,
                            colors.tertiary,
                        )),
                        colors,
                    )
                    .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            content = content.child(details_ui::section(
                t("panel.artifacts.local_servers"),
                Some(details_ui::count_label(ports.len(), colors)),
                group,
                colors,
            ));
        }
        content.into_any_element()
    }

    /// One pull request as an Ely-style card, wired to this inspector's
    /// selection, Ask, and fold state.
    fn pull_request_card(
        &mut self,
        pull_request: &PullRequestStatus,
        session_id: &SessionId,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = pull_request
            .body
            .as_deref()
            .filter(|body| !body.trim().is_empty())
            .map(|body| self.markdown_document(body));
        let inspector = cx.entity();
        let ask_inspector = inspector.clone();
        let select_inspector = inspector.clone();
        let checks_inspector = inspector.clone();
        let discussion_inspector = inspector;
        let checks_url = pull_request.url.clone();
        let discussion_url = pull_request.url.clone();
        PullRequestCard {
            pull_request,
            session_id: session_id.clone(),
            body,
            selected_key: self.selected_turn_key().map(str::to_owned),
            checks_open: self.pr_cards.checks_open(pull_request),
            discussion_expanded: self.pr_cards.discussion_expanded(&pull_request.url),
            actions: PrCardActions {
                ask: Rc::new(move |evidence, window, cx| {
                    ask_inspector.update(cx, |inspector, cx| {
                        inspector.open_ask(evidence, window, cx);
                    });
                }),
                select: Rc::new(move |key, source, content, window, cx| {
                    select_inspector.update(cx, |inspector, cx| {
                        inspector.select_turn(key, source, content, window, cx);
                    });
                }),
                toggle_checks: Rc::new(move |cx| {
                    checks_inspector.update(cx, |inspector, cx| {
                        inspector.pr_cards.toggle_checks(&checks_url);
                        cx.notify();
                    });
                }),
                toggle_discussion: Rc::new(move |cx| {
                    discussion_inspector.update(cx, |inspector, cx| {
                        inspector.pr_cards.toggle_discussion(&discussion_url);
                        cx.notify();
                    });
                }),
            },
        }
        .render(colors)
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
        // The omitted names are extra rows of the same virtualized list, so
        // thousands of them cost only the handful that are on screen.
        let omitted_open =
            self.omitted_untracked_open && snapshot.omitted_untracked_notice_row().is_some();
        let built = self.review_ui.rows_for(&snapshot, omitted_open);
        let layout = self.review_ui.effective_layout();
        let text_columns = if omitted_open {
            snapshot
                .omitted_untracked_paths
                .iter()
                .map(|path| path.as_os_str().len() + OMITTED_PATH_INDENT_COLUMNS)
                .fold(snapshot.max_text_columns, usize::max)
        } else {
            snapshot.max_text_columns
        };
        let (palette, font) = self.review_look(colors);
        let mutable = !self.remote_context() && self.review_ui.mode == ReviewMode::Changes;
        let entity = cx.entity();
        let handlers = Rc::new(DiffHandlers {
            select_row: Box::new({
                let entity = entity.clone();
                move |row, extend, window, cx| {
                    entity.update(cx, |this, cx| this.select_diff_row(row, extend, window, cx));
                }
            }),
            file: Box::new({
                let (entity, snapshot) = (entity.clone(), Arc::clone(&snapshot));
                move |file, action, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.diff_file_action(&snapshot, file, action, window, cx);
                    });
                }
            }),
            hunk: Box::new({
                let (entity, snapshot) = (entity.clone(), Arc::clone(&snapshot));
                move |file, hunk, action, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.diff_hunk_action(&snapshot, (file, hunk), action, window, cx);
                    });
                }
            }),
            toggle_omitted: Box::new({
                let entity = entity.clone();
                move |_, cx| entity.update(cx, |this, cx| this.toggle_omitted_untracked(cx))
            }),
            omitted: Box::new({
                let (entity, snapshot) = (entity, Arc::clone(&snapshot));
                move |ordinal, action, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.omitted_path_action(&snapshot, ordinal, action, cx);
                    });
                }
            }),
        });
        let props = DiffViewProps {
            selection: self.diff_selection.row_range(&snapshot),
            snapshot,
            rows: built.rows.clone(),
            layout,
            palette,
            font,
            armed_hunk: self.armed_hunk,
            omitted_open,
            collapsed: self.review_ui.collapsed(),
            can_ask: true,
            mutable,
            digits: built.digits,
            content_width: diff_view::inline_content_width(built.digits, text_columns),
            handlers,
        };
        let list = uniform_list("inspector-diff", built.rows.len(), move |range, _, _| {
            diff_view::render_rows(&props, range)
        });
        // Inline rows scroll sideways to the end of the longest line; split
        // columns each clip to their half instead.
        let list = if layout == DiffLayout::Inline {
            list.with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        } else {
            list
        }
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
            SessionDiffBase::DefaultBranch => t("panel.git.default_branch_lower").to_owned(),
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
                t("panel.git.default_branch"),
                t("panel.git.default_branch_detail"),
                "INSPECTOR_COMPARE_DEFAULT",
            ),
            SessionDiffBase::Head => ("HEAD", t("panel.git.head_detail"), "INSPECTOR_COMPARE_HEAD"),
        };
        let selected = self.comparison == comparison;
        div()
            .id(SharedString::from(format!("compare-option-{title}")))
            .debug_selector(move || selector.to_owned())
            .min_h(px(42.0))
            .mx(px(4.0))
            .px(px(8.0))
            .py(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(Radius::inner(crate::floating::MENU_RADIUS, 4.0)))
            .cursor_pointer()
            .glass_menu_row(colors, selected)
            .child(
                div()
                    .w(px(12.0))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .when(selected, |slot| {
                        slot.child(sf_symbol("checkmark", 10.0, colors.primary))
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
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
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

    /// Branch / Working / Staged, as one segmented control.
    fn render_layer_option(&self, palette: &DiffPalette, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        diff_view::segmented(
            "review-layer",
            &[
                (
                    DiffLayer::Branch,
                    t("panel.info.branch"),
                    "INSPECTOR_LAYER_BRANCH",
                ),
                (
                    DiffLayer::Working,
                    t("panel.review.layer_working"),
                    "INSPECTOR_LAYER_WORKING",
                ),
                (
                    DiffLayer::Staged,
                    t("panel.review.layer_staged"),
                    "INSPECTOR_LAYER_STAGED",
                ),
            ],
            self.diff_layer,
            palette,
            move |layer, _, cx| entity.update(cx, |this, cx| this.select_diff_layer(layer, cx)),
        )
    }

    /// The changed-file rows of the review navigator, without host chrome.
    fn file_navigator_list(
        &self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (palette, _) = self.review_look(colors);
        let mut files = div()
            .id("review-file-navigator-list")
            .max_h(px(390.0))
            .py(px(4.0))
            .overflow_y_scroll();
        for (index, file) in snapshot.file_diffs.iter().enumerate() {
            let row = file.row_range.start;
            let path = file.path.to_string_lossy().into_owned();
            let (directory, name) = match path.rfind('/') {
                Some(slash) => (path[..=slash].to_owned(), path[slash + 1..].to_owned()),
                None => (String::new(), path.clone()),
            };
            files = files.child(
                div()
                    .id(("review-file-navigator-row", index))
                    .debug_selector(move || format!("INSPECTOR_REVIEW_FILE_{index}"))
                    .h(px(30.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .cursor_pointer()
                    .mx(px(4.0))
                    .rounded(px(Radius::inner(crate::floating::MENU_RADIUS, 4.0)))
                    .glass_menu_row(colors, false)
                    .child(diff_view::status_badge(file.status, &palette))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(11.5))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary)
                                    .child(name),
                            )
                            .when(!directory.is_empty(), |label| {
                                label.child(
                                    div()
                                        .min_w(px(0.0))
                                        .ml(px(6.0))
                                        .truncate()
                                        .text_size(px(10.5))
                                        .text_color(colors.tertiary)
                                        .child(directory),
                                )
                            }),
                    )
                    .child(diff_view::diff_stat(
                        file.additions,
                        file.deletions,
                        &palette,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_diff_row(row, cx);
                        cx.stop_propagation();
                    })),
            );
        }

        files
    }

    fn render_file_navigator(
        &self,
        snapshot: Arc<DiffSnapshot>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let files = self.file_navigator_list(snapshot, colors, cx);
        let scrim = div().absolute().inset_0().occlude().on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.files_open = false;
                cx.notify();
                cx.stop_propagation();
            }),
        );
        let panel = if crate::floating::uses_panels(false, colors, cx) {
            crate::floating::host_here(
                INSPECTOR_FILES_MENU,
                crate::floating::surface(colors, crate::floating::MENU_RADIUS, 330.0, files)
                    .into_any_element(),
                Some(330.0),
                gpui::Anchor::TopRight,
                8.0,
                cx,
            )
            .absolute()
            .top(px(40.0))
            .right(px(9.0))
            .w(px(0.0))
            .h(px(0.0))
            .into_any_element()
        } else {
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
                .child(FloatingSurface::new(colors, files).radius(crate::floating::MENU_RADIUS))
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .child(scrim)
            .child(panel)
            .into_any_element()
    }

    /// The comparison base rows, without host chrome.
    fn comparison_menu_items(&self, colors: SemanticColors, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .py(px(4.0))
            .overflow_hidden()
            .child(self.render_comparison_option(SessionDiffBase::DefaultBranch, colors, cx))
            .child(self.render_comparison_option(SessionDiffBase::Head, colors, cx))
    }

    fn remote_context(&self) -> bool {
        self.context.as_ref().is_some_and(|context| context.remote)
    }

    /// The palette the right panel paints with; its fill is
    /// `crate::right_panel::panel_background(colors)`.
    fn panel_colors(&self) -> SemanticColors {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        crate::right_panel::panel_colors_in(&store)
    }

    /// The file navigator's pixels for its floating panel.
    fn files_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.files_open || self.remote_context() {
            return None;
        }
        let LoadState::Ready(snapshot) = &self.state else {
            return None;
        };
        let snapshot = Arc::clone(snapshot);
        let colors = self.panel_colors();
        let files = self.file_navigator_list(snapshot, colors, cx);
        Some(
            crate::floating::surface(colors, crate::floating::MENU_RADIUS, 330.0, files)
                .into_any_element(),
        )
    }

    /// The comparison menu's pixels for its floating panel.
    fn comparison_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !(self.comparison_menu_open && self.remote_context()) {
            return None;
        }
        let colors = self.panel_colors();
        let items = self.comparison_menu_items(colors, cx);
        Some(
            crate::floating::surface(colors, crate::floating::MENU_RADIUS, 230.0, items)
                .into_any_element(),
        )
    }

    /// One of the review's small text buttons. `strong` lifts the primary
    /// action of a lane (Stage all with nothing staged yet, Commit).
    #[allow(clippy::too_many_arguments)]
    fn review_button(
        id: &'static str,
        label: &'static str,
        icon: Option<&'static str>,
        tone: Rgba,
        strong: bool,
        enabled: bool,
        palette: &DiffPalette,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let hover = tone.alpha(if strong { 0.24 } else { 0.10 });
        div()
            .id(id)
            .flex_none()
            .h(px(22.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .rounded(px(Radius::CHIP))
            .when(strong, |button| button.bg(tone.alpha(0.15)))
            .text_size(px(10.5))
            .font_weight(if strong {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(if enabled { tone } else { palette.muted })
            .when_some(icon, |button, icon| {
                button.child(sf_symbol(
                    icon,
                    9.5,
                    if enabled { tone } else { palette.muted },
                ))
            })
            .child(label)
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |button| button.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        on_click(this, window, cx);
                        cx.stop_propagation();
                    }))
            })
    }

    fn render_review_controls(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _ = window;
        let (palette, font) = self.review_look(colors);
        let container = || {
            div()
                .flex_none()
                .px(px(10.0))
                .pt(px(6.0))
                .pb(px(7.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .border_b_1()
                .border_color(palette.rule)
        };
        let message_line = |symbol: &'static str, label: String, color: Rgba| {
            div()
                .min_h(px(22.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(10.5))
                .text_color(color)
                .child(sf_symbol(symbol, 10.5, color))
                .child(div().min_w(px(0.0)).flex_1().child(label))
        };
        let layers = self.render_layer_option(&palette, cx);
        let ReviewLoadState::Ready(status) = &self.review_state else {
            let line = match &self.review_state {
                ReviewLoadState::NoSession => message_line(
                    "minus.circle",
                    t("panel.review.select_agent").to_owned(),
                    palette.muted,
                ),
                ReviewLoadState::Remote => {
                    return container()
                        .child(message_line(
                            "network",
                            t("panel.review.remote_view_only").to_owned(),
                            palette.muted,
                        ))
                        .into_any_element();
                }
                ReviewLoadState::Loading => message_line(
                    "ellipsis",
                    t("panel.review.reading").to_owned(),
                    palette.muted,
                ),
                ReviewLoadState::Error(error) => {
                    message_line("exclamationmark.triangle", error.clone(), Ink::ATTENTION)
                }
                ReviewLoadState::Ready(_) => unreachable!(),
            };
            return container()
                .child(line)
                .child(div().flex().items_center().child(layers))
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
            .unwrap_or_else(|| t("panel.review.detached_head").to_owned());
        let busy = self.review_action_busy;
        let commit_open = self.commit_open;
        let discard_armed = self.discard_armed;

        let mut actions = div().flex().items_center().gap(px(2.0));
        if self.diff_layer == DiffLayer::Working && !stage_paths.is_empty() {
            let paths = stage_paths;
            actions = actions.child(Self::review_button(
                "review-stage-all",
                t("panel.review.stage_all"),
                Some("plus"),
                if staged_count == 0 {
                    palette.accent
                } else {
                    palette.secondary
                },
                staged_count == 0,
                !busy,
                &palette,
                move |this, _, cx| this.run_review_action(ReviewAction::Stage(paths.clone()), cx),
                cx,
            ));
        }
        if self.diff_layer == DiffLayer::Staged && !staged_paths.is_empty() {
            let paths = staged_paths;
            actions = actions.child(Self::review_button(
                "review-unstage-all",
                t("panel.review.unstage"),
                None,
                palette.secondary,
                false,
                !busy,
                &palette,
                move |this, _, cx| {
                    this.run_review_action(ReviewAction::Unstage(paths.clone()), cx);
                },
                cx,
            ));
            actions = actions.child(Self::review_button(
                "review-open-commit",
                if commit_open {
                    t("panel.review.cancel")
                } else {
                    t("panel.review.commit")
                },
                (!commit_open).then_some("checkmark"),
                if commit_open {
                    palette.secondary
                } else {
                    palette.accent
                },
                !commit_open,
                !busy,
                &palette,
                |this, window, cx| {
                    this.commit_open = !this.commit_open;
                    this.discard_armed = false;
                    this.ask_draft = None;
                    this.ask_feedback = None;
                    this.ask_query.clear();
                    if this.commit_open {
                        window.focus(&this.focus, cx);
                    }
                    cx.notify();
                },
                cx,
            ));
        }
        if self.diff_layer == DiffLayer::Working && !discard_paths.is_empty() {
            let paths = discard_paths;
            actions = actions.child(Self::review_button(
                "review-discard-all",
                if discard_armed {
                    t("panel.review.discard_confirm")
                } else {
                    t("panel.review.discard")
                },
                Some("trash"),
                if discard_armed {
                    palette.removed
                } else {
                    palette.secondary
                },
                discard_armed,
                !busy,
                &palette,
                move |this, _, cx| {
                    if this.discard_armed {
                        this.run_review_action(ReviewAction::Discard(paths.clone()), cx);
                    } else {
                        this.discard_armed = true;
                        this.commit_open = false;
                        cx.notify();
                    }
                },
                cx,
            ));
        }
        // The Branch lane is an overview: its right side carries the size of
        // the change instead of actions.
        let overview_stat = (self.diff_layer == DiffLayer::Branch)
            .then(|| match &self.state {
                LoadState::Ready(snapshot) if snapshot.files > 0 => Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(10.0))
                        .text_color(palette.muted)
                        .child(tf(
                            if snapshot.files == 1 {
                                "panel.review.files_one"
                            } else {
                                "panel.review.files_other"
                            },
                            &[("count", &snapshot.files)],
                        ))
                        .child(diff_view::diff_stat(
                            snapshot.additions,
                            snapshot.deletions,
                            &palette,
                        )),
                ),
                _ => None,
            })
            .flatten();

        let branch_detail = match (status.branch.ahead, status.branch.behind) {
            (0, 0) => None,
            (ahead, 0) => Some(format!("↑{ahead}")),
            (0, behind) => Some(format!("↓{behind}")),
            (ahead, behind) => Some(format!("↑{ahead} ↓{behind}")),
        };
        let mut counts = tf(
            "panel.review.counts",
            &[("staged", &staged_count), ("working", &working_count)],
        );
        if conflicted_count > 0 {
            counts.push_str(&tf(
                "panel.review.conflicted",
                &[("count", &conflicted_count)],
            ));
        }
        let mut panel = container()
            .child(
                div()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(sf_symbol("arrow.branch", 10.5, palette.muted))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .font_family(font.clone())
                            .text_size(px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(palette.text)
                            .child(branch),
                    )
                    .when_some(branch_detail, |row, detail| {
                        row.child(
                            div()
                                .flex_none()
                                .font_family(font.clone())
                                .text_size(px(10.0))
                                .text_color(palette.muted)
                                .child(detail),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(if conflicted_count > 0 {
                                palette.removed
                            } else {
                                palette.muted
                            })
                            .child(counts),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .child(layers)
                    .child(div().flex_1())
                    .when(self.diff_layer != DiffLayer::Branch, |row| {
                        row.child(actions)
                    })
                    .when_some(overview_stat, |row, stat| row.child(stat)),
            );

        if self.commit_open {
            let empty = self.commit_query.is_empty();
            panel = panel.child(
                div()
                    .h(px(30.0))
                    .pl(px(9.0))
                    .pr(px(3.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(Radius::BADGE))
                    .bg(palette.control)
                    .border_1()
                    .border_color(palette.rule)
                    .child(
                        div()
                            .id("review-commit-message")
                            .min_w(px(0.0))
                            .h_full()
                            .flex_1()
                            .flex()
                            .items_center()
                            .cursor_text()
                            .text_size(px(11.5))
                            .text_color(palette.text)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .child(if empty {
                                div()
                                    .text_color(palette.muted)
                                    .child(t("panel.review.commit_placeholder"))
                                    .into_any_element()
                            } else {
                                crate::navigation::query_label(&self.commit_query)
                            }),
                    )
                    .child(Self::review_button(
                        "review-submit-commit",
                        t("panel.review.commit"),
                        None,
                        palette.accent,
                        !empty,
                        !empty && !busy,
                        &palette,
                        |this, _, cx| this.submit_commit(cx),
                        cx,
                    )),
            );
        }
        if let Some((success, message)) = &self.review_feedback {
            let accent = if *success {
                palette.added
            } else {
                palette.removed
            };
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(10.5))
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
        panel.into_any_element()
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_selected == Some(WorkspaceSurface::Browser)
            && self.browser_address_focused
        {
            if self.apply_browser_edit(event, cx) {
                cx.stop_propagation();
            }
            return;
        }
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
        if !self.commit_open
            && (self.workspace_selected == Some(WorkspaceSurface::Review)
                || (self.workspace_selected == Some(WorkspaceSurface::Details)
                    && self.selected_tab == InspectorTab::Changes))
        {
            let shift = event.keystroke.modifiers.shift;
            let snapshot = self.displayed_diff().cloned();
            let moved = match (event.keystroke.key.as_str(), snapshot) {
                ("up", Some(snapshot)) => self.diff_selection.move_by(&snapshot, -1, shift),
                ("down", Some(snapshot)) => self.diff_selection.move_by(&snapshot, 1, shift),
                ("escape", _) if !self.diff_selection.is_empty() => {
                    self.diff_selection.clear();
                    true
                }
                _ => false,
            };
            if moved {
                // Keep the line the keyboard moved to on screen.
                if let Some(position) = self
                    .diff_selection
                    .head()
                    .and_then(|row| self.diff_position(row))
                {
                    self.scroll
                        .scroll_to_item(position, ScrollStrategy::Nearest);
                }
                self.selected_turn = None;
                cx.stop_propagation();
                cx.notify();
                return;
            }
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
        let (palette, font) = self.review_look(colors);
        let mode = self.review_ui.mode;
        let remote = self.remote_context();
        let body = match mode {
            ReviewMode::Changes => self.render_changes_body(colors, window, cx),
            ReviewMode::Commits => self.render_commits_body(colors, &palette, &font, window, cx),
        };
        let comparison_open = remote && self.comparison_menu_open && mode == ReviewMode::Changes;
        let snapshot = match &self.state {
            LoadState::Ready(snapshot) => Some(Arc::clone(snapshot)),
            _ => None,
        };
        let menu = if mode != ReviewMode::Changes {
            None
        } else if !remote && self.files_open {
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
                    .child(if crate::floating::uses_panels(false, colors, cx) {
                        crate::floating::host_here(
                            INSPECTOR_COMPARISON_MENU,
                            crate::floating::surface(
                                colors,
                                crate::floating::MENU_RADIUS,
                                230.0,
                                self.comparison_menu_items(colors, cx),
                            )
                            .into_any_element(),
                            Some(230.0),
                            gpui::Anchor::TopRight,
                            8.0,
                            cx,
                        )
                        .absolute()
                        .top(px(38.0))
                        .right(px(10.0))
                        .w(px(0.0))
                        .h(px(0.0))
                        .into_any_element()
                    } else {
                        div()
                            .id("inspector-comparison-menu")
                            .absolute()
                            .top(px(38.0))
                            .right(px(10.0))
                            .w(px(230.0))
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.comparison_menu_open = false;
                                cx.notify();
                            }))
                            .child(
                                FloatingSurface::new(
                                    colors,
                                    self.comparison_menu_items(colors, cx),
                                )
                                .radius(crate::floating::MENU_RADIUS),
                            )
                            .into_any_element()
                    })
                    .into_any_element(),
            )
        } else {
            None
        };

        // Split needs room for two columns of code. The body's width is only
        // known after layout, so a probe records it and asks for one more
        // frame when it crosses the threshold (a dock resize, a window drag).
        let split_fits = self.review_ui.split_fits.clone();
        let inspector = cx.entity().downgrade();
        let probe = canvas(
            move |bounds, window, _| {
                let fits = f32::from(bounds.size.width) >= crate::git_ui::SPLIT_MIN_WIDTH;
                if split_fits.replace(fits) != fits {
                    let inspector = inspector.clone();
                    window.on_next_frame(move |_, cx| {
                        let _ = inspector.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(probe)
            .child(self.render_review_toolbar(colors, &palette, cx))
            .when(mode == ReviewMode::Changes, |panel| {
                panel.child(self.render_review_controls(colors, window, cx))
            })
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(body))
            .when_some(menu, |panel, menu| panel.child(menu))
            .into_any_element()
    }

    /// Changes | Commits on the left; the layout toggle and the file or
    /// comparison picker on the right.
    fn render_review_toolbar(
        &mut self,
        colors: SemanticColors,
        palette: &DiffPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = *palette;
        let mode = self.review_ui.mode;
        let remote = self.remote_context();
        let entity = cx.entity();
        let modes = diff_view::segmented(
            "review-mode",
            &[
                (
                    ReviewMode::Changes,
                    t("panel.info.changes"),
                    "INSPECTOR_REVIEW_MODE_CHANGES",
                ),
                (
                    ReviewMode::Commits,
                    t("panel.review.commits"),
                    "INSPECTOR_REVIEW_MODE_COMMITS",
                ),
            ],
            mode,
            &palette,
            move |mode, _, cx| entity.update(cx, |this, cx| this.set_review_mode(mode, cx)),
        );
        let shows_diff = match mode {
            ReviewMode::Changes => true,
            ReviewMode::Commits => self.review_ui.selected_commit.is_some(),
        };
        let layout_toggle = (shows_diff && self.review_ui.split_fits.get()).then(|| {
            let entity = cx.entity();
            diff_view::segmented(
                "review-layout",
                &[
                    (
                        DiffLayout::Inline,
                        t("panel.review.inline"),
                        "INSPECTOR_DIFF_INLINE",
                    ),
                    (
                        DiffLayout::Split,
                        t("panel.review.split"),
                        "INSPECTOR_DIFF_SPLIT",
                    ),
                ],
                self.review_ui.layout,
                &palette,
                move |layout, _, cx| entity.update(cx, |this, cx| this.set_diff_layout(layout, cx)),
            )
        });
        let picker = (mode == ReviewMode::Changes).then(|| {
            if remote {
                let label = self.comparison_label();
                let open = self.comparison_menu_open;
                div()
                    .id("inspector-comparison-button")
                    .debug_selector(|| "INSPECTOR_COMPARE_BUTTON".to_owned())
                    .flex_none()
                    .max_w(px(184.0))
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .bg(if open { palette.hover } else { palette.control })
                    .cursor_pointer()
                    .hover(move |button| button.bg(palette.hover))
                    .child(sf_symbol("arrow.branch", 10.0, palette.muted))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(palette.secondary)
                            .child(tf("panel.review.versus", &[("base", &label)])),
                    )
                    .child(sf_symbol("chevron.down", 8.5, palette.muted))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.comparison_menu_open = !this.comparison_menu_open;
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .into_any_element()
            } else {
                let file_count = match &self.state {
                    LoadState::Ready(snapshot) => snapshot.file_diffs.len(),
                    _ => 0,
                };
                div()
                    .id("review-files-button")
                    .debug_selector(|| "INSPECTOR_REVIEW_FILES".to_owned())
                    .flex_none()
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .bg(if self.files_open {
                        palette.hover
                    } else {
                        palette.control
                    })
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if file_count > 0 {
                        palette.secondary
                    } else {
                        palette.muted
                    })
                    .when(file_count > 0, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| button.bg(palette.hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.files_open = !this.files_open;
                                cx.notify();
                                cx.stop_propagation();
                            }))
                    })
                    .child(sf_symbol("list.bullet", 10.0, palette.muted))
                    .child(tf(
                        if file_count == 1 {
                            "panel.review.files_one"
                        } else {
                            "panel.review.files_other"
                        },
                        &[("count", &file_count)],
                    ))
                    .child(sf_symbol("chevron.down", 8.5, palette.muted))
                    .into_any_element()
            }
        });
        let _ = colors;
        div()
            .h(px(38.0))
            .flex_none()
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .border_b_1()
            .border_color(palette.rule)
            .child(modes)
            .child(div().flex_1())
            .children(layout_toggle)
            .children(picker)
            .into_any_element()
    }

    /// The selected lane's diff, or why there is none.
    fn render_changes_body(
        &mut self,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = self.comparison_label();
        let empty_detail = match self.diff_layer {
            DiffLayer::Branch => tf("panel.review.branch_matches", &[("base", &label)]),
            DiffLayer::Working => t("panel.review.working_matches").to_owned(),
            DiffLayer::Staged => t("panel.review.index_matches").to_owned(),
        };
        match self.state.clone() {
            LoadState::Ready(snapshot) if snapshot.rows.is_empty() => self
                .render_message(
                    colors,
                    "checkmark.circle",
                    t("panel.git.no_changes"),
                    empty_detail,
                )
                .into_any_element(),
            LoadState::Ready(snapshot) => self
                .render_diff(snapshot, colors, window, cx)
                .into_any_element(),
            LoadState::Loading => self
                .render_message(
                    colors,
                    "ellipsis",
                    t("panel.review.loading"),
                    t("panel.review.reading_tree"),
                )
                .into_any_element(),
            LoadState::NoSession => self
                .render_message(
                    colors,
                    "sidebar.left",
                    t("panel.select_session"),
                    t("panel.review.empty_session"),
                )
                .into_any_element(),
            LoadState::Error(error) => self
                .render_message(
                    colors,
                    "exclamationmark.triangle",
                    t("panel.review.load_failed"),
                    error,
                )
                .into_any_element(),
        }
    }

    /// The branch's commits with their graph; a picked commit's diff below.
    fn render_commits_body(
        &mut self,
        colors: SemanticColors,
        palette: &DiffPalette,
        font: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = *palette;
        if self.remote_context() {
            return self
                .render_message(
                    colors,
                    "network",
                    t("panel.history.local_only"),
                    t("panel.history.local_only_detail"),
                )
                .into_any_element();
        }
        if self.context.is_none() {
            return self
                .render_message(
                    colors,
                    "sidebar.left",
                    t("panel.select_session"),
                    t("panel.history.empty_session"),
                )
                .into_any_element();
        }
        let loaded = match &self.review_ui.history {
            HistoryLoad::Idle | HistoryLoad::Loading => {
                return self
                    .render_message(
                        colors,
                        "ellipsis",
                        t("panel.history.loading"),
                        t("panel.history.reading"),
                    )
                    .into_any_element();
            }
            HistoryLoad::Error(error) => {
                return self
                    .render_message(
                        colors,
                        "exclamationmark.triangle",
                        t("panel.history.load_failed"),
                        error.clone(),
                    )
                    .into_any_element();
            }
            HistoryLoad::Ready(loaded) => Arc::clone(loaded),
        };
        let history = &loaded.history;
        if history.commits.is_empty() {
            return self
                .render_message(
                    colors,
                    "checkmark.circle",
                    t("panel.history.empty"),
                    t("panel.history.empty_detail"),
                )
                .into_any_element();
        }

        let summary = match (&history.base, history.ahead) {
            (Some(base), ahead) if ahead > 0 => tf(
                if ahead == 1 {
                    "panel.history.ahead_one"
                } else {
                    "panel.history.ahead_other"
                },
                &[
                    (
                        "count",
                        &format!("{ahead}{}", if history.truncated { "+" } else { "" }),
                    ),
                    ("base", base),
                ],
            ),
            _ => tf(
                if history.commits.len() == 1 {
                    "panel.history.recent_one"
                } else {
                    "panel.history.recent_other"
                },
                &[(
                    "count",
                    &format!(
                        "{}{}",
                        history.commits.len(),
                        if history.truncated { "+" } else { "" }
                    ),
                )],
            ),
        };
        let unpushed = loaded.unpushed();
        let header = div()
            .flex_none()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(10.5))
            .text_color(palette.muted)
            .child(sf_symbol(
                "point.3.filled.connected.trianglepath.dotted",
                10.0,
                palette.muted,
            ))
            .child(div().min_w(px(0.0)).flex_1().truncate().child(summary))
            .when(unpushed > 0, |header| {
                header.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(3.0))
                        .text_color(palette.modified)
                        .child(sf_symbol("arrow.up", 9.0, palette.modified))
                        .child(tf("panel.history.not_pushed", &[("count", &unpushed)])),
                )
            });

        let entity = cx.entity();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        let props = crate::git_ui::commit_list::CommitListProps {
            loaded: Arc::clone(&loaded),
            selected: self.review_ui.selected_commit.clone(),
            palette,
            mono: font.clone(),
            now,
            on_pick: Rc::new(move |index, _, cx| {
                entity.update(cx, |this, cx| this.select_commit(index, cx));
            }),
        };
        let count = history.commits.len();
        let list = uniform_list("inspector-commits", count, move |range, _, _| {
            crate::git_ui::commit_list::render_commit_rows(&props, range)
        })
        .track_scroll(&self.review_ui.commit_scroll)
        .size_full();

        let picked = self.review_ui.selected_commit.clone().and_then(|oid| {
            history
                .commits
                .iter()
                .find(|commit| commit.oid == oid)
                .cloned()
        });
        let Some(commit) = picked else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(header)
                .child(div().min_h(px(0.0)).flex_1().child(list))
                .into_any_element();
        };

        // With a commit open, the list keeps a few rows above its diff.
        let list_height = (count as f32 * crate::git_ui::commit_list::COMMIT_ROW_HEIGHT)
            .min(crate::git_ui::commit_list::COMMIT_ROW_HEIGHT * 4.5);
        let diff = match self.review_ui.commit_diff.clone() {
            Some(CommitDiffLoad::Ready(snapshot)) if snapshot.rows.is_empty() => self
                .render_message(
                    colors,
                    "checkmark.circle",
                    t("panel.commit.no_files"),
                    t("panel.commit.no_files_detail"),
                )
                .into_any_element(),
            Some(CommitDiffLoad::Ready(snapshot)) => self
                .render_diff(snapshot, colors, window, cx)
                .into_any_element(),
            Some(CommitDiffLoad::Error(error)) => self
                .render_message(
                    colors,
                    "exclamationmark.triangle",
                    t("panel.commit.load_failed"),
                    error,
                )
                .into_any_element(),
            Some(CommitDiffLoad::Loading) | None => self
                .render_message(
                    colors,
                    "ellipsis",
                    t("panel.commit.loading"),
                    t("panel.commit.reading"),
                )
                .into_any_element(),
        };
        let stat = match &self.review_ui.commit_diff {
            Some(CommitDiffLoad::Ready(snapshot)) => Some(diff_view::diff_stat(
                snapshot.additions,
                snapshot.deletions,
                &palette,
            )),
            _ => None,
        };
        let short: String = commit.oid.chars().take(7).collect();
        let strip = div()
            .id("review-commit-strip")
            .debug_selector(|| "INSPECTOR_COMMIT_STRIP".to_owned())
            .flex_none()
            .h(px(30.0))
            .pl(px(10.0))
            .pr(px(6.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(palette.file)
            .border_t_1()
            .border_b_1()
            .border_color(palette.rule)
            .child(
                div()
                    .flex_none()
                    .font_family(font.clone())
                    .text_size(px(10.5))
                    .text_color(palette.accent)
                    .child(short),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(palette.text)
                    .child(commit.subject.clone()),
            )
            .children(stat)
            .child(
                div()
                    .id("review-commit-close")
                    .flex_none()
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .cursor_pointer()
                    .hover(move |button| button.bg(palette.hover))
                    .child(sf_symbol("xmark", 9.0, palette.muted))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.review_ui.clear_commit();
                        this.diff_selection.clear();
                        this.reset_diff_scroll();
                        cx.notify();
                        cx.stop_propagation();
                    })),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_none().h(px(list_height)).child(list))
            .child(strip)
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(diff))
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
                            .child(t("panel.ask.title")),
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
                        t("panel.tab.review"),
                        "Review this for correctness, regressions, and missing tests.",
                        colors,
                        cx,
                    ))
                    .child(self.render_ask_preset(
                        "ask-preset-risks",
                        t("panel.ask.find_risks"),
                        "Find the highest-risk behavior changes and explain why they matter.",
                        colors,
                        cx,
                    ))
                    .child(self.render_ask_preset(
                        "ask-preset-tests",
                        t("panel.ask.suggest_tests"),
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
                                    .child(t("panel.ask.placeholder"))
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
                            .child(if busy {
                                t("panel.ask.sending")
                            } else {
                                t("panel.ask.send")
                            })
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

/// The API surface (`crate::api_client`): tab creation, rendering, and the
/// `open_api_request` MCP path.
impl WorkbenchInspector {
    fn new_api_client(
        &mut self,
        project: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Entity<crate::api_client::ApiClient> {
        let library =
            crate::api_client::library_for(&self.api_store_root, project.unwrap_or("default"), cx);
        let colors = self.panel_colors();
        let tokio = self.tokio.clone();
        let api = cx.new(|cx| crate::api_client::ApiClient::new(tokio, library, colors, cx));
        cx.observe(&api, |_, _, cx| cx.notify()).detach();
        api
    }

    fn active_api(&self) -> Option<&Entity<crate::api_client::ApiClient>> {
        self.workspace_tabs
            .iter()
            .find(|tab| Some(tab.id) == self.workspace_active)
            .and_then(|tab| tab.api.as_ref())
    }

    fn render_api(&self, colors: SemanticColors) -> AnyElement {
        match self.active_api() {
            Some(api) => div()
                .id("workspace-api")
                .size_full()
                .child(api.clone())
                .into_any_element(),
            None => self
                .render_message(
                    colors,
                    "server.rack",
                    t("panel.select_session"),
                    t("panel.api.empty"),
                )
                .into_any_element(),
        }
    }

    fn sync_api_colors(&self, cx: &mut Context<Self>) {
        let colors = self.panel_colors();
        for tab in &self.workspace_tabs {
            if let Some(api) = &tab.api {
                api.update(cx, |api, cx| api.set_colors(colors, cx));
            }
        }
    }

    fn api_focused(&self, window: &Window, cx: &App) -> bool {
        self.workspace_selected == Some(WorkspaceSurface::Api)
            && self
                .active_api()
                .is_some_and(|api| api.read(cx).has_focus(window))
    }

    /// Opens a request an agent sent with `open_api_request` in that
    /// Session's API tab: a fresh tab, or one still showing an untouched
    /// request. For the Session in front it becomes the active tab and the
    /// caller opens the panel (returns true); for another Session it waits as
    /// that Session's active tab. Only a `GET` marked `autoSend` is sent.
    pub(crate) fn open_api_request(
        &mut self,
        session: SessionId,
        draft: diri_proto::ApiRequestDraft,
        cx: &mut Context<Self>,
    ) -> bool {
        if draft.validate().is_err() {
            return false;
        }
        self.sync_workspace_session(cx);
        let project = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            match store.sessions().get(&session) {
                Some(record) if !record.is_archived() => record.project_id.0.clone(),
                _ => return false,
            }
        };
        if self.workspace_session.as_ref() == Some(&session) {
            let reusable = self
                .workspace_tabs
                .iter()
                .find(|tab| {
                    tab.surface == WorkspaceSurface::Api
                        && tab
                            .api
                            .as_ref()
                            .is_some_and(|api| api.read(cx).is_pristine())
                })
                .map(|tab| tab.id);
            let id = match reusable {
                Some(id) => id,
                None => {
                    let id = self.next_workspace_id;
                    self.next_workspace_id += 1;
                    let mut tab = WorkspaceTab::new(id, WorkspaceSurface::Api);
                    tab.api = Some(self.new_api_client(Some(&project), cx));
                    self.workspace_tabs.push(tab);
                    id
                }
            };
            if let Some(api) = self
                .workspace_tabs
                .iter()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.api.clone())
            {
                api.update(cx, |api, cx| api.open_draft(&draft, cx));
            }
            if self.workspace_active == Some(id) {
                cx.notify();
            } else if let Some(surface) = self.load_workspace(id, cx) {
                cx.emit(InspectorEvent::WorkspaceRestored(surface));
            }
            return true;
        }
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;
        let api = self.new_api_client(Some(&project), cx);
        api.update(cx, |api, cx| api.open_draft(&draft, cx));
        let mut tab = WorkspaceTab::new(id, WorkspaceSurface::Api);
        tab.api = Some(api);
        let details = self.next_workspace_id;
        self.next_workspace_id += 1;
        let workspace = self
            .session_workspaces
            .entry(Some(session))
            .or_insert_with(|| SessionWorkspace {
                tabs: vec![WorkspaceTab::new(details, WorkspaceSurface::Details)],
                active: None,
                visible: true,
                next_terminal_slot: 0,
            });
        workspace.tabs.push(tab);
        workspace.active = Some(id);
        workspace.visible = true;
        false
    }
}

impl Render for WorkbenchInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("inspector");
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::right_panel::panel_colors_in(&store)
        };
        let held_hint = crate::held_hints::opacity(window, cx);
        // A hosted shell paints the terminal fill itself. Under a glass window
        // that fill is translucent, and two layers of it would read darker
        // than the session beside it, so the panel fills only its header then.
        let background = crate::right_panel::panel_background(colors);
        let hosted_terminal = self.workspace_selected == Some(WorkspaceSurface::Terminal)
            && self.terminal_surface.is_some();
        let session = self.selected_session();
        let body = match self.workspace_selected {
            Some(WorkspaceSurface::Details) => {
                let detail = match self.selected_tab {
                    InspectorTab::Info => self.render_info(session.as_ref(), colors, cx),
                    InspectorTab::Artifacts => self.render_artifacts(session.as_ref(), colors, cx),
                    InspectorTab::Changes => self.render_changes(colors, window, cx),
                    InspectorTab::Code => self.code_viewer.clone().into_any_element(),
                };
                div()
                    .id("workspace-details")
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(self.render_header(session.as_ref(), colors, cx))
                    .child(
                        div()
                            .min_h(px(0.0))
                            .flex_1()
                            .overflow_hidden()
                            .child(detail),
                    )
                    .into_any_element()
            }
            Some(WorkspaceSurface::Browser) => self.render_browser(colors, cx),
            Some(WorkspaceSurface::Terminal) => self.render_terminal(colors),
            Some(WorkspaceSurface::Files) => self.code_viewer.clone().into_any_element(),
            Some(WorkspaceSurface::Review) => self.render_changes(colors, window, cx),
            // The last tab just closed and the panel is sliding shut: paint
            // nothing rather than flash the chooser on the way out.
            None if !self.visible && self.workspace_tabs.is_empty() => {
                div().size_full().into_any_element()
            }
            Some(WorkspaceSurface::Api) => self.render_api(colors),
            None => self.render_surface_chooser(colors, cx),
        };
        let transition_id = SharedString::from(format!(
            "inspector-tab-transition-{}",
            self.tab_transition_generation
        ));
        let direction = self.tab_direction;
        let ask_composer = matches!(
            self.workspace_selected,
            Some(WorkspaceSurface::Details | WorkspaceSurface::Review)
        )
        .then(|| self.render_ask_composer(colors, cx))
        .flatten();
        let body = div().relative().size_full().child(body);
        // A native child cannot share GPUI's opacity or clipping animation.
        // Keep its measured viewport stable when entering the Browser tab.
        let body =
            if cx.reduce_motion() || self.workspace_selected == Some(WorkspaceSurface::Browser) {
                body.into_any_element()
            } else {
                body.with_animation(
                    transition_id,
                    Animation::new(Duration::from_millis(190)).with_easing(ease_out_quint()),
                    move |body, delta| {
                        body.left(px(direction * (1.0 - delta) * 8.0))
                            .opacity(0.70 + 0.30 * delta)
                    },
                )
                .into_any_element()
            };
        div()
            .id("workbench-inspector")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .relative()
            .when(!hosted_terminal, |panel| panel.bg(background))
            .text_color(colors.primary)
            .child(
                div()
                    .flex_none()
                    .when(hosted_terminal, |header| header.bg(background))
                    .child(self.render_workspace_header(colors, held_hint, cx)),
            )
            .when_some(
                self.render_worktree_follow(session.as_ref(), colors, cx),
                |panel, bar| panel.child(bar),
            )
            .child(div().min_h(px(0.0)).flex_1().overflow_hidden().child(body))
            .when_some(ask_composer, |panel, composer| panel.child(composer))
            .when(self.workspace_chooser_open, |panel| {
                panel.child(self.render_add_menu(colors, cx))
            })
            .children(crate::perf_overlay::badge("inspector"))
    }
}

impl crate::workspace_follow::FollowHost for WorkbenchInspector {
    fn follow(&mut self) -> &mut crate::workspace_follow::FollowController {
        &mut self.follow
    }

    fn follow_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_if_context_changed(cx);
        cx.notify();
    }

    fn new_agent_from_default_branch(&mut self, session: SessionId, cx: &mut Context<Self>) {
        cx.emit(InspectorEvent::NewAgentFromDefaultBranch(session));
    }
}

fn status_evidence_explanation(source: diri_proto::StatusEvidenceSource) -> &'static str {
    t(match source {
        diri_proto::StatusEvidenceSource::Hook => "panel.evidence.source.hook",
        diri_proto::StatusEvidenceSource::Notify => "panel.evidence.source.notify",
        diri_proto::StatusEvidenceSource::ScreenRule => "panel.evidence.source.screen_rule",
        diri_proto::StatusEvidenceSource::ProcessLiveness => "panel.evidence.source.process",
        diri_proto::StatusEvidenceSource::Staleness => "panel.evidence.source.staleness",
        diri_proto::StatusEvidenceSource::Transport => "panel.evidence.source.transport",
        diri_proto::StatusEvidenceSource::ProgramStatus => "panel.evidence.source.program_status",
        diri_proto::StatusEvidenceSource::Unknown => "panel.evidence.source.unknown",
    })
}

/// A quiet one-line caption under a row title.
fn meta_text(text: String, colors: SemanticColors) -> AnyElement {
    div()
        .min_w(px(0.0))
        .truncate()
        .text_size(px(Typo::META.size))
        .font_weight(FontWeight::NORMAL)
        .text_color(colors.tertiary)
        .child(text)
        .into_any_element()
}

fn artifact_look(kind: &ArtifactKind) -> (IconName, &'static str) {
    match kind {
        ArtifactKind::PullRequest => (IconName::PullRequest, t("panel.pull_request")),
        ArtifactKind::LinearIssue => (IconName::Linear, t("panel.artifact.linear_issue")),
        ArtifactKind::Preview => (IconName::Monitor, t("panel.artifact.preview")),
        ArtifactKind::Link | ArtifactKind::Unknown => {
            (IconName::ExternalLink, t("panel.artifact.link"))
        }
    }
}

fn render_artifact_row(artifact: &SessionArtifact, colors: SemanticColors) -> AnyElement {
    let (glyph, kind_label) = artifact_look(&artifact.kind);
    let url = artifact.url.clone();
    details_ui::list_row(
        SharedString::from(format!("inspector-artifact-{}", artifact.url)),
        details_ui::icon_tile(glyph, colors.secondary, colors),
        artifact_title(artifact),
        Some(format!(
            "{kind_label} · {}",
            details_ui::relative_time(artifact.first_seen_at.0)
        )),
        Some(details_ui::icon(
            IconName::ExternalLink,
            12.0,
            colors.tertiary,
        )),
        colors,
    )
    .on_click(move |_, _, cx| cx.open_url(&url))
    .into_any_element()
}

/// The kinds of artifact a session has, for the Info summary row:
/// `Pull request · 2 links · Port`.
fn artifact_kind_summary(session: &SessionRecord) -> String {
    let artifacts = session.artifacts.as_deref().unwrap_or_default();
    let pull_requests = session.pull_requests.as_deref().unwrap_or_default().len()
        + artifacts
            .iter()
            .filter(|artifact| {
                artifact.kind == ArtifactKind::PullRequest
                    && !session
                        .pull_requests
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|status| status.url == artifact.url)
            })
            .count();
    let count_of = |kind: ArtifactKind| {
        artifacts
            .iter()
            .filter(|artifact| artifact.kind == kind)
            .count()
    };
    let links = count_of(ArtifactKind::Link) + count_of(ArtifactKind::Unknown);
    let ports = session.listening_ports.as_deref().unwrap_or_default().len();
    [
        (
            pull_requests,
            "panel.artifacts.kind.pull_requests_one",
            "panel.artifacts.kind.pull_requests_other",
        ),
        (
            count_of(ArtifactKind::LinearIssue),
            "panel.artifacts.kind.issues_one",
            "panel.artifacts.kind.issues_other",
        ),
        (
            count_of(ArtifactKind::Preview),
            "panel.artifacts.kind.previews_one",
            "panel.artifacts.kind.previews_other",
        ),
        (
            links,
            "panel.artifacts.kind.links_one",
            "panel.artifacts.kind.links_other",
        ),
        (
            ports,
            "panel.artifacts.kind.ports_one",
            "panel.artifacts.kind.ports_other",
        ),
    ]
    .iter()
    .filter(|(count, _, _)| *count > 0)
    .map(|(count, one, many)| tf(if *count == 1 { one } else { many }, &[("count", count)]))
    .collect::<Vec<_>>()
    .join(" · ")
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
        return (t("panel.status.sleeping"), colors.secondary);
    }
    match session.status {
        SessionStatus::Starting => (
            t("panel.status.starting"),
            Ink::working(ui_agent_kind(session.effective_kind()), colors),
        ),
        SessionStatus::Working => (
            t("panel.status.working"),
            Ink::working(ui_agent_kind(session.effective_kind()), colors),
        ),
        SessionStatus::NeedsInput(_) => {
            let destructive = session
                .needs_input
                .as_ref()
                .is_some_and(|detail| detail.risk_hint == diri_proto::RiskHint::Destructive);
            (
                t("panel.status.needs_input"),
                if destructive {
                    Ink::DANGER
                } else {
                    Ink::ATTENTION
                },
            )
        }
        SessionStatus::Idle if session.attention() == diri_proto::AttentionLevel::DoneUnseen => {
            (t("panel.status.finished"), Ink::FRESH)
        }
        SessionStatus::Idle => (t("panel.status.idle"), colors.secondary),
        SessionStatus::Exited(_) => (t("panel.status.ended"), colors.tertiary),
        SessionStatus::Unknown => (t("panel.status.unknown"), colors.tertiary),
    }
}

fn artifact_title(artifact: &SessionArtifact) -> String {
    match artifact.kind {
        ArtifactKind::PullRequest => pr_number(&artifact.url)
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| t("panel.pull_request").to_owned()),
        ArtifactKind::LinearIssue => {
            linear_key(&artifact.url).unwrap_or_else(|| t("panel.artifact.linear_issue").to_owned())
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

fn prompt_layer(layer: DiffLayer) -> ReviewLayer {
    match layer {
        DiffLayer::Branch => ReviewLayer::Branch,
        DiffLayer::Staged => ReviewLayer::Staged,
        DiffLayer::Working => ReviewLayer::Working,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffRow, DiffRowKind};
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use diri_proto::DateMillis;
    use gpui::{Entity, Modifiers, TestAppContext};
    use std::io::Write;

    struct InspectorHarness {
        inspector: Entity<WorkbenchInspector>,
    }

    fn composite(foreground: gpui::Rgba, background: gpui::Rgba) -> gpui::Rgba {
        let alpha = foreground.a + background.a * (1.0 - foreground.a);
        if alpha == 0.0 {
            return rgba(0x00000000);
        }
        gpui::Rgba {
            r: (foreground.r * foreground.a + background.r * background.a * (1.0 - foreground.a))
                / alpha,
            g: (foreground.g * foreground.a + background.g * background.a * (1.0 - foreground.a))
                / alpha,
            b: (foreground.b * foreground.a + background.b * background.a * (1.0 - foreground.a))
                / alpha,
            a: alpha,
        }
    }

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

    impl Render for InspectorHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.0))
                .h_full()
                .overflow_hidden()
                .child(self.inspector.clone())
        }
    }

    /// A Working-lane snapshot whose file-count notice has names to expand.
    fn omitted_untracked_preview() -> DiffSnapshot {
        const OMITTED: usize = 5_000;
        let mut snapshot = crate::diff::parse_unified_diff(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n",
        );
        snapshot.layer = DiffLayer::Working;
        snapshot.truncated = true;
        snapshot.omitted_untracked = OMITTED;
        snapshot.omitted_untracked_paths = (0..OMITTED)
            .map(|index| PathBuf::from(format!("generated/file-{index:04}.txt")))
            .collect();
        snapshot.rows.push(DiffRow {
            kind: DiffRowKind::Meta,
            old_line: None,
            new_line: None,
            text: "5000 more untracked files not shown (limit 200); Stage all still includes them"
                .to_owned(),
        });
        snapshot
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the isolated workspace-panel screenshot"]
    fn render_workspace_preview_screenshot() {
        let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("output path");
        // DIRI_VISUAL_LANGUAGE=zh-Hans renders the page in that catalog.
        if let Some(language) = std::env::var("DIRI_VISUAL_LANGUAGE")
            .ok()
            .and_then(|tag| crate::i18n::Language::from_tag(&tag))
        {
            diri_i18n::set_language(language);
        }
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let window = cx
            .open_window(
                gpui::size(
                    px(std::env::var("DIRI_VISUAL_WIDTH")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(700.0)),
                    px(700.0),
                ),
                |_, cx| {
                    let runtime = Arc::new(StoreRuntime::inert());
                    if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                        runtime
                            .store
                            .write()
                            .unwrap()
                            .update_preferences(|prefs| {
                                prefs.terminal_theme = "dirijor-light".into()
                            })
                            .unwrap();
                    }
                    let tokio = Arc::new(
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap(),
                    );
                    cx.new(|cx| {
                        let mut inspector = WorkbenchInspector::new(runtime, tokio, cx);
                        inspector.workspace_tabs.clear();
                        inspector.workspace_active = None;
                        inspector.workspace_selected = None;
                        if std::env::var_os("DIRI_VISUAL_FILES").is_some() {
                            inspector.add_workspace(WorkspaceSurface::Files, cx);
                            inspector.add_workspace(WorkspaceSurface::Files, cx);
                            inspector
                                .code_viewer
                                .update(cx, |viewer, cx| viewer.seed_explorer_preview(cx));
                        }
                        if let Some(scenario) = std::env::var_os("DIRI_VISUAL_REVIEW") {
                            seed_review_preview(&mut inspector, &scenario.to_string_lossy(), cx);
                        }
                        if std::env::var_os("DIRI_VISUAL_OMITTED").is_some() {
                            inspector.select_workspace(WorkspaceSurface::Review, cx);
                            inspector.state =
                                LoadState::Ready(Arc::new(omitted_untracked_preview()));
                            inspector.omitted_untracked_open = true;
                        }
                        if std::env::var_os("DIRI_VISUAL_BROWSER").is_some() {
                            inspector.select_workspace(WorkspaceSurface::Review, cx);
                            inspector.select_workspace(WorkspaceSurface::Browser, cx);
                            if std::env::var_os("DIRI_VISUAL_BROWSER_TABS").is_some() {
                                inspector.browser_state.title = Some("Local preview".into());
                                inspector.add_workspace(WorkspaceSurface::Browser, cx);
                                inspector.browser_state = BrowserState {
                                    url: Some("https://diri.app/docs".into()),
                                    title: Some("Diri documentation and guides".into()),
                                    favicon: Some(Arc::new(gpui::Image::from_bytes(
                                        gpui::ImageFormat::Png,
                                        include_bytes!("../../../assets/icon.png").to_vec(),
                                    ))),
                                    ..BrowserState::default()
                                };
                                inspector.browser_query.insert("https://diri.app/docs");
                            }
                        }
                        inspector
                    })
                },
            )
            .expect("headless window");
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .expect("screenshot")
            .save(output)
            .expect("save");
    }

    /// Fills the review with a synthetic branch for the offscreen
    /// screenshot: `changes`, `split`, `commits`, or `commit` (one picked).
    #[cfg(target_os = "macos")]
    fn seed_review_preview(
        inspector: &mut WorkbenchInspector,
        scenario: &str,
        cx: &mut Context<WorkbenchInspector>,
    ) {
        use crate::git_review::{
            BranchInfo, ChangeKind, CommitHistory, CommitRef, CommitRefKind, CommitSummary,
            FileChange,
        };
        const PATCH: &str = "diff --git a/src/review/diff_view.rs b/src/review/diff_view.rs
index 3f2a9c1..8b41d07 100644
--- a/src/review/diff_view.rs
+++ b/src/review/diff_view.rs
@@ -42,9 +42,11 @@ impl DiffView {
     pub fn render(&self, rows: Range<usize>) -> Vec<Row> {
-        let height = 18.0;
-        let gutter = self.digits * 7;
+        let height = ROW_HEIGHT;
+        let gutter = self.digits * DIGIT_WIDTH + 12;
         rows.map(|index| {
             let row = &self.rows[index];
-            draw(row, height)
+            let words = self.words_for(index);
+            draw(row, height, words)
         })
         .collect()
     }
@@ -88,6 +90,7 @@ impl DiffView {
     fn wash(kind: Kind) -> Color {
         match kind {
             Kind::Added => GREEN.alpha(0.12),
+            Kind::Moved => BLUE.alpha(0.10),
             Kind::Removed => RED.alpha(0.12),
         }
     }
diff --git a/src/review/graph.rs b/src/review/graph.rs
new file mode 100644
index 0000000..5d7f1e2
--- /dev/null
+++ b/src/review/graph.rs
@@ -0,0 +1,6 @@
+/// Lanes for commits given newest first.
+pub fn lanes(commits: &[Commit]) -> Vec<Row> {
+    let mut waiting = Vec::new();
+    commits.iter().map(|commit| place(commit, &mut waiting)).collect()
+}
+
diff --git a/README.md b/README.md
index 1111111..2222222 100644
--- a/README.md
+++ b/README.md
@@ -1,3 +1,3 @@
 # Review
-A quick look at what changed.
+A careful look at what changed, word by word.
 
";
        let mut snapshot = crate::diff::parse_unified_diff(PATCH);
        snapshot.layer = DiffLayer::Working;
        snapshot.repo_root = PathBuf::from("/tmp/preview");
        let snapshot = Arc::new(snapshot);
        let change = |path: &str, kind| FileChange {
            path: PathBuf::from(path),
            original_path: None,
            kind,
        };
        inspector.select_workspace(WorkspaceSurface::Review, cx);
        inspector.diff_layer = DiffLayer::Working;
        inspector.context = Some(DiffContext {
            id: SessionId("preview".to_owned()),
            cwd: PathBuf::from("/tmp/preview"),
            launch_cwd: PathBuf::from("/tmp/preview"),
            remote: false,
            agent_session_id: None,
            transcript_path: None,
            kind: ProtoAgentKind::CLAUDE_CODE,
        });
        inspector.review_state = ReviewLoadState::Ready(Arc::new(ReviewStatus {
            repo_root: PathBuf::from("/tmp/preview"),
            branch: BranchInfo {
                name: Some("polish/right-panel-review".to_owned()),
                oid: Some("c0ffee".to_owned()),
                upstream: Some("origin/polish/right-panel-review".to_owned()),
                ahead: 2,
                behind: 0,
            },
            staged: vec![change("README.md", ChangeKind::Modified)],
            unstaged: vec![change("src/review/diff_view.rs", ChangeKind::Modified)],
            untracked: vec![change("src/review/graph.rs", ChangeKind::Added)],
            conflicted: Vec::new(),
        }));
        inspector.state = LoadState::Ready(Arc::clone(&snapshot));
        // Select the replaced lines of the first hunk.
        inspector.diff_selection.select(&snapshot, 3, false);
        inspector.diff_selection.select(&snapshot, 6, true);
        if scenario == "split" {
            inspector.review_ui.layout = DiffLayout::Split;
            inspector.review_ui.split_fits.set(true);
        }
        if scenario == "commits" || scenario == "commit" {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs() as i64);
            let commit = |oid: &str, parents: &[&str], subject: &str, author: &str, ago: i64| {
                CommitSummary {
                    oid: format!("{oid}{}", "0".repeat(40 - oid.len())),
                    parents: parents
                        .iter()
                        .map(|parent| format!("{parent}{}", "0".repeat(40 - parent.len())))
                        .collect(),
                    author: author.to_owned(),
                    timestamp: now - ago,
                    subject: subject.to_owned(),
                    refs: Vec::new(),
                    on_branch: true,
                    pushed: true,
                }
            };
            let mut commits = vec![
                commit(
                    "a1",
                    &["a2"],
                    "Mark changed words in the diff viewer",
                    "Cristian Cretu",
                    600,
                ),
                commit(
                    "a2",
                    &["a3", "b1"],
                    "Merge branch 'graph' into polish/right-panel-review",
                    "Cristian Cretu",
                    3_600,
                ),
                commit(
                    "b1",
                    &["b2"],
                    "Paint commit graph lanes",
                    "Ada Lovelace",
                    7_200,
                ),
                commit(
                    "a3",
                    &["b2"],
                    "Split layout for wide panels",
                    "Cristian Cretu",
                    18_000,
                ),
                commit(
                    "b2",
                    &["c1"],
                    "Port Ely's diff stat and status badges",
                    "Grace Hopper",
                    86_400,
                ),
                commit(
                    "c1",
                    &["c2"],
                    "Keep the omitted-untracked notice expandable",
                    "Cristian Cretu",
                    3 * 86_400,
                ),
                commit(
                    "c2",
                    &["c3"],
                    "Never spawn client event subscriptions outside a runtime",
                    "Linus T.",
                    5 * 86_400,
                ),
                commit(
                    "c3",
                    &[],
                    "Ending a session lands on the next one",
                    "Linus T.",
                    9 * 86_400,
                ),
            ];
            commits[0].refs = vec![CommitRef {
                name: "polish/right-panel-review".to_owned(),
                kind: CommitRefKind::Head,
            }];
            commits[0].pushed = false;
            commits[2].refs = vec![CommitRef {
                name: "origin/graph".to_owned(),
                kind: CommitRefKind::Remote,
            }];
            commits[5].refs = vec![CommitRef {
                name: "origin/main".to_owned(),
                kind: CommitRefKind::Remote,
            }];
            for commit in &mut commits[5..] {
                commit.on_branch = false;
            }
            let head = Some(commits[0].oid.clone());
            let picked = commits[0].oid.clone();
            inspector.review_ui.mode = ReviewMode::Commits;
            inspector.review_ui.history =
                HistoryLoad::Ready(Arc::new(LoadedHistory::new(CommitHistory {
                    head,
                    base: Some("origin/main".to_owned()),
                    commits,
                    ahead: 5,
                    truncated: false,
                    has_remote: true,
                })));
            inspector.diff_selection.clear();
            if scenario == "commit" {
                let mut commit_snapshot = (*snapshot).clone();
                commit_snapshot.layer = DiffLayer::Branch;
                inspector.review_ui.selected_commit = Some(picked);
                inspector.review_ui.commit_diff =
                    Some(CommitDiffLoad::Ready(Arc::new(commit_snapshot)));
            }
        }
    }

    /// Renders the Details surface (Info, or Artifacts with
    /// `DIRI_VISUAL_DETAILS=artifacts`) for the selected Artifacts-fixture
    /// session into `DIRI_VISUAL_OUTPUT`, headless.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the isolated Details-surface screenshot"]
    fn render_details_preview_screenshot() {
        let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("output path");
        // DIRI_VISUAL_LANGUAGE=zh-Hans renders the page in that catalog.
        if let Some(language) = std::env::var("DIRI_VISUAL_LANGUAGE")
            .ok()
            .and_then(|tag| crate::i18n::Language::from_tag(&tag))
        {
            diri_i18n::set_language(language);
        }
        let dimension = |name: &str, fallback: f32| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(fallback)
        };
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let window = cx
            .open_window(
                gpui::size(
                    px(dimension("DIRI_VISUAL_WIDTH", 340.0)),
                    px(dimension("DIRI_VISUAL_HEIGHT", 1400.0)),
                ),
                |_, cx| {
                    let runtime = Arc::new(StoreRuntime::inert());
                    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Artifacts);
                    if let Some(session) =
                        fixture.list.sessions.iter_mut().find(|session| {
                            Some(&session.id) == fixture.selected_session_id.as_ref()
                        })
                    {
                        let seen = DateMillis(session.updated_at.0);
                        session.artifacts.get_or_insert_with(Vec::new).extend([
                            SessionArtifact {
                                kind: ArtifactKind::Preview,
                                url: "https://feature-dirijor.vercel.app/build".to_owned(),
                                first_seen_at: seen,
                            },
                            SessionArtifact {
                                kind: ArtifactKind::LinearIssue,
                                url: "https://linear.app/acme/issue/DIR-19/polish".to_owned(),
                                first_seen_at: seen,
                            },
                        ]);
                        session.listening_ports = Some(vec![diri_proto::PortInfo {
                            port: 3000,
                            process_name: "node".to_owned(),
                        }]);
                    }
                    {
                        let mut store = runtime.store.write().unwrap();
                        if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                            store
                                .update_preferences(|prefs| {
                                    prefs.terminal_theme = "dirijor-light".into()
                                })
                                .unwrap();
                        }
                        let selected = fixture.selected_session_id.clone();
                        store.hydrate(fixture.list);
                        if let Some(selected) = selected {
                            store.select(selected);
                        }
                    }
                    let tokio = Arc::new(
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap(),
                    );
                    cx.new(|cx| {
                        let mut inspector = WorkbenchInspector::new(runtime, tokio, cx);
                        inspector.select_workspace(WorkspaceSurface::Details, cx);
                        inspector.selected_tab = match std::env::var("DIRI_VISUAL_DETAILS") {
                            Ok(tab) if tab == "artifacts" => InspectorTab::Artifacts,
                            _ => InspectorTab::Info,
                        };
                        inspector.state = LoadState::Ready(Arc::new(DiffSnapshot {
                            files: 8,
                            additions: 431,
                            deletions: 381,
                            ..DiffSnapshot::default()
                        }));
                        inspector
                    })
                },
            )
            .expect("headless window");
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .expect("screenshot")
            .save(output)
            .expect("save");
    }

    #[test]
    fn inspector_tabs_have_stable_spatial_order() {
        assert!(InspectorTab::Info.index() < InspectorTab::Changes.index());
        assert!(InspectorTab::Changes.index() < InspectorTab::Code.index());
        assert!(InspectorTab::Code.index() < InspectorTab::Artifacts.index());
    }

    #[gpui::test]
    fn workspace_surfaces_are_closable_without_rewriting_inspector_preferences(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));

        inspector.update(cx, |inspector, cx| {
            inspector.select_workspace(WorkspaceSurface::Browser, cx);
            assert!(inspector.close_active_workspace(cx));
            inspector.close_workspace(0, cx);
            assert!(!inspector.close_active_terminal(cx));
            inspector.select_workspace(WorkspaceSurface::Terminal, cx);
            assert!(inspector.close_active_terminal(cx));
            assert!(!inspector.workspace_needs_terminal());
        });

        inspector.read_with(cx, |inspector, _| {
            assert!(inspector.workspace_tabs.is_empty());
            assert_eq!(inspector.workspace_selected, None);
        });
        assert_eq!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .preferences()
                .inspector_tab,
            InspectorTab::Info
        );
    }

    fn test_inspector(
        cx: &mut TestAppContext,
    ) -> (Entity<WorkbenchInspector>, &mut gpui::VisualTestContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (inspector, cx) =
            cx.add_window_view(move |_, cx| WorkbenchInspector::new(runtime, tokio, cx));
        cx.simulate_resize(gpui::size(px(420.0), px(600.0)));
        (inspector, cx)
    }

    /// The last tab takes the panel with it, the panel paints nothing while it
    /// slides shut, and reopening lands on a real tab rather than a chooser.
    #[gpui::test]
    fn closing_the_last_tab_closes_the_panel_and_reopening_seeds_a_tab(cx: &mut TestAppContext) {
        let (inspector, cx) = test_inspector(cx);
        let closes = std::rc::Rc::new(std::cell::Cell::new(0));
        let _subscription = cx.update(|_, cx| {
            let closes = closes.clone();
            cx.subscribe(&inspector, move |_, event: &InspectorEvent, _| {
                if matches!(event, InspectorEvent::Close) {
                    closes.set(closes.get() + 1);
                }
            })
        });
        inspector.update(cx, |inspector, cx| {
            inspector.set_visible(true, cx);
            inspector.add_workspace(WorkspaceSurface::Review, cx);
            inspector.add_workspace(WorkspaceSurface::Browser, cx);
        });
        cx.run_until_parked();
        inspector.update(cx, |inspector, cx| {
            assert!(inspector.close_active_workspace(cx));
            assert!(inspector.close_active_workspace(cx));
        });
        cx.run_until_parked();
        assert_eq!(
            closes.get(),
            0,
            "closing a tab with siblings keeps the panel"
        );
        inspector.update(cx, |inspector, cx| {
            assert!(inspector.close_active_workspace(cx));
            assert!(inspector.workspace_tabs.is_empty());
        });
        cx.run_until_parked();
        assert_eq!(closes.get(), 1, "the last tab closes the panel");

        // The root answers Close by hiding the panel; while it slides shut
        // the body stays blank instead of flashing the chooser.
        inspector.update(cx, |inspector, cx| inspector.set_visible(false, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-empty").is_none());
        assert!(cx.debug_bounds("workspace-open-Review").is_none());

        inspector.update(cx, |inspector, cx| {
            inspector.set_visible(true, cx);
            assert_eq!(inspector.workspace_tabs.len(), 1);
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Details),
                "reopening lands on the last details destination shown"
            );
            assert!(inspector.workspace_active.is_some());
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-empty").is_none());
        assert!(cx.debug_bounds("INSPECTOR_TOGGLE").is_some());

        // Closing tabs of a hidden panel never asks to close it again.
        inspector.update(cx, |inspector, cx| {
            inspector.set_visible(false, cx);
            assert!(inspector.close_active_workspace(cx));
        });
        cx.run_until_parked();
        assert_eq!(closes.get(), 1);
    }

    /// If the panel is ever open with nothing in it, the fallback is a quiet
    /// line of plain choices, not a grid of bordered cards.
    #[gpui::test]
    fn an_empty_open_panel_offers_compact_choices(cx: &mut TestAppContext) {
        let (inspector, cx) = test_inspector(cx);
        inspector.update(cx, |inspector, cx| {
            inspector.set_visible(true, cx);
            inspector.workspace_tabs.clear();
            inspector.workspace_active = None;
            inspector.workspace_selected = None;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-empty").is_some());
        let review = cx
            .debug_bounds("workspace-open-Review")
            .expect("review choice");
        assert!(review.size.height <= px(crate::right_panel::TAB_HEIGHT + 0.5));
        cx.simulate_click(review.center(), Modifiers::default());
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(inspector.workspace_selected, Some(WorkspaceSurface::Review));
        });
        assert!(cx.debug_bounds("workspace-empty").is_none());
    }

    /// The + menu is a menu: a click anywhere else dismisses it without
    /// adding a tab, and a second click on + closes rather than reopens it.
    #[gpui::test]
    fn the_add_menu_dismisses_on_an_outside_click(cx: &mut TestAppContext) {
        let (inspector, cx) = test_inspector(cx);
        cx.run_until_parked();
        let before = inspector.read_with(cx, |inspector, _| inspector.workspace_tabs.len());
        let add = cx.debug_bounds("workspace-add-surface").unwrap();
        cx.simulate_click(add.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-surface-catalog").is_some());
        cx.simulate_click(gpui::point(px(200.0), px(400.0)), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-surface-catalog").is_none());

        cx.simulate_click(add.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-surface-catalog").is_some());
        cx.simulate_click(add.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("workspace-surface-catalog").is_none());
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.workspace_tabs.len()),
            before
        );
    }

    /// Tabs keep their width when selection moves: one font weight for every
    /// state and a close slot that is always laid out.
    #[gpui::test]
    fn tab_widths_do_not_change_with_selection(cx: &mut TestAppContext) {
        let (inspector, cx) = test_inspector(cx);
        let (first, second) = inspector.update(cx, |inspector, cx| {
            let first = inspector.workspace_active.unwrap();
            inspector.add_workspace(WorkspaceSurface::Review, cx);
            (first, inspector.workspace_active.unwrap())
        });
        cx.run_until_parked();
        let width = |cx: &mut gpui::VisualTestContext, id: u64| {
            cx.debug_bounds(format!("workspace-tab-{id}").leak())
                .unwrap()
                .size
                .width
        };
        let (a, b) = (width(cx, first), width(cx, second));
        inspector.update(cx, |inspector, cx| inspector.activate_workspace(first, cx));
        cx.run_until_parked();
        assert_eq!(width(cx, first), a);
        assert_eq!(width(cx, second), b);
        assert!(
            cx.debug_bounds(format!("close-workspace-{second}").leak())
                .is_some(),
            "the inactive tab still reserves its close slot"
        );
    }

    #[gpui::test]
    fn agent_api_requests_open_in_their_sessions_api_tab(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let ids: Vec<_> = fixture
            .list
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let temp = tempfile::tempdir().unwrap();
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        let draft = |method: &str| diri_proto::ApiRequestDraft {
            method: method.into(),
            url: "http://localhost:3000/items".into(),
            ..Default::default()
        };
        inspector.update(cx, |inspector, cx| {
            inspector.api_store_root = temp.path().to_path_buf();
            // The Session in front: its API tab opens and is active.
            assert!(inspector.open_api_request(ids[0].clone(), draft("POST"), cx));
            assert_eq!(inspector.workspace_selected, Some(WorkspaceSurface::Api));
            let api_tabs = |inspector: &WorkbenchInspector| {
                inspector
                    .workspace_tabs
                    .iter()
                    .filter(|tab| tab.surface == WorkspaceSurface::Api)
                    .count()
            };
            assert_eq!(api_tabs(inspector), 1);
            // An untouched tab is reused by the next request.
            assert!(inspector.open_api_request(ids[0].clone(), draft("GET"), cx));
            assert_eq!(api_tabs(inspector), 1);
            let api = inspector.active_api().unwrap().clone();
            assert_eq!(
                api.read(cx).draft.method,
                crate::api_client::model::Method::Get
            );
            // Another Session's request waits in that Session's tabs.
            assert!(!inspector.open_api_request(ids[1].clone(), draft("DELETE"), cx));
            assert_eq!(api_tabs(inspector), 1);
            let waiting = &inspector.session_workspaces[&Some(ids[1].clone())];
            assert!(waiting.visible);
            let active = waiting.active.unwrap();
            assert!(
                waiting
                    .tabs
                    .iter()
                    .any(|tab| tab.id == active && tab.surface == WorkspaceSurface::Api)
            );
            // Unknown Sessions and invalid drafts are dropped.
            assert!(!inspector.open_api_request(SessionId("gone".into()), draft("GET"), cx));
            assert!(!inspector.open_api_request(ids[0].clone(), draft("TRACE"), cx));
        });
    }

    #[gpui::test]
    fn saved_pane_context_is_window_local_and_empty_does_not_fall_back(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let ids: Vec<_> = fixture
            .list
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let first = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio.clone(), cx));
        let second = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio.clone(), cx));
        first.update(cx, |inspector, cx| {
            inspector.set_session_context(Some(Some(ids[1].clone())), cx);
            assert_eq!(inspector.selected_context().unwrap().id, ids[1]);
            assert_eq!(inspector.selected_session().unwrap().id, ids[1]);
            inspector.add_workspace(WorkspaceSurface::Browser, cx);
        });
        second.read_with(cx, |inspector, _| {
            assert_eq!(inspector.selected_session().unwrap().id, ids[0])
        });
        assert_eq!(
            runtime.store.read().unwrap().selected_session_id(),
            Some(&ids[0])
        );
        first.update(cx, |inspector, cx| {
            inspector.set_session_context(Some(None), cx);
            assert!(inspector.selected_context().is_none());
            assert!(inspector.selected_session().is_none());
            inspector.set_session_context(Some(Some(ids[1].clone())), cx);
            assert_eq!(
                inspector.workspace_selected,
                Some(WorkspaceSurface::Browser)
            );
            inspector.set_session_context(None, cx);
            assert_eq!(inspector.selected_session().unwrap().id, ids[0]);
        });
    }

    #[gpui::test]
    fn workspace_tabs_follow_session_selection_even_when_hidden(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        fixture.list.sessions[0].cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let ids: Vec<_> = fixture.list.sessions.iter().map(|s| s.id.clone()).collect();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        let first = inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            i.add_workspace(WorkspaceSurface::Files, cx);
            i.add_workspace(WorkspaceSurface::Files, cx);
            i.add_workspace(WorkspaceSurface::Browser, cx);
            i.browser_query.insert("https://example.com/session-a");
            i.workspace_active.unwrap()
        });
        let files = inspector.read_with(cx, |i, _| {
            i.workspace_tabs
                .iter()
                .filter_map(|t| t.viewer.clone())
                .collect::<Vec<_>>()
        });
        files[0].update(cx, |viewer, cx| viewer.seed_explorer_preview(cx));
        runtime.store.write().unwrap().select(ids[1].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(
                i.workspace_selected,
                Some(WorkspaceSurface::Details),
                "session B must start with its own sidebar"
            );
            assert!(i.workspace_tabs.iter().all(|tab| tab.id != first));
            i.add_workspace(WorkspaceSurface::Browser, cx);
            assert!(i.browser_query.is_empty());
            assert_ne!(i.workspace_active, Some(first));
            i.set_visible(true, cx);
        });
        runtime.store.write().unwrap().select(ids[0].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(i.workspace_active, Some(first));
            assert_eq!(i.browser_query.text(), "https://example.com/session-a");
            assert!(!i.visible, "A retains its own collapsed state");
            assert_eq!(
                files[0].read(cx).tab_label().as_deref(),
                Some("code_intelligence.rs"),
                "B must not clear A's open file"
            );
            assert_eq!(
                i.workspace_tabs
                    .iter()
                    .filter_map(|t| t.viewer.clone())
                    .collect::<Vec<_>>(),
                files
            );
        });
        runtime
            .store
            .write()
            .unwrap()
            .remove_session_record(&ids[1]);
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert!(
                !i.session_workspaces.contains_key(&Some(ids[1].clone())),
                "closing B releases its hidden tabs"
            );
        });
    }

    #[gpui::test]
    fn archiving_a_session_releases_its_hidden_workspace(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let sessions = fixture.list.sessions.clone();
        let ids: Vec<_> = sessions.iter().map(|s| s.id.clone()).collect();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            i.add_workspace(WorkspaceSurface::Browser, cx);
        });
        runtime.store.write().unwrap().select(ids[1].clone());
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert!(i.session_workspaces.contains_key(&Some(ids[0].clone())));
        });
        let mut archived = sessions[0].clone();
        archived.archived_at = Some(DateMillis(1.0));
        runtime.store.write().unwrap().upsert_session(archived);
        inspector.update(cx, |i, cx| {
            i.sync_workspace_session(cx);
            assert!(
                !i.session_workspaces.contains_key(&Some(ids[0].clone())),
                "an archived session keeps no hidden tabs"
            );
        });
        // Unarchived and reselected, it starts from the default workspace.
        let mut restored = sessions[0].clone();
        restored.archived_at = None;
        {
            let mut store = runtime.store.write().unwrap();
            store.upsert_session(restored);
            store.select(ids[0].clone());
        }
        inspector.update(cx, |i, cx| {
            i.refresh_if_context_changed(cx);
            assert_eq!(i.workspace_session, Some(ids[0].clone()));
            assert_eq!(i.workspace_tabs.len(), 1);
            assert_eq!(i.workspace_tabs[0].surface, WorkspaceSurface::Details);
        });
    }

    #[gpui::test]
    fn active_tab_and_close_control_remain_visible_in_narrow_sidebar(cx: &mut TestAppContext) {
        let (inspector, cx) = cx.add_window_view(|_, cx| {
            let runtime = Arc::new(StoreRuntime::inert());
            let tokio = Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            );
            let mut inspector = WorkbenchInspector::new(runtime, tokio, cx);
            for _ in 0..4 {
                inspector.add_workspace(WorkspaceSurface::Browser, cx);
            }
            inspector.browser_state.title =
                Some("A very long page title that must leave room for closing the tab".into());
            inspector
        });
        cx.simulate_resize(gpui::size(px(300.0), px(500.0)));
        cx.run_until_parked();
        cx.refresh().unwrap();
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            let handle = &i.workspace_tab_scroll;
            let index = i
                .workspace_tabs
                .iter()
                .position(|tab| Some(tab.id) == i.workspace_active)
                .unwrap();
            let tab = handle.bounds_for_item(index).unwrap();
            let viewport = handle.bounds();
            assert!(
                tab.right() + handle.offset().x <= viewport.right() + px(1.0),
                "active tab must be fully visible: tab={tab:?}, viewport={viewport:?}, offset={:?}",
                handle.offset()
            );
        });
        let tab = cx.debug_bounds("workspace-tab-5").unwrap();
        let close = cx.debug_bounds("close-workspace-5").unwrap();
        assert!(
            close.right() <= tab.right(),
            "long titles must not push the close button outside the tab"
        );
    }

    #[gpui::test]
    fn browser_address_shortcuts_edit_navigate_and_create_tabs(cx: &mut TestAppContext) {
        struct BrowserHarness {
            inspector: Entity<WorkbenchInspector>,
            navigations: Vec<String>,
        }
        impl Render for BrowserHarness {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if this
                            .inspector
                            .update(cx, |i, cx| i.browser_shortcut(event, window, cx))
                        {
                            cx.stop_propagation();
                        }
                    }))
                    .child(self.inspector.clone())
            }
        }
        let (harness, cx) = cx.add_window_view(|window, cx| {
            let runtime = Arc::new(StoreRuntime::inert());
            let tokio = Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            );
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            cx.subscribe(&inspector, |this: &mut BrowserHarness, _, event, _| {
                if let InspectorEvent::Browser(BrowserAction::Navigate(url)) = event {
                    this.navigations.push(url.clone());
                }
            })
            .detach();
            inspector.update(cx, |i, cx| {
                i.set_visible(true, cx);
                i.add_workspace(WorkspaceSurface::Browser, cx);
                i.browser_state.url = Some("https://example.com/original".into());
                i.browser_query.insert("https://example.com/original");
                window.focus(&i.focus, cx);
            });
            BrowserHarness {
                inspector,
                navigations: Vec::new(),
            }
        });
        cx.simulate_resize(gpui::size(px(600.0), px(500.0)));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-l");
        cx.simulate_keystrokes("x enter");
        cx.run_until_parked();
        harness.read_with(cx, |h, _| assert_eq!(h.navigations, ["https://x"]));
        let inspector = harness.read_with(cx, |h, _| h.inspector.clone());
        cx.simulate_keystrokes("cmd-l");
        cx.simulate_keystrokes("z escape");
        inspector.read_with(cx, |i, _| {
            assert_eq!(i.browser_query.text(), "https://example.com/original")
        });
        let original = inspector.read_with(cx, |i, _| i.workspace_active.unwrap());
        cx.simulate_keystrokes("cmd-t");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| {
            assert_ne!(i.workspace_active, Some(original));
            assert!(i.browser_query.is_empty());
            assert!(i.browser_address_focused);
        });
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| assert_eq!(i.workspace_active, Some(original)));
        // Clicking an already selected tab must leave it open.
        let tab = cx.debug_bounds("workspace-tab-2").unwrap();
        cx.simulate_click(tab.center(), Modifiers::default());
        cx.run_until_parked();
        inspector.read_with(cx, |i, _| assert_eq!(i.workspace_active, Some(original)));
    }

    #[gpui::test]
    fn add_menu_creates_independent_same_kind_tabs(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (inspector, cx) =
            cx.add_window_view(move |_, cx| WorkbenchInspector::new(runtime, tokio, cx));
        cx.simulate_resize(gpui::size(px(600.0), px(500.0)));
        for _ in 0..2 {
            cx.run_until_parked();
            let add = cx.debug_bounds("workspace-add-surface").unwrap();
            cx.simulate_click(add.center(), Modifiers::default());
            cx.run_until_parked();
            let files = cx.debug_bounds("workspace-catalog-Files").unwrap();
            cx.simulate_click(files.center(), Modifiers::default());
        }
        inspector.update(cx, |inspector, cx| {
            let files: Vec<_> = inspector
                .workspace_tabs
                .iter()
                .filter(|tab| tab.surface == WorkspaceSurface::Files)
                .collect();
            assert_eq!(files.len(), 2, "plus creates another Files tab");
            let first = files[0].id;
            let second = files[1].id;
            assert_ne!(
                files[0].viewer, files[1].viewer,
                "file history and tree state are per tab"
            );
            let viewer = files[0].viewer.clone().unwrap();
            inspector.activate_workspace(first, cx);
            assert_eq!(inspector.code_viewer, viewer);
            inspector.close_workspace(second, cx);
            assert_eq!(
                inspector.workspace_active,
                Some(first),
                "closing inactive sibling preserves selection"
            );
            for surface in [
                WorkspaceSurface::Browser,
                WorkspaceSurface::Terminal,
                WorkspaceSurface::Review,
                WorkspaceSurface::Details,
            ] {
                let before = inspector.workspace_tabs.len();
                inspector.add_workspace(surface, cx);
                let id = inspector.workspace_active.unwrap();
                inspector.add_workspace(surface, cx);
                assert_eq!(inspector.workspace_tabs.len(), before + 2);
                assert_ne!(inspector.workspace_active, Some(id));
            }
            let slots: Vec<_> = inspector
                .workspace_tabs
                .iter()
                .filter_map(|tab| tab.terminal_slot)
                .collect();
            assert_eq!(slots, [0, 1]);
            inspector.select_workspace(WorkspaceSurface::Files, cx);
            inspector.select_tab(InspectorTab::Artifacts, cx);
            let details = inspector.workspace_active.unwrap();
            inspector.add_workspace(WorkspaceSurface::Details, cx);
            assert_eq!(inspector.selected_tab, InspectorTab::Info);
            inspector.activate_workspace(details, cx);
            assert_eq!(inspector.selected_tab, InspectorTab::Artifacts);
        });
    }

    #[gpui::test]
    fn light_theme_reaches_the_code_tab(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|preferences| {
                preferences.terminal_theme = "dirijor-light".to_owned();
                preferences.inspector_tab = InspectorTab::Code;
            })
            .expect("inert preferences update");
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(inspector_runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let code_viewer = harness.read_with(cx, |harness, cx| {
            harness
                .inspector
                .read_with(cx, |inspector, _| inspector.code_viewer.clone())
        });

        assert_eq!(
            code_viewer.read_with(cx, |viewer, _| viewer.appearance()),
            diri_ui::Appearance::Light
        );
    }

    #[gpui::test]
    fn code_tab_tracks_live_light_theme_changes(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(inspector_runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        let code_viewer = inspector.read_with(cx, |inspector, _| inspector.code_viewer.clone());
        assert_eq!(
            code_viewer.read_with(cx, |viewer, _| viewer.appearance()),
            diri_ui::Appearance::Dark
        );

        runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|preferences| {
                preferences.terminal_theme = "dirijor-light".to_owned();
            })
            .expect("inert preferences update");
        runtime.publish_local_change();
        cx.run_until_parked();

        assert_eq!(
            code_viewer.read_with(cx, |viewer, _| viewer.appearance()),
            diri_ui::Appearance::Light
        );
    }

    #[test]
    fn light_review_rows_keep_readable_contrast() {
        for theme in ["dirijor-light", "solarized-light", "github-light"] {
            let colors = crate::app_theme::sidebar_colors(theme);
            let palette = DiffPalette::new(colors, &crate::app_theme::terminal_theme(theme));
            let inspector_surface = composite(colors.sidebar_surface(), colors.background);

            for (name, wash, ink) in [
                ("added", palette.added_line, palette.text),
                ("removed", palette.removed_line, palette.text),
                ("added word", palette.added_word, palette.text),
                ("removed word", palette.removed_word, palette.text),
                ("hunk", palette.hunk, palette.secondary),
                ("file", palette.file, palette.text),
                ("context", rgba(0x00000000), palette.text),
                ("selection", palette.selection, palette.text),
            ] {
                let row_surface = composite(wash, inspector_surface);
                let text = composite(ink, row_surface);
                assert!(
                    contrast(text, row_surface) >= 4.5,
                    "{name} contrast must remain readable with {theme}"
                );
            }
        }
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

    /// The Info tab renders the Git summary, so it must be refreshed when it
    /// becomes visible and whenever the selected session changes — but it must
    /// never install the periodic diff poll, which stays exclusive to Changes.
    #[gpui::test]
    fn info_refreshes_on_context_change_without_a_periodic_poll(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let ids: Vec<SessionId> = fixture
            .list
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        assert!(ids.len() >= 2, "fixture must offer two sessions to switch");
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(ids[0].clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(inspector_runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());

        // Shipping defaults: the inspector opens visible on Info.
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.selected_tab),
            InspectorTab::Info
        );
        inspector.update(cx, |inspector, cx| inspector.set_visible(true, cx));
        // Drain the Info refresh before asserts/teardown so no background Git
        // work outlives the deterministic GPUI test scheduler.
        cx.run_until_parked();

        let (generation, context, polling) = inspector.read_with(cx, |inspector, _| {
            (
                inspector.generation,
                inspector.context.clone(),
                inspector.poll_task.is_some(),
            )
        });
        assert!(
            generation > 0,
            "becoming visible on Info must read Git once"
        );
        assert_eq!(context.map(|context| context.id), Some(ids[0].clone()));
        assert!(!polling, "Info must not install a periodic diff poll");

        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.select(ids[1].clone());
        }
        inspector.update(cx, |inspector, cx| inspector.refresh_if_context_changed(cx));
        cx.run_until_parked();

        let (next_generation, next_context, still_polling) = inspector.read_with(cx, |i, _| {
            (i.generation, i.context.clone(), i.poll_task.is_some())
        });
        assert!(
            next_generation > generation,
            "a session change on Info must refresh instead of stranding stale counts"
        );
        assert_eq!(next_context.map(|context| context.id), Some(ids[1].clone()));
        assert!(!still_polling, "Info must still hold no periodic poll");

        // Contrast: Changes owns the timer, and leaving it disposes of it.
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx);
        });
        assert!(inspector.read_with(cx, |inspector, _| inspector.poll_task.is_some()));
        let transcript_generation =
            inspector.read_with(cx, |inspector, _| inspector.transcript_generation);
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Info, cx);
        });
        assert!(inspector.read_with(cx, |inspector, _| inspector.poll_task.is_none()));
        assert!(inspector.read_with(cx, |inspector, _| {
            inspector.transcript_generation > transcript_generation
        }));
        // Cancel any leftover refresh/review tasks on this thread before the
        // TestAppContext tears the window down.
        inspector.update(cx, |inspector, _| {
            inspector.refresh_task = None;
            inspector.review_task = None;
            inspector.transcript_task = None;
            inspector.poll_task = None;
        });
        cx.run_until_parked();
    }

    /// The commit composer shares `run_review_action` with staging, unstaging,
    /// and discarding. Only a commit that actually landed may consume the
    /// draft message; every other outcome leaves it for the user.
    #[gpui::test]
    fn commit_draft_is_cleared_only_by_a_successful_commit(cx: &mut TestAppContext) {
        fn git(root: &std::path::Path, arguments: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(arguments)
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let repository = tempfile::tempdir().expect("temporary repository");
        let root = repository.path();
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.name", "diri tests"]);
        git(root, &["config", "user.email", "diri@example.invalid"]);
        std::fs::write(root.join("one.txt"), "one\n").unwrap();
        std::fs::write(root.join("two.txt"), "two\n").unwrap();
        git(root, &["add", "one.txt", "two.txt"]);

        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        fixture.list.sessions[0].cwd = root.to_string_lossy().into_owned();
        fixture.list.sessions[0].host = None;
        let id = fixture.list.sessions[0].id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(id);
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |inspector, cx| inspector.set_visible(true, cx));
        cx.run_until_parked();

        inspector.update(cx, |inspector, cx| {
            inspector.commit_open = true;
            inspector.commit_query.insert("Add the first file");
            inspector.run_review_action(ReviewAction::Unstage(vec![PathBuf::from("two.txt")]), cx);
        });
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.review_feedback.as_ref().map(|(ok, _)| *ok),
                Some(true)
            );
            assert!(
                inspector.commit_open,
                "unstaging must not close the composer"
            );
            assert_eq!(inspector.commit_query.text(), "Add the first file");
        });

        inspector.update(cx, |inspector, cx| inspector.submit_commit(cx));
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.review_feedback.as_ref().map(|(ok, _)| *ok),
                Some(true)
            );
            assert!(!inspector.commit_open);
            assert!(inspector.commit_query.is_empty());
        });

        // Nothing is staged any more, so this commit fails and keeps its draft.
        inspector.update(cx, |inspector, cx| {
            inspector.commit_open = true;
            inspector.commit_query.insert("Add the second file");
            inspector.submit_commit(cx);
        });
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(
                inspector.review_feedback.as_ref().map(|(ok, _)| *ok),
                Some(false)
            );
            assert!(inspector.commit_open);
            assert_eq!(inspector.commit_query.text(), "Add the second file");
        });

        // Cancel any leftover refresh/review tasks on this thread before the
        // TestAppContext tears the entity down.
        inspector.update(cx, |inspector, _| {
            inspector.refresh_task = None;
            inspector.review_task = None;
            inspector.transcript_task = None;
            inspector.poll_task = None;
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn claude_transcript_turns_are_selectable_quotes_in_the_info_surface(cx: &mut TestAppContext) {
        let transcript_home = tempfile::tempdir().expect("transcript home");
        let agent_id = "77777777-7777-4777-8777-777777777777";
        let transcript_path = transcript_home
            .path()
            .join(".claude/projects/-tmp-project")
            .join(format!("{agent_id}.jsonl"));
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        let mut transcript = std::fs::File::create(&transcript_path).expect("transcript");
        writeln!(
            transcript,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"The parser drops UTF-8"}}]}}}}"#
        )
        .unwrap();

        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture
            .selected_session_id
            .clone()
            .expect("selected session");
        let session = fixture
            .list
            .sessions
            .iter_mut()
            .find(|session| session.id == selected)
            .expect("selected record");
        session.kind = ProtoAgentKind::CLAUDE_CODE;
        session.foreground_agent = None;
        session.agent_session_id = Some(agent_id.to_owned());
        session.transcript_path = Some(transcript_path.to_string_lossy().into_owned());
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(selected.clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let inspector_runtime = Arc::clone(&runtime);
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(inspector_runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            // Exercise the production transcript refresh and Info renderer in
            // isolation. `set_visible` also starts unrelated Git workers,
            // which makes this deterministic GPUI test depend on Tokio's
            // real blocking pool.
            let context = inspector.selected_context().expect("selected context");
            inspector.visible = true;
            inspector.context = Some(context.clone());
            inspector.transcript_home = transcript_home.path().to_path_buf();
            inspector.refresh_transcript(&context, false, cx);
            cx.notify();
        });
        cx.run_until_parked();
        let transcript_state = inspector.read_with(cx, |inspector, _| {
            format!("{:?}", inspector.transcript_state)
        });
        assert!(
            transcript_state.starts_with("Ready"),
            "transcript did not load: {transcript_state}"
        );

        let turn = cx
            .debug_bounds("INSPECTOR_TRANSCRIPT_TURN_0")
            .expect("assistant transcript turn");
        cx.simulate_click(turn.center(), Modifiers::none());
        let quote = inspector
            .read_with(cx, |inspector, _| inspector.quote_selection())
            .expect("selected transcript quote");
        assert_eq!(quote.content, "The parser drops UTF-8");
        assert_eq!(
            quote.source,
            QuoteSource::Transcript {
                session_id: selected,
                turn: "Claude turn near line 1".to_owned(),
            }
        );

        writeln!(
            transcript,
            r#"{{"type":"assistant","message":{{"content":"The appended turn is visible"}}}}"#
        )
        .unwrap();
        transcript.flush().unwrap();
        inspector.update(cx, |inspector, cx| inspector.refresh_if_context_changed(cx));
        cx.executor().advance_clock(TRANSCRIPT_REFRESH_DEBOUNCE);
        cx.run_until_parked();
        let appended_turns = inspector.read_with(cx, |inspector, _| {
            let TranscriptLoadState::Ready(document) = &inspector.transcript_state else {
                panic!("transcript not ready after same-session append");
            };
            document.turns.clone()
        });
        assert_eq!(appended_turns.len(), 2);
        assert_eq!(appended_turns[1].text, "The appended turn is visible");

        // Off Info, a same-session store change arms no transcript read.
        inspector.update(cx, |inspector, cx| {
            inspector.selected_tab = InspectorTab::Artifacts;
            let generation = inspector.transcript_generation;
            inspector.refresh_if_context_changed(cx);
            assert_eq!(inspector.transcript_generation, generation);
            inspector.selected_tab = InspectorTab::Info;
        });

        // Hiding releases the loaded documents; the next load is a full
        // read rather than a version no-op against the dropped snapshot.
        inspector.update(cx, |inspector, cx| {
            inspector.set_visible(false, cx);
            assert!(matches!(inspector.state, LoadState::NoSession));
            assert!(matches!(
                inspector.transcript_state,
                TranscriptLoadState::Unavailable
            ));
            assert!(inspector.transcript_version.is_none());
            assert!(inspector.markdown_cache.is_empty());
            let context = inspector.selected_context().expect("selected context");
            inspector.visible = true;
            inspector.refresh_transcript(&context, false, cx);
        });
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            let TranscriptLoadState::Ready(document) = &inspector.transcript_state else {
                panic!("transcript did not reload after the panel was shown again");
            };
            assert_eq!(document.turns.len(), 2);
        });

        inspector.update(cx, |inspector, _| {
            inspector.refresh_task = None;
            inspector.review_task = None;
            inspector.transcript_task = None;
            inspector.poll_task = None;
        });
        cx.run_until_parked();
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
        let artifacts = cx
            .debug_bounds("INSPECTOR_TAB_ARTIFACTS")
            .expect("Artifacts tab");
        let close = cx.debug_bounds("INSPECTOR_TOGGLE").expect("panel toggle");
        assert!(info.right() <= artifacts.left());
        assert!(artifacts.right() <= px(300.0));
        assert!(close.right() <= px(300.0));
        assert!(cx.debug_bounds("INSPECTOR_TAB_CHANGES").is_none());
        assert!(cx.debug_bounds("INSPECTOR_TAB_CODE").is_none());
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx)
        });
        assert_eq!(
            inspector.read_with(cx, |inspector, _| inspector.workspace_selected),
            Some(WorkspaceSurface::Review)
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

        inspector.update(cx, |inspector, cx| {
            inspector.select_workspace(WorkspaceSurface::Details, cx)
        });
        cx.run_until_parked();
        let artifacts = cx
            .debug_bounds("INSPECTOR_TAB_ARTIFACTS")
            .expect("Artifacts tab restored");
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
        let markdown = cx
            .debug_bounds("INSPECTOR_PR_BODY")
            .expect("pull request Markdown body");
        cx.simulate_click(markdown.center(), Modifiers::none());
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        let quote = inspector
            .read_with(cx, |inspector, _| inspector.quote_selection())
            .expect("selected Markdown quote");
        assert!(matches!(quote.source, QuoteSource::Markdown { .. }));
        assert!(!quote.content.trim().is_empty());
        let ask = cx.debug_bounds("INSPECTOR_PR_ASK").expect("PR ask action");
        cx.simulate_click(ask.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_ASK_COMPOSER").is_some());
        assert!(cx.debug_bounds("INSPECTOR_ASK_SEND").is_some());
    }

    /// The file-count notice is the only way to see what the bounded preview
    /// left out, so it opens in place into the omitted names — as rows of the
    /// same virtualized list, never one element per omitted file.
    #[gpui::test]
    fn omitted_untracked_notice_expands_into_a_virtualized_name_list(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.list.sessions[0].id.clone();
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(selected);
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx)
        });
        cx.run_until_parked();
        // The fixture session has no checkout; stand in for a loaded snapshot.
        inspector.update(cx, |inspector, cx| {
            inspector.state = LoadState::Ready(Arc::new(omitted_untracked_preview()));
            cx.notify();
        });
        cx.run_until_parked();

        let notice = cx
            .debug_bounds("INSPECTOR_OMITTED_UNTRACKED_NOTICE")
            .expect("omission notice");
        assert!(
            cx.debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_0")
                .is_none()
        );
        cx.simulate_click(notice.center(), Modifiers::none());
        cx.run_until_parked();

        assert!(inspector.read_with(cx, |inspector, _| inspector.omitted_untracked_open));
        let first = cx
            .debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_0")
            .expect("first omitted name");
        assert!(first.top() >= notice.bottom());
        assert!(
            cx.debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_4999")
                .is_none(),
            "names outside the viewport must not be built"
        );

        let notice = cx
            .debug_bounds("INSPECTOR_OMITTED_UNTRACKED_NOTICE")
            .expect("omission notice");
        cx.simulate_click(notice.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("INSPECTOR_OMITTED_UNTRACKED_PATH_0")
                .is_none()
        );
    }

    struct WideHarness {
        inspector: Entity<WorkbenchInspector>,
    }

    impl Render for WideHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(900.0))
                .h_full()
                .overflow_hidden()
                .child(self.inspector.clone())
        }
    }

    fn git_in(root: &std::path::Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn test_tokio() -> Arc<tokio::runtime::Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        )
    }

    /// Commits lists the branch's own commits over its base, and picking one
    /// shows that commit's diff in the same viewer, quotable like any other.
    #[gpui::test]
    fn commits_view_lists_branch_history_and_opens_a_commit_diff(cx: &mut TestAppContext) {
        let repository = tempfile::tempdir().expect("temporary repository");
        let root = repository.path();
        git_in(root, &["init", "--quiet"]);
        git_in(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_in(root, &["config", "user.name", "diri tests"]);
        git_in(root, &["config", "user.email", "diri@example.invalid"]);
        std::fs::write(root.join("app.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        git_in(root, &["add", "app.rs"]);
        git_in(root, &["commit", "--quiet", "-m", "Base"]);
        git_in(root, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(root.join("app.rs"), "fn main() {\n    let x = 2;\n}\n").unwrap();
        git_in(root, &["commit", "--quiet", "-am", "Change x"]);

        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        fixture.list.sessions[0].cwd = root.to_string_lossy().into_owned();
        fixture.list.sessions[0].host = None;
        let id = fixture.list.sessions[0].id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(id);
        }
        let tokio = test_tokio();
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.set_visible(true, cx);
            inspector.select_tab(InspectorTab::Changes, cx);
        });
        cx.run_until_parked();

        let commits = cx
            .debug_bounds("INSPECTOR_REVIEW_MODE_COMMITS")
            .expect("Commits segment");
        cx.simulate_click(commits.center(), Modifiers::none());
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(inspector.review_ui.mode, ReviewMode::Commits);
            let loaded = inspector
                .review_ui
                .loaded_history()
                .expect("history loaded");
            let subjects: Vec<&str> = loaded
                .history
                .commits
                .iter()
                .map(|commit| commit.subject.as_str())
                .collect();
            assert_eq!(subjects, ["Change x", "Base"]);
            assert_eq!(loaded.history.ahead, 1);
        });

        let first = cx
            .debug_bounds("INSPECTOR_COMMIT_0")
            .expect("first commit row");
        cx.simulate_click(first.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_COMMIT_STRIP").is_some());
        let snapshot = inspector
            .read_with(cx, |inspector, _| inspector.displayed_diff().cloned())
            .expect("commit diff loaded");
        assert_eq!(snapshot.files, 1);
        assert_eq!((snapshot.additions, snapshot.deletions), (1, 1));
        let removed = snapshot
            .rows
            .iter()
            .position(|row| row.text == "    let x = 1;")
            .expect("removed line");
        assert!(
            !snapshot.words_for(removed).is_empty(),
            "the changed literal is marked"
        );

        inspector.update(cx, |inspector, cx| {
            inspector.diff_selection.select(&snapshot, removed, false);
            cx.notify();
        });
        let quote = inspector
            .read_with(cx, |inspector, _| inspector.quote_selection())
            .expect("commit diff quote");
        assert!(quote.content.contains("let x = 1;"));

        // Back to Changes: the commit's rows no longer feed the selection.
        let changes = cx
            .debug_bounds("INSPECTOR_REVIEW_MODE_CHANGES")
            .expect("Changes segment");
        cx.simulate_click(changes.center(), Modifiers::none());
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(inspector.review_ui.mode, ReviewMode::Changes);
            assert!(inspector.diff_selection.is_empty());
        });

        inspector.update(cx, |inspector, _| {
            inspector.refresh_task = None;
            inspector.review_task = None;
            inspector.transcript_task = None;
            inspector.poll_task = None;
            inspector.review_ui.reset_for_context();
        });
        cx.run_until_parked();
    }

    /// Split appears only when the panel has room for two columns, and file
    /// headers collapse their bodies without touching the snapshot.
    #[gpui::test]
    fn wide_review_offers_split_layout_and_collapses_files(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let selected = fixture.list.sessions[0].id.clone();
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(selected);
        }
        let tokio = test_tokio();
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            WideHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx)
        });
        cx.run_until_parked();
        let snapshot = Arc::new(crate::diff::parse_unified_diff(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n keep\n-let x = 1;\n+let x = 2;\n",
        ));
        inspector.update(cx, |inspector, cx| {
            inspector.state = LoadState::Ready(Arc::clone(&snapshot));
            cx.notify();
        });
        cx.run_until_parked();

        let split = cx
            .debug_bounds("INSPECTOR_DIFF_SPLIT")
            .expect("a 900px panel offers split");
        cx.simulate_click(split.center(), Modifiers::none());
        cx.run_until_parked();
        inspector.update(cx, |inspector, _| {
            assert_eq!(inspector.review_ui.effective_layout(), DiffLayout::Split);
            let rows = inspector.review_ui.rows_for(&snapshot, false).rows;
            assert!(rows.contains(&crate::git_ui::ViewRow::Pair {
                left: Some(3),
                right: Some(4)
            }));
        });

        inspector.update(cx, |inspector, cx| {
            let snapshot = Arc::clone(&snapshot);
            inspector
                .review_ui
                .toggle_collapsed(snapshot.file_diffs[0].path.clone());
            cx.notify();
        });
        inspector.update(cx, |inspector, _| {
            let rows = inspector.review_ui.rows_for(&snapshot, false).rows;
            assert_eq!(rows.as_slice(), [crate::git_ui::ViewRow::Row(0)]);
        });
    }

    /// A narrow panel never shows split, even when it was chosen.
    #[gpui::test]
    fn narrow_review_keeps_the_inline_layout(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = test_tokio();
        let (harness, cx) = cx.add_window_view(move |_window, cx| {
            let inspector = cx.new(|cx| WorkbenchInspector::new(runtime, tokio, cx));
            InspectorHarness { inspector }
        });
        let inspector = harness.read_with(cx, |harness, _| harness.inspector.clone());
        inspector.update(cx, |inspector, cx| {
            inspector.select_tab(InspectorTab::Changes, cx);
            inspector.review_ui.layout = DiffLayout::Split;
            inspector.state = LoadState::Ready(Arc::new(crate::diff::parse_unified_diff(
                "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n",
            )));
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("INSPECTOR_DIFF_SPLIT").is_none());
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(inspector.review_ui.effective_layout(), DiffLayout::Inline);
        });
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

    /// The panel follows an Agent into another worktree of the same
    /// repository: Review/Details/Files read that checkout, a pin can send it
    /// back, and ⌘T still starts in the launch checkout.
    #[gpui::test]
    fn the_panel_follows_the_agent_into_another_worktree(cx: &mut TestAppContext) {
        fn git(root: &std::path::Path, arguments: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(arguments)
                .env("GIT_AUTHOR_NAME", "diri tests")
                .env("GIT_AUTHOR_EMAIL", "diri@example.invalid")
                .env("GIT_COMMITTER_NAME", "diri tests")
                .env("GIT_COMMITTER_EMAIL", "diri@example.invalid")
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let temp = tempfile::tempdir().expect("temporary directory");
        let main = temp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        std::fs::write(main.join("one.txt"), "one\n").unwrap();
        git(&main, &["add", "one.txt"]);
        git(&main, &["commit", "--quiet", "-m", "first"]);
        let feature = temp.path().join("main-feature");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                feature.to_str().unwrap(),
            ],
        );
        let feature = std::fs::canonicalize(&feature).unwrap();

        let runtime = Arc::new(StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let session = &mut fixture.list.sessions[0];
        // A shell reads its directory from the record; an Agent would also
        // sample its process, which the inert test client cannot answer.
        session.kind = ProtoAgentKind::SHELL;
        session.foreground_agent = None;
        session.terminal_cwd = None;
        session.cwd = main.to_string_lossy().into_owned();
        session.host = None;
        session.agent_workspace = Some(diri_proto::AgentWorkspace {
            cwd: None,
            edits: vec![diri_proto::AgentPlace {
                path: feature.join("two.txt").to_string_lossy().into_owned(),
                at: diri_proto::DateMillis(4_000_000_000_000.0),
            }],
        });
        let id = session.id.clone();
        {
            let mut store = runtime.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(id.clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let inspector = cx.new(|cx| WorkbenchInspector::new(runtime.clone(), tokio, cx));
        inspector.update(cx, |inspector, cx| inspector.set_visible(true, cx));
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();

        inspector.read_with(cx, |inspector, _| {
            let context = inspector.selected_context().expect("context");
            assert_eq!(context.cwd, feature);
            assert_eq!(context.launch_cwd, main);
            assert_eq!(
                inspector.context.as_ref().map(|c| c.cwd.clone()),
                Some(feature.clone())
            );
        });
        let followed = runtime.store.read().unwrap().fresh_worktree_repo(Some(&id));
        assert_eq!(followed, None, "fresh worktrees stay opt-in");
        // ⌘T belongs to the project: following the Agent into a worktree
        // must not move new Sessions there.
        let spawn_cwd = runtime
            .store
            .read()
            .unwrap()
            .spawn_params(
                ProtoAgentKind::CLAUDE_CODE,
                crate::store::SpawnOptions::default(),
            )
            .cwd;
        assert_eq!(spawn_cwd, main.to_string_lossy());

        // Pinning the launch checkout sends the panel back.
        let root = inspector.read_with(cx, |inspector, _| {
            inspector
                .follow
                .state
                .resolution(&id)
                .and_then(|resolution| resolution.candidates.iter().find(|c| c.launch))
                .map(|candidate| candidate.root.clone())
                .expect("launch candidate")
        });
        inspector.update(cx, |inspector, cx| {
            inspector.follow.state.set_pin(&id, Some(root));
            crate::workspace_follow::FollowHost::follow_changed(inspector, cx);
        });
        cx.run_until_parked();
        inspector.read_with(cx, |inspector, _| {
            assert_eq!(inspector.selected_context().unwrap().cwd, main);
        });
        inspector.update(cx, |inspector, _| {
            inspector.refresh_task = None;
            inspector.review_task = None;
            inspector.transcript_task = None;
            inspector.poll_task = None;
        });
        cx.run_until_parked();
    }
}
