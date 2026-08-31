//! Compact new-session destination opened in the main pane by Command-N.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use diri_proto::{AgentKind, Project, SessionId};
use diri_ui::{
    AgentKind as UiAgentKind, AgentLogo, Fill, FloatingSurface, Ink, Palette, Radius,
    SemanticColors,
};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyDownEvent, MouseButton, PathPromptOptions, Render, Role, ScrollHandle, Task, Window, div,
    prelude::*, px, rgba,
};

use crate::AppServices;
use crate::agent_catalog::{AgentOption, quick_agent_options, title_case_id};
use crate::composer::PromptComposer;
use crate::delegation::HandoffProposal;
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::launch_recipe::{
    LaunchRecipe, RecipeBookError, RecipeIssue, RecipeProject, WorktreePolicy,
    suggested_recipe_name,
};
use crate::navigation::CARET;
use crate::notifications::SendTextCommand;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};

const PANEL_WIDTH: f32 = 540.0;
const TITLE_HEIGHT: f32 = 36.0;
const TITLE_GAP: f32 = 22.0;
const CONTROL_SIZE: f32 = 32.0;
const CONTROL_RADIUS: f32 = 9.0;
const SHELF_HEIGHT: f32 = 40.0;
const PICKER_HEIGHT: f32 = 200.0;
const RECIPE_PICKER_MIN_HEIGHT: f32 = 156.0;
const RECIPE_PICKER_MAX_HEIGHT: f32 = 320.0;
const RECIPE_PICKER_GAP: f32 = 8.0;
const PANEL_EDGE_INSET: f32 = 12.0;
const RECIPE_ROW_GROUP: &str = "recipe-row";

/// Composer metrics. The text area is sized from the wrapped line count
/// rather than pinned at one height: a one-line prompt should not sit in a
/// half-empty box, and a twenty-line one should not vanish out of the bottom
/// of a fixed one — it grows to [`COMPOSER_MAX_LINES`] and then scrolls,
/// following the caret.
const COMPOSER_FONT_SIZE: f32 = 13.0;
const COMPOSER_LINE_HEIGHT: f32 = 19.0;
const COMPOSER_MIN_LINES: usize = 3;
const COMPOSER_MAX_LINES: usize = 9;
const COMPOSER_INSET: f32 = 8.0;
const COMPOSER_PADDING: f32 = 16.0;
const COMPOSER_PAD_TOP: f32 = 12.0;
const COMPOSER_PAD_BOTTOM: f32 = 6.0;
const COMPOSER_CONTROLS_HEIGHT: f32 = 44.0;

/// The width text actually wraps at, derived from the panel so the two cannot
/// drift apart: the panel, less the composer's margin, padding and border.
const COMPOSER_TEXT_WIDTH: f32 = PANEL_WIDTH - 2.0 * COMPOSER_INSET - 2.0 * COMPOSER_PADDING - 2.0;

fn recipe_picker_height(viewport_height: f32, composer_height: f32) -> f32 {
    let base_height = TITLE_HEIGHT + TITLE_GAP + composer_height + SHELF_HEIGHT;
    (viewport_height - base_height - RECIPE_PICKER_GAP - 2.0 * PANEL_EDGE_INSET)
        .clamp(RECIPE_PICKER_MIN_HEIGHT, RECIPE_PICKER_MAX_HEIGHT)
}

