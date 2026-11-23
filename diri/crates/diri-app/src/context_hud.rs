//! Capture Context: the global ⇧⌘2 shortcut, the capture itself, and the
//! floating dispatch HUD that sends the clip on without switching windows.
//!
//! The flow: read the front app on the main thread → capture and extract
//! text on a background thread → `clip.save` (the Engine copies the PNG into
//! its owner-only ring) → open the HUD with the clip and the Session the
//! page's feedback went to last preselected → `clip.dispatch` on ↩.
//!
//! The HUD is a non-activating key panel over whatever app is in front: it
//! takes keys without raising Diri's windows, and Escape or clicking away
//! closes it. The clip is already in the Context Ring by then, so closing
//! never loses it. Nothing runs before the shortcut is pressed.

use std::sync::Arc;
use std::time::Duration;

use diri_proto::clip::{
    ClipRecord, ClipTarget, ClipTargetReason, ClipTargetsResult, ConversationPreview,
};
use diri_proto::{AgentKind, Method, SessionStatus};
use diri_ui::{Fill, Glass, Palette, Radius, SemanticColors, Typo};
use gpui::{
    App, AppContext as _, Context, StyledImage as _, Div, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Window, div, prelude::FluentBuilder,
    px,
};

use crate::i18n::{t, tf};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};

pub(crate) const HUD_WIDTH: f32 = 560.0;
pub(crate) const HUD_HEIGHT: f32 = 468.0;
const THUMB_W: f32 = 132.0;
const THUMB_H: f32 = 84.0;
/// Rows shown at once; the list scrolls past them with the highlight.
const VISIBLE_ROWS: usize = 5;
/// Agents offered for a new Session, the person's default first.
const NEW_AGENT_KINDS: [&str; 2] = [AgentKind::CLAUDE_CODE_ID, AgentKind::CODEX_ID];

/// Where ↩ sends the clip.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Row {
    Target(usize),
    NewSession,
    Ring,
}

/// Which field typing goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Prompt,
    Filter,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Phase {
    Ready,
    Sending,
    Sent(String),
    Failed(String),
}

/// Everything the HUD shows. Rendering is a pure function of this and the
/// theme, so the visual fixtures render it without an Engine.
#[derive(Clone, Debug)]
pub(crate) struct HudModel {
    pub clip: ClipRecord,
    pub targets: Vec<ClipTarget>,
    pub loading: bool,
    pub prompt: QueryEditor,
    pub filter: QueryEditor,
    pub field: Field,
    pub highlight: usize,
    /// Index into the new-Session agent kinds.
    pub new_kind: usize,
    /// Where a new Session starts, and its project's name.
    pub new_cwd: Option<(String, String)>,
    pub phase: Phase,
}

impl HudModel {
    pub(crate) fn new(clip: ClipRecord, new_cwd: Option<(String, String)>) -> Self {
        Self {
            clip,
            targets: Vec::new(),
            loading: true,
            prompt: QueryEditor::default(),
            filter: QueryEditor::default(),
            field: Field::Prompt,
            highlight: 0,
            new_kind: 0,
            new_cwd,
            phase: Phase::Ready,
        }
    }

    pub(crate) fn set_targets(&mut self, targets: Vec<ClipTarget>) {
        self.targets = targets;
        self.loading = false;
        self.highlight = 0;
    }

    /// Targets matching the filter, best first, by index into `targets`.
    pub(crate) fn visible_targets(&self) -> Vec<usize> {
        let needle = self.filter.text().trim().to_lowercase();
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, target)| {
                needle.is_empty()
                    || target.title.to_lowercase().contains(&needle)
                    || target
                        .project
                        .as_deref()
                        .is_some_and(|p| p.to_lowercase().contains(&needle))
                    || kind_label(&target.kind).to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self.visible_targets().into_iter().map(Row::Target).collect();
        if self.new_cwd.is_some() {
            rows.push(Row::NewSession);
        }
        rows.push(Row::Ring);
        rows
    }

    pub(crate) fn highlighted(&self) -> Row {
        let rows = self.rows();
        rows.get(self.highlight.min(rows.len() - 1))
            .cloned()
            .unwrap_or(Row::Ring)
    }

