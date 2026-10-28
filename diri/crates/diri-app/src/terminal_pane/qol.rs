//! Client-owned terminal interactions. No PTY parsing or remote execution here.
use super::autoscroll;
use super::*;
use diri_proto::grid::{GridCell, GridRowCodec, RowMetadata, TermStyle};
use diri_term::command_marks::{CommandOutput, NewestCommandOutput};
use diri_term::element::ReferenceHit;
use gpui::{Pixels, Point};
use std::io::Write;

#[derive(Default)]
pub(super) struct QolState {
    session: Option<SessionId>,
    pub hover: Option<(usize, usize)>,
    hover_key: Option<(usize, usize, u64, i64, Option<u64>)>,
    pub hit: Option<ReferenceHit>,
    /// The window row of the finished command's mark the pointer rests on,
    /// in the gutter left of the grid; a click there selects its output.
    pub gutter_mark: Option<usize>,
    pub pressed: Option<(ReferenceHit, (usize, usize))>,
    pub menu: Option<TerminalMenu>,
    pub copy_mode: Option<CopyMode>,
    pub paste: Option<PendingPaste>,
    /// The system alert asking about `paste`, while it is up.
    paste_prompt: Option<Task<()>>,
    /// Last toast message, retained briefly to suppress repeated rejections.
    pub feedback: Option<String>,
    pub(super) feedback_generation: u64,
    feedback_timer: Option<Task<()>>,
    /// Selection drag held past an edge: cell under the pointer and signed
    /// pixels past the edge, positive above the top.
    pub drag: Option<(SessionId, usize, usize, f32)>,
    pub autoscroll: Option<Autoscroll>,
    autoscroll_generation: u64,
    export_files: Vec<tempfile::NamedTempFile>,
    pub(super) busy: bool,
    /// The history read whose result may still land; a newer read or a
    /// session switch replaces it.
    pub(super) read_owner: Option<Arc<()>>,
    pub(super) message_jump: Option<super::messages::MessageJump>,
    pub(super) message_mark: Option<super::messages::MessageMark>,
    pub(super) message_tokens: u64,
    /// Which file references under the pointer name a real local file.
    files: crate::file_links::ExistenceCache,
}

impl QolState {
    pub(super) fn clear_feedback(&mut self) {
        self.feedback = None;
        self.feedback_timer = None;
        self.feedback_generation += 1;
    }

    pub fn hover_key_clear(&mut self) {
        self.hover_key = None;
    }
}

/// A selection autoscroll in flight. It advances once per display-link
/// frame, integrating the time since the last one, so it moves at 120 Hz on
/// ProMotion rather than at a 16 ms timer's ~53 uneven frames a second.
pub(super) struct Autoscroll {
    accumulator: autoscroll::Accumulator,
    last: Instant,
    /// Tells a frame callback left over from a cancelled run to stop.
    generation: u64,
}

pub(super) struct PendingPaste {
    pub text: String,
    id: SessionId,
    generation: AttachmentGeneration,
    bracketed: bool,
    /// Staged at a password prompt: the review must not show on screen what
    /// the terminal itself is hiding.
    secret: bool,
    cancel_selected: bool,
}

pub(super) struct CopyMode {
    col: usize,
    row: usize,
    selecting: bool,
}

#[derive(Clone)]
pub(super) struct TerminalMenu {
    position: Point<Pixels>,
    target: Option<ReferenceHit>,
    selected: usize,
    actions: Vec<MenuAction>,
    /// The editor file links open in, named by the Open item.
    editor: Option<crate::file_links::Editor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Open,
    CopyLink,
    OpenFile,
    CopyPath,
    Copy,
    Paste,
    Find,
    CopyMode,
    Export,
    PreviousPrompt,
    NextPrompt,
    PreviousMessage,
    NextMessage,
    /// The output of the finished shell command under the pointer.
    SelectCommandOutput(CommandOutput),
    CopyCommandOutput(CommandOutput),
}

/// What a prompt jump that finds no prompt mark does.
#[derive(Clone, Copy)]
pub(super) enum PromptMiss {
    /// Explains that prompt jumps need shell integration.
    NeedsMarks,
    /// Says this instead: a message jump found nothing either.
    Say(&'static str),
    /// Returns to the latest output, as Next past the last message does.
    Live,
}

impl MenuAction {
    fn label(self, editor: Option<crate::file_links::Editor>) -> SharedString {
        match self {
            Self::OpenFile => match editor {
                Some(editor) => tf("terminal.menu.open_in", &[("editor", &editor.name())]).into(),
                None => t("terminal.menu.open_file").into(),
            },
            other => other.static_label().into(),
        }
    }

    fn static_label(self) -> &'static str {
        t(match self {
            Self::Open => "terminal.menu.open_link",
            Self::CopyLink => "terminal.menu.copy_link",
            Self::OpenFile => "terminal.menu.open_file",
            Self::CopyPath => "terminal.menu.copy_path",
            Self::Copy => "terminal.menu.copy_selection",
            Self::Paste => "terminal.menu.paste",
            Self::Find => "terminal.menu.find_selection",
            Self::CopyMode => "terminal.menu.copy_mode",
            Self::Export => "terminal.menu.export",
            Self::PreviousPrompt => "terminal.menu.previous_prompt",
            Self::NextPrompt => "terminal.menu.next_prompt",
            Self::PreviousMessage => "terminal.menu.previous_message",
            Self::NextMessage => "terminal.menu.next_message",
            Self::SelectCommandOutput(_) => "terminal.menu.select_command_output",
            Self::CopyCommandOutput(_) => "terminal.menu.copy_command_output",
        })
    }
}

impl TerminalPane {
    pub(super) fn reset_qol_session(&mut self, id: &SessionId) {
        if self.qol.session.as_ref() != Some(id) {
            if let Some(previous) = self
                .qol
                .session
                .as_ref()
                .and_then(|id| self.residents.get(id))
            {
                previous.element.pin_keyboard_selection(false);
                previous.element.hold_frame(false);
            }
            self.qol = QolState {
                session: Some(id.clone()),
                ..Default::default()
            };
        }
    }

    pub(super) fn refresh_link_hover(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.reset_qol_session(&id);
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let Some((col, row)) = self.qol.hover else {
            self.qol.hit = None;
            self.qol.hover_key = None;
            return;
        };
        let (generation, offset, sequence) = resident.element.reference_revision();
        let key = (col, row, generation, offset, sequence);
        if self.qol.hover_key != Some(key) {
            self.qol.hover_key = Some(key);
            let hit = resident.element.reference_hit_at(col, row);
            self.qol.hit = self.linkable(hit);
        }
    }

