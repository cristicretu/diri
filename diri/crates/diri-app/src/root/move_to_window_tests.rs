//! Moving a tab between windows: the palette commands, and dragging a
//! sidebar row out of the window.
use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use diri_proto::workspace::*;
use gpui::{AnyWindowHandle, Modifiers, MouseButton, Pixels, Point, TestAppContext, point, size};

fn pane(id: &str, session: &str) -> LayoutNode {
    LayoutNode::Pane {
        id: PaneId::new(id),
        session_id: SessionId::new(session),
    }
}

/// One project workspace with two tabs, Claude (selected) and Codex. The
/// fixture's other sessions are shown alone, outside any workspace. With
/// `only`, just those sessions exist and Claude and Codex share one split
/// tab.
fn services(only: Option<&[&str]>) -> Arc<AppServices> {
    let services = test_services();
    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    if let Some(only) = only {
        fixture
            .list
            .sessions
            .retain(|session| only.contains(&session.id.0.as_str()));
    }
    let mut store = services.store.store.write().unwrap();
    store.hydrate(fixture.list);
    store
        .update_preferences(|prefs| prefs.sidebar_visible = true)
        .unwrap();
    store.select(SessionId::new("preview-claude"));
    let tab = |id: &str, session: &str| WorkspaceTab {
        id: TabId::new(id),
        title: None,
        focused_pane: PaneId::new(id),
        zoomed_pane: None,
        layout: pane(id, session),
    };
    store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
        revision: 1,
        workspaces: vec![WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new("project-workspace"),
            name: "diri".into(),
            selected_tab: Some(TabId::new("release")),
            tabs: if only.is_some() {
                vec![WorkspaceTab {
                    layout: LayoutNode::Split {
                        id: SplitId::new("columns"),
                        axis: LayoutAxis::Horizontal,
                        fraction: 0.5,
                        first: Box::new(pane("release", "preview-claude")),
                        second: Box::new(pane("codex", "preview-codex")),
                    },
                    ..tab("release", "preview-claude")
                }]
            } else {
                vec![tab("release", "preview-claude"), tab("codex", "preview-codex")]
            },
        }],
        ..Default::default()
    });
    drop(store);
    services
}

fn open(cx: &mut TestAppContext) -> (Entity<RootView>, &mut gpui::VisualTestContext) {
    open_with(None, cx)
}

