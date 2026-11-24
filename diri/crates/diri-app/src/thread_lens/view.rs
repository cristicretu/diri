//! The GUI Lens (Conductor Mode): one live Agent session drawn as an
//! editorial thread, beside the TUI Lens (Pilot Mode) on the same Holder.
//!
//! Everything typed or clicked here goes to the real PTY: prompts through
//! `session.send_text`, permission answers through `session.answer_blocker`
//! (which re-checks that the prompt is still live), model switches as the
//! CLI's own `/model` command. The thread itself comes from the Engine's
//! transcript tail, so this view never scrapes the terminal; a picker the
//! CLI draws instead shows as a live slice of the real grid (the pane's PTY
//! portal) while this view hides its composer.
//!
//! Rendering stays inside the ADEMark budgets: rows are virtualized by
//! GPUI's `list` (only visible rows lay out and shape text, so off-screen
//! rows hold no glyph runs), assistant Markdown is parsed once per record
//! ([`ThreadModel`]), every paragraph is one shaped text run list (so
//! punctuation never wraps away from a bold word or a code span), diff
//! cards and decoded images are cached per item key, and nothing animates
//! or wakes unless the Agent works or the composer has focus.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use diri_proto::thread::{
    BlockerChoice, CacheWarmth, ItemKey, SegmentCause, ThreadImage, ThreadReadParams, ToolDetail,
};
use diri_proto::{AgentKind, NeedsInputKind, SessionId, SessionRecord, SessionStatus};
use diri_ui::{Ink, Radius, SemanticColors};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, ElementId, EventEmitter, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Hsla, Image, ImageFormat, InteractiveText,
    KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, Rgba, SharedString, StrikethroughStyle, StyledText, Task, TextLayout, TextRun,
    UnderlineStyle, WeakEntity, Window, div, img, list, prelude::*, px, rgba,
};
use tokio::runtime::Handle;
use unicode_segmentation::UnicodeSegmentation as _;

use super::diff::{self, DiffCard, Side};
pub use super::model::model_display;
use super::model::{Call, RowChange, Row, ThreadModel, WorkStrip};
use super::setup::{AgentModels, AgentSetup};
use super::slash::{self, SlashCommand};
use crate::code_editor::EditorPalette;
use crate::code_editor::syntax::{self, LexState, TokenKind};
use crate::code_intelligence::SourceLanguage;
use crate::git_ui::palette::DiffPalette;
use crate::i18n::{t, tf};
use crate::markdown::{InlineText, MarkdownBlock, MarkdownDocument};
use crate::notes::editor_view::diagram::{self, Diagrams, Look};
use crate::query_editor::{ClipboardEdit, Edit, LocalEdit, QueryEditor};
use crate::store::StoreRuntime;
use crate::tooltip_warmth::WarmTooltip as _;

/// Prose measure: about 75 characters at the body size.
const MEASURE: f32 = 720.0;
const PROSE_SIZE: f32 = 14.0;
const PROSE_LEADING: f32 = 23.0;
const MONO_SIZE: f32 = 12.0;
const MONO_LEADING: f32 = 18.0;
const ACCENT: u32 = 0xd97757;
const LINK: u32 = 0x5b9cf0;
/// How long a model or effort picked here shows before the CLI confirms.
const PICK_GRACE: Duration = Duration::from_secs(20);
/// How long a newly arrived row takes to fade in.
const FADE_IN: Duration = Duration::from_millis(320);
/// The composer grows with its text up to this, then scrolls.
const COMPOSER_MAX: f32 = 240.0;
/// macOS's caret period.
const BLINK: Duration = Duration::from_millis(530);

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
    /// The thread continues in another Agent's new session (Tier 3).
    HandedOff(SessionId),
}

enum LensMessage {
    Page(Result<diri_proto::thread::ThreadReadResult, String>),
    Answered(Result<(), String>),
    Agents(Vec<AgentModels>),
    HandedOff(Result<SessionId, String>),
}

/// The composer's primary action, by the Agent's state and the draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Send,
    Steer,
    Stop,
}

/// One selectable run of text: a paragraph, a code block, a tool output, a
/// diff row. Ordered as the thread reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SegId {
    key: ItemKey,
    block: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    seg: SegId,
    index: usize,
}

/// Clickable ranges of one text: links and file references.
type Targets = Vec<(Range<usize>, Target)>;

#[derive(Clone, Debug)]
enum Target {
    Url(String),
    Path(String),
}