    pub(crate) fn move_highlight(&mut self, delta: isize) {
        let count = self.rows().len() as isize;
        self.highlight = (self.highlight as isize + delta).rem_euclid(count) as usize;
    }

    pub(crate) fn new_kind(&self) -> AgentKind {
        AgentKind::new(NEW_AGENT_KINDS[self.new_kind % NEW_AGENT_KINDS.len()])
    }

    pub(crate) fn cycle_new_kind(&mut self, delta: isize) {
        let count = NEW_AGENT_KINDS.len() as isize;
        self.new_kind = (self.new_kind as isize + delta).rem_euclid(count) as usize;
    }

    /// The `clip.dispatch` destination for the highlighted row.
    pub(crate) fn destination(&self) -> serde_json::Value {
        match self.highlighted() {
            Row::Target(index) => serde_json::json!({
                "type": "session",
                "sessionID": self.targets[index].session_id.0,
            }),
            Row::NewSession => serde_json::json!({
                "type": "new",
                "kind": self.new_kind(),
                "cwd": self.new_cwd.as_ref().map(|(cwd, _)| cwd.clone()).unwrap_or_default(),
            }),
            Row::Ring => serde_json::json!({"type": "ring"}),
        }
    }
}

pub(crate) fn kind_label(kind: &AgentKind) -> String {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "Claude Code".to_owned(),
        AgentKind::CODEX_ID => "Codex".to_owned(),
        AgentKind::CURSOR_ID => "Cursor".to_owned(),
        AgentKind::GEMINI_ID => "Gemini".to_owned(),
        AgentKind::SHELL_ID => t("context_hud.kind.terminal").to_owned(),
        other => other.to_owned(),
    }
}

fn status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => t("context_hud.status.starting"),
        SessionStatus::Idle => t("context_hud.status.idle"),
        SessionStatus::Working => t("context_hud.status.working"),
        SessionStatus::NeedsInput(_) => t("context_hud.status.needs_input"),
        SessionStatus::Exited(_) => t("context_hud.status.exited"),
        SessionStatus::Unknown => "",
    }
}

fn reason_label(reason: ClipTargetReason) -> Option<&'static str> {
    match reason {
        ClipTargetReason::Remembered => Some(t("context_hud.reason.remembered")),
        ClipTargetReason::ServesPort => Some(t("context_hud.reason.serves_port")),
        ClipTargetReason::Recent => None,
    }
}

fn host_and_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    crate::context_hud::ellipsize(rest.trim_end_matches('/'), 64)
}

pub(crate) fn ellipsize(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

fn key_cap(label: &str, colors: SemanticColors) -> Div {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(Radius::CHIP))
        .bg(Fill::subtle(colors))
        .text_size(px(Typo::META.size))
        .text_color(colors.secondary)
        .child(label.to_owned())
}

fn hint(keys: &str, label: &str, colors: SemanticColors) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(key_cap(keys, colors))
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(colors.secondary)
                .child(label.to_owned()),
        )
}

fn header(model: &HudModel, colors: SemanticColors) -> Div {
    let clip = &model.clip;
    let thumbnail = match clip.png_path.as_deref() {
        Some(path) => gpui::img(std::path::PathBuf::from(path))
            .w(px(THUMB_W))
            .h(px(THUMB_H))
            .object_fit(gpui::ObjectFit::Cover)
            .into_any_element(),
        None => div().into_any_element(),
    };
    let subtitle = clip
        .source
        .url
        .as_deref()
        .map(host_and_path)
        .or_else(|| clip.source.window_title.clone())
        .unwrap_or_default();
    let excerpt = clip.text.as_deref().map(|text| ellipsize(text, 150));
    div()
        .flex()
        .gap(px(12.0))
        .p(px(12.0))
        .child(
            div()
                .flex_none()
                .w(px(THUMB_W))
                .h(px(THUMB_H))
                .rounded(px(Radius::ROW))
                .overflow_hidden()
                .border_1()
                .border_color(colors.floating_stroke())
                .bg(Fill::subtle(colors))
                .child(thumbnail),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div().flex().child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(Radius::BADGE))
                            .bg(Palette::CLAY.alpha(0.16))
                            .text_color(colors.primary)
                            .text_size(px(Typo::META.size))
                            .font_weight(Typo::TITLE.weight)
                            .child(clip.summary()),
                    ),
                )
                .when(!subtitle.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.secondary)
                            .truncate()
                            .child(subtitle),
                    )
                })
                .when_some(excerpt, |d, excerpt| {
                    d.child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .line_clamp(2)
                            .child(excerpt),
                    )
                }),
        )
}

