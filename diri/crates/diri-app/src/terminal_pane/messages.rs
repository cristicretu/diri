//! Jumps between the messages a person sent to an Agent (⌘⇧↑ / ⌘⇧↓).
//!
//! A full-screen Agent owns its transcript's scrolling, so a jump sends it
//! the wheel notches a trackpad would (or the PageUp/PageDown it scrolls with)
//! and reads each redrawn screen until the message sits at the top
//! ([`diri_term::messages::Travel`]). The pane keeps showing the screen as it
//! was meanwhile, so the jump appears as one cut. An inline Agent
//! leaves its transcript in the terminal history: a jump reads retained rows
//! from the Engine and scrolls Diri's own view, as the wheel would. Either
//! way the view stays an ordinary terminal: scrolling, typing and selecting
//! work as before, and any of them ends a jump in flight.
use super::*;
use diri_proto::grid::{GridCell, GridRowCodec};
use diri_term::messages::{Gutter, Travel, TravelStep};

/// A jump in flight.
pub(super) struct MessageJump {
    token: u64,
    id: SessionId,
    /// Pressed again while travelling: where to go once this jump arrives.
    queued: Option<bool>,
}

/// Where the last jump arrived. The next jump continues from there while
/// the view is as the jump left it.
pub(super) struct MessageMark {
    id: SessionId,
    full_screen: bool,
    /// A screen row on a full-screen Agent; an absolute row in history.
    pub(super) row: i64,
    interactions: u64,
    /// The arrived screen row as drawn.
    cells: Vec<GridCell>,
    /// The reading view's top row, in history.
    top: Option<i64>,
}

/// How long a jump may take before it gives the view back.
const JUMP_DEADLINE: Duration = Duration::from_secs(8);
/// An Agent that has not redrawn this long after notches did not move:
/// each answers within about 45 ms.
const REDRAW_TIMEOUT: Duration = Duration::from_millis(90);

/// How a full-screen jump ended, and what it needs to mark the result.
struct TravelEnd {
    next: bool,
    /// Whether the Agent's view moved at all.
    moved: bool,
    finished: TravelStep,
    /// The screen it ended on.
    rows: Vec<Vec<GridCell>>,
    /// The row the jump started from, to keep reading from it when nothing
    /// lay that way.
    origin: Option<Vec<GridCell>>,
}

/// What a jump step sends to the Agent.
#[derive(Clone, Copy)]
enum TravelInput {
    Notches { up: bool, ticks: u16 },
    Page { up: bool },
}

fn screen_rows(buffer: &diri_term::buffer::GridBuffer) -> Vec<Vec<GridCell>> {
    (0..usize::from(buffer.rows))
        .filter_map(|row| buffer.row(row).map(<[GridCell]>::to_vec))
        .collect()
}

impl TerminalPane {
    /// The gutter of the Agent on screen; `None` for plain shells and notes,
    /// which jump between prompt marks instead. A shell running a recognised
    /// Agent in its foreground uses that Agent's.
    pub(super) fn message_gutter(&self) -> Option<Gutter> {
        let id = self.selected_id()?;
        let store = self.runtime.store.read().expect("session store");
        Gutter::for_agent(store.sessions().get(&id)?.effective_kind().id())
    }

    pub(super) fn jump_to_message(
        &mut self,
        next: bool,
        gutter: Gutter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.reset_qol_session(&id);
        if let Some(jump) = &mut self.qol.message_jump {
            jump.queued = Some(next);
            return;
        }
        if self.qol.busy {
            return;
        }
        let Some(resident) = self.residents.get(&id) else {
            return;
        };
        let full_screen = resident.element.alt_screen();
        if full_screen && resident.attachment_state != AttachmentState::Live {
            // Only a running Agent can move its own view.
            self.show_terminal_feedback("Reconnecting to the session…", window, cx);
            return;
        }
        // A jump moves the live view; a retained search view would hide it.
        if self.close_find_for_selected() {
            find_input::discard_native(window, cx);
        }
        self.qol.copy_mode = None;
        self.residents[&id].element.pin_keyboard_selection(false);
        if full_screen {
            self.travel_full_screen(id, next, gutter, window, cx);
        } else {
            self.jump_in_history(id, next, gutter, window, cx);
        }
    }

