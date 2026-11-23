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
use diri_ui::{AgentLogo, Fill, Glass, Palette, Radius, SemanticColors};
use gpui::{
    AnyElement, App, AppContext as _, Context, Div, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, Styled, StyledImage as _, Window, div,
    prelude::FluentBuilder, px,
};

use crate::i18n::{t, tf};
use crate::navigation::{PALETTE_RADIUS, PALETTE_ROW_INSET, palette_selection, row_ink};
use crate::query_editor::{self, ClipboardEdit, Edit, QueryEditor};

/// The ⌘K palette's width, so the two read as one family.
pub(crate) const HUD_WIDTH: f32 = 460.0;
/// The tallest the HUD gets; it is sized to its rows below that.
pub(crate) const HUD_MAX_HEIGHT: f32 =
    HEADER_HEIGHT + FILTER_HEIGHT + 2.0 * PALETTE_ROW_INSET + LIST_MAX + FOOTER_HEIGHT;
const THUMB_W: f32 = 44.0;
const THUMB_H: f32 = 34.0;
/// Thumbnail plus 12 pt above and below, then the hairline.
const HEADER_HEIGHT: f32 = THUMB_H + 24.0 + 1.0;
const FILTER_HEIGHT: f32 = PALETTE_ROW_INSET + 30.0;
const ROW_HEIGHT: f32 = 32.0;
/// What the highlighted Session's one-line preview adds under its title:
/// an 18 pt line tucked 6 pt up into the title's row.
const PREVIEW_HEIGHT: f32 = 12.0;
const PREVIEW_LINE: f32 = 18.0;
const FOOTER_HEIGHT: f32 = 28.0;
/// Rows shown at once; the list scrolls past them with the highlight.
const VISIBLE_ROWS: usize = 6;
const LIST_MAX: f32 = ROW_HEIGHT * VISIBLE_ROWS as f32 + PREVIEW_HEIGHT;
/// Agents offered for a new Session, the person's default first.
const NEW_AGENT_KINDS: [&str; 2] = [AgentKind::CLAUDE_CODE_ID, AgentKind::CODEX_ID];

/// Where ↩ sends the clip. With no row (nothing to send to), ↩ keeps the
/// prompt with the clip in the ring.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Row {
    Target(usize),
    NewSession,
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
        rows
    }

    pub(crate) fn highlighted(&self) -> Option<Row> {
        let rows = self.rows();
        let last = rows.len().checked_sub(1)?;
        rows.get(self.highlight.min(last)).cloned()
    }

    pub(crate) fn move_highlight(&mut self, delta: isize) {
        let count = self.rows().len() as isize;
        if count > 0 {
            self.highlight = (self.highlight as isize + delta).rem_euclid(count) as usize;
        }
    }

    pub(crate) fn new_kind(&self) -> AgentKind {
        AgentKind::new(NEW_AGENT_KINDS[self.new_kind % NEW_AGENT_KINDS.len()])
    }

    pub(crate) fn cycle_new_kind(&mut self, delta: isize) {
        let count = NEW_AGENT_KINDS.len() as isize;
        self.new_kind = (self.new_kind as isize + delta).rem_euclid(count) as usize;
    }

    /// The `clip.dispatch` destination for the highlighted row, or the ring
    /// when `ring` (⌘↩) or there is nowhere else.
    pub(crate) fn destination(&self, ring: bool) -> serde_json::Value {
        match self.highlighted().filter(|_| !ring) {
            Some(Row::Target(index)) => serde_json::json!({
                "type": "session",
                "sessionID": self.targets[index].session_id.0,
            }),
            Some(Row::NewSession) => serde_json::json!({
                "type": "new",
                "kind": self.new_kind(),
                "cwd": self.new_cwd.as_ref().map(|(cwd, _)| cwd.clone()).unwrap_or_default(),
            }),
            None => serde_json::json!({"type": "ring"}),
        }
    }

    /// Whether the find field is on screen: while typing in it, or while it
    /// still narrows the list.
    fn filtering(&self) -> bool {
        self.field == Field::Filter || !self.filter.is_empty()
    }

    /// Project names only tell Sessions apart when there is more than one.
    fn spans_projects(&self) -> bool {
        let mut projects = self.targets.iter().filter_map(|t| t.project.as_deref());
        projects
            .next()
            .is_some_and(|first| projects.any(|other| other != first))
    }

    /// The panel's height for what it shows now: no empty space under the
    /// last row.
    pub(crate) fn height(&self) -> f32 {
        let rows = self.rows();
        let shown = rows.len().clamp(1, VISIBLE_ROWS);
        let preview = match self.highlighted() {
            Some(Row::Target(index)) if preview_text(&self.targets[index].preview).is_some() => {
                PREVIEW_HEIGHT
            }
            _ => 0.0,
        };
        HEADER_HEIGHT
            + if self.filtering() { FILTER_HEIGHT } else { 0.0 }
            + 2.0 * PALETTE_ROW_INSET
            + ROW_HEIGHT * shown as f32
            + preview
            + FOOTER_HEIGHT
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

/// Where the clip came from, as a person names it: `Slack`, `github.com`,
/// `localhost:3000`, or the app. Never ids, paths or sizes.
pub(crate) fn source_label(clip: &ClipRecord) -> (String, &'static str) {
    let source = &clip.source;
    if let Some(url) = source.url.as_deref() {
        let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => (host, Some(port)),
            _ => (authority, None),
        };
        let host = host.to_lowercase();
        if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0")
            || host.ends_with(".localhost")
        {
            let label = match port {
                Some(port) => format!("localhost:{port}"),
                None => "localhost".to_owned(),
            };
            return (label, "chevron.left.forwardslash.chevron.right");
        }
        let host = host.strip_prefix("www.").unwrap_or(&host);
        let named = [
            ("slack.com", "Slack"),
            ("github.com", "GitHub"),
            ("linear.app", "Linear"),
            ("figma.com", "Figma"),
            ("docs.google.com", "Google Docs"),
            ("notion.so", "Notion"),
            ("vercel.com", "Vercel"),
        ]
        .into_iter()
        .find(|(domain, _)| host == *domain || host.ends_with(&format!(".{domain}")));
        if let Some((_, name)) = named {
            return (name.to_owned(), "network");
        }
        if !host.is_empty() {
            return (host.to_owned(), "network");
        }
    }
    let app = source
        .app_name
        .clone()
        .or_else(|| source.browser.clone())
        .unwrap_or_default();
    (app, "rectangle")
}