pub struct ThreadLens {
    session_id: SessionId,
    runtime: Arc<StoreRuntime>,
    tokio: Handle,
    pub(crate) model: ThreadModel,
    list: ListState,
    /// Rows drawn after the model's: pending sends, then the working line.
    shown_extra: usize,
    /// The list is top-aligned until its rows overflow the viewport.
    top_anchored: bool,
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
    /// Text laid out this frame, for hit-testing a selection.
    segments: BTreeMap<SegId, TextLayout>,
    /// Text of every segment drawn so far, so a selection that scrolled
    /// past rows still copies them.
    seg_text: BTreeMap<SegId, SharedString>,
    /// Anchor, then head.
    selection: Option<(TextPoint, TextPoint)>,
    selecting: bool,
    /// The caret's blink phase; solid while typing.
    caret_on: bool,
    typed: bool,
    /// Focus as of the last frame, read by the blink timer.
    focused: bool,
    blink: Option<Task<()>>,
    /// The pane shows the live PTY in place of the composer.
    pub(crate) portal_open: bool,
    slash: Option<Arc<Vec<SlashCommand>>>,
    slash_index: usize,
    /// The composer text the `/` menu was dismissed for.
    slash_dismissed: Option<String>,
    /// The CLI's own settings and model list, read on first use.
    setup: Option<Arc<AgentSetup>>,
    /// The effort and model picked here, shown until the CLI confirms (or
    /// for 20 s, if it never does).
    effort: Option<(String, std::time::Instant)>,
    picked: Option<(String, std::time::Instant)>,
    /// Installed Agents and their models, for the picker's sections.
    agents: Option<Vec<AgentModels>>,
    agents_loading: bool,
    handoff_error: Option<SharedString>,
    /// Follow-up reads after a wake-up, for a JSONL flushed just after it.
    burst: Option<Task<()>>,
    /// The permission mode the terminal's footer shows right now.
    screen_mode: Option<&'static str>,
    /// Re-renders the working line's elapsed seconds while it shows.
    ticker: Option<Task<()>>,
    /// Rows that arrived after the first page, by row id, for their fade-in.
    fresh: HashMap<(ItemKey, u16), std::time::Instant>,
    /// A live slice of the PTY the pane docks above the composer this frame
    /// (a picker the transcript never records).
    dock: Option<AnyElement>,
    /// What a copy button just copied, for its brief "Copied".
    copied: Option<SharedString>,
    weak: WeakEntity<Self>,
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
                    // The file watch, and every Agent hook (tool start and
                    // finish, stop, notification), announce new bytes.
                    diri_proto::EventName::SESSION_THREAD_APPENDED => {
                        event.params.get("sessionID").and_then(|v| v.as_str())
                            == Some(watched.0.as_str())
                    }
                    // Status changes precede and follow every turn, so they
                    // also renew the Engine's watch lease.
                    diri_proto::EventName::SESSION_UPDATED => {
                        event.params.get("id").and_then(|v| v.as_str()) == Some(watched.0.as_str())
                    }
                    _ => false,
                };
                if ours && this.update(cx, |this, cx| this.burst_read(cx)).is_err() {
                    return;
                }
            }
        });
        let list = new_list(ListAlignment::Top, 0, cx.entity().downgrade());
        let mut lens = Self {
            session_id,
            runtime,
            tokio,
            model: ThreadModel::default(),
            list,
            shown_extra: 0,
            top_anchored: true,
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
            segments: BTreeMap::new(),
            seg_text: BTreeMap::new(),
            selection: None,
            selecting: false,
            caret_on: true,
            typed: false,
            focused: false,
            blink: None,
            portal_open: false,
            slash: None,
            slash_index: 0,
            slash_dismissed: None,
            copied: None,
            fresh: HashMap::new(),
            setup: None,
            effort: None,
            picked: None,
            agents: None,
            agents_loading: false,
            handoff_error: None,
            burst: None,
            screen_mode: None,
            ticker: None,
            dock: None,
            weak: cx.entity().downgrade(),
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
        self.list.reset(self.model.rows.len() + self.shown_extra);
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

    /// Sends like Enter would, minus the PTY: the optimistic row appears.
    #[cfg(test)]
    pub(crate) fn send_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.composer.insert_multiline(text);
        self.submit_with(cx, false);
    }

    #[cfg(test)]
    pub(crate) fn select_all_text_for_test(&mut self) -> Option<String> {
        let first = *self.seg_text.keys().next()?;
        let (last, text) = self.seg_text.iter().next_back()?;
        self.selection = Some((
            TextPoint { seg: first, index: 0 },
            TextPoint { seg: *last, index: text.len() },
        ));
        self.selection_text()
    }

    #[cfg(test)]
    pub(crate) fn rows_for_test(&self) -> usize {
        self.list.item_count()
    }

    /// Fixtures pin the CLI setup instead of reading this Mac's settings.
    #[cfg(test)]
    pub(crate) fn set_setup_for_test(&mut self, setup: AgentSetup) {
        self.setup = Some(Arc::new(setup));
    }

    #[cfg(test)]
    pub(crate) fn open_model_menu_for_test(&mut self, agents: Vec<AgentModels>) {
        self.model_menu = true;
        self.agents = Some(agents);
    }

    #[cfg(test)]
    pub(crate) fn composer_is_empty(&self) -> bool {
        self.composer.is_empty()
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn is_structured(&self) -> Option<bool> {
        self.model.structured
    }

    /// Re-reads the transcript from the cursor (the pane calls this once
    /// the terminal settles after output).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.request_read(cx);
    }

    /// The permission mode the terminal's footer shows (the pane reads it
    /// from the live screen), which beats the transcript's last record.
    pub fn set_screen_mode(&mut self, mode: Option<&'static str>) {
        if mode.is_some() {
            self.screen_mode = mode;
        }
    }

    /// The pane's live PTY slice for this frame, docked above the composer
    /// in the same slot as a permission card; `None` once the picker closes.
    pub fn dock_portal(&mut self, portal: Option<AnyElement>, cx: &mut Context<Self>) {
        let open = portal.is_some();
        self.dock = portal;
        if self.portal_open != open {
            self.portal_open = open;
            cx.notify();
        }
    }

    /// A wake-up (file watch, hook, status change): read now, then twice
    /// more within 200 ms, so a reply whose JSONL lands a moment after the
    /// Stop hook still clears the working line at once.
    fn burst_read(&mut self, cx: &mut Context<Self>) {
        self.request_read(cx);
        self.burst = Some(cx.spawn(async move |this, cx| {
            for delay in [60u64, 140] {
                cx.background_executor().timer(Duration::from_millis(delay)).await;
                if this.update(cx, |this, cx| this.request_read(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    /// Esc into the PTY: closes a picker, cancels a prompt, interrupts.
    pub fn send_escape(&mut self, cx: &mut Context<Self>) {
        self.send("\u{1b}".into(), false, cx);
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
            LensMessage::Agents(agents) => {
                self.agents_loading = false;
                self.agents = Some(agents);
                cx.notify();
            }
            LensMessage::HandedOff(Ok(id)) => {
                self.handoff_error = None;
                cx.emit(ThreadLensEvent::HandedOff(id));
                cx.notify();
            }
            LensMessage::HandedOff(Err(error)) => {
                self.handoff_error = Some(SharedString::from(error));
                cx.notify();
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

    /// Rows after the model's own: one per pending send, then the working
    /// line when the Agent is busy and no live strip already says so.
    fn extra_rows(&self) -> usize {
        // A running strip already says what the Agent is doing.
        let live_strip = matches!(self.model.rows.last(), Some(Row::Work(strip)) if strip.pending());
        // Only while a reply is owed: the moment the reply row lands the line
        // goes, whatever the session status still says.
        self.model.pending.len() + usize::from(self.model.awaiting_reply() && self.busy() && !live_strip)
    }

    fn apply_change(&mut self, change: RowChange) {
        let rows = self.model.rows.len();
        let extra = self.extra_rows();
        match change {
            RowChange::None => {
                if extra != self.shown_extra {
                    self.list.splice(rows..rows + self.shown_extra, extra);
                }
            }
            RowChange::Tail { from, old_len } => {
                if old_len > 0 {
                    let now = std::time::Instant::now();
                    self.fresh.retain(|_, born| born.elapsed() < FADE_IN);
                    for row in &self.model.rows[old_len.min(rows)..] {
                        self.fresh.insert(row.id(), now);
                    }
                }
                // Cached cards and images belong to immutable items; only the
                // reopened tail rows need remeasuring.
                self.list
                    .splice(from..old_len + self.shown_extra, rows.saturating_sub(from) + extra);
            }
            RowChange::Rebuilt => {
                if !self.top_anchored && self.model.rows.len() < 4 {
                    self.top_anchored = true;
                    self.list = new_list(ListAlignment::Top, 0, self.weak.clone());
                }
                self.seg_text.clear();
                self.selection = None;
                self.list.reset(rows + extra);
            }
        }
        self.shown_extra = extra;
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
        self.submit_with(cx, true);
    }

    /// Enter: the message shows in the thread this frame (optimistic), the
    /// list re-follows the tail, then the PTY gets it. The transcript
    /// record that lands a moment later replaces the pending row in place.
    fn submit_with(&mut self, cx: &mut Context<Self>, to_pty: bool) {
        let text = self.composer.text().trim_end().to_owned();
        if text.trim().is_empty() {
            return;
        }
        self.composer.clear();
        self.slash_dismissed = None;
        if !text.starts_with('/') {
            self.model.push_pending(&text, now_ms());
            self.apply_change(RowChange::None);
        }
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        if to_pty {
            self.send(text, true, cx);
        }
        cx.notify();
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

    fn slash_matches(&mut self) -> Vec<SlashCommand> {
        let text = self.composer.text().to_owned();
        let Some(query) = slash::query(&text) else {
            return Vec::new();
        };
        if self.slash_dismissed.as_deref() == Some(text.as_str()) {
            return Vec::new();
        }
        let catalog = match &self.slash {
            Some(catalog) => Arc::clone(catalog),
            None => {
                let session = self.session();
                let kind = session.as_ref().map_or(AgentKind::CLAUDE_CODE, |s| s.effective_kind().clone());
                let cwd = session.as_ref().map(|s| std::path::PathBuf::from(&s.cwd)).unwrap_or_default();
                let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
                let catalog = Arc::new(slash::catalog(&kind, &cwd, &home));
                self.slash = Some(Arc::clone(&catalog));
                catalog
            }
        };
        slash::matching(&catalog, query, 8).into_iter().cloned().collect()
    }

    /// Takes the highlighted `/` choice into the composer. A command with
    /// no argument runs straight away on Enter.
    fn complete_slash(&mut self, run: bool, cx: &mut Context<Self>) -> bool {
        let matches = self.slash_matches();
        let Some(choice) = matches.get(self.slash_index.min(matches.len().saturating_sub(1))) else {
            return false;
        };
        self.composer.clear();
        if run && !choice.takes_args {
            self.composer.insert_multiline(&choice.name);
            self.submit(cx);
        } else {
            self.composer.insert_multiline(&format!("{} ", choice.name));
        }
        self.slash_index = 0;
        self.typed(cx);
        true
    }

    /// A keystroke landed: the caret stays solid until the next blink.
    fn typed(&mut self, cx: &mut Context<Self>) {
        self.caret_on = true;
        self.typed = true;
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The docked PTY portal holds the focus inside this view's tree: its
        // keys bubble through here on their way to the terminal. Let them.
        if self.portal_open {
            return;
        }
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let session = self.session();
        let blocker = session.as_ref().and_then(|s| permission(s));
        let slash_open = !self.slash_matches().is_empty();
        match keystroke.key.as_str() {
            "up" | "down" if slash_open && !modifiers.platform => {
                let count = self.slash_matches().len().max(1);
                self.slash_index = if keystroke.key == "up" {
                    (self.slash_index + count - 1) % count
                } else {
                    (self.slash_index + 1) % count
                };
                cx.stop_propagation();
                cx.notify();
                return;
            }
            "tab" if slash_open => {
                self.complete_slash(false, cx);
                cx.stop_propagation();
                return;
            }
            "enter" if slash_open && !modifiers.shift && !modifiers.alt && !modifiers.platform => {
                // The exact command typed out runs; a prefix completes.
                let exact = self
                    .slash_matches()
                    .first()
                    .is_some_and(|c| c.name == self.composer.text().trim());
                if exact && self.slash_index == 0 {
                    self.submit(cx);
                } else {
                    self.complete_slash(true, cx);
                }
                cx.stop_propagation();
                return;
            }
            "enter" if modifiers.platform => {
                if let Some(occurred_at) = blocker {
                    self.answer(BlockerChoice::Approve, occurred_at, cx);
                    cx.stop_propagation();
                    return;
                }
            }
            "enter" if modifiers.shift || modifiers.alt => {
                self.composer.insert_multiline("\n");
                self.typed(cx);
                cx.stop_propagation();
                return;
            }
            "enter" if !modifiers.control && !modifiers.platform => {
                self.submit(cx);
                self.typed(cx);
                cx.stop_propagation();
                return;
            }
            "escape" => {
                if slash_open {
                    self.slash_dismissed = Some(self.composer.text().to_owned());
                } else if self.model_menu {
                    self.model_menu = false;
                } else if self.selection.take().is_some() {
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
                self.slash_index = 0;
                self.selection = None;
                self.typed(cx);
                cx.stop_propagation();
            }
            Some(Edit::Clipboard(ClipboardEdit::Copy)) => {
                // A selection in the thread wins over the composer's.
                if let Some(text) = self.selection_text() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                } else {
                    crate::query_editor::copy_selection(&self.composer, cx);
                }
                cx.stop_propagation();
            }
            Some(Edit::Clipboard(ClipboardEdit::Cut)) => {
                crate::query_editor::cut_selection(&mut self.composer, cx);
                self.typed(cx);
                cx.stop_propagation();
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
                self.typed(cx);
                cx.stop_propagation();
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

    fn open_target(&mut self, target: &Target, cx: &mut Context<Self>) {
        if self.selection_text().is_some() {
            // The click ended a drag that selected text, not a link click.
            return;
        }
        match target {
            Target::Url(url) => cx.open_url(url),
            Target::Path(path) => cx.emit(ThreadLensEvent::OpenReference(path.clone())),
        }
    }

    fn copy_text(&mut self, id: SharedString, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.copied = Some(id.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1200)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied.as_ref() == Some(&id) {
                    this.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ── Selection ────────────────────────────────────────────────────────

    /// The text point under `position`; with `nearest`, the closest one
    /// even between paragraphs (a drag through a gap keeps selecting).
    fn hit(&self, position: Point<Pixels>, nearest: bool) -> Option<TextPoint> {
        let mut above: Option<TextPoint> = None;
        for (seg, layout) in &self.segments {
            let Some(bounds) = layout.try_bounds() else {
                continue;
            };
            if position.y < bounds.top() {
                if nearest {
                    return Some(above.unwrap_or(TextPoint { seg: *seg, index: 0 }));
                }
                continue;
            }
            let len = self.seg_text.get(seg).map_or(0, |text| text.len());
            if position.y <= bounds.bottom() {
                if !nearest && (position.x < bounds.left() - px(8.0) || position.x > bounds.right() + px(8.0)) {
                    continue;
                }
                let index = match layout.index_for_position(position) {
                    Ok(index) | Err(index) => index.min(len),
                };
                return Some(TextPoint { seg: *seg, index });
            }
            above = Some(TextPoint { seg: *seg, index: len });
        }
        above.filter(|_| nearest)
    }

    fn ordered_selection(&self) -> Option<(TextPoint, TextPoint)> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        (start != end).then_some((start, end))
    }

    /// The selected byte range inside `seg`, if the selection touches it.
    fn selected_range(&self, seg: SegId, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered_selection()?;
        if seg < start.seg || seg > end.seg {
            return None;
        }
        let from = if seg == start.seg { start.index.min(len) } else { 0 };
        let to = if seg == end.seg { end.index.min(len) } else { len };
        (from < to).then_some(from..to)
    }

    fn selection_text(&self) -> Option<String> {
        let (start, end) = self.ordered_selection()?;
        let mut out = String::new();
        let mut previous: Option<ItemKey> = None;
        for (seg, text) in self.seg_text.range(start.seg..=end.seg) {
            let len = text.len();
            let mut from = if *seg == start.seg { start.index.min(len) } else { 0 };
            let mut to = if *seg == end.seg { end.index.min(len) } else { len };
            while !text.is_char_boundary(from) {
                from -= 1;
            }
            while !text.is_char_boundary(to) {
                to += 1;
            }
            if let Some(key) = previous {
                out.push('\n');
                if key != seg.key {
                    out.push('\n');
                }
            }
            out.push_str(&text[from..to.max(from)]);
            previous = Some(seg.key);
        }
        (!out.trim().is_empty()).then_some(out)
    }

    fn select_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let Some(point) = self.hit(event.position, false) else {
            if self.selection.take().is_some() {
                cx.notify();
            }
            return;
        };
        let text = self.seg_text.get(&point.seg).cloned().unwrap_or_default();
        self.selection = match event.click_count {
            2 => {
                let word = word_at(&text, point.index);
                Some((
                    TextPoint { seg: point.seg, index: word.start },
                    TextPoint { seg: point.seg, index: word.end },
                ))
            }
            n if n >= 3 => Some((
                TextPoint { seg: point.seg, index: 0 },
                TextPoint { seg: point.seg, index: text.len() },
            )),
            _ => Some((point, point)),
        };
        self.selecting = event.click_count < 2;
        cx.notify();
    }

    fn select_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting &= event.pressed_button == Some(MouseButton::Left);
            return;
        }
        // Dragging past the list's edges scrolls it under the pointer.
        let viewport = self.list.viewport_bounds();
        if event.position.y < viewport.top() + px(8.0) {
            self.list.scroll_by(px(-14.0));
        } else if event.position.y > viewport.bottom() - px(8.0) {
            self.list.scroll_by(px(14.0));
        }
        if let (Some(point), Some((anchor, _))) = (self.hit(event.position, true), self.selection) {
            self.selection = Some((anchor, point));
            cx.notify();
        }
    }

    // ── Rows ─────────────────────────────────────────────────────────────

    fn render_row(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (colors, theme) = self.colors();
        let Some(row) = self.model.rows.get(index).cloned() else {
            return self.render_extra(index - self.model.rows.len(), colors, cx);
        };
        let working = self.session().is_some_and(|s| is_working(&s));
        let last = index + 1 == self.model.rows.len();
        let (key, part) = row.id();
        let body = match &row {
            Row::User { key, text, images } => self.render_user(*key, text, images, colors),
            Row::Command { key, name, args, output } => self.render_command(*key, name, args, output.as_ref(), colors),
            Row::Assistant { key, block, last, text, document } => {
                self.render_assistant(*key, *block, *last, text, document, colors, &theme, cx)
            }
            Row::Work(strip) => self.render_strip(index, strip, last && working, colors, &theme, cx),
            Row::Notice { text, .. } => notice(text, colors),
            Row::Divider(segment) => divider(segment, colors),
        };
        let top = match row {
            Row::Work(_) | Row::Command { .. } => 4.0,
            Row::Divider(_) | Row::Notice { .. } => 14.0,
            Row::User { .. } => 20.0,
            Row::Assistant { block: 0, .. } => 16.0,
            Row::Assistant { .. } => 8.0,
        };
        let row_el = div()
            .id(SharedString::from(format!("lens-row-{}-{}-{part}", key.offset, key.part)))
            .w_full()
            .flex()
            .justify_center()
            .px(px(28.0))
            .pt(px(top))
            .pb(px(4.0))
            .child(div().w_full().max_w(px(MEASURE)).child(body));
        // Rows that arrive while the lens is open fade in on the paint layer
        // (opacity only: nothing re-lays out), once.
        match self.fresh.get(&(key, part)) {
            Some(born) if born.elapsed() < FADE_IN => row_el
                .with_animation(
                    SharedString::from(format!("lens-fresh-{}-{}-{part}", key.offset, key.part)),
                    Animation::new(FADE_IN).with_easing(gpui::ease_out_quint()),
                    |row, t| row.opacity(t),
                )
                .into_any_element(),
            _ => row_el.into_any_element(),
        }
    }

    /// Sent but not yet in the transcript, and the working line.
    /// Re-renders once a second while the working line shows, for its
    /// elapsed time; ends the moment the line goes.
    fn ensure_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep = this.update(cx, |this, cx| {
                    let showing = this.shown_extra > this.model.pending.len();
                    if !showing {
                        this.ticker = None;
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !matches!(keep, Ok(true)) {
                    return;
                }
            }
        }));
    }

    fn render_extra(&mut self, extra: usize, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let body = if let Some(pending) = self.model.pending.get(extra) {
            div()
                .w_full()
                .flex()
                .justify_end()
                .child(
                    bubble(colors)
                        .opacity(0.72)
                        .child(
                            div()
                                .text_size(px(PROSE_SIZE))
                                .line_height(px(PROSE_LEADING))
                                .text_color(colors.primary)
                                .child(SharedString::from(pending.text.to_string())),
                        ),
                )
                .with_animation(
                    SharedString::from(format!("lens-pending-{}", pending.sent_ms)),
                    Animation::new(Duration::from_millis(180)).with_easing(gpui::ease_out_quint()),
                    |bubble, t| bubble.opacity(t).mt(px(6.0 * (1.0 - t))),
                )
                .into_any_element()
        } else {
            let mut text = match self.model.rows.last() {
                Some(Row::Work(strip)) if strip.pending() => strip.activity(),
                _ => t("thread_lens.work.thinking").to_owned(),
            };
            if let Some(since) = self.model.last_user_at() {
                let seconds = now_ms().saturating_sub(since) / 1000;
                if seconds >= 1 {
                    text.push_str(&format!(" · {seconds}s"));
                }
            }
            self.ensure_ticker(cx);
            working_line(&text, colors)
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(28.0))
            .pt(px(if extra < self.model.pending.len() { 20.0 } else { 14.0 }))
            .pb(px(4.0))
            .child(div().w_full().max_w(px(MEASURE)).child(body))
            .into_any_element()
    }

    fn render_user(&mut self, key: ItemKey, text: &str, images: &[ThreadImage], colors: SemanticColors) -> AnyElement {
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
        let text_el = (!text.is_empty()).then(|| {
            let font = ui_font();
            let runs = vec![run(text.len(), &font, colors.primary.into())];
            self.segment(SegId { key, block: 0 }, SharedString::from(text.to_owned()), runs, Vec::new(), colors)
        });
        div()
            .w_full()
            .flex()
            .justify_end()
            .child(
                bubble(colors)
                    .when(!thumbs.is_empty(), |bubble| {
                        bubble.child(div().flex().flex_wrap().gap(px(6.0)).children(thumbs))
                    })
                    .children(text_el.map(|text| {
                        div()
                            .text_size(px(PROSE_SIZE))
                            .line_height(px(PROSE_LEADING))
                            .child(text)
                    })),
            )
            .into_any_element()
    }

    /// `/cost` and its printout, quietly, as the terminal shows them.
    fn render_command(&mut self, key: ItemKey, name: &str, args: &str, output: Option<&(Arc<str>, bool)>, colors: SemanticColors) -> AnyElement {
        let mono = mono_font();
        let output = output.map(|(text, is_error)| {
            let color: Hsla = if *is_error { Ink::DANGER.into() } else { colors.secondary.into() };
            let runs = vec![run(text.len(), &mono, color)];
            self.segment(SegId { key, block: 1 }, SharedString::from(text.to_string()), runs, Vec::new(), colors)
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .font_family(crate::fonts::mono_family())
            .text_size(px(MONO_SIZE))
            .line_height(px(MONO_LEADING))
            .when(!name.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .child(div().text_color(colors.tertiary).child("›"))
                        .child(div().text_color(rgba(ACCENT << 8 | 0xff)).child(SharedString::from(name.to_owned())))
                        .when(!args.is_empty(), |el| el.child(div().text_color(colors.secondary).child(SharedString::from(args.to_owned())))),
                )
            })
            .children(output.map(|output| {
                div()
                    .ml(px(4.0))
                    .pl(px(12.0))
                    .border_l_1()
                    .border_color(colors.primary.alpha(0.08))
                    .child(output)
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_assistant(
        &mut self,
        key: ItemKey,
        block: u16,
        last: bool,
        text: &str,
        document: &MarkdownDocument,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(markdown) = document.blocks.get(usize::from(block)) else {
            return div().into_any_element();
        };
        // Segment numbers stay in reading order across a turn's rows.
        let mut counter = u32::from(block) * 1000;
        let body = self.render_block(key, &mut counter, markdown, colors, theme, cx);
        if !last {
            return body;
        }
        let group = SharedString::from(format!("lens-reply-{}-{}", key.offset, key.part));
        let copy_id = SharedString::from(format!("lens-copy-reply-{}-{}", key.offset, key.part));
        let copied = self.copied.as_ref() == Some(&copy_id);
        let source = text.to_owned();
        div()
            .group(group.clone())
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(body)
            .child(
                div().h(px(20.0)).flex().items_center().child(
                    quiet_button(copy_id.clone(), if copied { t("thread_lens.copied") } else { t("thread_lens.copy") }, colors)
                        .opacity(if copied { 1.0 } else { 0.0 })
                        .group_hover(group, |button| button.opacity(1.0))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_text(copy_id.clone(), source.clone(), cx);
                            cx.stop_propagation();
                        })),
                ),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_block(
        &mut self,
        key: ItemKey,
        block: &mut u32,
        markdown: &MarkdownBlock,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let seg = SegId { key, block: *block };
        *block += 1;
        match markdown {
            MarkdownBlock::Heading { level, content } => {
                let size = match level {
                    1 => 20.0,
                    2 => 17.0,
                    _ => 15.0,
                };
                div()
                    .pt(px(6.0))
                    .text_size(px(size))
                    .line_height(px(size * 1.35))
                    .child(self.prose(seg, content, FontWeight::SEMIBOLD, colors.primary.into(), colors, cx))
                    .into_any_element()
            }
            MarkdownBlock::Paragraph(content) => div()
                .text_size(px(PROSE_SIZE))
                .line_height(px(PROSE_LEADING))
                .child(self.prose(seg, content, FontWeight::NORMAL, colors.primary.alpha(0.92).into(), colors, cx))
                .into_any_element(),
            MarkdownBlock::List {
                ordered,
                start,
                items,
            } => {
                let mut list = div().flex().flex_col().gap(px(5.0));
                for (n, item) in items.iter().enumerate() {
                    let marker = match item.checked {
                        Some(true) => "✓".to_owned(),
                        Some(false) => "○".to_owned(),
                        None if *ordered => format!("{}.", start + n),
                        None => "•".to_owned(),
                    };
                    let item_seg = SegId { key, block: *block };
                    *block += 1;
                    list = list.child(
                        div()
                            .flex()
                            .gap(px(10.0))
                            .text_size(px(PROSE_SIZE))
                            .line_height(px(PROSE_LEADING))
                            .child(
                                div()
                                    .w(px(18.0))
                                    .flex_none()
                                    .text_right()
                                    .text_color(colors.tertiary)
                                    .child(marker),
                            )
                            .child(div().min_w(px(0.0)).flex_1().child(self.prose(
                                item_seg,
                                &item.content,
                                FontWeight::NORMAL,
                                colors.primary.alpha(0.92).into(),
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
                for inner in blocks {
                    quote = quote.child(self.render_block(key, block, inner, colors, theme, cx));
                }
                quote.into_any_element()
            }
            MarkdownBlock::CodeBlock { language, code } => {
                if language.as_deref() == Some("mermaid") {
                    return self.render_mermaid(key, seg.block as usize, code, colors, cx);
                }
                self.code_block(seg, language.as_deref(), code, colors, theme, cx)
            }
            MarkdownBlock::ThematicBreak => div()
                .h(px(1.0))
                .w_full()
                .bg(colors.primary.alpha(0.08))
                .into_any_element(),
        }
    }

    /// One selectable text run list. Registers its layout for this frame's
    /// hit-testing and its text for copying.
    fn segment(
        &mut self,
        seg: SegId,
        text: SharedString,
        runs: Vec<TextRun>,
        targets: Targets,
        colors: SemanticColors,
    ) -> AnyElement {
        let runs = match self.selected_range(seg, text.len()) {
            Some(range) => select_runs(runs, range, selection_color(colors)),
            None => runs,
        };
        let styled = StyledText::new(text.clone()).with_runs(runs);
        self.segments.insert(seg, styled.layout().clone());
        self.seg_text.insert(seg, text);
        if targets.is_empty() {
            return styled.into_any_element();
        }
        let ranges: Vec<Range<usize>> = targets.iter().map(|(range, _)| range.clone()).collect();
        let targets = Arc::new(targets);
        let lens = self.weak.clone();
        let click_targets = Arc::clone(&targets);
        let cwd = self.session().map(|s| s.cwd.clone()).unwrap_or_default();
        InteractiveText::new(
            ElementId::from(SharedString::from(format!("lens-text-{}-{}-{}", seg.key.offset, seg.key.part, seg.block))),
            styled,
        )
        .on_click(ranges, move |index, _, cx| {
            let target = click_targets[index].1.clone();
            let _ = lens.update(cx, |this, cx| this.open_target(&target, cx));
        })
        .tooltip(move |at, _, cx| {
            let (_, Target::Path(reference)) = targets.iter().find(|(range, _)| range.contains(&at))? else {
                return None;
            };
            let preview = file_preview(&cwd, reference);
            Some(cx.new(|_| PathPreview { preview, colors }).into())
        })
        .into_any_element()
    }

    /// Markdown inline spans as one shaped paragraph (see [`inline_runs`]).
    fn prose(&mut self, seg: SegId, content: &InlineText, weight: FontWeight, color: Hsla, colors: SemanticColors, _cx: &mut Context<Self>) -> AnyElement {
        let (text, runs, targets) = inline_runs(content, weight, color, colors);
        self.segment(seg, SharedString::from(text), runs, targets, colors)
    }

    fn code_block(
        &mut self,
        seg: SegId,
        language: Option<&str>,
        code: &str,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = EditorPalette::from_theme(theme);
        let source_language = language
            .map(|lang| SourceLanguage::from_path(std::path::Path::new(&format!("x.{}", match lang {
                "rust" => "rs",
                "python" => "py",
                "typescript" => "ts",
                "javascript" => "js",
                "bash" | "shell" | "console" | "sh" | "zsh" | "fish" => "sh",
                other => other,
            }))))
            .unwrap_or(SourceLanguage::from_path(std::path::Path::new("x.txt")));
        let grammar = syntax::grammar(source_language);
        let mono = mono_font();
        let mut state = LexState::default();
        let mut text = String::new();
        let mut runs = Vec::new();
        for (n, line) in code.lines().take(400).enumerate() {
            if n > 0 {
                text.push('\n');
                runs.push(run(1, &mono, palette.foreground.into()));
            }
            let (tokens, next) = syntax::lex_line(line, state, grammar);
            state = next;
            let mut at = 0;
            for token in tokens {
                if token.range.start > at {
                    runs.push(run(token.range.start - at, &mono, palette.foreground.into()));
                }
                runs.push(run(token.range.len(), &mono, token_color(&palette, token.kind).into()));
                at = token.range.end;
            }
            if at < line.len() {
                runs.push(run(line.len() - at, &mono, palette.foreground.into()));
            }
            text.push_str(line);
        }
        let group = SharedString::from(format!("lens-code-{}-{}-{}", seg.key.offset, seg.key.part, seg.block));
        let copy_id = SharedString::from(format!("{group}-copy"));
        let copied = self.copied.as_ref() == Some(&copy_id);
        let source = code.to_owned();
        let body = self.segment(seg, SharedString::from(text), runs, Vec::new(), colors);
        div()
            .group(group.clone())
            .relative()
            .w_full()
            .rounded(px(Radius::CARD))
            .bg(palette.background)
            .border_1()
            .border_color(colors.primary.alpha(0.07))
            .overflow_hidden()
            .child(
                div()
                    .id(SharedString::from(format!("{group}-scroll")))
                    .overflow_x_scroll()
                    .child(
                        div()
                            .px(px(14.0))
                            .pt(px(if language.is_some() { 26.0 } else { 12.0 }))
                            .pb(px(12.0))
                            .whitespace_nowrap()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(MONO_SIZE))
                            .line_height(px(MONO_LEADING))
                            .child(body),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .left(px(14.0))
                    .right(px(8.0))
                    .flex()
                    .items_center()
                    .children(language.map(|language| {
                        div()
                            .text_size(px(10.5))
                            .text_color(colors.tertiary)
                            .child(SharedString::from(language.to_owned()))
                    }))
                    .child(div().flex_1())
                    .child(
                        quiet_button(copy_id.clone(), if copied { t("thread_lens.copied") } else { t("thread_lens.copy") }, colors)
                            .opacity(if copied { 1.0 } else { 0.0 })
                            .group_hover(group, |button| button.opacity(1.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_text(copy_id.clone(), source.clone(), cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
            .into_any_element()
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
            .p(px(16.0))
            .rounded(px(Radius::CARD))
            .border_1()
            .border_color(colors.primary.alpha(0.07))
            .bg(colors.primary.alpha(0.02))
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
            Look::Failed(message, _) => {
                let theme = self.colors().1;
                let seg = SegId { key, block: 10_000 + index as u32 };
                self.code_block(seg, Some("mermaid"), &format!("{code}\n\n%% {message}"), colors, &theme, cx)
            }
        }
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
        let running = live && strip.pending();
        let mut meta = if running { strip.activity() } else { strip.summary() };
        if !running
            && let Some(ms) = strip.duration_ms()
            && ms >= 100
        {
            meta.push_str(&format!(" · {}", duration(ms)));
        }
        let dot = div()
            .size(px(6.0))
            .rounded_full()
            .bg(if failures > 0 {
                Ink::DANGER
            } else if running {
                rgba(ACCENT << 8 | 0xff)
            } else {
                colors.tertiary.alpha(0.55)
            });
        // Breathes only while the Agent works; a settled strip is static.
        let dot = if running {
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
            .h(px(28.0))
            .px(px(8.0))
            .ml(px(-8.0))
            .flex()
            .items_center()
            .gap(px(9.0))
            .rounded(px(Radius::ROW))
            .cursor_pointer()
            .hover(|header| header.bg(colors.primary.alpha(0.035)))
            .child(dot)
            .child(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.5))
                    .text_color(if running { colors.primary.alpha(0.8) } else { colors.secondary })
                    .child(SharedString::from(meta)),
            )
            .when(failures > 0, |header| {
                header.child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(Ink::DANGER)
                        .child(SharedString::from(tf("thread_lens.failed", &[("count", &failures)]))),
                )
            })
            .child(div().flex_1())
            .when(!running, |header| {
                header.children(edited.iter().map(|path| {
                    div()
                        .flex_none()
                        .px(px(5.0))
                        .rounded(px(Radius::CHIP))
                        .bg(colors.primary.alpha(0.05))
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(10.5))
                        .text_color(colors.tertiary)
                        .child(SharedString::from(file_name(path)))
                }))
            })
            .child(
                crate::icons::sf_symbol(if open { "chevron.down" } else { "chevron.right" }, 9.0, colors.tertiary),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(open_key, row, cx)));
        let mut strip_el = div().w_full().flex().flex_col().child(header);
        if open {
            let mut body = div()
                .ml(px(2.0))
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
        for (n, card) in cards.iter().enumerate() {
            el = el.child(self.diff_card(call.key, n, card, colors, theme, cx));
        }
        let is_edit = matches!(call.detail.work(), diri_proto::thread::WorkKind::Edit);
        if open && let Some(result) = &call.result {
            if !result.preview.trim().is_empty() && (!is_edit || result.is_error) {
                let preview: String = result.preview.lines().take(40).collect::<Vec<_>>().join("\n");
                let color: Hsla = if result.is_error { Ink::DANGER.into() } else { colors.secondary.into() };
                let runs = vec![run(preview.len(), &mono_font(), color)];
                let output = self.segment(SegId { key: call.key, block: 1 }, SharedString::from(preview), runs, Vec::new(), colors);
                el = el.child(
                    div()
                        .id(SharedString::from(format!("lens-output-{}-{}", call.key.offset, call.key.part)))
                        .my(px(4.0))
                        .p(px(10.0))
                        .rounded(px(Radius::BADGE))
                        .bg(colors.primary.alpha(0.035))
                        .overflow_x_scroll()
                        .whitespace_nowrap()
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .child(output),
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
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                            if event.pressed_button != Some(MouseButton::Left) {
                                return;
                            }
                            let x = f32::from(event.position.x);
                            // The wipe's own origin is not tracked; approximate
                            // from the measure-centred column.
                            let viewport = f32::from(window.viewport_size().width);
                            let left = ((viewport - MEASURE) / 2.0).max(28.0) + 27.0;
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

    /// A file change as a reviewer reads it: path, counts, unified rows with
    /// the changed words washed, and quick actions. Each row's text is
    /// selectable.
    fn diff_card(
        &mut self,
        key: ItemKey,
        index: usize,
        card: &DiffCard,
        colors: SemanticColors,
        theme: &diri_term::theme::TermTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = DiffPalette::new(colors, theme);
        let path = card.path.clone();
        let quote = format!("In `{}`: ", card.path);
        let mono = mono_font();
        let mut rows = div()
            .flex()
            .flex_col()
            .font_family(crate::fonts::mono_family())
            .text_size(px(11.5))
            .line_height(px(17.0));
        for (n, row) in card.rows.iter().take(80).enumerate() {
            let (bg, sign, word_bg) = match row.side {
                Side::Added => (Some(palette.added_line), "+", Some(palette.added_word)),
                Side::Removed => (Some(palette.removed_line), "−", Some(palette.removed_word)),
                Side::Context => (None, " ", None),
                Side::Gap => (Some(palette.hunk), "⋯", None),
            };
            let text = if row.text.is_empty() { " ".to_owned() } else { row.text.clone() };
            let mut runs = Vec::new();
            let mut at = 0;
            for range in &row.words {
                if range.start > at {
                    runs.push(run(range.start - at, &mono, palette.text.into()));
                }
                let mut word = run(range.len(), &mono, palette.text.into());
                word.background_color = word_bg.map(Into::into);
                runs.push(word);
                at = range.end;
            }
            if at < text.len() {
                runs.push(run(text.len() - at, &mono, palette.text.into()));
            }
            let seg = SegId { key, block: 100 + (index as u32) * 1000 + n as u32 };
            let text_el = self.segment(seg, SharedString::from(text), runs, Vec::new(), colors);
            rows = rows.child(
                div()
                    .flex()
                    .when_some(bg, |row, bg| row.bg(bg))
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
                    .child(div().whitespace_nowrap().child(text_el)),
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
                            .id(SharedString::from(format!("lens-card-open-{}-{index}", key.offset)))
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
                        quiet_button(
                            SharedString::from(format!("lens-card-quote-{}-{index}", key.offset)),
                            t("thread_lens.quote"),
                            colors,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.composer.insert_context(&quote);
                            this.focus.focus(window, cx);
                            this.typed(cx);
                        })),
                    ),
            )
            .child(div().id(SharedString::from(format!("lens-card-rows-{}-{index}", key.offset))).overflow_x_scroll().child(rows))
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
            div().mx(px(28.0)).mb(px(10.0)).flex().justify_center().child(
                div()
                    .w_full()
                    .max_w(px(MEASURE))
                    .p(px(14.0))
                    .rounded(px(Radius::PANEL))
                    .bg(colors.floating_fill())
                    .border_1()
                    .border_color(Ink::ATTENTION.alpha(0.4))
                    .shadow_md()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().size(px(7.0)).rounded_full().bg(Ink::ATTENTION))
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
                    ),
            )
            .into_any_element(),
        )
    }

    /// The composer's text as one run list: the caret is a run of its own
    /// (accent, or transparent between blinks, so nothing reflows), the
    /// selection a background.
    fn composer_text(&self, focused: bool, kind: &AgentKind, colors: SemanticColors) -> AnyElement {
        let font = ui_font();
        if self.composer.is_empty() {
            let agent = if kind == &AgentKind::CODEX { "Codex" } else { "Claude" };
            let placeholder = tf("thread_lens.placeholder", &[("agent", &agent)]);
            let placeholder = placeholder.as_str();
            let mut text = String::new();
            let mut runs = Vec::new();
            if focused {
                text.push('▏');
                runs.push(run('▏'.len_utf8(), &font, caret_color(self.caret_on, colors)));
            }
            text.push_str(placeholder);
            runs.push(run(placeholder.len(), &font, colors.tertiary.alpha(0.8).into()));
            return StyledText::new(text).with_runs(runs).into_any_element();
        }
        let caret = if focused { "▏" } else { "" };
        let (display, selection) = self.composer.display(caret);
        let mut runs = vec![run(display.len(), &font, colors.primary.into())];
        if let Some(range) = selection {
            runs = select_runs(runs, range, selection_color(colors));
        } else if focused {
            let at = self.composer.cursor();
            let caret_range = at..at + caret.len();
            runs = split_runs(runs, caret_range, |run| run.color = caret_color(self.caret_on, colors));
        }
        StyledText::new(display).with_runs(runs).into_any_element()
    }

    /// The CLI's own setup (model, effort, mode, model list), read once.
    fn setup(&mut self) -> Arc<AgentSetup> {
        if let Some(setup) = &self.setup {
            return Arc::clone(setup);
        }
        let session = self.session();
        let kind = session.as_ref().map_or(AgentKind::CLAUDE_CODE, |s| s.effective_kind().clone());
        let cwd = session.as_ref().map(|s| std::path::PathBuf::from(&s.cwd)).unwrap_or_default();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
        let setup = Arc::new(super::setup::read(&kind, &cwd, &home));
        self.setup = Some(Arc::clone(&setup));
        setup
    }

    /// The Agent is busy with this thread: its status says so, or it owes a
    /// reply to a message sent in the last half minute (the status lags the
    /// keystroke by a hook round trip).
    fn busy(&self) -> bool {
        if self.session().is_some_and(|s| is_working(&s)) {
            return true;
        }
        self.model.awaiting_reply()
            && self
                .model
                .last_user_at()
                .is_some_and(|at| now_ms().saturating_sub(at) < 30_000)
    }

    /// The model the pill names: what was picked here until the CLI
    /// confirms it (or 20 s pass), then what the CLI confirmed (its `/model`
    /// output or the last request), else what its settings say it will use.
    /// The startup default never overrides a pick or a confirmation.
    fn model_label(&mut self, setup: &AgentSetup) -> String {
        let confirmed = self
            .model
            .selected_model()
            .map(str::to_owned)
            .or_else(|| setup.model.as_deref().map(|id| setup.label_for(id)));
        if self.picked.as_ref().is_some_and(|(label, at)| {
            self.model.selected_model() == Some(label.as_str()) || at.elapsed() > PICK_GRACE
        }) {
            self.picked = None;
        }
        self.picked
            .as_ref()
            .map(|(label, _)| label.clone())
            .or(confirmed)
            .unwrap_or_else(|| setup.label_for("default"))
    }

    /// The effort the pill names, by the same rule as the model.
    fn effort_level(&mut self, setup: &AgentSetup) -> Option<String> {
        if self.effort.as_ref().is_some_and(|(level, at)| {
            self.model.effort() == Some(level.as_str()) || at.elapsed() > PICK_GRACE
        }) {
            self.effort = None;
        }
        self.effort
            .as_ref()
            .map(|(level, _)| level.clone())
            .or_else(|| self.model.effort().map(str::to_owned))
            .or_else(|| setup.effort.clone())
    }

    /// A model picked in the picker, before the CLI says anything.
    #[cfg(test)]
    pub(crate) fn pick_for_test(&mut self, label: &str) {
        self.picked = Some((label.to_owned(), std::time::Instant::now()));
    }

    fn render_composer(&mut self, session: Option<&SessionRecord>, colors: SemanticColors, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.focus.is_focused(window);
        self.ensure_blink(focused, cx);
        let dark = colors.lightness() < 0.5;
        let empty = self.composer.is_empty();
        let busy = self.busy();
        let kind = session.map_or(AgentKind::CLAUDE_CODE, |s| s.effective_kind().clone());
        let claude = kind != AgentKind::CODEX;
        let setup = self.setup();
        let now_ms = now_ms();
        let model_label = self.model_label(&setup);
        let warm = matches!(self.model.warmth(now_ms), Some((_, CacheWarmth::Warm { .. })));
        let effort = self.effort_level(&setup);
        let mode = self
            .screen_mode
            .map(str::to_owned)
            .or_else(|| self.model.mode().map(str::to_owned))
            .or_else(|| setup.mode.clone());
        let pill = |id: &'static str| {
            div()
                .id(id)
                .h(px(26.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .rounded(px(8.0))
                .text_size(px(12.0))
                .text_color(colors.secondary)
                .cursor_pointer()
                .hover(|pill| pill.bg(colors.primary.alpha(0.06)).text_color(colors.primary))
        };
        let icon_button = |id: &'static str, symbol: &'static str| {
            div()
                .id(id)
                .size(px(26.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.0))
                .cursor_pointer()
                .hover(|button| button.bg(colors.primary.alpha(0.06)))
                .child(crate::icons::sf_symbol(symbol, 13.0, colors.tertiary))
        };
        let attach = icon_button("lens-attach", "plus").on_click(cx.listener(|this, _, window, cx| this.attach(window, cx)));
        let slash = div()
            .id("lens-slash-button")
            .size(px(26.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .cursor_pointer()
            .hover(|button| button.bg(colors.primary.alpha(0.06)))
            .font_family(crate::fonts::mono_family())
            .text_size(px(13.0))
            .text_color(colors.tertiary)
            .child("/")
            .on_click(cx.listener(|this, _, window, cx| {
            this.composer.clear();
            this.composer.insert_multiline("/");
            this.slash_dismissed = None;
            this.focus.focus(window, cx);
            this.typed(cx);
        }));
        let model_pill = pill("lens-model-pill")
            .when(self.model_menu, |pill| pill.bg(colors.primary.alpha(0.07)).text_color(colors.primary))
            .when(warm, |pill| pill.child(div().size(px(5.0)).rounded_full().bg(Ink::FRESH)))
            .child(SharedString::from(model_label.clone()))
            .children(effort.as_deref().map(|effort| {
                div()
                    .text_color(colors.tertiary)
                    .child(SharedString::from(effort_label(effort)))
            }))
            .child(crate::icons::sf_symbol("chevron.down", 8.0, colors.tertiary))
            .on_click(cx.listener(|this, _, _, cx| {
                this.model_menu = !this.model_menu;
                if this.model_menu {
                    this.load_agents(cx);
                }
                cx.notify();
            }));
        // Shift+Tab, the CLI's own mode key; the footer it redraws names
        // the new mode on the next frame.
        let mode_pill = (claude && mode.is_some()).then(|| {
            let mode = mode.clone().unwrap_or_default();
            pill("lens-mode-pill")
                .child(div().size(px(5.0)).rounded_full().bg(mode_tint(&mode, colors)))
                .child(SharedString::from(mode_label(&mode)))
                .on_click(cx.listener(|this, _, _, cx| this.send("\u{1b}[Z".into(), false, cx)))
        });
        let ring = self.model.context().map(|(used, window)| context_ring(used, window, colors));
        // The primary action follows the live state: Send when idle, Stop
        // (Esc into the PTY, as the terminal's own interrupt) while the Agent
        // works with nothing typed, Steer while it works and you type, which
        // delivers the message mid-turn the way the CLI queues it.
        let action = match (busy, empty) {
            (true, true) => Action::Stop,
            (true, false) => Action::Steer,
            (false, _) => Action::Send,
        };
        let send = div()
            .id("lens-send")
            .h(px(28.0))
            .min_w(px(28.0))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(5.0))
            .when(action == Action::Steer, |button| button.px(px(11.0)))
            .bg(match action {
                Action::Stop => colors.primary.alpha(0.9),
                Action::Send if empty => colors.primary.alpha(0.08),
                _ => rgba(ACCENT << 8 | 0xff),
            })
            .cursor_pointer()
            .child(match action {
                Action::Stop => div()
                    .size(px(9.0))
                    .rounded(px(2.0))
                    .bg(colors.terminal_surface())
                    .into_any_element(),
                Action::Steer => div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgba(0xffffffff))
                    .child(t("thread_lens.steer"))
                    .into_any_element(),
                Action::Send => crate::icons::sf_symbol(
                    "arrow.up",
                    13.0,
                    if empty { colors.tertiary } else { rgba(0xffffffff) },
                ),
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if action == Action::Stop {
                    this.send("\u{1b}".into(), false, cx);
                } else {
                    this.submit(cx);
                }
            }))
            .with_animation(
                SharedString::from(format!("lens-action-{action:?}")),
                Animation::new(Duration::from_millis(160)).with_easing(gpui::ease_out_quint()),
                |button, t| button.opacity(0.5 + 0.5 * t),
            );
        let menu = self.model_menu.then(|| self.render_model_menu(&kind, &model_label, effort.as_deref(), colors, cx));
        let slash_menu = self.render_slash(colors, cx);
        let border = if focused {
            colors.primary.alpha(if dark { 0.2 } else { 0.22 })
        } else {
            colors.primary.alpha(if dark { 0.09 } else { 0.1 })
        };
        div()
            .relative()
            .px(px(28.0))
            .pb(px(18.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .id("lens-composer")
                    .relative()
                    .w_full()
                    .max_w(px(MEASURE))
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(8.0))
                    .rounded(px(16.0))
                    .bg(if dark { colors.primary.alpha(0.045) } else { gpui::white().into() })
                    .border_1()
                    .border_color(border)
                    .shadow(vec![gpui::BoxShadow {
                        color: gpui::black().opacity(if dark { 0.28 } else { 0.06 }),
                        offset: gpui::point(px(0.0), px(4.0)),
                        blur_radius: px(16.0),
                        spread_radius: px(0.0),
                        inset: false,
                    }])
                    // Keys go to the picker while the portal is docked.
                    .when(self.portal_open, |composer| composer.opacity(0.45))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .cursor_text()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.typed(cx);
                    }))
                    // A hairline of light along the top edge, as a lit surface.
                    .when(dark, |composer| {
                        composer.child(
                            div()
                                .absolute()
                                .top_0()
                                .left(px(14.0))
                                .right(px(14.0))
                                .h(px(1.0))
                                .bg(gpui::white().opacity(0.05)),
                        )
                    })
                    .children(menu.or(slash_menu).map(|menu| {
                        // Rises from the composer's own top-left corner.
                        div()
                            .absolute()
                            .left(px(-1.0))
                            .bottom(gpui::relative(1.0))
                            .pb(px(8.0))
                            .child(menu)
                    }))
                    .child(
                        div()
                            .min_h(px(22.0))
                            .max_h(px(COMPOSER_MAX))
                            .overflow_hidden()
                            .text_size(px(PROSE_SIZE))
                            .line_height(px(22.0))
                            .child(self.composer_text(focused, &kind, colors)),
                    )
                    .child(
                        div()
                            .h(px(28.0))
                            .ml(px(-6.0))
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .child(attach)
                            .child(slash)
                            .child(model_pill)
                            .children(mode_pill)
                            .child(div().flex_1())
                            .children(ring)
                            .child(div().w(px(6.0)))
                            .child(send),
                    ),
            )
            .into_any_element()
    }

    /// Installed Agents and their models, read once off the UI thread: the
    /// Engine's readiness scan says which CLIs are installed (and where),
    /// then each lists its models (Antigravity's takes a round trip).
    fn load_agents(&mut self, _cx: &mut Context<Self>) {
        if self.agents.is_some() || self.agents_loading {
            return;
        }
        self.agents_loading = true;
        let client = Arc::clone(self.runtime.client());
        let messages = self.messages.clone();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
        self.tokio.spawn(async move {
            let readiness = client
                .agent_readiness(diri_proto::AgentReadinessParams::default())
                .await
                .ok();
            let installed: Vec<(AgentKind, Option<std::path::PathBuf>)> = super::setup::PICKER_AGENTS
                .iter()
                .filter_map(|id| {
                    let item = readiness.as_ref()?.agents.iter().find(|item| item.kind.id() == *id)?;
                    let path = item.path.clone()?;
                    Some((item.kind.clone(), Some(std::path::PathBuf::from(path))))
                })
                .collect();
            let agents = tokio::task::spawn_blocking(move || {
                installed
                    .into_iter()
                    .map(|(kind, binary)| AgentModels {
                        label: super::setup::agent_label(&kind),
                        models: super::setup::models_for(&kind, &home, binary.as_deref()),
                        kind,
                    })
                    .filter(|agent| !agent.models.is_empty())
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let _ = messages.send(LensMessage::Agents(agents));
        });
    }

    /// Tier 3 from the picker: a model on another Agent continues this
    /// thread there (capsule, new session on that model, Handoff divider).
    fn handoff(&mut self, kind: AgentKind, model: String, cx: &mut Context<Self>) {
        self.model_menu = false;
        self.handoff_error = None;
        let goal = (!self.composer.is_empty()).then(|| self.composer.text().trim().to_owned());
        let params = diri_proto::thread::HandoffParams {
            session_id: self.session_id.clone(),
            kind,
            model: Some(model),
            goal,
        };
        let client = Arc::clone(self.runtime.client());
        let messages = self.messages.clone();
        self.tokio.spawn(async move {
            let result = client
                .handoff(&params)
                .await
                .map(|result| result.session_id)
                .map_err(|error| error.to_string());
            let _ = messages.send(LensMessage::HandedOff(result));
        });
        cx.notify();
    }

    /// The model picker: this session's Agent first (switches in place),
    /// then every other installed Agent (a pick there hands the thread
    /// off), then effort.
    fn render_model_menu(
        &mut self,
        kind: &AgentKind,
        current: &str,
        effort: Option<&str>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let setup = self.setup();
        let claude = kind != &AgentKind::CODEX;
        let now_ms = now_ms();
        let section = |label: SharedString, note: Option<&'static str>| {
            div()
                .flex_none()
                .px(px(10.0))
                .pt(px(8.0))
                .pb(px(4.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(10.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.tertiary)
                .child(label)
                .children(note.map(|note| div().font_weight(FontWeight::NORMAL).text_color(colors.tertiary.alpha(0.7)).child(note)))
        };
        // This Agent's models, plus any this thread used that the list lacks.
        let mut own = setup.models.clone();
        for usage in self.model.usage_models() {
            if !own.iter().any(|c| c.label == setup.label_for(&usage)) {
                own.push(super::setup::ModelChoice { label: setup.label_for(&usage), id: usage, detail: String::new() });
            }
        }
        let mut list = div()
            .id("lens-model-list")
            .max_h(px(380.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(section(
                SharedString::from(super::setup::agent_label(kind)),
                Some(t("thread_lens.menu_this_session")),
            ));
        for choice in own.iter().take(12) {
            let selected = choice.label == current;
            let warmth = (claude && choice.id != "default").then(|| self.model.warmth_for(&claude_model_id(&choice.id), now_ms));
            let id = choice.id.clone();
            let label = choice.label.clone();
            list = list.child(
                model_row(&format!("own-{}", choice.id), choice, selected, warmth, colors).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.model_menu = false;
                        if claude {
                            // Shown at once; the CLI's "Set model to …" confirms it.
                            this.picked = Some((label.clone(), std::time::Instant::now()));
                            this.send(format!("/model {id}"), true, cx);
                        } else {
                            // Codex switches in its own picker, docked as the portal.
                            this.send("/model".into(), true, cx);
                        }
                    },
                )),
            );
        }
        let others: Vec<AgentModels> = self
            .agents
            .iter()
            .flatten()
            .filter(|agent| &agent.kind != kind)
            .cloned()
            .collect();
        for agent in others {
            list = list.child(section(SharedString::from(agent.label), Some(t("thread_lens.menu_hand_off"))));
            for choice in agent.models.iter().take(8) {
                let target = agent.kind.clone();
                let id = choice.id.clone();
                list = list.child(
                    model_row(&format!("{}-{}", agent.kind.id(), choice.id), choice, false, None, colors).on_click(
                        cx.listener(move |this, _, _, cx| this.handoff(target.clone(), id.clone(), cx)),
                    ),
                );
            }
        }
        if self.agents_loading {
            list = list.child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(t("thread_lens.menu_loading_agents")),
            );
        }
        let efforts = setup.efforts.clone();
        let mut segments = div()
            .mx(px(8.0))
            .p(px(2.0))
            .flex()
            .rounded(px(9.0))
            .bg(colors.primary.alpha(0.05));
        for level in efforts {
            let active = effort == Some(level.as_str());
            let chosen = level.clone();
            segments = segments.child(
                div()
                    .id(SharedString::from(format!("lens-effort-{level}")))
                    .flex_1()
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .when(active, |seg| {
                        seg.bg(colors.terminal_surface())
                            .shadow_sm()
                            .text_color(colors.primary)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!active, |seg| seg.text_color(colors.tertiary).hover(|seg| seg.text_color(colors.primary)))
                    .child(SharedString::from(effort_label(&level)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model_menu = false;
                        if claude {
                            this.effort = Some((chosen.clone(), std::time::Instant::now()));
                            this.send(format!("/effort {chosen}"), true, cx);
                        } else {
                            this.send("/model".into(), true, cx);
                        }
                    })),
            );
        }
        div()
            .w(px(380.0))
            .p(px(6.0))
            .rounded(px(14.0))
            .bg(menu_surface(colors))
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(list)
            .child(div().mx(px(10.0)).my(px(6.0)).h(px(1.0)).bg(colors.primary.alpha(0.07)))
            .child(section(SharedString::from(t("thread_lens.menu_effort")), None))
            .child(segments)
            .child(div().h(px(6.0)))
            .into_any_element()
    }

    /// `+`: files become `@path` mentions the CLI reads itself.
    fn attach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.session().map(|s| std::path::PathBuf::from(&s.cwd)).unwrap_or_default();
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: None,
        });
        let _ = window;
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                for path in paths {
                    let shown = path.strip_prefix(&cwd).unwrap_or(&path);
                    this.composer.insert_context(&format!("@{} ", shown.display()));
                }
                this.typed(cx);
            });
        })
        .detach();
    }

    /// The `/` menu above the composer.
    fn render_slash(&mut self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return None;
        }
        let selected = self.slash_index.min(matches.len() - 1);
        let mut menu = div()
            .w(px(400.0))
            .p(px(5.0))
            .rounded(px(Radius::FLOATING_MENU - 4.0))
            .bg(menu_surface(colors))
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow_lg()
            .flex()
            .flex_col();
        for (index, command) in matches.iter().enumerate() {
            let active = index == selected;
            menu = menu.child(
                div()
                    .id(SharedString::from(format!("lens-slash-{}", command.name)))
                    .h(px(30.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .rounded(px(Radius::ROW))
                    .when(active, |row| row.bg(colors.primary.alpha(0.08)))
                    .hover(|row| row.bg(colors.primary.alpha(0.06)))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex_none()
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(12.0))
                            .text_color(colors.primary)
                            .child(SharedString::from(command.name.clone())),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.5))
                            .text_color(colors.tertiary)
                            .child(SharedString::from(command.description.clone())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.slash_index = index;
                        this.complete_slash(true, cx);
                    })),
            );
        }
        Some(menu.into_any_element())
    }

    /// The caret blinks at macOS's period while the composer has focus and
    /// stops (no timer at all) once a frame drawn without focus says so.
    fn ensure_blink(&mut self, focused: bool, cx: &mut Context<Self>) {
        self.focused = focused;
        if !focused || self.blink.is_some() || cx.reduce_motion() {
            if !focused {
                self.caret_on = true;
            }
            return;
        }
        self.blink = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let keep = this.update(cx, |this, cx| {
                    if !this.focused {
                        this.blink = None;
                        this.caret_on = true;
                        return false;
                    }
                    if std::mem::take(&mut this.typed) {
                        this.caret_on = true;
                    } else {
                        this.caret_on = !this.caret_on;
                        cx.notify();
                    }
                    true
                });
                if !matches!(keep, Ok(true)) {
                    return;
                }
            }
        }));
    }
}