fn field(
    editor: &QueryEditor,
    placeholder: &str,
    icon: &'static str,
    active: bool,
    colors: SemanticColors,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .h(px(36.0))
        .child(crate::icons::sf_symbol(icon, 13.0, colors.secondary))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_size(px(14.0))
                .text_color(if editor.is_empty() {
                    colors.tertiary
                } else {
                    colors.primary
                })
                .child(if editor.is_empty() {
                    if active {
                        div()
                            .flex()
                            .child(div().w(px(1.5)).h(px(16.0)).bg(Palette::CLAY))
                            .child(div().pl(px(2.0)).child(placeholder.to_owned()))
                            .into_any_element()
                    } else {
                        div().child(placeholder.to_owned()).into_any_element()
                    }
                } else if active {
                    crate::navigation::query_label(editor)
                } else {
                    div().child(editor.text().to_owned()).into_any_element()
                }),
        )
}

fn preview_lines(preview: &ConversationPreview, colors: SemanticColors) -> Div {
    let line = |who: &str, text: &str| {
        div()
            .flex()
            .gap(px(6.0))
            .text_size(px(Typo::META.size))
            .child(
                div()
                    .flex_none()
                    .text_color(colors.tertiary)
                    .child(who.to_owned()),
            )
            .child(
                div()
                    .min_w_0()
                    .text_color(colors.secondary)
                    .truncate()
                    .child(text.to_owned()),
            )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pl(px(18.0))
        .pt(px(3.0))
        .when_some(preview.user.as_deref(), |d, text| {
            d.child(line(t("context_hud.preview.you"), text))
        })
        .when_some(preview.agent.as_deref(), |d, text| {
            d.child(line(t("context_hud.preview.agent"), text))
        })
}

fn row_shell(selected: bool, colors: SemanticColors) -> Div {
    div()
        .mx(px(6.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(Radius::ROW))
        .when(selected, |d| d.bg(Fill::selected(colors, true)))
}

fn target_row(target: &ClipTarget, selected: bool, colors: SemanticColors) -> Div {
    let remembered = reason_label(target.reason);
    let mut meta: Vec<String> = vec![kind_label(&target.kind)];
    let status = status_label(&target.status);
    if !status.is_empty() {
        meta.push(status.to_owned());
    }
    let dot = match target.status {
        SessionStatus::Working => Palette::CLAY,
        SessionStatus::NeedsInput(_) => colors.primary,
        _ => colors.tertiary,
    };
    row_shell(selected, colors)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_none().size(px(7.0)).rounded_full().bg(dot))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(Typo::ROW.size))
                        .font_weight(if selected {
                            Typo::ROW_EMPHASIZED.weight
                        } else {
                            Typo::ROW.weight
                        })
                        .child(ellipsize(&target.title, 48)),
                )
                .when_some(target.project.clone(), |d, project| {
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(project),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .gap(px(4.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.secondary)
                        .when_some(remembered, |d, reason| {
                            let color = if target.reason == ClipTargetReason::Remembered {
                                Palette::CLAY
                            } else {
                                colors.secondary
                            };
                            d.child(div().text_color(color).child(reason.to_owned()))
                                .child("·")
                        })
                        .child(meta.join(" · ")),
                ),
        )
        .when(selected && !target.preview.is_empty(), |d| {
            d.child(preview_lines(&target.preview, colors))
        })
}

fn action_row(icon: &'static str, label: String, extra: Option<String>, selected: bool, colors: SemanticColors) -> Div {
    row_shell(selected, colors).child(
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(crate::icons::sf_symbol(icon, 12.0, colors.secondary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(Typo::ROW.size))
                    .child(label),
            )
            .when_some(extra, |d, extra| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(extra),
                )
            }),
    )
}

fn divider(colors: SemanticColors) -> Div {
    div().h(px(1.0)).w_full().bg(colors.floating_stroke())
}

