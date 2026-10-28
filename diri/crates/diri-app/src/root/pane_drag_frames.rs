//! Renders a pane dragged by its title frame by frame against a stepped
//! clock, because an agent session cannot record the screen.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/frames \
//!   cargo test -p diri-app --bin diri -- --ignored render_pane_drag_frames
//! ```
//!
//! The default clip carries the `cargo test` pane across the Claude pane
//! (the preview slides between the edges and the middle it passes) and
//! drops it on the left edge; the panes glide to the new layout.
//! `DIRI_PANE_DRAG_CLIP=tab` instead rests it on the Codex row until that
//! tab springs open, then drops it on the Codex pane's bottom edge.
//! `DIRI_PANE_DRAG_REDUCED=1` renders with Reduce Motion on.

use std::time::Duration;

use diri_proto::workspace::*;
use gpui::{
    HeadlessAppContext, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, PlatformInput, Point, point, size,
};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use crate::pane_drag::test_clock;

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

fn snapshot(
    release: LayoutNode,
    codex: LayoutNode,
    selected: &str,
    revision: u64,
) -> WorkspaceSnapshot {
    let focused = |layout: &LayoutNode| match layout {
        LayoutNode::Pane { id, .. } => id.clone(),
        LayoutNode::Split { .. } => PaneId::new("agent"),
    };
    WorkspaceSnapshot {
        revision,
        workspaces: vec![WorkspaceRecord {
            project_id: None,
            id: WorkspaceId::new("project-workspace"),
            name: "diri".into(),
            selected_tab: Some(TabId::new(selected)),
            tabs: vec![
                WorkspaceTab {
                    id: TabId::new("release"),
                    title: None,
                    focused_pane: focused(&release),
                    zoomed_pane: None,
                    layout: release,
                },
                WorkspaceTab {
                    id: TabId::new("codex"),
                    title: None,
                    focused_pane: PaneId::new("codex"),
                    zoomed_pane: None,
                    layout: codex,
                },
            ],
        }],
        ..Default::default()
    }
}

fn three() -> LayoutNode {
    split(
        "columns",
        LayoutAxis::Horizontal,
        0.55,
        pane("agent", "preview-claude"),
        split(
            "stack",
            LayoutAxis::Vertical,
            0.5,
            pane("tests", "pane-tests"),
            pane("server", "pane-server"),
        ),
    )
}

const CLAUDE: &str = "> Prepare the signed 0.4.4 release\n\n● Bumping the version and changelog.\n\n  Update(Cargo.toml)\n  ⎿  version = \"0.4.4\"\n\n● The tag needs network access to publish.";
const TESTS: &str = "~/work/diri $ cargo test -p diri-engine\n    Finished test profile in 9.81s\n\ntest result: ok. 36 passed; 0 failed\n~/work/diri $ ";
const SERVER: &str = "~/work/diri/website $ pnpm dev\n\n  VITE v6.0.3  ready in 412 ms\n  ➜  Local:   http://localhost:3000/";
const CODEX: &str = "› Fix the flaky reconnect test\n\n• Reading crates/diri-engine/tests/wake.rs\n• The retry races the holder's exit marker.";

