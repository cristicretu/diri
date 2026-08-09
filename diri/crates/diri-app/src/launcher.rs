//! Compact new-session destination opened in the main pane by Command-N.

use std::path::Path;
use std::sync::Arc;

use diri_proto::{AgentKind, Project};
use diri_ui::{
    AgentKind as UiAgentKind, AgentLogo, Fill, FloatingSurface, Palette, Radius, SemanticColors,
};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    MouseButton, PathPromptOptions, Render, Task, Window, div, prelude::*, px, rgba,
};

use crate::AppServices;
use crate::macos::sf_symbols::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::query_label;
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};
use crate::store::SpawnOptions;

const PANEL_WIDTH: f32 = 540.0;
const TITLE_HEIGHT: f32 = 36.0;
const TITLE_GAP: f32 = 22.0;
const COMPOSER_HEIGHT: f32 = 108.0;
const COMPOSER_TEXT_HEIGHT: f32 = 64.0;
const CONTROL_SIZE: f32 = 32.0;
const CONTROL_RADIUS: f32 = 9.0;
const SHELF_HEIGHT: f32 = 40.0;
const PICKER_HEIGHT: f32 = 200.0;

#[derive(Clone)]
struct HarnessChoice {
    kind: AgentKind,
    label: String,
    available: bool,
}

pub(crate) enum LauncherEvent {
    Closed,
}

pub(crate) struct LauncherOverlay {
    services: Arc<AppServices>,
    focus: FocusHandle,
    prompt: QueryEditor,
    selected_harness: AgentKind,
    selected_root: String,
    harness_picker_open: bool,
    project_picker_open: bool,
    open: bool,
    preview: bool,
    _store_changes: Task<()>,
}

impl EventEmitter<LauncherEvent> for LauncherOverlay {}

