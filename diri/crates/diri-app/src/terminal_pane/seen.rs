//! Where the reader stopped looking at each session.
//!
//! Switching away from a session records the absolute row its cursor was on.
//! Coming back draws a hairline above that row when output has moved past it,
//! so ten minutes of agent output has an obvious place to start reading.
//!
//! The pane cannot place the mark from its own state. A deselected session is
//! detached, and even an attached live view is never told how far the grid
//! has scrolled; only a scrollback read reports `live_start_row`. Every
//! transition here is therefore completed by a one-row read, the probe, and
//! this type only decides when one is owed and what its answer means.

use std::collections::HashMap;

use diri_proto::model::SessionId;
use diri_term::element::SeenMarker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mark {
    /// Deselected; waiting for the probe that turns the cursor's grid row
    /// into an absolute one.
    Leaving { cursor_row: u16 },
    /// Deselected with the first unseen row known. `probing` is set once the
    /// session is selected again and its arrival probe is in flight.
    Left { row: i64, probing: bool },
    /// Selected again with output past the mark. `settled` is false while
    /// output is moving a marker that is still inside the live grid: its
    /// `live_start_row` is stale, so it is hidden rather than drawn on the
    /// wrong row, until a probe after the output settles.
    Shown { marker: SeenMarker, settled: bool },
}

#[derive(Default)]
pub(super) struct SeenMarks {
    marks: HashMap<SessionId, Mark>,
}

impl SeenMarks {
    /// The session was deselected with its cursor on `cursor_row`. Returns
    /// whether a probe is owed. A session with no resident grid has nothing
    /// the reader could have been looking at.
    pub fn leave(&mut self, id: &SessionId, cursor_row: Option<u16>) -> bool {
        match cursor_row {
            Some(cursor_row) => {
                self.marks.insert(id.clone(), Mark::Leaving { cursor_row });
                true
            }
            None => {
                self.marks.remove(id);
                false
            }
        }
    }

    /// The selected session painted a current frame. Returns whether a probe
    /// is owed; every later frame asks again, so only the first is answered.
    pub fn arrive(&mut self, id: &SessionId) -> bool {
        match self.marks.get_mut(id) {
            Some(Mark::Left { probing, .. }) if !*probing => {
                *probing = true;
                true
            }
            _ => false,
        }
    }

    /// Output reached the selected session. Returns whether a probe is owed
    /// once the output settles.
    pub fn output(&mut self, id: &SessionId) -> bool {
        match self.marks.get_mut(id) {
            Some(Mark::Shown { marker, settled }) if marker.is_live() => {
                *settled = false;
                true
            }
            _ => false,
        }
    }

    /// A probe answered. `cursor_row` is the session's current cursor row
    /// when it is the selected session, `None` when it is not.
    pub fn probed(&mut self, id: &SessionId, live_start_row: i64, cursor_row: Option<u16>) {
        let Some(mark) = self.marks.get(id).copied() else {
            return;
        };
        let row = match mark {
            Mark::Leaving { cursor_row } => live_start_row + i64::from(cursor_row),
            Mark::Left { row, .. } => row,
            Mark::Shown { marker, .. } => marker.row,
        };
        let next = match cursor_row {
            // Came back before the leaving probe answered, or never left.
            Some(cursor_row) if live_start_row + i64::from(cursor_row) > row => Some(Mark::Shown {
                marker: SeenMarker {
                    row,
                    live_start_row,
                },
                settled: true,
            }),
            // Selected with nothing new past the mark.
            Some(_) => None,
            None => Some(Mark::Left {
                row,
                probing: false,
            }),
        };
        match next {
            Some(next) => self.marks.insert(id.clone(), next),
            None => self.marks.remove(id),
        };
    }

