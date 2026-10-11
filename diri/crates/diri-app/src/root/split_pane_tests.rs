//! A tab is a split tree of panes: an Agent plus the shells it owns, shown
//! as one sidebar row. Headless screenshots of the layouts the PR shows.
use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use crate::commands::CommandId;
use diri_proto::workspace::*;
use gpui::size;

/// The Release tab: Claude, with two terminal panes it owns.
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

fn pane(id: &str, session: &str) -> LayoutNode {
    LayoutNode::Pane {
        id: PaneId::new(id),
        session_id: SessionId::new(session),
    }
}

fn split(
    id: &str,
    axis: LayoutAxis,
    fraction: f32,
    first: LayoutNode,
    second: LayoutNode,
) -> LayoutNode {
    LayoutNode::Split {
        id: SplitId::new(id),
        axis,
        fraction,
        first: Box::new(first),
        second: Box::new(second),
    }
}

/// `layout` names: "two" (agent | shell), "three" (agent | shell over
/// shell), "zoomed" (three, with the agent zoomed).
fn layout(name: &str) -> (LayoutNode, PaneId, Option<PaneId>) {
    let agent = pane("agent", "preview-claude");
    match name {
        "two" => (
            split(
                "columns",
                LayoutAxis::Horizontal,
                0.5,
                agent,
                pane("tests", "pane-tests"),
            ),
            PaneId::new("tests"),
            None,
        ),
        "three" | "zoomed" => {
            let tree = split(
                "columns",
                LayoutAxis::Horizontal,
                0.58,
                agent,
                split(
                    "stack",
                    LayoutAxis::Vertical,
                    0.55,
                    pane("tests", "pane-tests"),
                    pane("server", "pane-server"),
                ),
            );
            if name == "zoomed" {
                (tree, PaneId::new("agent"), Some(PaneId::new("agent")))
            } else {
                (tree, PaneId::new("tests"), None)
            }
        }
        other => panic!("unknown layout {other}"),
    }
}

#[test]
#[ignore = "writes split pane screenshots to DIRI_SPLIT_SCREENSHOTS (a directory)"]
fn render_split_panes_screenshot() {
    use gpui::HeadlessAppContext;
    let output = std::path::PathBuf::from(std::env::var("DIRI_SPLIT_SCREENSHOTS").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    for name in ["two", "three", "zoomed"] {
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let services = test_services();
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let tests = pane_shell("pane-tests", "cargo test", &fixture);
        let server = pane_shell("pane-server", "pnpm dev · website", &fixture);
        fixture.list.sessions.push(tests);
        fixture.list.sessions.push(server);
        let workspace = WorkspaceId::new("project-workspace");
        let (tree, focused, zoomed) = layout(name);
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
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 3,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "diri".into(),
                    selected_tab: Some(TabId::new("release")),
                    tabs: vec![
                        WorkspaceTab {
                            id: TabId::new("release"),
                            title: None,
                            focused_pane: focused,
                            zoomed_pane: zoomed,
                            layout: tree,
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
            });
        }
        let window = cx
            .open_window(size(px(1280.0), px(780.0)), |window, cx| {
                cx.new(|cx| {
                    let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                    root.sidebar.update(cx, |sidebar, cx| {
                        sidebar.activate_workspace(Some(workspace), cx);
                    });
                    root
                })
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |root, _, cx| {
            let root = root.downcast::<RootView>().unwrap();
            let workbench = root.read(cx).workspace_workbench.clone().unwrap();
            workbench.update(cx, |workbench, cx| {
                workbench.seed_pane_text_for_test(
                    &[
                        ("preview-claude", CLAUDE),
                        ("pane-tests", TESTS),
                        ("pane-server", SERVER),
                    ],
                    cx,
                )
            });
        })
        .unwrap();
        cx.run_until_parked();
        let image = cx.capture_screenshot(window.into()).unwrap();
        image.save(output.join(format!("{name}.png"))).unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
}

const CLAUDE: &str = "> Prepare the signed 0.4.4 release\n\n● Bumping the version and changelog.\n\n  Update(Cargo.toml)\n  ⎿  version = \"0.4.4\"\n\n  Update(CHANGELOG.md)\n  ⎿  Added 6 lines\n\n● The tag needs network access to publish.\n\n  Bash(git push origin v0.4.4)\n\n  Do you want to proceed?\n  ❯ 1. Yes\n    2. No, and tell Claude what to do differently";
const TESTS: &str = "~/work/diri $ cargo test -p diri-engine workspace\n   Compiling diri-engine v0.4.4\n    Finished test profile in 9.81s\n\nrunning 36 tests\ntest workspace::equalize ... ok\ntest workspace::prune ... ok\ntest workspace::placement ... ok\n\ntest result: ok. 36 passed; 0 failed\n~/work/diri $ ";
const SERVER: &str = "~/work/diri/website $ pnpm dev\n\n  VITE v6.0.3  ready in 412 ms\n\n  ➜  Local:   http://localhost:3000/\n  ➜  Network: use --host to expose\n";

/// Agent "preview-claude" over its shell "pane-tests"; `focused` names the
/// focused pane ("agent" or "tests").
fn stacked_tab(focused: &str) -> (Arc<AppServices>, WorkspaceId) {
    let services = test_services();
    let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    let shell = pane_shell("pane-tests", "cargo test", &fixture);
    fixture.list.sessions.push(shell);
    let workspace = WorkspaceId::new("project-workspace");
    {
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
            revision: 1,
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: workspace.clone(),
                name: "diri".into(),
                selected_tab: Some(TabId::new("release")),
                tabs: vec![WorkspaceTab {
                    id: TabId::new("release"),
                    title: None,
                    focused_pane: PaneId::new(focused),
                    zoomed_pane: None,
                    layout: split(
                        "stack",
                        LayoutAxis::Vertical,
                        0.6,
                        pane("agent", "preview-claude"),
                        pane("tests", "pane-tests"),
                    ),
                }],
            }],
            ..Default::default()
        });
    }
    (services, workspace)
}

