//! Drives [`RowMotion`] from the store: sessions that arrive grow into the
//! sidebar and sessions that leave collapse out of it.
use super::*;
use crate::sidebar::row_motion::{self, Presence, Slot};

impl Sidebar {
    /// Notes which sessions the sidebar holds. The panel calls this before
    /// it builds, and `RootView` calls it while the panel is not on screen,
    /// so a session that came or went out of sight is not replayed on reveal.
    pub(crate) fn observe_rows(&mut self, cx: &mut Context<Self>) {
        let now = (self.title_clock)();
        let animate = (self.ui.visible || self.peek_open)
            && self.settings_nav.is_none()
            && !cx.reduce_motion();
        let mut store = self.store.write().expect("session store lock poisoned");
        // Hydration is the list arriving, not sessions arriving.
        if !store.has_hydrated_sessions() {
            self.row_motion = Default::default();
            return;
        }
        let projection = store.sidebar_projection();
        drop(store);
        let live = projection
            .projects
            .iter()
            .flat_map(|group| group.active.iter().map(|session| &session.id));
        self.row_motion.observe(live, animate, now);
    }

    /// Lays out one container's rows for this pass; see [`RowMotion::layout`].
    pub(super) fn arrange_rows(
        &mut self,
        container: impl std::hash::Hash,
        rows: &[crate::store::SidebarRow],
    ) -> Option<Vec<Slot<crate::store::SidebarRow>>> {
        self.row_motion.layout(
            row_motion::container(container),
            rows,
            crate::store::SidebarRow::id,
            self.title_now,
        )
    }

    /// A row's slot while it arrives or leaves; the row itself at rest.
    pub(super) fn row_slot(
        &self,
        row: AnyElement,
        presence: Presence,
        ghost: bool,
    ) -> AnyElement {
        row_motion::slot(row, presence, self.session_row_height(), ghost)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};

    use super::super::titles::testing::advance;
    use super::*;

    const PARENT: &str = "preview-codex";
    const ARRIVAL: &str = "preview-arrival";

    fn panel(cx: &mut TestAppContext) -> (Entity<Sidebar>, &mut VisualTestContext) {
        cx.add_window_view(|_, cx| {
            let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
            sidebar.use_manual_title_clock();
            sidebar
        })
    }

