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
    let horizontal = std::env::var_os("DIRI_SPLIT_HORIZONTAL").is_some();
    for (name, density) in ["two", "three", "zoomed"]
        .into_iter()
        .flat_map(|name| crate::store::PaneDensity::ALL.map(|density| (name, density)))
    {
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
                    prefs.sidebar_visible = !horizontal;
                    prefs.pane_density = density;
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
                        if horizontal {
                            sidebar
                                .set_tab_orientation(crate::store::TabOrientation::Horizontal, cx)
                                .unwrap();
                        }
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
        let density = if density.islands() {
            "comfortable"
        } else {
            "compact"
        };
        image
            .save(output.join(format!("{name}-{density}.png")))
            .unwrap();
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

/// Which panes paint a fade, by selector, in paint order.
fn fades(cx: &mut gpui::VisualTestContext) -> Vec<&'static str> {
    [
        "pane-fade:key-inactive",
        "pane-fade:nonkey-active",
        "pane-fade:nonkey-inactive",
    ]
    .into_iter()
    .filter(|selector| cx.debug_bounds(selector).is_some())
    .collect()
}

fn set_window_key(root: &Entity<RootView>, key: bool, cx: &mut gpui::VisualTestContext) {
    if key {
        root.update_in(cx, |_, window, _| window.activate_window());
    } else {
        cx.deactivate_window();
    }
    cx.run_until_parked();
    root.update_in(cx, |_, window, _| assert_eq!(window.is_window_active(), key));
}

/// The key window marks only the panes typing does not reach; a window that
/// is not key fades every pane, keeping the focused one ahead.
#[gpui::test]
fn panes_fade_by_window_key_status_and_pane_focus(cx: &mut gpui::TestAppContext) {
    let (services, workspace) = stacked_tab("agent");
    let (root, cx) = cx.add_window_view(move |window, cx| {
        RootView::new(services, false, PreviewScenario::Empty, window, cx)
    });
    root.update_in(cx, |root, window, cx| {
        root.activate_saved_workspace(Some(workspace), window, cx)
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    set_window_key(&root, true, cx);
    assert_eq!(fades(cx), ["pane-fade:key-inactive"]);
    // The agent fills the top 60 %; the fade covers only the shell below.
    let faded = cx.debug_bounds("pane-fade:key-inactive").unwrap();
    assert!(
        faded.origin.y > px(400.0),
        "the focused pane stays at full strength: {faded:?}"
    );

    set_window_key(&root, false, cx);
    assert_eq!(
        fades(cx),
        ["pane-fade:nonkey-active", "pane-fade:nonkey-inactive"]
    );

    set_window_key(&root, true, cx);
    assert_eq!(fades(cx), ["pane-fade:key-inactive"]);
}

/// A tab with one pane has nothing to point at while its window is key, and
/// fades with the window otherwise; the same for a session outside any
/// workspace.
#[gpui::test]
fn a_lone_pane_fades_only_with_its_window(cx: &mut gpui::TestAppContext) {
    for in_workspace in [true, false] {
        let services = test_services();
        let workspace = WorkspaceId::new("project-workspace");
        {
            let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
            let mut store = services.store.store.write().unwrap();
            store.hydrate(fixture.list);
            store.select(SessionId::new("preview-claude"));
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
                        focused_pane: PaneId::new("agent"),
                        zoomed_pane: None,
                        layout: pane("agent", "preview-claude"),
                    }],
                }],
                ..Default::default()
            });
        }
        let (root, cx) = cx.add_window_view(move |window, cx| {
            RootView::new(services, false, PreviewScenario::Empty, window, cx)
        });
        if in_workspace {
            root.update_in(cx, |root, window, cx| {
                root.activate_saved_workspace(Some(workspace), window, cx)
            });
        }
        cx.simulate_resize(size(px(1100.0), px(800.0)));
        set_window_key(&root, true, cx);
        assert_eq!(fades(cx), Vec::<&str>::new(), "workspace: {in_workspace}");
        set_window_key(&root, false, cx);
        assert_eq!(
            fades(cx),
            ["pane-fade:nonkey-active"],
            "workspace: {in_workspace}"
        );
    }
}

