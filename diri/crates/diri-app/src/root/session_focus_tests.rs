//! The keyboard follows the workspace session on screen: a shell that exits
//! hands the window to another session, and a new tab takes typing at once.
use super::tests::test_services;
use super::*;
use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
use diri_proto::workspace::{
    LayoutNode, PaneId, TabId, WorkspaceId, WorkspaceMutation, WorkspaceRecord, WorkspaceSnapshot,
    WorkspaceTab,
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

fn exited(code: i32) -> SessionStatus {
    SessionStatus::Exited(ExitInfo {
        reason: ExitReason::Exited,
        code: Some(code),
        signal: None,
        system_restart: false,
    })
}

fn claude(template: &SessionRecord, id: &str) -> SessionRecord {
    let mut agent = shell(template, id);
    agent.kind = AgentKind::CLAUDE_CODE;
    agent.agent_session_id = Some("conversation".into());
    agent.resumability = Resumability::Resumable;
    agent
}

/// How the session on screen ends.
enum Ending {
    /// Its process exits with this status.
    Process(SessionStatus),
    /// ⌘W, with no confirmation asked.
    CloseShortcut,
}

#[gpui::test]
fn exiting_the_focused_workspace_shell_shows_the_previous_session_at_once(
    cx: &mut gpui::TestAppContext,
) {
    check_the_window_moves_on_in_place(
        cx,
        |template| shell(template, "exiting-shell"),
        Ending::Process(exited(0)),
        None,
    );
}

#[gpui::test]
fn quitting_a_focused_claude_session_shows_the_previous_session_at_once(
    cx: &mut gpui::TestAppContext,
) {
    check_the_window_moves_on_in_place(
        cx,
        |template| claude(template, "quitting-claude"),
        Ending::Process(exited(0)),
        None,
    );
}

#[gpui::test]
fn a_crashed_claude_session_stays_listed_and_is_announced(cx: &mut gpui::TestAppContext) {
    check_the_window_moves_on_in_place(
        cx,
        |template| claude(template, "crashed-claude"),
        Ending::Process(exited(1)),
        Some("exited with code 1"),
    );
}

#[gpui::test]
fn closing_the_focused_session_with_the_shortcut_stays_in_the_layout(
    cx: &mut gpui::TestAppContext,
) {
    check_the_window_moves_on_in_place(
        cx,
        |template| claude(template, "closed-claude"),
        Ending::CloseShortcut,
        None,
    );
}

/// The session on screen ends. The window shows the session used before it
/// in the same layout at once, ahead of the Engine, and never paints the
/// ended one's exit card or an empty pane. A session that did not end
/// cleanly stays listed and the window says so.
fn check_the_window_moves_on_in_place(
    cx: &mut gpui::TestAppContext,
    ending_session: impl FnOnce(&SessionRecord) -> SessionRecord,
    ending: Ending,
    notice: Option<&str>,
) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let ending_session = ending_session(&fixture.list.sessions[0]);
    let exiting = ending_session.id.clone();
    let previous = {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.upsert_session(ending_session);
        store
            .update_preferences(|prefs| prefs.confirm_before_closing_session = false)
            .unwrap();
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
    let shows_previous = |cx: &mut gpui::VisualTestContext| {
        for selector in ["exit-pill", "exited-card", "workspace-pane-unavailable"] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{selector} flashed while the window moved on"
            );
        }
        root.update_in(cx, |root, window, cx| {
            assert!(root.active_workspace.is_some(), "the layout stays");
            assert_eq!(
                root.active_session_id(cx),
                Some(previous.clone()),
                "the window shows the session used before"
            );
            assert!(typing_reaches_active_terminal(root, window, cx));
            assert!(keyboard_reaches_app_commands(window, cx));
        });
    };

    match ending {
        Ending::Process(status) => {
            let mut store = runtime.store.write().unwrap();
            let mut record = store.sessions()[&exiting].as_ref().clone();
            record.status = status;
            store.upsert_session(record);
        }
        Ending::CloseShortcut => {
            cx.simulate_keystrokes(&commands::test_chords("cmd-w"));
        }
    }
    runtime.publish_local_change();
    cx.run_until_parked();
    // Shown before the Engine answers: the edit that selects it is in flight.
    shows_previous(cx);
    assert!(!runtime.store.read().unwrap().workspace_catalog().can_edit());
    let closes = notice.is_none();
    assert_eq!(
        runtime.store.read().unwrap().is_open(&exiting),
        !closes,
        "only a session that ended cleanly closes"
    );
    root.update_in(cx, |root, _, _| match notice {
        Some(notice) => {
            let toast = root.toast.current().expect("the failure is announced");
            assert_eq!(toast.tone, crate::toast::ToastTone::Warning);
            assert!(toast.message.contains(notice), "{}", toast.message);
        }
        None => assert!(root.toast.current().is_none(), "a clean end is silent"),
    });

    if closes {
        // The Engine confirms the removal while the layout still names it.
        runtime
            .store
            .write()
            .unwrap()
            .remove_session_record(&exiting);
        runtime.publish_local_change();
        cx.run_until_parked();
        shows_previous(cx);
    }

    // The Engine selects the same tab; the window stops leading it.
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
    shows_previous(cx);
    root.update_in(cx, |root, _, _| assert!(root.pending_tab.is_none()));
}

