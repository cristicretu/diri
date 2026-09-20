//! Drives [`TitleSettles`] from the store and paints settling titles in the
//! rows and the horizontal strip.
use super::*;
use crate::switcher::display_title_str;

impl Sidebar {
    /// Notes every session's current title. Both surfaces call this before
    /// they build, and `RootView` calls it while neither is on screen, so a
    /// title that changed out of sight is never replayed on reveal.
    pub(crate) fn observe_titles(&mut self, cx: &mut Context<Self>) {
        let now = (self.title_clock)();
        self.title_now = now;
        let painted = self.activity_marks_painted() && !cx.reduce_motion();
        let store = self.store.read().expect("store");
        let sessions = store.sessions();
        for session in sessions.values() {
            // The user's own rename answers the keypress; it does not drift in.
            let animate = painted && session.title_source != diri_proto::TitleSource::UserRename;
            self.title_settles
                .observe(&session.id, display_title_str(session), animate, now);
        }
        // Every live session was just observed, so a surplus is the departed.
        if self.title_settles.len() > sessions.len() {
            self.title_settles.retain(|id| sessions.contains_key(id));
        }
    }

    /// Wakes the painted surface for the next frame of a fade, and lets the
    /// wake lapse the moment the last title lands.
    pub(super) fn schedule_title_tick(&mut self, cx: &mut Context<Self>) {
        if !self.title_settles.is_settling(self.title_now) {
            self.title_tick = None;
        } else if self.title_tick.is_none() {
            // A timer rather than an animation frame, like the disclosure:
            // the strip is painted by `RootView`, and a covered window can
            // stop delivering display-link callbacks.
            self.title_tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.title_tick = None;
                    cx.notify();
                });
            }));
        }
    }

    /// The layered label for `id` while its title settles; `None` at rest,
    /// when the caller paints its ordinary label.
    pub(super) fn settling_title(
        &self,
        id: &SessionId,
        available_width: f32,
    ) -> Option<SettlingLabel> {
        Some(SettlingLabel {
            frame: self.title_settles.frame(id, self.title_now)?,
            font: Typo::ROW,
            available_width,
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::Sidebar;

    thread_local! {
        static NOW: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    fn now() -> Instant {
        NOW.get().expect("manual clock installed")
    }

    impl Sidebar {
        /// Samples title fades against a clock the test advances.
        pub(crate) fn use_manual_title_clock(&mut self) {
            NOW.set(Some(Instant::now()));
            self.title_clock = now;
        }
    }

    pub(crate) fn advance(by: Duration) {
        NOW.set(Some(now() + by));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};

    use super::testing::advance;
    use super::*;
    use crate::store::TabOrientation;

    const SESSION: &str = "preview-cursor";

    /// Only the strip paints, as while horizontal tabs hide the panel.
    struct StripOnly(Entity<Sidebar>);
    impl Render for StripOnly {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.update(cx, |sidebar, cx| {
                sidebar.render_horizontal_tabs(900.0, None, cx)
            }))
        }
    }

    fn panel(cx: &mut TestAppContext) -> (Entity<Sidebar>, &mut VisualTestContext) {
        cx.add_window_view(|_, cx| {
            let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
            sidebar.use_manual_title_clock();
            sidebar
        })
    }

    fn strip(cx: &mut TestAppContext) -> (Entity<Sidebar>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar.use_manual_title_clock();
                sidebar
                    .set_tab_orientation(TabOrientation::Horizontal, cx)
                    .unwrap();
                sidebar
            });
            cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
            StripOnly(sidebar)
        });
        (view.read_with(cx, |view, _| view.0.clone()), cx)
    }

    fn retitle(
        sidebar: &Entity<Sidebar>,
        title: &str,
        source: diri_proto::TitleSource,
        cx: &mut VisualTestContext,
    ) {
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let mut session = (**store.sessions().get(&SessionId::new(SESSION)).unwrap()).clone();
            session.title = title.into();
            session.title_source = source;
            store.upsert_session(session);
            drop(store);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn settling(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> bool {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .title_settles
                .frame(&SessionId::new(SESSION), sidebar.title_now)
                .is_some()
        })
    }

    /// Runs the fade to its end the way the app does: each wake repaints,
    /// and a repaint that still owes a frame arms the next wake.
    fn run_out(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> usize {
        let mut wakes = 0;
        while sidebar.read_with(cx, |sidebar, _| sidebar.title_tick.is_some()) {
            wakes += 1;
            assert!(wakes < 100, "a finite fade must stop asking for frames");
            advance(Duration::from_millis(16));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
        }
        wakes
    }

    #[gpui::test]
    fn an_agent_rename_settles_in_the_row_then_stops_asking_for_frames(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.title_tick.is_none(), "first paint is at rest");
        });

        retitle(
            &sidebar,
            "Fix tab focus after close",
            diri_proto::TitleSource::AgentProvided,
            cx,
        );
        assert!(settling(&sidebar, cx));
        let wakes = run_out(&sidebar, cx);
        // 180 ms of 16 ms wakes, and not one more.
        assert_eq!(wakes, 12);
        assert!(!settling(&sidebar, cx));

        // Nothing is left to wake an idle sidebar.
        advance(Duration::from_secs(1));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| assert!(sidebar.title_tick.is_none()));
    }

    #[gpui::test]
    fn the_strip_settles_titles_while_the_panel_is_hidden(cx: &mut TestAppContext) {
        let (sidebar, cx) = strip(cx);
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.is_visible());
            assert!(sidebar.title_tick.is_none());
        });
        retitle(
            &sidebar,
            "Fix tab focus after close",
            diri_proto::TitleSource::TerminalTitle,
            cx,
        );
        assert!(settling(&sidebar, cx));
        assert_eq!(run_out(&sidebar, cx), 12);
        assert!(!settling(&sidebar, cx));
    }

    #[gpui::test]
    fn the_users_own_rename_commits_at_once(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .store
                .write()
                .unwrap()
                .rename(SessionId::new(SESSION), "Mine");
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!settling(&sidebar, cx));
        sidebar.read_with(cx, |sidebar, _| assert!(sidebar.title_tick.is_none()));
    }

    #[gpui::test]
    fn reduce_motion_and_new_sessions_never_fade(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        retitle(
            &sidebar,
            "Renamed under Reduce Motion",
            diri_proto::TitleSource::AgentProvided,
            cx,
        );
        assert!(!settling(&sidebar, cx));
        sidebar.read_with(cx, |sidebar, _| assert!(sidebar.title_tick.is_none()));

        cx.update(|_, cx| cx.set_reduce_motion(false));
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let mut session = (**store.sessions().get(&SessionId::new(SESSION)).unwrap()).clone();
            session.id = SessionId::new("just-spawned");
            session.title = "A session that was not here a frame ago".into();
            store.upsert_session(session);
            drop(store);
            cx.notify();
        });
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.title_tick.is_none());
            assert_eq!(
                sidebar.title_settles.len(),
                sidebar.store.read().unwrap().sessions().len()
            );
        });
    }

    #[gpui::test]
    fn a_title_changed_out_of_sight_is_not_replayed_on_reveal(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        // Vertical tabs with the panel closed: neither surface paints, and
        // `RootView` keeps the titles current.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.ui.visible = false;
            let mut store = sidebar.store.write().unwrap();
            let mut session = (**store.sessions().get(&SessionId::new(SESSION)).unwrap()).clone();
            session.title = "Renamed while hidden".into();
            store.upsert_session(session);
            drop(store);
            sidebar.observe_titles(cx);
            sidebar.ui.visible = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!settling(&sidebar, cx));
    }
}