    /// Keeps a reference only if clicking it would open something: every web
    /// URL, but a file reference only when it names a file on this Mac.
    /// Hover, press, release and the context menu all ask here, so what is
    /// underlined is exactly what opens.
    pub(super) fn linkable(&mut self, hit: Option<ReferenceHit>) -> Option<ReferenceHit> {
        let hit = hit?;
        match &hit.reference {
            TerminalReference::Url(_) => Some(hit),
            TerminalReference::File(reference) => {
                self.local_file(reference).is_some().then_some(hit)
            }
        }
    }

    /// Where a file reference in the selected session points, if it exists.
    /// A remote session's paths name the remote host, so none resolve here.
    fn local_file(&mut self, reference: &str) -> Option<crate::file_links::LocalFile> {
        let session = self.selected_session()?;
        let bases = file_reference_bases(&session)?;
        let bases: Vec<&std::path::Path> = bases.iter().map(std::path::PathBuf::as_path).collect();
        self.qol.files.resolve(&bases, reference, Instant::now())
    }

    /// The editor a file link opens in under the current preference.
    pub(super) fn file_editor(&self) -> Option<crate::file_links::Editor> {
        let choice = self
            .runtime
            .store
            .read()
            .expect("store")
            .preferences()
            .terminal_file_editor;
        crate::file_links::editor_for(choice)
    }

    pub(super) fn open_reference(
        &mut self,
        reference: TerminalReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match reference {
            TerminalReference::Url(url) => cx.open_url(&url),
            TerminalReference::File(reference) => {
                let opened = self
                    .local_file(&reference)
                    .and_then(|file| crate::file_links::open_url(&file, self.file_editor()));
                match opened {
                    Some(url) => cx.open_url(&url),
                    None => self.show_terminal_feedback(t("terminal.file_not_local"), window, cx),
                }
            }
        }
    }

    pub(super) fn show_terminal_feedback(
        &mut self,
        text: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.into();
        if self.qol.feedback.as_deref() == Some(text.as_str()) {
            return;
        }
        cx.emit(TerminalPaneEvent::Feedback {
            message: text.clone(),
        });
        self.qol.feedback = Some(text);
        self.qol.feedback_generation += 1;
        let generation = self.qol.feedback_generation;
        let session = self.selected_id();
        cx.notify();
        self.qol.feedback_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, _, cx| {
                if this.selected_id() == session && this.qol.feedback_generation == generation {
                    this.qol.feedback = None;
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn open_terminal_menu(
        &mut self,
        position: Point<Pixels>,
        col: usize,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.reset_qol_session(&id);
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let target = resident.element.reference_hit_at(col, row);
        let has_selection = !resident.element.selected_text().is_empty();
        let command_output = resident.element.command_output_at(col, row);
        let target = self.linkable(target);
        let mut actions = Vec::new();
        match target.as_ref().map(|hit| &hit.reference) {
            Some(TerminalReference::Url(_)) => {
                actions.extend([MenuAction::Open, MenuAction::CopyLink]);
            }
            Some(TerminalReference::File(_)) => {
                actions.extend([MenuAction::OpenFile, MenuAction::CopyPath]);
            }
            None => {}
        }
        if has_selection {
            actions.extend([MenuAction::Copy, MenuAction::Find]);
        }
        if let Some(output) = command_output {
            actions.extend([
                MenuAction::CopyCommandOutput(output),
                MenuAction::SelectCommandOutput(output),
            ]);
        }
        actions.extend([MenuAction::Paste, MenuAction::CopyMode, MenuAction::Export]);
        if self.message_gutter().is_some() {
            actions.extend([MenuAction::PreviousMessage, MenuAction::NextMessage]);
        } else {
            actions.extend([MenuAction::PreviousPrompt, MenuAction::NextPrompt]);
        }
        let editor = self.file_editor();
        self.qol.menu = Some(TerminalMenu {
            position,
            target,
            selected: 0,
            actions,
            editor,
        });
        self.qol.pressed = None;
        self.qol.drag = None;
        self.qol.autoscroll = None;
        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn run_menu_action(
        &mut self,
        action: MenuAction,
        target: Option<ReferenceHit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.qol.menu = None;
        match action {
            MenuAction::Open | MenuAction::OpenFile => {
                if let Some(hit) = target {
                    self.open_reference(hit.reference, window, cx);
                }
            }
            MenuAction::CopyPath => {
                let file = target.and_then(|hit| match hit.reference {
                    TerminalReference::File(reference) => self.local_file(&reference),
                    TerminalReference::Url(_) => None,
                });
                if let Some(file) = file {
                    cx.write_to_clipboard(ClipboardItem::new_string(file.display()));
                    self.show_terminal_feedback(t("terminal.path_copied"), window, cx);
                }
            }
            MenuAction::CopyLink => {
                if let Some(hit) = target {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        hit.reference.destination().to_owned(),
                    ));
                    self.show_terminal_feedback(t("terminal.link_copied"), window, cx);
                }
            }
            MenuAction::Copy => self.copy_selection(&CopySelection, window, cx),
            MenuAction::Paste => self.paste(&Paste, window, cx),
            MenuAction::Find => self.find_selection(window, cx),
            MenuAction::CopyMode => self.enter_copy_mode(window, cx),
            MenuAction::Export => self.read_terminal_history(None, window, cx),
            MenuAction::PreviousPrompt | MenuAction::PreviousMessage => {
                self.read_terminal_history(Some(false), window, cx)
            }
            MenuAction::NextPrompt | MenuAction::NextMessage => {
                self.read_terminal_history(Some(true), window, cx)
            }
            MenuAction::SelectCommandOutput(output) => {
                if let Some(resident) = self.selected_id().and_then(|id| self.residents.get(&id)) {
                    resident.element.select_range(output.start, output.end);
                }
            }
            MenuAction::CopyCommandOutput(output) => {
                if let Some(resident) = self.selected_id().and_then(|id| self.residents.get(&id)) {
                    resident.element.select_range(output.start, output.end);
                }
                self.copy_selection(&CopySelection, window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn find_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        let text = resident.element.selected_text();
        if text.is_empty() {
            return;
        }
        let text = text.lines().next().unwrap_or_default();
        resident.find_query.select_all();
        resident.find_query.insert(text);
        let find = resident.find.get_or_insert_with(TerminalFindModel::default);
        find.set_query(text, self.started_at.elapsed());
        resident.element.set_find_highlights(Vec::new());
        resident.element.pin_keyboard_selection(false);
        self.qol.copy_mode = None;
        self.schedule_query_search(id, window, cx);
        cx.notify();
    }

    pub(super) fn stage_paste_if_needed(
        &mut self,
        id: &SessionId,
        text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let protect = self
            .runtime
            .store
            .read()
            .expect("store")
            .preferences()
            .terminal_paste_protection;
        let Some(resident) = self.residents.get(id) else {
            return true;
        };
        if protect && diri_term::keys::paste_needs_confirmation(text, resident.bracketed_paste) {
            self.qol.hover = None;
            self.qol.hit = None;
            self.qol.paste = Some(PendingPaste {
                text: text.to_owned(),
                id: id.clone(),
                generation: resident.attachment_generation,
                bracketed: resident.bracketed_paste,
                secret: resident.secret_input,
                cancel_selected: false,
            });
            self.qol.copy_mode = None;
            cx.stop_propagation();
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Raises the system alert for a staged paste, once, and applies its
    /// answer. Called from render like the root's close prompt, because
    /// staging has no window. A paste that is withdrawn while the alert is up
    /// (the session changed) makes its answer a no-op: `confirm_terminal_paste`
    /// finds nothing staged.
    pub(super) fn sync_paste_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(paste) = &self.qol.paste else {
            self.qol.paste_prompt = None;
            return;
        };
        if self.qol.paste_prompt.is_some() {
            return;
        }
        let message = native_paste_review_message(&paste.text, paste.secret);
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            paste_review_title(),
            Some(&message),
            &[
                gpui::PromptButton::ok(t("terminal.menu.paste")),
                gpui::PromptButton::cancel(t("terminal.paste.cancel")),
            ],
            cx,
        );
        self.qol.paste_prompt = Some(cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await.ok();
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                this.qol.paste_prompt = None;
                if choice == Some(0) {
                    this.confirm_terminal_paste(window, cx);
                } else {
                    this.qol.paste = None;
                    cx.notify();
                }
            });
        }));
    }

    fn confirm_terminal_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.qol.paste.take() else {
            return;
        };
        if self.selected_id().as_ref() != Some(&pending.id) {
            return;
        }
        let Some(resident) = self.residents.get(&pending.id) else {
            return;
        };
        if resident.attachment_generation != pending.generation
            || resident.bracketed_paste != pending.bracketed
            || resident.attachment_state != AttachmentState::Live
        {
            self.show_terminal_feedback(t("terminal.paste.changed"), window, cx);
        } else {
            resident.send_user_input(terminal_paste(&pending.text, resident.bracketed_paste));
        }
        cx.notify();
    }