impl Render for ThreadLens {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf_overlay::rendered("thread-lens");
        let (colors, _) = self.colors();
        let session = self.session();
        // The working line follows the session's status.
        let extra = self.extra_rows();
        if extra != self.shown_extra {
            self.apply_change(RowChange::None);
        }
        // A short thread starts under the header (top-aligned). Once it
        // overflows, a bottom-aligned list looks the same but follows the
        // tail at about half the layout cost on long threads, so switch.
        if self.top_anchored
            && self.list.is_following_tail()
            && self.list.max_offset_for_scrollbar().y > px(0.0)
        {
            self.top_anchored = false;
            self.list = new_list(ListAlignment::Bottom, self.list.item_count(), cx.entity().downgrade());
        }
        // Rows re-register their text as the list lays them out.
        self.segments.clear();
        let empty = self.model.rows.is_empty() && self.shown_extra == 0;
        let thread = list(
            self.list.clone(),
            cx.processor(|this, index, window, cx| this.render_row(index, window, cx)),
        )
        .size_full();
        // One docked slot above the composer: the live PTY portal while a
        // picker is open, else a permission card.
        let docked = self.dock.take().or_else(|| {
            session
                .as_deref()
                .and_then(|session| self.render_permission(session, colors, cx))
        });
        let composer = self.render_composer(session.as_deref(), colors, window, cx);
        let behind = !self.list.is_following_tail() && !empty;
        let jump = behind.then(|| {
            div()
                .absolute()
                .bottom(px(12.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("lens-jump-latest")
                        .size(px(30.0))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(colors.floating_fill())
                        .border_1()
                        .border_color(colors.floating_stroke())
                        .shadow_md()
                        .cursor_pointer()
                        .hover(|button| button.bg(colors.primary.alpha(0.08)))
                        .child(crate::icons::sf_symbol("arrow.down", 12.0, colors.secondary))
                        .shortcut_tooltip(t("thread_lens.jump_latest"), None, colors)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.list.set_follow_mode(gpui::FollowMode::Tail);
                            cx.notify();
                        }))
                        .with_animation(
                            "lens-jump-in",
                            Animation::new(Duration::from_millis(160)).with_easing(gpui::ease_out_quint()),
                            |button, t| button.opacity(t).mt(px(6.0 * (1.0 - t))),
                        ),
                )
        });
        div()
            .id("thread-lens")
            .key_context("DiriThreadLens")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            // Exactly what the grid paints here, so a flip never shifts shade.
            .when_some(lens_background(colors, &self.colors().1), |lens, fill| lens.bg(fill))
            .text_color(colors.primary)
            .font_family(crate::fonts::ui_family())
            .child(
                div()
                    .id("lens-thread")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .pt(px(12.0))
                    .pb(px(16.0))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::select_down))
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| this.select_drag(event, cx)))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.selecting = false))
                    .child(thread)
                    .when(empty && self.model.structured == Some(false), |area| {
                        area.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(13.0))
                                .text_color(colors.tertiary)
                                .child(SharedString::from(
                                    self.model
                                        .reason
                                        .clone()
                                        .unwrap_or_else(|| t("thread_lens.unstructured").into()),
                                )),
                        )
                    })
                    .children(jump),
            )
            .children(docked)
            .child(composer)
    }
}

