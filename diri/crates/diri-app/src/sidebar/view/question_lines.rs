//! A waiting row's question: a second, smaller line under its title, in the
//! same style as the Activity filter's line. The title always stays.
//!
//! A row grows from one line to two when its session starts asking, and
//! shrinks back once answered, over the row-arrival timing and curve, so the
//! rows below slide instead of jumping. A row the list first sees already
//! asking (launch, unfolding a project, scrolling to it after it changed
//! out of sight) lands at full height at once.
//!
//! Cost: one map entry per session the list has walked, and a store read per
//! walked row to ask which pane, if any, is waiting.

use super::activity::ACTIVITY_ROW_HEIGHT;
use crate::sidebar::row_motion::ENTER;
use super::*;

/// A row's question line at one instant.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct QuestionLine {
    /// The question; while the line closes, the one it last showed.
    pub(super) text: String,
    /// How far the line is open, 0–1.
    pub(super) reveal: f32,
}

/// The question lines of the rows the list walks.
#[derive(Default)]
pub(super) struct QuestionLines {
    lines: HashMap<SessionId, Line>,
    /// Counts list walks, so a row skipped by one (folded away, the list
    /// hidden) lands rather than replays a change made out of sight.
    pass: u64,
}

#[derive(Clone, Debug)]
struct Line {
    text: Option<String>,
    shown: bool,
    from: f32,
    started: Instant,
    pass: u64,
}

impl Line {
    fn reveal(&self, now: Instant) -> f32 {
        let target = if self.shown { 1.0 } else { 0.0 };
        let progress = (now.saturating_duration_since(self.started).as_secs_f32()
            / ENTER.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (target - self.from) * Motion::SETTLE.settle(progress)
    }

    fn moving(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < ENTER
    }
}

impl QuestionLines {
    pub(super) fn begin_pass(&mut self) {
        self.pass += 1;
    }

    /// Notes what `id` is asking now (`None`: nothing) and returns its line.
    /// `animate` false lands any change at once.
    pub(super) fn observe(
        &mut self,
        id: &SessionId,
        question: Option<String>,
        animate: bool,
        now: Instant,
    ) -> Option<QuestionLine> {
        let pass = self.pass;
        let shown = question.is_some();
        let line = self.lines.entry(id.clone()).or_insert_with(|| Line {
            text: None,
            shown,
            from: if shown { 1.0 } else { 0.0 },
            started: now - ENTER,
            pass,
        });
        let fresh = line.pass + 1 < pass;
        if line.shown != shown {
            line.from = if animate && !fresh {
                line.reveal(now)
            } else if shown {
                1.0
            } else {
                0.0
            };
            line.started = if animate && !fresh { now } else { now - ENTER };
            line.shown = shown;
        }
        line.pass = pass;
        if question.is_some() {
            line.text = question;
        }
        let reveal = line.reveal(now);
        if reveal <= 0.0 {
            line.text = None;
            return None;
        }
        line.text.clone().map(|text| QuestionLine { text, reveal })
    }

    /// Whether some line is still opening or closing.
    pub(super) fn is_animating(&self, now: Instant) -> bool {
        let pass = self.pass;
        self.lines
            .values()
            .any(|line| line.pass == pass && line.moving(now))
    }

    pub(super) fn retain(&mut self, live: &[SessionId]) {
        if self.lines.len() > live.len() {
            let live: HashSet<&SessionId> = live.iter().collect();
            self.lines.retain(|id, _| live.contains(id));
        }
    }
}

impl Sidebar {
    /// Notes the question `row` is waiting on, if any, and returns its line
    /// and the row's natural height. Called for every row the list walks,
    /// built or not, so rows below an asking row are placed where they are
    /// painted.
    pub(super) fn observe_question(
        &mut self,
        row: &crate::store::SidebarRow,
        reduce_motion: bool,
    ) -> (Option<QuestionLine>, f32) {
        let question = {
            let store = self.store.read().expect("session store lock poisoned");
            row_blocker(&store, &row.session)
        };
        let animate = !reduce_motion && self.activity_marks_painted();
        let line = self
            .question_lines
            .observe(row.id(), question, animate, self.title_now);
        let height = if self.activity_filter {
            ACTIVITY_ROW_HEIGHT
        } else {
            let reveal = line.as_ref().map_or(0.0, |line| line.reveal);
            SIDEBAR_NAV_ROW_HEIGHT + (ACTIVITY_ROW_HEIGHT - SIDEBAR_NAV_ROW_HEIGHT) * reveal
        };
        (line, height)
    }
}

/// The pane that makes a row ask: the row's own session, unless a split
/// pane is the one waiting.
pub(super) fn asking_pane<'a>(
    session: &'a SessionRecord,
    shells: &'a [Arc<SessionRecord>],
) -> &'a SessionRecord {
    std::iter::once(session)
        .chain(shells.iter().map(|shell| &**shell))
        .find(|pane| matches!(status_state(pane, false), StatusState::NeedsInput { .. }))
        .unwrap_or(session)
}

/// The question a row is waiting on (`session_presentation::blocker_line`
/// of its asking pane).
fn row_blocker(store: &crate::store::SessionStore, session: &SessionRecord) -> Option<String> {
    if matches!(status_state(session, false), StatusState::NeedsInput { .. }) {
        return crate::session_presentation::blocker_line(session);
    }
    let (_, shells) = store.pane_group(&session.id)?;
    crate::session_presentation::blocker_line(asking_pane(session, &shells))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(lines: &mut QuestionLines, question: Option<&str>, now: Instant) -> Option<f32> {
        lines.begin_pass();
        lines
            .observe(&SessionId::new("s"), question.map(str::to_owned), true, now)
            .map(|line| line.reveal)
    }

    #[test]
    fn a_question_opens_and_closes_over_the_arrival_time() {
        let mut lines = QuestionLines::default();
        let t0 = Instant::now();
        assert_eq!(line(&mut lines, None, t0), None);
        assert_eq!(line(&mut lines, Some("Deploy?"), t0), None, "starts closed");
        let half = line(&mut lines, Some("Deploy?"), t0 + ENTER / 2).unwrap();
        assert!(half > 0.0 && half < 1.0, "{half}");
        assert!(lines.is_animating(t0 + ENTER / 2));
        assert_eq!(line(&mut lines, Some("Deploy?"), t0 + ENTER), Some(1.0));
        assert!(!lines.is_animating(t0 + ENTER));
        // Answered: the line closes, still reading its question.
        let t1 = t0 + ENTER * 2;
        lines.begin_pass();
        let closing = lines
            .observe(&SessionId::new("s"), None, true, t1 + ENTER / 2)
            .expect("closing");
        assert_eq!(closing.text, "Deploy?");
        assert_eq!(line(&mut lines, None, t1 + ENTER * 2), None);
    }

    #[test]
    fn a_row_first_seen_asking_lands_at_full_height() {
        let mut lines = QuestionLines::default();
        let now = Instant::now();
        assert_eq!(line(&mut lines, Some("Deploy?"), now), Some(1.0));
        assert!(!lines.is_animating(now));
        // Out of sight for a walk, then answered: no replay.
        lines.begin_pass();
        lines.begin_pass();
        assert_eq!(line(&mut lines, None, now), None);
        assert!(!lines.is_animating(now));
    }
}