    /// Scrolls while a selection drag holds the pointer past the top or
    /// bottom edge, at a speed set by how far past it is (see
    /// [`autoscroll::lines_per_second`]). Pointer moves only update the
    /// distance; one frame-rate task integrates it into sub-row scroll.
    pub(super) fn update_selection_autoscroll(
        &mut self,
        position: Point<Pixels>,
        col: usize,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let viewport = self.viewport.unwrap_or_default();
        let top = viewport.y + self.header_height() + 2.0;
        let bottom = viewport.y + viewport.height - 10.0;
        let y = f32::from(position.y);
        // Signed like the scroll position: positive is toward history.
        let past = if y < top {
            top - y
        } else if y > bottom {
            -(y - bottom)
        } else {
            0.0
        };
        if autoscroll::lines_per_second(past.abs()) == 0.0 {
            self.qol.drag = None;
            self.qol.autoscroll = None;
            return;
        }
        self.qol.drag = Some((id, col, row, past));
        if self.qol.autoscroll.is_some() {
            return;
        }
        self.qol.autoscroll_generation += 1;
        let generation = self.qol.autoscroll_generation;
        self.qol.autoscroll = Some(Autoscroll {
            accumulator: autoscroll::Accumulator::default(),
            last: Instant::now(),
            generation,
        });
        self.request_autoscroll_frame(generation, window, cx);
    }

