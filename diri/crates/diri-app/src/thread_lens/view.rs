//! The GUI Lens (Conductor Mode): one live Agent session drawn as an
//! editorial thread, beside the TUI Lens (Pilot Mode) on the same Holder.
//!
//! Everything typed or clicked here goes to the real PTY: prompts through
//! `session.send_text`, permission answers through `session.answer_blocker`
//! (which re-checks that the prompt is still live), model switches as the
//! CLI's own `/model` command. The thread itself comes from the Engine's
//! transcript tail, so this view never scrapes the terminal.
//!
//! Rendering stays inside the ADEMark budgets: rows are virtualized by
//! GPUI's `list` (only visible rows lay out and shape text, so off-screen
//! rows hold no glyph runs), assistant Markdown is parsed once per record
//! ([`ThreadModel`]), diff cards and decoded images are cached per item key,
//! and nothing animates or polls unless the Agent is working.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use diri_proto::thread::{
    BlockerChoice, CacheWarmth, ItemKey, SegmentCause, ThreadImage,
    ThreadReadParams, ToolDetail,
};
use diri_proto::{AgentKind, NeedsInputKind, SessionId, SessionRecord, SessionStatus};
use diri_ui::{Ink, Radius, SemanticColors};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, EventEmitter, FocusHandle,
    Focusable, FontWeight, Image, ImageFormat, KeyDownEvent, ListAlignment, ListState, Rgba,
    SharedString, Task, Window, div, img, list, prelude::*, px, rgba,
};
use tokio::runtime::Handle;

use super::diff::{self, DiffCard, Side};
use super::model::{Call, RowChange, Row, ThreadModel, WorkStrip};
use crate::code_editor::EditorPalette;
use crate::code_editor::syntax::{self, LexState, TokenKind};
use crate::code_intelligence::SourceLanguage;
use crate::git_ui::palette::DiffPalette;
use crate::markdown::{InlineText, MarkdownBlock, MarkdownDocument};
use crate::notes::editor_view::diagram::{self, Diagrams, Look};
use crate::query_editor::{ClipboardEdit, Edit, LocalEdit, QueryEditor};
use crate::i18n::{t, tf};
use crate::store::StoreRuntime;

/// Prose measure: about 75 characters at the body size.
const MEASURE: f32 = 720.0;
const PROSE_SIZE: f32 = 14.0;
const PROSE_LEADING: f32 = 22.0;
const MONO_SIZE: f32 = 12.0;
const MONO_LEADING: f32 = 18.0;
const ACCENT: u32 = 0xd97757;
/// Claude Code's own model aliases; `/model <alias>` switches in place.
const CLAUDE_MODELS: &[(&str, &str, &str)] = &[
    ("opus", "Opus", "claude-opus-5-5"),
    ("sonnet", "Sonnet", "claude-sonnet-5-5"),
    ("haiku", "Haiku", "claude-haiku-5-5"),
];

pub enum ThreadLensEvent {
    /// A `path:line` pill was clicked.
    OpenReference(String),
    /// The account pill: open Diri's existing account-continuation sheet.
    ContinueAccount,
    /// The PTY portal wants keyboard focus in the real terminal.
    FocusTerminal,
    /// ⌘V with an image on the pasteboard: the terminal's own paste path
    /// uploads it into the same PTY.
    PasteIntoTerminal,
}

enum LensMessage {
    Page(Result<diri_proto::thread::ThreadReadResult, String>),
    Answered(Result<(), String>),
}

