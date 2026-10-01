//! The keyboard follows the workspace session on screen: a shell that exits
//! hands the window to another session, and a new tab takes typing at once.
use super::tests::test_services;
use super::*;
use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
use diri_proto::workspace::{
    LayoutNode, PaneId, TabId, WorkspaceId, WorkspaceRecord, WorkspaceSnapshot, WorkspaceTab,
};
use diri_proto::{ExitInfo, ExitReason, Resumability, SessionStatus};
use gpui::size;

fn shell(template: &SessionRecord, id: &str) -> SessionRecord {
    let mut shell = template.clone();
    shell.id = SessionId::new(id);
    shell.kind = AgentKind::SHELL;
    shell.title = id.to_owned();
    shell.parent = None;
    shell.agent_session_id = None;
    shell.transcript_path = None;
    shell.foreground_agent = None;
    shell.archived_at = None;
    shell.capabilities = None;
    shell.resumability = Resumability::Live;
    shell.status = SessionStatus::Idle;
    shell
}

fn tab(name: &str, session: &SessionId) -> WorkspaceTab {
    let pane = PaneId::new(format!("{name}-pane"));
    WorkspaceTab {
        id: TabId::new(format!("{name}-tab")),
        title: None,
        focused_pane: pane.clone(),
        zoomed_pane: None,
        layout: LayoutNode::Pane {
            id: pane,
            session_id: session.clone(),
        },
    }
}

fn snapshot(revision: u64, tabs: Vec<WorkspaceTab>, selected: &WorkspaceTab) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        revision,
        workspaces: vec![WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new("focus-workspace"),
            name: "Focus".into(),
            selected_tab: Some(selected.id.clone()),
            tabs,
        }],
        ..Default::default()
    }
}

/// ⌘T reaches RootView: focus sits on a rendered element inside its key
/// context rather than on a dropped or hidden handle.
fn keyboard_reaches_app_commands(window: &Window, cx: &App) -> bool {
    window.is_action_available(&NewDefaultSession, cx)
}

/// Typing goes to the terminal on screen.
fn typing_reaches_active_terminal(root: &RootView, window: &Window, cx: &App) -> bool {
    root.active_terminal(cx)
        .is_some_and(|terminal| terminal.read(cx).is_focused(window))
}

#[gpui::test]
fn exiting_the_focused_workspace_shell_shows_the_previous_session_with_the_keyboard(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let template = fixture.list.sessions[0].clone();
    let exiting = SessionId::new("exiting-shell");
    let previous = {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.upsert_session(shell(&template, &exiting.0));
        store
            .ordered_sessions()
            .into_iter()
            .find(|session| session.id != exiting && !session.is_archived())
            .unwrap()
            .id
    };
    let previous_tab = tab("previous", &previous);
    let exiting_tab = tab("exiting", &exiting);
    runtime
        .store
        .write()
        .unwrap()
        .seed_workspace_snapshot_for_test(snapshot(
            1,
            vec![previous_tab.clone(), exiting_tab.clone()],
            &exiting_tab,
        ));
    let (root, cx) = cx.add_window_view({
        let previous = previous.clone();
        let exiting = exiting.clone();
        move |window, cx| {
            let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
            root.window_store.write().unwrap().select(previous);
            root.window_store.write().unwrap().select(exiting);
            root.sidebar.update(cx, |sidebar, cx| {
                sidebar.activate_workspace(Some(WorkspaceId::new("focus-workspace")), cx)
            });
            root
        }
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.active_session_id(cx), Some(exiting.clone()));
        assert!(typing_reaches_active_terminal(root, window, cx));
    });

    // The shell runs `exit`: a clean exit with nothing to resume closes it.
    {
        let mut store = runtime.store.write().unwrap();
        let mut exited = store.sessions()[&exiting].as_ref().clone();
        exited.status = SessionStatus::Exited(ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
        });
        store.upsert_session(exited);
    }
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(
            root.active_session_id(cx),
            Some(previous.clone()),
            "the window moves on to the session used before the shell"
        );
        assert!(typing_reaches_active_terminal(root, window, cx));
        assert!(keyboard_reaches_app_commands(window, cx));
    });

    // The Engine confirms the removal while the layout still names the shell.
    runtime
        .store
        .write()
        .unwrap()
        .remove_session_record(&exiting);
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.active_session_id(cx), Some(previous.clone()));
        assert!(typing_reaches_active_terminal(root, window, cx));
        assert!(keyboard_reaches_app_commands(window, cx));
    });

    // The project layout opens the survivor's own tab; typing goes there.
    runtime
        .store
        .write()
        .unwrap()
        .finish_workspace_edit_for_test(snapshot(
            2,
            vec![previous_tab.clone(), exiting_tab],
            &previous_tab,
        ));
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.active_session_id(cx), Some(previous.clone()));
        assert!(typing_reaches_active_terminal(root, window, cx));
        assert!(keyboard_reaches_app_commands(window, cx));
    });
}

