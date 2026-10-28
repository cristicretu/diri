//! The sidebar's side of windowed row rendering (see `row_window.rs`):
//! which band of the list this render builds, which rows are always built,
//! and what a skipped run of rows lays out as.

use super::*;

/// The list's bottom padding, which clears the floating filter control.
pub(super) const LIST_BOTTOM_PADDING: f32 = SIDEBAR_NAV_ROW_HEIGHT + 17.0;

impl Sidebar {
    /// Starts this render's walk down the session list.
    pub(super) fn begin_row_window(&mut self) {
        let band = self.row_band();
        self.built_band.set(band);
        self.rows_built_for_motion = band.is_none() && self.windowing;
        self.held_rows = if band.is_some() {
            self.held_rows()
        } else {
            HashSet::new()
        };
        self.row_window = RowWindow::new(band, LIST_TOP_PADDING);
    }

    /// Ends the walk: keeps every row's slot for reveals, and forgets the
    /// painted bounds of rows that were not built, which no longer say where
    /// those rows are.
    pub(super) fn finish_row_window(&mut self) {
        let walk = std::mem::take(&mut self.row_window);
        self.list_content_height = walk.cursor() + SECTION_GAP + LIST_BOTTOM_PADDING;
        let (slots, skipped) = walk.finish();
        {
            let mut bounds = self.row_bounds.borrow_mut();
            for id in &skipped {
                bounds.remove(id);
            }
        }
        *self.row_slots.borrow_mut() = slots;
    }

    /// A render that built every row for a motion that has since settled
    /// renders once more, so the rows it no longer needs leave the scene
    /// instead of being replayed by every frame until the next change.
    pub(super) fn settle_row_window(&mut self, window: &mut Window) {
        if self.rows_built_for_motion && self.rows_mounted && self.row_band().is_some() {
            self.rows_built_for_motion = false;
            Self::refresh_on_next_frame(&self.weak_self, window);
        }
    }

    /// The content-space band rows are built for, or `None` to build every
    /// row. Motion that moves rows off their walked slot builds everything
    /// while it runs: a disclosure clipping a body, sections sliding into new
    /// slots after a reorder, and a project section riding the pointer.
    fn row_band(&self) -> Option<row_window::Span> {
        if !self.windowing
            || self.disclosure_tick
            || self.section_shift.in_flight()
            || self
                .lift
                .as_ref()
                .is_some_and(|lift| matches!(lift.key, LiftKey::Project(_)))
        {
            return None;
        }
        let measured = f32::from(self.list_scroll.bounds().size.height);
        // Before the list has laid out once, the window bounds it.
        let viewport = if measured > 0.0 {
            measured
        } else {
            f32::from(self.main_viewport.height)
        };
        if viewport <= 0.0 {
            return None;
        }
        let scroll_top = -f32::from(self.list_scroll.offset().y);
        Some(row_window::band(
            scroll_top,
            viewport,
            self.list_content_height,
        ))
    }

