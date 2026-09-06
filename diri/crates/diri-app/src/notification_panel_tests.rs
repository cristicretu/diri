//! Native input and rendering coverage for the notification tray.
use super::tests::test_services;
use super::*;
use crate::sidebar::SidebarPreviewFixture;
use gpui::{Modifiers, size};

struct NotificationWheelHarness {
    root: Entity<RootView>,
    scrolls: Arc<std::sync::atomic::AtomicUsize>,
    _root_changed: Subscription,
}

impl Render for NotificationWheelHarness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scrolls = self.scrolls.clone();
        let panel = self
            .root
            .update(cx, |root, cx| root.notification_panel(window, cx));
        let root = self.root.clone();
        let colors = self
            .root
            .read(cx)
            .services
            .store
            .store
            .read()
            .map(|store| crate::app_theme::sidebar_colors(store.theme_id()))
            .unwrap();
        div()
            .size_full()
            .bg(colors.background)
            .on_key_down(move |event, window, cx| {
                root.update(cx, |root, cx| {
                    root.notification_key(event, window, cx);
                });
            })
            .child(div().absolute().inset_0().on_scroll_wheel(move |_, _, _| {
                scrolls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }))
            .children(panel)
    }
}

#[gpui::test]
fn notification_panel_contains_wheel_events(cx: &mut gpui::TestAppContext) {
    let scrolls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = scrolls.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let root = cx.new(|cx| {
            let mut root = RootView::new(test_services(), true, PreviewScenario::Empty, window, cx);
            root.notification_panel_open = true;
            root
        });
        let subscription = cx.observe(&root, |_, _, cx| cx.notify());
        NotificationWheelHarness {
            root,
            scrolls: probe,
            _root_changed: subscription,
        }
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    let panel = cx.debug_bounds("notification-panel").unwrap();
    for delta in [-40.0, 40.0] {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: panel.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(delta))),
            ..Default::default()
        });
    }
    assert_eq!(
        scrolls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "scrolling inside the notification panel must not reach content underneath"
    );
}

fn notification_services(count: usize) -> Arc<AppServices> {
    let services = test_services();
    let mut list = SidebarPreviewFixture::make(PreviewScenario::Typical).list;
    let base = list.sessions[0].clone();
    list.sessions.clear();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let titles = [
        "Summarize this week's customer interviews",
        "Refresh the launch checklist in Notion",
        "Polish the command palette",
        "Prepare the September release notes",
        "Compare the onboarding flows",
        "Review the new landing page",
        "Organize the design feedback",
        "Draft the team update",
    ];
    let mut store = services.store.store.write().unwrap();
    store.set_notification_surface_visible(false);
    for index in (0..count).rev() {
        let mut session = base.clone();
        session.id = SessionId::new(format!("notification-preview-{index}"));
        session.title = titles[index % titles.len()].into();
        session.status = if index % 4 == 1 {
            SessionStatus::NeedsInput(diri_proto::NeedsInputKind::Question)
        } else {
            SessionStatus::Idle
        };
        session.last_turn_completed_at = Some(diri_proto::DateMillis(
            now - (index + 1) as f64 * 3_600_000.0,
        ));
        session.last_seen_at = None;
        session.updated_at = session.last_turn_completed_at.unwrap();
        list.sessions.push(session);
        store.hydrate(list.clone());
    }
    assert_eq!(store.notifications().entries().len(), count);
    drop(store);
    services
}