#[gpui::test]
fn a_pane_whose_session_disappears_keeps_app_shortcuts_working(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let template = fixture.list.sessions[0].clone();
    let only = SessionId::new("only-shell");
    let only_tab = tab("only", &only);
    {
        let mut store = runtime.store.write().unwrap();
        store.upsert_session(shell(&template, &only.0));
        store.seed_workspace_snapshot_for_test(snapshot(1, vec![only_tab.clone()], &only_tab));
    }
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
        root.sidebar.update(cx, |sidebar, cx| {
            sidebar.activate_workspace(Some(WorkspaceId::new("focus-workspace")), cx)
        });
        root
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert!(typing_reaches_active_terminal(root, window, cx));
    });

    // Removed elsewhere (another window, the CLI) with no session left.
    runtime.store.write().unwrap().remove_session_record(&only);
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |_, window, cx| {
        assert!(
            keyboard_reaches_app_commands(window, cx),
            "⌘T must not wait for a click on the empty pane"
        );
    });
}

#[gpui::test]
fn a_new_workspace_tab_takes_the_keyboard_when_its_session_arrives_first(
    cx: &mut gpui::TestAppContext,
) {
    check_new_workspace_tab_takes_the_keyboard(cx, true);
}

#[gpui::test]
fn a_new_workspace_tab_takes_the_keyboard_when_its_tab_arrives_first(
    cx: &mut gpui::TestAppContext,
) {
    check_new_workspace_tab_takes_the_keyboard(cx, false);
}

/// ⌘T's session record and its placement travel separately, so either can
/// reach the window first.
fn check_new_workspace_tab_takes_the_keyboard(cx: &mut gpui::TestAppContext, session_first: bool) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let template = fixture.list.sessions[0].clone();
    let first = SessionId::new("first-shell");
    let created = SessionId::new("created-shell");
    let first_tab = tab("first", &first);
    let created_tab = tab("created", &created);
    {
        let mut store = runtime.store.write().unwrap();
        store.upsert_session(shell(&template, &first.0));
        store.seed_workspace_snapshot_for_test(snapshot(1, vec![first_tab.clone()], &first_tab));
    }
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
        root.sidebar.update(cx, |sidebar, cx| {
            sidebar.activate_workspace(Some(WorkspaceId::new("focus-workspace")), cx)
        });
        root
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert!(typing_reaches_active_terminal(root, window, cx));
    });

    let arrive_session = |cx: &mut gpui::VisualTestContext| {
        runtime
            .store
            .write()
            .unwrap()
            .upsert_session(shell(&template, &created.0));
        runtime.publish_local_change();
        cx.run_until_parked();
    };
    let arrive_tab = |cx: &mut gpui::VisualTestContext| {
        runtime
            .store
            .write()
            .unwrap()
            .finish_workspace_refresh_for_test(snapshot(
                2,
                vec![first_tab.clone(), created_tab.clone()],
                &created_tab,
            ));
        runtime.publish_local_change();
        cx.run_until_parked();
    };
    if session_first {
        arrive_session(cx);
        arrive_tab(cx);
    } else {
        arrive_tab(cx);
        root.update_in(cx, |_, window, cx| {
            assert!(keyboard_reaches_app_commands(window, cx));
        });
        arrive_session(cx);
    }
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.active_session_id(cx), Some(created.clone()));
        assert!(
            typing_reaches_active_terminal(root, window, cx),
            "typing goes to the new terminal without a click"
        );
        assert!(
            !root.terminal.as_ref().unwrap().read(cx).is_focused(window),
            "the covered plain terminal must not follow the selection"
        );
    });
}

#[gpui::test]
fn returning_to_an_unchanged_workspace_tab_moves_the_keyboard_into_it(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let template = fixture.list.sessions[0].clone();
    let only = SessionId::new("only-shell");
    let only_tab = tab("only", &only);
    {
        let mut store = runtime.store.write().unwrap();
        store.upsert_session(shell(&template, &only.0));
        store.seed_workspace_snapshot_for_test(snapshot(1, vec![only_tab.clone()], &only_tab));
    }
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    let workspace = Some(WorkspaceId::new("focus-workspace"));
    root.update_in(cx, |root, _, cx| {
        root.sidebar.update(cx, |sidebar, cx| {
            sidebar.activate_workspace(workspace.clone(), cx)
        })
    });
    cx.run_until_parked();
    // Leave for the plain terminal, which takes the keyboard, then return to
    // the same tab and pane.
    root.update_in(cx, |root, window, cx| {
        root.sidebar
            .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
        root.activate_saved_workspace(None, window, cx);
    });
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert!(root.terminal.as_ref().unwrap().read(cx).is_focused(window));
        root.sidebar.update(cx, |sidebar, cx| {
            sidebar.activate_workspace(workspace.clone(), cx)
        });
    });
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.active_session_id(cx), Some(only.clone()));
        assert!(typing_reaches_active_terminal(root, window, cx));
        assert!(keyboard_reaches_app_commands(window, cx));
    });
}
