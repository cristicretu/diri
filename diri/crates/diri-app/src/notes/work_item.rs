//! To-dos as tracked agent work, in the editor.
//!
//! The rules live in `diri_notes::work` (what counts as context, how state is
//! derived, what the agent receives); this module is the pixels and the
//! little view state they need: which to-dos are folded, which starts are in
//! flight, and the two panels (Start, and "still working" on tick). The pane
//! feeds it live session facts every frame and turns its requests into
//! spawns, selections and archives.

use std::collections::HashMap;

use diri_notes::doc::{Block, BlockId, BlockKind};
use diri_notes::edit::Pos;
use diri_notes::work::{self, Brief, SessionFacts, WorkState};
use diri_proto::{AgentKind, SessionId};
use diri_ui::{AgentLogo, Ink, SemanticColors, Typo};
use gpui::{
    AnyElement, Context, FontWeight, MouseButton, MouseDownEvent, ScrollHandle, SharedString, div,
    prelude::*, px,
};

use super::editor_view::{EditorEvent, NoteEditorView, accent};
use crate::floating;
use crate::icons::sf_symbol;

/// The keyboard shortcut that starts work from the to-do at the caret.
pub(crate) const START_SHORTCUT: &str = "⌃⌘↩";
const PANEL_WIDTH: f32 = 440.0;
const TICK_WIDTH: f32 = 280.0;
const PREVIEW_HEIGHT: f32 = 200.0;
const PREVIEW_LINE: f32 = 16.0;
/// Roughly how many mono characters fit a preview line, for the height the
/// panel reserves before it is measured.
const PREVIEW_CHARS_PER_LINE: usize = 62;
const LABEL_HEIGHT: f32 = 24.0;
/// Start's height; it sits centred on the line holding the to-do's last word.
const ACCESSORY_HEIGHT: f32 = 24.0;

/// What the editor asks its host to do for a work item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkRequest {
    /// Open the Start panel for this to-do: the host assembles the brief and
    /// the installed agents and calls [`NoteEditorView::open_start_panel`].
    Prepare { block: BlockId },
    /// Start `kind` on this to-do with the brief the panel showed.
    Start {
        block: BlockId,
        kind: AgentKind,
        agent_name: String,
        /// The existing session the to-do mentions, asked instead of a new
        /// agent being spawned.
        session: Option<String>,
    },
    /// Show this session (the sidebar selects it).
    Open { session: String },
    /// Stop this session's agent (the to-do was ticked while it ran).
    Stop { session: String },
}

/// One installed agent the Start panel offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentChoice {
    pub kind: AgentKind,
    pub name: String,
    pub is_default: bool,
    /// Set for the session the to-do mentions: Start prompts it.
    pub session: Option<String>,
}

pub(crate) struct StartPanel {
    block: BlockId,
    agents: Vec<AgentChoice>,
    selected: usize,
    brief: Brief,
    scroll: ScrollHandle,
}

/// Preserve how the to-do was targeted even if the confirmation uses a different input.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickOrigin {
    Checkbox,
    Keyboard,
}

struct TickPanel {
    origin: TickOrigin,
    block: BlockId,
    session: String,
    selected: usize,
}

const TICK_CHOICES: [&str; 3] = [
    "notes.work.tick_stop",
    "notes.work.tick_keep",
    "notes.cancel",
];

enum Pending {
    Starting { ticket: u64, label: String },
    Failed(String),
}

/// Work-item view state for one open note. Nothing here is saved: the file
/// holds the facts, the Engine holds the live state.
#[derive(Default)]
pub(crate) struct WorkView {
    facts: HashMap<String, SessionFacts>,
    /// To-dos that mention a session nobody has asked to take them on yet:
    /// Start prompts that session instead of spawning one.
    handoff: HashMap<BlockId, String>,
    pending: HashMap<BlockId, Pending>,
    start: Option<StartPanel>,
    tick: Option<TickPanel>,
}

impl WorkView {
    /// Live facts for the sessions the note links, keyed by session id. A
    /// linked session missing here is unknown to the Engine.
    pub(crate) fn set_facts(&mut self, facts: HashMap<String, SessionFacts>) {
        self.facts = facts;
    }

    /// The to-dos whose mentioned session Start would prompt.
    pub(crate) fn set_handoff(&mut self, handoff: HashMap<BlockId, String>) -> bool {
        let changed = self.handoff != handoff;
        self.handoff = handoff;
        changed
    }

    pub(crate) fn handoff(&self, block: BlockId) -> Option<&str> {
        self.handoff.get(&block).map(String::as_str)
    }

    /// Tickets of starts still waiting on the Engine.
    pub(crate) fn pending_tickets(&self) -> Vec<(BlockId, u64)> {
        self.pending
            .iter()
            .filter_map(|(block, pending)| match pending {
                Pending::Starting { ticket, .. } => Some((*block, *ticket)),
                Pending::Failed(_) => None,
            })
            .collect()
    }

