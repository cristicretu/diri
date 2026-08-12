//! Compact new-session destination opened in the main pane by Command-N.

use std::path::Path;
use std::sync::Arc;

use diri_proto::{AgentKind, Project};
use diri_ui::{
    AgentKind as UiAgentKind, AgentLogo, Fill, FloatingSurface, Palette, Radius, SemanticColors,
};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyDownEvent, MouseButton, PathPromptOptions, Render, Task, Window, div, prelude::*, px, rgba,
};

use crate::AppServices;
use crate::agent_catalog::{AgentOption, quick_agent_options, title_case_id};
use crate::composer::PromptComposer;
use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::CARET;
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
    selected_harness: AgentKind,
    selected_root: String,
    selected_host: Option<String>,
    fallback_notice: Option<String>,
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
                                if this.open {
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
            selected_harness,
            selected_root,
            selected_host,
            fallback_notice: None,
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

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.picker = None;
        cx.emit(LauncherEvent::Closed);
        cx.notify();
    }

    /// Close from outside the launcher (sidebar session click, menu bar, etc.).
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.close(cx);
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
    /// usable on a target where that Agent is absent. It runs only after a
    /// catalog exists, so an in-flight remote scan never causes a false
    /// fallback or visual jump; and it judges by installed state, not quick
    /// create visibility, so a hidden-but-installed default is not switched
    /// away from.
    fn reconcile_harness(&mut self) {
        let spawnable = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let catalog = store.agent_catalog(self.selected_host.as_deref());
            if catalog.is_none() {
                return;
            }
            crate::agent_catalog::kind_spawnable(&self.selected_harness, catalog)
        };
        if spawnable {
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
        if self.selected_root.is_empty() {
            return Some("Choose a project to start in".to_owned());
        }
        let spawnable = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            let catalog = store.agent_catalog(self.selected_host.as_deref());
            // No catalog means a scan is in flight or failed, not that the
            // Agent is absent: submitting stays possible and the daemon's
            // spawn-time check remains the authority. Claiming "not
            // available" here would block ⌘↵ for the length of an SSH scan —
            // or forever, when the scan errored.
            crate::agent_catalog::kind_spawnable(&self.selected_harness, catalog)
        };
        if spawnable {
            None
        } else {
            Some(format!(
                "{} is not available on this host",
                self.selected_harness_label()
            ))
        }
    }

    fn can_submit(&self) -> bool {
        !self.preview && !self.prompt.text().trim().is_empty() && self.blocker().is_none()
    }

    fn submit(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_submit() {
            return false;
        }
        let prompt = self.prompt.text().trim().to_owned();
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
        self.prompt.clear();
        self.close(cx);
        true
    }

    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
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
            "tab" => {
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
        let composer_fill = if colors.appearance == diri_ui::Appearance::Dark {
            rgba(0x26282dff)
        } else {
            rgba(0xf2f1efff)
        };
        let shelf_fill = if colors.appearance == diri_ui::Appearance::Dark {
            rgba(0x1d1f23ff)
        } else {
            rgba(0xe8e7e4ff)
        };

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
                    .bg(shelf_fill)
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
            });

        panel.into_any_element()
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

        // The session workbench is intentionally always dark, independent of
        // macOS appearance. This is a destination in that workbench—not a
        // translucent window overlay—so paint the same fully opaque surface.
        let colors = SemanticColors::dark();
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

    #[test]
    fn manifest_ids_have_readable_fallback_labels() {
        assert_eq!(title_case_id("claude-code"), "Claude Code");
        assert_eq!(title_case_id("open_code"), "Open Code");
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
}