/// The last live session of this layout ends; the next one lives in another
/// project's layout. The window switches layouts directly, never through
/// the plain terminal.
#[gpui::test]
fn the_last_session_of_a_layout_hands_over_to_another_layout(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let exiting = shell(&fixture.list.sessions[0], "last-shell");
    let exiting_id = exiting.id.clone();
    let other = fixture.list.sessions[1].id.clone();
    let here = WorkspaceId::new("focus-workspace");
    let there = WorkspaceId::new("other-workspace");
    {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.upsert_session(exiting);
        let mut both = snapshot(1, vec![tab("last", &exiting_id)], &tab("last", &exiting_id));
        let mut elsewhere = snapshot(1, vec![tab("other", &other)], &tab("other", &other))
            .workspaces
            .remove(0);
        elsewhere.id = there.clone();
        both.workspaces.push(elsewhere);
        store.seed_workspace_snapshot_for_test(both);
    }
    let (root, cx) = cx.add_window_view({
        let here = here.clone();
        move |window, cx| {
            let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
            root.sidebar
                .update(cx, |sidebar, cx| sidebar.activate_workspace(Some(here), cx));
            root
        }
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, _, cx| {
        assert_eq!(root.active_session_id(cx), Some(exiting_id.clone()));
    });

    {
        let mut store = runtime.store.write().unwrap();
        let mut record = store.sessions()[&exiting_id].as_ref().clone();
        record.status = exited(0);
        store.upsert_session(record);
    }
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        assert_eq!(root.active_workspace, Some(there.clone()));
        assert_eq!(root.active_session_id(cx), Some(other.clone()));
        assert!(typing_reaches_active_terminal(root, window, cx));
    });
}

/// The ended session's layout is already gone when its exit arrives (removed
/// from elsewhere in the same batch, or it was the layout's last tab). The
/// window still moves on to a live session in another layout.
#[gpui::test]
fn a_session_whose_layout_went_with_it_still_hands_over(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let exiting = shell(&fixture.list.sessions[0], "last-shell");
    let exiting_id = exiting.id.clone();
    let other = fixture.list.sessions[1].id.clone();
    let here = WorkspaceId::new("focus-workspace");
    let there = WorkspaceId::new("other-workspace");
    let layouts = |here_tabs: Vec<WorkspaceTab>| {
        let selected = here_tabs.first().map(|tab| tab.id.clone());
        let mut both = snapshot(1, here_tabs, &tab("last", &exiting_id));
        both.workspaces[0].selected_tab = selected;
        let mut elsewhere = snapshot(1, vec![tab("other", &other)], &tab("other", &other))
            .workspaces
            .remove(0);
        elsewhere.id = there.clone();
        both.workspaces.push(elsewhere);
        both
    };
    {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.upsert_session(exiting);
        store.seed_workspace_snapshot_for_test(layouts(vec![tab("last", &exiting_id)]));
    }
    let (root, cx) = cx.add_window_view({
        let here = here.clone();
        move |window, cx| {
            let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
            root.sidebar
                .update(cx, |sidebar, cx| sidebar.activate_workspace(Some(here), cx));
            root
        }
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, _, cx| {
        assert_eq!(root.active_session_id(cx), Some(exiting_id.clone()));
    });

    {
        let mut store = runtime.store.write().unwrap();
        let mut record = store.sessions()[&exiting_id].as_ref().clone();
        record.status = exited(0);
        store.upsert_session(record);
        store.seed_workspace_snapshot_for_test(layouts(Vec::new()));
    }
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, _, cx| {
        assert_eq!(root.active_workspace, Some(there.clone()));
        assert_eq!(root.active_session_id(cx), Some(other.clone()));
    });
}