    pub(crate) fn state(&self, block: &Block) -> WorkState {
        if matches!(self.pending.get(&block.id), Some(Pending::Starting { .. })) {
            return WorkState::Starting;
        }
        let checked = block.kind == BlockKind::Todo { checked: true };
        // A mention is where the work should go, not work under way.
        if !checked && self.handoff.contains_key(&block.id) {
            return WorkState::Ready;
        }
        let session = work::current_session(block);
        work::state(
            checked,
            session.as_ref().map(|id| self.facts.get(id.as_str())),
        )
    }

    fn failure(&self, block: BlockId) -> Option<&str> {
        match self.pending.get(&block) {
            Some(Pending::Failed(reason)) => Some(reason),
            _ => None,
        }
    }

    /// Carries start state across a reload, which renumbers
    /// blocks: each to-do is found again by its text, nearest first.
    pub(crate) fn remap(&mut self, old: &[Block], new: &[Block]) {
        let find = |id: BlockId| -> Option<BlockId> {
            let from = old.iter().position(|b| b.id == id)?;
            let text = work::task_text(&old[from]);
            new.iter()
                .enumerate()
                .filter(|(_, b)| {
                    matches!(b.kind, BlockKind::Todo { .. }) && work::task_text(b) == text
                })
                .min_by_key(|(index, _)| index.abs_diff(from))
                .map(|(_, b)| b.id)
        };
        self.pending = std::mem::take(&mut self.pending)
            .into_iter()
            .filter_map(|(id, pending)| Some((find(id)?, pending)))
            .collect();
        if let Some(panel) = &mut self.start {
            match find(panel.block) {
                Some(id) => panel.block = id,
                None => self.start = None,
            }
        }
        if let Some(panel) = &mut self.tick {
            match find(panel.block) {
                Some(id) => panel.block = id,
                None => self.tick = None,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn start_panel_agents(&self) -> Vec<AgentChoice> {
        self.start
            .as_ref()
            .map(|panel| panel.agents.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn start_panel_brief(&self) -> Option<&Brief> {
        self.start.as_ref().map(|panel| &panel.brief)
    }

    #[cfg(test)]
    pub(crate) fn tick_panel_open(&self) -> bool {
        self.tick.is_some()
    }
}

/// Keys the open panel consumes before the editor sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelKey {
    Up,
    Down,
    Enter,
    Escape,
}

impl NoteEditorView {
    fn block_index(&self, id: BlockId) -> Option<usize> {
        self.editor.blocks().iter().position(|b| b.id == id)
    }

    /// ⌥⌘↩: start work from the to-do at the caret.
    pub(super) fn start_work_at_caret(&mut self, cx: &mut Context<Self>) {
        let head = self.editor.selection.head.block;
        let block = self.editor.block(head);
        if matches!(block.kind, BlockKind::Todo { checked: false })
            && self.work.state(block).can_restart()
        {
            cx.emit(EditorEvent::Work(WorkRequest::Prepare { block: block.id }));
        }
    }

    /// Shows the Start panel under a to-do with what the agent will get.
    pub(crate) fn open_start_panel(
        &mut self,
        block: BlockId,
        agents: Vec<AgentChoice>,
        brief: Brief,
        cx: &mut Context<Self>,
    ) {
        let selected = agents.iter().position(|a| a.is_default).unwrap_or(0);
        self.work.tick = None;
        self.work.start = Some(StartPanel {
            block,
            agents,
            selected,
            brief,
            scroll: ScrollHandle::new(),
        });
        cx.notify();
    }

    fn confirm_start(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(panel) = self.work.start.take() else {
            return;
        };
        let Some(agent) = panel.agents.get(index) else {
            cx.notify();
            return;
        };
        self.work.pending.remove(&panel.block);
        cx.emit(EditorEvent::Work(WorkRequest::Start {
            block: panel.block,
            kind: agent.kind.clone(),
            agent_name: agent.name.clone(),
            session: agent.session.clone(),
        }));
        cx.notify();
    }

    /// The host asked the Engine to spawn; the to-do shows "Starting…" and
    /// links the session as `label` once it exists.
    pub(crate) fn work_started(
        &mut self,
        block: BlockId,
        ticket: u64,
        label: String,
        cx: &mut Context<Self>,
    ) {
        self.work
            .pending
            .insert(block, Pending::Starting { ticket, label });
        // Once work starts its context folds away under the to-do.
        if let Some(index) = self.block_index(block)
            && self.editor.has_children(index)
        {
            self.set_folded(index, true, cx);
        }
        cx.notify();
    }

    /// The Engine answered a start: link the new session into the to-do
    /// (one undo step, saved like typing), or say why it did not start.
    pub(crate) fn work_finished(
        &mut self,
        block: BlockId,
        outcome: Result<SessionId, String>,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(session) => {
                let label = match self.work.pending.remove(&block) {
                    Some(Pending::Starting { label, .. }) => label,
                    _ => diri_notes::mention::session_label("", ""),
                };
                if let Some(index) = self.block_index(block)
                    && self.editor.link_session(index, &label, &session.0, now_ms)
                {
                    cx.emit(EditorEvent::Changed);
                }
            }
            Err(reason) => {
                self.work.pending.insert(block, Pending::Failed(reason));
            }
        }
        cx.notify();
    }

    /// Ticking a to-do whose agent is still running asks first. Returns
    /// whether the tick was intercepted.
    pub(crate) fn guard_tick(
        &mut self,
        index: usize,
        origin: TickOrigin,
        cx: &mut Context<Self>,
    ) -> bool {
        let block = self.editor.block(index);
        if block.kind != (BlockKind::Todo { checked: false })
            || !self.work.state(block).is_running()
        {
            return false;
        }
        let Some(session) = work::current_session(block) else {
            return false;
        };
        self.work.start = None;
        // Return keeps the agent running; stopping it is a deliberate pick.
        self.work.tick = Some(TickPanel {
            origin,
            block: block.id,
            session,
            selected: 1,
        });
        cx.notify();
        true
    }

    fn confirm_tick(&mut self, choice: usize, cx: &mut Context<Self>) {
        let Some(panel) = self.work.tick.take() else {
            return;
        };
        if choice < 2
            && let Some(index) = self.block_index(panel.block)
        {
            if choice == 0 {
                cx.emit(EditorEvent::Work(WorkRequest::Stop {
                    session: panel.session,
                }));
            }
            self.set_todo_checked(index, true, panel.origin, cx);
        }
        cx.notify();
    }

    /// Arrow keys, Return and Escape drive an open panel.
    pub(super) fn work_panel_key(&mut self, key: PanelKey, cx: &mut Context<Self>) -> bool {
        if let Some(panel) = &mut self.work.start {
            let count = panel.agents.len().max(1);
            match key {
                PanelKey::Up => panel.selected = (panel.selected + count - 1) % count,
                PanelKey::Down => panel.selected = (panel.selected + 1) % count,
                PanelKey::Enter => {
                    let selected = panel.selected;
                    self.confirm_start(selected, cx);
                    return true;
                }
                PanelKey::Escape => self.work.start = None,
            }
            cx.notify();
            return true;
        }
        if let Some(panel) = &mut self.work.tick {
            let count = TICK_CHOICES.len();
            match key {
                PanelKey::Up => panel.selected = (panel.selected + count - 1) % count,
                PanelKey::Down => panel.selected = (panel.selected + 1) % count,
                PanelKey::Enter => {
                    let selected = panel.selected;
                    self.confirm_tick(selected, cx);
                    return true;
                }
                PanelKey::Escape => self.work.tick = None,
            }
            cx.notify();
            return true;
        }
        false
    }

    /// A note opens with the context of started work folded away, the way
    /// it folded when the work started.
    pub(crate) fn fold_started_work(&mut self) {
        for index in 0..self.editor.blocks().len() {
            let block = self.editor.block(index);
            if block.kind == (BlockKind::Todo { checked: false })
                && !work::sessions(block).is_empty()
                && self.editor.has_children(index)
            {
                self.editor.set_collapsed(index, true);
            }
        }
    }

    /// Puts the caret on the to-do that links `session`, unfolded, and
    /// scrolls to it: the way back from a session to its work item.
    pub(crate) fn reveal_session(&mut self, session: &str, cx: &mut Context<Self>) -> bool {
        let Some(index) = work::todo_for_session(self.editor.blocks(), session) else {
            return false;
        };
        let end = self.editor.block(index).text.len();
        self.editor.set_caret(Pos::new(index, end));
        self.moved(cx);
        true
    }

    // -----------------------------------------------------------------------
    // Pixels

    /// Whether an unstarted to-do offers Start, so its text keeps room for it.
    pub(super) fn wants_start(&self, block: &Block) -> bool {
        block.kind == (BlockKind::Todo { checked: false })
            && !block.text.trim().is_empty()
            && self.work.state(block) == WorkState::Ready
            && self.work.failure(block.id).is_none()
    }

    /// The quiet Start affordance right after an unstarted to-do's last word,
    /// so a narrow window wraps it with the words instead of cutting it off.
    /// It shows while the row is hovered or holds the caret.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn work_accessory(
        &self,
        index: usize,
        block: &Block,
        caret_here: bool,
        group: SharedString,
        line: f32,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.wants_start(block) {
            return None;
        }
        let at = self.tail_point(index)?;
        let id = block.id;
        Some(
            div()
                .absolute()
                .left(at.x)
                .top(at.y + px((line - ACCESSORY_HEIGHT) / 2.0))
                .child(
                    div()
                        .id(("todo-start", id))
                        .h(px(ACCESSORY_HEIGHT))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(7.0))
                        .cursor_pointer()
                        .text_size(px(12.0))
                        .text_color(colors.secondary)
                        .when(!caret_here, |el| {
                            el.opacity(0.0).group_hover(group, |el| el.opacity(1.0))
                        })
                        .hover(|el| el.bg(colors.primary.alpha(0.06)).text_color(colors.primary))
                        .child(crate::i18n::t("notes.work.start"))
                        .child(div().text_color(colors.tertiary).child(START_SHORTCUT))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(EditorEvent::Work(WorkRequest::Prepare { block: id }));
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The live line under a started to-do: where the work stands, the
    /// question when the agent waits, and what can be done about it.
    pub(super) fn work_status_line(
        &self,
        block: &Block,
        indent: f32,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(block.kind, BlockKind::Todo { .. }) {
            return None;
        }
        let id = block.id;
        let session = work::current_session(block);
        let (dot, text, detail): (Option<gpui::Rgba>, String, Option<String>) =
            if let Some(reason) = self.work.failure(id) {
                (
                    Some(Ink::DANGER),
                    crate::i18n::t("notes.work.failed").to_owned(),
                    Some(one_line(reason, 160)),
                )
            } else {
                match self.work.state(block) {
                    WorkState::Ready | WorkState::Done => return None,
                    WorkState::Starting => (
                        Some(colors.primary.alpha(0.3)),
                        crate::i18n::t("notes.work.starting").to_owned(),
                        None,
                    ),
                    WorkState::Working => (
                        Some(colors.primary.alpha(0.54)),
                        crate::i18n::t("notes.work.working").to_owned(),
                        None,
                    ),
                    WorkState::NeedsYou(question) => (
                        Some(Ink::ATTENTION),
                        crate::i18n::t("notes.work.needs_you").to_owned(),
                        Some(one_line(
                            question.excerpt.as_deref().unwrap_or(&question.summary),
                            200,
                        )),
                    ),
                    WorkState::Review(pr) => (
                        Some(Ink::FRESH),
                        match pr {
                            Some(pr) => {
                                crate::i18n::tf("notes.work.review_pr", &[("number", &pr.number)])
                            }
                            None => crate::i18n::t("notes.work.review").to_owned(),
                        },
                        None,
                    ),
                    state @ (WorkState::Stopped | WorkState::Archived | WorkState::Missing) => {
                        (None, state.label().to_owned(), None)
                    }
                }
            };
        let state = self.work.state(block);
        let mut actions: Vec<(&'static str, WorkRequest)> = Vec::new();
        if let Some(session) = session.clone()
            && !matches!(state, WorkState::Missing | WorkState::Archived)
            && self.work.failure(id).is_none()
        {
            let label = if matches!(state, WorkState::NeedsYou(_)) {
                crate::i18n::t("notes.work.answer")
            } else {
                crate::i18n::t("notes.work.open")
            };
            actions.push((label, WorkRequest::Open { session }));
        }
        if self.work.failure(id).is_some() {
            actions.push((
                crate::i18n::t("notes.work.retry"),
                WorkRequest::Prepare { block: id },
            ));
        } else if block.kind == (BlockKind::Todo { checked: false }) && state.can_restart() {
            actions.push((
                crate::i18n::t("notes.work.restart"),
                WorkRequest::Prepare { block: id },
            ));
        }

        let mut line = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(20.0))
            .text_size(px(12.5))
            .text_color(colors.secondary)
            .when_some(dot, |el, color| {
                el.child(div().flex_none().size(px(6.0)).rounded_full().bg(color))
            })
            .child(div().font_weight(FontWeight::MEDIUM).child(text));
        for (index, (label, request)) in actions.into_iter().enumerate() {
            line = line
                .child(div().text_color(colors.tertiary).child("·"))
                .child(
                    div()
                        .id(SharedString::from(format!("work-action-{id}-{index}")))
                        .cursor_pointer()
                        .text_color(colors.tertiary)
                        .hover(|el| el.text_color(accent()))
                        .child(label)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(EditorEvent::Work(request.clone()));
                            }),
                        ),
                );
        }
        Some(
            div()
                .id(("work-status", id))
                .w_full()
                .pl(px(indent + super::editor_view::MARKER_WIDTH))
                .pb(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(line)
                .when_some(detail, |el, detail| {
                    el.child(
                        div()
                            .pl(px(14.0))
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .text_color(colors.primary.alpha(0.78))
                            .child(detail),
                    )
                })
                .into_any_element(),
        )
    }

    /// Anchors an open panel under its to-do's text. Runs before the frame
    /// replaces the block layouts, while last frame's are still measured.
    pub(super) fn anchor_work_menu(&mut self) {
        let Some(block) = self.work.start.as_ref().map(|p| p.block).or(self
            .work
            .tick
            .as_ref()
            .map(|p| p.block))
        else {
            return;
        };
        if let Some(index) = self.block_index(block) {
            // Under the row's first character, like a dropdown from the row.
            self.anchor_menus_at(Pos::new(index, 0));
        }
    }

    /// The open Start or "still working" panel, hosted like the `/` and `@`
    /// menus: a glass panel window in the app, a floating surface in the
    /// window otherwise, anchored under the to-do.
    pub(super) fn work_menu(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let block = self.work.start.as_ref().map(|p| p.block).or(self
            .work
            .tick
            .as_ref()
            .map(|p| p.block))?;
        self.block_index(block)?;
        if self.work.start.is_some() {
            let height = self.start_panel_height();
            self.host_menu(
                START_PANEL,
                Self::start_panel_rows,
                PANEL_WIDTH,
                height,
                window,
                cx,
            )
        } else {
            let height = super::editor_view::menu_height(TICK_CHOICES.len(), 1) + LABEL_HEIGHT;
            self.host_menu(
                TICK_PANEL,
                Self::tick_panel_rows,
                TICK_WIDTH,
                height,
                window,
                cx,
            )
        }
    }

    fn start_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.start_panel_rows(cx)?;
        Some(
            floating::surface(self.colors(), floating::MENU_RADIUS, PANEL_WIDTH, rows)
                .into_any_element(),
        )
    }

    fn tick_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.tick_panel_rows(cx)?;
        Some(
            floating::surface(self.colors(), floating::MENU_RADIUS, TICK_WIDTH, rows)
                .into_any_element(),
        )
    }