// ── Pieces ───────────────────────────────────────────────────────────────

/// Bold, italic, struck, `code`, links and `path:line` references as runs
/// of one text, so they wrap together like any word: a period after a bold
/// phrase or a code span stays on its line.
fn inline_runs(content: &InlineText, weight: FontWeight, color: Hsla, colors: SemanticColors) -> (String, Vec<TextRun>, Targets) {
    let base = Font { weight, ..ui_font() };
    let mono = mono_font();
    let mut text = String::new();
    let mut runs = Vec::new();
    let mut targets = Vec::new();
    for span in &content.spans {
        if span.text.is_empty() {
            continue;
        }
        let style = span.style;
        let start = text.len();
        let path = style.code && looks_like_path(&span.text);
        let shown = if path { short_path(&span.text) } else { span.text.clone() };
        text.push_str(&shown);
        let mut font = if style.code { mono.clone() } else { base.clone() };
        if style.bold {
            font.weight = FontWeight::SEMIBOLD;
        }
        if style.italic {
            font.style = FontStyle::Italic;
        }
        let mut piece = run(shown.len(), &font, color);
        if style.code {
            piece.background_color = Some(colors.primary.alpha(if path { 0.09 } else { 0.07 }).into());
            piece.color = if path { colors.primary.into() } else { colors.primary.alpha(0.88).into() };
        }
        if style.strikethrough {
            piece.strikethrough = Some(StrikethroughStyle { thickness: px(1.0), color: Some(color) });
        }
        if let Some(link) = &span.link {
            piece.color = rgba(LINK << 8 | 0xff).into();
            piece.underline = Some(UnderlineStyle { thickness: px(1.0), color: Some(rgba(LINK << 8 | 0x66).into()), wavy: false });
            targets.push((start..text.len(), Target::Url(link.clone())));
        } else if path {
            piece.underline = Some(UnderlineStyle { thickness: px(1.0), color: Some(colors.primary.alpha(0.22).into()), wavy: false });
            targets.push((start..text.len(), Target::Path(span.text.clone())));
        }
        runs.push(piece);
    }
    (text, runs, targets)
}

