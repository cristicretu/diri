//! Cursor blink and glide as a pure function of elapsed time.
//!
//! Nothing here reads a clock or owns a timer: every entry point takes `now`,
//! so the same model drives the live element, the unit tests and the
//! frame-by-frame renders. `CursorMotion::sample` also says when the next
//! frame is worth painting, which is what keeps an idle terminal at zero
//! frames: a resting cursor schedules nothing.

use std::time::{Duration, Instant};

use crate::cursor_focus::{CursorFocus, FocusFrame};

/// The cursor holds solid for this long after the last keystroke, cursor move
/// or output on its row. A cursor that blinks while it is being used is the
/// thing that makes blinking annoying.
pub const BLINK_IDLE_DELAY: Duration = Duration::from_millis(500);
/// Fade down, hold low, fade up, hold high. 1.2 s in total, close to the
/// macOS insertion point's 0.5 s on / 0.5 s off once the fades are counted.
pub const BLINK_FADE: Duration = Duration::from_millis(200);
pub const BLINK_LOW_HOLD: Duration = Duration::from_millis(300);
pub const BLINK_HIGH_HOLD: Duration = Duration::from_millis(500);
pub const BLINK_CYCLE: Duration = Duration::from_millis(200 + 300 + 200 + 500);
/// The cursor never disappears: losing the insertion point for 300 ms out of
/// every cycle makes the eye hunt for it.
pub const BLINK_FLOOR: f32 = 0.28;
/// Blinking is a finite response to going idle, not a loop. After this many
/// cycles the cursor rests solid and the terminal stops painting (GTK does
/// the same with `gtk-cursor-blink-timeout`, 10 s by default).
pub const BLINK_CYCLES: u32 = 10;
/// A fade is opacity on a cell-sized block, so it is repainted at this step
/// instead of the display rate: six paints per fade rather than 24 at 120 Hz,
/// about ten frames a second while blinking.
pub const BLINK_STEP: Duration = Duration::from_millis(33);

pub const GLIDE_DURATION: Duration = Duration::from_millis(80);
/// Horizontal reach of a glide, in cells. Word motions and short edits stay
/// under it; prompt repaints and line wraps do not.
pub const GLIDE_MAX_COLS: u16 = 8;
pub const GLIDE_MAX_ROWS: u16 = 2;
/// Typing at the wrap edge damages two rows. Anything larger is a redraw.
pub const GLIDE_MAX_DAMAGED_ROWS: usize = 2;
/// Output arriving closer together than this is a stream, not an echo: the
/// fastest macOS key repeat is 30 ms apart.
pub const STREAMING_GAP: Duration = Duration::from_millis(20);
/// A move glides only when the user caused it. The window covers the echo
/// round trip of a remote session.
pub const KEYSTROKE_WINDOW: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorCell {
    pub col: u16,
    pub row: u16,
}

