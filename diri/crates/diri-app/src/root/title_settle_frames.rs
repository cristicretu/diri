//! Renders agent renames frame by frame against a stepped clock, because an
//! agent session cannot record the screen. `docs/screenshots/settling-titles`
//! was made with it.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/frames \
//!   cargo test -p diri-app --bin diri -- --ignored render_title_settle_frames
//! ```
//!
//! `DIRI_TITLES_HORIZONTAL=1` renders the tab strip instead of the sidebar,
//! `DIRI_TITLES_INSTANT=1` files every rename as the user's own, which
//! commits at once: what a build without the settle paints.

use std::time::Duration;

use diri_proto::TitleSource;
use gpui::{HeadlessAppContext, size};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;

/// When each rename lands, which session it names, and the new title.
const SCRIPT: [(u64, &str, &str); 4] = [
    (300, "preview-spawned-deep", "Check the rail geometry"),
    (1100, "preview-cursor", "Fix tab focus after close"),
    (1900, "preview-spawned-review", "Tighten tests"),
    // Two renames inside one fade: the second starts from what is on screen.
    (1960, "preview-spawned-review", "Audit the fixtures"),
];

#[test]
#[ignore = "writes agent renames, one PNG per frame, to the DIRI_VISUAL_OUTPUT directory"]
fn render_title_settle_frames() {
    let output = std::path::PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let horizontal = std::env::var_os("DIRI_TITLES_HORIZONTAL").is_some();
    let instant = std::env::var_os("DIRI_TITLES_INSTANT").is_some();
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
        // One session has not been named yet, as a fresh agent is not.
        let mut unnamed = (**store
            .sessions()
            .get(&SessionId::new("preview-spawned-deep"))
            .unwrap())
        .clone();
        unnamed.title = "codex".into();
        unnamed.title_source = TitleSource::Placeholder;
        store.upsert_session(unnamed);
        let mut refined = (**store
            .sessions()
            .get(&SessionId::new("preview-cursor"))
            .unwrap())
        .clone();
        refined.title = "Fix tab focus".into();
        store.upsert_session(refined);
        // The strip shows the selected session's project.
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
    // finish so every captured frame differs only by the titles.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        root!(|_root, window, _cx| window.refresh());
        cx.run_until_parked();
    }

    let length = Duration::from_millis(SCRIPT[SCRIPT.len() - 1].0 + 700);
    let mut elapsed = Duration::ZERO;
    let mut applied = 0;
    let mut index = 0;
    while elapsed <= length {
        while let Some((at, id, title)) = SCRIPT.get(applied)
            && elapsed >= Duration::from_millis(*at)
        {
            applied += 1;
            let mut store = store.store.write().unwrap();
            let mut session = (**store.sessions().get(&SessionId::new(*id)).unwrap()).clone();
            session.title = (*title).into();
            session.title_source = if instant {
                TitleSource::UserRename
            } else {
                TitleSource::AgentProvided
            };
            store.upsert_session(session);
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