/// The thread's list, following the tail; scrolling away from it shows the
/// jump-to-latest control.
fn new_list(alignment: ListAlignment, count: usize, lens: WeakEntity<ThreadLens>) -> ListState {
    let list = ListState::new(count, alignment, px(600.0));
    list.set_follow_mode(gpui::FollowMode::Tail);
    list.set_scroll_handler(move |_, _, cx| {
        let _ = lens.update(cx, |_, cx| cx.notify());
    });
    list
}

fn ui_font() -> Font {
    gpui::font(crate::fonts::ui_family())
}

fn mono_font() -> Font {
    gpui::font(crate::fonts::mono_family())
}

fn run(len: usize, font: &Font, color: Hsla) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// A crisp caret in the text's own color, not the accent.
fn caret_color(on: bool, colors: SemanticColors) -> Hsla {
    if on {
        colors.primary.alpha(0.9).into()
    } else {
        gpui::transparent_black()
    }
}

/// One model in the picker: its name, the provider's note, its cache
/// warmth (Claude), and a check on the current one.
fn model_row(
    id: &str,
    choice: &super::setup::ModelChoice,
    selected: bool,
    warmth: Option<CacheWarmth>,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    let warm = matches!(warmth, Some(CacheWarmth::Warm { .. }));
    div()
        .id(SharedString::from(format!("lens-model-{id}")))
        // Rows keep their height inside the scrolling, height-capped list.
        .flex_none()
        .min_h(px(32.0))
        .px(px(10.0))
        .py(px(if choice.detail.is_empty() { 5.0 } else { 8.0 }))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(8.0))
        .when(selected, |row| row.bg(colors.primary.alpha(0.06)))
        .hover(|row| row.bg(colors.primary.alpha(0.08)))
        .cursor_pointer()
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().text_size(px(13.0)).text_color(colors.primary).child(SharedString::from(choice.label.clone())))
                .when(!choice.detail.is_empty(), |col| {
                    col.child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(colors.tertiary)
                            .child(SharedString::from(choice.detail.clone())),
                    )
                }),
        )
        .children(warmth.map(|warmth| {
            div()
                .flex_none()
                .px(px(6.0))
                .h(px(18.0))
                .flex()
                .items_center()
                .rounded(px(9.0))
                .when(warm, |badge| badge.bg(Ink::FRESH.alpha(0.12)))
                .text_size(px(10.5))
                .text_color(if warm { Ink::FRESH } else { colors.tertiary })
                .child(SharedString::from(warmth_text(warmth)))
        }))
        .child(
            div()
                .w(px(14.0))
                .flex_none()
                .when(selected, |slot| slot.child(crate::icons::sf_symbol("checkmark", 11.0, colors.primary))),
        )
}