/// Everything the glide rule may look at for one cursor move.
#[derive(Clone, Copy, Debug)]
pub struct CursorMove {
    pub previous: CursorCell,
    pub next: CursorCell,
    /// Rows replaced by the update that carried the move.
    pub rows_damaged: usize,
    /// Time between this update and the one before it.
    pub since_previous_output: Option<Duration>,
    pub since_keystroke: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveKind {
    Glide,
    Snap,
}

/// Glide short moves the user caused; snap everything else.
#[must_use]
pub fn glide_decision(cursor_move: CursorMove) -> MoveKind {
    let CursorMove {
        previous,
        next,
        rows_damaged,
        since_previous_output,
        since_keystroke,
    } = cursor_move;
    let cols = previous.col.abs_diff(next.col);
    let rows = previous.row.abs_diff(next.row);
    let short = (cols, rows) != (0, 0) && cols <= GLIDE_MAX_COLS && rows <= GLIDE_MAX_ROWS;
    let small_damage = rows_damaged <= GLIDE_MAX_DAMAGED_ROWS;
    let streaming = since_previous_output.is_some_and(|gap| gap < STREAMING_GAP);
    let user_caused = since_keystroke.is_some_and(|age| age <= KEYSTROKE_WINDOW);
    if short && small_damage && !streaming && user_caused {
        MoveKind::Glide
    } else {
        MoveKind::Snap
    }
}

/// Opacity of the cursor `idle` after the last activity.
#[must_use]
pub fn blink_opacity(idle: Duration) -> f32 {
    let Some(blinking) = idle.checked_sub(BLINK_IDLE_DELAY) else {
        return 1.0;
    };
    if blinking >= BLINK_CYCLE * BLINK_CYCLES {
        return 1.0;
    }
    let phase = Duration::from_nanos((blinking.as_nanos() % BLINK_CYCLE.as_nanos()) as u64);
    let fade_up_at = BLINK_FADE + BLINK_LOW_HOLD;
    let covered = if phase < BLINK_FADE {
        1.0 - ease_in_out(fraction(phase, BLINK_FADE))
    } else if phase < fade_up_at {
        0.0
    } else if phase < fade_up_at + BLINK_FADE {
        ease_in_out(fraction(phase - fade_up_at, BLINK_FADE))
    } else {
        1.0
    };
    BLINK_FLOOR + (1.0 - BLINK_FLOOR) * covered
}

/// How long until the blink next changes what is on screen, or `None` once
/// the cursor has come to rest. A hold wakes one step into the fade that
/// follows it, because the fade's first instant still looks like the hold.
#[must_use]
pub fn blink_next_change(idle: Duration) -> Option<Duration> {
    let Some(blinking) = idle.checked_sub(BLINK_IDLE_DELAY) else {
        return Some(BLINK_IDLE_DELAY - idle + BLINK_STEP);
    };
    // The last cycle ends on its high hold, which already looks like rest.
    let end = BLINK_CYCLE * BLINK_CYCLES - BLINK_HIGH_HOLD;
    if blinking >= end {
        return None;
    }
    let phase = Duration::from_nanos((blinking.as_nanos() % BLINK_CYCLE.as_nanos()) as u64);
    let fade_up_at = BLINK_FADE + BLINK_LOW_HOLD;
    let fade_up_end = fade_up_at + BLINK_FADE;
    Some(if phase < BLINK_FADE {
        fade_step(BLINK_FADE - phase)
    } else if phase < fade_up_at {
        fade_up_at - phase + BLINK_STEP
    } else if phase < fade_up_end {
        fade_step(fade_up_end - phase)
    } else {
        BLINK_CYCLE - phase + BLINK_STEP
    })
}

/// One step, or straight to the end of the fade when less than a step and a
/// half is left, so a fade never ends on a sliver of a step.
fn fade_step(remaining: Duration) -> Duration {
    if remaining < BLINK_STEP * 3 / 2 {
        remaining
    } else {
        BLINK_STEP
    }
}

/// Cubic ease-out: most of the distance is covered in the first third, so a
/// held key keeps the block within a fraction of a cell of the true position.
#[must_use]
pub fn ease_out(t: f32) -> f32 {
    let inverse = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inverse * inverse * inverse
}

/// Smoothstep: a fade has no direction, so it eases at both ends.
#[must_use]
pub fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn fraction(elapsed: Duration, total: Duration) -> f32 {
    (elapsed.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
}

/// When the cursor next needs a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorSchedule {
    /// Nothing is animating. No frame, no timer.
    Rest,
    /// A glide is in flight: position needs the display rate.
    NextFrame,
    /// One wake after this long: a blink step or the end of a hold.
    After(Duration),
}

/// What to draw for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorFrame {
    pub opacity: f32,
    /// Offset from the true cell, in cells. Zero at rest.
    pub offset_cols: f32,
    pub offset_rows: f32,
    pub schedule: CursorSchedule,
}

impl CursorFrame {
    pub const REST: Self = Self {
        opacity: 1.0,
        offset_cols: 0.0,
        offset_rows: 0.0,
        schedule: CursorSchedule::Rest,
    };

    /// True when the frame is exactly the static cursor, so the caller can
    /// keep its unanimated paint path (and its pixels) untouched.
    #[must_use]
    pub fn is_static(self) -> bool {
        self.opacity >= 1.0 && !self.is_gliding()
    }

    #[must_use]
    pub fn is_gliding(self) -> bool {
        self.offset_cols != 0.0 || self.offset_rows != 0.0
    }
}

#[derive(Clone, Copy, Debug)]
struct Glide {
    from_col: f32,
    from_row: f32,
    to: CursorCell,
    started_at: Instant,
}

impl Glide {
    /// Position in cells at `now`, or `None` once it has arrived.
    fn position(self, now: Instant) -> Option<(f32, f32)> {
        let elapsed = now.saturating_duration_since(self.started_at);
        if elapsed >= GLIDE_DURATION {
            return None;
        }
        let eased = ease_out(fraction(elapsed, GLIDE_DURATION));
        let to_col = f32::from(self.to.col);
        let to_row = f32::from(self.to.row);
        Some((
            self.from_col + (to_col - self.from_col) * eased,
            self.from_row + (to_row - self.from_row) * eased,
        ))
    }
}