#[test]
#[ignore = "writes key and non-key window renders to DIRI_FADE_SCREENSHOTS (a directory)"]
fn render_pane_fade_screenshots() {
    use gpui::HeadlessAppContext;
    let output = std::path::PathBuf::from(std::env::var("DIRI_FADE_SCREENSHOTS").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    let themes = ["dirijor-dark", "dirijor-light"];
    for theme in themes {
        for density in crate::store::PaneDensity::ALL {
            for key in [true, false] {
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
                fixture
                    .list
                    .sessions
                    .push(pane_shell("pane-tests", "cargo test", &fixture));
                let workspace = WorkspaceId::new("project-workspace");
                {
                    let mut store = services.store.store.write().unwrap();
                    store.hydrate(fixture.list);
                    store
                        .update_preferences(|prefs| {
                            prefs.terminal_theme = theme.into();
                            prefs.pane_density = density;
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
                            tabs: vec![WorkspaceTab {
                                id: TabId::new("release"),
                                title: None,
                                focused_pane: PaneId::new("agent"),
                                zoomed_pane: None,
                                layout: split(
                                    "columns",
                                    LayoutAxis::Horizontal,
                                    0.5,
                                    pane("agent", "preview-claude"),
                                    pane("tests", "pane-tests"),
                                ),
                            }],
                        }],
                        ..Default::default()
                    });
                }
                let window = cx
                    .open_window(size(px(1280.0), px(640.0)), |window, cx| {
                        cx.new(|cx| {
                            let root =
                                RootView::new(services, false, PreviewScenario::Empty, window, cx);
                            root.sidebar.update(cx, |sidebar, cx| {
                                sidebar.activate_workspace(Some(workspace), cx);
                            });
                            root
                        })
                    })
                    .unwrap();
                cx.run_until_parked();
                if key {
                    cx.update_window(window.into(), |_, window, _| window.activate_window())
                        .unwrap();
                    cx.run_until_parked();
                }
                cx.update_window(window.into(), |root, window, cx| {
                    assert_eq!(window.is_window_active(), key);
                    let root = root.downcast::<RootView>().unwrap();
                    let workbench = root.read(cx).workspace_workbench.clone().unwrap();
                    workbench.update(cx, |workbench, cx| {
                        workbench.seed_pane_text_for_test(
                            &[("preview-claude", CLAUDE), ("pane-tests", TESTS)],
                            cx,
                        )
                    });
                })
                .unwrap();
                cx.run_until_parked();
                let image = cx.capture_screenshot(window.into()).unwrap();
                let density = if density.islands() {
                    "comfortable"
                } else {
                    "compact"
                };
                let key = if key { "key" } else { "nonkey" };
                image
                    .save(output.join(format!("{theme}-{density}-{key}.png")))
                    .unwrap();
                cx.update_window(window.into(), |_, window, _| window.remove_window())
                    .unwrap();
                cx.run_until_parked();
            }
        }
    }
}