    fn start_panel_height(&self) -> f32 {
        let Some(panel) = &self.work.start else {
            return 0.0;
        };
        let lines: usize = panel
            .brief
            .prompt
            .lines()
            .map(|line| line.chars().count() / PREVIEW_CHARS_PER_LINE + 1)
            .sum();
        let preview = (lines as f32 * PREVIEW_LINE + 16.0).min(PREVIEW_HEIGHT);
        super::editor_view::menu_height(panel.agents.len().max(1), 1)
            + 2.0 * LABEL_HEIGHT
            + preview
            + 8.0
    }

    fn start_panel_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let colors = self.colors();
        let panel = self.work.start.as_ref()?;
        let mut list = div()
            .flex()
            .flex_col()
            .py(px(floating::MENU_PADDING_Y))
            .child(section_label(
                crate::i18n::t("notes.work.start_with").into(),
                None,
                colors,
            ));
        if panel.agents.is_empty() {
            list = list.child(super::editor_view::menu_empty(
                crate::i18n::t("notes.work.no_agents"),
                colors,
            ));
        }
        for (index, agent) in panel.agents.iter().enumerate() {
            let logo = AgentLogo::new(
                crate::session_presentation::ui_agent_kind(&agent.kind),
                super::editor_view::MENU_LOGO,
                colors,
            )
            .badged(false)
            .inset(floating::MENU_AGENT_LOGO_INSET)
            .monochrome(true)
            .into_any_element();
            let row = floating::menu_row(
                ("note-work-agent", index),
                logo,
                colors,
                index == panel.selected,
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered
                    && let Some(panel) = &mut this.work.start
                    && panel.selected != index
                {
                    panel.selected = index;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.confirm_start(index, cx);
                }),
            )
            .child(super::editor_view::menu_label(agent.name.clone(), colors))
            .when(index == panel.selected, |row| {
                row.child(floating::menu_shortcut("↩", colors))
            })
            .when(agent.session.is_some() && index != panel.selected, |row| {
                row.child(floating::menu_shortcut(
                    crate::i18n::t("notes.work.mentioned"),
                    colors,
                ))
            })
            .when(
                agent.is_default && agent.session.is_none() && index != panel.selected,
                |row| {
                    row.child(floating::menu_shortcut(
                        crate::i18n::t("notes.work.default"),
                        colors,
                    ))
                },
            );
            list = list.child(row);
        }
        let brief = &panel.brief;
        let mut summary = vec![format_bytes(brief.prompt.len())];
        if brief.context_lines > 0 {
            summary.push(plural(
                brief.context_lines,
                "notes.work.brief.context_lines_one",
                "notes.work.brief.context_lines_other",
            ));
        }
        if brief.links > 0 {
            summary.push(plural(
                brief.links,
                "notes.work.brief.links_one",
                "notes.work.brief.links_other",
            ));
        }
        if brief.notes > 0 {
            summary.push(plural(
                brief.notes,
                "notes.work.brief.notes_one",
                "notes.work.brief.notes_other",
            ));
        }
        if brief.sessions > 0 {
            summary.push(plural(
                brief.sessions,
                "notes.work.brief.sessions_one",
                "notes.work.brief.sessions_other",
            ));
        }
        if brief.truncated {
            summary.push(crate::i18n::t("notes.work.brief.shortened").to_owned());
        }
        list = list
            .child(floating::menu_separator(colors))
            .child(section_label(
                crate::i18n::t("notes.work.brief.title").into(),
                Some(summary.join(" · ").into()),
                colors,
            ))
            .child(
                div()
                    .id("note-work-brief")
                    .mx(px(floating::MENU_ROW_MARGIN + 4.0))
                    .mb(px(4.0))
                    .max_h(px(PREVIEW_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&panel.scroll)
                    .rounded(px(8.0))
                    .bg(colors.primary.alpha(0.045))
                    .px(px(10.0))
                    .py(px(8.0))
                    .font_family(crate::fonts::mono_family())
                    .text_size(px(11.0))
                    .line_height(px(PREVIEW_LINE))
                    .text_color(colors.primary.alpha(0.8))
                    .child(readable_brief(brief.prompt.trim_end())),
            );
        Some(list)
    }

    fn tick_panel_rows(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let colors = self.colors();
        let panel = self.work.tick.as_ref()?;
        let mut list = div()
            .flex()
            .flex_col()
            .py(px(floating::MENU_PADDING_Y))
            .child(section_label(
                crate::i18n::t("notes.work.still_working").into(),
                None,
                colors,
            ));
        for (index, label) in TICK_CHOICES.iter().enumerate() {
            if index == 2 {
                list = list.child(floating::menu_separator(colors));
            }
            let icon = sf_symbol(
                ["checkmark.circle.fill", "checkmark", "xmark"][index],
                super::editor_view::MENU_ICON,
                colors.secondary,
            );
            let row = floating::menu_row(
                ("note-work-tick", index),
                icon,
                colors,
                index == panel.selected,
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered
                    && let Some(panel) = &mut this.work.tick
                    && panel.selected != index
                {
                    panel.selected = index;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.confirm_tick(index, cx);
                }),
            )
            .child(super::editor_view::menu_label(
                crate::i18n::t(label),
                colors,
            ))
            .when(index == 2, |row| {
                row.child(floating::menu_shortcut("esc", colors))
            });
            list = list.child(row);
        }
        Some(list)
    }
}