/// The latest word in the conversation: the agent's reply, else the prompt.
fn preview_text(preview: &ConversationPreview) -> Option<&str> {
    preview
        .agent
        .as_deref()
        .or(preview.user.as_deref())
        .filter(|text| !text.trim().is_empty())
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

fn agent_mark(kind: &AgentKind, colors: SemanticColors) -> AnyElement {
    AgentLogo::new(crate::session_presentation::ui_agent_kind(kind), 16.0, colors)
        .badged(false)
        .inset(0.08)
        .into_any_element()
}

/// The clip, quietly: its thumbnail, the prompt as the focal line, and
/// where it came from underneath.
fn header(model: &HudModel, colors: SemanticColors) -> Div {
    let clip = &model.clip;
    let thumbnail = match clip.png_path.as_deref() {
        Some(path) => gpui::img(std::path::PathBuf::from(path))
            .size_full()
            .object_fit(gpui::ObjectFit::Cover)
            .into_any_element(),
        None => div().into_any_element(),
    };
    let (source, icon) = source_label(clip);
    let editor = &model.prompt;
    let active = model.field == Field::Prompt;
    let prompt = if editor.is_empty() {
        div()
            .flex()
            .items_center()
            .text_color(colors.tertiary)
            .when(active, |d| {
                d.child(div().w(px(1.5)).h(px(17.0)).mr(px(1.0)).bg(Palette::CLAY))
            })
            .child(t("context_hud.prompt_placeholder").to_owned())
            .into_any_element()
    } else if active {
        crate::navigation::query_label(editor)
    } else {
        div().child(editor.text().to_owned()).into_any_element()
    };
    div()
        .flex_none()
        .h(px(HEADER_HEIGHT))
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(PALETTE_ROW_INSET + 10.0))
        .border_b_1()
        .border_color(colors.primary.alpha(0.06))
        .child(
            div()
                .flex_none()
                .w(px(THUMB_W))
                .h(px(THUMB_H))
                .rounded(px(Radius::CHIP))
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
                .gap(px(1.0))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(15.0))
                        .text_color(colors.primary)
                        .child(prompt),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_size(px(11.5))
                        .text_color(colors.tertiary)
                        .child(crate::icons::sf_symbol(icon, 10.0, colors.tertiary))
                        .child(div().truncate().child(source)),
                ),
        )
}