    fn request_autoscroll_frame(
        &mut self,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let _ = this.update(cx, |this, cx| {
                let current = this
                    .qol
                    .autoscroll
                    .as_ref()
                    .is_some_and(|run| run.generation == generation);
                if !current {
                    return;
                }
                if this.autoscroll_frame(window, cx) {
                    this.request_autoscroll_frame(generation, window, cx);
                } else {
                    this.qol.autoscroll = None;
                }
            });
        });
    }

    /// Scrolls by the travel owed since the last frame. False ends the run.
    fn autoscroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let Some(run) = self.qol.autoscroll.as_mut() else {
            return false;
        };
        let elapsed = now.saturating_duration_since(run.last);
        run.last = now;
        let Some((id, col, row, past)) = self.qol.drag.clone() else {
            return false;
        };
        if self.selected_id().as_ref() != Some(&id) || !self.focus.is_focused(window) {
            self.qol.drag = None;
            return false;
        }
        let velocity = autoscroll::lines_per_second(past.abs()).copysign(past);
        // Autoscroll is function, not decoration: reduced motion still
        // scrolls, but in whole rows with no sub-row glide.
        let whole_rows = cx.reduce_motion();
        let Some(run) = self.qol.autoscroll.as_mut() else {
            return false;
        };
        let travel = run.accumulator.advance(velocity, elapsed, whole_rows);
        let Some(resident) = self.residents.get(&id) else {
            return false;
        };
        if resident.pointer_owner != Some((MouseButton::Left, PointerOwner::LocalSelection)) {
            return false;
        }
        if travel == 0.0 {
            return true;
        }
        let rows = usize::from(resident.last_size.1);
        let mut before = resident.element.scroll_position();
        if whole_rows {
            // Settle a trackpad's leftover fraction onto the row grid so
            // every step lands on a whole row.
            before = before.round();
        }
        let moved = resident.element.set_scroll_position(before + travel, rows);
        if !moved {
            // Oldest retained row or the live edge: nothing more to reveal
            // until the pointer moves again.
            self.qol.drag = None;
            return false;
        }
        resident.element.drag_selection(col, row);
        self.pump_scrollback_fetch(&id, rows);
        cx.notify();
        true
    }

    pub(super) fn enter_copy_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get_mut(&id) else {
            return;
        };
        resident.find = None;
        resident.element.set_find_highlights(Vec::new());
        resident.element.clear_selection();
        resident.element.pin_keyboard_selection(true);
        resident.element.begin_selection(0, 0);
        resident.element.drag_selection(1, 0);
        self.qol.hover = None;
        self.qol.hit = None;
        self.qol.copy_mode = Some(CopyMode {
            col: 0,
            row: 0,
            selecting: false,
        });
        window.focus(&self.focus, cx);
        self.show_terminal_feedback(t("terminal.copy_mode.on"), window, cx);
    }

    pub(super) fn handle_qol_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        if self.qol.paste.is_some() {
            match key {
                "escape" => {
                    self.qol.paste = None;
                }
                "tab" => {
                    let paste = self.qol.paste.as_mut().expect("pending paste");
                    paste.cancel_selected = !paste.cancel_selected;
                }
                "enter"
                    if self
                        .qol
                        .paste
                        .as_ref()
                        .is_some_and(|paste| paste.cancel_selected) =>
                {
                    self.qol.paste = None;
                }
                "enter" => self.confirm_terminal_paste(window, cx),
                _ => {}
            }
            cx.notify();
            return true;
        }
        if let Some(menu) = self.qol.menu.as_mut() {
            match key {
                "escape" => self.qol.menu = None,
                "down" => menu.selected = (menu.selected + 1) % menu.actions.len(),
                "up" => {
                    menu.selected = (menu.selected + menu.actions.len() - 1) % menu.actions.len()
                }
                "enter" => {
                    let action = menu.actions[menu.selected];
                    let target = menu.target.clone();
                    self.run_menu_action(action, target, window, cx);
                }
                _ => {}
            }
            cx.notify();
            return true;
        }
        let Some(mut mode) = self.qol.copy_mode.take() else {
            return false;
        };
        let Some(id) = self.selected_id() else {
            return true;
        };
        let Some(resident) = self.residents.get(&id) else {
            return true;
        };
        if matches!(key, "escape" | "q") {
            resident.element.pin_keyboard_selection(false);
            resident.element.clear_selection();
            cx.notify();
            return true;
        }
        if matches!(key, "y" | "enter") || (mods.platform && key == "c") {
            self.copy_selection(&CopySelection, window, cx);
            if let Some(resident) = self.residents.get(&id) {
                resident.element.pin_keyboard_selection(false);
                resident.element.clear_selection();
            }
            cx.notify();
            return true;
        }
        let cols = usize::from(resident.element.grid_cols()).max(1);
        let rows = usize::from(resident.element.grid_rows()).max(1);
        mode.col = mode.col.min(cols - 1);
        mode.row = mode.row.min(rows - 1);
        match key {
            "v" | "space" => {
                mode.selecting = !mode.selecting;
                resident.element.begin_selection(mode.col, mode.row);
            }
            "left" | "h" => mode.col = mode.col.saturating_sub(1),
            "right" | "l" => mode.col = (mode.col + 1).min(cols - 1),
            "up" | "k" => {
                if mode.row > 0 {
                    mode.row -= 1;
                } else {
                    resident
                        .element
                        .set_view_offset(resident.element.view_offset() + 1, rows);
                }
            }
            "down" | "j" => {
                if mode.row + 1 < rows {
                    mode.row += 1;
                } else {
                    resident
                        .element
                        .set_view_offset(resident.element.view_offset() - 1, rows);
                }
            }
            "pageup" => {
                resident
                    .element
                    .set_view_offset(resident.element.view_offset() + rows as i64, rows);
            }
            "pagedown" => {
                resident
                    .element
                    .set_view_offset(resident.element.view_offset() - rows as i64, rows);
            }
            "home" | "0" => mode.col = 0,
            "end" | "$" => mode.col = cols - 1,
            "w" | "b" => {
                let viewport = resident.element.viewport();
                let buffer = resident.element.buffer();
                let buffer = buffer.read().expect("grid");
                let cells = viewport.window_row(&buffer, mode.row);
                let word = |col: usize| {
                    cells.get(col).is_some_and(|c| {
                        char::from_u32(c.scalar).is_some_and(|c| c.is_alphanumeric() || c == '_')
                    })
                };
                if key == "w" {
                    while mode.col + 1 < cols && word(mode.col) {
                        mode.col += 1;
                    }
                    while mode.col + 1 < cols && !word(mode.col) {
                        mode.col += 1;
                    }
                } else {
                    mode.col = mode.col.saturating_sub(1);
                    while mode.col > 0 && !word(mode.col) {
                        mode.col -= 1;
                    }
                    while mode.col > 0 && word(mode.col - 1) {
                        mode.col -= 1;
                    }
                }
            }
            _ => {}
        }
        if mode.selecting {
            resident
                .element
                .drag_selection((mode.col + 1).min(cols), mode.row);
        } else {
            resident.element.begin_selection(mode.col, mode.row);
            resident
                .element
                .drag_selection((mode.col + 1).min(cols), mode.row);
        }
        self.qol.copy_mode = Some(mode);
        self.pump_scrollback_fetch(&id, rows);
        cx.notify();
        true
    }

    /// Copies the output of the newest finished shell command in retained
    /// history, found from its OSC 133 output and end marks.
    pub(super) fn copy_last_command_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.reset_qol_session(&id);
        if self.qol.busy || !self.residents.contains_key(&id) {
            return;
        }
        let client = Arc::clone(self.runtime.client());
        let request_id = id.clone();
        let task = self.tokio.spawn(async move {
            let output = find_last_command_output(&client, &request_id).await?;
            Ok::<_, &'static str>(output.map(|output| command_output_text(&output)))
        });
        self.qol.busy = true;
        let read_owner = Arc::new(());
        self.qol.read_owner = Some(Arc::clone(&read_owner));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                if this
                    .qol
                    .read_owner
                    .as_ref()
                    .is_none_or(|owner| !Arc::ptr_eq(owner, &read_owner))
                {
                    return;
                }
                this.qol.busy = false;
                this.qol.read_owner = None;
                if this.selected_id().as_ref() != Some(&id) {
                    return;
                }
                let message = match result {
                    Ok(Ok(Some(text))) if !text.is_empty() => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        t("terminal.command_output.copied")
                    }
                    Ok(Ok(_)) => t("terminal.command_output.none"),
                    Ok(Err(message)) => message,
                    Err(_) => t("terminal.history.read_failed"),
                };
                this.show_terminal_feedback(message, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Uses the same Engine RPC for local and remote history. A changing
    /// sequence aborts instead of exporting mismatched pages or jumping wrong.
    pub(super) fn read_terminal_history(
        &mut self,
        direction: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(next) = direction
            && let Some(gutter) = self.message_gutter()
        {
            self.jump_to_message(next, gutter, window, cx);
            return;
        }
        self.read_history(direction, PromptMiss::NeedsMarks, window, cx);
    }

    /// Exports the history, or with a `direction` jumps to the shell prompt
    /// that way; `miss` says what happens when there is none.
    pub(super) fn read_history(
        &mut self,
        direction: Option<bool>,
        miss: PromptMiss,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(next) = direction {
            self.jump_to_prompt(next, miss, window, cx);
        } else {
            self.export_history(window, cx);
        }
    }

    /// Scrolls the nearest shell prompt that way to the top of the window.
    /// Rows this view already holds answer at once, with no Engine round
    /// trip; only a prompt beyond them is searched for in the Engine's
    /// history, outward from the window and no further than that prompt.
    fn jump_to_prompt(
        &mut self,
        next: bool,
        miss: PromptMiss,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use diri_term::command_marks::PromptSearch;
        let Some(id) = self.selected_id() else {
            return;
        };
        self.reset_qol_session(&id);
        if self.qol.busy {
            return;
        }
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        match resident.element.held_prompt_target(next) {
            PromptSearch::Found(target) => {
                let rows = usize::from(resident.element.grid_rows());
                resident.element.scroll_to_absolute(target, 0.0, rows);
                self.pump_scrollback_fetch(&id, rows);
                self.qol.feedback = None;
                cx.notify();
                return;
            }
            PromptSearch::Absent => {
                self.prompt_missed(&id, miss, window, cx);
                cx.notify();
                return;
            }
            PromptSearch::Unknown => {}
        }
        let top_offset = resident.element.view_offset();
        let generation = resident.attachment_generation;
        let client = Arc::clone(self.runtime.client());
        let request_id = id.clone();
        let task = self.tokio.spawn(async move {
            find_retained_prompt(&client, &request_id, top_offset, next).await
        });
        self.qol.busy = true;
        let read_owner = Arc::new(());
        self.qol.read_owner = Some(Arc::clone(&read_owner));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                if this.selected_id().as_ref() != Some(&id)
                    || this
                        .qol
                        .read_owner
                        .as_ref()
                        .is_none_or(|owner| !Arc::ptr_eq(owner, &read_owner))
                {
                    return;
                }
                this.qol.busy = false;
                this.qol.read_owner = None;
                if this.residents.get(&id).is_none_or(|resident| {
                    resident.attachment_generation != generation
                        || resident.element.view_offset() != top_offset
                }) {
                    this.show_terminal_feedback(t("terminal.history.terminal_changed"), window, cx);
                    return;
                }
                match result {
                    Ok(Ok(PromptFound {
                        target: Some(target),
                        live_start,
                        total,
                        sequence,
                    })) => {
                        if let Some(resident) = this.residents.get(&id) {
                            let rows = usize::from(resident.element.grid_rows());
                            resident
                                .element
                                .adopt_history_geometry(live_start, total, sequence, rows);
                            resident.element.scroll_to_absolute(target, 0.0, rows);
                            this.pump_scrollback_fetch(&id, rows);
                        }
                        this.qol.feedback = None;
                    }
                    Ok(Ok(PromptFound { target: None, .. })) => {
                        this.prompt_missed(&id, miss, window, cx);
                    }
                    Ok(Err(message)) => this.show_terminal_feedback(message, window, cx),
                    Err(_) => {
                        this.show_terminal_feedback(t("terminal.history.read_failed"), window, cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// What a prompt jump that found no prompt does; see [`PromptMiss`].
    fn prompt_missed(
        &mut self,
        id: &SessionId,
        miss: PromptMiss,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match miss {
            PromptMiss::NeedsMarks => {
                self.show_terminal_feedback(t("terminal.prompt.none"), window, cx);
            }
            PromptMiss::Say(message) => self.show_terminal_feedback(message, window, cx),
            PromptMiss::Live => {
                if let Some(resident) = self.residents.get(id) {
                    let rows = usize::from(resident.element.grid_rows());
                    resident.element.scroll_to_live(rows);
                }
                self.show_terminal_feedback(t("terminal.back_to_latest"), window, cx);
            }
        }
    }

    /// Writes every retained row to a text file and opens it.
    fn export_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.reset_qol_session(&id);
        if self.qol.busy {
            return;
        }
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let top_offset = resident.element.view_offset();
        let generation = resident.attachment_generation;
        let client = Arc::clone(self.runtime.client());
        let request_id = id.clone();
        let task = self.tokio.spawn(async move {
            let mut text = String::new();
            read_retained_rows(&client, &request_id, |_, cells, metadata| {
                append_export_row(&mut text, cells, metadata);
                if text.len() > 16 * 1024 * 1024 {
                    return Err(t("terminal.history.too_large"));
                }
                Ok(())
            })
            .await?;
            Ok::<_, &'static str>(text)
        });
        self.qol.busy = true;
        self.show_terminal_feedback(t("terminal.history.reading"), window, cx);
        let read_owner = Arc::new(());
        self.qol.read_owner = Some(Arc::clone(&read_owner));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                if this.selected_id().as_ref() != Some(&id)
                    || this
                        .qol
                        .read_owner
                        .as_ref()
                        .is_none_or(|owner| !Arc::ptr_eq(owner, &read_owner))
                {
                    return;
                }
                this.qol.busy = false;
                this.qol.read_owner = None;
                if this.residents.get(&id).is_none_or(|resident| {
                    resident.attachment_generation != generation
                        || resident.element.view_offset() != top_offset
                }) {
                    this.show_terminal_feedback(t("terminal.history.terminal_changed"), window, cx);
                    return;
                }
                match result {
                    Ok(Ok(text)) => {
                        let saved = (|| -> std::io::Result<tempfile::NamedTempFile> {
                            let mut file = tempfile::Builder::new()
                                .prefix("diri-scrollback-")
                                .suffix(".txt")
                                .tempfile()?;
                            file.write_all(text.as_bytes())?;
                            file.flush()?;
                            Ok(file)
                        })();
                        match saved {
                            Ok(file) => {
                                if let Ok(url) = url::Url::from_file_path(file.path()) {
                                    cx.open_url(url.as_str());
                                }
                                this.qol.export_files.push(file);
                                this.show_terminal_feedback(
                                    t("terminal.history.opened"),
                                    window,
                                    cx,
                                );
                            }
                            Err(_) => this.show_terminal_feedback(
                                t("terminal.history.save_failed"),
                                window,
                                cx,
                            ),
                        }
                    }
                    Ok(Err(message)) => this.show_terminal_feedback(message, window, cx),
                    Err(_) => {
                        this.show_terminal_feedback(t("terminal.history.read_failed"), window, cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_qol(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let pane = cx.weak_entity();
        let mut overlay = div().absolute().inset_0().child(
            gpui::canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let pane = pane.clone();
                    window.on_mouse_event(
                        move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Capture
                                && !bounds.contains(&event.position)
                            {
                                let _ = pane.update(cx, |this, cx| {
                                    let owns_drag = this
                                        .selected_id()
                                        .and_then(|id| this.residents.get(&id))
                                        .is_some_and(|resident| {
                                            matches!(
                                                resident.pointer_owner,
                                                Some((
                                                    MouseButton::Left,
                                                    PointerOwner::LocalSelection
                                                        | PointerOwner::LocalReference
                                                ))
                                            )
                                        });
                                    if owns_drag && event.pressed_button == Some(MouseButton::Left)
                                    {
                                        this.qol.pressed = None;
                                        this.handle_pointer_move(event, window, cx);
                                    }
                                });
                            }
                        },
                    );
                },
            )
            .absolute()
            .size_full(),
        );
        if self.qol.copy_mode.is_some() {
            overlay = overlay.child(
                div()
                    .absolute()
                    .top(px(8.0))
                    .right(px(14.0))
                    .px(px(8.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .bg(colors.floating_surface())
                    .text_size(px(11.0))
                    .text_color(colors.primary)
                    .child(t("terminal.copy_mode.hint")),
            );
        }
        if let Some(menu) = &self.qol.menu {
            let viewport = self.viewport.unwrap_or_default();
            let x = (f32::from(menu.position.x) - viewport.x)
                .clamp(8.0, (viewport.width - 258.0).max(8.0));
            let menu_height = menu.actions.len() as f32 * 29.0 + 12.0;
            let header_height = self.header_height();
            let y = (f32::from(menu.position.y) - viewport.y - header_height).clamp(
                4.0,
                (viewport.height - header_height - menu_height).max(4.0),
            );
            let mut items = div()
                .id("terminal-context-menu")
                .debug_selector(|| "terminal-context-menu".into())
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(250.0))
                .p(px(6.0))
                .rounded(px(10.0))
                .bg(colors.floating_surface())
                .border_1()
                .border_color(colors.floating_stroke())
                .shadow_md()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.qol.menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
            for (index, &action) in menu.actions.iter().enumerate() {
                let target = menu.target.clone();
                items = items.child(
                    div()
                        .id(("terminal-menu-action", index))
                        .h(px(29.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(5.0))
                        .text_size(px(12.0))
                        .text_color(colors.primary)
                        .when(index == menu.selected, |item| {
                            item.bg(colors.primary.alpha(0.08))
                        })
                        .cursor_pointer()
                        .hover(move |style| style.bg(colors.primary.alpha(0.08)))
                        .child(action.label(menu.editor))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_menu_action(action, target.clone(), window, cx)
                        })),
                );
            }
            overlay = overlay.child(items);
        }
        // In the running app the review is the system alert raised by
        // `sync_paste_prompt`; this panel is what tests and previews inspect.
        if let Some(paste) = self
            .qol
            .paste
            .as_ref()
            .filter(|_| !crate::alerts::enabled(cx))
        {
            let message = paste_review_message(&paste.text);
            // Keep layout work bounded, and make any omitted content explicit.
            let mut chars = paste.text.chars();
            let preview = paste_review_preview(&mut chars, paste.secret);
            let truncated = !paste.secret && chars.next().is_some();
            let viewport = self.viewport.unwrap_or_default();
            let panel_width = (viewport.width - 40.0).clamp(0.0, 480.0);
            let preview_height = (viewport.height - 300.0).clamp(40.0, 200.0);
            let panel = div()
                .id("terminal-paste-review")
                .debug_selector(|| "terminal-paste-review".into())
                .w(px(panel_width))
                .max_w_full()
                .text_color(colors.primary)
                .flex()
                .flex_col()
                .child(
                    div()
                        .p(px(24.0))
                        .flex()
                        .flex_col()
                        .gap(px(16.0))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .text_size(px(Typo::DISPLAY_TITLE.size))
                                        .font_weight(Typo::DISPLAY_TITLE.weight)
                                        .child(paste_review_title()),
                                )
                                .child(
                                    div()
                                        .text_size(px(Typo::ROW.size))
                                        .line_height(px(20.0))
                                        .text_color(colors.secondary)
                                        .child(message),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.secondary)
                                        .child(if truncated {
                                            t("terminal.paste.preview_truncated")
                                        } else {
                                            t("terminal.paste.preview")
                                        }),
                                )
                                .child(
                                    div()
                                        .id("terminal-paste-preview")
                                        .max_h(px(preview_height))
                                        .overflow_y_scroll()
                                        .p(px(12.0))
                                        .rounded(px(Radius::ROW))
                                        .bg(colors.primary.alpha(0.04))
                                        .border_1()
                                        .border_color(colors.floating_stroke())
                                        .font_family(crate::fonts::mono_family())
                                        .text_size(px(Typo::ROW.size))
                                        .line_height(px(20.0))
                                        .child(preview),
                                ),
                        ),
                )
                .child(
                    div()
                        .px(px(24.0))
                        .py(px(16.0))
                        .border_t_1()
                        .border_color(colors.floating_stroke())
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("cancel-terminal-paste")
                                .role(Role::Button)
                                .border_1()
                                .border_color(if paste.cancel_selected {
                                    colors.secondary
                                } else {
                                    colors.primary.alpha(0.0)
                                })
                                .h(px(34.0))
                                .px(px(12.0))
                                .rounded(px(Radius::ROW))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .cursor_pointer()
                                .text_size(px(Typo::ROW.size))
                                .text_color(colors.primary)
                                .hover(move |style| style.bg(colors.primary.alpha(0.06)))
                                .active(move |style| style.bg(colors.primary.alpha(0.1)))
                                .child(t("terminal.paste.cancel"))
                                .child(
                                    div()
                                        .text_size(px(Typo::META.size))
                                        .text_color(colors.secondary)
                                        .child("Esc"),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.qol.paste = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("confirm-terminal-paste")
                                .role(Role::Button)
                                .h(px(34.0))
                                .px(px(14.0))
                                .rounded(px(Radius::ROW))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .cursor_pointer()
                                .text_size(px(Typo::ROW_EMPHASIZED.size))
                                .font_weight(Typo::ROW_EMPHASIZED.weight)
                                .bg(colors.primary)
                                .text_color(colors.background)
                                .hover(move |style| style.bg(colors.primary.alpha(0.88)))
                                .active(move |style| style.bg(colors.primary.alpha(0.75)))
                                .child(t("terminal.menu.paste"))
                                .child(
                                    div()
                                        .when(paste.cancel_selected, |hint| hint.invisible())
                                        .child("↵"),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_terminal_paste(window, cx)
                                })),
                        ),
                );
            overlay = overlay.child(
                div()
                    .absolute()
                    .inset_0()
                    .p(px(20.0))
                    .occlude()
                    .bg(gpui::rgba(0x00000038))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(FloatingSurface::new(colors, panel)),
            );
        }
        overlay.into_any_element()
    }
}