/// While the window leads the Engine to the next session, the user picks
/// another tab. Their choice stands: the window does not reselect the one it
/// moved to.
#[gpui::test]
fn choosing_another_tab_while_moving_on_is_not_undone(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let exiting = shell(&fixture.list.sessions[0], "exiting-shell");
    let chosen = shell(&fixture.list.sessions[0], "chosen-shell");
    let exiting_id = exiting.id.clone();
    let chosen_id = chosen.id.clone();
    let previous = {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.upsert_session(exiting);
        store.upsert_session(chosen);
        store
            .ordered_sessions()
            .into_iter()
            .find(|session| {
                session.id != exiting_id && session.id != chosen_id && !session.is_archived()
            })
            .unwrap()
            .id
    };
    let previous_tab = tab("previous", &previous);
    let exiting_tab = tab("exiting", &exiting_id);
    let chosen_tab = tab("chosen", &chosen_id);
    let tabs = || {
        vec![
            previous_tab.clone(),
            exiting_tab.clone(),
            chosen_tab.clone(),
        ]
    };
    runtime
        .store
        .write()
        .unwrap()
        .seed_workspace_snapshot_for_test(snapshot(1, tabs(), &exiting_tab));
    let (root, cx) = cx.add_window_view({
        let previous = previous.clone();
        let exiting = exiting_id.clone();
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

    {
        let mut store = runtime.store.write().unwrap();
        let mut record = store.sessions()[&exiting_id].as_ref().clone();
        record.status = exited(0);
        store.upsert_session(record);
    }
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, _, cx| {
        assert_eq!(root.active_session_id(cx), Some(previous.clone()));
    });

    // The Engine moved on without taking the edit, and before the window
    // tries again the user picks a tab.
    {
        let mut store = runtime.store.write().unwrap();
        store.finish_workspace_edit_for_test(snapshot(2, tabs(), &exiting_tab));
        assert!(store.edit_workspace(WorkspaceMutation::SelectTab {
            workspace_id: WorkspaceId::new("focus-workspace"),
            tab_id: chosen_tab.id.clone(),
        }));
    }
    runtime.publish_local_change();
    cx.run_until_parked();
    runtime
        .store
        .write()
        .unwrap()
        .finish_workspace_edit_for_test(snapshot(3, tabs(), &chosen_tab));
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, _, cx| {
        assert_eq!(root.active_session_id(cx), Some(chosen_id.clone()));
        assert!(root.pending_tab.is_none());
    });
    assert!(
        runtime.store.read().unwrap().workspace_catalog().can_edit(),
        "the window sent nothing more"
    );
}

/// Moving on is for a session that ends while on screen. Opening one that
/// already ended, to read its last screen or resume it, stays on it.
#[gpui::test]
fn opening_a_session_that_already_ended_stays_on_it(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| commands::bind_keys(cx, &Default::default()));
    let services = test_services();
    let runtime = services.store.clone();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let mut crashed = claude(&fixture.list.sessions[0], "crashed-earlier");
    crashed.status = exited(1);
    let ended = crashed.id.clone();
    let other = fixture.list.sessions[1].id.clone();
    {
        let mut store = runtime.store.write().unwrap();
        store.hydrate(fixture.list);
        store.upsert_session(crashed);
        let ended_tab = tab("ended", &ended);
        store.seed_workspace_snapshot_for_test(snapshot(
            1,
            vec![tab("other", &other), ended_tab.clone()],
            &ended_tab,
        ));
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
    runtime.publish_local_change();
    cx.run_until_parked();
    root.update_in(cx, |root, _, cx| {
        assert_eq!(root.active_session_id(cx), Some(ended.clone()));
        assert!(root.pending_tab.is_none());
        assert!(root.toast.current().is_none());
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