/// One update's effect on the cursor, as the damage observer sees it.
#[derive(Clone, Copy, Debug)]
pub struct CursorDamage {
    /// Cursor cell before the update, if it was visible.
    pub previous: Option<CursorCell>,
    /// Cursor cell after the update, if it is visible.
    pub next: Option<CursorCell>,
    pub rows_damaged: usize,
    pub touches_cursor_row: bool,
}

/// What a blink wake does when it comes due.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wake {
    /// The cursor on screen no longer matches the model: paint a frame.
    Paint,
    /// Activity moved the blink since the wake was armed. Wait again; a
    /// frame now would repaint the identical solid cursor.
    Sleep(Duration),
    /// At rest, or not painted at all.
    Stop,
}

/// Per-view cursor animation state.
#[derive(Debug, Default)]
pub struct CursorMotion {
    last_activity: Option<Instant>,
    last_keystroke: Option<Instant>,
    last_output: Option<Instant>,
    glide: Option<Glide>,
    /// False until the cursor has been painted, and again after it stops
    /// being painted (blur, hidden, scrollback), so its return starts solid.
    drawn: bool,
}

impl CursorMotion {
    /// A key or committed text went to the program.
    pub fn note_keystroke(&mut self, now: Instant) {
        self.last_keystroke = Some(now);
        self.last_activity = Some(now);
    }

    pub fn note_damage(&mut self, damage: CursorDamage, now: Instant) {
        let since_previous_output = self
            .last_output
            .map(|previous| now.saturating_duration_since(previous));
        self.last_output = Some(now);
        if damage.previous != damage.next || damage.touches_cursor_row {
            self.last_activity = Some(now);
        }
        let (Some(previous), Some(next)) = (damage.previous, damage.next) else {
            self.glide = None;
            return;
        };
        if previous == next {
            return;
        }
        let kind = glide_decision(CursorMove {
            previous,
            next,
            rows_damaged: damage.rows_damaged,
            since_previous_output,
            since_keystroke: self
                .last_keystroke
                .map(|keystroke| now.saturating_duration_since(keystroke)),
        });
        self.glide = match kind {
            MoveKind::Snap => None,
            // Retarget from wherever the block is right now, so a held key is
            // one continuous motion and the block is never further behind
            // than one glide.
            MoveKind::Glide => {
                let (from_col, from_row) = self
                    .glide
                    .and_then(|glide| glide.position(now))
                    .unwrap_or((f32::from(previous.col), f32::from(previous.row)));
                Some(Glide {
                    from_col,
                    from_row,
                    to: next,
                    started_at: now,
                })
            }
        };
    }

    /// Decides a wake against the opacity the last frame painted.
    #[must_use]
    pub fn wake(&self, painted_opacity: f32, now: Instant) -> Wake {
        if !self.drawn {
            return Wake::Stop;
        }
        let idle = self.last_activity.map_or(Duration::MAX, |activity| {
            now.saturating_duration_since(activity)
        });
        if blink_opacity(idle) != painted_opacity {
            return Wake::Paint;
        }
        blink_next_change(idle).map_or(Wake::Stop, Wake::Sleep)
    }

    /// The cursor is not being painted this frame.
    pub fn note_hidden(&mut self) {
        self.drawn = false;
        self.glide = None;
    }

    /// The frame for a cursor painted at `cell`.
    pub fn sample(&mut self, cell: CursorCell, now: Instant, reduce_motion: bool) -> CursorFrame {
        if !std::mem::replace(&mut self.drawn, true) {
            self.last_activity = Some(now);
            self.glide = None;
        }
        if reduce_motion {
            self.glide = None;
            return CursorFrame::REST;
        }
        if let Some(glide) = self.glide {
            match glide.position(now).filter(|_| glide.to == cell) {
                Some((col, row)) => {
                    return CursorFrame {
                        opacity: 1.0,
                        offset_cols: col - f32::from(cell.col),
                        offset_rows: row - f32::from(cell.row),
                        schedule: CursorSchedule::NextFrame,
                    };
                }
                None => self.glide = None,
            }
        }
        let idle = self.last_activity.map_or(Duration::MAX, |activity| {
            now.saturating_duration_since(activity)
        });
        CursorFrame {
            opacity: blink_opacity(idle),
            offset_cols: 0.0,
            offset_rows: 0.0,
            schedule: blink_next_change(idle).map_or(CursorSchedule::Rest, CursorSchedule::After),
        }
    }
}