/// Renders the window key and not key under every interface style and
/// density, for the themes in `DIRI_CHROME_THEMES` (comma-separated, default
/// the two Diri themes), into `DIRI_CHROME_SCREENSHOTS`.
/// `DIRI_CHROME_HORIZONTAL=1` uses the horizontal tab strip.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes PNGs; run with DIRI_CHROME_SCREENSHOTS=<dir>"]
fn render_inactive_chrome_screenshots() {
    use crate::store::{InterfaceStyle, PaneDensity, TabOrientation};
    use gpui::HeadlessAppContext;
    let output = std::path::PathBuf::from(std::env::var("DIRI_CHROME_SCREENSHOTS").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    let themes = std::env::var("DIRI_CHROME_THEMES")
        .unwrap_or_else(|_| "dirijor-dark,dirijor-light".into());
    let horizontal = std::env::var("DIRI_CHROME_HORIZONTAL").is_ok_and(|value| value == "1");
    let styles: Vec<InterfaceStyle> = match std::env::var("DIRI_CHROME_STYLES") {
        Ok(list) => InterfaceStyle::ALL
            .into_iter()
            .filter(|style| list.split(',').any(|name| name.eq_ignore_ascii_case(&format!("{style:?}"))))
            .collect(),
        Err(_) => InterfaceStyle::ALL.to_vec(),
    };
    for theme in themes.split(',') {
        for &style in &styles {
            for density in PaneDensity::ALL {
                for key in [true, false] {
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
                    fixture
                        .list
                        .sessions
                        .push(pane_shell("pane-tests", "cargo test", &fixture));
                    let workspace = WorkspaceId::new("project-workspace");
                    {
                        let mut store = services.store.store.write().unwrap();
                        store.hydrate(fixture.list);
                        store
                            .update_preferences(|prefs| {
                                prefs.terminal_theme = theme.into();
                                prefs.pane_density = density;
                                prefs.interface_style = Some(style);
                                prefs.window_material = style.material();
                                if horizontal {
                                    prefs.tab_orientation = TabOrientation::Horizontal;
                                    prefs.sidebar_visible = false;
                                } else {
                                    prefs.tab_orientation = TabOrientation::Vertical;
                                    prefs.sidebar_visible = true;
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
                                tabs: vec![WorkspaceTab {
                                    id: TabId::new("release"),
                                    title: None,
                                    focused_pane: PaneId::new("agent"),
                                    zoomed_pane: None,
                                    layout: split(
                                        "columns",
                                        LayoutAxis::Horizontal,
                                        0.5,
                                        pane("agent", "preview-claude"),
                                        pane("tests", "pane-tests"),
                                    ),
                                }],
                            }],
                            ..Default::default()
                        });
                    }
                    let window = cx
                        .open_window(size(px(1280.0), px(640.0)), |window, cx| {
                            cx.new(|cx| {
                                let root = RootView::new(
                                    services,
                                    false,
                                    PreviewScenario::Empty,
                                    window,
                                    cx,
                                );
                                root.sidebar.update(cx, |sidebar, cx| {
                                    sidebar.activate_workspace(Some(workspace), cx);
                                });
                                root
                            })
                        })
                        .unwrap();
                    cx.run_until_parked();
                    if key {
                        cx.update_window(window.into(), |_, window, _| window.activate_window())
                            .unwrap();
                        cx.run_until_parked();
                    }
                    cx.update_window(window.into(), |root, window, cx| {
                        assert_eq!(window.is_window_active(), key);
                        let root = root.downcast::<RootView>().unwrap();
                        let workbench = root.read(cx).workspace_workbench.clone().unwrap();
                        workbench.update(cx, |workbench, cx| {
                            workbench.seed_pane_text_for_test(
                                &[("preview-claude", CLAUDE), ("pane-tests", TESTS)],
                                cx,
                            )
                        });
                    })
                    .unwrap();
                    cx.run_until_parked();
                    let image = cx.capture_screenshot(window.into()).unwrap();
                    let density = if density.islands() {
                        "comfortable"
                    } else {
                        "compact"
                    };
                    let key = if key { "key" } else { "nonkey" };
                    let style = format!("{style:?}").to_lowercase();
                    let orientation = if horizontal { "-strip" } else { "" };
                    image
                        .save(output.join(format!(
                            "{theme}-{style}-{density}{orientation}-{key}.png"
                        )))
                        .unwrap();
                    cx.update_window(window.into(), |_, window, _| window.remove_window())
                        .unwrap();
                    cx.run_until_parked();
                }
            }
        }
    }
}