#[gpui::test]
fn command_j_hides_the_terminal_below_and_command_w_closes_only_a_shell_pane(
    cx: &mut gpui::TestAppContext,
) {
    for focused in ["agent", "tests"] {
        let (services, workspace) = stacked_tab(focused);
        let runtime = services.store.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        root.update_in(cx, |root, window, cx| {
            root.activate_saved_workspace(Some(workspace), window, cx)
        });
        cx.simulate_resize(size(px(1100.0), px(800.0)));
        cx.run_until_parked();
        root.update_in(cx, |root, _, cx| {
            let workbench = root.workspace_workbench.clone().unwrap();
            // ⌘W: a shell pane closes on its own; the agent's pane is the tab.
            assert_eq!(
                workbench.read(cx).closable_pane_session(),
                (focused == "tests").then(|| SessionId::new("pane-tests")),
            );
        });
        // The sidebar keeps the tab's row selected, whichever pane has focus.
        assert_eq!(
            runtime
                .store
                .read()
                .unwrap()
                .pane_owner(&SessionId::new("pane-tests")),
            SessionId::new("preview-claude")
        );
        root.update_in(cx, |root, window, cx| {
            root.run_command(CommandId::ToggleAuxiliaryTerminal, window, cx)
        });
        // From the agent, ⌘J hides the terminal docked below it; from the
        // terminal, it hides itself. Either way the shell keeps running.
        let store = runtime.store.read().unwrap();
        assert_eq!(
            store.in_flight_workspace_edit_for_test(),
            Some(&WorkspaceMutation::RemovePane {
                tab_id: TabId::new("release"),
                pane_id: PaneId::new("tests"),
            })
        );
        assert!(store.is_open(&SessionId::new("pane-tests")));
    }
}

/// Settles the edit in flight as the Engine would: same layout, next
/// revision. Returns the edit.
fn settle(runtime: &crate::store::StoreRuntime) -> Option<WorkspaceMutation> {
    let mut store = runtime.store.write().unwrap();
    let edit = store.in_flight_workspace_edit_for_test().cloned();
    let mut snapshot = (*store.workspace_catalog().snapshot().unwrap()).clone();
    snapshot.revision += 1;
    store.finish_workspace_edit_for_test(snapshot);
    edit
}

/// Settles every edit the last keys set off, follow-ups included.
fn settle_all(runtime: &crate::store::StoreRuntime, cx: &mut gpui::VisualTestContext) {
    loop {
        cx.run_until_parked();
        if settle(runtime).is_none() {
            return;
        }
    }
}