/// The element's side of the model: the clock, and the one pending wake.
#[derive(Debug)]
pub(crate) struct CursorDriver {
    pub(crate) motion: CursorMotion,
    focus: CursorFocus,
    /// Frame renders and tests step time by hand. With a clock injected the
    /// driver records what it would schedule and schedules nothing.
    clock: Option<Instant>,
    wake_at: Option<Instant>,
    last_schedule: Option<CursorSchedule>,
    /// Opacity of the cursor as last painted.
    painted_opacity: f32,
}

impl Default for CursorDriver {
    fn default() -> Self {
        Self {
            motion: CursorMotion::default(),
            focus: CursorFocus::default(),
            clock: None,
            wake_at: None,
            last_schedule: None,
            painted_opacity: 1.0,
        }
    }
}

impl CursorDriver {
    pub(crate) fn now(&self) -> Instant {
        self.clock.unwrap_or_else(Instant::now)
    }

    pub(crate) fn set_clock(&mut self, clock: Option<Instant>) {
        self.clock = clock;
    }

    #[cfg(test)]
    pub(crate) fn painted_focused(&self) -> Option<bool> {
        self.focus.painted_focused()
    }

    pub(crate) fn last_schedule(&self) -> Option<CursorSchedule> {
        self.last_schedule
    }

    /// Returns true when the cursor on screen is dimmed, so the host repaints
    /// it solid now rather than at the next blink step. A key with no echo
    /// (a password prompt) would otherwise leave it dim.
    pub(crate) fn note_keystroke(&mut self) -> bool {
        let now = self.now();
        self.motion.note_keystroke(now);
        // Claimed here so a burst of keys asks for one repaint, not one each.
        std::mem::replace(&mut self.painted_opacity, 1.0) < 1.0
    }

    pub(crate) fn rest(&mut self) {
        self.painted_opacity = 1.0;
        self.motion.note_hidden();
        self.focus.note_hidden();
        self.last_schedule = Some(CursorSchedule::Rest);
    }

    /// The frame for a cursor in a pane that does or does not hold the
    /// keyboard. Unfocused, the blink and glide model is put to rest, so a
    /// hollow cursor arms no wake and returns solid when focus comes back;
    /// the only frames it asks for are the few that empty the block.
    pub(crate) fn sample_in_pane(
        &mut self,
        cell: CursorCell,
        focused: bool,
        reduce_motion: bool,
    ) -> (CursorFrame, FocusFrame) {
        let focus = self.focus.sample(focused, self.now(), reduce_motion);
        let mut frame = if focused {
            self.sample(cell, reduce_motion)
        } else {
            self.motion.note_hidden();
            self.painted_opacity = 1.0;
            CursorFrame::REST
        };
        if frame.is_gliding() {
            self.focus.settle();
            self.focus.note_painted(frame.opacity);
            return (frame, FocusFrame::FILLED);
        }
        frame.opacity *= focus.fill;
        if focus.morphing {
            frame.schedule = CursorSchedule::NextFrame;
        }
        self.focus.note_painted(frame.opacity);
        self.last_schedule = Some(frame.schedule);
        (frame, focus)
    }

    pub(crate) fn sample(&mut self, cell: CursorCell, reduce_motion: bool) -> CursorFrame {
        let frame = self.motion.sample(cell, self.now(), reduce_motion);
        self.last_schedule = Some(frame.schedule);
        self.painted_opacity = frame.opacity;
        frame
    }

    /// Returns the delay of a wake that still has to be armed. A wake already
    /// pending at or before the deadline serves.
    fn arm(&mut self, schedule: CursorSchedule, now: Instant) -> Option<Duration> {
        let CursorSchedule::After(delay) = schedule else {
            return None;
        };
        if self.clock.is_some() {
            return None;
        }
        let deadline = now + delay;
        if self
            .wake_at
            .is_some_and(|pending| pending > now && pending <= deadline)
        {
            return None;
        }
        self.wake_at = Some(deadline);
        Some(delay)
    }
}