pub struct ThreadLens {
    session_id: SessionId,
    runtime: Arc<StoreRuntime>,
    tokio: Handle,
    pub(crate) model: ThreadModel,
    list: ListState,
    focus: FocusHandle,
    composer: QueryEditor,
    expanded: HashSet<(ItemKey, u16)>,
    reading: bool,
    reread: bool,
    answer_error: Option<SharedString>,
    model_menu: bool,
    images: HashMap<(ItemKey, usize), Option<Arc<Image>>>,
    cards: HashMap<ItemKey, Arc<Vec<DiffCard>>>,
    diagrams: Diagrams,
    /// Wipe position per before/after pair, 0..1.
    wipes: HashMap<ItemKey, f32>,
    messages: tokio::sync::mpsc::UnboundedSender<LensMessage>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<ThreadLensEvent> for ThreadLens {}

impl Focusable for ThreadLens {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ThreadLens {
    pub fn new(
        session_id: SessionId,
        runtime: Arc<StoreRuntime>,
        tokio: Handle,
        cx: &mut Context<Self>,
    ) -> Self {
        let (messages, mut inbox) = tokio::sync::mpsc::unbounded_channel();
        let inbox_task = cx.spawn(async move |this, cx| {
            while let Some(message) = inbox.recv().await {
                if this
                    .update(cx, |this, cx| this.receive(message, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut events = runtime.client().events();
        let watched = session_id.clone();
        let events_task = cx.spawn(async move |this, cx| {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this.update(cx, |this, cx| this.request_read(cx)).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                let ours = match event.name.as_str() {
                    diri_proto::EventName::SESSION_THREAD_APPENDED => {
                        event.params.get("sessionID").and_then(|v| v.as_str())
                            == Some(watched.0.as_str())
                    }
                    // Hook-driven status changes precede and follow every
                    // turn, so they also renew the Engine's watch lease.
                    diri_proto::EventName::SESSION_UPDATED => {
                        event.params.get("id").and_then(|v| v.as_str()) == Some(watched.0.as_str())
                    }
                    _ => false,
                };
                if ours && this.update(cx, |this, cx| this.request_read(cx)).is_err() {
                    return;
                }
            }
        });
        let list = ListState::new(0, ListAlignment::Bottom, px(600.0));
        list.set_follow_mode(gpui::FollowMode::Tail);
        let mut lens = Self {
            session_id,
            runtime,
            tokio,
            model: ThreadModel::default(),
            list,
            focus: cx.focus_handle(),
            composer: QueryEditor::default(),
            expanded: HashSet::new(),
            reading: false,
            reread: false,
            answer_error: None,
            model_menu: false,
            images: HashMap::new(),
            cards: HashMap::new(),
            diagrams: Diagrams::default(),
            wipes: HashMap::new(),
            messages,
            _tasks: vec![inbox_task, events_task],
        };
        lens.request_read(cx);
        lens
    }

    /// For headless fixtures: a lens fed directly, with no Engine.
    #[cfg(test)]
    pub(crate) fn ingest_for_test(
        &mut self,
        page: diri_proto::thread::ThreadReadResult,
        cx: &mut Context<Self>,
    ) {
        let change = self.model.ingest(page);
        self.apply_change(change);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn expand_all_for_test(&mut self, cx: &mut Context<Self>) {
        for row in &self.model.rows {
            if let Row::Work(strip) = row {
                self.expanded.insert((strip.key, u16::MAX));
                for call in &strip.calls {
                    self.expanded.insert((call.key, 0));
                }
            }
        }
        self.list.reset(self.model.rows.len());
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn scroll_for_test(&mut self, pixels: f32) {
        self.list.scroll_by(px(pixels));
    }

    #[cfg(test)]
    pub(crate) fn type_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.composer.insert_multiline(text);
        cx.notify();
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn is_structured(&self) -> Option<bool> {
        self.model.structured
    }

    fn request_read(&mut self, cx: &mut Context<Self>) {
        if self.reading {
            self.reread = true;
            return;
        }
        self.reading = true;
        self.reread = false;
        let client = Arc::clone(self.runtime.client());
        let params = ThreadReadParams {
            session_id: self.session_id.clone(),
            from: self.model.cursor.clone(),
            max_bytes: None,
            watch: true,
        };
        let messages = self.messages.clone();
        self.tokio.spawn(async move {
            let result = client
                .thread_read(&params)
                .await
                .map_err(|error| error.to_string());
            let _ = messages.send(LensMessage::Page(result));
        });
        let _ = cx;
    }

    fn receive(&mut self, message: LensMessage, cx: &mut Context<Self>) {
        match message {
            LensMessage::Page(Ok(page)) => {
                self.reading = false;
                let more = page.more;
                let change = self.model.ingest(page);
                self.apply_change(change);
                if more || self.reread {
                    self.request_read(cx);
                }
                cx.notify();
            }
            LensMessage::Page(Err(_)) => {
                self.reading = false;
                if self.reread {
                    self.request_read(cx);
                }
            }
            LensMessage::Answered(result) => {
                self.answer_error = result.err().map(|error| {
                    if error.contains("blocker_changed") || error.contains("no longer showing") {
                        SharedString::from(t("thread_lens.prompt_changed"))
                    } else {
                        SharedString::from(error)
                    }
                });
                cx.notify();
            }
        }
    }

    fn apply_change(&mut self, change: RowChange) {
        match change {
            RowChange::None => {}
            RowChange::Tail { from, old_len } => {
                // Cached cards and images belong to immutable items; only the
                // reopened tail row needs remeasuring.
                self.list
                    .splice(from..old_len, self.model.rows.len().saturating_sub(from));
            }
            RowChange::Rebuilt => self.list.reset(self.model.rows.len()),
        }
    }

    fn session(&self) -> Option<Arc<SessionRecord>> {
        self.runtime
            .store
            .read()
            .expect("store")
            .sessions()
            .get(&self.session_id)
            .map(Arc::clone)
    }

    fn colors(&self) -> (SemanticColors, diri_term::theme::TermTheme) {
        let store = self.runtime.store.read().expect("store");
        (
            crate::app_theme::colors_in(&store),
            crate::app_theme::terminal_theme_in(&store),
        )
    }

    // ── Input ────────────────────────────────────────────────────────────

    fn send(&mut self, text: String, submit: bool, cx: &mut Context<Self>) {
        let client = Arc::clone(self.runtime.client());
        let id = self.session_id.clone();
        self.tokio.spawn(async move {
            let _ = client.send_text(&id, text, submit).await;
        });
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.text().trim_end().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.clear();
        self.send(text, true, cx);
        self.list.scroll_to_end();
    }

    fn answer(&mut self, choice: BlockerChoice, occurred_at: f64, cx: &mut Context<Self>) {
        self.answer_error = None;
        let client = Arc::clone(self.runtime.client());
        let params = diri_proto::thread::AnswerBlockerParams {
            session_id: self.session_id.clone(),
            occurred_at,
            choice,
        };
        let messages = self.messages.clone();
        self.tokio.spawn(async move {
            let result = client
                .answer_blocker(&params)
                .await
                .map_err(|error| error.to_string());
            let _ = messages.send(LensMessage::Answered(result));
        });
        cx.notify();
    }

    /// Tier 1: the CLI's own in-session switch. Claude keeps its
    /// conversation and the old model's cache stays warm until its TTL.
    fn switch_model(&mut self, alias: &str, cx: &mut Context<Self>) {
        self.model_menu = false;
        self.send(format!("/model {alias}"), true, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let session = self.session();
        let blocker = session.as_ref().and_then(|s| permission(s));
        match keystroke.key.as_str() {
            "enter" if modifiers.platform => {
                if let Some(occurred_at) = blocker {
                    self.answer(BlockerChoice::Approve, occurred_at, cx);
                    cx.stop_propagation();
                    return;
                }
            }
            "enter" if modifiers.shift || modifiers.alt => {
                self.composer.insert_multiline("\n");
                cx.stop_propagation();
                cx.notify();
                return;
            }
            "enter" if !modifiers.control && !modifiers.platform => {
                self.submit(cx);
                cx.stop_propagation();
                return;
            }
            "escape" => {
                if self.model_menu {
                    self.model_menu = false;
                } else if let Some(occurred_at) = blocker {
                    self.answer(BlockerChoice::Deny, occurred_at, cx);
                } else if self.composer.is_empty() {
                    // Esc into the PTY interrupts the Agent, as in the TUI.
                    self.send("\u{1b}".into(), false, cx);
                } else {
                    self.composer.clear();
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
            _ => {}
        }
        match crate::query_editor::edit_for(keystroke) {
            Some(Edit::Local(edit)) => {
                if let LocalEdit::Insert(text) = &edit {
                    self.composer.insert_multiline(text);
                } else {
                    self.composer.apply(edit);
                }
                cx.stop_propagation();
                cx.notify();
            }
            Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                crate::query_editor::copy_selection(&self.composer, cx);
                cx.stop_propagation();
            }
            Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                crate::query_editor::cut_selection(&mut self.composer, cx);
                cx.stop_propagation();
                cx.notify();
            }
            Some(Edit::Clipboard(ClipboardEdit::Paste)) => {
                let item = cx.read_from_clipboard();
                let text = item.as_ref().and_then(|item| item.text());
                let has_image = item.as_ref().is_some_and(|item| {
                    item.entries()
                        .iter()
                        .any(|entry| matches!(entry, gpui::ClipboardEntry::Image(_)))
                });
                if has_image {
                    cx.emit(ThreadLensEvent::PasteIntoTerminal);
                } else if let Some(text) = text {
                    self.composer.insert_context(&text);
                }
                cx.stop_propagation();
                cx.notify();
            }
            None => {
                // Keys with no meaning here must not fall through to the
                // terminal behind the lens; ⌘ shortcuts stay with the app.
                if !modifiers.platform {
                    cx.stop_propagation();
                }
            }
        }
        let _ = window;
    }

    fn toggle(&mut self, key: (ItemKey, u16), row: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.list.splice(row..row + 1, 1);
        cx.notify();
    }

    // ── Rows ─────────────────────────────────────────────────────────────

    fn render_row(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.model.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let (colors, theme) = self.colors();
        let working = self.session().is_some_and(|s| is_working(&s));
        let last = index + 1 == self.model.rows.len();
        let body = match &row {
            Row::User { key, text, images } => self.render_user(*key, text, images, colors, cx),
            Row::Command { name, args, .. } => command_chip(name, args, colors),
            Row::Assistant { key, document, .. } => {
                self.render_document(*key, document, colors, &theme, cx)
            }
            Row::Work(strip) => self.render_strip(index, strip, last && working, colors, &theme, cx),
            Row::Notice { text, .. } => notice(text, colors),
            Row::Divider(segment) => divider(segment, colors),
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(24.0))
            .py(px(match row {
                Row::Work(_) | Row::Command { .. } => 3.0,
                Row::Divider(_) | Row::Notice { .. } => 10.0,
                _ => 8.0,
            }))
            .child(div().w_full().max_w(px(MEASURE)).child(body))
            .into_any_element()
    }

    fn render_user(
        &mut self,
        key: ItemKey,
        text: &str,
        images: &[ThreadImage],
        colors: SemanticColors,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let thumbs: Vec<AnyElement> = images
            .iter()
            .enumerate()
            .filter_map(|(index, image)| {
                let decoded = self.image(key, index, image)?;
                Some(
                    img(decoded)
                        .max_h(px(160.0))
                        .max_w(px(240.0))
                        .rounded(px(Radius::BADGE))
                        .into_any_element(),
                )
            })
            .collect();
        div()
            .w_full()
            .flex()
            .justify_end()
            .child(
                div()
                    .max_w(px(MEASURE * 0.82))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(14.0))
                    .bg(colors.primary.alpha(0.055))
                    .border_1()
                    .border_color(colors.primary.alpha(0.06))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .when(!thumbs.is_empty(), |bubble| {
                        bubble.child(div().flex().flex_wrap().gap(px(6.0)).children(thumbs))
                    })
                    .when(!text.is_empty(), |bubble| {
                        bubble.child(
                            div()
                                .text_size(px(PROSE_SIZE))
                                .line_height(px(PROSE_LEADING))
                                .text_color(colors.primary)
                                .child(SharedString::from(text.to_owned())),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_document(
        &mut self,
        key: ItemKey,
        document: &MarkdownDocument,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = div().w_full().flex().flex_col().gap(px(12.0));
        for (index, block) in document.blocks.iter().enumerate() {
            column = column.child(self.render_block(key, index, block, colors, theme, cx));
        }
        column.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_block(
        &mut self,
        key: ItemKey,
        index: usize,
        block: &MarkdownBlock,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match block {
            MarkdownBlock::Heading { level, content } => {
                let size = match level {
                    1 => 20.0,
                    2 => 17.0,
                    _ => 15.0,
                };
                div()
                    .pt(px(4.0))
                    .text_size(px(size))
                    .line_height(px(size * 1.35))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.primary)
                    .child(self.inline(content, colors, cx))
                    .into_any_element()
            }
            MarkdownBlock::Paragraph(content) => div()
                .text_size(px(PROSE_SIZE))
                .line_height(px(PROSE_LEADING))
                .text_color(colors.primary.alpha(0.92))
                .child(self.inline(content, colors, cx))
                .into_any_element(),
            MarkdownBlock::List {
                ordered,
                start,
                items,
            } => {
                let mut list = div().flex().flex_col().gap(px(4.0));
                for (n, item) in items.iter().enumerate() {
                    let marker = match item.checked {
                        Some(true) => "✓".to_owned(),
                        Some(false) => "○".to_owned(),
                        None if *ordered => format!("{}.", start + n),
                        None => "•".to_owned(),
                    };
                    list = list.child(
                        div()
                            .flex()
                            .gap(px(10.0))
                            .text_size(px(PROSE_SIZE))
                            .line_height(px(PROSE_LEADING))
                            .text_color(colors.primary.alpha(0.92))
                            .child(
                                div()
                                    .w(px(18.0))
                                    .flex_none()
                                    .text_right()
                                    .font_family(crate::fonts::mono_family())
                                    .text_color(colors.tertiary)
                                    .child(marker),
                            )
                            .child(div().min_w(px(0.0)).flex_1().child(self.inline(
                                &item.content,
                                colors,
                                cx,
                            ))),
                    );
                }
                list.into_any_element()
            }
            MarkdownBlock::Quote(blocks) => {
                let mut quote = div()
                    .pl(px(14.0))
                    .border_l_2()
                    .border_color(rgba(ACCENT << 8 | 0x88))
                    .flex()
                    .flex_col()
                    .gap(px(8.0));
                for (n, block) in blocks.iter().enumerate() {
                    quote = quote.child(self.render_block(key, index * 64 + n, block, colors, theme, cx));
                }
                quote.into_any_element()
            }
            MarkdownBlock::CodeBlock { language, code } => {
                if language.as_deref() == Some("mermaid") {
                    return self.render_mermaid(key, index, code, colors, cx);
                }
                code_block(language.as_deref(), code, colors, theme)
            }
            MarkdownBlock::ThematicBreak => div()
                .h(px(1.0))
                .w_full()
                .bg(colors.primary.alpha(0.08))
                .into_any_element(),
        }
    }

    fn render_mermaid(
        &mut self,
        key: ItemKey,
        index: usize,
        code: &str,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = key.offset.wrapping_mul(131) ^ (u64::from(key.part) << 48) ^ index as u64;
        let dark = colors.lightness() < 0.5;
        let (look, start) = self.diagrams.look(id, code, dark);
        if let Some(draw_key) = start {
            let source = code.to_owned();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move { diagram::draw(&source, dark) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.diagrams.landed(id, draw_key, result) {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        let frame = div()
            .w_full()
            .p(px(14.0))
            .rounded(px(Radius::CARD))
            .border_1()
            .border_color(colors.primary.alpha(0.07))
            .bg(colors.primary.alpha(0.025))
            .flex()
            .justify_center();
        match look {
            Look::Drawn(drawing) => {
                // Diagrams read at the prose measure's scale, never larger
                // than their natural size or a third of a tall window.
                let scale = (MEASURE * 0.8 / drawing.width.max(1.0))
                    .min(300.0 / drawing.height.max(1.0))
                    .min(1.0);
                frame
                    .child(
                        img(drawing.image)
                            .w(px(drawing.width * scale))
                            .h(px(drawing.height * scale)),
                    )
                    .into_any_element()
            }
            Look::Pending => frame
                .h(px(120.0))
                .text_color(colors.tertiary)
                .text_size(px(12.0))
                .items_center()
                .child(t("thread_lens.diagram_drawing"))
                .into_any_element(),
            Look::Failed(message, _) => code_block(Some("mermaid"), &format!("{code}\n\n%% {message}"), colors, &self.colors().1),
        }
    }

    /// Inline spans. Code spans that read as `path:line` become pills that
    /// open the file; a hover shows its first lines.
    fn inline(&self, content: &InlineText, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let mut line = div().min_w(px(0.0)).flex().flex_wrap().items_baseline();
        for (index, span) in content.spans.iter().enumerate() {
            if span.text.is_empty() {
                continue;
            }
            let style = span.style;
            if style.code && looks_like_path(&span.text) {
                line = line.child(self.path_pill(&span.text, index, colors, cx));
                continue;
            }
            let link = span.link.clone();
            line = line.child(
                div()
                    .id(SharedString::from(format!("lens-inline-{index}-{:p}", span)))
                    .min_w(px(0.0))
                    .max_w_full()
                    .when(style.bold, |piece| piece.font_weight(FontWeight::SEMIBOLD))
                    .when(style.italic, |piece| piece.italic())
                    .when(style.strikethrough, |piece| piece.line_through())
                    .when(style.code, |piece| {
                        piece
                            .px(px(4.0))
                            .rounded(px(4.0))
                            .bg(colors.primary.alpha(0.07))
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(MONO_SIZE))
                    })
                    .when(link.is_some(), |piece| {
                        piece.underline().cursor_pointer().text_color(rgba(0x6aa6e8ff))
                    })
                    .when_some(link, |piece, link| {
                        piece.on_click(move |_, _, cx| {
                            cx.open_url(&link);
                            cx.stop_propagation();
                        })
                    })
                    .child(SharedString::from(span.text.clone())),
            );
        }
        line.into_any_element()
    }

    fn path_pill(&self, reference: &str, index: usize, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let reference = reference.to_owned();
        let label = short_path(&reference);
        let cwd = self.session().map(|s| s.cwd.clone()).unwrap_or_default();
        let preview_reference = reference.clone();
        div()
            .id(SharedString::from(format!("lens-path-{index}-{reference}")))
            .flex()
            .items_center()
            .gap(px(4.0))
            .mx(px(1.0))
            .px(px(6.0))
            .h(px(20.0))
            .rounded(px(Radius::CHIP))
            .bg(colors.primary.alpha(0.07))
            .hover(|pill| pill.bg(colors.primary.alpha(0.12)))
            .cursor_pointer()
            .font_family(crate::fonts::mono_family())
            .text_size(px(MONO_SIZE))
            .text_color(colors.primary)
            .child(div().text_color(colors.tertiary).child("↗"))
            .child(SharedString::from(label))
            .tooltip(move |_, cx| {
                let preview = file_preview(&cwd, &preview_reference);
                cx.new(|_| PathPreview { preview, colors }).into()
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ThreadLensEvent::OpenReference(reference.clone()));
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_strip(
        &mut self,
        row: usize,
        strip: &WorkStrip,
        live: bool,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_key = (strip.key, u16::MAX);
        let open = self.expanded.contains(&open_key);
        let failures = strip.failures();
        let mut meta = strip.summary();
        if let Some(ms) = strip.duration_ms()
            && ms >= 100
        {
            meta.push_str(&format!(" · {}", duration(ms)));
        }
        let dot = div()
            .size(px(7.0))
            .rounded_full()
            .bg(if failures > 0 {
                Ink::DANGER
            } else if live || strip.pending() {
                rgba(ACCENT << 8 | 0xff)
            } else {
                colors.tertiary.alpha(0.6)
            });
        // Breathes only while the Agent works; a settled strip is static.
        let dot = if live {
            dot.with_animation(
                SharedString::from(format!("lens-breathe-{}", strip.key.offset)),
                Animation::new(Duration::from_millis(1400)).repeat(),
                |dot, t| dot.opacity(0.35 + 0.65 * (0.5 - 0.5 * (t * std::f32::consts::TAU).cos())),
            )
            .into_any_element()
        } else {
            dot.into_any_element()
        };
        let edited: Vec<String> = strip
            .edits()
            .flat_map(|call| match &call.detail {
                ToolDetail::Edit { path, .. } | ToolDetail::Write { path, .. } => vec![path.clone()],
                ToolDetail::Patch { patch } => super::model::patch_paths(patch)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take(4)
            .collect();
        let header = div()
            .id(SharedString::from(format!("lens-strip-{}", strip.key.offset)))
            .w_full()
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .rounded(px(Radius::ROW))
            .cursor_pointer()
            .hover(|header| header.bg(colors.primary.alpha(0.04)))
            .child(dot)
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(colors.secondary)
                    .child(SharedString::from(meta)),
            )
            .when(failures > 0, |header| {
                header.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(Ink::DANGER)
                        .child(SharedString::from(tf("thread_lens.failed", &[("count", &failures)]))),
                )
            })
            .child(div().flex_1())
            .children(edited.iter().map(|path| {
                div()
                    .px(px(5.0))
                    .rounded(px(Radius::CHIP))
                    .bg(colors.primary.alpha(0.05))
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(10.5))
                    .text_color(colors.tertiary)
                    .child(SharedString::from(file_name(path)))
            }))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(colors.tertiary)
                    .child(if open { "▾" } else { "▸" }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(open_key, row, cx)));
        let mut strip_el = div().w_full().flex().flex_col().child(header);
        if open {
            let mut body = div()
                .ml(px(13.0))
                .pl(px(14.0))
                .border_l_1()
                .border_color(colors.primary.alpha(0.08))
                .flex()
                .flex_col()
                .gap(px(2.0));
            let image_count: usize = strip
                .calls
                .iter()
                .filter_map(|call| call.result.as_ref())
                .map(|result| result.images.len())
                .sum();
            for call in &strip.calls {
                body = body.child(self.render_call(row, call, image_count >= 2, colors, theme, cx));
            }
            if let Some(compare) = self.render_compare(strip, colors, cx) {
                body = body.child(compare);
            }
            strip_el = strip_el.child(body);
        }
        strip_el.into_any_element()
    }

    fn render_call(
        &mut self,
        row: usize,
        call: &Call,
        paired_images: bool,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (call.key, 0u16);
        let open = self.expanded.contains(&key);
        let (glyph, tint) = match &call.result {
            None => ("◌", colors.tertiary),
            Some(result) if result.is_error => ("✕", Ink::DANGER),
            Some(_) => ("✓", Ink::FRESH),
        };
        let (verb, subject) = call_title(call);
        let line = div()
            .id(SharedString::from(format!("lens-call-{}-{}", call.key.offset, call.key.part)))
            .h(px(26.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(Radius::CHIP))
            .cursor_pointer()
            .hover(|line| line.bg(colors.primary.alpha(0.035)))
            .child(div().w(px(12.0)).text_size(px(11.0)).text_color(tint).child(glyph))
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(verb),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(11.5))
                    .text_color(colors.primary.alpha(0.85))
                    .child(SharedString::from(subject)),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, row, cx)));
        let mut el = div().flex().flex_col().child(line);
        let cards = self.cards_for(call);
        if !cards.is_empty() {
            for card in cards.iter() {
                el = el.child(diff_card(card, colors, theme, cx));
            }
        }
        let is_edit = matches!(call.detail.work(), diri_proto::thread::WorkKind::Edit);
        if open && let Some(result) = &call.result {
            if !result.preview.trim().is_empty() && (!is_edit || result.is_error) {
                el = el.child(
                    div()
                        .my(px(4.0))
                        .p(px(10.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors.primary.alpha(0.04))
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .text_color(if result.is_error { Ink::DANGER } else { colors.secondary })
                        .children(result.preview.lines().take(40).map(|line| {
                            div().whitespace_nowrap().child(SharedString::from(if line.is_empty() {
                                " ".to_owned()
                            } else {
                                line.to_owned()
                            }))
                        })),
                );
            }
            let images: Vec<AnyElement> = result
                .images
                .iter()
                .filter(|_| !paired_images)
                .enumerate()
                .filter_map(|(n, image)| {
                    let decoded = self.image(call.key, 100 + n, image)?;
                    Some(img(decoded).max_w(px(MEASURE - 60.0)).max_h(px(320.0)).rounded(px(Radius::BADGE)).into_any_element())
                })
                .collect();
            el = el.children(images);
        }
        el.into_any_element()
    }

    /// Two screenshots in one strip pair up as before/after with a wipe.
    fn render_compare(&mut self, strip: &WorkStrip, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shots: Vec<(ItemKey, usize, ThreadImage)> = strip
            .calls
            .iter()
            .filter_map(|call| Some((call.key, call.result.as_ref()?)))
            .flat_map(|(key, result)| {
                result
                    .images
                    .iter()
                    .enumerate()
                    .map(move |(n, image)| (key, 100 + n, image.clone()))
            })
            .collect();
        if shots.len() < 2 {
            return None;
        }
        let (before_key, before_n, before) = shots[0].clone();
        let (after_key, after_n, after) = shots[shots.len() - 1].clone();
        let before = self.image(before_key, before_n, &before)?;
        let after = self.image(after_key, after_n, &after)?;
        let width = MEASURE - 60.0;
        let height = 300.0;
        let fraction = *self.wipes.get(&strip.key).unwrap_or(&0.5);
        let strip_key = strip.key;
        Some(
            div()
                .my(px(6.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(colors.tertiary)
                        .child(t("thread_lens.compare")),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("lens-wipe-{}", strip.key.offset)))
                        .relative()
                        .w(px(width))
                        .h(px(height))
                        .rounded(px(Radius::CARD))
                        .overflow_hidden()
                        .cursor_ew_resize()
                        .child(img(after).absolute().top_0().left_0().w(px(width)).h(px(height)))
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .h(px(height))
                                .w(px(width * fraction))
                                .overflow_hidden()
                                .child(img(before).w(px(width)).h(px(height))),
                        )
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left(px(width * fraction - 1.0))
                                .w(px(2.0))
                                .h(px(height))
                                .bg(rgba(0xffffffff).alpha(0.9)),
                        )
                        .on_mouse_move(cx.listener(move |this, event: &gpui::MouseMoveEvent, window, cx| {
                            if event.pressed_button != Some(gpui::MouseButton::Left) {
                                return;
                            }
                            let _ = window;
                            let x = f32::from(event.position.x);
                            // The wipe's own origin is not tracked; approximate
                            // from the measure-centred column.
                            let viewport = f32::from(window.viewport_size().width);
                            let left = ((viewport - MEASURE) / 2.0).max(24.0) + 27.0;
                            let next = ((x - left) / width).clamp(0.0, 1.0);
                            this.wipes.insert(strip_key, next);
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn cards_for(&mut self, call: &Call) -> Arc<Vec<DiffCard>> {
        Arc::clone(
            self.cards
                .entry(call.key)
                .or_insert_with(|| Arc::new(diff::cards(&call.detail))),
        )
    }

    fn image(&mut self, key: ItemKey, index: usize, image: &ThreadImage) -> Option<Arc<Image>> {
        self.images
            .entry((key, index))
            .or_insert_with(|| {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(image.base64.as_deref()?)
                    .ok()?;
                let format = match image.media_type.as_str() {
                    "image/png" => ImageFormat::Png,
                    "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
                    "image/gif" => ImageFormat::Gif,
                    "image/webp" => ImageFormat::Webp,
                    _ => return None,
                };
                Some(Arc::new(Image::from_bytes(format, bytes)))
            })
            .clone()
    }

    // ── Chrome ───────────────────────────────────────────────────────────

    fn render_permission(&self, session: &SessionRecord, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let detail = session.needs_input.as_ref()?;
        if detail.kind != NeedsInputKind::Permission || detail.secret {
            return None;
        }
        let occurred_at = detail.occurred_at.0;
        let title = detail
            .tool_name
            .as_deref()
            .map_or_else(|| t("thread_lens.permission_needed").to_owned(), |tool| tf("thread_lens.allow_tool", &[("tool", &tool)]));
        Some(
            div().mx(px(24.0)).mb(px(10.0)).flex().justify_center().child(
            div()
                .w_full()
                .max_w(px(MEASURE + 40.0))
                .p(px(14.0))
                .rounded(px(Radius::PANEL))
                .bg(colors.floating_fill())
                .border_1()
                .border_color(Ink::ATTENTION.alpha(0.45))
                .shadow_md()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().size(px(8.0)).rounded_full().bg(Ink::ATTENTION))
                        .child(
                            div()
                                .text_size(px(13.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.primary)
                                .child(SharedString::from(title)),
                        ),
                )
                .child(
                    div()
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(12.0))
                        .line_height(px(17.0))
                        .text_color(colors.secondary)
                        .child(SharedString::from(
                            detail
                                .prompt_excerpt
                                .clone()
                                .unwrap_or_else(|| detail.summary.clone()),
                        )),
                )
                .children(self.answer_error.clone().map(|error| {
                    div().text_size(px(11.5)).text_color(Ink::DANGER).child(error)
                }))
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(button("lens-approve", t("thread_lens.approve"), "⌘↩", true, colors).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.answer(BlockerChoice::Approve, occurred_at, cx)
                            }),
                        ))
                        .child(button("lens-deny", t("thread_lens.deny"), "esc", false, colors).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.answer(BlockerChoice::Deny, occurred_at, cx)
                            }),
                        )),
                ))
                .into_any_element(),
        )
    }

    fn render_composer(&self, session: Option<&SessionRecord>, colors: SemanticColors, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.focus.is_focused(window);
        let (display, selection) = self.composer.display(if focused { "▏" } else { "" });
        let placeholder = self.composer.is_empty();
        let working = session.is_some_and(is_working);
        let kind = session.map(|s| s.effective_kind().clone());
        let now_ms = now_ms();
        let warmth = self.model.warmth(now_ms);
        let model_label = warmth
            .as_ref()
            .map(|(model, _)| model_display(model))
            .unwrap_or_else(|| t("thread_lens.model").into());
        let warmth_label = warmth.as_ref().map(|(_, warmth)| warmth_text(*warmth));
        let warm = matches!(warmth, Some((_, CacheWarmth::Warm { .. })));
        let account = session
            .and_then(|s| s.account_profile.as_ref())
            .map(|profile| profile.label.clone());
        let text_el = if placeholder {
            div()
                .text_color(colors.tertiary)
                .child(if focused { format!("▏{}  {}", t("thread_lens.placeholder"), t("thread_lens.placeholder_hint")) } else { t("thread_lens.placeholder").to_owned() })
                .into_any_element()
        } else {
            let mut text = div().text_color(colors.primary);
            match selection {
                Some(range) => {
                    let (a, rest) = display.split_at(range.start);
                    let (b, c) = rest.split_at(range.end - range.start);
                    text = text
                        .flex()
                        .flex_wrap()
                        .child(SharedString::from(a.to_owned()))
                        .child(div().bg(rgba(0x3d7ef099)).child(SharedString::from(b.to_owned())))
                        .child(SharedString::from(c.to_owned()));
                }
                None => text = text.child(SharedString::from(display)),
            }
            text.into_any_element()
        };
        let model_pill = div()
            .id("lens-model-pill")
            .h(px(24.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(12.0))
            .bg(colors.primary.alpha(0.06))
            .hover(|pill| pill.bg(colors.primary.alpha(0.1)))
            .cursor_pointer()
            .child(
                div()
                    .size(px(6.0))
                    .rounded_full()
                    .bg(if warm { Ink::FRESH } else { colors.tertiary.alpha(0.6) }),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.primary)
                    .child(SharedString::from(model_label)),
            )
            .children(warmth_label.map(|label| {
                div()
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(10.5))
                    .text_color(colors.tertiary)
                    .child(SharedString::from(label))
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.model_menu = !this.model_menu;
                cx.notify();
            }));
        let account_pill = div()
            .id("lens-account-pill")
            .h(px(24.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .rounded(px(12.0))
            .hover(|pill| pill.bg(colors.primary.alpha(0.06)))
            .cursor_pointer()
            .text_size(px(11.5))
            .text_color(colors.secondary)
            .child(SharedString::from(account.unwrap_or_else(|| t("thread_lens.default_account").into())))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(ThreadLensEvent::ContinueAccount)));
        let menu = (self.model_menu && kind.as_ref() == Some(&AgentKind::CLAUDE_CODE)).then(|| {
            let mut menu = div()
                .absolute()
                .bottom(px(44.0))
                .left(px(12.0))
                .w(px(260.0))
                .p(px(5.0))
                .rounded(px(Radius::FLOATING_MENU - 4.0))
                .bg(colors.floating_fill())
                .border_1()
                .border_color(colors.floating_stroke())
                .shadow_lg()
                .flex()
                .flex_col();
            for (alias, name, id) in CLAUDE_MODELS {
                let state = warmth_text(self.model.warmth_for(id, now_ms));
                let alias = *alias;
                menu = menu.child(
                    div()
                        .id(SharedString::from(format!("lens-model-{alias}")))
                        .h(px(30.0))
                        .px(px(9.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::ROW))
                        .hover(|row| row.bg(colors.primary.alpha(0.07)))
                        .cursor_pointer()
                        .child(div().flex_1().text_size(px(12.5)).text_color(colors.primary).child(*name))
                        .child(
                            div()
                                .font_family(crate::fonts::mono_family())
                                .text_size(px(10.5))
                                .text_color(colors.tertiary)
                                .child(SharedString::from(state)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.switch_model(alias, cx))),
                );
            }
            menu.into_any_element()
        });
        div()
            .relative()
            .mx(px(24.0))
            .mb(px(14.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .id("lens-composer")
                    .w_full()
                    .max_w(px(MEASURE + 40.0))
                    .p(px(10.0))
                    .rounded(px(16.0))
                    .bg(colors.floating_fill())
                    .border_1()
                    .border_color(if focused { colors.primary.alpha(0.16) } else { colors.primary.alpha(0.08) })
                    .shadow_sm()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .cursor_text()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus.focus(window, cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .min_h(px(PROSE_LEADING))
                            .px(px(4.0))
                            .text_size(px(PROSE_SIZE))
                            .line_height(px(PROSE_LEADING))
                            .child(text_el),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(model_pill)
                            .child(account_pill)
                            .child(div().flex_1())
                            .when(working, |row| {
                                row.child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(colors.tertiary)
                                        .child(t("thread_lens.interrupt")),
                                )
                            })
                            .child(
                                div()
                                    .id("lens-send")
                                    .size(px(26.0))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(if placeholder {
                                        colors.primary.alpha(0.08)
                                    } else {
                                        rgba(ACCENT << 8 | 0xff)
                                    })
                                    .text_color(if placeholder { colors.tertiary } else { rgba(0xffffffff) })
                                    .text_size(px(13.0))
                                    .cursor_pointer()
                                    .child("↑")
                                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                            ),
                    ),
            )
            .children(menu)
            .into_any_element()
    }
}

impl Render for ThreadLens {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("thread-lens");
        let (colors, _) = self.colors();
        let session = self.session();
        let rows = self.model.rows.len();
        let empty = rows == 0;
        let thread = list(
            self.list.clone(),
            cx.processor(|this, index, window, cx| this.render_row(index, window, cx)),
        )
        .size_full();
        let permission = session
            .as_deref()
            .and_then(|session| self.render_permission(session, colors, cx));
        let composer = self.render_composer(session.as_deref(), colors, window, cx);
        div()
            .id("thread-lens")
            .key_context("DiriThreadLens")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.terminal_surface())
            .text_color(colors.primary)
            .font_family(crate::fonts::ui_family())
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .pt(px(8.0))
                    .child(thread)
                    .when(empty, |area| {
                        area.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(13.0))
                                .text_color(colors.tertiary)
                                .child(match self.model.structured {
                                    Some(false) => SharedString::from(
                                        self.model
                                            .reason
                                            .clone()
                                            .unwrap_or_else(|| t("thread_lens.unstructured").into()),
                                    ),
                                    _ => SharedString::from(t("thread_lens.empty")),
                                }),
                        )
                    }),
            )
            .children(permission)
            .child(composer)
    }
}

// ── Pieces ───────────────────────────────────────────────────────────────

fn permission(session: &SessionRecord) -> Option<f64> {
    session
        .needs_input
        .as_ref()
        .filter(|detail| detail.kind == NeedsInputKind::Permission && !detail.secret)
        .map(|detail| detail.occurred_at.0)
}

/// A prompt the GUI cannot answer structurally: a question, a picker, a
/// password, an auth flow. The pane shows the real terminal inline for it.
pub fn needs_portal(session: &SessionRecord) -> bool {
    session
        .needs_input
        .as_ref()
        .is_some_and(|detail| detail.kind != NeedsInputKind::Permission || detail.secret)
}

fn is_working(session: &SessionRecord) -> bool {
    matches!(session.status, SessionStatus::Working)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub fn warmth_text(warmth: CacheWarmth) -> String {
    match warmth {
        CacheWarmth::Warm { remaining_ms, .. } => {
            let seconds = remaining_ms / 1000;
            if seconds >= 60 {
                tf("thread_lens.warm", &[("time", &format!("{}m{:02}s", seconds / 60, seconds % 60))])
            } else {
                tf("thread_lens.warm", &[("time", &format!("{seconds}s"))])
            }
        }
        CacheWarmth::Cold { ever: true } => t("thread_lens.cold_rewrite").into(),
        CacheWarmth::Cold { ever: false } => t("thread_lens.cold_write").into(),
    }
}

pub fn model_display(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    for family in ["opus", "sonnet", "haiku", "fable"] {
        if let Some(rest) = lower.split(&format!("{family}-")).nth(1) {
            let version: Vec<&str> = rest.split(['-', '[']).take_while(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit())).collect();
            let mut name = String::from(&family[..1]).to_ascii_uppercase() + &family[1..];
            if !version.is_empty() {
                name.push(' ');
                name.push_str(&version.join("."));
            }
            return name;
        }
    }
    model.to_owned()
}

fn duration(ms: i64) -> String {
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

fn call_title(call: &Call) -> (&'static str, String) {
    match &call.detail {
        ToolDetail::Command { command } => (t("thread_lens.verb.ran"), command.lines().next().unwrap_or_default().to_owned()),
        ToolDetail::Read { path } => (t("thread_lens.verb.read"), short_path(path)),
        ToolDetail::Search { pattern, path } => (
            t("thread_lens.verb.searched"),
            match path {
                Some(path) => format!("{pattern}  in {}", short_path(path)),
                None => pattern.clone(),
            },
        ),
        ToolDetail::Edit { path, .. } => (t("thread_lens.verb.edited"), short_path(path)),
        ToolDetail::Write { path, .. } => (t("thread_lens.verb.wrote"), short_path(path)),
        ToolDetail::Patch { patch } => (
            t("thread_lens.verb.patched"),
            super::model::patch_paths(patch).join(", "),
        ),
        ToolDetail::Task { description } => (t("thread_lens.verb.delegated"), description.lines().next().unwrap_or_default().to_owned()),
        ToolDetail::Other { input } => (
            t("thread_lens.verb.used"),
            format!("{}  {}", call.name, input.chars().take(80).collect::<String>()),
        ),
    }
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return path.trim_start_matches("./").to_owned();
    }
    format!("…/{}", parts[parts.len() - 3..].join("/"))
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// `src/main.rs`, `crates/a/b.rs:42`, `./x.toml:3:7` — not prose, not URLs.
pub fn looks_like_path(text: &str) -> bool {
    let text = text.trim();
    if text.len() > 240 || text.contains(char::is_whitespace) || text.contains("://") {
        return false;
    }
    let path = text.split(':').next().unwrap_or(text);
    let has_extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(stem, ext)| !stem.is_empty() && (1..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    has_extension && (path.contains('/') || text.contains(':'))
}

fn file_preview(cwd: &str, reference: &str) -> Vec<(usize, String)> {
    let mut parts = reference.split(':');
    let path = parts.next().unwrap_or_default();
    let line = parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1);
    let full = if path.starts_with('/') {
        std::path::PathBuf::from(path)
    } else {
        std::path::Path::new(cwd).join(path)
    };
    let Ok(file) = std::fs::File::open(&full) else {
        return Vec::new();
    };
    use std::io::{BufRead, Read};
    let reader = std::io::BufReader::new(file.take(512 * 1024));
    let first = line.saturating_sub(3).max(1);
    reader
        .lines()
        .map_while(Result::ok)
        .enumerate()
        .map(|(n, text)| (n + 1, text))
        .skip(first - 1)
        .take(9)
        .collect()
}

struct PathPreview {
    preview: Vec<(usize, String)>,
    colors: SemanticColors,
}

impl Render for PathPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let mut body = div()
            .p(px(8.0))
            .rounded(px(Radius::BADGE))
            .bg(colors.floating_fill())
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_md()
            .font_family(crate::fonts::mono_family())
            .text_size(px(11.0))
            .line_height(px(16.0))
            .max_w(px(560.0));
        if self.preview.is_empty() {
            body = body.text_color(colors.tertiary).child(t("thread_lens.not_local"));
        }
        for (number, text) in &self.preview {
            body = body.child(
                div()
                    .flex()
                    .gap(px(10.0))
                    .whitespace_nowrap()
                    .child(div().w(px(32.0)).text_right().text_color(colors.tertiary).child(number.to_string()))
                    .child(div().text_color(colors.secondary).child(SharedString::from(text.clone()))),
            );
        }
        body
    }
}