#[gpui::test]
fn the_prefix_layer_runs_tmux_verbs_and_keeps_its_keys_from_the_terminal(
    cx: &mut gpui::TestAppContext,
) {
    let (services, workspace) = stacked_tab("tests");
    let runtime = services.store.clone();
    runtime
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| prefs.multiplexer_prefix = true)
        .unwrap();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    root.update_in(cx, |root, window, cx| {
        root.activate_saved_workspace(Some(workspace), window, cx)
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        let workbench = root.workspace_workbench.clone().unwrap();
        workbench.update(cx, |workbench, cx| workbench.focus(window, cx));
    });
    cx.run_until_parked();
    let release = TabId::new("release");

    // ⌃A arms the layer (and shows that it did); Space cycles layouts from
    // even-horizontal on.
    cx.simulate_keystrokes("ctrl-a");
    assert!(root.read_with(cx, |root, _| root.prefix.is_armed()));
    cx.simulate_keystrokes("space");
    assert!(root.read_with(cx, |root, _| !root.prefix.is_armed()));
    assert_eq!(
        settle(&runtime),
        Some(WorkspaceMutation::ApplyLayout {
            tab_id: release.clone(),
            preset: LayoutPreset::EvenHorizontal,
        })
    );
    cx.simulate_keystrokes("ctrl-a space");
    assert_eq!(
        settle(&runtime),
        Some(WorkspaceMutation::ApplyLayout {
            tab_id: release.clone(),
            preset: LayoutPreset::EvenVertical,
        })
    );

    // S synchronizes this tab's panes (both copy their typing); again
    // turns it off and no pane copies anything.
    let sync = |cx: &mut gpui::VisualTestContext| {
        root.read_with(cx, |root, cx| {
            let workbench = root.workspace_workbench.as_ref().unwrap().read(cx);
            (
                workbench.synchronized(),
                workbench.mirrored_panes_for_test(),
            )
        })
    };
    assert_eq!(sync(cx), (false, 0));
    cx.simulate_keystrokes("ctrl-a shift-s->S");
    assert_eq!(sync(cx), (true, 2));
    cx.simulate_keystrokes("ctrl-a shift-s->S");
    assert_eq!(sync(cx), (false, 0));

    // ? shows the cheatsheet; the next key only takes it down.
    cx.simulate_keystrokes("ctrl-a shift-/->?");
    assert!(root.read_with(cx, |root, _| root.multiplexer_sheet));
    cx.simulate_keystrokes("z");
    assert!(root.read_with(cx, |root, _| !root.multiplexer_sheet));
    assert!(
        !matches!(settle(&runtime), Some(WorkspaceMutation::ZoomPane { .. })),
        "z only closed the sheet"
    );
    settle_all(&runtime, cx);

    // ! breaks the focused pane into its own tab.
    cx.simulate_keystrokes("ctrl-a shift-1->!");
    assert_eq!(
        settle(&runtime),
        Some(WorkspaceMutation::BreakPane {
            tab_id: release.clone(),
            pane_id: PaneId::new("tests"),
        })
    );

    // q numbers the panes; a digit focuses one.
    cx.simulate_keystrokes("ctrl-a q");
    assert!(root.read_with(cx, |root, cx| {
        root.workspace_workbench
            .as_ref()
            .unwrap()
            .read(cx)
            .pane_numbers_shown()
    }));
    cx.simulate_keystrokes("1");
    cx.run_until_parked();
    root.read_with(cx, |root, cx| {
        let workbench = root.workspace_workbench.as_ref().unwrap().read(cx);
        assert!(!workbench.pane_numbers_shown());
        assert_eq!(
            workbench.focused_session_id(),
            Some(SessionId::new("preview-claude"))
        );
    });
    // Focus and the sidebar following it.
    settle_all(&runtime, cx);

    // x on the agent's pane never closes it: the agent's pane is the tab.
    cx.simulate_keystrokes("ctrl-a x");
    assert!(
        runtime
            .store
            .read()
            .unwrap()
            .is_open(&SessionId::new("preview-claude"))
    );

    // Off, ⌃A is the terminal's again.
    runtime
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| prefs.multiplexer_prefix = false)
        .unwrap();
    cx.simulate_keystrokes("ctrl-a");
    assert!(root.read_with(cx, |root, _| !root.prefix.is_armed()));
}