    fn take_mark(
        &mut self,
        id: &SessionId,
        full_screen: bool,
        interactions: u64,
    ) -> Option<MessageMark> {
        self.qol.message_mark.take().filter(|mark| {
            &mark.id == id && mark.full_screen == full_screen && mark.interactions == interactions
        })
    }

    fn begin_jump(&mut self, id: &SessionId) -> u64 {
        self.qol.message_tokens += 1;
        self.qol.message_jump = Some(MessageJump {
            token: self.qol.message_tokens,
            id: id.clone(),
            queued: None,
        });
        self.qol.message_tokens
    }

    /// Whether the jump `token` still owns the view: the same session, and
    /// nobody typed, scrolled or selected since it started.
    fn jump_current(&self, id: &SessionId, token: u64, interactions: u64) -> bool {
        self.selected_id().as_ref() == Some(id)
            && self
                .qol
                .message_jump
                .as_ref()
                .is_some_and(|jump| jump.token == token)
            && self.residents.get(id).is_some_and(|resident| {
                resident.element.interaction_generation() == interactions
                    && resident.attachment_state == AttachmentState::Live
            })
    }

    /// Ends the jump `token`; a press queued while it travelled continues
    /// from where it arrived.
    fn end_jump(&mut self, token: u64, arrived: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(jump) = self.qol.message_jump.take_if(|jump| jump.token == token) else {
            return;
        };
        if let Some(resident) = self.residents.get(&jump.id) {
            resident.element.hold_frame(false);
        }
        cx.notify();
        if arrived
            && let Some(next) = jump.queued
            && let Some(gutter) = self.message_gutter()
        {
            self.jump_to_message(next, gutter, window, cx);
        }
    }