/// The find field, palette-style: an inset pill, only while finding.
fn filter_field(model: &HudModel, colors: SemanticColors) -> Div {
    let editor = &model.filter;
    let active = model.field == Field::Filter;
    div()
        .flex_none()
        .mt(px(PALETTE_ROW_INSET))
        .mx(px(PALETTE_ROW_INSET))
        .h(px(FILTER_HEIGHT - PALETTE_ROW_INSET))
        .px(px(10.0))
        .rounded(px(Radius::inner(PALETTE_RADIUS, PALETTE_ROW_INSET)))
        .bg(Fill::subtle(colors))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(13.0))
        .child(crate::icons::sf_symbol(
            "magnifyingglass",
            12.0,
            colors.tertiary,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_color(if editor.is_empty() {
                    colors.tertiary
                } else {
                    colors.primary
                })
                .child(if editor.is_empty() {
                    t("context_hud.filter_placeholder").to_owned().into_any_element()
                } else if active {
                    crate::navigation::query_label(editor)
                } else {
                    editor.text().to_owned().into_any_element()
                }),
        )
}

/// A palette row: leading glyph, title, at most one quiet cue.
fn row(
    leading: AnyElement,
    title: String,
    cue: Option<(String, gpui::Rgba)>,
    preview: Option<&str>,
    highlighted: bool,
    colors: SemanticColors,
) -> Div {
    let ink = row_ink(colors, highlighted);
    div()
        .flex_none()
        .mx(px(PALETTE_ROW_INSET))
        .px(px(10.0))
        .rounded(px(Radius::inner(PALETTE_RADIUS, PALETTE_ROW_INSET)))
        .when(highlighted, |d| d.bg(palette_selection()))
        .text_color(ink.primary)
        .child(
            div()
                .h(px(ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(13.0))
                .child(
                    div()
                        .w(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(leading),
                )
                .child(div().flex_1().min_w_0().truncate().child(title))
                .when_some(cue, |d, (cue, color)| {
                    let color = if highlighted { ink.secondary } else { color };
                    d.child(
                        div()
                            .flex_none()
                            .text_size(px(12.0))
                            .text_color(color)
                            .child(cue),
                    )
                }),
        )
        .when_some(preview, |d, text| {
            d.child(
                div()
                    .h(px(PREVIEW_LINE))
                    .mt(px(PREVIEW_HEIGHT - PREVIEW_LINE))
                    .pl(px(24.0))
                    .text_size(px(11.5))
                    .text_color(ink.tertiary)
                    .truncate()
                    .child(ellipsize(text, 90)),
            )
        })
}

fn target_row(
    target: &ClipTarget,
    spans_projects: bool,
    highlighted: bool,
    colors: SemanticColors,
) -> Div {
    let ink = row_ink(colors, highlighted);
    // The agent's mark, with a dot only while it is busy or waiting: an idle
    // Session needs no badge.
    let dot = match target.status {
        SessionStatus::Working => Some(Palette::CLAY),
        SessionStatus::NeedsInput(_) => Some(ink.primary),
        _ => None,
    };
    let leading = div()
        .relative()
        .size(px(16.0))
        .child(agent_mark(&target.kind, ink))
        .when_some(dot, |d, dot| {
            d.child(
                div()
                    .absolute()
                    .right(px(-2.0))
                    .bottom(px(-2.0))
                    .size(px(7.0))
                    .rounded_full()
                    .border_1()
                    .border_color(if highlighted {
                        palette_selection()
                    } else {
                        colors.floating_surface()
                    })
                    .bg(dot),
            )
        })
        .into_any_element();
    let cue = match target.reason {
        ClipTargetReason::Remembered => {
            Some((t("context_hud.reason.remembered").to_owned(), Palette::CLAY))
        }
        ClipTargetReason::ServesPort => {
            Some((t("context_hud.reason.serves_port").to_owned(), colors.tertiary))
        }
        ClipTargetReason::Recent => target
            .project
            .clone()
            .filter(|_| spans_projects)
            .map(|project| (project, colors.tertiary)),
    };
    let preview = highlighted
        .then(|| preview_text(&target.preview))
        .flatten();
    row(
        leading,
        ellipsize(&target.title, 60),
        cue,
        preview,
        highlighted,
        colors,
    )
}

/// The whole HUD for `model` in `colors`.
pub(crate) fn hud_view(model: &HudModel, colors: SemanticColors) -> Div {
    let rows = model.rows();
    let highlight = model.highlight.min(rows.len().saturating_sub(1));
    let spans_projects = model.spans_projects();
    // Keep the highlight in view: a window of VISIBLE_ROWS around it.
    let first = highlight.saturating_sub(VISIBLE_ROWS - 1);
    let mut list = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(VISIBLE_ROWS)
        .map(|(index, row_kind)| {
            let highlighted = index == highlight;
            match row_kind {
                Row::Target(target) => {
                    target_row(&model.targets[*target], spans_projects, highlighted, colors)
                }
                Row::NewSession => {
                    let ink = row_ink(colors, highlighted);
                    let project = model
                        .new_cwd
                        .as_ref()
                        .map(|(_, name)| name.as_str())
                        .unwrap_or_default();
                    row(
                        crate::icons::sf_symbol("plus", 12.0, ink.secondary),
                        tf("context_hud.new_session", &[("project", &project)]),
                        highlighted.then(|| (kind_label(&model.new_kind()), colors.tertiary)),
                        None,
                        highlighted,
                        colors,
                    )
                }
            }
        })
        .collect::<Vec<_>>();
    if list.is_empty() {
        let note = if model.loading {
            t("context_hud.loading")
        } else {
            t("context_hud.no_sessions")
        };
        list.push(
            div()
                .h(px(ROW_HEIGHT))
                .px(px(PALETTE_ROW_INSET + 10.0))
                .flex()
                .items_center()
                .text_size(px(13.0))
                .text_color(colors.tertiary)
                .child(note.to_owned()),
        );
    }
    let status: Option<(String, gpui::Rgba)> = match &model.phase {
        Phase::Ready => None,
        Phase::Sending => Some((t("context_hud.sending").to_owned(), colors.secondary)),
        Phase::Sent(to) => Some((tf("context_hud.sent", &[("target", to)]), Palette::CLAY)),
        Phase::Failed(error) => Some((error.clone(), colors.primary)),
    };
    let hint = |label: &str, key: &str| {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(6.0))
            .child(div().text_color(colors.secondary).child(label.to_owned()))
            .child(div().text_color(colors.tertiary).child(key.to_owned()))
    };
    div()
        .w(px(HUD_WIDTH))
        .h(px(model.height()))
        .flex()
        .flex_col()
        .rounded(px(PALETTE_RADIUS))
        .overflow_hidden()
        .text_color(colors.primary)
        .bg(Glass::panel_fill(colors))
        .border_1()
        .border_color(colors.floating_stroke())
        .child(header(model, colors))
        .when(model.filtering(), |d| d.child(filter_field(model, colors)))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .py(px(PALETTE_ROW_INSET))
                .flex()
                .flex_col()
                .children(list),
        )
        .child(
            div()
                .flex_none()
                .h(px(FOOTER_HEIGHT))
                .flex()
                .items_center()
                .gap(px(2.0))
                .pl(px(PALETTE_ROW_INSET + 10.0))
                .pr(px(PALETTE_ROW_INSET + 4.0))
                .border_t_1()
                .border_color(colors.primary.alpha(0.06))
                .text_size(px(11.5))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when_some(status, |d, (text, color)| d.text_color(color).child(text)),
                )
                .child(hint(t("context_hud.hint.send"), "↩"))
                .child(hint(t("context_hud.hint.find"), "⇥"))
                .child(hint(t("context_hud.hint.close"), "esc")),
        )
}