fn paste_review_title() -> &'static str {
    t("terminal.paste.title")
}

fn paste_review_message(text: &str) -> &'static str {
    let has_controls = text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'));
    if has_controls {
        t("terminal.paste.controls")
    } else {
        t("terminal.paste.multiline")
    }
}

/// Informative text for the system alert: the warning, then as much of the
/// clipboard as an alert can show without growing past the screen. What is
/// left out is counted, so a long paste never looks like a short one.
pub(super) fn native_paste_review_message(text: &str, secret: bool) -> String {
    const LINES: usize = 8;
    const LINE_CHARS: usize = 72;
    let warning = paste_review_message(text);
    if secret {
        return format!(
            "{warning}\n\n{}",
            paste_review_preview(&mut text.chars(), true)
        );
    }
    let cleaned = paste_review_preview(&mut text.chars(), false);
    let total = text.lines().count();
    let mut shown = String::new();
    for line in cleaned.lines().take(LINES) {
        let mut chars = line.chars();
        shown.extend(chars.by_ref().take(LINE_CHARS));
        if chars.next().is_some() {
            shown.push('…');
        }
        shown.push('\n');
    }
    let hidden = total.saturating_sub(LINES);
    if hidden > 0 {
        shown.push_str(&tf(
            if hidden == 1 {
                "terminal.paste.more_lines_one"
            } else {
                "terminal.paste.more_lines_other"
            },
            &[("count", &hidden)],
        ));
    }
    format!("{warning}\n\n{}", shown.trim_end())
}