/// The fill the terminal grid paints over the pane's surface: the theme's
/// background on an opaque window, nothing under glass (the pane's own
/// tint shows through). The lens paints exactly this, so Conversation and
/// Terminal share one background to the byte.
pub fn lens_background(colors: SemanticColors, theme: &diri_term::theme::TermTheme) -> Option<Rgba> {
    (grid_fill_opacity(colors.material()) > 0.0).then_some(theme.background)
}

/// The grid's own fill opacity, which the terminal pane passes to it.
pub fn grid_fill_opacity(material: diri_ui::Material) -> f32 {
    match material {
        diri_ui::Material::Opaque => 1.0,
        diri_ui::Material::Glass => 0.0,
    }
}

/// Popovers sit on the composer's own surface: white in light mode, the
/// floating surface in dark.
fn menu_surface(colors: SemanticColors) -> Rgba {
    if colors.lightness() < 0.5 {
        colors.floating_surface()
    } else {
        gpui::white().into()
    }
}

fn effort_label(effort: &str) -> &'static str {
    match effort {
        "minimal" => t("thread_lens.effort.minimal"),
        "low" => t("thread_lens.effort.low"),
        "medium" => t("thread_lens.effort.medium"),
        "high" => t("thread_lens.effort.high"),
        "xhigh" => t("thread_lens.effort.xhigh"),
        "max" => t("thread_lens.effort.max"),
        "ultra" => t("thread_lens.effort.ultra"),
        _ => t("thread_lens.effort.custom"),
    }
}