pub(super) const START_PANEL: floating::Target<NoteEditorView> = floating::Target {
    key: "note-work-start",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::start_panel_content,
    dismiss: |this, _, cx| {
        this.work.start = None;
        cx.notify();
    },
};

pub(super) const TICK_PANEL: floating::Target<NoteEditorView> = floating::Target {
    key: "note-work-tick",
    radius: floating::MENU_RADIUS,
    content: NoteEditorView::tick_panel_content,
    dismiss: |this, _, cx| {
        this.work.tick = None;
        cx.notify();
    },
};

/// A quiet heading inside a panel, with optional trailing detail.
fn section_label(
    text: SharedString,
    detail: Option<SharedString>,
    colors: SemanticColors,
) -> impl IntoElement {
    div()
        .h(px(LABEL_HEIGHT))
        .px(px(floating::MENU_ROW_MARGIN + floating::MENU_ROW_INSET))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .text_size(px(Typo::META.size))
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.secondary)
                .child(text),
        )
        .when_some(detail, |el, detail| {
            el.child(
                div()
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(colors.tertiary)
                    .child(detail),
            )
        })
}

fn one_line(text: &str, max_chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max_chars {
        return line;
    }
    let cut: String = line.chars().take(max_chars - 1).collect();
    format!("{}…", cut.trim_end())
}

