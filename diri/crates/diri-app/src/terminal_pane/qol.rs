//! Client-owned terminal interactions. No PTY parsing or remote execution here.
use super::*;
use diri_proto::grid::{GridCell, GridRowCodec, RowMetadata, TermStyle};
use diri_term::element::ReferenceHit;
use gpui::{Pixels, Point};
use std::io::Write;

#[derive(Default)]
pub(super) struct QolState {
    session: Option<SessionId>,
    pub hover: Option<(usize, usize)>,
    hover_key: Option<(usize, usize, u64, i64, Option<u64>)>,
    pub hit: Option<ReferenceHit>,
    pub pressed: Option<(ReferenceHit, (usize, usize))>,
    pub menu: Option<TerminalMenu>,
    pub copy_mode: Option<CopyMode>,
    pub paste: Option<PendingPaste>,
    /// Last toast message, retained briefly to suppress repeated rejections.
    pub feedback: Option<String>,
    pub(super) feedback_generation: u64,
    feedback_timer: Option<Task<()>>,
    pub drag: Option<(SessionId, usize, usize, i64)>,
    pub autoscroll: Option<Task<()>>,
    export_files: Vec<tempfile::NamedTempFile>,
    busy: bool,
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

pub(super) struct PendingPaste {
    pub text: String,
    id: SessionId,
    generation: AttachmentGeneration,
    bracketed: bool,
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
}

#[derive(Clone, Copy, Debug)]
enum MenuAction {
    Open,
    CopyLink,
    Copy,
    Paste,
    Find,
    CopyMode,
    Export,
    PreviousPrompt,
    NextPrompt,
}

impl MenuAction {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "Open link",
            Self::CopyLink => "Copy link",
            Self::Copy => "Copy selection",
            Self::Paste => "Paste",
            Self::Find => "Find selection",
            Self::CopyMode => "Keyboard copy mode",
            Self::Export => "Open scrollback in editor",
            Self::PreviousPrompt => "Previous shell prompt",
            Self::NextPrompt => "Next shell prompt",
        }
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
            self.qol.hit = resident.element.reference_hit_at(col, row);
        }
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
                if let Some(session) = self.selected_session() {
                    if session.host.is_none() {
                        match crate::code_intelligence::local_reference_url(
                            std::path::Path::new(&session.cwd),
                            &reference,
                        ) {
                            Some(url) => cx.open_url(url.as_str()),
                            None => self.show_terminal_feedback(
                                "Could not open this local file link",
                                window,
                                cx,
                            ),
                        }
                    } else {
                        cx.emit(TerminalPaneEvent::OpenFileReference {
                            reference,
                            cwd: session.cwd.clone(),
                            session_id: session.id.clone(),
                        });
                    }
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
            let _ = this.update_in(cx, |this, _, cx| {
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
        let mut actions = Vec::new();
        if target.is_some() {
            actions.extend([MenuAction::Open, MenuAction::CopyLink]);
        }
        if !resident.element.selected_text().is_empty() {
            actions.extend([MenuAction::Copy, MenuAction::Find]);
        }
        actions.extend([
            MenuAction::Paste,
            MenuAction::CopyMode,
            MenuAction::Export,
            MenuAction::PreviousPrompt,
            MenuAction::NextPrompt,
        ]);
        self.qol.menu = Some(TerminalMenu {
            position,
            target,
            selected: 0,
            actions,
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
            MenuAction::Open => {
                if let Some(hit) = target {
                    self.open_reference(hit.reference, window, cx);
                }
            }
            MenuAction::CopyLink => {
                if let Some(hit) = target {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        hit.reference.destination().to_owned(),
                    ));
                    self.show_terminal_feedback("Link copied", window, cx);
                }
            }
            MenuAction::Copy => self.copy_selection(&CopySelection, window, cx),
            MenuAction::Paste => self.paste(&Paste, window, cx),
            MenuAction::Find => self.find_selection(window, cx),
            MenuAction::CopyMode => self.enter_copy_mode(window, cx),
            MenuAction::Export => self.read_terminal_history(None, window, cx),
            MenuAction::PreviousPrompt => self.read_terminal_history(Some(false), window, cx),
            MenuAction::NextPrompt => self.read_terminal_history(Some(true), window, cx),
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
        self.schedule_find(id, Duration::from_millis(200), window, cx);
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
            self.show_terminal_feedback("Terminal changed. Paste again to review.", window, cx);
        } else {
            resident.send_user_input(terminal_paste(&pending.text, resident.bracketed_paste));
        }
        cx.notify();
    }

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
        let delta = if y < top {
            ((top - y) / 16.0).ceil().clamp(1.0, 12.0) as i64
        } else if y > bottom {
            -((y - bottom) / 16.0).ceil().clamp(1.0, 12.0) as i64
        } else {
            0
        };
        if delta == 0 {
            self.qol.drag = None;
            self.qol.autoscroll = None;
            return;
        }
        self.qol.drag = Some((id, col, row, delta));
        if self.qol.autoscroll.is_some() {
            return;
        }
        self.qol.autoscroll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(32))
                    .await;
                let keep = this
                    .update_in(cx, |this, window, cx| {
                        let Some((id, col, row, delta)) = this.qol.drag.clone() else {
                            return false;
                        };
                        if this.selected_id().as_ref() != Some(&id)
                            || !this.focus.is_focused(window)
                        {
                            this.qol.drag = None;
                            return false;
                        }
                        let Some(resident) = this.residents.get(&id) else {
                            return false;
                        };
                        if resident.pointer_owner
                            != Some((MouseButton::Left, PointerOwner::LocalSelection))
                        {
                            return false;
                        }
                        let rows = usize::from(resident.last_size.1);
                        resident.element.set_view_offset(
                            resident.element.view_offset().saturating_add(delta),
                            rows,
                        );
                        resident.element.drag_selection(col, row);
                        this.pump_scrollback_fetch(&id, rows);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
            let _ = this.update_in(cx, |this, _, _| {
                this.qol.autoscroll = None;
            });
        }));
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
        self.show_terminal_feedback(
            "Copy mode · arrows or h j k l · v select · y copy · Esc exit",
            window,
            cx,
        );
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

    /// Uses the same Engine RPC for local and remote history. A changing
    /// sequence aborts instead of exporting mismatched pages or jumping wrong.
    pub(super) fn read_terminal_history(
        &mut self,
        direction: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.qol.busy {
            return;
        }
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let top_offset = resident.element.view_offset();
        let generation = resident.attachment_generation;
        let client = Arc::clone(self.runtime.client());
        let request_id = id.clone();
        let task = self.tokio.spawn(async move {
            let mut first = 0_i64;
            let mut sequence = None;
            let mut text = String::new();
            let mut prompts = Vec::new();
            let mut live_start = 0;
            loop {
                let response = client
                    .read_scrollback_cells(&request_id, first, 128)
                    .await
                    .map_err(|_| "Could not read terminal history")?;
                if sequence.is_some_and(|seq| seq != response.content_seq) {
                    return Err("Output changed during the read. Try again when it settles.");
                }
                sequence = Some(response.content_seq);
                if first == 0 {
                    live_start = response.live_start_row;
                }
                let count =
                    usize::try_from(response.row_count).map_err(|_| "Invalid history response")?;
                if count > 128 || response.first_row != first || response.total_rows > 1_000_000 {
                    return Err("Invalid history response");
                }
                let rows = GridRowCodec::decode_rows(&response.payload, count)
                    .map_err(|_| "Invalid history response")?;
                for (index, row) in rows.iter().enumerate() {
                    if row
                        .iter()
                        .any(|cell| cell.style.contains(TermStyle::PROMPT_START))
                    {
                        prompts.push(first + index as i64);
                    }
                    if direction.is_none() {
                        append_export_row(&mut text, row, response.metadata.get(index));
                    }
                }
                if text.len() > 16 * 1024 * 1024 {
                    return Err("Retained output is too large to export");
                }
                first += count as i64;
                if count == 0 || first >= response.total_rows {
                    break;
                }
            }
            Ok((
                text,
                prompts,
                live_start,
                first,
                sequence.unwrap_or_default(),
            ))
        });
        self.qol.busy = true;
        self.show_terminal_feedback("Reading retained terminal output…", window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.selected_id().as_ref() != Some(&id) { return; }
                this.qol.busy = false;
                if !this.residents.get(&id).is_some_and(|resident| resident.attachment_generation == generation) {
                    this.show_terminal_feedback("Terminal changed. Try again.", window, cx);
                    return;
                }
                match result {
                    Ok(Ok((text, prompts, live_start, total, sequence))) => {
                        if let Some(next) = direction {
                            let top = live_start - top_offset;
                            let target = if next { prompts.into_iter().find(|row| *row > top) } else { prompts.into_iter().rev().find(|row| *row < top) };
                            if let Some(target) = target {
                                if let Some(resident) = this.residents.get(&id) {
                                    let rows = usize::from(resident.element.grid_rows());
                                    resident.element.adopt_history_geometry(live_start, total, sequence, rows);
                                    resident.element.scroll_to_absolute(target, 0.0, rows);
                                    this.pump_scrollback_fetch(&id, rows);
                                }
                                this.qol.feedback = None;
                            } else { this.show_terminal_feedback("No shell prompt in that direction · requires OSC 133 prompt marks", window, cx); }
                        } else {
                            let saved = (|| -> std::io::Result<tempfile::NamedTempFile> {
                                let mut file = tempfile::Builder::new().prefix("diri-scrollback-").suffix(".txt").tempfile()?;
                                file.write_all(text.as_bytes())?; file.flush()?; Ok(file)
                            })();
                            match saved {
                                Ok(file) => {
                                    if let Ok(url) = url::Url::from_file_path(file.path()) { cx.open_url(url.as_str()); }
                                    this.qol.export_files.push(file);
                                    this.show_terminal_feedback("Opened retained output in your editor", window, cx);
                                }
                                Err(_) => this.show_terminal_feedback("Could not save terminal output", window, cx),
                            }
                        }
                    }
                    Ok(Err(message)) => this.show_terminal_feedback(message, window, cx),
                    Err(_) => this.show_terminal_feedback("Could not read terminal history", window, cx),
                }
                cx.notify();
            });
        }).detach();
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
                    .child("Copy mode · v select · y copy · Esc exit"),
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
                        .child(action.label())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_menu_action(action, target.clone(), window, cx)
                        })),
                );
            }
            overlay = overlay.child(items);
        }
        if let Some(paste) = &self.qol.paste {
            let has_controls = paste
                .text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'));
            let message = if has_controls {
                "This text contains control characters. They’ll be replaced with spaces before pasting."
            } else {
                "This terminal may run each line as a command when you paste."
            };
            // Keep layout work bounded, and make any omitted content explicit.
            let mut chars = paste.text.chars();
            let preview: String = chars
                .by_ref()
                .take(4_000)
                .map(|ch| {
                    if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                        ' '
                    } else {
                        ch
                    }
                })
                .collect();
            let truncated = chars.next().is_some();
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
                                        .child("Paste into terminal?"),
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
                                            "Clipboard preview · first 4,000 characters"
                                        } else {
                                            "Clipboard preview"
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
                                .child("Cancel")
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
                                .child("Paste")
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
