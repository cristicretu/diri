//! Dragging a pane by its title: onto an edge of another pane (a split,
//! previewed live), onto a sidebar row (spring-loads that tab, or docks
//! into it on release), onto the free space below the rows (a tab of its
//! own), and Escape. Plus the headless frame renders of the motion.
use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use diri_proto::workspace::*;
use gpui::{Bounds, Modifiers, MouseButton, Pixels, Point, point, size};

fn pane(id: &str, session: &str) -> LayoutNode {
    LayoutNode::Pane {
        id: PaneId::new(id),
        session_id: SessionId::new(session),
    }
}

fn split(id: &str, axis: LayoutAxis, first: LayoutNode, second: LayoutNode) -> LayoutNode {
    LayoutNode::Split {
        id: SplitId::new(id),
        axis,
        fraction: 0.5,
        first: Box::new(first),
        second: Box::new(second),
    }
}

fn pane_shell(id: &str, title: &str, fixture: &SidebarPreviewFixture) -> SessionRecord {
    let mut shell = fixture
        .list
        .sessions
        .iter()
        .find(|session| session.id.0 == "preview-shell")
        .unwrap()
        .clone();
    shell.id = SessionId::new(id);
    shell.title = title.into();
    shell.parent = Some(SessionId::new("preview-claude"));
    shell.listening_ports = None;
    shell
}

/// Tab "release": `release` (the layout under test, selected). Tab "codex":
/// the Codex agent alone.
fn services_with(release: LayoutNode, focused: &str) -> (Arc<AppServices>, WorkspaceId) {
    let services = test_services();
    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let tests = pane_shell("pane-tests", "cargo test", &fixture);
    let server = pane_shell("pane-server", "pnpm dev · website", &fixture);
    fixture.list.sessions.push(tests);
    fixture.list.sessions.push(server);
    let workspace = WorkspaceId::new("project-workspace");
    {
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store
            .update_preferences(|prefs| {
                prefs.terminal_theme = "dirijor-dark".into();
                prefs.sidebar_visible = true;
            })
            .unwrap();
        store.select(SessionId::new("preview-claude"));
        store.seed_workspace_snapshot_for_test(snapshot(release, focused, 1));
    }
    (services, workspace)
}

fn snapshot(release: LayoutNode, focused: &str, revision: u64) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        revision,
        workspaces: vec![WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new("project-workspace"),
            name: "diri".into(),
            selected_tab: Some(TabId::new("release")),
            tabs: vec![
                WorkspaceTab {
                    id: TabId::new("release"),
                    title: None,
                    focused_pane: PaneId::new(focused),
                    zoomed_pane: None,
                    layout: release,
                },
                WorkspaceTab {
                    id: TabId::new("codex"),
                    title: None,
                    focused_pane: PaneId::new("codex"),
                    zoomed_pane: None,
                    layout: pane("codex", "preview-codex"),
                },
            ],
        }],
        ..Default::default()
    }
}

fn agent_and_tests() -> LayoutNode {
    split(
        "columns",
        LayoutAxis::Horizontal,
        pane("agent", "preview-claude"),
        pane("tests", "pane-tests"),
    )
}

fn open<'a>(
    release: LayoutNode,
    focused: &str,
    cx: &'a mut gpui::TestAppContext,
) -> (Entity<RootView>, Arc<AppServices>, &'a mut gpui::VisualTestContext) {
    let (services, workspace) = services_with(release, focused);
    let held = services.clone();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    root.update_in(cx, |root, window, cx| {
        root.activate_saved_workspace(Some(workspace), window, cx)
    });
    cx.simulate_resize(size(px(1100.0), px(900.0)));
    cx.run_until_parked();
    (root, held, cx)
}

fn bounds(cx: &mut gpui::VisualTestContext, selector: String) -> Option<Bounds<Pixels>> {
    cx.debug_bounds(Box::leak(selector.into_boxed_str()))
}