/// Asks for the frame `schedule` calls for. A glide rides the display link.
/// A blink arms one wake that repaints the hosting view. The wake is re-armed
/// only by the frame it causes and `CursorSchedule::Rest` ends the chain, so
/// there is no periodic timer and nothing to cancel. While the user is typing
/// the wake keeps deferring itself instead of painting a cursor that has not
/// changed.
pub(crate) fn request_frame(
    driver: &std::sync::Arc<std::sync::Mutex<CursorDriver>>,
    schedule: CursorSchedule,
    window: &gpui::Window,
    cx: &mut gpui::App,
) {
    let delay = {
        let mut state = driver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = state.now();
        let delay = state.arm(schedule, now);
        if state.clock.is_some() {
            return;
        }
        delay
    };
    if schedule == CursorSchedule::NextFrame {
        window.request_animation_frame();
    }
    let Some(delay) = delay else { return };
    let view = window.current_view();
    let driver = std::sync::Arc::downgrade(driver);
    cx.spawn(async move |cx| {
        let mut delay = delay;
        loop {
            cx.background_executor().timer(delay).await;
            let Some(driver) = driver.upgrade() else {
                return;
            };
            let mut state = driver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = state.now();
            match state.motion.wake(state.painted_opacity, now) {
                Wake::Sleep(again) => {
                    state.wake_at = Some(now + again);
                    delay = again;
                }
                Wake::Paint => {
                    state.wake_at = None;
                    break;
                }
                Wake::Stop => {
                    state.wake_at = None;
                    return;
                }
            }
        }
        cx.update(|cx| cx.notify(view));
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn cell(col: u16, row: u16) -> CursorCell {
        CursorCell { col, row }
    }

    fn typed(previous: CursorCell, next: CursorCell) -> CursorMove {
        CursorMove {
            previous,
            next,
            rows_damaged: 1,
            since_previous_output: Some(Duration::from_millis(120)),
            since_keystroke: Some(Duration::from_millis(4)),
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn short_user_moves_glide() {
        for (previous, next) in [
            (cell(10, 4), cell(11, 4)),
            (cell(10, 4), cell(9, 4)),
            (cell(10, 4), cell(18, 4)),
            (cell(10, 4), cell(10, 5)),
            (cell(10, 4), cell(10, 2)),
            (cell(10, 4), cell(7, 5)),
        ] {
            assert_eq!(glide_decision(typed(previous, next)), MoveKind::Glide);
        }
    }

    #[test]
    fn jumps_redraws_streams_and_program_moves_snap() {
        let glide = typed(cell(10, 4), cell(11, 4));
        let cases = [
            ("no move", typed(cell(10, 4), cell(10, 4))),
            ("prompt after a command", typed(cell(24, 4), cell(2, 5))),
            ("line wrap", typed(cell(79, 4), cell(0, 5))),
            ("nine columns", typed(cell(10, 4), cell(19, 4))),
            ("three rows", typed(cell(10, 4), cell(10, 7))),
            (
                "redraw",
                CursorMove {
                    rows_damaged: 3,
                    ..glide
                },
            ),
            (
                "full snapshot",
                CursorMove {
                    rows_damaged: usize::MAX,
                    ..glide
                },
            ),
            (
                "streaming output",
                CursorMove {
                    since_previous_output: Some(ms(8)),
                    ..glide
                },
            ),
            (
                "the program moved it",
                CursorMove {
                    since_keystroke: Some(ms(900)),
                    ..glide
                },
            ),
            (
                "no keystroke yet",
                CursorMove {
                    since_keystroke: None,
                    ..glide
                },
            ),
        ];
        for (name, cursor_move) in cases {
            assert_eq!(glide_decision(cursor_move), MoveKind::Snap, "{name}");
        }
        // Key repeat is slower than the streaming gap, and the first output
        // of a session has nothing before it.
        for gap in [Some(ms(30)), None] {
            let held = CursorMove {
                since_previous_output: gap,
                ..glide
            };
            assert_eq!(glide_decision(held), MoveKind::Glide);
        }
    }

    #[test]
    fn blink_holds_solid_then_fades_between_full_and_the_floor() {
        assert_eq!(blink_opacity(Duration::ZERO), 1.0);
        assert_eq!(blink_opacity(BLINK_IDLE_DELAY - ms(1)), 1.0);
        let at = |phase: Duration| blink_opacity(BLINK_IDLE_DELAY + phase);
        assert_eq!(at(Duration::ZERO), 1.0);
        let halfway = at(BLINK_FADE / 2);
        assert!((halfway - (1.0 + BLINK_FLOOR) / 2.0).abs() < 1e-4);
        assert_eq!(at(BLINK_FADE), BLINK_FLOOR);
        assert_eq!(at(BLINK_FADE + BLINK_LOW_HOLD - ms(1)), BLINK_FLOOR);
        assert_eq!(at(BLINK_FADE * 2 + BLINK_LOW_HOLD), 1.0);
        assert_eq!(at(BLINK_CYCLE - ms(1)), 1.0);
        // The second cycle repeats the first.
        assert_eq!(at(BLINK_CYCLE + BLINK_FADE), BLINK_FLOOR);
        // Monotonic inside a fade, and never below the floor.
        let mut previous = 1.0;
        for step in 0..=200 {
            let opacity = at(ms(step));
            assert!(opacity <= previous && opacity >= BLINK_FLOOR);
            previous = opacity;
        }
    }

    #[test]
    fn blink_comes_to_rest_solid() {
        let end = BLINK_IDLE_DELAY + BLINK_CYCLE * BLINK_CYCLES;
        assert_eq!(blink_opacity(end), 1.0);
        assert_eq!(blink_opacity(end + Duration::from_secs(3600)), 1.0);
        assert_eq!(blink_next_change(end), None);
        // Rest begins with the last high hold: nothing changes after the
        // final fade up, so nothing is scheduled.
        let last_fade_up_end = end - BLINK_HIGH_HOLD;
        assert_eq!(blink_next_change(last_fade_up_end), None);
        assert_eq!(blink_opacity(last_fade_up_end), 1.0);
        assert_eq!(blink_next_change(last_fade_up_end - ms(1)), Some(ms(1)));
    }

    /// Walks the schedule the way the element does, one wake per frame.
    fn frames_until_rest(
        motion: &mut CursorMotion,
        at: CursorCell,
        start: Instant,
    ) -> Vec<Duration> {
        let mut now = start;
        let mut frames = Vec::new();
        loop {
            let frame = motion.sample(at, now, false);
            frames.push(now - start);
            match frame.schedule {
                CursorSchedule::Rest => return frames,
                CursorSchedule::NextFrame => now += Duration::from_micros(8_333),
                CursorSchedule::After(delay) => {
                    assert!(!delay.is_zero(), "a zero wake would spin");
                    now += delay;
                }
            }
            assert!(frames.len() < 10_000, "the cursor never came to rest");
        }
    }

    #[test]
    fn an_idle_cursor_paints_a_bounded_number_of_frames_then_none() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        let frames = frames_until_rest(&mut motion, cell(0, 0), start);
        let total = BLINK_IDLE_DELAY + BLINK_CYCLE * BLINK_CYCLES - BLINK_HIGH_HOLD;
        assert_eq!(*frames.last().unwrap(), total);
        // Six paints per fade, two fades a cycle, plus the first paint: ten
        // frames a second while blinking, against 120 at the display rate.
        assert_eq!(frames.len() - 1, 12 * BLINK_CYCLES as usize);
        // At rest, every later sample is static and schedules nothing.
        let later = motion.sample(cell(0, 0), start + Duration::from_secs(60), false);
        assert_eq!(later, CursorFrame::REST);
    }

    #[test]
    fn a_hollow_cursor_neither_blinks_nor_wakes() {
        let start = Instant::now();
        let mut driver = CursorDriver::default();
        driver.set_clock(Some(start));
        let (frame, focus) = driver.sample_in_pane(cell(2, 1), false, false);
        assert_eq!((frame.opacity, frame.schedule), (0.0, CursorSchedule::Rest));
        assert!(focus.outlined() && !focus.morphing);
        // Long past the idle delay, in the middle of what would be a blink.
        for idle in [ms(700), ms(1_500), Duration::from_secs(600)] {
            driver.set_clock(Some(start + idle));
            let (frame, _) = driver.sample_in_pane(cell(2, 1), false, false);
            assert_eq!((frame.opacity, frame.schedule), (0.0, CursorSchedule::Rest));
            assert_eq!(driver.last_schedule(), Some(CursorSchedule::Rest));
            assert_eq!(
                driver.motion.wake(driver.painted_opacity, start + idle),
                Wake::Stop
            );
        }
    }

    #[test]
    fn a_blur_mid_blink_ends_the_blink_and_focus_returns_solid() {
        let start = Instant::now();
        let mut driver = CursorDriver::default();
        driver.set_clock(Some(start));
        let _ = driver.sample_in_pane(cell(0, 0), true, false);
        let low = start + BLINK_IDLE_DELAY + BLINK_FADE;
        driver.set_clock(Some(low));
        let (dimmed, _) = driver.sample_in_pane(cell(0, 0), true, false);
        assert_eq!(dimmed.opacity, BLINK_FLOOR);

        // The block empties from the dimmed opacity, at the display rate.
        let (first, focus) = driver.sample_in_pane(cell(0, 0), false, false);
        assert_eq!(first.opacity, BLINK_FLOOR);
        assert_eq!(first.schedule, CursorSchedule::NextFrame);
        assert!(focus.morphing);
        assert_eq!(driver.motion.wake(driver.painted_opacity, low), Wake::Stop);
        let mut frames = 0;
        let mut now = low;
        while driver.last_schedule() == Some(CursorSchedule::NextFrame) {
            now += Duration::from_micros(16_667);
            driver.set_clock(Some(now));
            let _ = driver.sample_in_pane(cell(0, 0), false, false);
            frames += 1;
            assert!(frames < 100, "the block never emptied");
        }
        // 120 ms at 60 Hz, then nothing.
        assert_eq!(frames, 8);
        assert_eq!(driver.last_schedule(), Some(CursorSchedule::Rest));

        // Focus returns: filling, solid underneath, and the blink's idle
        // delay starts over once the block is full.
        let back = now + Duration::from_secs(5);
        driver.set_clock(Some(back));
        let (frame, _) = driver.sample_in_pane(cell(0, 0), true, false);
        assert_eq!(
            (frame.opacity, frame.schedule),
            (0.0, CursorSchedule::NextFrame)
        );
        driver.set_clock(Some(back + crate::cursor_focus::FOCUS_MORPH));
        let (frame, focus) = driver.sample_in_pane(cell(0, 0), true, false);
        assert_eq!(frame.opacity, 1.0);
        assert!(!focus.outlined());
        assert_eq!(
            frame.schedule,
            CursorSchedule::After(BLINK_IDLE_DELAY - crate::cursor_focus::FOCUS_MORPH + BLINK_STEP)
        );
    }

    fn type_one_cell(motion: &mut CursorMotion, from: CursorCell, now: Instant) -> CursorCell {
        let next = cell(from.col + 1, from.row);
        motion.note_keystroke(now);
        motion.note_damage(
            CursorDamage {
                previous: Some(from),
                next: Some(next),
                rows_damaged: 1,
                touches_cursor_row: true,
            },
            now,
        );
        next
    }

    #[test]
    fn a_glide_eases_out_arrives_and_stops_asking_for_frames() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        motion.sample(cell(4, 0), start, false);
        let target = type_one_cell(&mut motion, cell(4, 0), start);

        let first = motion.sample(target, start, false);
        assert_eq!((first.offset_cols, first.offset_rows), (-1.0, 0.0));
        assert_eq!(first.opacity, 1.0);
        assert_eq!(first.schedule, CursorSchedule::NextFrame);
        assert!(!first.is_static());

        // Ease-out: past 85% of the way by half time.
        let halfway = motion.sample(target, start + GLIDE_DURATION / 2, false);
        assert!(halfway.offset_cols > -0.15 && halfway.offset_cols < 0.0);

        let arrived = motion.sample(target, start + GLIDE_DURATION, false);
        assert!(arrived.is_static());
        // Solid after the move: the next wake is the idle delay, not a frame.
        assert_eq!(
            arrived.schedule,
            CursorSchedule::After(BLINK_IDLE_DELAY - GLIDE_DURATION + BLINK_STEP)
        );
    }

    #[test]
    fn a_held_key_retargets_without_falling_behind() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        let mut at = cell(0, 0);
        motion.sample(at, start, false);
        let repeat = ms(33);
        let mut previous_position = 0.0;
        for press in 0..20 {
            let now = start + repeat * press;
            at = type_one_cell(&mut motion, at, now);
            for sub in 0..4 {
                let frame = motion.sample(at, now + ms(8) * sub, false);
                let position = f32::from(at.col) + frame.offset_cols;
                assert!(
                    position >= previous_position,
                    "the block never moves backwards"
                );
                // At the instant of a repeat: the new cell plus the quarter
                // cell left of the previous glide. It closes from there.
                assert!(frame.offset_cols > -1.3, "{}", frame.offset_cols);
                previous_position = position;
            }
        }
        let settled = motion.sample(at, start + repeat * 19 + GLIDE_DURATION, false);
        assert!(settled.is_static());
    }

    #[test]
    fn a_snap_cancels_a_glide_in_flight() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        motion.sample(cell(4, 0), start, false);
        let target = type_one_cell(&mut motion, cell(4, 0), start);
        motion.note_damage(
            CursorDamage {
                previous: Some(target),
                next: Some(cell(0, 9)),
                rows_damaged: 12,
                touches_cursor_row: true,
            },
            start + ms(30),
        );
        assert!(motion.sample(cell(0, 9), start + ms(31), false).is_static());
    }

    #[test]
    fn activity_keeps_the_cursor_solid_and_restarts_the_idle_delay() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        motion.sample(cell(0, 0), start, false);
        let fading = motion.sample(cell(0, 0), start + BLINK_IDLE_DELAY + BLINK_FADE, false);
        assert_eq!(fading.opacity, BLINK_FLOOR);

        let now = start + BLINK_IDLE_DELAY + BLINK_FADE + ms(10);
        motion.note_keystroke(now);
        let frame = motion.sample(cell(0, 0), now, false);
        assert_eq!(frame.opacity, 1.0);
        assert_eq!(
            frame.schedule,
            CursorSchedule::After(BLINK_IDLE_DELAY + BLINK_STEP)
        );

        // Output on the cursor row counts; output elsewhere does not.
        let quiet = CursorDamage {
            previous: Some(cell(0, 0)),
            next: Some(cell(0, 0)),
            rows_damaged: 1,
            touches_cursor_row: false,
        };
        motion.note_damage(quiet, now + ms(400));
        let frame = motion.sample(cell(0, 0), now + ms(400), false);
        assert_eq!(frame.schedule, CursorSchedule::After(ms(100) + BLINK_STEP));
        motion.note_damage(
            CursorDamage {
                touches_cursor_row: true,
                ..quiet
            },
            now + ms(450),
        );
        let frame = motion.sample(cell(0, 0), now + ms(450), false);
        assert_eq!(
            frame.schedule,
            CursorSchedule::After(BLINK_IDLE_DELAY + BLINK_STEP)
        );
    }

    #[test]
    fn a_wake_paints_only_when_the_cursor_on_screen_is_out_of_date() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        assert_eq!(motion.wake(1.0, start), Wake::Stop, "never painted");
        let frame = motion.sample(cell(0, 0), start, false);
        let CursorSchedule::After(first) = frame.schedule else {
            panic!("an idle cursor waits for its first fade step");
        };
        assert_eq!(motion.wake(1.0, start + first), Wake::Paint);

        // Typing in the meantime: the armed wake defers instead of painting.
        motion.note_keystroke(start + ms(400));
        assert_eq!(
            motion.wake(1.0, start + first),
            Wake::Sleep(ms(400) + BLINK_IDLE_DELAY + BLINK_STEP - first)
        );
        // Dimmed on screen and solid in the model is out of date too.
        assert_eq!(motion.wake(BLINK_FLOOR, start + first), Wake::Paint);

        let rest = start + ms(400) + BLINK_IDLE_DELAY + BLINK_CYCLE * BLINK_CYCLES;
        assert_eq!(motion.wake(1.0, rest), Wake::Stop);
        motion.note_hidden();
        assert_eq!(motion.wake(BLINK_FLOOR, start + first), Wake::Stop);
    }

    #[test]
    fn a_cursor_that_returns_starts_solid() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        motion.sample(cell(0, 0), start, false);
        motion.note_hidden();
        let back = start + BLINK_IDLE_DELAY + BLINK_FADE;
        let frame = motion.sample(cell(0, 0), back, false);
        assert_eq!(frame.opacity, 1.0);
        assert_eq!(
            frame.schedule,
            CursorSchedule::After(BLINK_IDLE_DELAY + BLINK_STEP)
        );
    }

    #[test]
    fn reduce_motion_is_fully_static() {
        let start = Instant::now();
        let mut motion = CursorMotion::default();
        motion.sample(cell(4, 0), start, true);
        let target = type_one_cell(&mut motion, cell(4, 0), start);
        for offset in [0, 40, 700, 900, 5_000] {
            let frame = motion.sample(target, start + ms(offset), true);
            assert_eq!(frame, CursorFrame::REST);
        }
    }
}