/// What the paste review shows of the clipboard. At a password prompt that
/// is only its size: the text is most likely the password itself.
pub(super) fn paste_review_preview(chars: &mut std::str::Chars<'_>, secret: bool) -> String {
    if secret {
        return tf(
            "terminal.paste.secret",
            &[("count", &chars.by_ref().count())],
        );
    }
    chars
        .by_ref()
        .take(4_000)
        .map(|ch| {
            if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

/// Cells a copied command output may hold: the Engine retains 4 MiB of
/// history, so this is never the limit for a single command in practice.
const COMMAND_OUTPUT_MAX_CELLS: usize = 4 * 1024 * 1024;

/// Reads all retained rows through the Engine RPC used for local and remote
/// history alike, in order. A changing sequence aborts instead of mixing
/// revisions. Returns the live start row, the rows read and the sequence.
/// One validated page of retained rows.
struct RowPage {
    first: i64,
    rows: Vec<Vec<GridCell>>,
    metadata: Vec<RowMetadata>,
    total: i64,
    live_start: i64,
    cols: i64,
    sequence: u64,
}

/// Rows a page may hold: worst-case RLE plus base64 must fit one control
/// response, even for wide, densely styled terminal rows.
fn page_limit(cols: i64) -> i64 {
    (diri_proto::FIND_CAPTURE_MAX_CELLS as i64 / cols.max(1)).clamp(1, 1024)
}

/// Reads up to `max_rows` retained rows from `first`, checked against the
/// width and content sequence of earlier pages of the same read.
async fn read_row_page(
    client: &diri_client::DaemonClient,
    id: &SessionId,
    first: i64,
    max_rows: i64,
    earlier: Option<&RowPage>,
) -> Result<RowPage, &'static str> {
    let response = client
        .read_scrollback_cells(id, first, max_rows)
        .await
        .map_err(|_| t("terminal.history.read_failed"))?;
    if earlier.is_some_and(|page| page.sequence != response.content_seq) {
        return Err(t("terminal.history.changed"));
    }
    let count = usize::try_from(response.row_count).map_err(|_| t("terminal.history.invalid"))?;
    if count as i64 > max_rows
        || response.first_row != first
        || response.total_rows > 1_000_000
        || !(1..=4096).contains(&response.cols)
        || earlier.is_some_and(|page| page.cols != response.cols)
        || (!response.metadata.is_empty() && response.metadata.len() != count)
    {
        return Err(t("terminal.history.invalid"));
    }
    let rows = GridRowCodec::decode_rows(&response.payload, count)
        .map_err(|_| t("terminal.history.invalid"))?;
    if rows.iter().any(|row| row.len() as i64 != response.cols) {
        return Err(t("terminal.history.invalid"));
    }
    Ok(RowPage {
        first,
        rows,
        metadata: response.metadata,
        total: response.total_rows,
        live_start: response.live_start_row,
        cols: response.cols,
        sequence: response.content_seq,
    })
}

async fn read_retained_rows(
    client: &diri_client::DaemonClient,
    id: &SessionId,
    mut visit: impl FnMut(i64, &[GridCell], Option<&RowMetadata>) -> Result<(), &'static str>,
) -> Result<(i64, i64, u64), &'static str> {
    let mut first = 0_i64;
    let mut page_rows = 1_i64;
    let mut previous: Option<RowPage> = None;
    loop {
        let page = read_row_page(client, id, first, page_rows, previous.as_ref()).await?;
        page_rows = page_limit(page.cols);
        for (index, row) in page.rows.iter().enumerate() {
            visit(first + index as i64, row, page.metadata.get(index))?;
        }
        let count = page.rows.len() as i64;
        first += count;
        let done = count == 0 || first >= page.total;
        let page = previous.get_or_insert(page);
        if done {
            return Ok((page.live_start, first, page.sequence));
        }
    }
}

/// Where a prompt search over the Engine's retained rows landed.
pub(super) struct PromptFound {
    pub target: Option<i64>,
    pub live_start: i64,
    pub total: i64,
    pub sequence: u64,
}

/// First page of an outward prompt search. Most prompts are a screen or two
/// away, so the search starts small and widens.
const PROMPT_SEARCH_FIRST_PAGE: i64 = 64;

/// The prompt nearest a window whose top row is `top_offset` rows above the
/// live screen, read outward from it page by page. Unlike an export it stops
/// at the first prompt, so it reads as far as the prompt is, not the whole
/// history.
pub(super) async fn find_retained_prompt(
    client: &diri_client::DaemonClient,
    id: &SessionId,
    top_offset: i64,
    next: bool,
) -> Result<PromptFound, &'static str> {
    use diri_term::command_marks::starts_prompt;
    // The live screen's place in the history numbers every other row.
    let probe = read_row_page(client, id, 0, 1, None).await?;
    let top = probe.live_start - top_offset;
    let found = |target| PromptFound {
        target,
        live_start: probe.live_start,
        total: probe.total,
        sequence: probe.sequence,
    };
    let limit = page_limit(probe.cols);
    let mut page_rows = PROMPT_SEARCH_FIRST_PAGE.min(limit);
    if next {
        let mut first = top + 1;
        while first < probe.total {
            let page = read_row_page(client, id, first, page_rows, Some(&probe)).await?;
            if let Some(index) = page.rows.iter().position(|row| starts_prompt(row)) {
                return Ok(found(Some(page.first + index as i64)));
            }
            if page.rows.is_empty() {
                break;
            }
            first += page.rows.len() as i64;
            page_rows = (page_rows * 4).min(limit);
        }
    } else {
        let mut end = top.min(probe.total);
        while end > 0 {
            let first = (end - page_rows).max(0);
            let page = read_row_page(client, id, first, end - first, Some(&probe)).await?;
            if let Some(index) = page.rows.iter().rposition(|row| starts_prompt(row)) {
                return Ok(found(Some(page.first + index as i64)));
            }
            if page.rows.is_empty() {
                break;
            }
            end = first;
            page_rows = (page_rows * 4).min(limit);
        }
    }
    Ok(found(None))
}

