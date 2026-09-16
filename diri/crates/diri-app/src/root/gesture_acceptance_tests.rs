use super::*;
use crate::tab_peek::{GestureFrame, ThreeFingerGesture};
use diri_proto::workspace::WorkspaceMutation;
use gpui::{HeadlessAppContext, size};

#[test]
#[ignore = "native gesture transitions with disposable live PTYs and optional screenshots"]
fn live_gesture_orientation_cancel_and_saved_tab_selection() {
    let fixture = crate::workspace_fixture::LiveWorkspace::start();
    let client = fixture.services.store.client();
    for index in 0..7 {
        let snapshot = fixture
            .services
            .tokio
            .block_on(client.workspaces())
            .unwrap();
        fixture
            .services
            .tokio
            .block_on(
                client.mutate_workspace(&diri_proto::workspace::WorkspaceMutationParams {
                    expected_revision: snapshot.revision,
                    mutation: WorkspaceMutation::CreateTab {
                        select: false,
                        workspace_id: fixture.workspace.clone(),
                        session_id: SessionId::new(if index % 2 == 0 { "build" } else { "review" }),
                        title: Some(format!("Saved tab {}", index + 2)),
                    },
                }),
            )
            .unwrap();
    }
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
            prefs.tab_orientation = crate::store::TabOrientation::Horizontal;
        })
        .unwrap();
    let output = std::env::var_os("DIRI_GESTURE_LIVE_OUTPUT").map(|_| fixture.continuous_output());
    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        commands::bind_keys(cx, &Default::default());
    });
    let services = fixture.services.clone();
    let root = cx
        .open_window(size(px(1120.0), px(760.0)), |window, cx| {
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
    macro_rules! frame {
        ($frame:expr) => {
            update!(
                |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| root
                    .session_surfaces
                    .as_ref()
                    .unwrap()
                    .update(cx, |surface, cx| {
                        surface.tab_gesture($frame, cx);
                        surface.sync_tab_peek_focus(window, cx);
                    })
            );
            cx.run_until_parked();
        };
    }
    macro_rules! key {
        ($key:expr) => {
            cx.update_window(root.into(), |_, window, cx| {
                window.dispatch_keystroke(gpui::Keystroke::parse($key).unwrap(), cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
    }
    macro_rules! state {
        () => {
            update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root
                .session_surfaces
                .as_ref()
                .unwrap()
                .read(cx)
                .peek_state_for_test())
        };
    }
    macro_rules! geometry {
        () => {
            update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root
                .workspace_workbench
                .as_ref()
                .unwrap()
                .read(cx)
                .send_owned_fixture_input(cx))
        };
    }
    let save = |cx: &mut HeadlessAppContext, name: &str| {
        if let Some(path) = std::env::var_os("DIRI_GESTURE_SCREENSHOTS") {
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).unwrap();
            cx.capture_screenshot(root.into())
                .unwrap()
                .save(path.join(name))
                .unwrap();
        }
    };
    settle!();
    update!(
        |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| {
            window.activate_window();
            if let Some(terminal) = root.active_terminal(cx) {
                terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
            }
        }
    );
    settle!();
    if let Some(path) = std::env::var_os("DIRI_GESTURE_PROFILE") {
        super::gesture_schedule_profile::run(
            &mut cx,
            root,
            output
                .as_ref()
                .expect("profile requires DIRI_GESTURE_LIVE_OUTPUT=1"),
            std::path::Path::new(&path),
        );
        settle!();
    }
    let initial = fixture
        .services
        .tokio
        .block_on(client.workspaces())
        .unwrap();
    let original_buffers =
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root.preview_buffers(cx));
    let first_tab = initial.workspaces[0].selected_tab.clone().unwrap();
    let last_tab = initial.workspaces[0].tabs.last().unwrap().id.clone();
    for reduced in [false, true] {
        cx.update(|cx| cx.set_reduce_motion(reduced));
        let original_geometry = geometry!();
        assert_eq!(original_geometry.len(), 2);
        let mut recognizer = ThreeFingerGesture::default();
        let contacts = |y| (1..=3).map(|id| (id, 0.5, y)).collect();
        assert_eq!(recognizer.sample(contacts(0.7), false), None);
        for y in [0.66, 0.59, 0.5, 0.38] {
            frame!(recognizer.sample(contacts(y), false).unwrap());
            assert_eq!(
                geometry!(),
                original_geometry,
                "gesture alone changed settled geometry"
            );
        }
        frame!(recognizer.sample(vec![], false).unwrap());
        settle!();
        assert_eq!(state!().1, 1.0);
        assert_eq!(state!().3, 8);
        assert_eq!(
            fixture
                .services
                .tokio
                .block_on(client.workspaces())
                .unwrap()
                .workspaces[0]
                .selected_tab,
            Some(first_tab.clone())
        );
        // Hold delivery over a completed stroke and the next reverse stroke.
        // Its timestamps include enough elapsed time for the first release to
        // settle, even though the UI processes the samples in one update.
        frame!(GestureFrame::Tracking(-200.0));
        let began = update!(|_: &mut RootView, _, cx: &mut Context<RootView>| cx
            .background_executor()
            .now());
        let (sender, mut receiver) = crate::gesture_delivery::channel();
        for (millis, sample) in [
            (10, GestureFrame::Released(-200.0)),
            (300, GestureFrame::Tracking(-20.0)),
            (310, GestureFrame::Tracking(-60.0)),
            (320, GestureFrame::Released(-60.0)),
        ] {
            assert!(sender.send(sample, began + Duration::from_millis(millis)));
        }
        cx.advance_clock(Duration::from_millis(600));
        let batch = receiver.take_pending().unwrap();
        update!(
            |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| {
                root.session_surfaces
                    .as_ref()
                    .unwrap()
                    .update(cx, |surface, cx| {
                        for sample in batch.iter() {
                            surface.tab_gesture_at(sample.frame, sample.observed_at, cx);
                        }
                        surface.sync_tab_peek_focus(window, cx);
                    });
            }
        );
        settle!();
        assert!(state!().0);
        assert_eq!(state!().1, 0.0);
        assert_eq!(state!().2, Some(first_tab.0.clone()));
        assert_eq!(geometry!(), original_geometry);
        if !reduced {
            save(&mut cx, "delayed-strokes-peek.png");
        }
        frame!(GestureFrame::Tracking(240.0));
        frame!(GestureFrame::Released(240.0));
        settle!();
        assert_eq!(state!().1, 1.0);
        // A second upward stroke folds the same overview into the strip.
        recognizer.sample_with_reverse(contacts(0.3), false, true);
        frame!(
            recognizer
                .sample_with_reverse(contacts(0.5), false, true)
                .unwrap()
        );
        frame!(recognizer.sample_with_reverse(vec![], false, true).unwrap());
        settle!();
        assert!(state!().0);
        assert_eq!(state!().1, 0.0);
        assert_eq!(geometry!(), original_geometry);
        if !reduced {
            save(&mut cx, "horizontal-peek.png");
        }
        // Orientation changes are permitted while peek owns keyboard focus.
        key!("cmd-shift-s");
        settle!();
        assert_eq!(
            update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root
                .sidebar
                .read(cx)
                .tab_orientation()),
            crate::store::TabOrientation::Vertical
        );
        assert!(state!().0);
        let vertical_geometry = geometry!();
        assert_eq!(vertical_geometry.len(), 2);
        frame!(GestureFrame::Tracking(240.0));
        frame!(GestureFrame::Released(240.0));
        settle!();
        assert_eq!(state!().1, 1.0);
        assert_eq!(geometry!(), vertical_geometry);
        if !reduced {
            save(&mut cx, "vertical-overview.png");
        }
        // Escape cancels without changing durable selection or grid dimensions.
        key!("escape");
        settle!();
        assert!(!state!().0);
        assert_eq!(geometry!(), vertical_geometry);
        assert!(update!(
            |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| root
                .active_terminal(cx)
                .unwrap()
                .read(cx)
                .quote_focus_handle()
                .is_focused(window)
        ));
        fixture.verify_geometry(&vertical_geometry);
        key!("cmd-shift-s");
        settle!();
    }
    // Keyboard navigation must reach a card beyond the initial visible strip.
    cx.update(|cx| cx.set_reduce_motion(false));
    frame!(GestureFrame::Tracking(380.0));
    frame!(GestureFrame::Released(380.0));
    settle!();
    for _ in 0..7 {
        key!("right");
    }
    assert_eq!(state!().2, Some(last_tab.0.clone()));
    save(&mut cx, "last-tab-overview.png");
    frame!(GestureFrame::Tracking(-240.0));
    frame!(GestureFrame::Released(-240.0));
    settle!();
    assert_eq!(state!().1, 0.0);
    assert_eq!(state!().2, Some(last_tab.0.clone()));
    save(&mut cx, "last-tab-peek.png");
    key!("enter");
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture
        .services
        .tokio
        .block_on(client.workspaces())
        .unwrap()
        .workspaces[0]
        .selected_tab
        != Some(last_tab.clone())
    {
        assert!(Instant::now() < deadline);
        settle!();
    }
    settle!();
    assert!(!state!().0);
    assert!(update!(|root: &mut RootView,
                     window: &mut Window,
                     cx: &mut Context<RootView>| {
        root.active_terminal(cx)
            .unwrap()
            .read(cx)
            .quote_focus_handle()
            .is_focused(window)
    }));
    save(&mut cx, "selected-tab.png");
    let selected_buffers =
        update!(|root: &mut RootView, _, cx: &mut Context<RootView>| root.preview_buffers(cx));
    for (id, grid) in &original_buffers {
        assert!(
            selected_buffers
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(grid, current)),
            "selecting a saved card replaced the shared session grid"
        );
    }
    frame!(GestureFrame::Tracking(380.0));
    frame!(GestureFrame::Released(380.0));
    settle!();
    for _ in 0..7 {
        key!("left");
    }
    let point = update!(
        |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| root
            .session_surfaces
            .as_ref()
            .unwrap()
            .read(cx)
            .peek_card_center_for_test(0, window, cx)
    );
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture
        .services
        .tokio
        .block_on(client.workspaces())
        .unwrap()
        .workspaces[0]
        .selected_tab
        != Some(first_tab.clone())
    {
        assert!(
            Instant::now() < deadline,
            "click must activate its saved tab"
        );
        settle!();
    }
    settle!();
    assert!(!state!().0);
    assert!(update!(|root: &mut RootView,
                     window: &mut Window,
                     cx: &mut Context<RootView>| {
        root.active_terminal(cx)
            .unwrap()
            .read(cx)
            .quote_focus_handle()
            .is_focused(window)
    }));
    fixture.verify_process_identity();
    if let Some(output) = &output {
        assert!(output.ticks() > 0);
    }
    drop(output);
    cx.update_window(root.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