/// The whole HUD for `model` in `colors`.
pub(crate) fn hud_view(model: &HudModel, colors: SemanticColors) -> Div {
    let rows = model.rows();
    let highlight = model.highlight.min(rows.len().saturating_sub(1));
    // Keep the highlight in view: a window of VISIBLE_ROWS around it.
    let first = highlight.saturating_sub(VISIBLE_ROWS - 1);
    let list = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(VISIBLE_ROWS)
        .map(|(index, row)| {
            let selected = index == highlight;
            match row {
                Row::Target(target) => target_row(&model.targets[*target], selected, colors),
                Row::NewSession => {
                    let project = model
                        .new_cwd
                        .as_ref()
                        .map(|(_, name)| name.as_str())
                        .unwrap_or_default();
                    action_row(
                        "plus",
                        tf(
                            "context_hud.new_session",
                            &[("agent", &kind_label(&model.new_kind())), ("project", &project)],
                        ),
                        selected.then(|| t("context_hud.new_session_cycle").to_owned()),
                        selected,
                        colors,
                    )
                }
                Row::Ring => action_row(
                    "archivebox",
                    t("context_hud.save_to_ring").to_owned(),
                    None,
                    selected,
                    colors,
                ),
            }
        })
        .collect::<Vec<_>>();
    let status: Option<(String, gpui::Rgba)> = match &model.phase {
        Phase::Ready if model.loading => Some((t("context_hud.loading").to_owned(), colors.tertiary)),
        Phase::Ready => None,
        Phase::Sending => Some((t("context_hud.sending").to_owned(), colors.secondary)),
        Phase::Sent(to) => Some((tf("context_hud.sent", &[("target", to)]), Palette::CLAY)),
        Phase::Failed(error) => Some((error.clone(), colors.primary)),
    };
    div()
        .w(px(HUD_WIDTH))
        .h(px(HUD_HEIGHT))
        .flex()
        .flex_col()
        .rounded(px(Radius::FLOATING_MENU))
        .overflow_hidden()
        .text_color(colors.primary)
        .text_size(px(Typo::ROW.size))
        .bg(Glass::panel_fill(colors))
        .border_1()
        .border_color(colors.floating_stroke())
        .child(header(model, colors))
        .child(divider(colors))
        .child(field(
            &model.prompt,
            t("context_hud.prompt_placeholder"),
            "pencil",
            model.field == Field::Prompt,
            colors,
        ))
        .child(divider(colors))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(px(14.0))
                .pt(px(8.0))
                .pb(px(4.0))
                .child(
                    div()
                        .text_size(px(Typo::SECTION_HEADER.size))
                        .font_weight(Typo::SECTION_HEADER.weight)
                        .text_color(colors.tertiary)
                        .child(t("context_hud.send_to").to_owned()),
                )
                .child(
                    div()
                        .w(px(200.0))
                        .rounded(px(Radius::CHIP))
                        .when(model.field == Field::Filter, |d| d.bg(Fill::subtle(colors)))
                        .child(
                            field(
                                &model.filter,
                                t("context_hud.filter_placeholder"),
                                "magnifyingglass",
                                model.field == Field::Filter,
                                colors,
                            )
                            .h(px(24.0))
                            .px(px(6.0)),
                        ),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(list),
        )
        .child(divider(colors))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(14.0))
                .px(px(14.0))
                .h(px(34.0))
                .child(hint("↩", t("context_hud.hint.send"), colors))
                .child(hint("⌘↩", t("context_hud.hint.save"), colors))
                .child(hint("⇥", t("context_hud.hint.find"), colors))
                .child(hint("esc", t("context_hud.hint.close"), colors))
                .child(div().flex_1())
                .when_some(status, |d, (text, color)| {
                    d.child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(color)
                            .truncate()
                            .child(text),
                    )
                }),
        )
}

/// The live HUD window's root view.
pub(crate) struct ContextHud {
    pub model: HudModel,
    colors: SemanticColors,
    focus: FocusHandle,
    services: Option<HudServices>,
}

#[derive(Clone)]
pub(crate) struct HudServices {
    pub store: Arc<crate::store::StoreRuntime>,
    pub tokio: Arc<tokio::runtime::Runtime>,
}

