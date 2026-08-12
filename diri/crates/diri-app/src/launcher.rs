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
    KeyDownEvent, MouseButton, PathPromptOptions, Render, Task, Window, div, prelude::*, px, rgba,
};

use crate::AppServices;
use crate::agent_catalog::{AgentOption, quick_agent_options, title_case_id};
use crate::composer::PromptComposer;
use crate::delegation::HandoffProposal;
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::CARET;
use crate::notifications::SendTextCommand;
use crate::query_editor::{self, ClipboardEdit, Edit};
use crate::store::SpawnOptions;

const PANEL_WIDTH: f32 = 540.0;
const TITLE_HEIGHT: f32 = 36.0;
const TITLE_GAP: f32 = 22.0;
const CONTROL_SIZE: f32 = 32.0;
const CONTROL_RADIUS: f32 = 9.0;
const SHELF_HEIGHT: f32 = 40.0;
const PICKER_HEIGHT: f32 = 200.0;

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
    fallback_notice: Option<String>,
    /// Finder drops may partially succeed. Keep their ignored-path detail
    /// inline with the staged draft until the user sends or replaces it; a
    /// toast would separate the reason from its action.
    drop_notice: Option<String>,
    /// Which picker, if any, is open — and where its keyboard highlight sits,
    /// so both are reachable without the mouse.
    picker: Option<Picker>,
    highlight: usize,
    open: bool,
    preview: bool,
    _store_changes: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Harness,
    Project,
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
                                if this.open
                                    && matches!(this.target, LauncherTarget::NewSession)
                                    && matches!(this.mode, LauncherMode::NewSession)
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
            fallback_notice: None,
            drop_notice: None,
            picker: None,
            highlight: 0,
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
        if spawnable || !catalog_known {
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

    fn selected_harness_label(&self) -> String {
        self.harness_choices()
            .into_iter()
            .find(|choice| choice.kind == self.selected_harness)
            .map(|choice| choice.display_name)
            .unwrap_or_else(|| title_case_id(self.selected_harness.id()))
    }

    fn selected_project_label(&self) -> String {
        self.projects()
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
            .unwrap_or_else(|| "Choose project".to_owned())
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
        if self.selected_root.is_empty() {
            return Some("Choose a project to start in".to_owned());
        }
        let (spawnable, catalog_known, scan_error) = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let host = self.selected_host.as_deref();
            let catalog = store.agent_catalog(host);
            (
                crate::agent_catalog::kind_spawnable(&self.selected_harness, catalog),
                catalog.is_some(),
                store.agent_catalog_error(host).map(str::to_owned),
            )
        };
        if spawnable {
            return None;
        }
        if catalog_known {
            return Some(format!(
                "{} is not available on this host",
                self.selected_harness_label()
            ));
        }
        // Readiness for this target has not answered yet, so submitting would
        // spawn blind. Name the state rather than claiming absence — and leave
        // the Agent menu usable, since picking Terminal explicitly stays a
        // valid escape hatch when a scan cannot answer at all.
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
                self.services
                    .store
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .spawn_kind(
                        self.selected_harness.clone(),
                        SpawnOptions {
                            cwd: Some(self.selected_root.clone()),
                            initial_prompt: Some(prompt),
                            host: self.selected_host.clone(),
                            ..SpawnOptions::default()
                        },
                    );
                self.new_session_draft.clear();
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
            None => return false,
        };
        match event.keystroke.key.as_str() {
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
                    self.selected_harness = choice.kind.clone();
                    self.fallback_notice = None;
                }
            }
            Some(Picker::Project) => {
                let projects = self.projects();
                match project_commit(projects.len(), self.highlight) {
                    ProjectCommit::Recent(index) => {
                        self.selected_root.clone_from(&projects[index].project.root);
                        self.selected_host.clone_from(&projects[index].host);
                        self.services
                            .store
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .request_agent_catalog(projects[index].host.clone(), false);
                        self.reconcile_harness();
                        self.picker = None;
                        window.focus(&self.focus, cx);
                    }
                    ProjectCommit::ChooseFolder => {
                        self.choose_folder(window, cx);
                    }
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
        self.highlight = match picker {
            Picker::Harness => self
                .harness_choices()
                .iter()
                .position(|choice| choice.kind == self.selected_harness),
            Picker::Project => self.projects().iter().position(|project| {
                project.project.root == self.selected_root && project.host == self.selected_host
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
        self.selected_harness = choices[next].kind.clone();
        self.fallback_notice = None;
    }

    fn edit_prompt(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(edit) = query_editor::edit_for(&event.keystroke) else {
            return false;
        };
        match edit {
            Edit::Local(local) => {
                self.prompt.apply(local);
            }
            Edit::Clipboard(ClipboardEdit::Copy) => {
                query_editor::copy_selection(self.prompt.editor(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Cut) => {
                query_editor::cut_selection(self.prompt.editor_mut(), cx);
            }
            Edit::Clipboard(ClipboardEdit::Paste) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
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
                    this.selected_host = None;
                    this.services
                        .store
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .request_agent_catalog(None, false);
                    this.reconcile_harness();
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
                        this.selected_root.clone_from(&root);
                        this.selected_host.clone_from(&host);
                        this.services
                            .store
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .request_agent_catalog(host.clone(), false);
                        this.reconcile_harness();
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
                                    .child(project.project.root),
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

    fn render_panel(
        &self,
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
        let text_height = composer_text_height(self.prompt.line_count());
        let composer_height = text_height + COMPOSER_CONTROLS_HEIGHT;
        // The pickers hang off the bottom of the panel, which now moves with
        // the composer.
        let picker_top = TITLE_HEIGHT + TITLE_GAP + composer_height + SHELF_HEIGHT + 8.0;
        let blocker = self.blocker();
        let harness_label = self.selected_harness_label();
        let project_label = self.selected_project_label();
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
            .child(
                div()
                    .h(px(TITLE_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
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
                            .id("launcher-new-project")
                            .h(px(CONTROL_SIZE - 2.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(CONTROL_RADIUS - 1.0))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                            .active(move |button| button.bg(colors.primary.alpha(0.11)))
                            .child(sf_symbol("plus", 9.0, colors.tertiary))
                            .child("Choose folder…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_folder(window, cx);
                            })),
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
            .child(self.render_panel(colors, focused, cx))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
    use crate::store::StoreRuntime;
    use crate::usage::UsageSnapshot;
    use gpui::TestAppContext;

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