fn command_chip(name: &str, args: &str, colors: SemanticColors) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_end()
        .child(
            div()
                .px(px(9.0))
                .h(px(24.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(colors.primary.alpha(0.1))
                .font_family(crate::fonts::mono_family())
                .text_size(px(11.5))
                .child(div().text_color(rgba(ACCENT << 8 | 0xff)).child(SharedString::from(name.to_owned())))
                .when(!args.is_empty(), |chip| {
                    chip.child(div().text_color(colors.secondary).child(SharedString::from(args.to_owned())))
                }),
        )
        .into_any_element()
}

fn notice(text: &str, colors: SemanticColors) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.0))
        .child(div().flex_1().h(px(1.0)).bg(colors.primary.alpha(0.07)))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .child(SharedString::from(text.to_owned())),
        )
        .child(div().flex_1().h(px(1.0)).bg(colors.primary.alpha(0.07)))
        .into_any_element()
}

fn divider(segment: &diri_proto::thread::ThreadSegment, colors: SemanticColors) -> AnyElement {
    let model = segment.model.as_deref().map(model_display);
    let text = match segment.cause {
        SegmentCause::Start => t("thread_lens.segment_started").to_owned(),
        SegmentCause::Model => match model {
            Some(model) => tf("thread_lens.segment_model", &[("model", &model)]),
            None => t("thread_lens.segment_model_unknown").to_owned(),
        },
        SegmentCause::Account => match &segment.account {
            Some(account) => tf("thread_lens.segment_account", &[("account", account)]),
            None => t("thread_lens.segment_account_unknown").to_owned(),
        },
        SegmentCause::Handoff => tf("thread_lens.segment_handoff", &[("agent", &segment.kind.id())]),
    };
    let tint = match segment.cause {
        SegmentCause::Account => Ink::ATTENTION,
        SegmentCause::Handoff => rgba(0x8b7cf6ff),
        _ => rgba(ACCENT << 8 | 0xff),
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(div().flex_1().h(px(1.0)).bg(colors.primary.alpha(0.08)))
        .child(
            div()
                .h(px(22.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(11.0))
                .bg(tint.alpha(0.1))
                .child(div().size(px(5.0)).rounded_full().bg(tint))
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.secondary)
                        .child(SharedString::from(text)),
                ),
        )
        .child(div().flex_1().h(px(1.0)).bg(colors.primary.alpha(0.08)))
        .into_any_element()
}