fn plural(count: usize, one: &'static str, many: &'static str) -> String {
    crate::i18n::tf(if count == 1 { one } else { many }, &[("count", &count)])
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        crate::i18n::tf("notes.work.brief.bytes", &[("count", &bytes)])
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// Writes the new session's chip into the to-do in the note file, so the
/// link lands even when the note was closed while the agent started. The
/// to-do is found by the text it had when Start was pressed; the open
/// editor links by block id as well, and both writes are idempotent.
pub(crate) fn link_in_file(link: &crate::store::WorkLink, session: &SessionId) {
    let Ok(store) = diri_notes::store::NoteStore::open(link.notes_dir.clone()) else {
        return;
    };
    let _ = store.update(&link.note_id, &diri_notes::history::Author::User, |note| {
        let found = note.doc.blocks.iter().position(|block| {
            matches!(block.kind, BlockKind::Todo { .. }) && work::task_text(block) == link.todo_text
        });
        if let Some(index) = found {
            diri_notes::handoff::link_session(note, index, &link.label, &session.0);
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// The pane: live facts in, spawns and selections out

impl super::NotePane {
    /// Live facts for every session the open note links, plus any starts the
    /// Engine has answered. Runs on every session-store change.
    pub(crate) fn push_work(&mut self, cx: &mut Context<Self>) {
        let super::PaneState::Open(open) = &self.state else {
            return;
        };
        let editor = open.editor.clone();
        // Answered starts first, so a session linked just now gets its
        // facts in this same pass.
        let pending = editor.read(cx).work.pending_tickets();
        let outcomes: Vec<_> = {
            let mut store = self.runtime.store.write().expect("store");
            pending
                .into_iter()
                .filter_map(|(block, ticket)| Some((block, store.take_work_outcome(ticket)?)))
                .collect()
        };
        if !outcomes.is_empty() {
            editor.update(cx, |view, cx| {
                for (block, outcome) in outcomes {
                    view.work_finished(block, outcome, super::editor_view_now_ms(), cx);
                }
            });
        }
        let linked: Vec<String> = editor
            .read(cx)
            .editor
            .blocks()
            .iter()
            .flat_map(work::sessions)
            .collect();
        let facts: HashMap<String, SessionFacts> = {
            let store = self.runtime.store.read().expect("store");
            linked
                .into_iter()
                .filter_map(|id| {
                    let record = store.sessions().get(&SessionId::new(id.clone()))?;
                    Some((id, SessionFacts::from_record(record)))
                })
                .collect()
        };
        let handoff = self.handoff_targets(&editor, cx);
        editor.update(cx, |view, cx| {
            let handoff_changed = view.work.set_handoff(handoff);
            if view.work.facts != facts {
                view.work.set_facts(facts);
                cx.notify();
            } else if handoff_changed {
                cx.notify();
            }
        });
    }

    /// Unticked to-dos whose newest session chip names an agent the user
    /// mentioned rather than one Start made: not this note's child, and not
    /// yet reporting under the to-do. Start asks that session to take it on.
    fn handoff_targets(
        &self,
        editor: &gpui::Entity<super::editor_view::NoteEditorView>,
        cx: &Context<Self>,
    ) -> HashMap<BlockId, String> {
        let super::PaneState::Open(open) = &self.state else {
            return HashMap::new();
        };
        let view = editor.read(cx);
        let blocks = view.editor.blocks();
        let store = self.runtime.store.read().expect("store");
        let mut out = HashMap::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.kind != (BlockKind::Todo { checked: false }) {
                continue;
            }
            let Some(session) = work::current_session(block) else {
                continue;
            };
            let Some(record) = store.sessions().get(&SessionId::new(session.clone())) else {
                continue;
            };
            let askable = !record.is_note()
                && !record.is_archived()
                && !record.effective_kind().is_terminal()
                && record.parent.as_ref() != Some(&open.session)
                && !work::has_update_from(blocks, index, &session);
            if askable {
                out.insert(block.id, session);
            }
        }
        out
    }

    /// The Start panel row for the session a to-do mentions.
    fn mentioned_choice(&self, session: &str) -> Option<AgentChoice> {
        let store = self.runtime.store.read().expect("store");
        let record = store.sessions().get(&SessionId::new(session.to_owned()))?;
        Some(AgentChoice {
            kind: record.effective_kind().clone(),
            name: crate::switcher::display_title_str(record).to_owned(),
            is_default: true,
            session: Some(session.to_owned()),
        })
    }

    /// Start on a to-do that mentions a session: send that session the
    /// brief, and note under the to-do that it was asked, which is also what
    /// turns the to-do from "Start" into tracked work.
    fn hand_off_work(&mut self, block: BlockId, session: &str, cx: &mut Context<Self>) {
        let Some((note, todo)) = self.work_note(block, cx) else {
            return;
        };
        let brief = self.brief_for(&note, todo);
        let label = {
            let store = self.runtime.store.read().expect("store");
            let Some(record) = store.sessions().get(&SessionId::new(session.to_owned())) else {
                return;
            };
            diri_notes::mention::session_label("", crate::switcher::display_title_str(record))
        };
        let command = crate::notifications::SendTextCommand {
            session_id: SessionId::new(session.to_owned()),
            text: brief.prompt,
            submit: true,
        };
        #[cfg(test)]
        self.sent_for_test.push(command.clone());
        let _ = self.runtime.notification_action_sender().send(command);
        let super::PaneState::Open(open) = &self.state else {
            return;
        };
        open.editor.update(cx, |view, cx| {
            let Some(index) = view.block_index(block) else {
                return;
            };
            if view.editor.add_update(
                index,
                &diri_notes::handoff::entry_stamp(),
                &label,
                session,
                "Asked to take this on",
                super::editor_view_now_ms(),
            ) {
                cx.emit(EditorEvent::Changed);
            }
            if view.editor.has_children(index) {
                view.set_folded(index, true, cx);
            }
        });
        self.save(cx);
        self.push_work(cx);
    }

    pub(crate) fn on_work(&mut self, request: &WorkRequest, cx: &mut Context<Self>) {
        match request {
            WorkRequest::Prepare { block } => self.prepare_work(*block, cx),
            WorkRequest::Start {
                block,
                session: Some(session),
                ..
            } => self.hand_off_work(*block, session, cx),
            WorkRequest::Start {
                block,
                kind,
                agent_name,
                session: None,
            } => self.start_work(*block, kind.clone(), agent_name, cx),
            WorkRequest::Open { session } => {
                let id = SessionId::new(session.clone());
                if self
                    .runtime
                    .store
                    .read()
                    .expect("store")
                    .sessions()
                    .contains_key(&id)
                {
                    self.save(cx);
                    cx.emit(super::NotePaneEvent::Reveal(id));
                }
            }
            WorkRequest::Stop { session } => {
                self.runtime
                    .store
                    .write()
                    .expect("store")
                    .archive_sessions(vec![SessionId::new(session.clone())]);
            }
        }
    }

    /// The note as it stands in the editor, and the document index of the
    /// editor block `block` (the editor's block 0 is the title).
    fn work_note(
        &self,
        block: BlockId,
        cx: &Context<Self>,
    ) -> Option<(diri_notes::store::Note, usize)> {
        let super::PaneState::Open(open) = &self.state else {
            return None;
        };
        let view = open.editor.read(cx);
        let index = view.editor.blocks().iter().position(|b| b.id == block)?;
        let note = diri_notes::store::Note {
            front: open.front.clone(),
            doc: view.editor.document(),
        };
        Some((note, index.checked_sub(1)?))
    }

    fn brief_for(&self, note: &diri_notes::store::Note, todo: usize) -> Brief {
        let super::PaneState::Open(open) = &self.state else {
            return Brief::default();
        };
        let mut notes = Vec::new();
        let mut sessions = Vec::new();
        let store = self.runtime.store.read().expect("store");
        for target in work::mentions(&note.doc.blocks, todo) {
            match target {
                diri_notes::mention::MentionTarget::Note(id) => {
                    if let Some(loaded) = self.store.as_ref().and_then(|s| s.load(&id).ok()) {
                        let body = diri_notes::markdown::write(
                            &diri_notes::markdown::FrontMatter::default(),
                            &loaded.doc,
                        );
                        notes.push(work::ResolvedNote {
                            id,
                            title: loaded.doc.title.clone(),
                            body,
                        });
                    }
                }
                diri_notes::mention::MentionTarget::Session(id) => {
                    if let Some(record) = store.sessions().get(&SessionId::new(id.clone())) {
                        let facts = SessionFacts::from_record(record);
                        let state = work::state(false, Some(Some(&facts)));
                        sessions.push(work::ResolvedSession {
                            id,
                            kind: record.effective_kind().id().to_owned(),
                            title: record.title.clone(),
                            status: state_word(&state).to_owned(),
                        });
                    }
                }
            }
        }
        work::brief(&open.id, note, todo, &notes, &sessions)
    }

    /// Installed agents, the user's default first; a Terminal default falls
    /// back to the first agent, since a shell cannot read a brief.
    fn work_agents(&self) -> Vec<AgentChoice> {
        let store = self.runtime.store.read().expect("store");
        let catalog = store.agent_catalog(None);
        let default = crate::agent_catalog::resolved_target_agent(
            &store.preferences().default_agent,
            catalog,
        );
        let mut agents: Vec<AgentChoice> = crate::agent_catalog::quick_agent_options(catalog)
            .into_iter()
            .filter(|option| option.available && !option.kind.is_terminal())
            .map(|option| AgentChoice {
                is_default: option.kind == default,
                kind: option.kind,
                name: option.display_name,
                session: None,
            })
            .collect();
        if !agents.iter().any(|a| a.is_default)
            && let Some(first) = agents.first_mut()
        {
            first.is_default = true;
        }
        agents.sort_by_key(|a| !a.is_default);
        agents
    }

    fn prepare_work(&mut self, block: BlockId, cx: &mut Context<Self>) {
        let Some((note, todo)) = self.work_note(block, cx) else {
            return;
        };
        let brief = self.brief_for(&note, todo);
        let mut agents = self.work_agents();
        let super::PaneState::Open(open) = &self.state else {
            return;
        };
        let mentioned = open.editor.read(cx).work.handoff(block).map(str::to_owned);
        if let Some(choice) = mentioned.and_then(|id| self.mentioned_choice(&id)) {
            for agent in &mut agents {
                agent.is_default = false;
            }
            agents.insert(0, choice);
        }
        open.editor.update(cx, |view, cx| {
            if let Some(index) = view.editor.blocks().iter().position(|b| b.id == block) {
                let end = view.editor.block(index).text.len();
                view.editor.set_caret(Pos::new(index, end));
            }
            view.open_start_panel(block, agents, brief, cx);
        });
    }

    fn start_work(
        &mut self,
        block: BlockId,
        kind: AgentKind,
        agent_name: &str,
        cx: &mut Context<Self>,
    ) {
        let Some((note, todo)) = self.work_note(block, cx) else {
            return;
        };
        let brief = self.brief_for(&note, todo);
        let todo_block = &note.doc.blocks[todo];
        let title = work::task_title(todo_block);
        // The chip sits right after the to-do's own words: name the agent,
        // not the task again.
        let label = diri_notes::mention::session_label(agent_name, "");
        // The file must hold the to-do before the Engine answers, so the
        // executor can link it even if this note is closed by then.
        self.save(cx);
        let (Some(notes), super::PaneState::Open(open)) = (self.store.as_ref(), &self.state) else {
            return;
        };
        let link = crate::store::WorkLink {
            notes_dir: notes.dir().to_path_buf(),
            note_id: open.id.clone(),
            todo_text: work::task_text(todo_block),
            label: label.clone(),
        };
        let editor = open.editor.clone();
        let note_session = open.session.clone();
        let ticket = {
            let mut store = self.runtime.store.write().expect("store");
            let cwd = store.sessions().get(&note_session).map(|s| s.cwd.clone());
            let params = store.spawn_params(
                kind,
                crate::store::SpawnOptions {
                    cwd,
                    title: Some(title),
                    initial_prompt: Some(brief.prompt),
                    parent: Some(note_session),
                    ..crate::store::SpawnOptions::default()
                },
            );
            store.start_work_item(params, link)
        };
        editor.update(cx, |view, cx| view.work_started(block, ticket, label, cx));
    }
}

/// One word for a session's state, for the brief's list of mentioned
/// sessions.
fn state_word(state: &WorkState) -> &'static str {
    match state {
        WorkState::Ready | WorkState::Starting => "starting",
        WorkState::Working => "working",
        WorkState::NeedsYou(_) => "waiting for input",
        WorkState::Review(_) => "idle",
        WorkState::Stopped => "exited",
        WorkState::Archived => "archived",
        WorkState::Missing => "unknown",
        WorkState::Done => "done",
    }
}

/// The preview reads like the note: `[@Claude](diri://session/…)` shows as
/// `@Claude`. The agent still receives the links, which carry the ids it
/// needs to find those sessions and notes.
fn readable_brief(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while let Some(at) = rest.find("](diri://") {
        let Some(open) = rest[..at].rfind('[') else {
            break;
        };
        let Some(close) = rest[at..].find(')') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..at]);
        rest = &rest[at + close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod readable_brief_tests {
    #[test]
    fn diri_links_read_as_their_labels() {
        assert_eq!(
            super::readable_brief(
                "- [ ] Draft [@Claude](diri://session/s_1) and [spec](https://x.dev)"
            ),
            "- [ ] Draft @Claude and [spec](https://x.dev)"
        );
    }
}