fn open_with<'a>(
    only: Option<&[&str]>,
    cx: &'a mut TestAppContext,
) -> (Entity<RootView>, &'a mut gpui::VisualTestContext) {
    let services = services(only);
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    root.update_in(cx, |root, window, cx| {
        root.activate_saved_workspace(Some(WorkspaceId::new("project-workspace")), window, cx)
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    (root, cx)
}

fn roots(cx: &mut gpui::VisualTestContext) -> Vec<AnyWindowHandle> {
    // Through the app, not the test window: that window may have closed.
    cx.cx.update(|cx| {
        cx.windows()
            .into_iter()
            .filter(|handle| handle.downcast::<RootView>().is_some())
            .collect()
    })
}

/// What a window shows: its workspace and the tab (row) of its selection.
fn shows(cx: &mut gpui::VisualTestContext, handle: AnyWindowHandle) -> (Option<WorkspaceId>, Option<SessionId>) {
    cx.update_window(handle, |view, _, cx| {
        let root = view.downcast::<RootView>().unwrap();
        let root = root.read(cx);
        (root.window_workspace(), root.shown_tab_session())
    })
    .unwrap()
}

fn row(root: &Entity<RootView>, cx: &mut gpui::VisualTestContext, id: &str) -> Point<Pixels> {
    root.read_with(cx, |root, cx| {
        root.sidebar
            .read(cx)
            .project_agent_center_for_test(&SessionId::new(id))
            .expect("the row is drawn")
    })
}

fn drag_row_to(root: &Entity<RootView>, cx: &mut gpui::VisualTestContext, id: &str, to: Point<Pixels>) {
    let start = row(root, cx, id);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(start + point(px(10.0), px(4.0)), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn move_to_new_window_shows_a_one_pane_tab_alone_there_and_the_source_moves_on(
    cx: &mut TestAppContext,
) {
    let (root, cx) = open(cx);
    let source = roots(cx)[0];
    assert_eq!(
        shows(cx, source),
        (
            Some(WorkspaceId::new("project-workspace")),
            Some(SessionId::new("preview-claude"))
        )
    );
    root.update_in(cx, |root, window, cx| {
        root.run_command(CommandId::MoveTabToNewWindow, window, cx)
    });
    cx.run_until_parked();
    let windows = roots(cx);
    assert_eq!(windows.len(), 2, "one new window");
    let opened = windows.into_iter().find(|handle| *handle != source).unwrap();
    // Alone, in the new window's own All sessions view, so the source's
    // next tab cannot pull it along.
    assert_eq!(shows(cx, opened), (None, Some(SessionId::new("preview-claude"))));
    let (_, tab) = shows(cx, source);
    assert!(tab.is_some_and(|tab| tab.0 != "preview-claude"));
}

#[gpui::test]
fn move_to_next_window_reuses_the_other_window(cx: &mut TestAppContext) {
    let (root, cx) = open(cx);
    root.update_in(cx, |root, window, cx| root.run_command(CommandId::MoveTabToNewWindow, window, cx));
    cx.run_until_parked();
    let windows = roots(cx);
    assert_eq!(windows.len(), 2);
    let source = windows[0];
    let other = windows[1];
    // Bring Claude back to the first window from the second.
    cx.update_window(other, |view, window, cx| {
        view.downcast::<RootView>().unwrap().update(cx, |root, cx| {
            root.run_command(CommandId::MoveTabToNextWindow, window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(roots(cx).len(), 2, "no window was opened or left empty");
    assert_eq!(shows(cx, source).1, Some(SessionId::new("preview-claude")));
    assert_ne!(shows(cx, other).1, Some(SessionId::new("preview-claude")));
}

#[gpui::test]
fn a_row_dragged_out_of_the_window_tears_off_into_a_new_window(cx: &mut TestAppContext) {
    let (root, cx) = open(cx);
    // Released inside the window, on nothing: no tear-off.
    drag_row_to(&root, cx, "preview-claude", point(px(700.0), px(780.0)));
    assert_eq!(roots(cx).len(), 1);
    // Released to the left of the window.
    drag_row_to(&root, cx, "preview-claude", point(px(-240.0), px(300.0)));
    let windows = roots(cx);
    assert_eq!(windows.len(), 2, "the row became a window");
    let opened = windows[1];
    assert_eq!(shows(cx, opened).1, Some(SessionId::new("preview-claude")));
    assert_ne!(shows(cx, windows[0]).1, Some(SessionId::new("preview-claude")));
    // The session itself was never asked to end or respawn.
    assert!(
        root.read_with(cx, |root, _| root
            .window_store
            .read()
            .unwrap()
            .sessions()
            .contains_key(&SessionId::new("preview-claude")))
    );

    // Closing the torn-off window frees its view.
    let view = cx
        .update_window(opened, |view, _, _| {
            view.downcast::<RootView>().unwrap().downgrade()
        })
        .unwrap();
    cx.update_window(opened, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_eq!(roots(cx).len(), 1);
    assert!(view.upgrade().is_none(), "a closed torn-off window keeps no RootView alive");
}

#[gpui::test]
fn a_window_left_with_nothing_else_closes(cx: &mut TestAppContext) {
    // Claude's tab is a split with Codex, so it arrives as its workspace,
    // and every other session is in it: nothing the source could show
    // without mirroring the new window.
    let (root, cx) = open_with(Some(&["preview-claude", "preview-codex"]), cx);
    let source = roots(cx)[0];
    root.update_in(cx, |root, window, cx| root.run_command(CommandId::MoveTabToNewWindow, window, cx));
    cx.run_until_parked();
    let windows = roots(cx);
    assert_eq!(windows.len(), 1, "the tab moved; the emptied window closed");
    assert_ne!(windows[0], source);
    assert_eq!(shows(cx, windows[0]).1, Some(SessionId::new("preview-claude")));
}