fn mode_label(mode: &str) -> &'static str {
    match mode {
        "plan" => t("thread_lens.mode.plan"),
        "acceptEdits" => t("thread_lens.mode.accept_edits"),
        "auto" => t("thread_lens.mode.auto"),
        "bypassPermissions" => t("thread_lens.mode.bypass"),
        _ => t("thread_lens.mode.ask"),
    }
}

fn mode_tint(mode: &str, colors: SemanticColors) -> Rgba {
    match mode {
        "plan" => rgba(0x5b9cf0ff),
        "acceptEdits" | "auto" => rgba(ACCENT << 8 | 0xff),
        "bypassPermissions" => Ink::DANGER,
        _ => colors.tertiary.alpha(0.7),
    }
}

/// Claude's aliases as the provider ids its usage records carry.
fn claude_model_id(alias: &str) -> String {
    match alias {
        "opus" => "claude-opus-5-5".into(),
        "sonnet" => "claude-sonnet-5-5".into(),
        "haiku" => "claude-haiku-5-5".into(),
        other => other.into(),
    }
}

/// A 16 pt ring filling clockwise with the context window, and its
/// percentage. Past 80% it turns amber, a cue to compact.
fn context_ring(used: u64, window: u64, colors: SemanticColors) -> AnyElement {
    let fraction = (used as f32 / window.max(1) as f32).clamp(0.0, 1.0);
    let tint: Hsla = if fraction > 0.8 { Ink::ATTENTION.into() } else { colors.secondary.alpha(0.85).into() };
    let track: Hsla = colors.primary.alpha(0.12).into();
    let percent = (fraction * 100.0).round() as u32;
    let label = tf(
        "thread_lens.context",
        &[("percent", &percent), ("used", &compact_tokens(used)), ("window", &compact_tokens(window))],
    );
    div()
        .id("lens-context")
        .h(px(26.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .rounded(px(8.0))
        .hover(|ring| ring.bg(colors.primary.alpha(0.06)))
        .child(
            gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let radius = px(6.0);
                    let center = bounds.center();
                    let point = |turn: f32| {
                        let angle = turn * std::f32::consts::TAU;
                        gpui::point(center.x + radius * angle.sin(), center.y - radius * angle.cos())
                    };
                    let mut circle = gpui::PathBuilder::stroke(px(2.0));
                    circle.move_to(point(0.0));
                    circle.arc_to(gpui::point(radius, radius), px(0.0), false, true, point(0.5));
                    circle.arc_to(gpui::point(radius, radius), px(0.0), false, true, point(0.0));
                    if let Ok(path) = circle.build() {
                        window.paint_path(path, track);
                    }
                    if fraction > 0.005 {
                        let mut arc = gpui::PathBuilder::stroke(px(2.0));
                        arc.move_to(point(0.0));
                        let end = fraction.min(0.999);
                        arc.arc_to(gpui::point(radius, radius), px(0.0), end > 0.5, true, point(end));
                        if let Ok(path) = arc.build() {
                            window.paint_path(path, tint);
                        }
                    }
                },
            )
            .size(px(16.0)),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.tertiary)
                .child(SharedString::from(format!("{percent}%"))),
        )
        .shortcut_tooltip(label, None, colors)
        .into_any_element()
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1e6)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn selection_color(colors: SemanticColors) -> Hsla {
    if colors.lightness() < 0.5 {
        rgba(0x3d7ef066).into()
    } else {
        rgba(0x3d7ef040).into()
    }
}

/// Splits `runs` at `range`'s ends and edits the runs inside it.
fn split_runs(runs: Vec<TextRun>, range: Range<usize>, edit: impl Fn(&mut TextRun)) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for original in runs {
        let end = at + original.len;
        let cuts = [at, range.start.clamp(at, end), range.end.clamp(at, end), end];
        for pair in cuts.windows(2) {
            if pair[1] > pair[0] {
                let mut piece = original.clone();
                piece.len = pair[1] - pair[0];
                if pair[0] >= range.start && pair[1] <= range.end {
                    edit(&mut piece);
                }
                out.push(piece);
            }
        }
        at = end;
    }
    out
}

fn select_runs(runs: Vec<TextRun>, range: Range<usize>, background: Hsla) -> Vec<TextRun> {
    split_runs(runs, range, |run| run.background_color = Some(background))
}

/// The word (or run of punctuation, or space) around byte `index`.
fn word_at(text: &str, index: usize) -> Range<usize> {
    for (start, word) in text.split_word_bound_indices() {
        let end = start + word.len();
        if index < end || end == text.len() {
            return start..end;
        }
    }
    0..text.len()
}

fn bubble(colors: SemanticColors) -> gpui::Div {
    div()
        .max_w(px(MEASURE * 0.82))
        .px(px(15.0))
        .py(px(9.0))
        .rounded(px(18.0))
        .bg(colors.primary.alpha(0.06))
        .flex()
        .flex_col()
        .gap(px(8.0))
}