    fn spawn(sidebar: &Entity<Sidebar>, id: &str, cx: &mut VisualTestContext) {
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let mut session = (**store
                .sessions()
                .get(&SessionId::new("preview-spawned-review"))
                .unwrap())
            .clone();
            session.id = SessionId::new(id);
            session.title = "Draft the release notes".into();
            session.parent = Some(SessionId::new(PARENT));
            store.upsert_session(session);
            drop(store);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn close(sidebar: &Entity<Sidebar>, id: &str, cx: &mut VisualTestContext) {
        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .store
                .write()
                .unwrap()
                .remove_sessions(vec![SessionId::new(id)]);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn animating(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> bool {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar.row_motion.is_animating(sidebar.title_now)
        })
    }

    fn row_height(sidebar: &Entity<Sidebar>, id: &str, cx: &VisualTestContext) -> Option<f32> {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .row_bounds
                .borrow()
                .get(&SessionId::new(id))
                .map(|bounds| f32::from(bounds.size.height))
        })
    }

    /// One 120 Hz display-link tick: the frame request made by the last
    /// paint is delivered and repaints the sidebar.
    fn display_link_tick(cx: &mut VisualTestContext) {
        advance(Duration::from_nanos(8_333_333));
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
    }

    /// Runs the motion out the way the app does and counts the wakes.
    fn run_out(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> usize {
        let mut wakes = 0;
        while sidebar.read_with(cx, |sidebar, _| sidebar.disclosure_tick) {
            wakes += 1;
            assert!(wakes < 100, "a finite motion must stop asking for frames");
            display_link_tick(cx);
        }
        wakes
    }

    #[gpui::test]
    fn a_spawned_row_grows_in_and_then_stops_asking_for_frames(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        assert!(!animating(&sidebar, cx), "first paint is at rest");
        sidebar.read_with(cx, |sidebar, _| assert!(!sidebar.disclosure_tick));

        spawn(&sidebar, ARRIVAL, cx);
        assert!(animating(&sidebar, cx));
        // The slot starts closed and the tracked bounds follow it.
        advance(Duration::from_millis(16));
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
        let early = row_height(&sidebar, ARRIVAL, cx).unwrap();
        assert!(early < SIDEBAR_NAV_ROW_HEIGHT, "{early}");
        let wakes = run_out(&sidebar, cx);
        // Every 120 Hz frame of the motion (a 16 ms timer managed 10 to 14).
        assert!((20..=28).contains(&wakes), "{wakes}");
        assert!(!animating(&sidebar, cx));
        assert_eq!(
            row_height(&sidebar, ARRIVAL, cx),
            Some(SIDEBAR_NAV_ROW_HEIGHT)
        );

        // Nothing is left to wake an idle sidebar.
        advance(Duration::from_secs(1));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| assert!(!sidebar.disclosure_tick));
    }

    fn row_renders(sidebar: &Entity<Sidebar>, id: &str, cx: &VisualTestContext) -> usize {
        sidebar.read_with(cx, |sidebar, cx| {
            sidebar
                .session_row_views
                .get(&SessionId::new(id))
                .map_or(0, |view| view.read(cx).renders)
        })
    }

    /// Rows are cached views, but one growing in or collapsing out renders
    /// on every frame of its motion. (Its section's other rows re-render too:
    /// the section clip grows with it, and a changed clip misses the cache.)
    #[gpui::test]
    fn moving_rows_render_every_frame_under_caching(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();

        spawn(&sidebar, ARRIVAL, cx);
        let mut heights = Vec::new();
        let mut renders = row_renders(&sidebar, ARRIVAL, cx);
        while animating(&sidebar, cx) {
            display_link_tick(cx);
            let now = row_renders(&sidebar, ARRIVAL, cx);
            assert!(now > renders, "the arriving row rendered this frame");
            renders = now;
            heights.push(row_height(&sidebar, ARRIVAL, cx).unwrap());
            assert!(heights.len() < 100);
        }
        assert!(heights.len() >= 10, "{heights:?}");
        assert!(
            heights.windows(2).all(|pair| pair[0] <= pair[1]),
            "{heights:?}"
        );
        assert!(heights[0] < SIDEBAR_NAV_ROW_HEIGHT);
        assert_eq!(heights.last().copied(), Some(SIDEBAR_NAV_ROW_HEIGHT));

        // At rest again, the arrival is reused like any other row.
        let settled = row_renders(&sidebar, ARRIVAL, cx);
        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
        cx.run_until_parked();
        assert_eq!(row_renders(&sidebar, ARRIVAL, cx), settled);

        // Leaving: the ghost renders every frame until its slot closes.
        close(&sidebar, ARRIVAL, cx);
        let mut frames = 0;
        let mut renders = row_renders(&sidebar, ARRIVAL, cx);
        while animating(&sidebar, cx) {
            display_link_tick(cx);
            if animating(&sidebar, cx) {
                let now = row_renders(&sidebar, ARRIVAL, cx);
                assert!(now > renders, "the leaving row rendered this frame");
                renders = now;
            }
            frames += 1;
            assert!(frames < 100);
        }
        assert!(frames >= 10, "{frames}");
        assert!(
            sidebar.read_with(cx, |sidebar, _| !sidebar
                .session_row_views
                .contains_key(&SessionId::new(ARRIVAL))),
            "a departed row drops its view once it is gone"
        );
    }

    #[gpui::test]
    fn a_closed_row_leaves_a_ghost_that_takes_no_clicks(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        let bounds = sidebar.read_with(cx, |sidebar, _| {
            sidebar.row_bounds.borrow()[&SessionId::new("preview-shell")]
        });
        close(&sidebar, "preview-shell", cx);
        assert!(animating(&sidebar, cx));
        // The ghost still paints where the row was; clicking it selects
        // nothing and does not reach a row underneath.
        let before = sidebar.read_with(cx, |sidebar, _| {
            sidebar.store.read().unwrap().selected_session_id().cloned()
        });
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let after = sidebar.read_with(cx, |sidebar, _| {
            sidebar.store.read().unwrap().selected_session_id().cloned()
        });
        assert_eq!(before, after);
        run_out(&sidebar, cx);
        assert!(!animating(&sidebar, cx));
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.row_motion.is_idle_for_test());
        });
    }

    #[gpui::test]
    fn reduce_motion_and_a_hidden_panel_never_animate_rows(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        spawn(&sidebar, ARRIVAL, cx);
        assert!(!animating(&sidebar, cx));
        sidebar.read_with(cx, |sidebar, _| assert!(!sidebar.disclosure_tick));

        cx.update(|_, cx| cx.set_reduce_motion(false));
        // Closed while the panel is hidden: `RootView` observes it, and the
        // row is simply gone when the panel comes back.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.ui.visible = false;
            sidebar
                .store
                .write()
                .unwrap()
                .remove_sessions(vec![SessionId::new(ARRIVAL)]);
            sidebar.observe_rows(cx);
            sidebar.ui.visible = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!animating(&sidebar, cx));
    }

    #[gpui::test]
    fn closing_a_whole_project_cuts(cx: &mut TestAppContext) {
        let (sidebar, cx) = panel(cx);
        cx.run_until_parked();
        let ids: Vec<_> = sidebar.read_with(cx, |sidebar, _| {
            let mut store = sidebar.store.write().unwrap();
            store
                .sidebar_projection()
                .projects
                .iter()
                .find(|group| group.project.id == ProjectId::new("preview-dirijor"))
                .unwrap()
                .active
                .iter()
                .map(|session| session.id.clone())
                .collect()
        });
        assert!(ids.len() > row_motion::BULK);
        sidebar.update(cx, |sidebar, cx| {
            sidebar.store.write().unwrap().remove_sessions(ids);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!animating(&sidebar, cx));
    }
}