impl LauncherOverlay {
    pub(crate) fn new(services: Arc<AppServices>, preview: bool, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let (selected_harness, selected_root) = initial_target(&services);
        let mut changes = services.store.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
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
            prompt: QueryEditor::default(),
            selected_harness,
            selected_root,
            harness_picker_open: false,
            project_picker_open: false,
            open: false,
            preview,
            _store_changes: store_changes,
        }
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (harness, root) = initial_target(&self.services);
        self.selected_harness = harness;
        self.selected_root = root;
        self.prompt.clear();
        self.harness_picker_open = false;
        self.project_picker_open = false;
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
        self.harness_picker_open = false;
        self.project_picker_open = false;
        cx.emit(LauncherEvent::Closed);
        cx.notify();
    }

    fn harness_choices(&self) -> Vec<HarnessChoice> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        let catalog = &store.agent_catalog().agents;
        if catalog.is_empty() {
            return [
                (AgentKind::CLAUDE_CODE, "Claude Code"),
                (AgentKind::CODEX, "Codex"),
                (AgentKind::CURSOR, "Cursor"),
                (AgentKind::GEMINI, "Gemini"),
            ]
            .into_iter()
            .map(|(kind, label)| HarnessChoice {
                kind,
                label: label.to_owned(),
                available: true,
            })
            .collect();
        }

        catalog
            .iter()
            .filter(|item| !item.kind.is_terminal())
            .map(|item| HarnessChoice {
                kind: item.kind.clone(),
                label: item
                    .descriptor
                    .as_ref()
                    .map(|descriptor| descriptor.display_name.clone())
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| title_case_id(item.kind.id())),
                available: item.available(),
            })
            .collect()
    }

    fn projects(&self) -> Vec<Project> {
        let store = self
            .services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        let mut projects: Vec<_> = store.projects().values().cloned().collect();
        projects.sort_by(|left, right| {
            left.pinned_order
                .unwrap_or(i64::MAX)
                .cmp(&right.pinned_order.unwrap_or(i64::MAX))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        projects
    }

    fn selected_harness_label(&self) -> String {
        self.harness_choices()
            .into_iter()
            .find(|choice| choice.kind == self.selected_harness)
            .map(|choice| choice.label)
            .unwrap_or_else(|| title_case_id(self.selected_harness.id()))
    }

    fn selected_project_label(&self) -> String {
        self.projects()
            .into_iter()
            .find(|project| project.root == self.selected_root)
            .map(|project| project.name)
            .or_else(|| {
                Path::new(&self.selected_root)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Choose project".to_owned())
    }

    fn can_submit(&self) -> bool {
        !self.preview
            && !self.prompt.text().trim().is_empty()
            && !self.selected_root.is_empty()
            && self
                .harness_choices()
                .iter()
                .any(|choice| choice.kind == self.selected_harness && choice.available)
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
                    ..SpawnOptions::default()
                },
            );
        self.close(cx);
        true
    }

    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match event.keystroke.key.as_str() {
            "escape" if self.harness_picker_open || self.project_picker_open => {
                self.harness_picker_open = false;
                self.project_picker_open = false;
                cx.notify();
                true
            }
            "escape" => {
                self.close(cx);
                true
            }
            "enter" if event.keystroke.modifiers.shift => {
                self.prompt.insert_multiline("\n");
                cx.notify();
                true
            }
            "enter" => self.submit(cx),
            _ => self.edit_prompt(event, cx),
        }
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
                query_editor::copy_selection(&self.prompt, cx);
            }
            Edit::Clipboard(ClipboardEdit::Cut) => {
                query_editor::cut_selection(&mut self.prompt, cx);
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
        self.project_picker_open = false;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Start Here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.pop() else {
                return;
            };
            let _ = this.update_in(cx, |this, _window, cx| {
                this.selected_root = path.to_string_lossy().into_owned();
                cx.notify();
            });
        })
        .detach();
    }

    fn render_harness_picker(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("launcher-harness-list")
            .py(px(6.0))
            .w(px(260.0))
            .max_h(px(PICKER_HEIGHT))
            .overflow_y_scroll();
        for (index, choice) in self.harness_choices().into_iter().enumerate() {
            let selected = choice.kind == self.selected_harness;
            let enabled = choice.available;
            let kind = choice.kind.clone();
            let logo = ui_agent_kind(&choice.kind);
            list = list.child(
                div()
                    .id(format!("launcher-harness-{index}"))
                    .mx(px(6.0))
                    .h(px(38.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(8.0))
                    .text_size(px(12.0))
                    .text_color(if enabled {
                        colors.primary
                    } else {
                        colors.tertiary
                    })
                    .when(enabled, |row| {
                        row.cursor_pointer()
                            .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_harness = kind.clone();
                                this.harness_picker_open = false;
                                cx.notify();
                            }))
                    })
                    .child(AgentLogo::new(logo, 21.0, colors))
                    .child(div().flex_1().child(choice.label))
                    .when(!enabled, |row| {
                        row.child(
                            div()
                                .text_size(px(9.0))
                                .text_color(colors.tertiary)
                                .child("Unavailable"),
                        )
                    })
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
        if projects.is_empty() {
            list = list.child(
                div()
                    .h(px(38.0))
                    .px(px(11.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child("No recent projects"),
            );
        }
        for (index, project) in projects.into_iter().enumerate() {
            let selected = project.root == self.selected_root;
            let root = project.root.clone();
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
                    .hover(move |row| row.bg(colors.primary.alpha(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_root.clone_from(&root);
                        this.project_picker_open = false;
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
                                    .child(project.name),
                            )
                            .child(
                                div()
                                    .text_size(px(9.0))
                                    .text_color(colors.tertiary)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(project.root),
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
        FloatingSurface::new(colors, list).into_any_element()
    }

    fn render_panel(
        &self,
        colors: SemanticColors,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let can_submit = self.can_submit();
        let harness_open = self.harness_picker_open;
        let project_open = self.project_picker_open;
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

        let prompt = if self.prompt.is_empty() {
            div()
                .flex()
                .items_center()
                .when(focused, |line| {
                    line.child(
                        div()
                            .text_color(colors.primary.alpha(0.92))
                            .child(query_label(&self.prompt)),
                    )
                })
                .child(
                    div()
                        .text_color(colors.tertiary)
                        .child("Describe the task…"),
                )
                .into_any_element()
        } else {
            query_label(&self.prompt)
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
                    .mx(px(8.0))
                    .h(px(COMPOSER_HEIGHT))
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
                            .h(px(COMPOSER_TEXT_HEIGHT))
                            .px(px(16.0))
                            .pt(px(12.0))
                            .overflow_hidden()
                            .text_size(px(13.0))
                            .line_height(px(19.0))
                            .text_color(colors.primary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .h(px(COMPOSER_HEIGHT - COMPOSER_TEXT_HEIGHT))
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
                                            .size(px(CONTROL_SIZE))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(CONTROL_RADIUS))
                                            .cursor_pointer()
                                            .hover(move |button| button.bg(Fill::subtle(colors)))
                                            .active(move |button| {
                                                button.bg(colors.primary.alpha(0.10))
                                            })
                                            .child(sf_symbol("plus", 11.0, colors.secondary))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.choose_folder(window, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(colors.tertiary)
                                            .child("⇧↵  New line"),
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
                                                this.harness_picker_open =
                                                    !this.harness_picker_open;
                                                this.project_picker_open = false;
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
                                this.project_picker_open = !this.project_picker_open;
                                this.harness_picker_open = false;
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
                            .child("New project")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_folder(window, cx);
                            })),
                    ),
            )
            .when(harness_open, |panel| {
                panel.child(
                    div()
                        .absolute()
                        .right(px(8.0))
                        .top(px(TITLE_HEIGHT
                            + TITLE_GAP
                            + COMPOSER_HEIGHT
                            + SHELF_HEIGHT
                            + 8.0))
                        .child(self.render_harness_picker(colors, cx)),
                )
            })
            .when(project_open, |panel| {
                panel.child(
                    div()
                        .absolute()
                        .left(px(8.0))
                        .top(px(TITLE_HEIGHT
                            + TITLE_GAP
                            + COMPOSER_HEIGHT
                            + SHELF_HEIGHT
                            + 8.0))
                        .child(self.render_project_picker(colors, cx)),
                )
            });

        panel.into_any_element()
    }
}

impl Focusable for LauncherOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LauncherOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // The session workbench is intentionally always dark, independent of
        // macOS appearance. This is a destination in that workbench—not a
        // translucent window overlay—so paint the same fully opaque surface.
        let colors = SemanticColors::dark();
        let focused = self.focus.is_focused(_window);
        let focus = self.focus.clone();
        root.size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.background)
            // The entire empty workbench behaves like the editor's canvas.
            // This also recovers focus after a project/harness popover click.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&focus, cx);
            })
            // Command-N is a high-frequency keyboard action; the destination
            // appears immediately rather than making the user wait on motion.
            .child(self.render_panel(colors, focused, cx))
    }
}

fn initial_target(services: &AppServices) -> (AgentKind, String) {
    let store = services
        .store
        .store
        .read()
        .expect("session store lock poisoned");
    let selected_root = store
        .selected_session()
        .and_then(|session| store.projects().get(&session.project_id))
        .map(|project| project.root.clone())
        .or_else(|| {
            store
                .projects()
                .values()
                .min_by(|left, right| left.name.cmp(&right.name))
                .map(|project| project.root.clone())
        })
        .unwrap_or_default();
    (store.preferences().default_agent.kind(), selected_root)
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

fn title_case_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_ids_have_readable_fallback_labels() {
        assert_eq!(title_case_id("claude-code"), "Claude Code");
        assert_eq!(title_case_id("open_code"), "Open Code");
    }
}
