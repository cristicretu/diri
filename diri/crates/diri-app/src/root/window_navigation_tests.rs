use super::*;
use diri_proto::SessionId;
use gpui::{HeadlessAppContext, size};
use std::time::{Duration, Instant};

#[test]
#[ignore = "native two-window navigation, real PTYs, optional screenshots and timing"]
fn all_sessions_windows_keep_terminal_inspector_and_mru_independent() {
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
    let services = fixture.services.clone();
    let first = cx
        .open_window(size(px(1040.0), px(720.0)), |window, cx| {
            cx.new(|cx| {
                RootView::new_with_selection(
                    services,
                    false,
                    PreviewScenario::Empty,
                    Some(None),
                    Some(Some(SessionId::new("build"))),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    cx.run_until_parked();
    let captured = cx
        .update_window(first.into(), |root, _, cx| {
            root.downcast::<RootView>()
                .unwrap()
                .read(cx)
                .window_session()
        })
        .unwrap();
    let services = fixture.services.clone();
    let second = cx
        .open_window(size(px(820.0), px(720.0)), |window, cx| {
            cx.new(|cx| {
                RootView::new_with_selection(
                    services,
                    false,
                    PreviewScenario::Empty,
                    Some(None),
                    Some(captured),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    let first_notifier = cx
        .update_window(first.into(), |root, _, cx| {
            root.downcast::<RootView>()
                .unwrap()
                .read(cx)
                .notifier
                .clone()
        })
        .unwrap();
    let second_notifier = cx
        .update_window(second.into(), |root, _, cx| {
            root.downcast::<RootView>()
                .unwrap()
                .read(cx)
                .notifier
                .clone()
        })
        .unwrap();
    assert!(std::rc::Rc::ptr_eq(&first_notifier, &second_notifier));
    let output = fixture.continuous_output();
    macro_rules! update {
        ($handle:expr,$body:expr) => {
            cx.update_window($handle.into(), |root, window, cx| {
                root.downcast::<RootView>()
                    .unwrap()
                    .update(cx, |root, cx| ($body)(root, window, cx))
            })
            .unwrap()
        };
    }
    macro_rules! assert_context {
        ($handle:expr,$id:expr) => {
            update!($handle, |root: &mut RootView,
                              _,
                              cx: &mut Context<RootView>| {
                let id = Some(SessionId::new($id));
                assert_eq!(root.window_session(), id);
                assert_eq!(
                    root.terminal
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .session_id_for_test(),
                    id
                );
                assert_eq!(
                    root.inspector
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .session_id_for_test(),
                    id
                );
            })
        };
    }
    update!(second, |root: &mut RootView,
                     window: &mut Window,
                     cx: &mut Context<RootView>| {
        window.activate_window();
        root.window_store
            .write()
            .unwrap()
            .select(SessionId::new("review"));
        root.sync_inspector_context(cx);
        root.terminal
            .as_ref()
            .unwrap()
            .update(cx, |pane, cx| pane.focus(window, cx));
    });
    for _ in 0..25 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_context!(first, "build");
    assert_context!(second, "review");
    assert_eq!(
        cx.update(|cx| TerminalPane::controller_counts_for_test(cx))
            .0,
        2
    );
    fixture.verify_process_identity();
    // Active shell output continues while this window changes selection. The
    // other window's terminal and inspector remain on their own session.
    let mut navigation_ms = Vec::new();
    let mut render_readback_ms = Vec::new();
    for index in 0..30 {
        let id = if index % 2 == 0 { "build" } else { "review" };
        let began = Instant::now();
        update!(
            second,
            |root: &mut RootView, window: &mut Window, cx: &mut Context<RootView>| {
                root.window_store
                    .write()
                    .unwrap()
                    .select(SessionId::new(id));
                root.sync_inspector_context(cx);
                root.terminal
                    .as_ref()
                    .unwrap()
                    .update(cx, |pane, cx| pane.focus(window, cx));
                cx.notify();
            }
        );
        let updated = Instant::now();
        cx.run_until_parked();
        let _ = cx.capture_screenshot(second.into()).unwrap();
        if index >= 5 {
            navigation_ms.push((updated - began).as_secs_f64() * 1000.0);
            render_readback_ms.push(updated.elapsed().as_secs_f64() * 1000.0);
        }
        assert_context!(first, "build");
        assert_context!(second, id);
    }
    fn percentile(values: &mut [f64], fraction: f64) -> f64 {
        values.sort_by(f64::total_cmp);
        values[((values.len() - 1) as f64 * fraction).round() as usize]
    }
    println!(
        "WINDOW_NAVIGATION active_output_hz=50 windows=2 sessions=2 samples={} navigation_p50_ms={:.3} navigation_p95_ms={:.3} render_readback_p50_ms={:.3} render_readback_p95_ms={:.3}",
        navigation_ms.len(),
        percentile(&mut navigation_ms, 0.5),
        percentile(&mut navigation_ms, 0.95),
        percentile(&mut render_readback_ms, 0.5),
        percentile(&mut render_readback_ms, 0.95)
    );
    if let Some(directory) = std::env::var_os("DIRI_WINDOW_SELECTION_SCREENSHOTS") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        cx.capture_screenshot(first.into())
            .unwrap()
            .save(directory.join("window-a-build.png"))
            .unwrap();
        cx.capture_screenshot(second.into())
            .unwrap()
            .save(directory.join("window-b-review.png"))
            .unwrap();
    }
    assert_eq!(
        cx.update(|cx| TerminalPane::controller_counts_for_test(cx))
            .0,
        2
    );
    fixture.verify_process_identity();
    cx.update_window(first.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_context!(second, "review");
    fixture.verify_process_identity();
    // The original window is gone. The application delegate keeps the exact
    // SessionId and routes to the remaining live window.
    let open = || crate::macos::notifier::NativeNotificationEvent::Open {
        session_id: "build".into(),
        notification_id: "fixture-alert".into(),
    };
    cx.update(|cx| {
        crate::application_notifications::route(
            open(),
            &fixture.services,
            false,
            PreviewScenario::Empty,
            cx,
        )
    });
    for _ in 0..25 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_context!(second, "build");
    drop(output);
    cx.update_window(second.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    fixture.verify_process_identity();
    // With no live Root the same application route explicitly creates one.
    cx.update(|cx| {
        crate::application_notifications::route(
            open(),
            &fixture.services,
            false,
            PreviewScenario::Empty,
            cx,
        )
    });
    for _ in 0..25 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    let reopened = cx.update(|cx| {
        let windows = cx.windows();
        assert_eq!(windows.len(), 1);
        windows[0]
    });
    assert_context!(reopened, "build");
    let reopened_notifier = cx
        .update_window(reopened, |root, _, cx| {
            root.downcast::<RootView>()
                .unwrap()
                .read(cx)
                .notifier
                .clone()
        })
        .unwrap();
    assert!(std::rc::Rc::ptr_eq(&first_notifier, &reopened_notifier));
    fixture.verify_process_identity();
    cx.update_window(reopened, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
