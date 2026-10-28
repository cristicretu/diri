//! Renders the split directions as stills, because an agent session cannot
//! record the screen.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/splits \
//!   cargo test -p diri-app --bin diri -- --ignored render_split_frames
//! ```
//!
//! Writes `1-one-pane.png` (a wide Claude pane), `2-best-fit-wide.png` (⌘↩
//! adds a column), `3-best-fit-tall.png` (⌘↩ on the now tall shell adds a
//! row), `4-left-unbalanced.png` (Split Left twice, dividers left where
//! each split put them) and `5-left-balanced.png` (the same with "Even out
//! panes" on). Each Best Fit layout is the one `best_fit_edge` picks from
//! the pane's drawn rect, asserted before it is drawn.

use std::time::Duration;

use diri_proto::workspace::*;
use gpui::{HeadlessAppContext, size};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use crate::workspace_workbench::best_fit_edge;

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

fn snapshot(layout: LayoutNode, focused: &str, revision: u64) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        revision,
        workspaces: vec![WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new("project-workspace"),
            name: "diri".into(),
            selected_tab: Some(TabId::new("release")),
            tabs: vec![WorkspaceTab {
                id: TabId::new("release"),
                title: None,
                focused_pane: PaneId::new(focused),
                zoomed_pane: None,
                layout,
            }],
        }],
        ..Default::default()
    }
}

const CLAUDE: &str = "> Prepare the signed 0.4.4 release\n\n● Bumping the version and changelog.\n\n  Update(Cargo.toml)\n  ⎿  version = \"0.4.4\"\n\n● The tag needs network access to publish.";
const TESTS: &str = "~/work/diri $ cargo test -p diri-engine\n    Finished test profile in 9.81s\n\ntest result: ok. 36 passed; 0 failed\n~/work/diri $ ";
const SERVER: &str = "~/work/diri/website $ pnpm dev\n\n  VITE v6.0.3  ready in 412 ms\n  ➜  Local:   http://localhost:3000/";

#[test]
#[ignore = "writes split layouts, one PNG each, to the DIRI_VISUAL_OUTPUT directory"]
fn render_split_frames() {
    let output = std::path::PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    std::fs::create_dir_all(&output).unwrap();

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
    let store = services.store.clone();
    {
        let mut fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let shell = fixture
            .list
            .sessions
            .iter()
            .find(|session| session.id.0 == "preview-shell")
            .unwrap()
            .clone();
        for (id, title) in [("pane-tests", "cargo test"), ("pane-server", "pnpm dev")] {
            let mut pane = shell.clone();
            pane.id = SessionId::new(id);
            pane.title = title.into();
            pane.parent = Some(SessionId::new("preview-claude"));
            pane.listening_ports = None;
            fixture.list.sessions.push(pane);
        }
        let mut store = store.store.write().unwrap();
        store.hydrate(fixture.list);
        store
            .update_preferences(|prefs| {
                prefs.terminal_theme = "dirijor-dark".into();
                prefs.sidebar_visible = true;
            })
            .unwrap();
        store.select(SessionId::new("preview-claude"));
        store.seed_workspace_snapshot_for_test(snapshot(pane("agent", "preview-claude"), "agent", 1));
    }
    let workspace = WorkspaceId::new("project-workspace");
    let window = cx
        .open_window(size(px(1280.0), px(780.0)), |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                root.sidebar
                    .update(cx, |sidebar, cx| sidebar.activate_workspace(Some(workspace), cx));
                root
            })
        })
        .unwrap();
    cx.run_until_parked();

    let settle = |cx: &mut HeadlessAppContext| {
        // Draw once so a pane the layout just added is mounted to seed.
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
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
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(120));
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
    };
    let rect = |cx: &mut HeadlessAppContext, pane: &str| {
        cx.update_window(window.into(), |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            root.read(cx)
                .workspace_workbench
                .as_ref()
                .unwrap()
                .read(cx)
                .pane_window_rect_for_test(&PaneId::new(pane))
                .unwrap()
        })
        .unwrap()
    };
    let save = |cx: &mut HeadlessAppContext, name: &str| {
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output.join(format!("{name}.png")))
            .unwrap();
    };
    let mut revision = 1;
    let mut show = |cx: &mut HeadlessAppContext, layout: LayoutNode, focused: &str| {
        revision += 1;
        store
            .store
            .write()
            .unwrap()
            .finish_workspace_refresh_for_test(snapshot(layout, focused, revision));
        cx.run_until_parked();
    };

    settle(&mut cx);
    save(&mut cx, "1-one-pane");

    // ⌘↩ on the wide Claude pane.
    assert_eq!(best_fit_edge(rect(&mut cx, "agent")), DockEdge::Right);
    let columns = |second: LayoutNode| {
        split(
            "columns",
            LayoutAxis::Horizontal,
            0.5,
            pane("agent", "preview-claude"),
            second,
        )
    };
    show(&mut cx, columns(pane("tests", "pane-tests")), "tests");
    settle(&mut cx);
    save(&mut cx, "2-best-fit-wide");

    // ⌘↩ again on the new shell, now taller than wide.
    assert_eq!(best_fit_edge(rect(&mut cx, "tests")), DockEdge::Bottom);
    show(
        &mut cx,
        columns(split(
            "rows",
            LayoutAxis::Vertical,
            0.5,
            pane("tests", "pane-tests"),
            pane("server", "pane-server"),
        )),
        "server",
    );
    settle(&mut cx);
    save(&mut cx, "3-best-fit-tall");

    // Split Left twice from Claude: each split halves the pane it cut.
    let left = |outer: f32| {
        split(
            "outer",
            LayoutAxis::Horizontal,
            outer,
            pane("tests", "pane-tests"),
            split(
                "inner",
                LayoutAxis::Horizontal,
                0.5,
                pane("server", "pane-server"),
                pane("agent", "preview-claude"),
            ),
        )
    };
    let uneven = left(0.5);
    assert!(!uneven.is_balanced());
    show(&mut cx, uneven, "tests");
    settle(&mut cx);
    save(&mut cx, "4-left-unbalanced");

    let even = left(1.0 / 3.0);
    assert!(even.is_balanced());
    show(&mut cx, even, "tests");
    settle(&mut cx);
    save(&mut cx, "5-left-balanced");

    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
