//! Renders sessions arriving in and leaving the sidebar frame by frame
//! against a stepped clock, because an agent session cannot record the
//! screen. `docs/screenshots/sidebar-row-motion` was made with it.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/frames \
//!   cargo test -p diri-app --bin diri -- --ignored render_row_motion_frames
//! ```
//!
//! `DIRI_ROWS_RECENCY=1` groups the sidebar by recency instead of project,
//! `DIRI_ROWS_HORIZONTAL=1` renders the tab strip instead of the sidebar.

use std::time::Duration;

use diri_proto::{SessionStatus, TitleSource};
use gpui::{HeadlessAppContext, size};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;

enum Step {
    /// A child agent the Codex session spawns.
    Spawn(&'static str, &'static str),
    Close(&'static str),
}

/// When each change lands.
const SCRIPT: [(u64, Step); 3] = [
    (
        300,
        Step::Spawn("preview-new-child", "Draft the release notes"),
    ),
    (1300, Step::Close("preview-claude")),
    (2300, Step::Close("preview-new-child")),
];

#[test]
#[ignore = "writes sessions arriving and leaving, one PNG per frame, to the DIRI_VISUAL_OUTPUT directory"]
fn render_row_motion_frames() {
    let output = std::path::PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let horizontal = std::env::var_os("DIRI_ROWS_HORIZONTAL").is_some();
    let recency = std::env::var_os("DIRI_ROWS_RECENCY").is_some();
    let frame = Duration::from_micros(1_000_000 / 60);
    std::fs::create_dir_all(&output).unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::fonts::init(cx));

    let services = test_services();
    let store = services.store.clone();
    {
        let mut store = store.store.write().unwrap();
        store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
        if recency {
            let _ = store.update_preferences(|prefs| {
                prefs.sidebar_grouping = crate::store::SidebarGrouping::Recency;
            });
        }
        store.select(SessionId::new("preview-codex"));
    }
    let window = cx
        .open_window(size(px(1100.0), px(720.0)), |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                root.sidebar.update(cx, |sidebar, cx| {
                    sidebar.use_manual_title_clock();
                    sidebar
                        .set_tab_orientation(
                            if horizontal {
                                crate::store::TabOrientation::Horizontal
                            } else {
                                crate::store::TabOrientation::Vertical
                            },
                            cx,
                        )
                        .unwrap();
                });
                root
            })
        })
        .unwrap();
    cx.run_until_parked();
    macro_rules! root {
        (|$root:ident, $window:ident, $cx:ident| $body:expr) => {
            cx.update_window(window.into(), |view, $window, $cx| {
                view.downcast::<RootView>()
                    .unwrap()
                    .update($cx, |$root, $cx| $body)
            })
            .unwrap()
        };
    }
    // Entrance motion elsewhere in the window runs on the wall clock. Let it
    // finish so every captured frame differs only by the rows.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        root!(|_root, window, _cx| window.refresh());
        cx.run_until_parked();
    }

    let length = Duration::from_millis(SCRIPT[SCRIPT.len() - 1].0 + 500);
    let mut elapsed = Duration::ZERO;
    let mut applied = 0;
    let mut index = 0;
    while elapsed <= length {
        while let Some((at, step)) = SCRIPT.get(applied)
            && elapsed >= Duration::from_millis(*at)
        {
            applied += 1;
            let mut store = store.store.write().unwrap();
            match step {
                Step::Spawn(id, title) => {
                    let mut session = (**store
                        .sessions()
                        .get(&SessionId::new("preview-spawned-review"))
                        .unwrap())
                    .clone();
                    session.id = SessionId::new(*id);
                    session.title = (*title).into();
                    session.title_source = TitleSource::AgentProvided;
                    session.status = SessionStatus::Idle;
                    session.parent = Some(SessionId::new("preview-codex"));
                    store.upsert_session(session);
                }
                Step::Close(id) => store.remove_sessions(vec![SessionId::new(*id)]),
            }
        }
        root!(|root, window, cx| {
            root.sidebar.update(cx, |_, cx| cx.notify());
            window.refresh();
        });
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output.join(format!("frame_{index:04}.png")))
            .unwrap();
        index += 1;
        elapsed += frame;
        crate::sidebar::title_clock_for_test::advance(frame);
    }
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