    /// A column change renumbered the session's rows; the mark no longer
    /// points at anything.
    pub fn forget(&mut self, id: &SessionId) {
        self.marks.remove(id);
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&SessionId) -> bool) {
        self.marks.retain(|id, _| keep(id));
    }

    pub fn marker(&self, id: &SessionId) -> Option<SeenMarker> {
        match self.marks.get(id) {
            Some(Mark::Shown {
                marker,
                settled: true,
            }) => Some(*marker),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SessionId {
        SessionId::new("session")
    }

    #[test]
    fn leaving_and_returning_to_new_output_shows_the_line_where_the_cursor_was() {
        let mut marks = SeenMarks::default();
        assert!(marks.leave(&id(), Some(12)));
        assert!(!marks.arrive(&id()), "the leaving probe is still in flight");
        marks.probed(&id(), 400, None);
        assert_eq!(marks.marker(&id()), None, "nothing is drawn while away");

        assert!(marks.arrive(&id()));
        assert!(!marks.arrive(&id()), "one probe per arrival");
        marks.probed(&id(), 950, Some(20));
        assert_eq!(
            marks.marker(&id()),
            Some(SeenMarker {
                row: 412,
                live_start_row: 950
            })
        );
    }

    #[test]
    fn returning_to_a_session_that_printed_nothing_shows_no_line() {
        let mut marks = SeenMarks::default();
        marks.leave(&id(), Some(12));
        marks.probed(&id(), 400, None);
        marks.probed(&id(), 400, Some(12));
        assert_eq!(marks.marker(&id()), None);
        assert!(!marks.arrive(&id()), "the mark is spent");
    }

    #[test]
    fn coming_back_before_the_leaving_probe_answers_still_resolves() {
        let mut marks = SeenMarks::default();
        marks.leave(&id(), Some(5));
        marks.probed(&id(), 100, Some(9));
        assert_eq!(
            marks.marker(&id()),
            Some(SeenMarker {
                row: 105,
                live_start_row: 100
            })
        );
    }

    #[test]
    fn output_hides_a_live_marker_until_a_probe_places_it_again() {
        let mut marks = SeenMarks::default();
        marks.leave(&id(), Some(5));
        marks.probed(&id(), 100, None);
        marks.probed(&id(), 102, Some(20));
        assert!(marks.marker(&id()).is_some());

        assert!(marks.output(&id()), "a settle probe is owed");
        assert_eq!(marks.marker(&id()), None, "never drawn on a stale row");
        marks.probed(&id(), 104, Some(20));
        assert_eq!(
            marks.marker(&id()),
            Some(SeenMarker {
                row: 105,
                live_start_row: 104
            })
        );
    }

    #[test]
    fn a_marker_in_history_ignores_output() {
        let mut marks = SeenMarks::default();
        marks.leave(&id(), Some(5));
        marks.probed(&id(), 100, None);
        marks.probed(&id(), 900, Some(20));
        assert!(!marks.output(&id()));
        assert!(marks.marker(&id()).is_some());
    }

    #[test]
    fn answering_or_leaving_again_replaces_the_mark() {
        let mut marks = SeenMarks::default();
        marks.leave(&id(), Some(5));
        marks.probed(&id(), 100, None);
        marks.probed(&id(), 900, Some(20));
        marks.forget(&id());
        assert_eq!(marks.marker(&id()), None);

        marks.leave(&id(), Some(5));
        marks.probed(&id(), 100, None);
        marks.probed(&id(), 900, Some(20));
        marks.leave(&id(), Some(20));
        marks.probed(&id(), 900, None);
        marks.probed(&id(), 900, Some(20));
        assert_eq!(marks.marker(&id()), None, "everything up to 920 was seen");
    }

    #[test]
    fn a_session_without_a_grid_leaves_no_mark() {
        let mut marks = SeenMarks::default();
        marks.leave(&id(), Some(5));
        assert!(!marks.leave(&id(), None));
        marks.probed(&id(), 100, None);
        assert!(!marks.arrive(&id()));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod workflow_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gpui::{AppContext as _, HeadlessAppContext, px, size};

    use super::super::{AttachmentState, TerminalPane, TerminalViewport};
    use diri_proto::model::SessionId;

    #[test]
    #[ignore = "real local PTYs"]
    fn returning_to_a_session_that_kept_printing_draws_the_line_where_the_reader_left() {
        // Each input line prints that many rows after a pause, so the output
        // arrives while the session is deselected.
        let fixture = crate::workspace_fixture::LiveWorkspace::start_with_script(
            r#"stty -echo; printf 'first\nsecond\n$ '; printf ready > ready; while IFS= read -r n; do sleep 1; seq 1 "$n"; done"#,
        );
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let services = fixture.services.clone();
        let store = services.store.clone();
        let window = cx
            .open_window(size(px(760.0), px(520.0)), {
                let services = services.clone();
                move |window, cx| {
                    cx.new(|cx| {
                        let mut pane = TerminalPane::new(
                            services.store.clone(),
                            services.tokio.clone(),
                            window,
                            cx,
                        );
                        pane.set_viewport(
                            TerminalViewport {
                                x: 0.0,
                                y: 0.0,
                                width: 760.0,
                                height: 520.0,
                            },
                            cx,
                        );
                        pane
                    })
                }
            })
            .unwrap();
        let build = SessionId::new("build");
        let review = SessionId::new("review");

        macro_rules! pane {
            ($f:expr) => {
                cx.update_window(window.into(), |root, window, cx| {
                    root.downcast::<TerminalPane>()
                        .unwrap()
                        .update(cx, |pane, cx| ($f)(pane, window, cx))
                })
                .unwrap()
            };
        }
        macro_rules! select {
            ($id:expr) => {
                store.store.write().unwrap().select($id.clone());
                pane!(|pane: &mut TerminalPane, window, cx| {
                    pane.reconcile_store_change(window, cx)
                });
            };
        }
        macro_rules! wait_for {
            ($what:literal, $f:expr) => {{
                let deadline = Instant::now() + Duration::from_secs(8);
                loop {
                    cx.run_until_parked();
                    if let Some(value) = pane!($f) {
                        break value;
                    }
                    assert!(Instant::now() < deadline, $what);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }};
        }

        select!(build);
        let cursor_row = wait_for!(
            "build attaches and shows its prompt",
            |pane: &mut TerminalPane, _, _| {
                pane.claim_selected_control();
                let resident = pane.residents.get(&build)?;
                (resident.attachment_state == AttachmentState::Live
                    && resident.attachment.is_controller())
                .then(|| pane.cursor_row(&build))
                .flatten()
                .filter(|row| *row == 2)
            }
        );
        pane!(|pane: &mut TerminalPane, _, _| {
            pane.residents[&build].attachment.input(b"200\n".to_vec());
        });

        select!(review);
        std::thread::sleep(Duration::from_millis(1500));
        cx.run_until_parked();
        assert_eq!(
            pane!(|pane: &mut TerminalPane, _, _| pane.seen.marker(&build)),
            None,
            "nothing is drawn for a session that is not shown"
        );

        select!(build);
        let marker = wait_for!(
            "the line appears once build repaints",
            |pane: &mut TerminalPane, _, _| pane.seen.marker(&build)
        );
        assert_eq!(
            marker.row,
            i64::from(cursor_row),
            "the first unseen row is the one the cursor was on"
        );
        assert!(
            !marker.is_live(),
            "200 rows pushed it into history: {marker:?}"
        );

        fixture.verify_process_identity();
    }
}