#[gpui::test]
fn x_closes_only_a_shell_pane(cx: &mut gpui::TestAppContext) {
    let (services, workspace) = stacked_tab("tests");
    let runtime = services.store.clone();
    runtime
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| prefs.multiplexer_prefix = true)
        .unwrap();
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    root.update_in(cx, |root, window, cx| {
        root.activate_saved_workspace(Some(workspace), window, cx)
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    root.update_in(cx, |root, window, cx| {
        let workbench = root.workspace_workbench.clone().unwrap();
        workbench.update(cx, |workbench, cx| workbench.focus(window, cx));
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-a x");
    let store = runtime.store.read().unwrap();
    assert!(!store.is_open(&SessionId::new("pane-tests")));
    assert!(store.is_open(&SessionId::new("preview-claude")));
}

const LOGS: &str = "~/work/diri $ tail -f target/release.log\n[12:04:11] signing diri.app\n[12:04:19] notarizing: submitted\n[12:05:02] notarizing: accepted\n[12:05:03] stapling ticket\n";

/// The PR's multiplexer screenshots: `name` picks the layout and what the
/// keys put on screen.
#[test]
#[ignore = "writes multiplexer screenshots to DIRI_MUX_SCREENSHOTS (a directory)"]
fn render_multiplexer_screenshot() {
    use gpui::HeadlessAppContext;
    let output = std::path::PathBuf::from(std::env::var("DIRI_MUX_SCREENSHOTS").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    for name in ["cheatsheet", "pane-numbers", "tiled", "sync", "tab-strip"] {
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            crate::fonts::init(cx);
            cx.set_reduce_motion(true);
        });
        let services = test_services();
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let mut tests = pane_shell("pane-tests", "cargo test", &fixture);
        // The strip folds a working pane into the tab's own state.
        tests.status = diri_proto::SessionStatus::Working;
        let server = pane_shell("pane-server", "pnpm dev · website", &fixture);
        let logs = pane_shell("pane-logs", "tail -f release.log", &fixture);
        fixture.list.sessions.extend([tests, server, logs]);
        let workspace = WorkspaceId::new("project-workspace");
        let agent = pane("agent", "preview-claude");
        let (tree, focused) = match name {
            "tiled" => (
                split(
                    "rows",
                    LayoutAxis::Vertical,
                    0.5,
                    split(
                        "top",
                        LayoutAxis::Horizontal,
                        0.5,
                        agent,
                        pane("tests", "pane-tests"),
                    ),
                    split(
                        "bottom",
                        LayoutAxis::Horizontal,
                        0.5,
                        pane("server", "pane-server"),
                        pane("logs", "pane-logs"),
                    ),
                ),
                PaneId::new("tests"),
            ),
            "sync" => (
                split(
                    "columns",
                    LayoutAxis::Horizontal,
                    0.5,
                    pane("tests", "pane-tests"),
                    pane("server", "pane-server"),
                ),
                PaneId::new("tests"),
            ),
            _ => {
                let (tree, focused, _) = layout("three");
                (tree, focused)
            }
        };
        {
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store
                .update_preferences(|prefs| {
                    prefs.terminal_theme = "dirijor-dark".into();
                    prefs.sidebar_visible = true;
                    prefs.multiplexer_prefix = true;
                    if name == "tab-strip" {
                        prefs.tab_orientation = crate::store::TabOrientation::Horizontal;
                    }
                })
                .unwrap();
            store.select(SessionId::new("preview-claude"));
            store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
                revision: 3,
                workspaces: vec![WorkspaceRecord {
                    project_id: None,
                    id: workspace.clone(),
                    name: "diri".into(),
                    selected_tab: Some(TabId::new("release")),
                    tabs: vec![
                        WorkspaceTab {
                            id: TabId::new("release"),
                            title: None,
                            focused_pane: focused,
                            zoomed_pane: None,
                            layout: tree,
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
            });
        }
        let window = cx
            .open_window(size(px(1280.0), px(780.0)), |window, cx| {
                cx.new(|cx| {
                    let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                    root.sidebar.update(cx, |sidebar, cx| {
                        sidebar.activate_workspace(Some(workspace), cx);
                    });
                    root
                })
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |root, window, cx| {
            let root = root.downcast::<RootView>().unwrap();
            let workbench = root.read(cx).workspace_workbench.clone().unwrap();
            workbench.update(cx, |workbench, cx| {
                workbench.seed_pane_text_for_test(
                    &[
                        ("preview-claude", CLAUDE),
                        ("pane-tests", TESTS),
                        ("pane-server", SERVER),
                        ("pane-logs", LOGS),
                    ],
                    cx,
                )
            });
            let command = match name {
                "cheatsheet" => Some(CommandId::ShowMultiplexerKeys),
                "pane-numbers" => Some(CommandId::ShowPaneNumbers),
                "sync" => Some(CommandId::ToggleSynchronizePanes),
                _ => None,
            };
            if let Some(command) = command {
                root.update(cx, |root, cx| root.run_command(command, window, cx));
            }
        })
        .unwrap();
        cx.run_until_parked();
        let image = cx.capture_screenshot(window.into()).unwrap();
        image.save(output.join(format!("{name}.png"))).unwrap();
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
}