/// Picks up `pane` by its title and carries it to `to`, in two moves so
/// GPUI's drag threshold is crossed before the target.
fn carry(cx: &mut gpui::VisualTestContext, pane: &str, to: Point<Pixels>) {
    let handle = bounds(cx, format!("pane-drag-handle-{pane}")).expect("the title is a handle");
    let start = handle.center();
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + point(px(12.0), px(12.0)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn release(cx: &mut gpui::VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn in_flight(services: &Arc<AppServices>) -> Option<WorkspaceMutation> {
    services
        .store
        .store
        .read()
        .unwrap()
        .in_flight_workspace_edit_for_test()
        .cloned()
}

/// The pane `agent` spans the left half of the workbench; its title tells
/// where that is.
fn agent_pane(cx: &mut gpui::VisualTestContext) -> (f32, f32) {
    let agent = bounds(cx, "pane-drag-handle-agent".into()).unwrap();
    let tests = bounds(cx, "pane-drag-handle-tests".into()).unwrap();
    (f32::from(agent.left()), f32::from(tests.left()))
}

#[gpui::test]
fn a_pane_dropped_on_another_panes_edge_splits_it_there(cx: &mut gpui::TestAppContext) {
    let (_root, services, cx) = open(agent_and_tests(), "agent", cx);
    let (left, right) = agent_pane(cx);
    let middle = (left + right) / 2.0;
    // Low in the agent's pane: its bottom edge.
    let target = point(px(middle), px(860.0));
    carry(cx, "tests", target);
    let preview = cx
        .debug_bounds("pane-drop-preview")
        .expect("the landing place is previewed");
    assert!(
        preview.top() > px(400.0) && preview.right() < px(right),
        "the bottom half of the agent's pane: {preview:?}"
    );
    // The carried pane stays put until it lands.
    assert!(in_flight(&services).is_none());
    release(cx, target);
    assert_eq!(
        in_flight(&services),
        Some(WorkspaceMutation::MoveNode {
            source_tab: TabId::new("release"),
            node: LayoutNodeId::Pane(PaneId::new("tests")),
            destination_tab: TabId::new("release"),
            target: PaneId::new("agent"),
            edge: DockEdge::Bottom,
        })
    );
    assert!(cx.debug_bounds("pane-drop-preview").is_none());
}

#[gpui::test]
fn the_middle_of_a_pane_swaps_and_its_own_place_does_nothing(cx: &mut gpui::TestAppContext) {
    let (_root, services, cx) = open(agent_and_tests(), "agent", cx);
    let (left, right) = agent_pane(cx);
    // a | b with a dropped on b's left edge, or b on a's right edge: the
    // layout it already has. Nothing lights up and nothing is sent.
    let own_place = point(px(right - 20.0), px(450.0));
    carry(cx, "tests", own_place);
    assert!(cx.debug_bounds("pane-drop-preview").is_none());
    release(cx, own_place);
    assert!(in_flight(&services).is_none());

    let centre = point(px((left + right) / 2.0), px(450.0));
    carry(cx, "tests", centre);
    assert!(cx.debug_bounds("pane-drop-preview").is_some());
    release(cx, centre);
    assert_eq!(
        in_flight(&services),
        Some(WorkspaceMutation::SwapPanes {
            first_tab: TabId::new("release"),
            first: PaneId::new("tests"),
            second_tab: TabId::new("release"),
            second: PaneId::new("agent"),
        })
    );
}

#[gpui::test]
fn escape_puts_a_carried_pane_back(cx: &mut gpui::TestAppContext) {
    let (_root, services, cx) = open(agent_and_tests(), "agent", cx);
    let (left, right) = agent_pane(cx);
    let target = point(px((left + right) / 2.0), px(860.0));
    carry(cx, "tests", target);
    assert!(cx.debug_bounds("pane-drop-preview").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("pane-drop-preview").is_none());
    release(cx, target);
    assert!(in_flight(&services).is_none());
}

#[gpui::test]
fn a_tab_of_one_pane_keeps_its_title_for_moving_the_window(cx: &mut gpui::TestAppContext) {
    let (_root, _services, cx) = open(pane("agent", "preview-claude"), "agent", cx);
    assert!(bounds(cx, "pane-drag-handle-agent".into()).is_none());
}

fn row(root: &Entity<RootView>, cx: &mut gpui::VisualTestContext, id: &str) -> Bounds<Pixels> {
    root.update_in(cx, |root, _, cx| {
        root.sidebar
            .read(cx)
            .project_agent_center_for_test(&SessionId::new(id))
            .map(|center| Bounds::centered_at(center, size(px(40.0), px(10.0))))
    })
    .expect("the row is on screen")
}

/// The window's selection (each window keeps its own).
fn selected(root: &Entity<RootView>, cx: &mut gpui::VisualTestContext) -> Option<SessionId> {
    root.update_in(cx, |root, _, _| {
        root.window_store
            .read()
            .expect("store")
            .selected_session_id()
            .cloned()
    })
}

#[gpui::test]
fn resting_on_a_row_opens_its_tab_after_the_spring_load_delay(cx: &mut gpui::TestAppContext) {
    let (root, _services, cx) = open(agent_and_tests(), "agent", cx);
    let codex = row(&root, cx, "preview-codex").center();
    carry(cx, "tests", codex);
    cx.executor()
        .advance_clock(crate::pane_drag::SPRING_LOAD - Duration::from_millis(100));
    cx.run_until_parked();
    assert_eq!(selected(&root, cx), Some(SessionId::new("preview-claude")));
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    assert_eq!(
        selected(&root, cx),
        Some(SessionId::new("preview-codex")),
        "the tab under the pane opened"
    );
    release(cx, codex);
}

#[gpui::test]
fn a_pane_released_on_a_row_joins_that_tab_and_the_window_follows(
    cx: &mut gpui::TestAppContext,
) {
    let (root, services, cx) = open(agent_and_tests(), "agent", cx);
    let codex = row(&root, cx, "preview-codex").center();
    carry(cx, "tests", codex);
    release(cx, codex);
    assert_eq!(
        in_flight(&services),
        Some(WorkspaceMutation::MoveNode {
            source_tab: TabId::new("release"),
            node: LayoutNodeId::Pane(PaneId::new("tests")),
            destination_tab: TabId::new("codex"),
            target: PaneId::new("codex"),
            edge: DockEdge::Right,
        })
    );
    assert_eq!(selected(&root, cx), Some(SessionId::new("preview-codex")));
}

#[gpui::test]
fn an_agent_pane_released_below_the_rows_gets_a_tab_of_its_own(
    cx: &mut gpui::TestAppContext,
) {
    // Two agents share the tab: either can leave, each has a row.
    let shared = split(
        "columns",
        LayoutAxis::Horizontal,
        pane("agent", "preview-claude"),
        pane("cursor", "preview-cursor"),
    );
    let (root, services, cx) = open(shared, "agent", cx);
    let last = root.update_in(cx, |root, _, cx| {
        root.sidebar.read(cx).last_row_bottom_for_test()
    });
    let free = point(px(120.0), last + px(40.0));
    carry(cx, "cursor", free);
    assert!(
        cx.debug_bounds("pane-new-tab-slot").is_some(),
        "the new tab's place is shown"
    );
    release(cx, free);
    assert_eq!(
        in_flight(&services),
        Some(WorkspaceMutation::BreakPane {
            tab_id: TabId::new("release"),
            pane_id: PaneId::new("cursor"),
        })
    );
    assert_eq!(selected(&root, cx), Some(SessionId::new("preview-cursor")));
}

#[gpui::test]
fn a_pane_shell_never_becomes_a_tab_nobody_can_reach(cx: &mut gpui::TestAppContext) {
    let (root, services, cx) = open(agent_and_tests(), "agent", cx);
    let last = root.update_in(cx, |root, _, cx| {
        root.sidebar.read(cx).last_row_bottom_for_test()
    });
    let free = point(px(120.0), last + px(40.0));
    carry(cx, "tests", free);
    assert!(cx.debug_bounds("pane-new-tab-slot").is_none());
    release(cx, free);
    assert!(in_flight(&services).is_none());
}

/// After a drop the panes slide from where they were to the new layout,
/// and settle within [`crate::pane_drag::MORPH`].
#[gpui::test]
fn a_moved_layout_glides_into_place_unless_motion_is_reduced(cx: &mut gpui::TestAppContext) {
    crate::pane_drag::test_clock::start();
    for reduce in [false, true] {
        cx.update(|cx| cx.set_reduce_motion(reduce));
        let (root, services, vcx) = open(agent_and_tests(), "agent", cx);
        let (left, right) = agent_pane(vcx);
        let target = point(px((left + right) / 2.0), px(860.0));
        let before = bounds(vcx, "pane-drag-handle-tests".into()).unwrap();
        let terminal = |root: &Entity<RootView>, vcx: &mut gpui::VisualTestContext| {
            root.update_in(vcx, |root, _, cx| {
                root.workspace_workbench
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .terminals_for_test()
                    .into_iter()
                    .find(|terminal| {
                        terminal.read(cx).session_id_for_test()
                            == Some(SessionId::new("pane-tests"))
                    })
                    .unwrap()
                    .entity_id()
            })
        };
        let mounted = terminal(&root, vcx);
        carry(vcx, "tests", target);
        release(vcx, target);
        // The Engine answers with the tests pane below the agent.
        services
            .store
            .store
            .write()
            .unwrap()
            .finish_workspace_edit_for_test(snapshot(
                split(
                    "rows",
                    LayoutAxis::Vertical,
                    pane("agent", "preview-claude"),
                    pane("tests", "pane-tests"),
                ),
                "tests",
                2,
            ));
        vcx.run_until_parked();
        root.update_in(vcx, |_, window, _| window.refresh());
        vcx.run_until_parked();
        let first = bounds(vcx, "pane-drag-handle-tests".into()).unwrap();
        // The same mounted terminal moved: nothing reattached or respawned.
        assert_eq!(terminal(&root, vcx), mounted);
        let moving = root.update_in(vcx, |root, _, cx| {
            root.workspace_workbench
                .as_ref()
                .unwrap()
                .read(cx)
                .morphing_for_test()
        });
        assert_eq!(moving, !reduce, "reduce motion: {reduce}");
        if reduce {
            assert!(first.left() < px(right), "it lands at once: {first:?}");
        } else {
            // First frame: still where it was.
            assert!(
                (f32::from(first.left()) - f32::from(before.left())).abs() < 40.0,
                "{before:?} -> {first:?}"
            );
            crate::pane_drag::test_clock::advance(crate::pane_drag::MORPH / 2);
            root.update_in(vcx, |_, window, _| window.refresh());
            vcx.run_until_parked();
            let halfway = bounds(vcx, "pane-drag-handle-tests".into()).unwrap();
            assert!(
                halfway.top() > first.top() && halfway.left() < first.left(),
                "{first:?} -> {halfway:?}"
            );
            crate::pane_drag::test_clock::advance(crate::pane_drag::MORPH);
            root.update_in(vcx, |_, window, _| window.refresh());
            vcx.run_until_parked();
            let settled = bounds(vcx, "pane-drag-handle-tests".into()).unwrap();
            assert!(settled.left() < px(right) && settled.top() > px(400.0));
            assert!(!root.update_in(vcx, |root, _, cx| {
                root.workspace_workbench
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .morphing_for_test()
            }));
        }
    }
    crate::pane_drag::test_clock::stop();
}

/// The keyboard twins, from the palette.
#[gpui::test]
fn the_palette_gives_the_focused_pane_a_tab_of_its_own(cx: &mut gpui::TestAppContext) {
    let shared = split(
        "columns",
        LayoutAxis::Horizontal,
        pane("agent", "preview-claude"),
        pane("cursor", "preview-cursor"),
    );
    let (root, services, cx) = open(shared, "cursor", cx);
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::MovePaneToNewTab, window, cx)
    });
    assert_eq!(
        in_flight(&services),
        Some(WorkspaceMutation::BreakPane {
            tab_id: TabId::new("release"),
            pane_id: PaneId::new("cursor"),
        })
    );
}

#[gpui::test]
fn the_palette_moves_the_focused_pane_into_the_next_tab(cx: &mut gpui::TestAppContext) {
    let (root, services, cx) = open(agent_and_tests(), "tests", cx);
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::MovePaneToNextTab, window, cx)
    });
    let Some(WorkspaceMutation::MoveNode {
        node,
        destination_tab,
        ..
    }) = in_flight(&services)
    else {
        panic!("a move: {:?}", in_flight(&services));
    };
    assert_eq!(node, LayoutNodeId::Pane(PaneId::new("tests")));
    assert_eq!(destination_tab, TabId::new("codex"));
}