#[test]
#[ignore = "writes a pane drag, one PNG per frame, to the DIRI_VISUAL_OUTPUT directory"]
fn render_pane_drag_frames() {
    let output = std::path::PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let to_tab = std::env::var("DIRI_PANE_DRAG_CLIP").is_ok_and(|clip| clip == "tab");
    let reduced = std::env::var_os("DIRI_PANE_DRAG_REDUCED").is_some();
    let frame = Duration::from_micros(1_000_000 / 60);
    std::fs::create_dir_all(&output).unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        cx.set_reduce_motion(reduced);
    });
    test_clock::start();

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
        store.seed_workspace_snapshot_for_test(snapshot(
            three(),
            pane("codex", "preview-codex"),
            "release",
            1,
        ));
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
    macro_rules! root {
        (|$root:ident, $window:ident, $cx:ident| $body:expr) => {
            cx.update_window(window.into(), |view, $window, $cx| {
                let $root = view.downcast::<RootView>().unwrap();
                $body
            })
            .unwrap()
        };
    }
    let seed = |cx: &mut HeadlessAppContext| {
        cx.update_window(window.into(), |view, _, cx| {
            let root = view.downcast::<RootView>().unwrap();
            let workbench = root.read(cx).workspace_workbench.clone().unwrap();
            workbench.update(cx, |workbench, cx| {
                workbench.seed_pane_text_for_test(
                    &[
                        ("preview-claude", CLAUDE),
                        ("pane-tests", TESTS),
                        ("pane-server", SERVER),
                        ("preview-codex", CODEX),
                    ],
                    cx,
                )
            });
        })
        .unwrap();
    };
    seed(&mut cx);
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        root!(|_root, window, _cx| window.refresh());
        cx.run_until_parked();
    }
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
    let send = |cx: &mut HeadlessAppContext, input: PlatformInput| {
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(input, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    let moved = |position| {
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::none(),
        })
    };

    // The pointer's path, as (time in ms, point) keyframes it moves
    // between in straight lines; then when it lets go.
    let tests = rect(&mut cx, "tests");
    let agent = rect(&mut cx, "agent");
    let grab = point(px(tests.x + 64.0), px(tests.y + 18.0));
    let row = root!(|root, _window, cx| root
        .read(cx)
        .sidebar
        .read(cx)
        .project_agent_center_for_test(&SessionId::new("preview-codex"))
        .unwrap());
    let path: Vec<(u64, Point<Pixels>)> = if to_tab {
        vec![
            (300, grab),
            (650, row),
            // Rest on the row through the spring-load delay.
            (1350, row + point(px(6.0), px(0.0))),
            (1700, point(px(agent.x + agent.width * 0.6), px(agent.y + agent.height * 0.6))),
            (1900, point(px(agent.x + agent.width * 0.62), px(agent.y + agent.height * 0.9))),
        ]
    } else {
        let mid = agent.y + agent.height * 0.55;
        vec![
            (300, grab),
            // Into the Claude pane by its right edge, through its middle
            // (a swap), out to its left edge.
            (650, point(px(agent.x + agent.width * 0.92), px(mid))),
            (950, point(px(agent.x + agent.width * 0.92), px(mid))),
            (1150, point(px(agent.x + agent.width * 0.5), px(mid))),
            (1450, point(px(agent.x + agent.width * 0.5), px(mid))),
            (1700, point(px(agent.x + agent.width * 0.08), px(mid))),
        ]
    };
    let drop_at = path.last().unwrap().0 + 350;
    let length = drop_at + 700;
    let at = |ms: u64| -> Point<Pixels> {
        let mut previous = path[0];
        for &(time, position) in &path {
            if ms <= time {
                if time == previous.0 {
                    return position;
                }
                let t = (ms - previous.0) as f32 / (time - previous.0) as f32;
                let t = crate::pane_drag::ease_out(t);
                return previous.1 + (position - previous.1) * t;
            }
            previous = (time, position);
        }
        previous.1
    };

    // Frame build times by phase (layout and paint, without the capture).
    let mut timings: std::collections::BTreeMap<&str, Vec<Duration>> = Default::default();
    let mut elapsed = 0u64;
    let mut index = 0;
    let mut grabbed = false;
    let mut sprung = false;
    let mut dropped = false;
    while elapsed <= length {
        if !grabbed && elapsed >= path[0].0 {
            grabbed = true;
            send(
                &mut cx,
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: grab,
                    modifiers: Modifiers::none(),
                    click_count: 1,
                    first_mouse: false,
                }),
            );
            send(&mut cx, moved(grab + point(px(8.0), px(6.0))));
        } else if grabbed && !dropped && elapsed < drop_at {
            send(&mut cx, moved(at(elapsed)));
        }
        // The tab under the pane opens once it has rested for the delay;
        // the Engine's answer selects its saved tab.
        if to_tab && !sprung && elapsed >= 650 + crate::pane_drag::SPRING_LOAD.as_millis() as u64 {
            sprung = true;
            root!(|root, _window, cx| root.read(cx).sidebar.clone().update(cx, |sidebar, cx| {
                sidebar.spring_load_for_test(&SessionId::new("preview-codex"), cx)
            }));
            store
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(snapshot(
                    three(),
                    pane("codex", "preview-codex"),
                    "codex",
                    2,
                ));
            cx.run_until_parked();
            seed(&mut cx);
        }
        if !dropped && elapsed >= drop_at {
            dropped = true;
            let position = path.last().unwrap().1;
            send(
                &mut cx,
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::none(),
                    click_count: 1,
                }),
            );
            // The Engine's answer to the move.
            assert!(matches!(
                store.store.read().unwrap().in_flight_workspace_edit_for_test(),
                Some(WorkspaceMutation::MoveNode { .. })
            ));
            let (release, codex, selected) = if to_tab {
                (
                    split(
                        "columns",
                        LayoutAxis::Horizontal,
                        0.55,
                        pane("agent", "preview-claude"),
                        pane("server", "pane-server"),
                    ),
                    split(
                        "moved",
                        LayoutAxis::Vertical,
                        0.5,
                        pane("codex", "preview-codex"),
                        pane("tests", "pane-tests"),
                    ),
                    "codex",
                )
            } else {
                (
                    split(
                        "columns",
                        LayoutAxis::Horizontal,
                        0.55,
                        split(
                            "moved",
                            LayoutAxis::Horizontal,
                            0.5,
                            pane("tests", "pane-tests"),
                            pane("agent", "preview-claude"),
                        ),
                        pane("server", "pane-server"),
                    ),
                    pane("codex", "preview-codex"),
                    "release",
                )
            };
            let mut next = snapshot(release, codex, selected, 3);
            if to_tab {
                next.workspaces[0].tabs[1].focused_pane = PaneId::new("tests");
            } else {
                next.workspaces[0].tabs[0].focused_pane = PaneId::new("tests");
            }
            store
                .store
                .write()
                .unwrap()
                .finish_workspace_edit_for_test(next);
            cx.run_until_parked();
        }
        let started = std::time::Instant::now();
        root!(|root, window, cx| {
            root.update(cx, |_, cx| cx.notify());
            window.refresh();
        });
        cx.run_until_parked();
        let phase = if !grabbed {
            "rest"
        } else if !dropped {
            "drag"
        } else if elapsed < drop_at + crate::pane_drag::MORPH.as_millis() as u64 {
            "morph"
        } else {
            "settled"
        };
        timings.entry(phase).or_default().push(started.elapsed());
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output.join(format!("frame_{index:04}.png")))
            .unwrap();
        index += 1;
        // 16, 17, 17 ms: sixty frames a second on a whole-millisecond clock.
        let step = frame.as_millis() as u64 + u64::from(index % 3 != 1);
        elapsed += step;
        test_clock::advance(Duration::from_millis(step));
    }
    test_clock::stop();
    for (phase, mut times) in timings {
        times.sort();
        eprintln!(
            "{phase}: {} frames, median {:?}, p90 {:?}",
            times.len(),
            times[times.len() / 2],
            times[times.len() * 9 / 10],
        );
    }
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