fn button(id: &'static str, label: &'static str, hint: &'static str, primary: bool, colors: SemanticColors) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Radius::ROW))
        .cursor_pointer()
        .when(primary, |b| b.bg(rgba(ACCENT << 8 | 0xff)).text_color(rgba(0xffffffff)))
        .when(!primary, |b| b.bg(colors.primary.alpha(0.07)).text_color(colors.primary))
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .child(label)
        .child(div().text_size(px(10.5)).opacity(0.7).child(hint))
}

fn code_block(language: Option<&str>, code: &str, colors: SemanticColors, theme: &diri_term::theme::TermTheme) -> AnyElement {
    let palette = EditorPalette::from_theme(theme);
    let source_language = language
        .map(|lang| SourceLanguage::from_path(std::path::Path::new(&format!("x.{}", match lang {
            "rust" => "rs",
            "python" => "py",
            "typescript" => "ts",
            "javascript" => "js",
            "bash" | "shell" | "console" => "sh",
            other => other,
        }))))
        .unwrap_or(SourceLanguage::from_path(std::path::Path::new("x.txt")));
    let grammar = syntax::grammar(source_language);
    let mut state = LexState::default();
    let mut lines = div()
        .p(px(12.0))
        .flex()
        .flex_col()
        .font_family(crate::fonts::mono_family())
        .text_size(px(MONO_SIZE))
        .line_height(px(MONO_LEADING))
        .text_color(palette.foreground);
    for line in code.lines().take(400) {
        let (tokens, next) = syntax::lex_line(line, state, grammar);
        state = next;
        let mut row = div().flex().whitespace_nowrap();
        let mut at = 0;
        for token in tokens {
            if token.range.start > at {
                row = row.child(SharedString::from(line[at..token.range.start].to_owned()));
            }
            row = row.child(
                div()
                    .text_color(token_color(&palette, token.kind))
                    .child(SharedString::from(line[token.range.clone()].to_owned())),
            );
            at = token.range.end;
        }
        if at < line.len() {
            row = row.child(SharedString::from(line[at..].to_owned()));
        }
        if line.is_empty() {
            row = row.child(" ");
        }
        lines = lines.child(row);
    }
    div()
        .w_full()
        .rounded(px(Radius::CARD))
        .bg(palette.background)
        .border_1()
        .border_color(colors.primary.alpha(0.07))
        .overflow_hidden()
        .when_some(language.map(str::to_owned), |block, language| {
            block.child(
                div()
                    .h(px(26.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(colors.primary.alpha(0.06))
                    .text_size(px(10.5))
                    .text_color(colors.tertiary)
                    .child(SharedString::from(language)),
            )
        })
        .child(
            div()
                .id(SharedString::from(format!("lens-code-{:x}", fxhash(code))))
                .overflow_x_scroll()
                .child(lines),
        )
        .into_any_element()
}

fn token_color(palette: &EditorPalette, kind: TokenKind) -> Rgba {
    match kind {
        TokenKind::Comment => palette.comment,
        TokenKind::String => palette.string,
        TokenKind::Number => palette.number,
        TokenKind::Keyword => palette.keyword,
        TokenKind::Type => palette.type_,
        TokenKind::Function => palette.function,
        TokenKind::Constant => palette.constant,
        TokenKind::Attribute => palette.attribute,
        TokenKind::Property => palette.property,
        TokenKind::Tag => palette.tag,
        TokenKind::Bracket | TokenKind::Punctuation => palette.punctuation,
    }
}

fn fxhash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A file change as a reviewer reads it: path, counts, unified rows with the
/// changed words washed, and quick actions.
fn diff_card(card: &DiffCard, colors: SemanticColors, theme: &diri_term::theme::TermTheme, cx: &mut Context<ThreadLens>) -> AnyElement {
    let palette = DiffPalette::new(colors, theme);
    let path = card.path.clone();
    let quote = format!("In `{}`: ", card.path);
    let mut rows = div()
        .flex()
        .flex_col()
        .font_family(crate::fonts::mono_family())
        .text_size(px(11.5))
        .line_height(px(17.0));
    for row in card.rows.iter().take(80) {
        let (bg, sign, word_bg) = match row.side {
            Side::Added => (palette.added_line, "+", palette.added_word),
            Side::Removed => (palette.removed_line, "−", palette.removed_word),
            Side::Context => (gpui::transparent_black().into(), " ", gpui::transparent_black().into()),
            Side::Gap => (palette.hunk, "⋯", gpui::transparent_black().into()),
        };
        let mut text = div().flex().whitespace_nowrap();
        if row.words.is_empty() {
            text = text.child(SharedString::from(if row.text.is_empty() { " ".to_owned() } else { row.text.clone() }));
        } else {
            let mut at = 0;
            for range in &row.words {
                if range.start > at {
                    text = text.child(SharedString::from(row.text[at..range.start].to_owned()));
                }
                text = text.child(
                    div()
                        .bg(word_bg)
                        .rounded(px(2.0))
                        .child(SharedString::from(row.text[range.clone()].to_owned())),
                );
                at = range.end;
            }
            if at < row.text.len() {
                text = text.child(SharedString::from(row.text[at..].to_owned()));
            }
        }
        rows = rows.child(
            div()
                .flex()
                .bg(bg)
                .child(
                    div()
                        .w(px(22.0))
                        .flex_none()
                        .text_center()
                        .text_color(match row.side {
                            Side::Added => palette.added,
                            Side::Removed => palette.removed,
                            _ => palette.muted,
                        })
                        .child(sign),
                )
                .child(text.text_color(palette.text)),
        );
    }
    let hidden = card.rows.len().saturating_sub(80) + card.hidden_rows;
    div()
        .my(px(4.0))
        .rounded(px(Radius::CARD))
        .border_1()
        .border_color(colors.primary.alpha(0.08))
        .bg(palette.surface)
        .overflow_hidden()
        .child(
            div()
                .h(px(30.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .border_b_1()
                .border_color(colors.primary.alpha(0.06))
                .child(
                    div()
                        .id(SharedString::from(format!("lens-card-open-{}", card.path)))
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(11.5))
                        .text_color(colors.primary)
                        .cursor_pointer()
                        .hover(|p| p.underline())
                        .child(SharedString::from(card.path.clone()))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ThreadLensEvent::OpenReference(path.clone()));
                        })),
                )
                .child(div().text_size(px(11.0)).text_color(palette.added).child(SharedString::from(format!("+{}", card.added))))
                .child(div().text_size(px(11.0)).text_color(palette.removed).child(SharedString::from(format!("−{}", card.removed))))
                .child(div().flex_1())
                .child(
                    div()
                        .id(SharedString::from(format!("lens-card-quote-{}", card.path)))
                        .px(px(7.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .rounded(px(Radius::CHIP))
                        .text_size(px(10.5))
                        .text_color(colors.secondary)
                        .cursor_pointer()
                        .hover(|b| b.bg(colors.primary.alpha(0.07)))
                        .child(t("thread_lens.quote"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.composer.insert_context(&quote);
                            this.focus.focus(window, cx);
                            cx.notify();
                        })),
                ),
        )
        .child(div().id(SharedString::from(format!("lens-card-rows-{}", card.path))).overflow_x_scroll().child(rows))
        .when(hidden > 0, |card_el| {
            card_el.child(
                div()
                    .px(px(10.0))
                    .py(px(4.0))
                    .text_size(px(10.5))
                    .text_color(colors.tertiary)
                    .child(SharedString::from(tf("thread_lens.more_lines", &[("count", &hidden)]))),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_detection_accepts_references_and_rejects_prose() {
        assert!(looks_like_path("src/main.rs"));
        assert!(looks_like_path("crates/diri-app/src/root.rs:42"));
        assert!(looks_like_path("Cargo.toml:3"));
        assert!(!looks_like_path("cargo test"));
        assert!(!looks_like_path("https://x.dev/a.rs"));
        assert!(!looks_like_path("Option<T>"));
        assert!(!looks_like_path("v1.2"));
    }

    #[test]
    fn model_names_and_warmth_read_naturally() {
        assert_eq!(model_display("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_display("claude-sonnet-5-5-20261001"), "Sonnet 5.5");
        assert_eq!(model_display("gpt-5.5-codex"), "gpt-5.5-codex");
        assert_eq!(
            warmth_text(CacheWarmth::Warm { remaining_ms: 252_000, ttl_ms: 300_000 }),
            "warm 4m12s"
        );
        assert_eq!(warmth_text(CacheWarmth::Cold { ever: false }), "cold write");
    }
}