/// The thread's tail while the Agent works and no strip says what on.
fn working_line(text: &str, colors: SemanticColors) -> AnyElement {
    div()
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .child(
            div()
                .size(px(6.0))
                .rounded_full()
                .bg(rgba(ACCENT << 8 | 0xff))
                .with_animation(
                    "lens-working-dot",
                    Animation::new(Duration::from_millis(1400)).repeat(),
                    |dot, t| dot.opacity(0.35 + 0.65 * (0.5 - 0.5 * (t * std::f32::consts::TAU).cos())),
                ),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(colors.tertiary)
                .child(SharedString::from(text.to_owned())),
        )
        .into_any_element()
}

fn quiet_button(id: SharedString, label: &'static str, colors: SemanticColors) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(20.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .text_size(px(10.5))
        .text_color(colors.tertiary)
        .cursor_pointer()
        .hover(|b| b.bg(colors.primary.alpha(0.07)).text_color(colors.secondary))
        .child(label)
}

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
            match super::model::tool_subject(input) {
                Some(subject) => format!("{}  {subject}", super::model::tool_label(&call.name)),
                None => super::model::tool_label(&call.name),
            },
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
        // A handoff segment names the session the thread came from.
        SegmentCause::Handoff => tf(
            "thread_lens.segment_continued",
            &[("agent", &super::setup::agent_label(&segment.kind))],
        ),
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


#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Entity;

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

    fn runs_of(lens: &[usize]) -> Vec<TextRun> {
        lens.iter().map(|len| run(*len, &ui_font(), gpui::black())).collect()
    }

    #[test]
    fn a_selection_splits_runs_without_moving_text() {
        let runs = select_runs(runs_of(&[5, 4, 6]), 3..11, gpui::red());
        let lens: Vec<usize> = runs.iter().map(|r| r.len).collect();
        assert_eq!(lens, vec![3, 2, 4, 2, 4]);
        assert_eq!(lens.iter().sum::<usize>(), 15);
        let marked: Vec<bool> = runs.iter().map(|r| r.background_color.is_some()).collect();
        assert_eq!(marked, vec![false, true, true, true, false]);
    }

    #[test]
    fn double_click_takes_the_word_under_the_pointer() {
        let text = "open **Set up**. It takes";
        assert_eq!(&text[word_at(text, 8)], "Set");
        assert_eq!(&text[word_at(text, 0)], "open");
    }

    fn colors() -> SemanticColors {
        diri_ui::SemanticColors::dark()
    }

    /// Screenshot 5: "click **Set up**" then a period wrapped onto its own
    /// line, because every span was its own flex item. Now a paragraph is a
    /// single text: the period follows "up" with no break opportunity.
    #[test]
    fn a_period_after_bold_stays_in_the_same_text() {
        let document = MarkdownDocument::parse(
            "To turn it on, open **Diri Settings › Agents › Computer use** and click **Set up**. It takes a few seconds, see `src/app.rs:42`.",
        );
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("one paragraph");
        };
        let (text, runs, targets) = inline_runs(content, FontWeight::NORMAL, gpui::white(), colors());
        assert!(text.contains("click Set up. It takes"), "{text}");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
        let bold: Vec<&str> = {
            let mut at = 0;
            runs.iter()
                .filter_map(|r| {
                    let piece = &text[at..at + r.len];
                    at += r.len;
                    (r.font.weight == FontWeight::SEMIBOLD).then_some(piece)
                })
                .collect()
        };
        assert_eq!(bold, vec!["Diri Settings › Agents › Computer use", "Set up"]);
        assert!(matches!(&targets[..], [(_, Target::Path(path))] if path == "src/app.rs:42"));
    }

    fn lens_window(cx: &mut gpui::TestAppContext) -> (Entity<ThreadLens>, &mut gpui::VisualTestContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let tokio = Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap());
        let handle = tokio.handle().clone();
        std::mem::forget(tokio);
        let (lens, cx) = cx.add_window_view(move |_, cx| {
            ThreadLens::new(SessionId::new("lens-test"), runtime, handle, cx)
        });
        (lens, cx)
    }

    fn user_page(offset: u64, text: &str) -> diri_proto::thread::ThreadReadResult {
        use diri_proto::thread::{ThreadBody, ThreadCursor, ThreadItem, THREAD_WIRE_VERSION};
        diri_proto::thread::ThreadReadResult {
            version: THREAD_WIRE_VERSION,
            structured: true,
            next: ThreadCursor { generation: "g".into(), offset: offset + 10 },
            items: vec![
                ThreadItem { key: ItemKey { offset, part: 0 }, at_ms: Some(now_ms() + 60_000), body: ThreadBody::User { text: text.into(), images: vec![] } },
                ThreadItem {
                    key: ItemKey { offset: offset + 1, part: 0 },
                    at_ms: None,
                    body: ThreadBody::Assistant { text: "Open **Settings**, then click **Set up**. Done.".into() },
                },
            ],
            ..Default::default()
        }
    }

    /// Enter shows the message in the same frame, with the working line;
    /// the transcript record replaces it without a duplicate.
    #[gpui::test]
    fn a_sent_message_shows_at_once_and_reconciles_with_its_record(cx: &mut gpui::TestAppContext) {
        let (lens, cx) = lens_window(cx);
        lens.update(cx, |lens, cx| lens.send_for_test("cool it's fine. /model", cx));
        lens.read_with(cx, |lens, _| {
            assert_eq!(lens.model.pending.len(), 1);
            assert_eq!(lens.rows_for_test(), 2, "pending bubble + working line, at 0 ms");
        });
        lens.update(cx, |lens, cx| lens.ingest_for_test(user_page(100, "cool it's fine. /model"), cx));
        cx.run_until_parked();
        lens.read_with(cx, |lens, _| {
            assert!(lens.model.pending.is_empty());
            assert_eq!(lens.rows_for_test(), lens.model.rows.len(), "{:?}", lens.model.rows.len());
            assert_eq!(lens.model.rows.len(), 2, "one user row, one reply, no duplicate");
        });
    }

    /// Text drawn in the thread is selectable end to end and copies as the
    /// reader sees it.
    #[gpui::test]
    fn selecting_the_thread_copies_its_text(cx: &mut gpui::TestAppContext) {
        let (lens, cx) = lens_window(cx);
        lens.update(cx, |lens, cx| lens.ingest_for_test(user_page(0, "hi there"), cx));
        cx.run_until_parked();
        let copied = lens.update(cx, |lens, _| lens.select_all_text_for_test());
        assert_eq!(copied.as_deref(), Some("hi there\n\nOpen Settings, then click Set up. Done."));
    }

    /// A short thread sits under the header; once it overflows, the lens
    /// switches to the cheaper bottom-aligned list, still on the tail.
    #[gpui::test]
    fn a_short_thread_is_top_anchored_until_it_overflows(cx: &mut gpui::TestAppContext) {
        let (lens, cx) = lens_window(cx);
        lens.update(cx, |lens, cx| lens.ingest_for_test(user_page(0, "hi"), cx));
        cx.run_until_parked();
        assert!(lens.read_with(cx, |lens, _| lens.top_anchored));
        for n in 1..120u64 {
            lens.update(cx, |lens, cx| lens.ingest_for_test(user_page(n * 10, "and another line"), cx));
        }
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        lens.read_with(cx, |lens, _| {
            assert!(!lens.top_anchored, "overflowing threads use the bottom-aligned list");
            assert!(lens.list.is_following_tail());
        });
    }

    fn claude_setup() -> AgentSetup {
        AgentSetup {
            model: Some("opus".into()),
            effort: Some("high".into()),
            ..super::super::setup::read(&AgentKind::CLAUDE_CODE, std::path::Path::new("/nonexistent"), std::path::Path::new("/nonexistent"))
        }
    }

    fn output_page(offset: u64, text: &str) -> diri_proto::thread::ThreadReadResult {
        use diri_proto::thread::{ThreadBody, ThreadCursor, ThreadItem, THREAD_WIRE_VERSION};
        diri_proto::thread::ThreadReadResult {
            version: THREAD_WIRE_VERSION,
            structured: true,
            next: ThreadCursor { generation: "g".into(), offset: offset + 1 },
            items: vec![ThreadItem {
                key: ItemKey { offset, part: 0 },
                at_ms: None,
                body: ThreadBody::CommandOutput { text: text.into(), is_error: false },
            }],
            ..Default::default()
        }
    }

    /// The user's report: Haiku 5.5 was picked and Claude switched, but the
    /// pill and the check stayed on the startup default, Opus 5.5.
    #[gpui::test]
    fn a_picked_model_shows_at_once_and_the_cli_confirmation_keeps_it(cx: &mut gpui::TestAppContext) {
        let (lens, cx) = lens_window(cx);
        let setup = claude_setup();
        lens.update(cx, |lens, _| {
            assert_eq!(lens.model_label(&setup), "Opus 5.5", "the settings' default before anything happens");
            lens.pick_for_test("Haiku 5.5");
            assert_eq!(lens.model_label(&setup), "Haiku 5.5", "shown the moment it is picked");
        });
        lens.update(cx, |lens, cx| {
            lens.ingest_for_test(output_page(0, "Set model to `Haiku 5.5` and saved as your default for new sessions"), cx);
            lens.ingest_for_test(output_page(1, "Set effort level to medium (saved as your default for new sessions)"), cx);
        });
        lens.update(cx, |lens, _| {
            assert_eq!(lens.model_label(&setup), "Haiku 5.5", "confirmed, and the default never wins back");
            assert!(lens.picked.is_none(), "the pick is reconciled");
            assert_eq!(lens.effort_level(&setup).as_deref(), Some("medium"));
        });
    }

    /// Screenshot 2: Claude was done in 1 s, the lens still said Thinking.
    /// The line goes the moment the reply row lands, even while the session
    /// status still says working.
    #[gpui::test]
    fn the_working_line_clears_when_the_reply_lands_even_if_status_lags(cx: &mut gpui::TestAppContext) {
        let (lens, cx) = lens_window(cx);
        lens.update(cx, |lens, cx| lens.send_for_test("im testing hello there", cx));
        lens.read_with(cx, |lens, _| assert_eq!(lens.rows_for_test(), 2, "pending + Thinking"));
        lens.update(cx, |lens, cx| lens.ingest_for_test(user_page(0, "im testing hello there"), cx));
        cx.run_until_parked();
        lens.read_with(cx, |lens, _| {
            assert!(!lens.model.awaiting_reply());
            assert_eq!(lens.rows_for_test(), lens.model.rows.len(), "no working line after the reply");
        });
    }

    /// Conversation and Terminal share one background to the byte: on an
    /// opaque window both paint the theme's background, under glass neither
    /// paints over the pane's tint.
    #[test]
    fn the_lens_paints_exactly_the_terminal_grid_fill() {
        for theme in diri_term::themes::all() {
            let opaque = diri_ui::SemanticColors::dark();
            let grid = theme.background.alpha(grid_fill_opacity(opaque.material()));
            assert_eq!(lens_background(opaque, &theme), Some(grid), "{}", theme.name);
            let glass = opaque.with_material(diri_ui::Material::Glass);
            assert_eq!(grid_fill_opacity(glass.material()), 0.0);
            assert_eq!(lens_background(glass, &theme), None);
        }
    }
}