fn recipe_surface_height(recipe_count: usize, editor_open: bool, budget: f32) -> f32 {
    if editor_open {
        budget.min(206.0)
    } else {
        let desired = ((recipe_count as f32 * 54.0) + 54.0).max(128.0);
        if desired <= budget {
            desired
        } else {
            // Rows are 54pt. Quantizing the crowded viewport keeps both edges
            // intentional instead of exposing a chopped half-row after
            // keyboard navigation scrolls to the end.
            let visible_rows = ((budget - 12.0) / 54.0).floor().max(2.0);
            12.0 + visible_rows * 54.0
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LauncherSurfaceFills {
    composer: gpui::Rgba,
    shelf: gpui::Rgba,
}

fn launcher_colors_for_theme(theme_id: &str) -> SemanticColors {
    crate::app_theme::colors(theme_id)
}

fn launcher_surface_fills(colors: SemanticColors) -> LauncherSurfaceFills {
    LauncherSurfaceFills {
        composer: colors.floating_surface(),
        shelf: colors.sidebar_surface(),
    }
}

const fn composer_text_height(lines: usize) -> f32 {
    let visible = if lines < COMPOSER_MIN_LINES {
        COMPOSER_MIN_LINES
    } else if lines > COMPOSER_MAX_LINES {
        COMPOSER_MAX_LINES
    } else {
        lines
    };
    visible as f32 * COMPOSER_LINE_HEIGHT + COMPOSER_PAD_TOP + COMPOSER_PAD_BOTTOM
}

#[derive(Clone)]
struct LauncherProject {
    project: Project,
    host: Option<String>,
}

pub(crate) enum LauncherEvent {
    Closed,
    ManageAgents(Option<String>),
}

pub(crate) struct LauncherOverlay {
    services: Arc<AppServices>,
    focus: FocusHandle,
    prompt: PromptComposer,
    target: LauncherTarget,
    new_session_draft: String,
    session_drafts: HashMap<SessionId, String>,
    mode: LauncherMode,
    /// The active destination draft survives a temporary handoff proposal.
    saved_new_prompt: Option<String>,
    handoff_delivery: HandoffDeliveryState,
    /// Drafts containing paths validated on this Mac cannot be submitted to a
    /// remote Agent. Pure text/quotes do not carry this restriction.
    session_drafts_with_local_paths: HashSet<SessionId>,
    selected_harness: AgentKind,
    selected_root: String,
    selected_host: Option<String>,
    selected_worktree: WorktreePolicy,
    selected_title: Option<String>,
    draft_recipe_name: Option<String>,
    /// Set while a saved workflow is being previewed. Editing launcher fields
    /// remains a one-off override until the explicit “Update recipe” action.
    active_recipe: Option<LaunchRecipe>,
    /// The saved project identity remains authoritative during repair until
    /// the user explicitly chooses another project or folder.
    recipe_project_edited: bool,
    recipe_editor: Option<RecipeMetadataEditor>,
    pending_recipe_delete: Option<String>,
    /// A recipe click made before its host catalog arrives is still one
    /// launch action. Readiness changes retry this identity automatically;
    /// closing the launcher or choosing another recipe cancels it.
    pending_recipe_activation: Option<String>,
    fallback_notice: Option<String>,
    /// Finder drops may partially succeed. Keep their ignored-path detail
    /// inline with the staged draft until the user sends or replaces it; a
    /// toast would separate the reason from its action.
    drop_notice: Option<String>,
    /// Which picker, if any, is open — and where its keyboard highlight sits,
    /// so both are reachable without the mouse.
    picker: Option<Picker>,
    highlight: usize,
    recipe_scroll: ScrollHandle,
    open: bool,
    preview: bool,
    _store_changes: Task<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Picker {
    Harness,
    Project,
    Recipe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecipeMetadataField {
    Name,
    Title,
    Branch,
}

impl RecipeMetadataField {
    const fn adjacent(self, backwards: bool) -> Self {
        match (self, backwards) {
            (Self::Name, false) | (Self::Branch, true) => Self::Title,
            (Self::Title, false) => Self::Branch,
            (Self::Title, true) => Self::Name,
            (Self::Branch, false) | (Self::Name, true) => Self::Name,
        }
    }
}

#[derive(Clone, Debug)]
struct RecipeMetadataEditor {
    /// `Some` edits the persisted recipe; `None` edits only the current launch
    /// draft, including one-off overrides from a saved recipe.
    id: Option<String>,
    name: QueryEditor,
    title: QueryEditor,
    branch: QueryEditor,
    active_field: RecipeMetadataField,
    error: Option<String>,
}

impl RecipeMetadataEditor {
    fn saved(recipe: &LaunchRecipe) -> Self {
        Self {
            id: Some(recipe.id.clone()),
            name: text_editor(&recipe.name),
            title: text_editor(recipe.title.as_deref().unwrap_or_default()),
            branch: text_editor(match &recipe.worktree {
                WorktreePolicy::Fresh { branch } => branch.as_deref().unwrap_or_default(),
                WorktreePolicy::CurrentCheckout => "",
            }),
            active_field: RecipeMetadataField::Name,
            error: None,
        }
    }

    fn draft(name: &str, title: Option<&str>, worktree: &WorktreePolicy) -> Self {
        Self {
            id: None,
            name: text_editor(name),
            title: text_editor(title.unwrap_or_default()),
            branch: text_editor(match worktree {
                WorktreePolicy::Fresh { branch } => branch.as_deref().unwrap_or_default(),
                WorktreePolicy::CurrentCheckout => "",
            }),
            active_field: RecipeMetadataField::Name,
            error: None,
        }
    }

    fn field_mut(&mut self) -> &mut QueryEditor {
        match self.active_field {
            RecipeMetadataField::Name => &mut self.name,
            RecipeMetadataField::Title => &mut self.title,
            RecipeMetadataField::Branch => &mut self.branch,
        }
    }

    fn field(&self, field: RecipeMetadataField) -> &QueryEditor {
        match field {
            RecipeMetadataField::Name => &self.name,
            RecipeMetadataField::Title => &self.title,
            RecipeMetadataField::Branch => &self.branch,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LauncherTarget {
    NewSession,
    Session(SessionId),
}

#[derive(Clone, Debug)]
enum LauncherMode {
    NewSession,
    Handoff(HandoffProposal),
}

/// One acknowledged handoff at a time. Tickets prevent a late completion
/// from an old proposal from closing or annotating a newer composer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HandoffDeliveryState {
    next_ticket: u64,
    pending: Option<u64>,
}

impl HandoffDeliveryState {
    fn begin(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.pending = Some(self.next_ticket);
        self.pending
    }

    fn settle(&mut self, ticket: u64) -> bool {
        if self.pending != Some(ticket) {
            return false;
        }
        self.pending = None;
        true
    }

    fn invalidate(&mut self) {
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.pending = None;
    }

    const fn is_sending(self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectCommit {
    Recent(usize),
    ChooseFolder,
}

impl EventEmitter<LauncherEvent> for LauncherOverlay {}

impl LauncherOverlay {
    pub(crate) fn new(services: Arc<AppServices>, preview: bool, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let (selected_harness, selected_root, selected_host) = initial_target(&services);
        let mut changes = services.store.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this
                            .update(cx, |this, cx| {
                                this.resume_pending_recipe_activation(cx);
                                if this.open
                                    && matches!(this.target, LauncherTarget::NewSession)
                                    && matches!(this.mode, LauncherMode::NewSession)
                                    && this.active_recipe.is_none()
                                {
                                    this.reconcile_harness();
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Self {
            services,
            focus,
            prompt: PromptComposer::default(),
            target: LauncherTarget::NewSession,
            new_session_draft: String::new(),
            session_drafts: HashMap::new(),
            mode: LauncherMode::NewSession,
            saved_new_prompt: None,
            handoff_delivery: HandoffDeliveryState::default(),
            session_drafts_with_local_paths: HashSet::new(),
            selected_harness,
            selected_root,
            selected_host,
            selected_worktree: WorktreePolicy::CurrentCheckout,
            selected_title: None,
            draft_recipe_name: None,
            active_recipe: None,
            recipe_project_edited: false,
            recipe_editor: None,
            pending_recipe_delete: None,
            pending_recipe_activation: None,
            fallback_notice: None,
            drop_notice: None,
            picker: None,
            highlight: 0,
            recipe_scroll: ScrollHandle::new(),
            open: false,
            preview,
            _store_changes: store_changes,
        }
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_new_prompt();
        self.switch_target(LauncherTarget::NewSession);
        self.drop_notice = None;
        // A half-written prompt survives Escape. This used to clear on every
        // open, so closing the launcher by reflex — or bouncing off it to
        // check something — threw the prompt away with no way back. It is
        // cleared on submit, and only there.
        if self.prompt.is_empty() {
            let (harness, root, host) = initial_target(&self.services);
            self.selected_harness = harness;
            self.selected_root = root;
            self.selected_host = host;
            self.selected_worktree = WorktreePolicy::CurrentCheckout;
            self.selected_title = None;
            self.draft_recipe_name = None;
            self.active_recipe = None;
            self.recipe_project_edited = false;
            self.fallback_notice = None;
        }
        self.activate_new_session(window, cx);
    }

    /// Command-N owns a reversible main-pane destination: invoking it again
    /// returns to the session that was already underneath the launcher.
    /// Returns whether the launcher is open after the transition so RootView
    /// only schedules focus for the branch it is about to mount.
    pub(crate) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.open {
            self.close(cx);
            false
        } else {
            self.open(window, cx);
            true
        }
    }

    /// Open Command-N at a validated local directory. The Finder gesture only
    /// prepares the form: the existing draft remains, focus lands in its
    /// prompt, and no spawn occurs before explicit submission.
    pub(crate) fn open_at_directory(
        &mut self,
        root: String,
        notice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_new_prompt();
        self.switch_target(LauncherTarget::NewSession);
        self.selected_root = root;
        self.selected_host = None;
        self.selected_worktree = WorktreePolicy::CurrentCheckout;
        self.selected_title = None;
        self.draft_recipe_name = None;
        self.active_recipe = None;
        self.recipe_project_edited = false;
        self.fallback_notice = None;
        self.drop_notice = notice;
        self.activate_new_session(window, cx);
    }

    /// Open the native composer for one existing session and append staged
    /// context to that session's identity-keyed local draft. Merely opening
    /// this surface never writes to the PTY, selects the session, or wakes a
    /// hibernated process.
    pub(crate) fn open_for_session(
        &mut self,
        session_id: SessionId,
        insertion: &str,
        notice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_new_prompt();
        self.switch_target(LauncherTarget::Session(session_id.clone()));
        self.prompt.append_context(insertion);
        self.drop_notice = notice;
        self.picker = None;
        self.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Finder paths are meaningful only on this Mac. Keep that provenance
    /// attached to the identity-keyed draft so a later target transition
    /// cannot accidentally make it submittable to a remote host.
    pub(crate) fn open_local_paths_for_session(
        &mut self,
        session_id: SessionId,
        insertion: &str,
        notice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session_drafts_with_local_paths
            .insert(session_id.clone());
        self.open_for_session(session_id, insertion, notice, window, cx);
    }

    fn activate_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .request_agent_catalog(self.selected_host.clone(), false);
        self.reconcile_harness();
        self.picker = None;
        self.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn switch_target(&mut self, target: LauncherTarget) {
        if self.target == target {
            return;
        }
        let saved = transition_draft(
            &self.target,
            &target,
            self.prompt.text(),
            &mut self.new_session_draft,
            &mut self.session_drafts,
        );
        self.prompt.clear();
        if !saved.is_empty() {
            self.prompt.insert_multiline(&saved);
        }
        self.target = target;
        self.picker = None;
    }

    /// Opens an identity-targeted review surface. Merely opening it cannot
    /// write to either session; the only send path is the labelled confirmation
    /// control rendered by `render_handoff_panel`.
    pub(crate) fn open_handoff(
        &mut self,
        proposal: HandoffProposal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_new_prompt();
        self.saved_new_prompt = Some(self.prompt.text().to_owned());
        self.prompt.clear();
        self.prompt.insert_multiline(&proposal.summary);
        self.mode = LauncherMode::Handoff(proposal);
        self.handoff_delivery.invalidate();
        self.fallback_notice = None;
        self.picker = None;
        self.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.picker = None;
        self.pending_recipe_activation = None;
        self.restore_new_prompt();
        cx.emit(LauncherEvent::Closed);
        cx.notify();
    }

    /// Close from outside the launcher (sidebar session click, menu bar, etc.).
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.close(cx);
    }

    fn restore_new_prompt(&mut self) {
        if !matches!(self.mode, LauncherMode::Handoff(_)) {
            return;
        }
        self.handoff_delivery.invalidate();
        self.prompt.clear();
        if let Some(prompt) = self.saved_new_prompt.take()
            && !prompt.is_empty()
        {
            self.prompt.insert_multiline(&prompt);
        }
        self.mode = LauncherMode::NewSession;
    }

    fn harness_choices(&self) -> Vec<AgentOption> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        quick_agent_options(store.agent_catalog(self.selected_host.as_deref()))
    }

    /// Keeps a saved default preference intact while making this invocation
    /// usable on a target where that Agent is absent. It judges by installed
    /// state, not quick-create visibility, so a hidden-but-installed default is
    /// not switched away from.
    ///
    /// A target with no readiness facts yet is *unknown*, not empty, so the
    /// selection is left alone: rewriting it to Terminal would silently discard
    /// the user's Agent for the length of an SSH scan, and — because Terminal
    /// is always spawnable — this would never run again to put it back.
    /// `blocker` holds ⌘↵ until the scan answers instead.
    fn reconcile_harness(&mut self) {
        let (spawnable, catalog_known) = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let catalog = store.agent_catalog(self.selected_host.as_deref());
            (
                crate::agent_catalog::kind_spawnable(&self.selected_harness, catalog),
                catalog.is_some(),
            )
        };
        if spawnable || !catalog_known || self.active_recipe.is_some() {
            return;
        }
        let choices = self.harness_choices();
        let Some(first) = choices.first() else {
            return;
        };
        let unavailable = title_case_id(self.selected_harness.id());
        self.selected_harness = first.kind.clone();
        self.fallback_notice = Some(format!(
            "{unavailable} is unavailable here; using {}",
            first.display_name
        ));
    }

    fn projects(&self) -> Vec<LauncherProject> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        let mut projects: Vec<_> = store
            .projects()
            .values()
            .cloned()
            .map(|project| LauncherProject {
                // The project record is the authority for which machine owns
                // the root; sessions are a fallback for records persisted by
                // daemons that predate the host field. Without it, a remote
                // project whose sessions were all closed would spawn locally
                // with the remote path as cwd.
                host: project.host.clone().or_else(|| {
                    store
                        .sessions()
                        .values()
                        .find(|session| session.project_id == project.id)
                        .and_then(|session| session.host.clone())
                }),
                project,
            })
            .collect();
        projects.sort_by(|left, right| {
            left.project
                .pinned_order
                .unwrap_or(i64::MAX)
                .cmp(&right.project.pinned_order.unwrap_or(i64::MAX))
                .then_with(|| {
                    left.project
                        .name
                        .to_lowercase()
                        .cmp(&right.project.name.to_lowercase())
                })
        });
        projects
    }

    fn recipes(&self) -> Vec<LaunchRecipe> {
        self.services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .launch_recipes
            .items()
            .to_vec()
    }

    /// Opening the library warms every distinct valid destination in one pass.
    /// A remote recipe should not need a sacrificial first click merely to
    /// discover whether its Agent is installed.
    fn warm_recipe_catalogs(&self) {
        let targets = self
            .recipes()
            .into_iter()
            .map(|recipe| recipe.host)
            .collect::<HashSet<_>>();
        let mut store = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned");
        let targets = targets
            .into_iter()
            .filter(|target| {
                target.is_none()
                    || target
                        .as_deref()
                        .is_some_and(|host| store.host(host).is_some())
            })
            .filter(|target| store.agent_catalog(target.as_deref()).is_none())
            .collect::<Vec<_>>();
        for target in targets {
            store.request_agent_catalog(target, false);
        }
    }

    fn host_label(&self, host: Option<&str>) -> String {
        match host {
            None => "This Mac".to_owned(),
            Some(host) => self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .host_display_name(host),
        }
    }

    fn recipe_render_facts(&self, recipes: &[LaunchRecipe]) -> Vec<(Option<RecipeIssue>, String)> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        recipes
            .iter()
            .map(|recipe| {
                let issue = recipe
                    .validate(
                        store.projects(),
                        store.hosts(),
                        store.agent_catalog(recipe.host.as_deref()),
                        |project| {
                            project.host.clone().or_else(|| {
                                store
                                    .sessions()
                                    .values()
                                    .find(|session| session.project_id == project.id)
                                    .and_then(|session| session.host.clone())
                            })
                        },
                    )
                    .err();
                let destination = recipe.host.as_deref().map_or_else(
                    || "This Mac".to_owned(),
                    |host| store.host_display_name(host),
                );
                (issue, destination)
            })
            .collect()
    }

    fn current_recipe(&self, name: String) -> LaunchRecipe {
        let projects = self.projects();
        let project = recipe_project_for_draft(
            self.active_recipe.as_ref(),
            self.recipe_project_edited,
            &self.selected_root,
            self.selected_host.as_deref(),
            &projects,
        );
        let mut recipe = LaunchRecipe::draft(
            name,
            self.selected_harness.clone(),
            project,
            self.selected_host.clone(),
            self.prompt.text(),
        );
        recipe.worktree = self.selected_worktree.clone();
        recipe.title.clone_from(&self.selected_title);
        recipe
    }

    fn resolve_recipe(
        &self,
        recipe: &LaunchRecipe,
    ) -> Result<crate::launch_recipe::ResolvedRecipe, RecipeIssue> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        recipe.resolve(
            store.projects(),
            store.hosts(),
            store.agent_catalog(recipe.host.as_deref()),
            |project| {
                project.host.clone().or_else(|| {
                    store
                        .sessions()
                        .values()
                        .find(|session| session.project_id == project.id)
                        .and_then(|session| session.host.clone())
                })
            },
        )
    }

    fn validate_recipe(&self, recipe: &LaunchRecipe) -> Result<(), RecipeIssue> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        recipe.validate(
            store.projects(),
            store.hosts(),
            store.agent_catalog(recipe.host.as_deref()),
            |project| {
                project.host.clone().or_else(|| {
                    store
                        .sessions()
                        .values()
                        .find(|session| session.project_id == project.id)
                        .and_then(|session| session.host.clone())
                })
            },
        )
    }

    /// Available recipes are one-click launches. A stale recipe unfolds into
    /// the ordinary launcher with its fields preserved and one specific repair
    /// message, so no missing dependency can silently retarget a run.
    fn activate_recipe(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // A second recipe choice supersedes a cold launch that was still
        // waiting for readiness. There must never be two delayed launches.
        self.pending_recipe_activation = None;
        let Some(recipe) = self.recipes().into_iter().find(|recipe| recipe.id == id) else {
            self.fallback_notice = Some("This recipe no longer exists".to_owned());
            return;
        };
        match self.resolve_recipe(&recipe) {
            Ok(resolved) if !self.preview => {
                self.complete_recipe_activation(resolved, cx);
            }
            Err(RecipeIssue::AgentsLoading) if !self.preview => {
                self.preview_recipe(&recipe);
                self.pending_recipe_activation = Some(recipe.id.clone());
                let force = self
                    .services
                    .store
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .agent_catalog_error(recipe.host.as_deref())
                    .is_some();
                self.fallback_notice = Some(RecipeIssue::AgentsLoading.message());
                self.services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .request_agent_catalog(recipe.host.clone(), force);
                self.picker = None;
                window.focus(&self.focus, cx);
                cx.notify();
            }
            result => {
                self.preview_recipe(&recipe);
                self.fallback_notice = result.err().map(|issue| issue.message()).or_else(|| {
                    self.preview
                        .then(|| "Preview mode — no Agent will be launched".to_owned())
                });
                self.services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .request_agent_catalog(recipe.host.clone(), false);
                self.picker = None;
                window.focus(&self.focus, cx);
                cx.notify();
            }
        }
    }

    fn complete_recipe_activation(
        &mut self,
        resolved: crate::launch_recipe::ResolvedRecipe,
        cx: &mut Context<Self>,
    ) {
        self.pending_recipe_activation = None;
        self.services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .spawn_kind(resolved.kind, resolved.options);
        self.new_session_draft.clear();
        self.prompt.clear();
        self.close(cx);
    }

    /// Store readiness is asynchronous, but a recipe activation is not a
    /// two-click interaction. Keep retrying the exact saved identity until its
    /// catalog arrives, then launch it through the canonical resolver. Any
    /// terminal repair state leaves the populated launcher open.
    fn resume_pending_recipe_activation(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_recipe_activation.clone() else {
            return;
        };
        let Some(recipe) = self.recipes().into_iter().find(|recipe| recipe.id == id) else {
            self.pending_recipe_activation = None;
            self.fallback_notice = Some("This recipe no longer exists".to_owned());
            return;
        };
        match self.resolve_recipe(&recipe) {
            Ok(resolved) if !self.preview => self.complete_recipe_activation(resolved, cx),
            Err(RecipeIssue::AgentsLoading) => {
                let (error, still_loading) = {
                    let store = self
                        .services
                        .store
                        .store
                        .read()
                        .expect("session store lock poisoned");
                    (
                        store
                            .agent_catalog_error(recipe.host.as_deref())
                            .map(str::to_owned),
                        store.agent_catalog_is_loading(recipe.host.as_deref()),
                    )
                };
                if let Some(error) = error
                    && !still_loading
                {
                    self.pending_recipe_activation = None;
                    self.fallback_notice = Some(format!(
                        "Could not check Agents on this host: {error}. Choose the recipe again to retry."
                    ));
                }
            }
            Err(issue) => {
                self.pending_recipe_activation = None;
                self.fallback_notice = Some(issue.message());
            }
            Ok(_) => {
                // Preview launchers never enqueue activations, but avoid
                // retaining stale state if a test or future caller does.
                self.pending_recipe_activation = None;
            }
        }
    }

    fn preview_recipe(&mut self, recipe: &LaunchRecipe) {
        self.pending_recipe_activation = None;
        self.selected_harness.clone_from(&recipe.agent);
        self.selected_host.clone_from(&recipe.host);
        self.selected_worktree.clone_from(&recipe.worktree);
        self.selected_title.clone_from(&recipe.title);
        self.draft_recipe_name = Some(recipe.name.clone());
        self.selected_root = match &recipe.project {
            RecipeProject::Tracked {
                id,
                last_known_root,
            } => self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .projects()
                .get(id)
                .map_or_else(|| last_known_root.clone(), |project| project.root.clone()),
            RecipeProject::Path { path } => path.clone(),
        };
        self.prompt.clear();
        self.prompt.insert_multiline(&recipe.initial_prompt);
        self.active_recipe = Some(recipe.clone());
        self.recipe_project_edited = false;
    }

    fn update_recipe_book(
        &self,
        update: impl FnOnce(&mut crate::launch_recipe::LaunchRecipeBook) -> Result<(), RecipeBookError>,
    ) -> Result<(), String> {
        let mut book_result = None;
        let io_result = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| {
                book_result = Some(update(&mut prefs.launch_recipes));
            });
        io_result.map_err(|error| error.to_string()).and_then(|()| {
            book_result
                .expect("recipe update closure ran")
                .map_err(|e| e.to_string())
        })
    }

    /// Rebind callers to the book's normalized value, never to the input
    /// draft. The book owns trimming and length limits, so keeping the draft
    /// object would make the active baseline disagree with durable prefs.
    fn replace_recipe(&self, id: &str, recipe: LaunchRecipe) -> Result<LaunchRecipe, String> {
        let mut persisted = None;
        self.update_recipe_book(|book| {
            book.replace(id, recipe)?;
            persisted = book.get(id).cloned();
            Ok(())
        })?;
        persisted.ok_or_else(|| "updated recipe disappeared".to_owned())
    }

    fn save_current_recipe(&mut self, cx: &mut Context<Self>) {
        if self.prompt.text().trim().is_empty() || self.selected_root.is_empty() {
            self.fallback_notice = Some("Add a task and project before saving a recipe".to_owned());
            cx.notify();
            return;
        }
        let name = self
            .draft_recipe_name
            .clone()
            .unwrap_or_else(|| suggested_recipe_name(self.prompt.text()));
        let recipe = self.current_recipe(name.clone());
        if let Err(issue) = self.validate_recipe(&recipe) {
            self.fallback_notice = Some(issue.message());
            cx.notify();
            return;
        }
        let mut saved = None;
        let result = self.update_recipe_book(|book| {
            saved = Some(book.add(recipe)?.clone());
            Ok(())
        });
        if result.is_ok() {
            self.active_recipe = saved;
            self.draft_recipe_name = self
                .active_recipe
                .as_ref()
                .map(|recipe| recipe.name.clone());
            self.recipe_project_edited = false;
        }
        self.fallback_notice = Some(match result {
            Ok(()) => format!("Saved “{name}” — it is now a one-click recipe"),
            Err(error) => format!("Could not save recipe: {error}"),
        });
        self.picker = None;
        cx.notify();
    }

    fn update_active_recipe(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active_recipe.clone() else {
            self.save_current_recipe(cx);
            return;
        };
        let id = active.id;
        let name = self.draft_recipe_name.clone().unwrap_or(active.name);
        let recipe = self.current_recipe(name.clone());
        if let Err(issue) = self.validate_recipe(&recipe) {
            self.fallback_notice = Some(issue.message());
            cx.notify();
            return;
        }
        let result = self.replace_recipe(&id, recipe);
        if let Ok(persisted) = &result {
            self.draft_recipe_name = Some(persisted.name.clone());
            self.active_recipe = Some(persisted.clone());
            self.recipe_project_edited = false;
        }
        self.fallback_notice = Some(match result {
            Ok(persisted) => format!("Updated “{}”", persisted.name),
            Err(error) => format!("Could not update recipe: {error}"),
        });
        cx.notify();
    }

    fn remove_recipe(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = self.update_recipe_book(|book| book.remove(id).map(|_| ()));
        if delete_clears_active_recipe(result.is_ok(), self.active_recipe.as_ref(), id) {
            self.active_recipe = None;
            self.recipe_project_edited = false;
        }
        self.fallback_notice = Some(match result {
            Ok(()) => "Recipe deleted".to_owned(),
            Err(error) => format!("Could not delete recipe: {error}"),
        });
        let count = self.recipes().len().saturating_add(1);
        self.highlight = self.highlight.min(count.saturating_sub(1));
        cx.notify();
    }

    fn duplicate_recipe(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = self.update_recipe_book(|book| book.duplicate(id).map(|_| ()));
        if let Err(error) = result {
            self.fallback_notice = Some(format!("Could not duplicate recipe: {error}"));
        }
        cx.notify();
    }

    fn move_recipe(&mut self, id: &str, delta: isize, cx: &mut Context<Self>) {
        let result = self.update_recipe_book(|book| book.move_by(id, delta).map(|_| ()));
        if let Err(error) = result {
            self.fallback_notice = Some(format!("Could not reorder recipe: {error}"));
        }
        cx.notify();
    }

    fn edit_recipe(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_recipe_activation = None;
        let Some(recipe) = self.recipes().into_iter().find(|recipe| recipe.id == id) else {
            self.fallback_notice = Some("This recipe no longer exists".to_owned());
            cx.notify();
            return;
        };
        self.recipe_editor = Some(RecipeMetadataEditor::saved(&recipe));
        self.pending_recipe_delete = None;
        cx.notify();
    }

    fn edit_launch_details(&mut self, cx: &mut Context<Self>) {
        self.pending_recipe_activation = None;
        let name = self
            .draft_recipe_name
            .clone()
            .unwrap_or_else(|| suggested_recipe_name(self.prompt.text()));
        self.recipe_editor = Some(RecipeMetadataEditor::draft(
            &name,
            self.selected_title.as_deref(),
            &self.selected_worktree,
        ));
        self.pending_recipe_delete = None;
        self.picker = Some(Picker::Recipe);
        cx.notify();
    }

    fn save_recipe_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.recipe_editor.clone() else {
            return;
        };
        let name = editor.name.text().trim();
        if name.is_empty() {
            if let Some(editor) = &mut self.recipe_editor {
                editor.error = Some("Give the recipe a name".to_owned());
            }
            cx.notify();
            return;
        }
        if editor.id.is_none() {
            if self.selected_host.is_some() && !editor.branch.is_empty() {
                if let Some(editor) = &mut self.recipe_editor {
                    editor.error = Some("Remote launches cannot create local worktrees".to_owned());
                }
                cx.notify();
                return;
            }
            self.draft_recipe_name = Some(name.to_owned());
            self.selected_title = nonempty(editor.title.text());
            let branch = nonempty(editor.branch.text());
            self.selected_worktree = match (&self.selected_worktree, branch) {
                (_, Some(branch)) => WorktreePolicy::Fresh {
                    branch: Some(branch),
                },
                (WorktreePolicy::Fresh { .. }, None) => WorktreePolicy::Fresh { branch: None },
                (WorktreePolicy::CurrentCheckout, None) => WorktreePolicy::CurrentCheckout,
            };
            self.recipe_editor = None;
            self.picker = None;
            self.fallback_notice = Some(if self.active_recipe.is_some() {
                "Launch details changed for this run — the saved recipe is untouched".to_owned()
            } else {
                "Launch details updated".to_owned()
            });
            cx.notify();
            return;
        }
        let id = editor.id.expect("saved recipe editor has an id");
        let Some(mut recipe) = self.recipes().into_iter().find(|recipe| recipe.id == id) else {
            self.recipe_editor = None;
            self.fallback_notice = Some("This recipe no longer exists".to_owned());
            cx.notify();
            return;
        };
        let preserve_live_name_override = self
            .active_recipe
            .as_ref()
            .filter(|active| active.id == id)
            .zip(self.draft_recipe_name.as_ref())
            .is_some_and(|(active, draft)| draft != &active.name);
        recipe.name = name.to_owned();
        recipe.title = nonempty(editor.title.text());
        if let WorktreePolicy::Fresh { branch } = &mut recipe.worktree {
            *branch = nonempty(editor.branch.text());
        } else if let Some(branch) = nonempty(editor.branch.text()) {
            recipe.worktree = WorktreePolicy::Fresh {
                branch: Some(branch),
            };
        }
        if recipe.host.is_some() && matches!(recipe.worktree, WorktreePolicy::Fresh { .. }) {
            if let Some(editor) = &mut self.recipe_editor {
                editor.error = Some("Remote recipes cannot create local worktrees".to_owned());
            }
            cx.notify();
            return;
        }
        match self.replace_recipe(&id, recipe) {
            Ok(persisted) => {
                if self
                    .active_recipe
                    .as_ref()
                    .is_some_and(|active| active.id == id)
                {
                    // This editor mutates the saved baseline only. The live
                    // launcher may contain one-off prompt, Agent, project,
                    // title, or worktree overrides and must not be reloaded.
                    if !preserve_live_name_override {
                        self.draft_recipe_name = Some(persisted.name.clone());
                    }
                    self.active_recipe = Some(persisted.clone());
                }
                self.recipe_editor = None;
                self.fallback_notice = Some(format!("Updated “{}”", persisted.name));
            }
            Err(error) => {
                if let Some(editor) = &mut self.recipe_editor {
                    editor.error = Some(error);
                }
            }
        }
        cx.notify();
    }

    fn request_recipe_delete(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.pending_recipe_delete.as_deref() == Some(id) {
            self.pending_recipe_delete = None;
            self.remove_recipe(id, cx);
            return;
        }
        self.pending_recipe_delete = Some(id.to_owned());
        self.fallback_notice = Some("Press Delete again to remove this recipe".to_owned());
        cx.notify();
    }

    fn handle_recipe_editor_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(editor) = &mut self.recipe_editor else {
            return false;
        };
        match event.keystroke.key.as_str() {
            "escape" => {
                self.recipe_editor = None;
                cx.notify();
            }
            "tab" => {
                editor.active_field = editor
                    .active_field
                    .adjacent(event.keystroke.modifiers.shift);
                editor.error = None;
                cx.notify();
            }
            "enter" => self.save_recipe_editor(cx),
            _ => {
                let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                    return true;
                };
                match edit {
                    Edit::Local(local) => {
                        editor.field_mut().apply(local);
                    }
                    Edit::Clipboard(ClipboardEdit::Copy) => {
                        query_editor::copy_selection(editor.field_mut(), cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Cut) => {
                        query_editor::cut_selection(editor.field_mut(), cx);
                    }
                    Edit::Clipboard(ClipboardEdit::Paste) => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            editor.field_mut().insert(&text);
                        }
                    }
                }
                editor.error = None;
                cx.notify();
            }
        }
        true
    }

    fn selected_harness_label(&self) -> String {
        self.harness_choices()
            .into_iter()
            .find(|choice| choice.kind == self.selected_harness)
            .map(|choice| choice.display_name)
            .unwrap_or_else(|| title_case_id(self.selected_harness.id()))
    }

    fn selected_project_label(&self) -> String {
        let project = self
            .projects()
            .into_iter()
            .find(|project| {
                project.project.root == self.selected_root && project.host == self.selected_host
            })
            .map(|project| project.project.name)
            .or_else(|| {
                Path::new(&self.selected_root)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Choose project".to_owned());
        format!(
            "{project} · {}",
            self.host_label(self.selected_host.as_deref())
        )
    }

    /// Why the prompt cannot be sent yet, as something to show the user.
    /// `None` means it can. The submit button used to just sit there dimmed
    /// with no explanation, which reads as "broken" rather than "not yet".
    fn blocker(&self) -> Option<String> {
        if let LauncherMode::Handoff(proposal) = &self.mode {
            if self.handoff_delivery.is_sending() {
                return Some("Sending handoff…".to_owned());
            }
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let Some(target) = store.sessions().get(&proposal.target_id) else {
                return Some("The target session is no longer available".to_owned());
            };
            if target.is_archived() || matches!(target.status, diri_proto::SessionStatus::Exited(_))
            {
                return Some("The target session has ended".to_owned());
            }
            return None;
        }
        if let LauncherTarget::Session(id) = &self.target {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let Some(session) = store.sessions().get(id) else {
                return Some("This session is no longer available".to_owned());
            };
            return (session.host.is_some() && self.session_drafts_with_local_paths.contains(id))
                .then(|| "Local paths cannot be used on a remote session".to_owned());
        }
        let recipe = self.current_recipe("New Agent".to_owned());
        match self.validate_recipe(&recipe) {
            Ok(_) => None,
            Err(RecipeIssue::AgentsLoading) => {
                let scan_error = self
                    .services
                    .store
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .agent_catalog_error(self.selected_host.as_deref())
                    .map(str::to_owned);
                Some(scan_error.map_or_else(
                    || {
                        format!(
                            "Checking whether {} is available here…",
                            self.selected_harness_label()
                        )
                    },
                    |error| format!("Could not check Agents on this host: {error}"),
                ))
            }
            Err(issue) => Some(issue.message()),
        }
    }

    fn can_submit(&self) -> bool {
        !self.preview && !self.prompt.text().trim().is_empty() && self.blocker().is_none()
    }

    fn submit(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_submit() {
            return false;
        }
        if let Some(command) = handoff_command(&self.mode, self.prompt.text()) {
            let Some(ticket) = self.handoff_delivery.begin() else {
                return false;
            };
            self.fallback_notice = None;
            let client = Arc::clone(self.services.store.client());
            let runtime = Arc::clone(&self.services.tokio);
            cx.spawn(async move |this, cx| {
                let task = runtime.spawn(async move {
                    client.wait_until_connected(Duration::from_secs(5)).await?;
                    client
                        .send_text(&command.session_id, command.text, command.submit)
                        .await
                });
                let result = match task.await {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(error) => Err(format!("handoff task stopped: {error}")),
                };
                let _ = this.update(cx, |this, cx| {
                    if !this.handoff_delivery.settle(ticket) {
                        return;
                    }
                    match result {
                        Ok(()) => {
                            this.prompt.clear();
                            this.close(cx);
                        }
                        Err(error) => {
                            this.fallback_notice = Some(format!(
                                "The handoff was not sent: {error}. Review it and try again."
                            ));
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
            cx.notify();
            return true;
        }
        let prompt = self.prompt.text().trim().to_owned();
        match &self.target {
            LauncherTarget::NewSession => {
                let recipe = self.current_recipe("One-off launch".to_owned());
                let Ok(resolved) = self.resolve_recipe(&recipe) else {
                    return false;
                };
                self.services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .spawn_kind(resolved.kind, resolved.options);
                self.new_session_draft.clear();
                self.active_recipe = None;
                self.recipe_project_edited = false;
            }
            LauncherTarget::Session(id) => {
                // Selection can attach or resume terminal state, so it belongs
                // to explicit confirmation—not the Finder release that merely
                // opened this draft.
                self.services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .select(id.clone());
                let _ = self
                    .services
                    .store
                    .notification_action_sender()
                    .send(SendTextCommand {
                        session_id: id.clone(),
                        text: prompt,
                        submit: true,
                    });
                self.session_drafts.remove(id);
                self.session_drafts_with_local_paths.remove(id);
            }
        }
        self.prompt.clear();
        self.drop_notice = None;
        self.close(cx);
        true
    }

    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Submission is already explicit at this point. Freeze the editor
        // until the daemon acknowledges it so post-submit edits cannot be
        // mistaken for content that was delivered, and Escape cannot claim to
        // cancel bytes already in flight.
        if self.handoff_delivery.is_sending() {
            return true;
        }
        if self.handle_recipe_editor_key(event, cx) {
            return true;
        }
        if self.picker.is_some() && self.handle_picker_key(event, window, cx) {
            return true;
        }
        let shift = event.keystroke.modifiers.shift;
        match event.keystroke.key.as_str() {
            "escape" => {
                self.close(cx);
                true
            }
            "enter" if shift => {
                self.prompt.insert_multiline("\n");
                cx.notify();
                true
            }
            "enter" => self.submit(cx),
            "r" if event.keystroke.modifiers.platform
                && event.keystroke.modifiers.shift
                && matches!(self.target, LauncherTarget::NewSession) =>
            {
                self.edit_launch_details(cx);
                true
            }
            "r" if event.keystroke.modifiers.platform
                && matches!(self.target, LauncherTarget::NewSession) =>
            {
                self.toggle_picker(Picker::Recipe);
                cx.notify();
                true
            }
            // Cycling the agent from the keyboard: the picker was mouse-only,
            // which is a strange thing to require of a surface you reached
            // with ⌘N and are about to leave with ↵.
            "tab" if matches!(self.target, LauncherTarget::NewSession) => {
                self.cycle_harness(if shift { -1 } else { 1 });
                cx.notify();
                true
            }
            "up" => {
                self.prompt.move_up(shift);
                cx.notify();
                true
            }
            "down" => {
                self.prompt.move_down(shift);
                cx.notify();
                true
            }
            _ => self.edit_prompt(event, cx),
        }
    }

    /// Arrow keys drive the open picker instead of the prompt behind it.
    fn handle_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let count = match self.picker {
            Some(Picker::Harness) => self.harness_choices().len(),
            Some(Picker::Project) => self.projects().len() + 1,
            Some(Picker::Recipe) => self.recipes().len() + 1,
            None => return false,
        };
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if self.picker == Some(Picker::Recipe) {
            let recipes = self.recipes();
            let selected = recipes.get(self.highlight).map(|recipe| recipe.id.clone());
            match key {
                "space" => {
                    if let Some(recipe) = recipes.get(self.highlight) {
                        self.preview_recipe(recipe);
                        self.fallback_notice = Some(
                            "Previewing — changes apply once unless you update the recipe"
                                .to_owned(),
                        );
                        self.picker = None;
                        window.focus(&self.focus, cx);
                    }
                    cx.notify();
                    return true;
                }
                "e" => {
                    if let Some(id) = selected {
                        self.edit_recipe(&id, cx);
                    }
                    return true;
                }
                "d" if modifiers.platform => {
                    if let Some(id) = selected {
                        self.duplicate_recipe(&id, cx);
                    }
                    return true;
                }
                "up" | "down" if modifiers.platform => {
                    if let Some(id) = selected {
                        let delta = if key == "up" { -1 } else { 1 };
                        self.move_recipe(&id, delta, cx);
                        self.highlight = (self.highlight as isize + delta)
                            .clamp(0, recipes.len().saturating_sub(1) as isize)
                            as usize;
                        self.recipe_scroll.scroll_to_item(self.highlight);
                    }
                    return true;
                }
                "backspace" | "delete" => {
                    if let Some(id) = selected {
                        self.request_recipe_delete(&id, cx);
                    }
                    return true;
                }
                _ => {}
            }
        }
        match key {
            "escape" => {
                self.picker = None;
                cx.notify();
                true
            }
            "up" | "down" if count > 0 => {
                self.highlight = if event.keystroke.key == "up" {
                    self.highlight.saturating_sub(1)
                } else {
                    (self.highlight + 1).min(count - 1)
                };
                if self.picker == Some(Picker::Recipe) {
                    self.recipe_scroll.scroll_to_item(self.highlight);
                }
                cx.notify();
                true
            }
            "enter" => {
                self.commit_highlight(window, cx);
                cx.notify();
                true
            }
            _ => false,
        }
    }

    fn commit_highlight(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.picker {
            Some(Picker::Harness) => {
                if let Some(choice) = self.harness_choices().get(self.highlight) {
                    self.pending_recipe_activation = None;
                    self.selected_harness = choice.kind.clone();
                    self.fallback_notice = None;
                }
            }
            Some(Picker::Project) => {
                let projects = self.projects();
                match project_commit(projects.len(), self.highlight) {
                    ProjectCommit::Recent(index) => {
                        self.pending_recipe_activation = None;
                        self.selected_root.clone_from(&projects[index].project.root);
                        self.selected_host.clone_from(&projects[index].host);
                        self.recipe_project_edited = true;
                        self.services
                            .store
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .request_agent_catalog(projects[index].host.clone(), false);
                        if self.active_recipe.is_none() {
                            self.reconcile_harness();
                        }
                        self.picker = None;
                        window.focus(&self.focus, cx);
                    }
                    ProjectCommit::ChooseFolder => {
                        self.choose_folder(window, cx);
                    }
                }
                return;
            }
            Some(Picker::Recipe) => {
                let recipes = self.recipes();
                if let Some(recipe) = recipes.get(self.highlight) {
                    let id = recipe.id.clone();
                    self.activate_recipe(&id, window, cx);
                } else {
                    self.save_current_recipe(cx);
                }
                return;
            }
            None => return,
        }
        self.picker = None;
    }

    fn toggle_picker(&mut self, picker: Picker) {
        if self.picker == Some(picker) {
            self.picker = None;
            return;
        }
        if picker == Picker::Recipe {
            self.warm_recipe_catalogs();
            self.recipe_scroll = ScrollHandle::new();
        }
        self.highlight = match picker {
            Picker::Harness => self
                .harness_choices()
                .iter()
                .position(|choice| choice.kind == self.selected_harness),
            Picker::Project => self.projects().iter().position(|project| {
                project.project.root == self.selected_root && project.host == self.selected_host
            }),
            Picker::Recipe => self.active_recipe.as_ref().and_then(|active| {
                self.recipes()
                    .iter()
                    .position(|recipe| recipe.id == active.id)
            }),
        }
        .unwrap_or(0);
        self.picker = Some(picker);
    }

    /// Steps to the next installed agent, skipping any that cannot run.
    fn cycle_harness(&mut self, delta: isize) {
        let choices = self.harness_choices();
        if choices.is_empty() {
            return;
        }
        let current = choices
            .iter()
            .position(|choice| choice.kind == self.selected_harness)
            .unwrap_or(0);
        let count = choices.len() as isize;
        let next = (current as isize + delta).rem_euclid(count) as usize;
        self.pending_recipe_activation = None;
        self.selected_harness = choices[next].kind.clone();
        self.fallback_notice = None;
    }

    fn toggle_worktree(&mut self) {
        self.pending_recipe_activation = None;
        self.selected_worktree = match &self.selected_worktree {
            WorktreePolicy::Fresh { .. } => WorktreePolicy::CurrentCheckout,
            WorktreePolicy::CurrentCheckout if self.selected_host.is_none() => {
                WorktreePolicy::Fresh { branch: None }
            }
            WorktreePolicy::CurrentCheckout => return,
        };
        self.fallback_notice = None;
    }

    fn edit_prompt(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(edit) = query_editor::edit_for(&event.keystroke) else {
            return false;
        };
        match edit {
            Edit::Local(local) => {
                self.pending_recipe_activation = None;
                self.prompt.apply(local);
            }
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(self.prompt.editor(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Cut) => {
                self.pending_recipe_activation = None;
                query_editor::cut_selection(self.prompt.editor_mut(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Paste) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.pending_recipe_activation = None;
                    self.prompt.insert_multiline(&text);
                }
            }
        }
        if self.prompt.text().is_empty()
            && let LauncherTarget::Session(id) = &self.target
        {
            // A remote user can recover from a rejected Finder drop by
            // clearing the draft, without weakening provenance while any of
            // the local insertion remains.
            self.session_drafts_with_local_paths.remove(id);
        }
        cx.notify();
        true
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        close_picker_for_folder_choice(&mut self.picker);
        // The native sheet temporarily owns focus. Keep the composer focused
        // on both sides so a cancel or completion returns keyboard input to
        // the untouched draft.
        window.focus(&self.focus, cx);
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Start Here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if apply_folder_choice(&mut this.selected_root, selected.as_deref()) {
                    this.pending_recipe_activation = None;
                    this.selected_host = None;
                    this.recipe_project_edited = true;
                    this.services
                        .store
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .request_agent_catalog(None, false);
                    if this.active_recipe.is_none() {
                        this.reconcile_harness();
                    }
                }
                window.focus(&this.focus, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_harness_picker(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("launcher-harness-list")
            .py(px(4.0))
            .w(px(260.0))
            .max_h(px(PICKER_HEIGHT))
            .overflow_y_scroll();
        for (index, choice) in self.harness_choices().into_iter().enumerate() {
            let selected = choice.kind == self.selected_harness;
            let highlighted = self.highlight == index;
            let kind = choice.kind.clone();
            let logo = ui_agent_kind(&choice.kind);
            list = list.child(
                div()
                    .id(format!("launcher-harness-{index}"))
                    .mx(px(6.0))
                    .h(px(32.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(8.0))
                    .text_size(px(12.0))
                    .text_color(colors.primary)
                    .when(highlighted, |row| row.bg(colors.primary.alpha(0.08)))
                    .cursor_pointer()
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pending_recipe_activation = None;
                        this.selected_harness = kind.clone();
                        this.fallback_notice = None;
                        this.picker = None;
                        cx.notify();
                    }))
                    .child(AgentLogo::new(logo, 21.0, colors))
                    .child(div().flex_1().child(choice.display_name))
                    .when(selected, |row| {
                        row.child(sf_symbol_weighted(
                            "checkmark",
                            9.0,
                            SymbolWeight::Semibold,
                            colors.secondary,
                        ))
                    }),
            );
        }
        let host = self.selected_host.clone();
        list = list.child(
            div()
                .id("launcher-manage-agents")
                .mt(px(4.0))
                .mx(px(6.0))
                .pt(px(5.0))
                .h(px(34.0))
                .px(px(9.0))
                .border_t_1()
                .border_color(colors.primary.alpha(0.07))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open = false;
                    this.picker = None;
                    cx.emit(LauncherEvent::ManageAgents(host.clone()));
                    cx.notify();
                }))
                .child(sf_symbol("gearshape", 11.0, colors.secondary))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child("Manage Agents…"),
                ),
        );
        FloatingSurface::new(colors, list).into_any_element()
    }

    fn render_project_picker(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let projects = self.projects();
        let mut list = div()
            .id("launcher-project-list")
            .py(px(6.0))
            .w(px(310.0))
            .max_h(px(PICKER_HEIGHT))
            .overflow_y_scroll();
        for (index, project) in projects.into_iter().enumerate() {
            let selected =
                project.project.root == self.selected_root && project.host == self.selected_host;
            let highlighted = self.highlight == index;
            let root = project.project.root.clone();
            let host = project.host.clone();
            let destination = self.host_label(host.as_deref());
            let project_detail = format!("{}  ·  {destination}", project.project.root);
            list = list.child(
                div()
                    .id(format!("launcher-project-{index}"))
                    .mx(px(6.0))
                    .min_h(px(44.0))
                    .px(px(9.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .when(highlighted, |row| row.bg(colors.primary.alpha(0.08)))
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pending_recipe_activation = None;
                        this.selected_root.clone_from(&root);
                        this.selected_host.clone_from(&host);
                        this.recipe_project_edited = true;
                        this.services
                            .store
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .request_agent_catalog(host.clone(), false);
                        if this.active_recipe.is_none() {
                            this.reconcile_harness();
                        }
                        this.picker = None;
                        cx.notify();
                    }))
                    .child(sf_symbol("folder", 12.0, colors.secondary))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(colors.primary)
                                    .child(project.project.name),
                            )
                            .child(
                                div()
                                    .text_size(px(9.0))
                                    .text_color(colors.tertiary)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(project_detail),
                            ),
                    )
                    .when(selected, |row| {
                        row.child(sf_symbol_weighted(
                            "checkmark",
                            9.0,
                            SymbolWeight::Semibold,
                            colors.secondary,
                        ))
                    }),
            );
        }
        let choose_index = self.projects().len();
        let highlighted = self.highlight == choose_index;
        list = list.child(
            div()
                .id("launcher-project-choose-folder")
                .mx(px(6.0))
                .h(px(42.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .gap(px(9.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .when(highlighted, |row| row.bg(colors.primary.alpha(0.08)))
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.choose_folder(window, cx);
                }))
                .child(sf_symbol("folder.badge.plus", 12.0, colors.secondary))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(colors.primary)
                        .child("Choose Folder…"),
                ),
        );
        FloatingSurface::new(colors, list).into_any_element()
    }

    fn render_recipe_picker(
        &self,
        max_height: f32,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(editor) = &self.recipe_editor {
            return div()
                .id("launcher-recipe-editor-scroll")
                .h(px(max_height))
                .overflow_y_scroll()
                .child(self.render_recipe_editor(editor, colors, cx))
                .into_any_element();
        }
        let recipes = self.recipes();
        let facts = self.recipe_render_facts(&recipes);
        let mut list = div()
            .id("launcher-recipe-list")
            .debug_selector(|| "launcher-recipe-list".into())
            .py(px(6.0))
            .w(px(PANEL_WIDTH - 2.0 * COMPOSER_INSET))
            .h(px(max_height))
            .overflow_y_scroll()
            .track_scroll(&self.recipe_scroll);

        if recipes.is_empty() {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child("Make this task repeatable"),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .line_height(px(14.0))
                            .text_color(colors.secondary)
                            .child("Recipes remember the Agent, project, destination, and prompt."),
                    ),
            );
        }

        for (index, (recipe, (issue, destination))) in recipes.into_iter().zip(facts).enumerate() {
            let id = recipe.id.clone();
            let preview_id = id.clone();
            let edit_id = id.clone();
            let duplicate_id = id.clone();
            let up_id = id.clone();
            let down_id = id.clone();
            let delete_id = id.clone();
            let highlighted = self.highlight == index;
            let active = self
                .active_recipe
                .as_ref()
                .is_some_and(|active| active.id == recipe.id);
            let metadata = format!(
                "{}  ·  {}  ·  {}",
                title_case_id(recipe.agent.id()),
                destination,
                recipe.project.display_path()
            );
            let status = issue.as_ref().map(RecipeIssue::message);
            list = list.child(
                div()
                    .id(format!("launcher-recipe-{id}"))
                    .debug_selector({
                        let id = id.clone();
                        move || format!("launcher-recipe-{id}")
                    })
                    .mx(px(6.0))
                    .min_h(px(54.0))
                    .px(px(9.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .group(RECIPE_ROW_GROUP)
                    .rounded(px(Radius::ROW))
                    .role(Role::Button)
                    .aria_label(format!("Launch recipe {}", recipe.name))
                    .aria_description(
                        "Enter launches, Space previews, E edits, Command-D duplicates, Command-Up or Command-Down reorders, Delete removes",
                    )
                    .cursor_pointer()
                    .when(highlighted || active, |row| {
                        row.bg(colors.primary.alpha(if active { 0.10 } else { 0.07 }))
                    })
                    .hover(move |row| row.bg(colors.primary.alpha(0.07)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_recipe(&id, window, cx);
                    }))
                    .child(sf_symbol(
                        if issue.is_some() {
                            "exclamationmark.triangle"
                        } else {
                            "chevron.right"
                        },
                        11.0,
                        if issue.is_some() {
                            Ink::ATTENTION
                        } else {
                            Palette::CLAY
                        },
                    ))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary)
                                    .child(recipe.name),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(9.0))
                                    .text_color(if status.is_some() {
                                        Ink::ATTENTION
                                    } else {
                                        colors.tertiary
                                    })
                                    .child(status.unwrap_or(metadata)),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .when(!(highlighted || active), |actions| {
                                actions
                                    .invisible()
                                    .group_hover(RECIPE_ROW_GROUP, |style| style.visible())
                            })
                            .child(recipe_action(
                                format!("recipe-preview-{preview_id}"),
                                "cursorarrow.rays",
                                "Preview and override recipe",
                                colors,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(recipe) = this
                                        .recipes()
                                        .into_iter()
                                        .find(|recipe| recipe.id == preview_id)
                                    {
                                        this.preview_recipe(&recipe);
                                        this.fallback_notice = Some(
                                            "Previewing — changes apply once unless you update the recipe"
                                                .to_owned(),
                                        );
                                        this.picker = None;
                                        cx.notify();
                                    }
                                }),
                            ))
                            .child(recipe_action(
                                format!("recipe-edit-{edit_id}"),
                                "gearshape",
                                "Edit recipe details",
                                colors,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.edit_recipe(&edit_id, cx);
                                }),
                            ))
                            .child(recipe_action(
                                format!("recipe-duplicate-{duplicate_id}"),
                                "square.stack.3d.up",
                                "Duplicate recipe",
                                colors,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.duplicate_recipe(&duplicate_id, cx);
                                }),
                            ))
                            .child(recipe_action(
                                format!("recipe-up-{up_id}"),
                                "chevron.up",
                                "Move recipe up",
                                colors,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.move_recipe(&up_id, -1, cx);
                                }),
                            ))
                            .child(recipe_action(
                                format!("recipe-down-{down_id}"),
                                "chevron.down",
                                "Move recipe down",
                                colors,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.move_recipe(&down_id, 1, cx);
                                }),
                            ))
                            .child(recipe_action(
                                format!("recipe-delete-{delete_id}"),
                                "trash",
                                "Delete recipe",
                                colors,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.request_recipe_delete(&delete_id, cx);
                                }),
                            )),
                    ),
            );
        }

        let update = self.active_recipe.is_some();
        list = list.child(
            div()
                .id("launcher-save-recipe")
                .mt(px(4.0))
                .mx(px(6.0))
                .pt(px(5.0))
                .h(px(38.0))
                .px(px(9.0))
                .border_t_1()
                .border_color(colors.primary.alpha(0.07))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .role(Role::Button)
                .aria_label(if update {
                    "Update active recipe"
                } else {
                    "Save current fields as a recipe"
                })
                .cursor_pointer()
                .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_active_recipe(cx);
                }))
                .child(sf_symbol(
                    if update {
                        "arrow.triangle.2.circlepath"
                    } else {
                        "plus"
                    },
                    11.0,
                    colors.secondary,
                ))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child(if update {
                            "Update recipe with these fields"
                        } else {
                            "Save current fields as a recipe"
                        }),
                ),
        );
        FloatingSurface::new(colors, list).into_any_element()
    }

    fn render_recipe_editor(
        &self,
        editor: &RecipeMetadataEditor,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut form = div()
            .id("launcher-recipe-editor")
            .w(px(PANEL_WIDTH - 2.0 * COMPOSER_INSET))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.primary)
                            .child("Recipe details"),
                    )
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(colors.tertiary)
                            .child("Tab fields · Return saves · Esc cancels"),
                    ),
            )
            .child(self.recipe_text_field(
                "Name",
                "Review this PR",
                editor,
                RecipeMetadataField::Name,
                colors,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(div().min_w(px(0.0)).flex_1().child(self.recipe_text_field(
                        "Session title",
                        "Optional",
                        editor,
                        RecipeMetadataField::Title,
                        colors,
                        cx,
                    )))
                    .child(div().min_w(px(0.0)).flex_1().child(self.recipe_text_field(
                        "Branch prefix",
                        "Optional · unique suffix added",
                        editor,
                        RecipeMetadataField::Branch,
                        colors,
                        cx,
                    ))),
            );
        if let Some(error) = &editor.error {
            form = form.child(
                div()
                    .text_size(px(10.0))
                    .text_color(Ink::DANGER)
                    .child(error.clone()),
            );
        }
        form = form.child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(7.0))
                .child(
                    div()
                        .id("cancel-recipe-editor")
                        .h(px(28.0))
                        .px(px(9.0))
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .text_size(px(10.0))
                        .text_color(colors.secondary)
                        .flex()
                        .items_center()
                        .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.recipe_editor = None;
                            cx.notify();
                        }))
                        .child("Cancel"),
                )
                .child(
                    div()
                        .id("save-recipe-editor")
                        .h(px(28.0))
                        .px(px(10.0))
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .bg(colors.primary)
                        .text_size(px(10.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.background)
                        .flex()
                        .items_center()
                        .hover(move |button| button.opacity(0.86))
                        .on_click(cx.listener(|this, _, _, cx| this.save_recipe_editor(cx)))
                        .child("Save details"),
                ),
        );
        FloatingSurface::new(colors, form).into_any_element()
    }

    fn recipe_text_field(
        &self,
        label: &'static str,
        placeholder: &'static str,
        editor: &RecipeMetadataEditor,
        field: RecipeMetadataField,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = editor.active_field == field;
        let value = editor.field(field);
        let content = if active {
            crate::navigation::query_label(value)
        } else if value.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child(placeholder)
                .into_any_element()
        } else {
            div().child(value.text().to_owned()).into_any_element()
        };
        div()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(9.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.secondary)
                    .child(label),
            )
            .child(
                div()
                    .id(format!("recipe-field-{field:?}"))
                    .h(px(32.0))
                    .px(px(9.0))
                    .rounded(px(Radius::CHIP))
                    .border_1()
                    .border_color(colors.primary.alpha(if active { 0.28 } else { 0.10 }))
                    .bg(colors.primary.alpha(if active { 0.07 } else { 0.035 }))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .text_size(px(10.0))
                    .text_color(colors.primary)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(editor) = &mut this.recipe_editor {
                            editor.active_field = field;
                            editor.field_mut().set_cursor(usize::MAX, false);
                        }
                        cx.notify();
                    }))
                    .child(content),
            )
    }

    fn render_panel(
        &self,
        viewport_height: f32,
        colors: SemanticColors,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if matches!(self.mode, LauncherMode::Handoff(_)) {
            return self.render_handoff_panel(colors, focused, cx);
        }
        if matches!(self.target, LauncherTarget::Session(_)) {
            return self.render_session_panel(colors, focused, cx);
        }
        let can_submit = self.can_submit();
        let harness_open = self.picker == Some(Picker::Harness);
        let project_open = self.picker == Some(Picker::Project);
        let recipe_open = self.picker == Some(Picker::Recipe);
        let recipe_count = self.recipes().len();
        let text_height = composer_text_height(self.prompt.line_count());
        let composer_height = text_height + COMPOSER_CONTROLS_HEIGHT;
        let recipe_picker_budget = recipe_picker_height(viewport_height, composer_height);
        let recipe_picker_height = recipe_surface_height(
            recipe_count,
            self.recipe_editor.is_some(),
            recipe_picker_budget,
        );
        // The pickers hang off the bottom of the panel, which now moves with
        // the composer.
        let picker_top = TITLE_HEIGHT + TITLE_GAP + composer_height + SHELF_HEIGHT + 8.0;
        let blocker = self.blocker();
        let harness_label = self.selected_harness_label();
        let project_label = self.selected_project_label();
        let fresh_worktree = matches!(self.selected_worktree, WorktreePolicy::Fresh { .. });
        let worktree_enabled = fresh_worktree || self.selected_host.is_none();
        let logo = ui_agent_kind(&self.selected_harness);
        let fills = launcher_surface_fills(colors);

        // Wrapped lines are children of a scroll container so the composer's
        // handle can scroll BY LINE to keep the caret on screen — the whole
        // point of the rewrite. An empty prompt shows the placeholder in the
        // same row the caret is on, so the two do not fight over the baseline.
        let prompt = if self.prompt.is_empty() {
            div()
                .h(px(COMPOSER_LINE_HEIGHT))
                .flex()
                .items_center()
                .when(focused, |line| {
                    line.child(div().text_color(colors.primary.alpha(0.92)).child(CARET))
                })
                .child(
                    div()
                        .text_color(colors.tertiary)
                        .child("Describe the task…"),
                )
                .into_any_element()
        } else {
            div()
                .id("launcher-prompt-lines")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(self.prompt.scroll_handle())
                .children(self.prompt.render_lines(
                    px(COMPOSER_LINE_HEIGHT),
                    focused.then_some(CARET),
                    HighlightStyle {
                        background_color: Some(Palette::CLAY.alpha(0.35).into()),
                        ..HighlightStyle::default()
                    },
                ))
                .into_any_element()
        };

        let panel = div()
            .relative()
            .w(px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .h(px(TITLE_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("launcher-recipes-button")
                            .debug_selector(|| "launcher-recipes-button".into())
                            .absolute()
                            .left(px(COMPOSER_INSET))
                            .h(px(28.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(CONTROL_RADIUS))
                            .role(Role::Button)
                            .aria_label("Open launch recipes")
                            .aria_keyshortcuts("Meta+R")
                            .cursor_pointer()
                            .bg(if recipe_open {
                                colors.primary.alpha(0.10)
                            } else {
                                colors.primary.alpha(0.0)
                            })
                            .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                            .active(move |button| button.bg(colors.primary.alpha(0.11)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_picker(Picker::Recipe);
                                cx.notify();
                            }))
                            .child(sf_symbol("square.stack.3d.up", 10.0, Palette::CLAY))
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.secondary)
                                    .child(if recipe_count == 0 {
                                        "Recipes".to_owned()
                                    } else {
                                        format!("Recipes  {recipe_count}")
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.primary.alpha(0.94))
                            .child("What should we work on?"),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mt(px(TITLE_GAP))
                    .mx(px(COMPOSER_INSET))
                    .h(px(composer_height))
                    .rounded(px(Radius::PANEL))
                    .bg(fills.composer)
                    .border_1()
                    .border_color(if focused {
                        Palette::CLAY.alpha(0.42)
                    } else {
                        colors.primary.alpha(0.09)
                    })
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, {
                        let focus = self.focus.clone();
                        move |_, window, cx| window.focus(&focus, cx)
                    })
                    .child(
                        div()
                            .h(px(text_height))
                            .px(px(COMPOSER_PADDING))
                            .pt(px(COMPOSER_PAD_TOP))
                            .pb(px(COMPOSER_PAD_BOTTOM))
                            .text_size(px(COMPOSER_FONT_SIZE))
                            .line_height(px(COMPOSER_LINE_HEIGHT))
                            .text_color(colors.primary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .h(px(COMPOSER_CONTROLS_HEIGHT))
                            .px(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .id("launcher-add-project")
                                            .h(px(CONTROL_SIZE))
                                            .px(px(9.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .gap(px(6.0))
                                            .rounded(px(CONTROL_RADIUS))
                                            .cursor_pointer()
                                            .hover(move |button| button.bg(Fill::subtle(colors)))
                                            .active(move |button| {
                                                button.bg(colors.primary.alpha(0.10))
                                            })
                                            .child(sf_symbol("plus", 11.0, colors.secondary))
                                            .child(
                                                div()
                                                    .text_size(px(10.0))
                                                    .text_color(colors.secondary)
                                                    .child("Choose folder"),
                                            )
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.choose_folder(window, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(colors.tertiary)
                                            .child(
                                                blocker
                                                    .clone()
                                                    .or_else(|| self.fallback_notice.clone())
                                                    .unwrap_or_else(|| {
                                                        "⇧↵  New line   ⇥  Agent".to_owned()
                                                    }),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .child(
                                        div()
                                            .id("launcher-harness-button")
                                            .h(px(CONTROL_SIZE))
                                            .px(px(10.0))
                                            .flex()
                                            .items_center()
                                            .gap(px(7.0))
                                            .rounded(px(CONTROL_RADIUS))
                                            .cursor_pointer()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(colors.secondary)
                                            .bg(if harness_open {
                                                colors.primary.alpha(0.10)
                                            } else {
                                                Fill::subtle(colors)
                                            })
                                            .hover(move |button| {
                                                button.bg(colors.primary.alpha(0.09))
                                            })
                                            .active(move |button| {
                                                button.bg(colors.primary.alpha(0.12))
                                            })
                                            .child(AgentLogo::new(logo, 16.0, colors).badged(false))
                                            .child(harness_label)
                                            .child(sf_symbol("chevron.down", 7.5, colors.tertiary))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_picker(Picker::Harness);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("launcher-submit")
                                            .size(px(CONTROL_SIZE))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(CONTROL_RADIUS))
                                            .bg(if can_submit {
                                                colors.primary
                                            } else {
                                                Fill::subtle(colors)
                                            })
                                            .when(can_submit, |button| {
                                                button
                                                    .cursor_pointer()
                                                    .hover(move |button| button.opacity(0.86))
                                                    .active(move |button| button.opacity(0.72))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.submit(cx);
                                                    }))
                                            })
                                            .child(sf_symbol_weighted(
                                                "chevron.up",
                                                10.0,
                                                SymbolWeight::Bold,
                                                if can_submit {
                                                    colors.background
                                                } else {
                                                    colors.tertiary
                                                },
                                            )),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mx(px(16.0))
                    .h(px(SHELF_HEIGHT))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded_bl(px(Radius::PANEL))
                    .rounded_br(px(Radius::PANEL))
                    .bg(fills.shelf)
                    .border_1()
                    .border_color(colors.primary.alpha(0.055))
                    .child(
                        div()
                            .id("launcher-project-button")
                            .h(px(CONTROL_SIZE - 2.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .rounded(px(CONTROL_RADIUS - 1.0))
                            .cursor_pointer()
                            .bg(if project_open {
                                colors.primary.alpha(0.08)
                            } else {
                                colors.primary.alpha(0.0)
                            })
                            .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                            .active(move |button| button.bg(colors.primary.alpha(0.11)))
                            .child(sf_symbol("folder", 11.0, colors.secondary))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary.alpha(0.86))
                                    .child(project_label),
                            )
                            .child(sf_symbol("chevron.down", 8.0, colors.tertiary))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_picker(Picker::Project);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .id("launcher-details-button")
                                    .size(px(CONTROL_SIZE - 2.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(CONTROL_RADIUS - 1.0))
                                    .role(Role::Button)
                                    .aria_label("Edit recipe name, session title, and branch")
                                    .aria_keyshortcuts("Meta+Shift+R")
                                    .cursor_pointer()
                                    .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                                    .active(move |button| button.bg(colors.primary.alpha(0.11)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.edit_launch_details(cx);
                                    }))
                                    .child(sf_symbol(
                                        "gearshape",
                                        10.0,
                                        if self.selected_title.is_some()
                                            || matches!(
                                                self.selected_worktree,
                                                WorktreePolicy::Fresh { branch: Some(_) }
                                            )
                                        {
                                            Palette::CLAY
                                        } else {
                                            colors.tertiary
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .id("launcher-worktree-button")
                                    .h(px(CONTROL_SIZE - 2.0))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .rounded(px(CONTROL_RADIUS - 1.0))
                                    .text_size(px(10.0))
                                    .text_color(if worktree_enabled {
                                        colors.secondary
                                    } else {
                                        colors.tertiary
                                    })
                                    .bg(if fresh_worktree {
                                        Palette::CLAY.alpha(0.12)
                                    } else {
                                        colors.primary.alpha(0.0)
                                    })
                                    .when(worktree_enabled, |button| {
                                        button
                                            .cursor_pointer()
                                            .hover(move |button| {
                                                button.bg(colors.primary.alpha(0.08))
                                            })
                                            .active(move |button| {
                                                button.bg(colors.primary.alpha(0.11))
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_worktree();
                                                cx.notify();
                                            }))
                                    })
                                    .child(sf_symbol(
                                        "point.3.filled.connected.trianglepath.dotted",
                                        10.0,
                                        if fresh_worktree {
                                            Palette::CLAY
                                        } else {
                                            colors.tertiary
                                        },
                                    ))
                                    .child(if fresh_worktree {
                                        "Fresh lane"
                                    } else {
                                        "Current"
                                    }),
                            ),
                    ),
            )
            .when(harness_open, |panel| {
                panel.child(
                    self.floating(picker_top, cx)
                        .right(px(COMPOSER_INSET))
                        .child(self.render_harness_picker(colors, cx)),
                )
            })
            .when(project_open, |panel| {
                panel.child(
                    self.floating(picker_top, cx)
                        .left(px(COMPOSER_INSET))
                        .child(self.render_project_picker(colors, cx)),
                )
            })
            .when(recipe_open, |panel| {
                panel.child(
                    div()
                        .mt(px(RECIPE_PICKER_GAP))
                        .ml(px(COMPOSER_INSET))
                        .w(px(PANEL_WIDTH - 2.0 * COMPOSER_INSET))
                        .h(px(recipe_picker_height))
                        .overflow_hidden()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .child(self.render_recipe_picker(recipe_picker_height, colors, cx)),
                )
            })
            .when_some(self.drop_notice.clone(), |panel, notice| {
                panel.child(
                    div()
                        .id("launcher-drop-notice")
                        .mt(px(9.0))
                        .mx(px(COMPOSER_INSET))
                        .px(px(9.0))
                        .py(px(7.0))
                        .flex()
                        .items_start()
                        .gap(px(7.0))
                        .rounded(px(Radius::ROW))
                        .bg(Ink::ATTENTION.alpha(0.08))
                        .border_1()
                        .border_color(Ink::ATTENTION.alpha(0.20))
                        .child(sf_symbol("exclamationmark.triangle", 11.0, Ink::ATTENTION))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(10.0))
                                .line_height(px(14.0))
                                .text_color(colors.secondary)
                                .child(notice),
                        ),
                )
            });

        panel.into_any_element()
    }

    fn render_handoff_panel(
        &self,
        colors: SemanticColors,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let LauncherMode::Handoff(proposal) = &self.mode else {
            unreachable!("handoff panel requires a handoff proposal");
        };
        let sending = self.handoff_delivery.is_sending();
        let can_submit = self.can_submit();
        let blocker = self.blocker();
        let text_height = composer_text_height(self.prompt.line_count());
        let composer_height = text_height + COMPOSER_CONTROLS_HEIGHT;
        let composer_fill = if colors.appearance == diri_ui::Appearance::Dark {
            rgba(0x26282dff)
        } else {
            rgba(0xf2f1efff)
        };
        let remote_label = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            store
                .sessions()
                .get(&proposal.target_id)
                .and_then(|target| target.host.as_deref())
                .map(|host| format!("Remote · {}", store.host_display_name(host)))
        };
        let prompt = if self.prompt.is_empty() {
            div()
                .h(px(COMPOSER_LINE_HEIGHT))
                .flex()
                .items_center()
                .when(focused, |line| {
                    line.child(div().text_color(colors.primary.alpha(0.92)).child(CARET))
                })
                .child(
                    div()
                        .text_color(colors.tertiary)
                        .child("Describe the handoff…"),
                )
                .into_any_element()
        } else {
            div()
                .id("handoff-prompt-lines")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(self.prompt.scroll_handle())
                .children(self.prompt.render_lines(
                    px(COMPOSER_LINE_HEIGHT),
                    focused.then_some(CARET),
                    HighlightStyle {
                        background_color: Some(Palette::CLAY.alpha(0.35).into()),
                        ..HighlightStyle::default()
                    },
                ))
                .into_any_element()
        };

        div()
            .relative()
            .w(px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.primary.alpha(0.94))
                            .child("Review handoff"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(proposal.source_title.clone())
                            .child(sf_symbol("arrow.right", 9.0, colors.tertiary))
                            .child(proposal.target_title.clone())
                            .when_some(remote_label, |row, label| {
                                row.child(
                                    div()
                                        .ml(px(3.0))
                                        .px(px(7.0))
                                        .py(px(3.0))
                                        .rounded(px(Radius::CHIP))
                                        .bg(Ink::ATTENTION.alpha(0.11))
                                        .border_1()
                                        .border_color(Ink::ATTENTION.alpha(0.28))
                                        .text_size(px(9.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Ink::ATTENTION)
                                        .child(label),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mx(px(COMPOSER_INSET))
                    .h(px(composer_height))
                    .rounded(px(Radius::PANEL))
                    .bg(composer_fill)
                    .border_1()
                    .border_color(if focused {
                        Palette::CLAY.alpha(0.42)
                    } else {
                        colors.primary.alpha(0.09)
                    })
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, {
                        let focus = self.focus.clone();
                        move |_, window, cx| window.focus(&focus, cx)
                    })
                    .child(
                        div()
                            .h(px(text_height))
                            .px(px(COMPOSER_PADDING))
                            .pt(px(COMPOSER_PAD_TOP))
                            .pb(px(COMPOSER_PAD_BOTTOM))
                            .text_size(px(COMPOSER_FONT_SIZE))
                            .line_height(px(COMPOSER_LINE_HEIGHT))
                            .text_color(colors.primary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .h(px(COMPOSER_CONTROLS_HEIGHT))
                            .px(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .text_size(px(10.0))
                                    .text_color(if self.fallback_notice.is_some() {
                                        Ink::ATTENTION
                                    } else {
                                        colors.tertiary
                                    })
                                    .child(
                                        blocker
                                            .or_else(|| self.fallback_notice.clone())
                                            .unwrap_or_else(|| {
                                                "Review and edit before sending · ⇧↵ new line"
                                                    .to_owned()
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .child(
                                        div()
                                            .id("handoff-cancel")
                                            .h(px(CONTROL_SIZE))
                                            .px(px(10.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(CONTROL_RADIUS))
                                            .text_size(px(11.0))
                                            .text_color(if sending {
                                                colors.tertiary
                                            } else {
                                                colors.secondary
                                            })
                                            .when(!sending, |button| {
                                                button
                                                    .cursor_pointer()
                                                    .hover(move |button| {
                                                        button.bg(Fill::subtle(colors))
                                                    })
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.close(cx);
                                                    }))
                                            })
                                            .child("Cancel"),
                                    )
                                    .child(
                                        div()
                                            .id("handoff-submit")
                                            .h(px(CONTROL_SIZE))
                                            .px(px(12.0))
                                            .flex()
                                            .items_center()
                                            .gap(px(6.0))
                                            .rounded(px(CONTROL_RADIUS))
                                            .bg(if can_submit {
                                                colors.primary
                                            } else {
                                                Fill::subtle(colors)
                                            })
                                            .when(can_submit, |button| {
                                                button
                                                    .cursor_pointer()
                                                    .hover(move |button| button.opacity(0.86))
                                                    .active(move |button| button.opacity(0.72))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.submit(cx);
                                                    }))
                                            })
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(if can_submit {
                                                colors.background
                                            } else {
                                                colors.tertiary
                                            })
                                            .child(sf_symbol_weighted(
                                                "paperplane.fill",
                                                10.0,
                                                SymbolWeight::Semibold,
                                                if can_submit {
                                                    colors.background
                                                } else {
                                                    colors.tertiary
                                                },
                                            ))
                                            .child(if sending {
                                                "Sending…"
                                            } else {
                                                "Send handoff"
                                            }),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_session_panel(
        &self,
        colors: SemanticColors,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = match &self.target {
            LauncherTarget::Session(id) => self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned")
                .sessions()
                .get(id)
                .cloned(),
            LauncherTarget::NewSession => None,
        };
        let title = session.as_ref().map_or_else(
            || "Unavailable session".to_owned(),
            |session| session.title.clone(),
        );
        let cwd = session.as_ref().map_or_else(
            || "Session no longer available".to_owned(),
            |session| session.cwd.clone(),
        );
        let logo = session.as_ref().map_or(UiAgentKind::Generic, |session| {
            ui_agent_kind(session.effective_kind())
        });
        let can_submit = self.can_submit();
        let draft_state = session.as_ref().map_or("Local draft", |session| {
            if session.hibernation.is_some() {
                "Local draft · sleeping agent untouched"
            } else {
                "Local draft · nothing sent"
            }
        });
        let text_height = composer_text_height(self.prompt.line_count());
        let composer_height = text_height + COMPOSER_CONTROLS_HEIGHT;
        let fills = launcher_surface_fills(colors);
        let prompt = if self.prompt.is_empty() {
            div()
                .h(px(COMPOSER_LINE_HEIGHT))
                .flex()
                .items_center()
                .when(focused, |line| {
                    line.child(div().text_color(colors.primary.alpha(0.92)).child(CARET))
                })
                .child(
                    div()
                        .text_color(colors.tertiary)
                        .child("Add context or instructions…"),
                )
                .into_any_element()
        } else {
            div()
                .id("session-composer-prompt-lines")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(self.prompt.scroll_handle())
                .children(self.prompt.render_lines(
                    px(COMPOSER_LINE_HEIGHT),
                    focused.then_some(CARET),
                    HighlightStyle {
                        background_color: Some(Palette::GEMINI_BLUE.alpha(0.30).into()),
                        ..HighlightStyle::default()
                    },
                ))
                .into_any_element()
        };

        div()
            .relative()
            .w(px(PANEL_WIDTH))
            .child(
                div()
                    .h(px(TITLE_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .child(sf_symbol("paperclip", 16.0, Palette::GEMINI_BLUE))
                    .child(
                        div()
                            .max_w(px(PANEL_WIDTH - 52.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(20.0))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(colors.primary.alpha(0.94))
                            .child(format!("Add context to {title}")),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mt(px(TITLE_GAP))
                    .mx(px(COMPOSER_INSET))
                    .h(px(composer_height))
                    .rounded(px(Radius::PANEL))
                    .bg(fills.composer)
                    .border_1()
                    .border_color(if focused {
                        Palette::GEMINI_BLUE.alpha(0.46)
                    } else {
                        colors.primary.alpha(0.09)
                    })
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, {
                        let focus = self.focus.clone();
                        move |_, window, cx| window.focus(&focus, cx)
                    })
                    .child(
                        div()
                            .h(px(text_height))
                            .px(px(COMPOSER_PADDING))
                            .pt(px(COMPOSER_PAD_TOP))
                            .pb(px(COMPOSER_PAD_BOTTOM))
                            .text_size(px(COMPOSER_FONT_SIZE))
                            .line_height(px(COMPOSER_LINE_HEIGHT))
                            .text_color(colors.primary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .h(px(COMPOSER_CONTROLS_HEIGHT))
                            .px(px(10.0))
                            .pb(px(8.0))
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(div().text_size(px(10.0)).text_color(colors.tertiary).child(
                                self.blocker().unwrap_or_else(|| {
                                    "Review first — dropping sent nothing".to_owned()
                                }),
                            ))
                            .child(
                                div()
                                    .id("session-composer-submit")
                                    .size(px(CONTROL_SIZE))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(CONTROL_RADIUS))
                                    .bg(if can_submit {
                                        colors.primary
                                    } else {
                                        Fill::subtle(colors)
                                    })
                                    .when(can_submit, |button| {
                                        button
                                            .cursor_pointer()
                                            .hover(move |button| button.opacity(0.86))
                                            .active(move |button| button.opacity(0.72))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit(cx);
                                            }))
                                    })
                                    .child(sf_symbol_weighted(
                                        "arrow.up",
                                        10.0,
                                        SymbolWeight::Bold,
                                        if can_submit {
                                            colors.background
                                        } else {
                                            colors.tertiary
                                        },
                                    )),
                            ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mx(px(16.0))
                    .h(px(SHELF_HEIGHT))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded_bl(px(Radius::PANEL))
                    .rounded_br(px(Radius::PANEL))
                    .bg(fills.shelf)
                    .border_1()
                    .border_color(colors.primary.alpha(0.055))
                    .child(AgentLogo::new(logo, 17.0, colors).badged(false))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child(cwd),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(colors.tertiary)
                            .child(draft_state),
                    ),
            )
            .when_some(self.drop_notice.clone(), |panel, notice| {
                panel.child(
                    div()
                        .id("session-composer-drop-notice")
                        .mt(px(9.0))
                        .mx(px(COMPOSER_INSET))
                        .px(px(9.0))
                        .py(px(7.0))
                        .flex()
                        .items_start()
                        .gap(px(7.0))
                        .rounded(px(Radius::ROW))
                        .bg(Ink::ATTENTION.alpha(0.08))
                        .border_1()
                        .border_color(Ink::ATTENTION.alpha(0.20))
                        .child(sf_symbol(
                            "exclamationmark.circle.fill",
                            11.0,
                            Ink::ATTENTION,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(10.0))
                                .line_height(px(14.0))
                                .text_color(colors.secondary)
                                .child(notice),
                        ),
                )
            })
            .into_any_element()
    }

    /// Wrapper for a picker popover. It swallows its own mouse-down so the
    /// canvas behind it — which closes any open picker — does not tear the
    /// list away between press and release, which would eat the click.
    fn floating(&self, top: f32, cx: &mut Context<Self>) -> gpui::Div {
        div().absolute().top(px(top)).on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
    }
}

impl Focusable for LauncherOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LauncherOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .id("new-session-launcher")
            .key_context("DiriLauncher")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.handle_key_down(event, window, cx);
            }));
        if !self.open {
            return root.size(px(0.0));
        }

        // Soft-wrapping needs the text system, which only exists here. Doing
        // it before the panel is built is what lets the composer size itself
        // to the prompt and scroll the caret into view.
        self.prompt.layout(
            px(COMPOSER_TEXT_WIDTH),
            gpui::font(crate::fonts::ui_family()),
            px(COMPOSER_FONT_SIZE),
            window,
        );

        let theme_id = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .terminal_theme
            .clone();
        let colors = launcher_colors_for_theme(&theme_id);
        let focused = self.focus.is_focused(window);
        root.size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.background)
            // The entire empty workbench behaves like the editor's canvas: a
            // click anywhere returns to the prompt and dismisses whichever
            // picker was open, which previously stayed up until you found the
            // button again or pressed Escape.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.picker = None;
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
            )
            // Command-N is a high-frequency keyboard action; the destination
            // appears immediately rather than making the user wait on motion.
            .child(self.render_panel(window.viewport_size().height.as_f32(), colors, focused, cx))
    }
}

fn initial_target(services: &AppServices) -> (AgentKind, String, Option<String>) {
    let store = services
        .store
        .store
        .read()
        .expect("session store lock poisoned");
    let selected = store
        .selected_session()
        .and_then(|session| {
            store
                .projects()
                .get(&session.project_id)
                .map(|project| (project.root.clone(), session.host.clone()))
        })
        .or_else(|| {
            store
                .projects()
                .values()
                .min_by(|left, right| left.name.cmp(&right.name))
                .map(|project| {
                    let host = store
                        .sessions()
                        .values()
                        .find(|session| session.project_id == project.id)
                        .and_then(|session| session.host.clone());
                    (project.root.clone(), host)
                })
        })
        .unwrap_or_default();
    (
        store.preferences().default_agent.clone(),
        selected.0,
        selected.1,
    )
}

fn text_editor(value: &str) -> QueryEditor {
    let mut editor = QueryEditor::default();
    editor.insert(value);
    editor
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn delete_clears_active_recipe(
    persisted: bool,
    active: Option<&LaunchRecipe>,
    deleted_id: &str,
) -> bool {
    persisted && active.is_some_and(|recipe| recipe.id == deleted_id)
}

fn recipe_project_for_draft(
    active: Option<&LaunchRecipe>,
    project_edited: bool,
    selected_root: &str,
    selected_host: Option<&str>,
    projects: &[LauncherProject],
) -> RecipeProject {
    if !project_edited && let Some(active) = active {
        return active.project.clone();
    }
    projects
        .iter()
        .find(|project| {
            project.project.root == selected_root && project.host.as_deref() == selected_host
        })
        .map_or_else(
            || RecipeProject::Path {
                path: selected_root.to_owned(),
            },
            |project| RecipeProject::Tracked {
                id: project.project.id.clone(),
                last_known_root: project.project.root.clone(),
            },
        )
}

fn transition_draft(
    current: &LauncherTarget,
    next: &LauncherTarget,
    current_text: &str,
    new_session_draft: &mut String,
    session_drafts: &mut HashMap<SessionId, String>,
) -> String {
    match current {
        LauncherTarget::NewSession => current_text.clone_into(new_session_draft),
        LauncherTarget::Session(id) if current_text.is_empty() => {
            session_drafts.remove(id);
        }
        LauncherTarget::Session(id) => {
            session_drafts.insert(id.clone(), current_text.to_owned());
        }
    }
    match next {
        LauncherTarget::NewSession => new_session_draft.clone(),
        LauncherTarget::Session(id) => session_drafts.get(id).cloned().unwrap_or_default(),
    }
}

fn handoff_command(mode: &LauncherMode, text: &str) -> Option<SendTextCommand> {
    let LauncherMode::Handoff(proposal) = mode else {
        return None;
    };
    let text = text.trim();
    (!text.is_empty()).then(|| SendTextCommand {
        session_id: proposal.target_id.clone(),
        text: text.to_owned(),
        submit: true,
    })
}

fn ui_agent_kind(kind: &AgentKind) -> UiAgentKind {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => UiAgentKind::ClaudeCode,
        AgentKind::CODEX_ID => UiAgentKind::Codex,
        AgentKind::CURSOR_ID => UiAgentKind::Cursor,
        AgentKind::GEMINI_ID => UiAgentKind::Gemini,
        AgentKind::SHELL_ID => UiAgentKind::Shell,
        _ => UiAgentKind::Generic,
    }
}

fn project_commit(project_count: usize, highlight: usize) -> ProjectCommit {
    if highlight < project_count {
        ProjectCommit::Recent(highlight)
    } else {
        ProjectCommit::ChooseFolder
    }
}

fn close_picker_for_folder_choice(picker: &mut Option<Picker>) {
    *picker = None;
}

fn apply_folder_choice(selected_root: &mut String, chosen: Option<&Path>) -> bool {
    let Some(chosen) = chosen else {
        return false;
    };
    *selected_root = chosen.to_string_lossy().into_owned();
    true
}

fn recipe_action(
    id: String,
    system_image: &'static str,
    label: &'static str,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let debug_id = id.clone();
    div()
        .id(id)
        .debug_selector(move || debug_id)
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .role(Role::Button)
        .aria_label(label)
        .cursor_pointer()
        .hover(move |button| button.bg(colors.primary.alpha(0.08)))
        .active(move |button| button.bg(colors.primary.alpha(0.12)))
        .on_click(on_click)
        .child(sf_symbol(system_image, 9.0, colors.secondary))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use crate::store::StoreRuntime;
    use crate::usage::UsageSnapshot;
    use gpui::{Keystroke, Modifiers, TestAppContext};

    fn test_services(store: Arc<StoreRuntime>) -> Arc<AppServices> {
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        Arc::new(AppServices {
            store,
            usage_tx,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        })
    }

    fn key(value: &str) -> KeyDownEvent {
        let mut keystroke = Keystroke::parse(value).expect("valid test key");
        if !keystroke.modifiers.platform
            && !keystroke.modifiers.control
            && !keystroke.modifiers.function
            && keystroke.key.chars().count() == 1
        {
            keystroke.key_char = Some(keystroke.key.clone());
        }
        KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn manifest_ids_have_readable_fallback_labels() {
        assert_eq!(title_case_id("claude-code"), "Claude Code");
        assert_eq!(title_case_id("open_code"), "Open Code");
    }

    #[gpui::test]
    fn command_n_toggles_the_launcher_open_then_closed(cx: &mut TestAppContext) {
        let store = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        let services = Arc::new(AppServices {
            store,
            usage_tx,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        });
        let (launcher, cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        assert!(!launcher.read_with(cx, |launcher, _| launcher.is_open()));
        launcher.update_in(cx, |launcher, window, cx| {
            assert!(launcher.toggle(window, cx));
        });
        assert!(launcher.read_with(cx, |launcher, _| launcher.is_open()));
        launcher.update_in(cx, |launcher, window, cx| {
            assert!(!launcher.toggle(window, cx));
        });
        assert!(!launcher.read_with(cx, |launcher, _| launcher.is_open()));
    }

    #[gpui::test]
    fn recipes_are_fully_manageable_from_the_keyboard(cx: &mut TestAppContext) {
        let store = Arc::new(StoreRuntime::inert());
        store
            .store
            .write()
            .expect("store lock")
            .update_preferences(|prefs| {
                prefs
                    .launch_recipes
                    .add(LaunchRecipe::draft(
                        "Review",
                        AgentKind::CODEX,
                        RecipeProject::Path {
                            path: "/tmp".into(),
                        },
                        None,
                        "Review this change",
                    ))
                    .expect("add recipe");
            })
            .expect("save fixture recipe");
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        let services = Arc::new(AppServices {
            store: Arc::clone(&store),
            usage_tx,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        });
        let (launcher, cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open(window, cx);
            assert!(launcher.handle_key_down(&key("cmd-shift-r"), window, cx));
            assert!(
                launcher
                    .recipe_editor
                    .as_ref()
                    .is_some_and(|editor| editor.id.is_none())
            );
            assert!(launcher.handle_key_down(&key("tab"), window, cx));
            assert!(launcher.handle_key_down(&key("l"), window, cx));
            assert!(launcher.handle_key_down(&key("enter"), window, cx));
            assert_eq!(launcher.selected_title.as_deref(), Some("l"));

            assert!(launcher.handle_key_down(&key("cmd-r"), window, cx));
            assert_eq!(launcher.picker, Some(Picker::Recipe));

            assert!(launcher.handle_key_down(&key("space"), window, cx));
            assert_eq!(
                launcher
                    .active_recipe
                    .as_ref()
                    .map(|recipe| recipe.name.as_str()),
                Some("Review")
            );

            assert!(launcher.handle_key_down(&key("cmd-r"), window, cx));
            assert!(launcher.handle_key_down(&key("e"), window, cx));
            assert!(launcher.recipe_editor.is_some());
            assert!(launcher.handle_key_down(&key("x"), window, cx));
            assert!(launcher.handle_key_down(&key("tab"), window, cx));
            assert!(launcher.handle_key_down(&key("tab"), window, cx));
            assert!(launcher.handle_key_down(&key("enter"), window, cx));
            assert!(launcher.recipe_editor.is_none());

            assert!(launcher.handle_key_down(&key("cmd-d"), window, cx));
            assert_eq!(launcher.recipes().len(), 2);
            assert!(launcher.handle_key_down(&key("cmd-down"), window, cx));
            assert_eq!(launcher.highlight, 1);
            assert!(launcher.handle_key_down(&key("backspace"), window, cx));
            assert!(launcher.pending_recipe_delete.is_some());
            assert!(launcher.handle_key_down(&key("backspace"), window, cx));
            assert_eq!(launcher.recipes().len(), 1);

            // Enter is the one-action launch key. Preview mode intentionally
            // unfolds the fields instead of spawning, which is observable as
            // a selected active recipe and a closed picker.
            launcher.highlight = 0;
            assert!(launcher.handle_key_down(&key("enter"), window, cx));
            assert!(launcher.active_recipe.is_some());
            assert!(launcher.picker.is_none());
        });
    }

    #[gpui::test]
    fn saving_a_recipe_binds_the_current_draft_to_its_allocated_identity(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .expect("store lock")
            .set_agent_catalog(diri_proto::AgentReadinessResult {
                agents: vec![diri_proto::AgentReadinessItem {
                    kind: AgentKind::CODEX,
                    binary: "codex".into(),
                    path: Some("/usr/bin/codex".into()),
                    ..diri_proto::AgentReadinessItem::default()
                }],
                ..diri_proto::AgentReadinessResult::default()
            });
        let services = test_services(Arc::clone(&runtime));
        let (launcher, _cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        launcher.update(cx, |launcher, cx| {
            launcher.selected_harness = AgentKind::CODEX;
            launcher.selected_root = "/tmp".into();
            launcher.prompt.insert_multiline("Review this change");
            launcher.save_current_recipe(cx);
            let active = launcher.active_recipe.as_ref().expect("bound saved recipe");
            assert!(!active.id.is_empty());
            assert_eq!(launcher.recipes().len(), 1);
            assert_eq!(launcher.recipes()[0].id, active.id);
        });
    }

    #[gpui::test]
    fn saved_metadata_edits_preserve_live_one_off_overrides(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let stored = {
            let mut store = runtime.store.write().expect("store lock");
            store
                .update_preferences(|prefs| {
                    prefs
                        .launch_recipes
                        .add(LaunchRecipe::draft(
                            "Review",
                            AgentKind::CODEX,
                            RecipeProject::Path {
                                path: "/tmp".into(),
                            },
                            None,
                            "Stored prompt",
                        ))
                        .expect("add recipe");
                })
                .expect("save fixture");
            store.preferences().launch_recipes.items()[0].clone()
        };
        let services = test_services(Arc::clone(&runtime));
        let (launcher, _cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        launcher.update(cx, |launcher, cx| {
            launcher.preview_recipe(&stored);
            launcher.prompt.clear();
            launcher.prompt.insert_multiline("One-off prompt");
            launcher.selected_root = "/private/tmp".into();
            launcher.recipe_project_edited = true;
            launcher.selected_harness = AgentKind::SHELL;
            launcher.selected_title = Some("One-off title".into());
            launcher.edit_recipe(&stored.id, cx);
            let editor = launcher.recipe_editor.as_mut().expect("editor");
            editor.name.select_all();
            editor.name.insert("Renamed baseline");
            editor.title.select_all();
            editor.title.insert("Stored title");
            launcher.save_recipe_editor(cx);

            assert_eq!(launcher.prompt.text(), "One-off prompt");
            assert_eq!(launcher.selected_root, "/private/tmp");
            assert_eq!(launcher.selected_harness, AgentKind::SHELL);
            assert_eq!(launcher.selected_title.as_deref(), Some("One-off title"));
            let active = launcher.active_recipe.as_ref().expect("active baseline");
            assert_eq!(active.name, "Renamed baseline");
            assert_eq!(active.title.as_deref(), Some("Stored title"));
        });
    }

    #[gpui::test]
    fn metadata_rename_preserves_a_live_name_override_and_rebinds_the_normalized_baseline(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let stored = {
            let mut store = runtime.store.write().expect("store lock");
            store
                .update_preferences(|prefs| {
                    prefs
                        .launch_recipes
                        .add(LaunchRecipe::draft(
                            "Review",
                            AgentKind::CODEX,
                            RecipeProject::Path {
                                path: "/tmp".into(),
                            },
                            None,
                            "Stored prompt",
                        ))
                        .expect("add recipe");
                })
                .expect("save fixture");
            store.preferences().launch_recipes.items()[0].clone()
        };
        let services = test_services(Arc::clone(&runtime));
        let (launcher, _cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        launcher.update(cx, |launcher, cx| {
            launcher.preview_recipe(&stored);
            launcher.draft_recipe_name = Some("One-off display name".into());
            launcher.edit_recipe(&stored.id, cx);
            let editor = launcher.recipe_editor.as_mut().expect("editor");
            editor.name.select_all();
            editor.name.insert(&"N".repeat(100));
            launcher.save_recipe_editor(cx);

            let persisted = launcher.recipes().into_iter().next().expect("recipe");
            let active = launcher.active_recipe.as_ref().expect("active baseline");
            assert_eq!(active, &persisted);
            assert_eq!(active.name.chars().count(), 80);
            assert_eq!(
                launcher.draft_recipe_name.as_deref(),
                Some("One-off display name")
            );
        });
    }

    #[gpui::test]
    fn cold_remote_recipe_launches_when_readiness_arrives_without_a_second_action(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        let stored = {
            let mut store = runtime.store.write().expect("store lock");
            store.set_hosts(vec![diri_proto::HostEntry {
                id: "forge".into(),
                name: Some("Build Forge".into()),
                ssh: "forge".into(),
                default_cwd: None,
                node: None,
            }]);
            store
                .update_preferences(|prefs| {
                    prefs
                        .launch_recipes
                        .add(LaunchRecipe::draft(
                            "Remote review",
                            AgentKind::CODEX,
                            RecipeProject::Path {
                                path: "~/diri".into(),
                            },
                            Some("forge".into()),
                            "Review the change",
                        ))
                        .expect("add recipe");
                })
                .expect("save fixture");
            store.preferences().launch_recipes.items()[0].clone()
        };
        let services = test_services(Arc::clone(&runtime));
        let (launcher, cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, false, cx));

        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open(window, cx);
            launcher.activate_recipe(&stored.id, window, cx);
            assert!(launcher.open, "the launcher stays visible while checking");
            assert_eq!(
                launcher.pending_recipe_activation.as_deref(),
                Some(stored.id.as_str())
            );
        });

        runtime
            .store
            .write()
            .expect("store lock")
            .set_agent_catalog(diri_proto::AgentReadinessResult {
                host: Some("forge".into()),
                agents: vec![diri_proto::AgentReadinessItem {
                    kind: AgentKind::CODEX,
                    binary: "codex".into(),
                    path: Some("/usr/bin/codex".into()),
                    ..diri_proto::AgentReadinessItem::default()
                }],
                ..diri_proto::AgentReadinessResult::default()
            });
        runtime.publish_local_change();
        cx.run_until_parked();

        launcher.read_with(cx, |launcher, _| {
            assert!(
                !launcher.open,
                "readiness must complete the original activation"
            );
            assert!(launcher.pending_recipe_activation.is_none());
            assert!(launcher.prompt.is_empty());
        });
    }

    #[gpui::test]
    fn opening_recipes_warms_each_valid_destination_and_names_the_host(cx: &mut TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        {
            let mut store = runtime.store.write().expect("store lock");
            store.set_hosts(vec![diri_proto::HostEntry {
                id: "forge".into(),
                name: Some("Build Forge".into()),
                ssh: "forge".into(),
                default_cwd: None,
                node: None,
            }]);
            store
                .update_preferences(|prefs| {
                    for name in ["Remote review", "Remote tests"] {
                        prefs
                            .launch_recipes
                            .add(LaunchRecipe::draft(
                                name,
                                AgentKind::CODEX,
                                RecipeProject::Path {
                                    path: "~/diri".into(),
                                },
                                Some("forge".into()),
                                "Run",
                            ))
                            .expect("add recipe");
                    }
                })
                .expect("save recipes");
        }
        let services = test_services(Arc::clone(&runtime));
        let (launcher, _cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));
        launcher.update(cx, |launcher, _| {
            launcher.selected_root = "~/diri".into();
            launcher.selected_host = Some("forge".into());
            launcher.toggle_picker(Picker::Recipe);
            assert_eq!(launcher.host_label(Some("forge")), "Build Forge");
            assert_eq!(launcher.selected_project_label(), "diri · Build Forge");
        });
        assert!(
            runtime
                .store
                .read()
                .expect("store lock")
                .agent_catalog_is_loading(Some("forge")),
            "opening the picker must start readiness before a row is clicked"
        );
    }

    #[test]
    fn failed_delete_keeps_the_active_recipe_identity() {
        let mut recipe = LaunchRecipe::draft(
            "Review",
            AgentKind::CODEX,
            RecipeProject::Path {
                path: "/tmp".into(),
            },
            None,
            "Run",
        );
        recipe.id = "recipe-1".into();
        assert!(!delete_clears_active_recipe(
            false,
            Some(&recipe),
            "recipe-1"
        ));
        assert!(delete_clears_active_recipe(true, Some(&recipe), "recipe-1"));
    }

    #[test]
    fn recipe_picker_budget_fits_the_minimum_window_even_with_a_tall_composer() {
        for lines in [COMPOSER_MIN_LINES, COMPOSER_MAX_LINES] {
            let composer = composer_text_height(lines) + COMPOSER_CONTROLS_HEIGHT;
            let picker = recipe_picker_height(560.0, composer);
            let total =
                TITLE_HEIGHT + TITLE_GAP + composer + SHELF_HEIGHT + RECIPE_PICKER_GAP + picker;
            assert!(total <= 560.0 - 2.0 * PANEL_EDGE_INSET + 0.01);
            assert!(picker >= RECIPE_PICKER_MIN_HEIGHT);
        }
    }

    #[gpui::test]
    fn crowded_recipe_picker_stays_in_view_scrolls_with_keys_and_reveals_hover_actions(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(StoreRuntime::inert());
        {
            let mut store = runtime.store.write().expect("store lock");
            store.set_agent_catalog(diri_proto::AgentReadinessResult {
                agents: vec![diri_proto::AgentReadinessItem {
                    kind: AgentKind::CODEX,
                    binary: "codex".into(),
                    path: Some("/usr/bin/codex".into()),
                    ..diri_proto::AgentReadinessItem::default()
                }],
                ..diri_proto::AgentReadinessResult::default()
            });
            store
                .update_preferences(|prefs| {
                    for index in 1..=8 {
                        prefs
                            .launch_recipes
                            .add(LaunchRecipe::draft(
                                format!("Recipe {index}"),
                                AgentKind::CODEX,
                                RecipeProject::Path {
                                    path: "/tmp".into(),
                                },
                                None,
                                "Run",
                            ))
                            .expect("add recipe");
                    }
                })
                .expect("save recipes");
        }
        let services = test_services(runtime);
        let (launcher, cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));
        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open(window, cx);
            launcher.selected_harness = AgentKind::CODEX;
            launcher.selected_root = "/tmp".into();
            launcher.toggle_picker(Picker::Recipe);
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(760.0), px(560.0)));

        assert!(
            cx.debug_bounds("launcher-recipes-button").is_some(),
            "open launcher renders"
        );
        let list = cx
            .debug_bounds("launcher-recipe-list")
            .expect("recipe viewport");
        assert!(list.top() >= px(0.0));
        assert!(list.bottom() <= px(560.0));

        let second = cx
            .debug_bounds("launcher-recipe-recipe-2")
            .expect("second recipe");
        cx.simulate_mouse_move(second.center(), None, Modifiers::default());
        assert!(
            cx.debug_bounds("recipe-edit-recipe-2").is_some(),
            "hovering an ordinary row must reveal its management controls"
        );

        launcher.update_in(cx, |launcher, window, cx| {
            for _ in 0..7 {
                assert!(launcher.handle_key_down(&key("down"), window, cx));
            }
        });
        let list = cx
            .debug_bounds("launcher-recipe-list")
            .expect("recipe viewport after navigation");
        let last = cx
            .debug_bounds("launcher-recipe-recipe-8")
            .expect("keyboard-selected last recipe");
        assert!(last.top() >= list.top());
        assert!(last.bottom() <= list.bottom());
    }

    /// Writes a deterministic visual artifact for design review without live
    /// user state or Screen Recording permission.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the deterministic launch-recipes screenshot artifact"]
    fn render_launch_recipes_preview_screenshot() {
        use std::path::PathBuf;

        let output = std::env::var_os("DIRI_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("set DIRI_VISUAL_OUTPUT to the target PNG path");
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));

        let runtime = Arc::new(StoreRuntime::inert());
        let repository_root = std::env::current_dir()
            .expect("fixture repository")
            .ancestors()
            .find(|path| path.join(".git").exists())
            .expect("fixture runs inside a repository")
            .to_string_lossy()
            .into_owned();
        assert!(Path::new(&repository_root).is_dir(), "fixture root exists");
        {
            let mut store = runtime.store.write().expect("store lock");
            store.set_agent_catalog(diri_proto::AgentReadinessResult {
                agents: vec![diri_proto::AgentReadinessItem {
                    kind: AgentKind::CODEX,
                    binary: "codex".into(),
                    path: Some("/usr/local/bin/codex".into()),
                    ..diri_proto::AgentReadinessItem::default()
                }],
                ..diri_proto::AgentReadinessResult::default()
            });
            store
                .update_preferences(|prefs| {
                    let mut fresh = LaunchRecipe::draft(
                        "Review this PR",
                        AgentKind::CODEX,
                        RecipeProject::Path {
                            path: repository_root.clone(),
                        },
                        None,
                        "Review this branch for correctness and product quality",
                    );
                    fresh.title = Some("PR review".into());
                    fresh.worktree = WorktreePolicy::Fresh {
                        branch: Some("review/current".into()),
                    };
                    prefs.launch_recipes.add(fresh).expect("add fresh recipe");
                    prefs
                        .launch_recipes
                        .add(LaunchRecipe::draft(
                            "Fix failing tests remotely",
                            AgentKind::CODEX,
                            RecipeProject::Path {
                                path: "~/diri".into(),
                            },
                            Some("missing-forge".into()),
                            "Find the failure and ship the smallest robust fix",
                        ))
                        .expect("add stale recipe");
                    for (name, prompt) in [
                        ("Audit terminal latency", "Profile input-to-paint latency"),
                        (
                            "Review accessibility",
                            "Audit keyboard and screen reader flows",
                        ),
                        ("Triage release blockers", "Find and rank release blockers"),
                        (
                            "Polish onboarding",
                            "Make the first-run path feel inevitable",
                        ),
                        (
                            "Harden persistence",
                            "Stress-test durable state transitions",
                        ),
                        ("Prepare changelog", "Draft a concise human changelog"),
                    ] {
                        prefs
                            .launch_recipes
                            .add(LaunchRecipe::draft(
                                name,
                                AgentKind::CODEX,
                                RecipeProject::Path {
                                    path: repository_root.clone(),
                                },
                                None,
                                prompt,
                            ))
                            .expect("add crowded fixture recipe");
                    }
                })
                .expect("save fixture recipes");
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        let services = Arc::new(AppServices {
            store: runtime,
            usage_tx,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        });
        let window = cx
            .open_window(gpui::size(px(760.0), px(560.0)), move |window, cx| {
                cx.new(|cx| {
                    let mut launcher = LauncherOverlay::new(services, true, cx);
                    launcher.open(window, cx);
                    launcher
                        .prompt
                        .insert_multiline("Audit this change before merge");
                    launcher.selected_root.clone_from(&repository_root);
                    launcher.selected_harness = AgentKind::CODEX;
                    launcher.picker = Some(Picker::Recipe);
                    launcher.highlight = 7;
                    launcher.recipe_scroll.scroll_to_item(7);
                    launcher
                })
            })
            .expect("open headless launcher window");
        cx.run_until_parked();
        window
            .update(&mut cx, |launcher, window, _| {
                launcher.recipe_scroll.scroll_to_item(7);
                window.refresh();
            })
            .expect("refresh launcher window");
        cx.run_until_parked();
        let screenshot = cx
            .capture_screenshot(window.into())
            .expect("capture launcher screenshot");
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).expect("create screenshot directory");
        }
        screenshot.save(output).expect("save launcher screenshot");
    }

    #[test]
    fn launcher_uses_the_selected_diri_theme_and_semantic_surfaces() {
        let colors = launcher_colors_for_theme("dirijor-light");
        let expected = crate::app_theme::colors("dirijor-light");
        let fills = launcher_surface_fills(colors);

        assert_eq!(colors, expected);
        assert_eq!(fills.composer, expected.floating_surface());
        assert_eq!(fills.shelf, expected.sidebar_surface());
    }

    #[test]
    fn project_picker_always_ends_with_choose_folder() {
        assert_eq!(project_commit(0, 0), ProjectCommit::ChooseFolder);
        assert_eq!(project_commit(2, 0), ProjectCommit::Recent(0));
        assert_eq!(project_commit(2, 1), ProjectCommit::Recent(1));
        assert_eq!(project_commit(2, 2), ProjectCommit::ChooseFolder);
    }

    #[test]
    fn folder_chooser_closes_picker_and_preserves_draft_across_cancel_and_completion() {
        let mut prompt = PromptComposer::default();
        prompt.insert_multiline("keep this\nunfinished prompt");
        let mut picker = Some(Picker::Project);
        let mut selected = "/work/current".to_owned();

        close_picker_for_folder_choice(&mut picker);
        assert!(picker.is_none(), "native chooser must dismiss the popover");
        assert!(!apply_folder_choice(&mut selected, None));
        assert_eq!(selected, "/work/current");
        assert_eq!(prompt.text(), "keep this\nunfinished prompt");

        assert!(apply_folder_choice(
            &mut selected,
            Some(Path::new("/work/chosen"))
        ));
        assert_eq!(selected, "/work/chosen");
        assert_eq!(prompt.text(), "keep this\nunfinished prompt");
    }

    #[test]
    fn active_recipe_keeps_stable_project_identity_until_an_explicit_repair() {
        let active = LaunchRecipe::draft(
            "Missing project",
            AgentKind::CODEX,
            RecipeProject::Tracked {
                id: diri_proto::ProjectId("gone".into()),
                last_known_root: "/tmp".into(),
            },
            None,
            "Repair me",
        );
        let replacement = LauncherProject {
            project: Project {
                id: diri_proto::ProjectId("replacement".into()),
                root: "/tmp".into(),
                name: "Replacement".into(),
                pinned_order: None,
                host: None,
            },
            host: None,
        };

        assert!(matches!(
            recipe_project_for_draft(
                Some(&active),
                false,
                "/tmp",
                None,
                std::slice::from_ref(&replacement),
            ),
            RecipeProject::Tracked { id, .. } if id.0 == "gone"
        ));
        assert!(matches!(
            recipe_project_for_draft(Some(&active), true, "/tmp", None, &[replacement]),
            RecipeProject::Tracked { id, .. } if id.0 == "replacement"
        ));
    }

    #[test]
    fn metadata_editor_preserves_recipe_until_explicit_save() {
        let mut recipe = LaunchRecipe::draft(
            "Review",
            AgentKind::CODEX,
            RecipeProject::Path {
                path: "/tmp".into(),
            },
            None,
            "Review this",
        );
        recipe.id = "recipe-1".into();
        recipe.title = Some("Old title".into());
        let serialized = serde_json::to_vec(&recipe).expect("serialize original");

        let mut editor = RecipeMetadataEditor::saved(&recipe);
        editor.name.select_all();
        editor.name.insert("Updated review");
        editor.title.select_all();
        editor.title.insert("New title");

        assert_eq!(
            serde_json::to_vec(&recipe).expect("serialize after one-off editing"),
            serialized,
            "draft metadata cannot mutate the saved value"
        );
        assert_eq!(editor.name.text(), "Updated review");
        assert_eq!(editor.title.text(), "New title");
    }

    #[test]
    fn each_session_and_the_new_session_launcher_keep_an_independent_draft() {
        let new = LauncherTarget::NewSession;
        let first = LauncherTarget::Session(SessionId("first".into()));
        let second = LauncherTarget::Session(SessionId("second".into()));
        let mut new_draft = String::new();
        let mut sessions = HashMap::new();

        assert_eq!(
            transition_draft(
                &new,
                &first,
                "unfinished new session",
                &mut new_draft,
                &mut sessions,
            ),
            ""
        );
        assert_eq!(
            transition_draft(
                &first,
                &second,
                "review this\n'/tmp/one.rs'",
                &mut new_draft,
                &mut sessions,
            ),
            ""
        );
        assert_eq!(
            transition_draft(
                &second,
                &first,
                "compare '/tmp/two.rs'",
                &mut new_draft,
                &mut sessions,
            ),
            "review this\n'/tmp/one.rs'"
        );
        assert_eq!(
            transition_draft(
                &first,
                &new,
                "review this\n'/tmp/one.rs'",
                &mut new_draft,
                &mut sessions,
            ),
            "unfinished new session"
        );
    }

    #[test]
    fn handoff_is_inert_until_explicit_submit_builds_one_targeted_command() {
        let proposal = HandoffProposal {
            source_id: SessionId("source".into()),
            target_id: SessionId("target".into()),
            source_title: "Source".into(),
            target_title: "Target".into(),
            summary: "cached summary".into(),
        };
        assert_eq!(handoff_command(&LauncherMode::NewSession, "edited"), None);
        assert_eq!(
            handoff_command(&LauncherMode::Handoff(proposal), "  edited summary  "),
            Some(SendTextCommand {
                session_id: SessionId("target".into()),
                text: "edited summary".into(),
                submit: true,
            })
        );
    }

    #[test]
    fn handoff_delivery_accepts_one_send_and_ignores_stale_completions() {
        let mut delivery = HandoffDeliveryState::default();
        let first = delivery.begin().expect("first send");
        assert!(delivery.is_sending());
        assert_eq!(delivery.begin(), None, "double submit must be refused");

        delivery.invalidate();
        let replacement = delivery.begin().expect("replacement proposal send");
        assert_ne!(first, replacement);
        assert!(
            !delivery.settle(first),
            "an old RPC must not close a replacement composer"
        );
        assert!(delivery.is_sending());
        assert!(delivery.settle(replacement));
        assert!(!delivery.is_sending());
    }

    #[gpui::test]
    fn staging_context_uses_the_requested_identity_without_selecting_or_sending(
        cx: &mut TestAppContext,
    ) {
        let runtime = Arc::new(crate::store::StoreRuntime::inert());
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        assert!(fixture.list.sessions.len() >= 2);
        let active = fixture.list.sessions[0].id.clone();
        let target = fixture.list.sessions[1].id.clone();
        fixture.list.sessions[0].kind = AgentKind::CODEX;
        fixture.list.sessions[1].kind = AgentKind::CLAUDE_CODE;
        fixture.list.sessions[0].foreground_agent = None;
        fixture.list.sessions[1].foreground_agent = None;
        fixture.list.sessions[1].host = Some("build-box".to_owned());
        fixture.list.sessions[1].hibernation = Some(diri_proto::HibernationInfo {
            since: diri_proto::DateMillis(1.0),
            reason: diri_proto::HibernationReason::Manual,
            tree_pids: vec![42],
            tree_start_times: None,
        });
        {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.hydrate(fixture.list);
            store.select(active.clone());
        }
        let tokio = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let (usage_tx, _) = tokio::sync::watch::channel(UsageSnapshot::default());
        let services = Arc::new(AppServices {
            store: Arc::clone(&runtime),
            usage_tx,
            updates: crate::updates::inert(),
            tokio,
            dev_build: None,
            #[cfg(unix)]
            daemon_startup: None,
        });
        let (launcher, cx) =
            cx.add_window_view(move |_window, cx| LauncherOverlay::new(services, true, cx));

        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open_for_session(target.clone(), "first quoted turn", None, window, cx);
            launcher.open_for_session(target.clone(), "second quoted turn", None, window, cx);
        });

        assert_eq!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .selected_session_id(),
            Some(&active),
            "staging a different target must not switch the active session"
        );
        assert!(
            runtime
                .store
                .read()
                .expect("session store lock poisoned")
                .sessions()
                .get(&target)
                .is_some_and(|record| record.hibernation.is_some()),
            "an app-owned draft cannot wake or rewrite hibernation state"
        );
        launcher.read_with(cx, |launcher, _| {
            assert_eq!(launcher.target, LauncherTarget::Session(target.clone()));
            assert_eq!(
                launcher.blocker(),
                None,
                "plain quoted text must remain submittable to a remote agent"
            );
            assert_eq!(
                launcher.prompt.text(),
                "first quoted turn\nsecond quoted turn"
            );
            assert!(launcher.open);
        });

        launcher.update_in(cx, |launcher, window, cx| {
            launcher.open_local_paths_for_session(
                target.clone(),
                "'/Users/me/local.png'",
                None,
                window,
                cx,
            );
        });
        launcher.read_with(cx, |launcher, _| {
            assert_eq!(
                launcher.blocker().as_deref(),
                Some("Local paths cannot be used on a remote session")
            );
        });
    }
}