#[gpui::test]
fn notification_list_scrolls_and_actions_stay_inside(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let services = notification_services(200);
    let store = services.store.clone();
    let (harness, cx) = cx.add_window_view(move |window, cx| {
        let root = cx.new(|cx| {
            let mut root = RootView::new(services, true, PreviewScenario::Empty, window, cx);
            root.notification_panel_open = true;
            root.notification_focus.focus(window, cx);
            root
        });
        let subscription = cx.observe(&root, |_, _, cx| cx.notify());
        NotificationWheelHarness {
            root,
            scrolls: Arc::default(),
            _root_changed: subscription,
        }
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    cx.run_until_parked();
    let root = harness.read_with(cx, |view, _| view.root.clone());
    assert!(cx.debug_bounds("notification-row-0").is_some());
    assert!(
        cx.debug_bounds("notification-row-20").is_none(),
        "offscreen rows should not be built"
    );
    let panel = cx.debug_bounds("notification-panel").unwrap();
    for delta in [-120.0, -100_000.0, -40.0, 100_000.0, 40.0] {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: panel.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(delta))),
            ..Default::default()
        });
        cx.run_until_parked();
        if delta == -120.0 {
            assert!(
                root.read_with(cx, |root, _| root
                    .notification_scroll
                    .0
                    .borrow()
                    .base_handle
                    .offset()
                    .y
                    < px(0.0)),
                "the inbox itself must scroll"
            );
        }
    }
    assert_eq!(
        harness.read_with(cx, |view, _| view
            .scrolls
            .load(std::sync::atomic::Ordering::Relaxed)),
        0
    );
    root.update_in(cx, |root, window, cx| {
        root.notification_focus.focus(window, cx)
    });
    for _ in 0..12 {
        cx.simulate_keystrokes("down");
    }
    cx.run_until_parked();
    assert_eq!(root.read_with(cx, |root, _| root.notification_selected), 12);
    assert!(cx.debug_bounds("notification-row-12").is_some());
    let row = cx.debug_bounds("notification-row-12").unwrap();
    assert!(row.bottom() <= panel.bottom());
    let selected_id = store.store.read().unwrap().notifications().entries()[12]
        .id
        .clone();
    let selected_session = store.store.read().unwrap().notifications().entries()[12]
        .session_id
        .clone();
    let button = cx.debug_bounds("notification-mute-12").unwrap().center();
    cx.simulate_click(button, Modifiers::default());
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .preferences()
            .muted_notification_sessions
            .contains(&selected_session.0)
    );
    assert!(root.read_with(cx, |root, _| root.notification_panel_open));
    let button = cx.debug_bounds("notification-read-12").unwrap().center();
    cx.simulate_click(button, Modifiers::default());
    cx.run_until_parked();
    assert!(
        store
            .store
            .read()
            .unwrap()
            .notifications()
            .entries()
            .iter()
            .find(|entry| entry.id == selected_id)
            .unwrap()
            .read
    );
    assert!(root.read_with(cx, |root, _| root.notification_panel_open));
    let button = cx.debug_bounds("notification-options").unwrap().center();
    cx.simulate_click(button, Modifiers::default());
    cx.run_until_parked();
    assert!(root.read_with(cx, |root, _| root.notification_options_open
        && root.notification_panel_open));
    let button = cx.debug_bounds("notification-read-all").unwrap().center();
    cx.simulate_click(button, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        store.store.read().unwrap().notifications().unread_count(),
        0
    );
    assert!(root.read_with(cx, |root, _| root.notification_panel_open));
    let button = cx.debug_bounds("notification-filter").unwrap().center();
    cx.simulate_click(button, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        root.read_with(cx, |root, _| root.notification_rows().len()),
        200
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!root.read_with(cx, |root, _| root.notification_panel_open));
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes the notification panel screenshot artifact"]
fn render_notification_panel_preview_screenshot() {
    let output = std::path::PathBuf::from(
        std::env::var_os("DIRI_VISUAL_OUTPUT").expect("set DIRI_VISUAL_OUTPUT"),
    );
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(true);
    });
    let services = notification_services(24);
    if std::env::var_os("DIRI_VISUAL_LIGHT").is_some() {
        services
            .store
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| prefs.terminal_theme = "dirijor-light".into())
            .unwrap();
    }
    let window = cx
        .open_window(size(px(480.0), px(520.0)), move |window, cx| {
            let root = cx.new(|cx| {
                let mut root = RootView::new(services, true, PreviewScenario::Empty, window, cx);
                root.notification_panel_open = true;
                root.notification_options_open = std::env::var_os("DIRI_VISUAL_OPTIONS").is_some();
                root
            });
            cx.new(|cx| {
                let subscription = cx.observe(&root, |_, _, cx| cx.notify());
                NotificationWheelHarness {
                    root,
                    scrolls: Arc::default(),
                    _root_changed: subscription,
                }
            })
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let screenshot = cx.capture_screenshot(window.into()).unwrap();
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    screenshot.save(output).unwrap();
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