/// The live HUD window's root view.
pub(crate) struct ContextHud {
    pub model: HudModel,
    colors: SemanticColors,
    focus: FocusHandle,
    services: Option<HudServices>,
    /// The height the panel was last sized to.
    height: f32,
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
            height: model.height(),
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
                if self.model.highlighted() == Some(Row::NewSession) && self.editor().is_empty() =>
            {
                self.model
                    .cycle_new_kind(if event.keystroke.key == "left" { -1 } else { 1 });
            }
            "enter" => self.dispatch(modifiers.platform, window, cx),
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

    /// Sends to the highlighted row, or with `ring` (⌘↩) only keeps the
    /// prompt with the clip in the ring.
    fn dispatch(&mut self, ring: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(services) = self.services.clone() else {
            return;
        };
        let target = match self.model.highlighted().filter(|_| !ring) {
            Some(Row::Target(index)) => self.model.targets[index].title.clone(),
            Some(Row::NewSession) => kind_label(&self.model.new_kind()),
            None => t("context_hud.ring_name").to_owned(),
        };
        let params = serde_json::json!({
            "id": self.model.clip.id,
            "prompt": self.model.prompt.text(),
            "destination": self.model.destination(ring),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Rows arrive after the panel opens and the find field comes and
        // goes: keep the panel exactly as tall as what it shows.
        let height = self.model.height();
        if (height - self.height).abs() > 0.5 {
            self.height = height;
            #[cfg(target_os = "macos")]
            crate::macos::context_hud_panel::fit_height(window, height, cx.foreground_executor());
            #[cfg(not(target_os = "macos"))]
            window.resize(gpui::size(px(HUD_WIDTH), px(height)));
        }
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
    let bounds = crate::macos::context_hud_panel::frame(HUD_WIDTH, model.height(), HUD_MAX_HEIGHT);
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
            // Clicking away closes it; the clip stays in the ring. Opening
            // reports key status off and on again (the style change in
            // `prepare`, then `makeKeyAndOrderFront`), and the notifications
            // arrive after this subscribes: only a loss that lasts closes it.
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    return;
                }
                cx.spawn_in(window, async move |_, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(200))
                        .await;
                    let _ = cx.update(|window, _| {
                        if !window.is_window_active() {
                            window.remove_window();
                        }
                    });
                })
                .detach();
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
        assert_eq!(model.highlighted(), Some(Row::Target(0)));
        assert_eq!(
            model.destination(false),
            serde_json::json!({"type": "session", "sessionID": "s1"})
        );
        assert_eq!(model.destination(true), serde_json::json!({"type": "ring"}));
    }

    #[test]
    fn rows_end_with_new_session_and_wrap() {
        let mut model = model();
        assert_eq!(
            model.rows(),
            [Row::Target(0), Row::Target(1), Row::Target(2), Row::NewSession]
        );
        model.move_highlight(-1);
        assert_eq!(model.highlighted(), Some(Row::NewSession));
        assert_eq!(model.destination(false)["type"], "new");
        assert_eq!(model.new_kind().id(), AgentKind::CLAUDE_CODE_ID);
        model.cycle_new_kind(1);
        assert_eq!(model.new_kind().id(), AgentKind::CODEX_ID);
        assert_eq!(model.destination(false)["cwd"], "/tmp/checkout");
        model.move_highlight(1);
        assert_eq!(model.highlighted(), Some(Row::Target(0)));
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
        assert_eq!(model.rows(), [Row::NewSession]);
    }

    #[test]
    fn with_nowhere_to_send_the_prompt_stays_with_the_clip() {
        let mut model = HudModel::new(sample_clip(None), None);
        model.set_targets(Vec::new());
        assert!(model.rows().is_empty());
        assert_eq!(model.highlighted(), None);
        model.move_highlight(1);
        assert_eq!(model.destination(false), serde_json::json!({"type": "ring"}));
    }

    #[test]
    fn the_panel_is_as_tall_as_its_rows() {
        let mut model = model();
        let with_preview = model.height();
        model.move_highlight(1);
        // The second Session has no conversation yet, so no preview line.
        assert_eq!(model.height(), with_preview - PREVIEW_HEIGHT);
        model.field = Field::Filter;
        assert_eq!(model.height(), with_preview - PREVIEW_HEIGHT + FILTER_HEIGHT);
        model.filter.insert("nothing matches");
        model.new_cwd = None;
        // One quiet "No sessions" line, never a collapsed panel.
        assert_eq!(
            model.height(),
            HEADER_HEIGHT + FILTER_HEIGHT + 2.0 * PALETTE_ROW_INSET + ROW_HEIGHT + FOOTER_HEIGHT
        );
        assert!(with_preview <= HUD_MAX_HEIGHT);
    }

    #[test]
    fn sources_read_as_people_name_them() {
        let mut clip = sample_clip(None);
        assert_eq!(source_label(&clip).0, "Slack");
        clip.source.url = Some("http://127.0.0.1:3000/settings?tab=2".into());
        assert_eq!(source_label(&clip).0, "localhost:3000");
        clip.source.url = Some("https://www.example.com/a/b".into());
        assert_eq!(source_label(&clip).0, "example.com");
        clip.source.url = Some("https://user:secret@github.com/o/r/pull/1".into());
        assert_eq!(source_label(&clip).0, "GitHub");
        clip.source.url = None;
        clip.source.app_name = Some("Figma".into());
        assert_eq!(source_label(&clip).0, "Figma");
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
                .open_window(size(px(HUD_WIDTH + 80.0), px(HUD_MAX_HEIGHT + 40.0)), |_, cx| {
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