impl ContextHud {
    pub(crate) fn new(
        model: HudModel,
        colors: SemanticColors,
        services: Option<HudServices>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            model,
            colors,
            focus: cx.focus_handle(),
            services,
        }
    }

    pub(crate) fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn close(&mut self, window: &mut Window) {
        window.remove_window();
    }

    fn editor(&mut self) -> &mut QueryEditor {
        match self.model.field {
            Field::Prompt => &mut self.model.prompt,
            Field::Filter => &mut self.model.filter,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if matches!(self.model.phase, Phase::Sending | Phase::Sent(_)) {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "escape" if self.model.field == Field::Filter && !self.model.filter.is_empty() => {
                self.model.filter.clear();
                self.model.highlight = 0;
            }
            "escape" => return self.close(window),
            "tab" => {
                self.model.field = match self.model.field {
                    Field::Prompt => Field::Filter,
                    Field::Filter => Field::Prompt,
                };
            }
            "up" => self.model.move_highlight(-1),
            "down" => self.model.move_highlight(1),
            "p" if modifiers.control => self.model.move_highlight(-1),
            "n" if modifiers.control => self.model.move_highlight(1),
            "left" | "right"
                if self.model.highlighted() == Row::NewSession && self.editor().is_empty() =>
            {
                self.model
                    .cycle_new_kind(if event.keystroke.key == "left" { -1 } else { 1 });
            }
            "enter" if modifiers.platform => {
                self.model.highlight = self.model.rows().len() - 1;
                self.dispatch(window, cx);
            }
            "enter" => self.dispatch(window, cx),
            _ => {
                if let Some(edit) = query_editor::edit_for(&event.keystroke) {
                    let filter = self.model.field == Field::Filter;
                    let editor = self.editor();
                    let changed = match edit {
                        Edit::Local(local) => editor.apply(local),
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(editor, cx);
                            false
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(editor, cx)
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => cx
                            .read_from_clipboard()
                            .and_then(|item| item.text())
                            .is_some_and(|text| editor.insert(&text)),
                    };
                    if changed && filter {
                        self.model.highlight = 0;
                    }
                }
            }
        }
        cx.notify();
    }

    fn dispatch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(services) = self.services.clone() else {
            return;
        };
        let target = match self.model.highlighted() {
            Row::Target(index) => self.model.targets[index].title.clone(),
            Row::NewSession => kind_label(&self.model.new_kind()),
            Row::Ring => t("context_hud.ring_name").to_owned(),
        };
        let params = serde_json::json!({
            "id": self.model.clip.id,
            "prompt": self.model.prompt.text(),
            "destination": self.model.destination(),
        });
        self.model.phase = Phase::Sending;
        cx.notify();
        let client = Arc::clone(services.store.client());
        let handle = window.window_handle();
        cx.spawn_in(window, async move |this, cx| {
            let result = services
                .tokio
                .spawn(async move {
                    client
                        .request(Method::CLIP_DISPATCH, Some(&params), Some(Duration::from_secs(30)))
                        .await
                })
                .await;
            let failed = match result {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some(t("context_hud.failed").to_owned()),
            };
            let sent = failed.is_none();
            let _ = this.update(cx, |hud, cx| {
                hud.model.phase = match failed {
                    None => Phase::Sent(target),
                    Some(error) => Phase::Failed(error),
                };
                cx.notify();
            });
            if sent {
                // A beat to read "Sent to …", then out of the way.
                cx.background_executor()
                    .timer(Duration::from_millis(550))
                    .await;
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        })
        .detach();
    }

    /// Loads ranked targets with previews for the clip.
    pub(crate) fn load_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(services) = self.services.clone() else {
            return;
        };
        let client = Arc::clone(services.store.client());
        let params = serde_json::json!({"id": self.model.clip.id});
        cx.spawn_in(window, async move |this, cx| {
            let result = services
                .tokio
                .spawn(async move {
                    client
                        .request(Method::CLIP_TARGETS, Some(&params), Some(Duration::from_secs(10)))
                        .await
                })
                .await;
            let targets = result
                .ok()
                .and_then(Result::ok)
                .and_then(|value| serde_json::from_value::<ClipTargetsResult>(value).ok())
                .map(|result| result.targets)
                .unwrap_or_default();
            let _ = this.update(cx, |hud, cx| {
                hud.model.set_targets(targets);
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for ContextHud {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("context-hud")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .child(hud_view(&self.model, self.colors))
    }
}

/// Opens the HUD for a saved clip, centred in the upper part of the screen
/// under the pointer.
pub(crate) fn open(clip: ClipRecord, services: HudServices, cx: &mut App) {
    let (colors, new_cwd) = {
        let store = services.store.store.read().expect("session store lock poisoned");
        let colors = crate::app_theme::colors_in(&store);
        let new_cwd = store
            .selected_session()
            .map(|record| {
                let cwd = record.worktree_path.clone().unwrap_or_else(|| record.cwd.clone());
                let name = store
                    .projects()
                    .get(&record.project_id)
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| {
                        cwd.rsplit('/').find(|s| !s.is_empty()).unwrap_or("~").to_owned()
                    });
                (cwd, name)
            })
            .filter(|(cwd, _)| std::path::Path::new(cwd).is_dir());
        (colors, new_cwd)
    };
    let mut model = HudModel::new(clip, new_cwd);
    let default_agent = services
        .store
        .store
        .read()
        .expect("session store lock poisoned")
        .preferences()
        .default_agent
        .clone();
    if let Some(index) = NEW_AGENT_KINDS.iter().position(|id| *id == default_agent.id()) {
        model.new_kind = index;
    }
    #[cfg(target_os = "macos")]
    let bounds = crate::macos::context_hud_panel::frame(HUD_WIDTH, HUD_HEIGHT);
    #[cfg(not(target_os = "macos"))]
    let bounds = None;
    let options = gpui::WindowOptions {
        window_bounds: bounds.map(gpui::WindowBounds::Windowed),
        titlebar: None,
        focus: true,
        show: true,
        kind: gpui::WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: gpui::WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        #[cfg(target_os = "macos")]
        crate::macos::context_hud_panel::prepare(window, Radius::FLOATING_MENU);
        let hud = cx.new(|cx| ContextHud::new(model, colors, Some(services), cx));
        hud.update(cx, |hud, cx| {
            window.focus(hud.focus_handle(), cx);
            hud.load_targets(window, cx);
            // Clicking away closes it; the clip stays in the ring.
            cx.observe_window_activation(window, |_, window, _| {
                if !window.is_window_active() {
                    window.remove_window();
                }
            })
            .detach();
        });
        hud
    });
}

/// Reopens the HUD on the newest clip in the ring, to send it again or
/// somewhere else. Does nothing when the ring is empty.
pub(crate) fn open_latest(services: HudServices, cx: &mut App) {
    let client = Arc::clone(services.store.client());
    let tokio = Arc::clone(&services.tokio);
    cx.spawn(async move |cx| {
        let clip = tokio
            .spawn(async move {
                client
                    .request(
                        Method::CLIP_GET,
                        Some(&serde_json::json!({"id": "latest"})),
                        Some(Duration::from_secs(5)),
                    )
                    .await
            })
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|value| serde_json::from_value::<ClipRecord>(value).ok());
        if let Some(clip) = clip {
            cx.update(|cx| open(clip, services, cx));
        }
    })
    .detach();
}

/// Runs a capture whenever the Engine relays a request. Woken only by
/// store changes; idles on the broadcast receiver otherwise.
pub(crate) fn watch_capture_requests(services: HudServices, cx: &mut App) {
    let mut changes = services.store.changes_without_memory();
    cx.spawn(async move |cx| {
        loop {
            match changes.recv().await {
                Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
            let request = services
                .store
                .store
                .write()
                .expect("session store lock poisoned")
                .take_capture_request();
            if let Some(window_mode) = request {
                let services = services.clone();
                cx.update(|cx| start_capture(services, window_mode, cx));
            }
        }
    })
    .detach();
}

/// What a capture became, for the person: nothing (cancelled), a HUD, or a
/// permission to grant.
pub(crate) fn start_capture(services: HudServices, window_mode: bool, cx: &mut App) {
    #[cfg(target_os = "macos")]
    {
        use crate::macos::context_capture::{self as capture, CaptureError, Mode};
        let Some(front) = capture::frontmost() else {
            return;
        };
        let scratch = std::env::temp_dir().join(format!("diri-capture-{}", unsafe { libc::getuid() }));
        let mode = if window_mode { Mode::Window } else { Mode::Region };
        let tokio = Arc::clone(&services.tokio);
        let client = Arc::clone(services.store.client());
        cx.spawn(async move |cx| {
            let outcome = tokio
                .spawn(async move {
                    let captured = tokio::task::spawn_blocking(move || {
                        capture::capture(&front, mode, &scratch)
                    })
                    .await
                    .map_err(|_| CaptureError::Failed)??;
                    let Some(captured) = captured else {
                        return Ok(None);
                    };
                    let params = serde_json::json!({
                        "source": captured.source,
                        "text": captured.text,
                        "textOrigin": captured.text_origin,
                        "pngPath": captured.png,
                    });
                    let saved = client
                        .request(Method::CLIP_SAVE, Some(&params), Some(Duration::from_secs(10)))
                        .await;
                    // Our scratch copy goes either way; the ring owns its own.
                    let _ = std::fs::remove_file(&captured.png);
                    let clip = saved
                        .ok()
                        .and_then(|value| serde_json::from_value::<ClipRecord>(value).ok())
                        .ok_or(CaptureError::Failed)?;
                    Ok::<_, CaptureError>(Some(clip))
                })
                .await
                .unwrap_or(Err(CaptureError::Failed));
            cx.update(|cx| match outcome {
                Ok(Some(clip)) => open(clip, services, cx),
                Ok(None) => {}
                Err(CaptureError::ScreenRecordingDenied) => {
                    if capture::guide_after_denial() {
                        crate::macos::context_hud_panel::guide_screen_recording();
                    }
                }
                Err(CaptureError::Failed) => {}
            });
        })
        .detach();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (services, window_mode, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::SessionId;
    use diri_proto::clip::{ClipSource, ClipTextOrigin};

    pub(crate) fn sample_clip(png: Option<String>) -> ClipRecord {
        ClipRecord {
            id: "0019a2b3c4d5e6f7".into(),
            created_at_ms: 0,
            source: ClipSource {
                app_name: Some("Arc".into()),
                bundle_id: Some("company.thebrowser.Browser".into()),
                url: Some("https://app.slack.com/client/T01/C02/thread/C02-1700000000.1".into()),
                page_title: Some("design-review (Acme) - Slack".into()),
                browser: Some("Arc".into()),
                ..Default::default()
            },
            tab_key: Some("https://app.slack.com/client/T01/C02/thread/C02-1700000000.1".into()),
            text: Some(
                "Ana Pop  10:42\nThe Save button overlaps the footer on narrow screens, and the card padding looks off next to the avatar.\nMihai  10:44\n+1, also the empty state is missing an illustration."
                    .into(),
            ),
            text_origin: ClipTextOrigin::Ocr,
            text_truncated: false,
            png_path: png,
            png_bytes: 1,
            width: 1280,
            height: 720,
            sent_to: Vec::new(),
        }
    }

    pub(crate) fn sample_targets() -> Vec<ClipTarget> {
        vec![
            ClipTarget {
                session_id: SessionId::new("s1"),
                title: "Fix settings card layout".into(),
                kind: AgentKind::CLAUDE_CODE,
                status: SessionStatus::Idle,
                project: Some("checkout".into()),
                reason: ClipTargetReason::Remembered,
                preview: ConversationPreview {
                    user: Some("tighten the card padding to match the design".into()),
                    agent: Some("Done: padding is 12px and the avatar is aligned.".into()),
                },
            },
            ClipTarget {
                session_id: SessionId::new("s2"),
                title: "Billing page empty states".into(),
                kind: AgentKind::CODEX,
                status: SessionStatus::Working,
                project: Some("checkout".into()),
                reason: ClipTargetReason::ServesPort,
                preview: ConversationPreview::default(),
            },
            ClipTarget {
                session_id: SessionId::new("s3"),
                title: "Investigate flaky e2e".into(),
                kind: AgentKind::CLAUDE_CODE,
                status: SessionStatus::Idle,
                project: Some("diri".into()),
                reason: ClipTargetReason::Recent,
                preview: ConversationPreview::default(),
            },
        ]
    }

    fn model() -> HudModel {
        let mut model = HudModel::new(
            sample_clip(None),
            Some(("/tmp/checkout".into(), "checkout".into())),
        );
        model.set_targets(sample_targets());
        model
    }

    #[test]
    fn the_remembered_session_is_preselected() {
        let model = model();
        assert_eq!(model.highlighted(), Row::Target(0));
        assert_eq!(
            model.destination(),
            serde_json::json!({"type": "session", "sessionID": "s1"})
        );
    }

    #[test]
    fn rows_end_with_new_session_and_ring_and_wrap() {
        let mut model = model();
        assert_eq!(
            model.rows(),
            [Row::Target(0), Row::Target(1), Row::Target(2), Row::NewSession, Row::Ring]
        );
        model.move_highlight(-1);
        assert_eq!(model.highlighted(), Row::Ring);
        assert_eq!(model.destination(), serde_json::json!({"type": "ring"}));
        model.move_highlight(-1);
        assert_eq!(model.highlighted(), Row::NewSession);
        assert_eq!(model.destination()["type"], "new");
        assert_eq!(model.new_kind().id(), AgentKind::CLAUDE_CODE_ID);
        model.cycle_new_kind(1);
        assert_eq!(model.new_kind().id(), AgentKind::CODEX_ID);
        assert_eq!(model.destination()["cwd"], "/tmp/checkout");
        model.move_highlight(1);
        model.move_highlight(1);
        assert_eq!(model.highlighted(), Row::Target(0));
    }

    #[test]
    fn filtering_narrows_targets_by_title_project_or_agent() {
        let mut model = model();
        model.filter.insert("billing");
        assert_eq!(model.visible_targets(), [1]);
        model.filter.clear();
        model.filter.insert("codex");
        assert_eq!(model.visible_targets(), [1]);
        model.filter.clear();
        model.filter.insert("diri");
        assert_eq!(model.visible_targets(), [2]);
        model.filter.clear();
        model.filter.insert("nothing matches");
        assert_eq!(model.rows(), [Row::NewSession, Row::Ring]);
    }

    #[test]
    fn without_a_project_there_is_no_new_session_row() {
        let mut model = HudModel::new(sample_clip(None), None);
        model.set_targets(Vec::new());
        assert_eq!(model.rows(), [Row::Ring]);
        assert_eq!(model.highlighted(), Row::Ring);
    }

    /// Renders the HUD in light and dark to `DIRI_VISUAL_OUTPUT` (a
    /// directory). `DIRI_VISUAL_PNG` names a screenshot for the thumbnail.
    #[test]
    #[ignore = "writes visual fixtures; run with DIRI_VISUAL_OUTPUT=dir --ignored"]
    fn render_context_hud_screenshot() {
        use gpui::{HeadlessAppContext, size};

        let output = std::path::PathBuf::from(
            std::env::var("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT"),
        );
        std::fs::create_dir_all(&output).unwrap();
        let png = std::env::var("DIRI_VISUAL_PNG").ok();
        for (name, colors) in [
            ("context-hud-light.png", SemanticColors::light()),
            ("context-hud-dark.png", SemanticColors::dark()),
        ] {
            let platform = gpui_platform::current_platform(true);
            let mut cx = HeadlessAppContext::with_platform(
                platform.text_system(),
                Arc::new(diri_ui::IconAssets),
                gpui_platform::current_headless_renderer,
            );
            cx.update(|cx| crate::fonts::init(cx));
            let mut model = HudModel::new(
                sample_clip(png.clone()),
                Some(("/tmp/checkout".into(), "checkout".into())),
            );
            model.set_targets(sample_targets());
            model.prompt.insert("address this feedback");
            struct Backdrop(HudModel, SemanticColors);
            impl Render for Backdrop {
                fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                    let backdrop = if self.1.appearance == diri_ui::Appearance::Light {
                        gpui::rgb(0xe8e6e1)
                    } else {
                        gpui::rgb(0x1d1f22)
                    };
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(backdrop)
                        .child(hud_view(&self.0, self.1))
                }
            }
            let window = cx
                .open_window(size(px(HUD_WIDTH + 80.0), px(HUD_HEIGHT + 80.0)), |_, cx| {
                    cx.new(|_| Backdrop(model, colors))
                })
                .unwrap();
            // The thumbnail decodes on the background executor.
            for _ in 0..20 {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(50));
                let _ = cx.update_window(window.into(), |_, window, _| window.refresh());
            }
            cx.run_until_parked();
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(output.join(name))
                .unwrap();
        }
    }
}