/// The newest finished command's output, read backward from the live end
/// page by page (64, then 256, then 1,024 rows, as the prompt search does)
/// and stopped once it is known, instead of reading every retained row.
async fn find_last_command_output(
    client: &diri_client::DaemonClient,
    id: &SessionId,
) -> Result<Option<diri_term::command_marks::OutputRows>, &'static str> {
    let probe = read_row_page(client, id, 0, 1, None).await?;
    let limit = page_limit(probe.cols);
    let mut page_rows = PROMPT_SEARCH_FIRST_PAGE.min(limit);
    let mut newest = NewestCommandOutput::default();
    let mut end = probe.total;
    while end > 0 {
        let first = (end - page_rows).max(0);
        let page = read_row_page(client, id, first, end - first, Some(&probe)).await?;
        if page.rows.is_empty() {
            break;
        }
        end = first;
        let mut metadata = page.metadata.into_iter();
        let rows = page
            .rows
            .into_iter()
            .map(|cells| (cells, metadata.next()))
            .collect::<Vec<_>>();
        if let Some(output) = newest.push_page_above(rows.into_iter()) {
            return Ok(Some(output));
        }
        if newest.held_cells() > COMMAND_OUTPUT_MAX_CELLS {
            return Err(t("terminal.command_output.too_large"));
        }
        page_rows = (page_rows * 4).min(limit);
    }
    Ok(None)
}

