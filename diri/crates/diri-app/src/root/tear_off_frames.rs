//! Renders a sidebar row torn off into its own window, because an agent
//! session cannot record the screen.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/tearoff \
//!   cargo test -p diri-app --bin diri -- --ignored render_tear_off_frames
//! ```
//!
//! Writes `1-carried.png` (the Claude row carried to the window's right
//! edge), `2-source.png` (the source window after the release outside it)
//! and `3-new-window.png` (the window opened where it was let go).

use diri_proto::workspace::*;
use gpui::{
    AnyWindowHandle, HeadlessAppContext, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, point, size,
};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;

const CLAUDE: &str = "> Prepare the signed 0.4.4 release\n\n● Bumping the version and changelog.\n\n  Update(Cargo.toml)\n  ⎿  version = \"0.4.4\"\n\n● The tag needs network access to publish.";
const AUDIT: &str = "› Audit hydration-safe inputs\n\n• Reading src/forms/Field.tsx\n• The value is set before hydration finishes.";
const CODEX: &str = "› Fix the flaky reconnect test\n\n• Reading crates/diri-engine/tests/wake.rs\n• The retry races the holder's exit marker.";

fn grid(text: &str) -> diri_term::buffer::GridBuffer {
    let lines: Vec<_> = text.lines().collect();
    let mut grid = diri_term::buffer::GridBuffer::new(100, lines.len() as u16 + 1);
    for (y, line) in lines.iter().enumerate() {
        for (x, ch) in line.chars().take(100).enumerate() {
            grid.cells[y * 100 + x].scalar = ch as u32;
        }
    }
    grid
}

#[test]
#[ignore = "writes a tab tear-off, one PNG per state, to the DIRI_VISUAL_OUTPUT directory"]
fn render_tear_off_frames() {
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
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store
            .update_preferences(|prefs| {
                prefs.terminal_theme = "dirijor-dark".into();
                prefs.sidebar_visible = true;
            })
            .unwrap();
        store.select(SessionId::new("preview-claude"));
        let tab = |id: &str, session: &str| WorkspaceTab {
            id: TabId::new(id),
            title: None,
            focused_pane: PaneId::new(id),
            zoomed_pane: None,
            layout: LayoutNode::Pane {
                id: PaneId::new(id),
                session_id: SessionId::new(session),
            },
        };
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
            revision: 1,
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: WorkspaceId::new("project-workspace"),
                name: "diri".into(),
                selected_tab: Some(TabId::new("release")),
                tabs: vec![
                    tab("release", "preview-claude"),
                    tab("codex", "preview-codex"),
                ],
            }],
            ..Default::default()
        });
    }
    let workspace = WorkspaceId::new("project-workspace");
    let first = cx
        .open_window(size(px(1100.0), px(700.0)), |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                root.sidebar
                    .update(cx, |sidebar, cx| sidebar.activate_workspace(Some(workspace), cx));
                root
            })
        })
        .unwrap();
    cx.run_until_parked();
    let settle = |cx: &mut HeadlessAppContext, handle: AnyWindowHandle| {
        for _ in 0..3 {
            cx.update_window(handle, |view, window, cx| {
                let root = view.downcast::<RootView>().unwrap();
                let (workbench, terminal) = {
                    let root = root.read(cx);
                    (root.workspace_workbench.clone(), root.terminal.clone())
                };
                if let Some(workbench) = workbench {
                    workbench.update(cx, |workbench, cx| {
                        workbench.seed_pane_text_for_test(
                            &[
                                ("preview-claude", CLAUDE),
                                ("preview-codex", CODEX),
                                ("preview-sleeping", AUDIT),
                            ],
                            cx,
                        )
                    });
                }
                if let Some(terminal) = terminal {
                    terminal.update(cx, |terminal, cx| {
                        let text = match terminal.session_id_for_test() {
                            Some(id) if id.0 == "preview-codex" => CODEX,
                            Some(id) if id.0 == "preview-sleeping" => AUDIT,
                            _ => CLAUDE,
                        };
                        terminal.seed_preview_grid_for_test(grid(text), cx)
                    });
                }
                window.refresh();
            })
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(120));
            cx.run_until_parked();
        }
    };
    let save = |cx: &mut HeadlessAppContext, handle: AnyWindowHandle, name: &str| {
        cx.capture_screenshot(handle)
            .unwrap()
            .save(output.join(format!("{name}.png")))
            .unwrap();
    };
    let send = |cx: &mut HeadlessAppContext, input: PlatformInput| {
        cx.update_window(first.into(), |_, window, cx| window.dispatch_event(input, cx))
            .unwrap();
        cx.run_until_parked();
    };
    settle(&mut cx, first.into());
    let row = cx
        .update_window(first.into(), |view, _, cx| {
            view.downcast::<RootView>()
                .unwrap()
                .read(cx)
                .sidebar
                .read(cx)
                .project_agent_center_for_test(&SessionId::new("preview-claude"))
                .unwrap()
        })
        .unwrap();
    let moved = |position| {
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::none(),
        })
    };
    send(
        &mut cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: row,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        }),
    );
    send(&mut cx, moved(row + point(px(10.0), px(4.0))));
    let edge = point(px(1040.0), row.y + px(120.0));
    send(&mut cx, moved(edge));
    save(&mut cx, first.into(), "1-carried");
    let outside = point(px(1260.0), row.y + px(160.0));
    send(&mut cx, moved(outside));
    send(
        &mut cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: outside,
            modifiers: Modifiers::none(),
            click_count: 1,
        }),
    );
    // The Engine's answer to the source opening its next tab: that tab's
    // project workspace, selected.
    {
        let mut store = store.store.write().unwrap();
        if let Some(WorkspaceMutation::OpenProjectAgent { session_id, .. }) =
            store.in_flight_workspace_edit_for_test().cloned()
        {
            let project = store.sessions()[&session_id].project_id.clone();
            let mut next = store.workspace_catalog().snapshot().unwrap().clone();
            next.revision += 1;
            next.workspaces.push(WorkspaceRecord {
                project_id: Some(project),
                id: WorkspaceId::new("next-workspace"),
                name: "next".into(),
                selected_tab: Some(TabId::new("next")),
                tabs: vec![WorkspaceTab {
                    id: TabId::new("next"),
                    title: None,
                    focused_pane: PaneId::new("next"),
                    zoomed_pane: None,
                    layout: LayoutNode::Pane {
                        id: PaneId::new("next"),
                        session_id,
                    },
                }],
            });
            store.finish_workspace_edit_for_test(next);
        }
    }
    cx.run_until_parked();
    let windows: Vec<AnyWindowHandle> = cx.update(|cx| cx.windows());
    assert_eq!(windows.len(), 2, "the row became a window");
    let opened = *windows.iter().find(|handle| **handle != first.into()).unwrap();
    settle(&mut cx, first.into());
    settle(&mut cx, opened);
    save(&mut cx, first.into(), "2-source");
    save(&mut cx, opened, "3-new-window");
    for handle in windows {
        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
    }
    cx.run_until_parked();
}