    fn travel_full_screen(
        &mut self,
        id: SessionId,
        next: bool,
        gutter: Gutter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resident = &self.residents[&id];
        if !resident.element.mouse_modes().is_reporting() {
            self.show_terminal_feedback(
                "This agent isn’t accepting scrolling right now",
                window,
                cx,
            );
            return;
        }
        let buffer = resident.element.buffer();
        let interactions = resident.element.interaction_generation();
        let rows = screen_rows(&buffer.read().unwrap());
        let origin = self.take_mark(&id, true, interactions).filter(|mark| {
            usize::try_from(mark.row).is_ok_and(|row| rows.get(row) == Some(&mark.cells))
        });
        let from = origin.as_ref().map(|mark| mark.row as usize);
        let origin = origin.map(|mark| mark.cells);
        let (mut travel, mut step) = Travel::begin(gutter, next, &rows, from);
        let token = self.begin_jump(&id);
        // The Agent's view moves underneath; the screen shows only where it
        // lands.
        self.residents[&id].element.hold_frame(true);
        cx.spawn_in(window, async move |this, cx| {
            let started = Instant::now();
            let notches = travel.notches();
            let mut last_burst: Option<Instant> = None;
            loop {
                let pointer = travel.pointer_row();
                let send = match step {
                    TravelStep::Scroll { up, ticks } => {
                        // Bursts closer than this are sped up unpredictably.
                        if let Some(sent) = last_burst {
                            let wait = notches.interval.saturating_sub(sent.elapsed());
                            if !wait.is_zero() {
                                cx.background_executor().timer(wait).await;
                            }
                        }
                        last_burst = Some(Instant::now());
                        Some(TravelInput::Notches { up, ticks })
                    }
                    TravelStep::Page { up } => Some(TravelInput::Page { up }),
                    TravelStep::Wait => None,
                    finished => {
                        let rows = screen_rows(&buffer.read().unwrap());
                        let moved = travel.moved();
                        let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                            if this.jump_current(&id, token, interactions) {
                                let end = TravelEnd {
                                    next,
                                    moved,
                                    finished,
                                    rows,
                                    origin,
                                };
                                this.finish_travel(&id, end, window, cx);
                            }
                            this.end_jump(
                                token,
                                matches!(finished, TravelStep::Arrived { .. }),
                                window,
                                cx,
                            );
                        });
                        return;
                    }
                };
                if let Some(input) = send {
                    let sent = crate::floating::update_in_owner(&this, cx, |this, _, _| {
                        if !this.jump_current(&id, token, interactions) {
                            return None;
                        }
                        let resident = &this.residents[&id];
                        let before = buffer.read().unwrap().generation();
                        match input {
                            TravelInput::Notches { up, ticks } => resident.attachment.scroll(
                                u8::from(!up),
                                ticks,
                                resident.element.grid_cols() / 2,
                                u16::try_from(pointer).unwrap_or(u16::MAX),
                            ),
                            TravelInput::Page { up } => resident
                                .attachment
                                .navigate(if up { b"\x1b[5~" } else { b"\x1b[6~" }.to_vec()),
                        }
                        Some(before)
                    });
                    let Some(Some(before)) = sent else {
                        let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                            this.end_jump(token, false, window, cx);
                        });
                        return;
                    };
                    Self::await_redraw(&buffer, before, &travel, notches.settle, cx).await;
                } else {
                    cx.background_executor()
                        .timer(notches.settle.max(Duration::from_millis(20)))
                        .await;
                }
                let rows = screen_rows(&buffer.read().unwrap());
                step = if started.elapsed() > JUMP_DEADLINE {
                    TravelStep::Lost
                } else {
                    travel.observe(&rows)
                };
            }
        })
        .detach();
    }

    /// Waits for the Agent to draw what was just sent: until a redraw shows
    /// the whole move predicted, or redraws stop for `settle`, or none comes.
    async fn await_redraw(
        buffer: &diri_term::element::SharedGridBuffer,
        before: u64,
        travel: &Travel,
        settle: Duration,
        cx: &mut gpui::AsyncWindowContext,
    ) {
        let sent = Instant::now();
        let mut seen = before;
        let mut changed = None;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(2))
                .await;
            let generation = buffer.read().unwrap().generation();
            if generation != seen {
                seen = generation;
                changed = Some(Instant::now());
                if travel.shows_predicted_move(&screen_rows(&buffer.read().unwrap())) {
                    return;
                }
            }
            match changed {
                Some(at) if at.elapsed() >= settle => return,
                None if sent.elapsed() >= REDRAW_TIMEOUT => return,
                _ if sent.elapsed() >= REDRAW_TIMEOUT * 3 => return,
                _ => {}
            }
        }
    }

    fn finish_travel(
        &mut self,
        id: &SessionId,
        end: TravelEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let TravelEnd {
            next,
            moved,
            finished,
            rows,
            origin,
        } = end;
        let Some(resident) = self.residents.get(id) else {
            return;
        };
        // Cut to where the jump landed.
        resident.element.hold_frame(false);
        match finished {
            TravelStep::Arrived { start, end } => {
                resident.element.flash_message(start..end);
                self.qol.message_mark = rows.get(start).map(|cells| MessageMark {
                    id: id.clone(),
                    full_screen: true,
                    row: start as i64,
                    interactions: resident.element.interaction_generation(),
                    cells: cells.clone(),
                    top: None,
                });
                self.qol.feedback = None;
            }
            TravelStep::Exhausted => {
                // The message the jump left from is still the reading
                // position, wherever the search left it on screen.
                self.qol.message_mark = origin.and_then(|cells| {
                    let row = rows.iter().skip(1).position(|row| *row == cells)? + 1;
                    Some(MessageMark {
                        id: id.clone(),
                        full_screen: true,
                        row: row as i64,
                        interactions: resident.element.interaction_generation(),
                        cells,
                        top: None,
                    })
                });
                self.show_terminal_feedback(
                    match (next, moved) {
                        (true, true) => "Back to the latest output",
                        (true, false) => "No later messages",
                        (false, _) => "No earlier messages",
                    },
                    window,
                    cx,
                );
            }
            _ => self.show_terminal_feedback("Couldn’t find a message on screen", window, cx),
        }
        cx.notify();
    }

    fn jump_in_history(
        &mut self,
        id: SessionId,
        next: bool,
        gutter: Gutter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resident = &self.residents[&id];
        let interactions = resident.element.interaction_generation();
        let offset = resident.element.view_offset();
        let top = resident.element.reading_top_row();
        let generation = resident.attachment_generation;
        let visible_rows = usize::from(resident.element.grid_rows());
        let live = screen_rows(&resident.element.buffer().read().unwrap());
        let origin = self
            .take_mark(&id, false, interactions)
            .filter(|mark| mark.top == top);
        let from = origin.as_ref().map(|mark| mark.row);
        let client = Arc::clone(self.runtime.client());
        let request_id = id.clone();
        let task = self.tokio.spawn(async move {
            let head = client
                .read_scrollback_cells(&request_id, 0, 0)
                .await
                .map_err(|_| "Couldn’t read the terminal history")?;
            if !(1..=4096).contains(&head.cols) || head.live_start_row < 0 {
                return Err("Invalid history response");
            }
            let live_start = head.live_start_row;
            let reading = from.or(top).unwrap_or(live_start);
            let page_rows = (diri_proto::FIND_CAPTURE_MAX_CELLS as i64 / head.cols).clamp(1, 1024);
            let read = |first: i64, last: i64| {
                let client = Arc::clone(&client);
                let id = request_id.clone();
                async move {
                    let page = client
                        .read_scrollback_cells(&id, first, last - first)
                        .await
                        .map_err(|_| "Couldn’t read the terminal history")?;
                    let count =
                        usize::try_from(page.row_count).map_err(|_| "Invalid history response")?;
                    if page.first_row != first || page.row_count > last - first {
                        return Err("Invalid history response");
                    }
                    GridRowCodec::decode_rows(&page.payload, count)
                        .map_err(|_| "Invalid history response")
                }
            };
            // Messages on the live grid, which the app already holds.
            let on_live = gutter.history_starts(None, &live, live_start, live_start);
            let length = |rows: &[Vec<GridCell>], index: usize| gutter.message_len(rows, index, 64);
            let found = if next {
                let mut found = None;
                let mut first = reading + 1;
                while found.is_none() && first < live_start {
                    let last = (first + page_rows).min(live_start);
                    let context = first.saturating_sub(1).max(0);
                    let rows = read(context, (last + 8).min(live_start)).await?;
                    let skip = usize::try_from(first - context).unwrap_or(0);
                    let above = skip
                        .checked_sub(1)
                        .and_then(|row| rows.get(row))
                        .map(Vec::as_slice);
                    let page = &rows[skip.min(rows.len())..];
                    found = gutter
                        .history_starts(above, page, first, live_start)
                        .into_iter()
                        .find(|row| *row > reading && *row < last)
                        .map(|row| (row, length(page, (row - first) as usize)));
                    first = last;
                }
                found.or_else(|| {
                    on_live
                        .iter()
                        .find(|row| **row > reading)
                        .map(|row| (*row, length(&live, (row - live_start) as usize)))
                })
            } else {
                let mut found = on_live
                    .iter()
                    .rev()
                    .find(|row| **row < reading)
                    .map(|row| (*row, length(&live, (row - live_start) as usize)));
                let mut last = reading.min(live_start);
                while found.is_none() && last > 0 {
                    let first = (last - page_rows).max(0);
                    let context = first.saturating_sub(1).max(0);
                    let rows = read(context, (last + 8).min(live_start).max(last)).await?;
                    let skip = usize::try_from(first - context).unwrap_or(0);
                    let above = skip
                        .checked_sub(1)
                        .and_then(|row| rows.get(row))
                        .map(Vec::as_slice);
                    let page = &rows[skip.min(rows.len())..];
                    found = gutter
                        .history_starts(above, page, first, live_start)
                        .into_iter()
                        .rev()
                        .find(|row| *row < last)
                        .map(|row| (row, length(page, (row - first) as usize)));
                    last = first;
                }
                found
            };
            Ok((found, live_start, head.total_rows, head.content_seq))
        });
        self.qol.busy = true;
        let read_owner = Arc::new(());
        self.qol.read_owner = Some(Arc::clone(&read_owner));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                if this.selected_id().as_ref() != Some(&id)
                    || !this
                        .qol
                        .read_owner
                        .as_ref()
                        .is_some_and(|owner| Arc::ptr_eq(owner, &read_owner))
                {
                    return;
                }
                this.qol.busy = false;
                this.qol.read_owner = None;
                let Some(resident) = this.residents.get(&id) else {
                    return;
                };
                if resident.attachment_generation != generation
                    || resident.element.interaction_generation() != interactions
                    || resident.element.view_offset() != offset
                {
                    // The user took the view while the history was read.
                    return;
                }
                match result {
                    Ok(Ok((None, ..))) if !next || offset == 0 => {
                        // Nothing that way: the view stays, and so does the mark.
                        this.qol.message_mark = origin;
                        this.show_terminal_feedback(
                            if next {
                                "No later messages"
                            } else {
                                "No earlier messages"
                            },
                            window,
                            cx,
                        );
                    }
                    Ok(Ok((found, live_start, total, sequence))) => this.land_in_history(
                        &id,
                        next,
                        found,
                        live_start,
                        total,
                        sequence,
                        visible_rows,
                        window,
                        cx,
                    ),
                    Ok(Err(message)) => this.show_terminal_feedback(message, window, cx),
                    Err(_) => this.show_terminal_feedback(
                        "Couldn’t read the terminal history",
                        window,
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn land_in_history(
        &mut self,
        id: &SessionId,
        next: bool,
        found: Option<(i64, usize)>,
        live_start: i64,
        total: i64,
        sequence: u64,
        visible_rows: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resident) = self.residents.get(id) else {
            return;
        };
        let Some((row, len)) = found else {
            if next && resident.element.view_offset() > 0 {
                resident.element.scroll_to_live(visible_rows);
                self.show_terminal_feedback("Back to the latest output", window, cx);
            } else {
                self.show_terminal_feedback(
                    if next {
                        "No later messages"
                    } else {
                        "No earlier messages"
                    },
                    window,
                    cx,
                );
            }
            return;
        };
        let len = len.max(1) as i64;
        if row >= live_start {
            // On the live grid: follow live output and mark it there.
            resident.element.scroll_to_live(visible_rows);
            let window_row = (row - live_start) as usize;
            resident
                .element
                .flash_message(window_row..window_row + len as usize);
        } else {
            resident
                .element
                .adopt_history_geometry(live_start, total, sequence, visible_rows);
            resident.element.scroll_to_absolute(row, 0.0, visible_rows);
            resident.element.flash_message_rows(row..row + len);
            self.pump_scrollback_fetch(id, visible_rows);
        }
        let resident = &self.residents[id];
        self.qol.message_mark = Some(MessageMark {
            id: id.clone(),
            full_screen: false,
            row,
            interactions: resident.element.interaction_generation(),
            cells: Vec::new(),
            top: resident.element.reading_top_row(),
        });
        self.qol.feedback = None;
    }
}