/// The text of a command's output rows, without the blank lines a shell
/// leaves before its next prompt.
fn command_output_text(output: &diri_term::command_marks::OutputRows) -> String {
    let mut text = String::new();
    for (cells, metadata) in &output.rows {
        append_export_row(&mut text, cells, metadata.as_ref());
    }
    text.trim_end_matches('\n').to_owned()
}

fn append_export_row(text: &mut String, row: &[GridCell], metadata: Option<&RowMetadata>) {
    let mut line = String::new();
    for (col, cell) in row.iter().enumerate() {
        if cell.scalar == 0 || cell.style.contains(TermStyle::WIDE_SPACER) {
            continue;
        }
        line.push(
            char::from_u32(cell.scalar)
                .filter(|ch| !ch.is_control())
                .unwrap_or(' '),
        );
        if let Some((_, extra)) =
            metadata.and_then(|m| m.graphemes.iter().find(|(x, _)| usize::from(*x) == col))
        {
            line.push_str(extra);
        }
    }
    if row
        .last()
        .is_some_and(|c| c.style.contains(TermStyle::SOFT_WRAP))
    {
        text.push_str(&line);
    } else {
        text.push_str(line.trim_end_matches(' '));
        text.push('\n');
    }
}

/// The directories a relative file reference is read from, most specific
/// first: a shell's live directory, then the launch directory, which is an
/// Agent's worktree. `None` for remote sessions.
fn file_reference_bases(session: &SessionRecord) -> Option<Vec<std::path::PathBuf>> {
    if session.host.is_some() {
        return None;
    }
    let mut bases = Vec::with_capacity(2);
    if let Some(live) = session.terminal_cwd.as_deref() {
        bases.push(std::path::PathBuf::from(live));
    }
    bases.push(std::path::PathBuf::from(&session.cwd));
    Some(bases)
}

#[cfg(test)]
mod native_paste_review_tests {
    use super::native_paste_review_message;

    #[test]
    fn a_short_paste_is_shown_whole_under_the_warning() {
        let message = native_paste_review_message("make build\nmake test", false);
        assert!(message.starts_with("This terminal may run each line"));
        assert!(message.ends_with("make build\nmake test"));
    }

    #[test]
    fn a_long_paste_is_bounded_and_says_what_it_left_out() {
        let text = (1..=30)
            .map(|n| format!("echo {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let message = native_paste_review_message(&text, false);
        assert!(message.contains("echo 8\n"));
        assert!(!message.contains("echo 9"));
        assert!(message.ends_with("… and 22 more lines"));

        let wide = "x".repeat(500);
        let message = native_paste_review_message(&format!("{wide}\nsecond"), false);
        let first = message.lines().nth(2).unwrap();
        assert_eq!(first.chars().count(), 73, "72 characters and an ellipsis");
        assert!(message.ends_with("second"));
    }

    #[test]
    fn control_characters_change_the_warning_and_never_reach_the_alert() {
        let message = native_paste_review_message("ls\u{1b}[2J\nrm -rf x", false);
        assert!(message.starts_with("This text contains control characters"));
        assert!(!message.contains('\u{1b}'));
    }

    #[test]
    fn a_paste_at_a_password_prompt_shows_only_its_size() {
        let message = native_paste_review_message("hunter2\nhunter2", true);
        assert!(!message.contains("hunter2"));
        assert!(message.ends_with("15 characters, hidden while the terminal reads a password."));
    }
}

#[cfg(test)]
mod command_output_tests {
    use super::command_output_text;
    use diri_proto::grid::{GridCell, TermStyle};
    use diri_term::command_marks::LastCommandOutput;

    fn row(text: &str, marks: &[(usize, TermStyle)], width: usize) -> Vec<GridCell> {
        let mut cells = vec![GridCell::BLANK; width];
        for (cell, ch) in cells.iter_mut().zip(text.chars()) {
            cell.scalar = u32::from(ch);
        }
        for &(col, mark) in marks {
            cells[col].style |= mark;
        }
        cells
    }

    #[test]
    fn copied_output_is_the_text_between_the_output_mark_and_the_end() {
        let width = 8;
        let mut wrapped = row("abcdefgh", &[], width);
        wrapped[width - 1].style |= TermStyle::SOFT_WRAP;
        let rows = [
            row("$ ls", &[(0, TermStyle::PROMPT_START)], width),
            // Output starting mid-row, as after `echo -n` or a same-line C.
            row("$ x  out", &[(5, TermStyle::OUTPUT_START)], width),
            wrapped,
            row("ij", &[], width),
            row("", &[], width),
            row(
                "$",
                &[(0, TermStyle::COMMAND_FAILED | TermStyle::PROMPT_START)],
                width,
            ),
        ];
        let mut last = LastCommandOutput::default();
        for cells in &rows {
            last.push_row(cells, None);
        }
        let output = last.finish().expect("a finished output");
        assert_eq!(command_output_text(&output), "out\nabcdefghij");
    }
}