    /// Rows something is holding are built wherever they are: the selected
    /// and keyboard-focused rows, the hovered row and its preview card, a
    /// rename, a delegation mark, and the rows a drag carries. Collected once
    /// per render; a title still settling is checked per row.
    fn held_rows(&self) -> HashSet<SessionId> {
        let ui = &self.ui;
        let mut held: HashSet<SessionId> = [
            &ui.focus_cursor,
            &ui.hovered_session,
            &ui.hover_card,
            &ui.renaming,
            &ui.delegation_mark,
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
        if let Some(DragItem::Session { id, .. }) = &ui.drag {
            held.insert(id.clone());
        }
        // The window's own navigation: its selection lives behind `write`.
        let store = self.store.write().expect("session store lock poisoned");
        held.extend(store.selected_session_id().cloned());
        if ui.drag.is_some() {
            held.extend(store.sidebar_selection().iter().cloned());
        }
        held
    }

    /// Places one session row on the walk and says whether to build it.
    /// `margin` and `height` are its slot's, presence included. A row in
    /// motion (growing in, collapsing out, or in a body whose disclosure is
    /// running) is always built.
    pub(super) fn place_row(
        &mut self,
        id: Option<&SessionId>,
        margin: f32,
        height: f32,
        moving: bool,
    ) -> bool {
        if !self.row_window.windowed() {
            self.row_window.place(id, margin, height, true);
            return true;
        }
        let keep = moving
            || id.is_some_and(|id| {
                self.held_rows.contains(id) || self.settling_title(id, 0.0).is_some()
            });
        self.row_window.place(id, margin, height, keep)
    }
}

/// Checked as the list prepaints: whether the scroll it painted at shows
/// only content inside the band this render built. A scroll that reached the
/// list without re-rendering the sidebar would otherwise show spacers.
pub(super) fn band_covers_scroll(band: Option<row_window::Span>, scroll: &ScrollHandle) -> bool {
    let Some(band) = band else {
        return true;
    };
    let top = -f32::from(scroll.offset().y);
    let visible = row_window::Span::new(top, top + f32::from(scroll.bounds().size.height));
    band.top <= visible.top && visible.bottom <= band.bottom
}

/// What a skipped run of rows lays out as: nothing but its height.
pub(super) fn skipped_spacer(height: f32) -> AnyElement {
    div().w_full().flex_none().h(px(height)).into_any_element()
}

/// A section body's rows with each skipped run collapsed into one spacer, in
/// `disclosure_body`'s `(element, height, gap)` form (`gap` scales its 2 px).
pub(super) fn body_rows(slots: Vec<Slot<AnyElement>>) -> Vec<(AnyElement, f32, f32)> {
    merge_skipped(slots, skipped_spacer)
        .into_iter()
        .map(|(row, height, margin)| (row, height, margin / 2.0))
        .collect()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use diri_proto::DateMillis;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    struct Panel {
        sidebar: Entity<Sidebar>,
    }

    impl Render for Panel {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.sidebar.clone())
        }
    }

    /// A sidebar over `sessions` sessions spread across `projects` projects
    /// (`bench-0`, `bench-1`, ... in display order within each project),
    /// with every `archive_every`-th session archived when given.
    fn fleet(
        cx: &mut TestAppContext,
        sessions: usize,
        projects: usize,
        archive_every: Option<usize>,
        grouping: SidebarGrouping,
    ) -> (Entity<Sidebar>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                let mut fixture = SidebarPreviewFixture::bench_fleet_across(sessions, 0, projects);
                if let Some(every) = archive_every {
                    for (index, session) in fixture.list.sessions.iter_mut().enumerate() {
                        if index % every == every - 1 {
                            session.archived_at = Some(DateMillis(1_750_000_000_000.0));
                        }
                    }
                }
                {
                    let mut store = sidebar.store.write().unwrap();
                    store.hydrate(fixture.list);
                    let projects: Vec<_> = store
                        .sidebar_projection()
                        .projects
                        .iter()
                        .map(|group| group.project.id.clone())
                        .collect();
                    store
                        .update_preferences(|prefs| {
                            prefs.sidebar_session_order = fixture.prefs.sidebar_session_order;
                            prefs.sidebar_grouping = grouping;
                            prefs.sidebar_recency_archives_expanded = true;
                            prefs.sidebar_expanded_archives = projects.into_iter().collect();
                        })
                        .unwrap();
                    store.reconcile();
                }
                sidebar
            });
            Panel { sidebar }
        });
        let sidebar = view.read_with(cx, |panel, _| panel.sidebar.clone());
        // The first frame lays the list out; the second windows it.
        redraw(&sidebar, cx);
        (sidebar, cx)
    }

    /// Cost of the Activity filter on a 1000-session fleet: the projection
    /// it adds to a render while on, the toggle's dot every render, and a
    /// full sidebar render either way. Prints; run with
    /// `cargo test -p diri-app --release activity_filter_cost -- --ignored --nocapture`.
    #[gpui::test]
    #[ignore]
    fn activity_filter_cost(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 1000, 20, None, SidebarGrouping::Project);
        const RUNS: u32 = 200;
        let time = |cx: &mut VisualTestContext, activity: bool| {
            sidebar.update(cx, |sidebar, _| sidebar.activity_filter = activity);
            let started = std::time::Instant::now();
            let mut listed = 0;
            for _ in 0..RUNS {
                listed = sidebar.update(cx, |sidebar, _| {
                    let mut store = sidebar.store.write().unwrap();
                    sidebar.visible_projection(&mut store).ordered_sessions.len()
                });
            }
            (started.elapsed() / RUNS, listed)
        };
        let (off, all) = time(cx, false);
        let (on, listed) = time(cx, true);
        let started = std::time::Instant::now();
        for _ in 0..RUNS {
            sidebar.update(cx, |sidebar, _| {
                let store = sidebar.store.read().unwrap();
                std::hint::black_box(sidebar.activity_pending(&store));
            });
        }
        let dot = started.elapsed() / RUNS;
        let render = |cx: &mut VisualTestContext, activity: bool| {
            sidebar.update(cx, |sidebar, _| sidebar.activity_filter = activity);
            redraw(&sidebar, cx);
            let started = std::time::Instant::now();
            for _ in 0..20 {
                redraw(&sidebar, cx);
            }
            started.elapsed() / 20
        };
        let render_off = render(cx, false);
        let render_on = render(cx, true);
        eprintln!(
            "activity_filter_cost: projection off {off:?} ({all} rows), on {on:?} ({listed} rows); \
             dot {dot:?}; sidebar render off {render_off:?}, on {render_on:?}"
        );
    }

    fn redraw(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
        sidebar.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }

    fn set_windowing(sidebar: &Entity<Sidebar>, on: bool, cx: &mut VisualTestContext) {
        sidebar.update(cx, |sidebar, _| sidebar.windowing = on);
        redraw(sidebar, cx);
    }

    fn scroll_to(sidebar: &Entity<Sidebar>, y: f32, cx: &mut VisualTestContext) {
        sidebar.update(cx, |sidebar, cx| {
            sidebar.list_scroll.set_offset(point(px(0.0), px(-y)));
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn built(
        sidebar: &Entity<Sidebar>,
        cx: &VisualTestContext,
    ) -> HashMap<SessionId, Bounds<Pixels>> {
        sidebar.read_with(cx, |sidebar, _| sidebar.row_bounds.borrow().clone())
    }

    fn slots(
        sidebar: &Entity<Sidebar>,
        cx: &VisualTestContext,
    ) -> HashMap<SessionId, row_window::Span> {
        sidebar.read_with(cx, |sidebar, _| sidebar.row_slots.borrow().clone())
    }

    /// (scroll container's window-space top, scroll offset, viewport height)
    fn scroll(sidebar: &Entity<Sidebar>, cx: &VisualTestContext) -> (f32, f32, f32) {
        sidebar.read_with(cx, |sidebar, _| {
            let bounds = sidebar.list_scroll.bounds();
            (
                f32::from(bounds.top()),
                f32::from(sidebar.list_scroll.offset().y),
                f32::from(bounds.size.height),
            )
        })
    }

    fn visible_order(sidebar: &Entity<Sidebar>, cx: &VisualTestContext) -> Vec<SessionId> {
        sidebar
            .read_with(cx, |sidebar, _| sidebar.focus_rows_snapshot().0)
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    /// The walk's slot for every row, placed in the window, against where
    /// the row actually painted. Built with windowing off, so every row
    /// paints.
    fn assert_slots_match_layout(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
        set_windowing(sidebar, false, cx);
        let (top, offset, _) = scroll(sidebar, cx);
        let slots = slots(sidebar, cx);
        let painted = built(sidebar, cx);
        assert!(!slots.is_empty());
        assert_eq!(
            slots.len(),
            painted.len(),
            "the walk covers exactly the rows that paint"
        );
        for (id, slot) in &slots {
            let bounds = painted
                .get(id)
                .unwrap_or_else(|| panic!("{id:?} walked but never painted"));
            let expected = top + offset + slot.top;
            assert!(
                (f32::from(bounds.top()) - expected).abs() < 0.5,
                "{id:?}: walked to {expected}, painted at {}",
                f32::from(bounds.top())
            );
            assert!((f32::from(bounds.size.height) - (slot.bottom - slot.top)).abs() < 0.5);
        }
        set_windowing(sidebar, true, cx);
    }

    #[gpui::test]
    fn walked_slots_match_the_painted_layout_in_project_grouping(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 120, 7, Some(9), SidebarGrouping::Project);
        assert_slots_match_layout(&sidebar, cx);
        // A collapsed project in the middle moves everything below it.
        sidebar.update(cx, |sidebar, _| {
            let mut store = sidebar.store.write().unwrap();
            let project = store.sidebar_projection().projects[2].project.id.clone();
            let _ = store.toggle_project_collapsed(project);
        });
        // The fold starts on the next render; let it finish before comparing.
        redraw(&sidebar, cx);
        std::thread::sleep(Duration::from_millis(400));
        redraw(&sidebar, cx);
        assert_slots_match_layout(&sidebar, cx);
    }

    #[gpui::test]
    fn walked_slots_match_the_painted_layout_in_recency_grouping(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 120, 7, Some(9), SidebarGrouping::Recency);
        assert_slots_match_layout(&sidebar, cx);
    }

    #[gpui::test]
    fn walked_slots_match_the_preview_fleet(cx: &mut TestAppContext) {
        // The hand-made preview: pinned rows, children, archives, remote.
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            Panel { sidebar }
        });
        let sidebar = view.read_with(cx, |panel, _| panel.sidebar.clone());
        assert_slots_match_layout(&sidebar, cx);
    }

    #[gpui::test]
    fn a_long_list_builds_only_the_rows_near_the_viewport(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 1000, 20, None, SidebarGrouping::Project);
        let (_, _, viewport) = scroll(&sidebar, cx);
        assert!(viewport > 200.0, "the test window shows a real list");
        let slots = slots(&sidebar, cx);
        assert_eq!(slots.len(), 1000, "every row is walked");
        let painted = built(&sidebar, cx);
        let budget = ((viewport + 2.0 * row_window::MIN_OVERSCAN.max(viewport / 2.0))
            / (SIDEBAR_NAV_ROW_HEIGHT + 2.0)) as usize
            + 4;
        assert!(
            painted.len() <= budget,
            "{} rows built for a {viewport} px viewport",
            painted.len()
        );
    }

    #[gpui::test]
    fn windowing_leaves_the_layout_untouched(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 400, 20, Some(11), SidebarGrouping::Project);
        scroll_to(&sidebar, 3_000.0, cx);
        set_windowing(&sidebar, false, cx);
        let (_, offset, _) = scroll(&sidebar, cx);
        let max = sidebar.read_with(cx, |sidebar, _| sidebar.list_scroll.max_offset().y);
        let everything = built(&sidebar, cx);
        let sections = sidebar.read_with(cx, |sidebar, _| sidebar.section_bounds.borrow().clone());
        set_windowing(&sidebar, true, cx);
        assert_eq!(scroll(&sidebar, cx).1, offset);
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.list_scroll.max_offset().y),
            max,
            "spacers keep the content height"
        );
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.section_bounds.borrow().clone()),
            sections,
            "every section lays out where it did"
        );
        let windowed = built(&sidebar, cx);
        assert!(windowed.len() < everything.len() / 4);
        for (id, bounds) in &windowed {
            assert_eq!(Some(bounds), everything.get(id), "{id:?} moved");
        }
    }

    #[gpui::test]
    fn scrolling_builds_every_row_in_view_and_drops_the_ones_left_behind(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 1000, 20, Some(13), SidebarGrouping::Project);
        let top_rows: Vec<SessionId> = built(&sidebar, cx).into_keys().collect();
        for y in [2_500.0, 9_000.0, 17_250.0] {
            scroll_to(&sidebar, y, cx);
            let (_, offset, viewport) = scroll(&sidebar, cx);
            assert_eq!(offset, -y);
            let visible = row_window::Span::new(y, y + viewport);
            let painted = built(&sidebar, cx);
            for (id, slot) in slots(&sidebar, cx) {
                if slot.top < visible.bottom && visible.top < slot.bottom {
                    assert!(painted.contains_key(&id), "{id:?} is in view but not built");
                }
            }
            let held = sidebar.read_with(cx, |sidebar, _| sidebar.held_rows.clone());
            for id in &top_rows {
                assert!(
                    held.contains(id) || !painted.contains_key(id),
                    "{id:?} scrolled far away but kept its bounds"
                );
            }
        }
    }

    #[gpui::test]
    fn the_keyboard_reveals_a_row_that_was_never_built(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 1000, 20, None, SidebarGrouping::Project);
        let last = visible_order(&sidebar, cx).last().cloned().unwrap();
        assert!(!built(&sidebar, cx).contains_key(&last));
        sidebar.update_in(cx, |sidebar, window, cx| sidebar.focus(window, cx));
        cx.simulate_keystrokes("end");
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.focus_cursor.clone()),
            Some(last.clone())
        );
        // The reveal runs on the following frames.
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
        }
        let (top, offset, viewport) = scroll(&sidebar, cx);
        assert!(offset < -1_000.0, "the list scrolled to the end ({offset})");
        let row = built(&sidebar, cx)[&last];
        assert!(f32::from(row.top()) >= top - 0.5);
        assert!(f32::from(row.bottom()) <= top + viewport + 0.5);
    }

    #[gpui::test]
    fn revealing_from_a_walked_slot_scrolls_the_row_fully_into_view(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 600, 20, None, SidebarGrouping::Project);
        let order = visible_order(&sidebar, cx);
        let target = order[order.len() / 2].clone();
        assert!(!built(&sidebar, cx).contains_key(&target));
        let slot = slots(&sidebar, cx)[&target];
        sidebar.update_in(cx, |sidebar, window, _| {
            assert!(reveal_tracked_row(
                &sidebar.list_scroll,
                &sidebar.row_bounds,
                &sidebar.row_slots,
                &target,
                window,
            ));
        });
        redraw(&sidebar, cx);
        let (top, offset, viewport) = scroll(&sidebar, cx);
        assert_eq!(
            offset,
            -(slot.bottom - viewport),
            "scrolled just far enough"
        );
        let row = built(&sidebar, cx)[&target];
        assert!((f32::from(row.bottom()) - (top + viewport)).abs() < 0.5);
    }

    #[gpui::test]
    fn a_shift_click_selects_the_range_across_rows_never_built(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 1000, 1, None, SidebarGrouping::Project);
        let order = visible_order(&sidebar, cx);
        let first = order[1].clone();
        cx.simulate_click(built(&sidebar, cx)[&first].center(), Modifiers::none());
        let far = order[700].clone();
        let slot = slots(&sidebar, cx)[&far];
        scroll_to(&sidebar, slot.top - 200.0, cx);
        cx.simulate_click(built(&sidebar, cx)[&far].center(), Modifiers::shift());
        let selection = sidebar.read_with(cx, |sidebar, _| {
            sidebar.store.write().unwrap().sidebar_selection().clone()
        });
        let expected: HashSet<SessionId> = order[1..=700].iter().cloned().collect();
        assert_eq!(selection, expected);
    }

    #[gpui::test]
    fn a_dragged_row_stays_built_when_it_scrolls_away_and_still_reorders(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 500, 1, None, SidebarGrouping::Project);
        let order = visible_order(&sidebar, cx);
        let source = order[2].clone();
        let from = built(&sidebar, cx)[&source].center();
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            from + point(px(0.0), px(6.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert!(sidebar.read_with(cx, |sidebar, _| sidebar.ui.drag.is_some()));
        // The list scrolls far below the dragged row mid-drag.
        let target = order[300].clone();
        let slot = slots(&sidebar, cx)[&target];
        scroll_to(&sidebar, slot.top - 300.0, cx);
        assert!(
            built(&sidebar, cx).contains_key(&source),
            "the dragged row keeps its view and bounds"
        );
        let target_bounds = built(&sidebar, cx)[&target];
        let below = point(target_bounds.center().x, target_bounds.bottom() - px(2.0));
        cx.simulate_mouse_move(below, MouseButton::Left, Modifiers::default());
        redraw(&sidebar, cx);
        assert!(
            cx.debug_bounds("insertion-marker:After").is_some(),
            "the drop resolves against the row under the pointer"
        );
        cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::default());
        let reordered = visible_order(&sidebar, cx);
        let position = |id: &SessionId| reordered.iter().position(|row| row == id).unwrap();
        assert_eq!(position(&source), position(&target) + 1);
        assert_eq!(reordered.len(), order.len());
    }

    #[gpui::test]
    fn held_rows_are_built_wherever_they_are(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 800, 4, None, SidebarGrouping::Project);
        let order = visible_order(&sidebar, cx);
        let selected = order[600].clone();
        let renaming = order[650].clone();
        sidebar.update(cx, |sidebar, _| {
            sidebar.store.write().unwrap().select(selected.clone());
            sidebar.ui.renaming = Some(renaming.clone());
        });
        redraw(&sidebar, cx);
        let painted = built(&sidebar, cx);
        assert!(painted.contains_key(&selected));
        assert!(painted.contains_key(&renaming));
        assert!(!painted.contains_key(&order[625]));
    }

    #[gpui::test]
    fn stable_row_views_survive_store_updates_and_scrolling(cx: &mut TestAppContext) {
        let (sidebar, cx) = fleet(cx, 300, 3, None, SidebarGrouping::Project);
        let order = visible_order(&sidebar, cx);
        let view_of = |sidebar: &Entity<Sidebar>, cx: &VisualTestContext, id: &SessionId| {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .session_row_views
                    .get(id)
                    .map(|view| view.entity_id())
            })
        };
        let first = view_of(&sidebar, cx, &order[0]).expect("a built row has a view");
        // A row far down is walked but not built, and still has no view
        // only because it never rendered.
        scroll_to(&sidebar, 5_000.0, cx);
        scroll_to(&sidebar, 0.0, cx);
        assert_eq!(
            view_of(&sidebar, cx, &order[0]),
            Some(first),
            "scrolling away and back keeps the row's view"
        );
        // Inserting a session above keeps every other row's slot identity.
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let mut session = (**store.sessions().get(&order[5]).unwrap()).clone();
            session.id = SessionId::new("bench-new");
            store.upsert_session(session);
            drop(store);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(view_of(&sidebar, cx, &order[0]), Some(first));
        assert_eq!(slots(&sidebar, cx).len(), 301);
    }
}
