use super::*;
use gpui::{HeadlessAppContext, size};

#[test]
#[ignore = "native project/agent navigation with disposable Engine, PTYs and screenshots"]
fn project_agents_remain_visible_and_open_preserved_layouts() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start();
    fixture
        .services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.terminal_theme = if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
                "dirijor-light"
            } else {
                "dirijor"
            }
            .into();
        })
        .unwrap();
    let driver = fixture.continuous_output();
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
        commands::bind_keys(cx, &Default::default());
    });
    let services = fixture.services.clone();
    let root = cx
        .open_window(size(px(1100.0), px(720.0)), |window, cx| {
            cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
        })
        .unwrap();
    macro_rules! update {
        ($body:expr) => {
            cx.update_window(root.into(), |view, window, cx| {
                view.downcast::<RootView>()
                    .unwrap()
                    .update(cx, |root, cx| ($body)(root, window, cx))
            })
            .unwrap()
        };
    }
    macro_rules! settle {
        () => {
            for _ in 0..25 {
                cx.advance_clock(Duration::from_millis(16));
                cx.update_window(root.into(), |_, window, cx| window.simulate_next_frame(cx))
                    .unwrap();
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(4));
            }
        };
    }
    macro_rules! wait {
        ($condition:expr) => {{
            let deadline = Instant::now() + Duration::from_secs(10);
            while !$condition {
                assert!(Instant::now() < deadline, "project navigation deadline");
                settle!();
            }
        }};
    }
    let snapshot = || {
        fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().workspaces())
            .unwrap()
    };
    let initial = snapshot();
    let original_layout = initial.workspaces[0].tabs[0].layout.clone();
    let original_tab = initial.workspaces[0].tabs[0].id.clone();
    let capture = |cx: &mut HeadlessAppContext, name: &str| {
        if let Some(directory) = std::env::var_os("DIRI_PROJECT_AGENT_SCREENSHOTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            cx.capture_screenshot(root.into())
                .unwrap()
                .save(directory.join(name))
                .unwrap();
        }
    };
    settle!();
    // These bounds only exist if the ordinary agent rows were actually painted.
    let point = update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        let sidebar = root.sidebar.read(cx);
        assert!(
            sidebar
                .project_agent_center_for_test(&SessionId::new("build"))
                .is_some()
        );
        sidebar
            .project_agent_center_for_test(&SessionId::new("review"))
            .expect("review agent remains visible in workspace")
    });
    cx.update_window(root.into(), |_, window, cx| {
        window.simulate_mouse_move(point, cx);
        window.dispatch_event(
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: MouseButton::Left,
                position: point,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: MouseButton::Left,
                position: point,
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
    })
    .unwrap();
    wait!(
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root.active_session_id(cx))
            == Some(SessionId::new("review"))
    );
    let after_click = snapshot();
    assert_eq!(after_click.workspaces.len(), 1);
    assert_eq!(after_click.workspaces[0].tabs[0].id, original_tab);
    assert_eq!(after_click.workspaces[0].tabs[0].layout, original_layout);
    fixture.verify_process_identity();
    settle!();
    capture(&mut cx, "project-agents-vertical.png");
    update!(
        |root: &mut RootView, _, cx: &mut Context<RootView>| root.sidebar.update(
            cx,
            |sidebar, cx| {
                sidebar
                    .set_tab_orientation(crate::store::TabOrientation::Horizontal, cx)
                    .unwrap();
                sidebar.reveal(cx);
            }
        )
    );
    settle!();
    capture(&mut cx, "project-agents-horizontal.png");
    // The Projects menu is anchored to the header and must not reveal the sidebar
    // or change the live terminal's geometry when opened or dismissed.
    update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root
        .sidebar
        .update(cx, |sidebar, cx| sidebar.conceal(cx)));
    settle!();
    let (picker, geometry) = update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        assert!(!root.sidebar.read(cx).is_visible());
        (
            root.sidebar
                .read(cx)
                .project_picker_center_for_test()
                .unwrap(),
            root.active_terminal(cx)
                .unwrap()
                .read(cx)
                .geometry_for_test()
                .0,
        )
    });
    cx.update_window(root.into(), |_, window, cx| {
        window.simulate_mouse_move(picker, cx);
        window.dispatch_event(
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: MouseButton::Left,
                position: picker,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: MouseButton::Left,
                position: picker,
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
    })
    .unwrap();
    settle!();
    update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        assert!(root.sidebar.read(cx).project_picker_is_open_for_test());
        assert!(!root.sidebar.read(cx).is_visible());
        assert!(!root.sidebar.read(cx).is_peeking());
        assert_eq!(
            root.active_terminal(cx)
                .unwrap()
                .read(cx)
                .geometry_for_test()
                .0,
            geometry
        );
    });
    capture(&mut cx, "projects-dropdown.png");
    cx.update_window(root.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("escape").unwrap(), cx);
    })
    .unwrap();
    settle!();
    update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
        assert!(!root.sidebar.read(cx).project_picker_is_open_for_test());
        assert!(!root.sidebar.read(cx).is_visible());
        assert_eq!(
            root.active_terminal(cx)
                .unwrap()
                .read(cx)
                .geometry_for_test()
                .0,
            geometry
        );
    });
    capture(&mut cx, "toolbar-open.png");
    for visible in [false, true] {
        cx.update_window(root.into(), |_, window, cx| {
            window.dispatch_keystroke(gpui::Keystroke::parse("cmd-b").unwrap(), cx);
        })
        .unwrap();
        settle!();
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| {
            assert_eq!(root.sidebar.read(cx).horizontal_tabs_visible(), visible);
            assert!(!root.sidebar.read(cx).is_visible());
            assert_eq!(root.active_session_id(cx), Some(SessionId::new("review")));
        });
        fixture.verify_process_identity();
        if !visible {
            capture(&mut cx, "toolbar-hidden.png");
        }
    }
    assert_eq!(snapshot().workspaces[0].tabs[0].layout, original_layout);
    // Leave the explicit layout, then use the same agent-first navigation.
    // The Engine adopts the one unambiguous project layout, preserving the split.
    update!(
        |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| {
            root.sidebar
                .update(cx, |sidebar, cx| sidebar.activate_workspace(None, cx));
            root.open_workspace_launch_session(SessionId::new("build"), window, cx);
        }
    );
    wait!(snapshot().workspaces[0].project_id.is_some());
    wait!(
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root.active_session_id(cx))
            == Some(SessionId::new("build"))
    );
    let adopted = snapshot();
    assert_eq!(adopted.workspaces.len(), 1);
    assert_eq!(adopted.workspaces[0].tabs[0].layout, original_layout);
    assert_eq!(adopted.workspaces[0].tabs[0].id, original_tab);
    fixture.verify_process_identity();
    assert!(driver.ticks() > 0);
    drop(driver);
    cx.update_window(root.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
