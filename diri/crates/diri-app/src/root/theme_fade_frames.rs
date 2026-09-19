//! Renders a theme change frame by frame against a stepped clock, because an
//! agent session cannot record the screen. `docs/screenshots/theme-crossfade`
//! was made with it.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/frames DIRI_FADE_FROM=tokyo-night DIRI_FADE_KEYS=6 \
//!   cargo test -p diri-app --bin diri -- --ignored render_theme_fade_frames
//! ```
//!
//! `DIRI_FADE_TO=<id>` saves that theme instead of arrowing through the
//! palette's preview, `DIRI_FADE_CADENCE_MS` spaces the arrow presses, and
//! `DIRI_FADE_INSTANT=1` leaves fades off, which is what a hard cut paints.
//! `DIRI_FADE_TIMING=1` under `--release` prints each frame's whole-window
//! draw time instead of saving it.

use std::time::Duration;

use diri_proto::grid::{TermColor, TermStyle};
use diri_proto::workspace::*;
use diri_term::buffer::GridBuffer;
use gpui::{HeadlessAppContext, size};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;

const COLS: u16 = 100;
const ROWS: u16 = 36;

type Span = (&'static str, TermColor, TermStyle);

fn plain(text: &'static str) -> Span {
    (text, TermColor::Default, TermStyle::empty())
}

fn ansi(text: &'static str, index: u8) -> Span {
    (text, TermColor::Ansi(index), TermStyle::empty())
}

fn bold(text: &'static str, index: u8) -> Span {
    (text, TermColor::Ansi(index), TermStyle::BOLD)
}

fn faint(text: &'static str) -> Span {
    (text, TermColor::Default, TermStyle::DIM)
}

/// An agent session as it really looks: a prompt, a colored diff, a test run,
/// and a status line.
fn transcript() -> GridBuffer {
    let lines: Vec<Vec<Span>> = vec![
        vec![
            bold("~/work/diri", 4),
            plain(" "),
            ansi("main", 5),
            plain(" "),
            ansi("$", 2),
            plain(" git diff --stat"),
        ],
        vec![
            plain(" crates/diri-term/src/crossfade.rs | 212 "),
            ansi("++++++++++++++++", 2),
        ],
        vec![
            plain(" crates/diri-term/src/contrast.rs  |  41 "),
            ansi("+++", 2),
            ansi("--", 1),
        ],
        vec![
            plain(" crates/diri-app/src/app_theme.rs  |  38 "),
            ansi("+++", 2),
            ansi("-", 1),
        ],
        vec![],
        vec![
            ansi("@@ -84,6 +84,12 @@", 6),
            faint(" fn semantic_colors(theme: TermTheme)"),
        ],
        vec![ansi("-    let sidebar_alpha = match theme.appearance {", 1)],
        vec![ansi("+    let appearance = chrome_appearance(theme);", 2)],
        vec![ansi("+    let sidebar_alpha = match appearance {", 2)],
        vec![
            plain("         ThemeAppearance::Dark => "),
            ansi("0.86", 3),
            plain(","),
        ],
        vec![
            plain("         ThemeAppearance::Light => "),
            ansi("0.90", 3),
            plain(","),
        ],
        vec![],
        vec![
            bold("~/work/diri", 4),
            plain(" "),
            ansi("main", 5),
            plain(" "),
            ansi("$", 2),
            plain(" cargo test -p diri-term"),
        ],
        vec![bold("   Compiling", 10), plain(" diri-term v0.1.0")],
        vec![bold("    Finished", 10), plain(" `test` profile in 4.31s")],
        vec![plain("running 139 tests")],
        vec![
            plain("test crossfade::tests::endpoints_are_exact ... "),
            ansi("ok", 2),
        ],
        vec![
            plain("test crossfade::tests::holding_the_arrow_key_never_jumps ... "),
            ansi("ok", 2),
        ],
        vec![
            plain("test contrast::tests::dark_themes_are_untouched ... "),
            ansi("ok", 2),
        ],
        vec![
            plain("test element::tests::wide_glyphs_keep_their_cells ... "),
            ansi("FAILED", 9),
        ],
        vec![],
        vec![bold("warning", 11), bold(": unused variable: `phase`", 15)],
        vec![
            ansi("  --> ", 12),
            plain("crates/diri-term/src/element.rs:1315:17"),
        ],
        vec![faint(
            "   = note: `#[warn(unused_variables)]` on by default",
        )],
        vec![],
        vec![
            ansi("●", 13),
            plain(" Reading "),
            ansi("crossfade.rs", 14),
            faint(" (212 lines)"),
        ],
        vec![
            ansi("●", 3),
            bold(" Update", 15),
            plain("(crates/diri-term/src/element.rs)"),
        ],
        vec![faint("  ⎿  Updated with 3 additions and 1 removal")],
        vec![],
        vec![
            (" NORMAL ", TermColor::Ansi(0), TermStyle::INVERSE),
            (" element.rs ", TermColor::Ansi(8), TermStyle::INVERSE),
            faint("  utf-8  rust  1315:17"),
        ],
    ];
    let mut grid = GridBuffer::new(COLS, ROWS);
    for (row, spans) in lines.into_iter().enumerate() {
        let mut col = 0;
        for (text, fg, style) in spans {
            for ch in text.chars() {
                let cell = &mut grid.cells[row * usize::from(COLS) + col];
                cell.scalar = u32::from(ch);
                cell.fg = fg;
                cell.style = style;
                col += 1;
            }
        }
    }
    grid
}

#[test]
#[ignore = "writes a theme change, one PNG per frame, to the DIRI_VISUAL_OUTPUT directory"]
fn render_theme_fade_frames() {
    let output = std::path::PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let from = std::env::var("DIRI_FADE_FROM").unwrap_or_else(|_| "dirijor-dark".into());
    let to = std::env::var("DIRI_FADE_TO").ok();
    let keys: u32 = env_number("DIRI_FADE_KEYS", 6);
    let cadence = Duration::from_millis(env_number("DIRI_FADE_CADENCE_MS", 150));
    let frame = Duration::from_micros(1_000_000 / env_number::<u64>("DIRI_FADE_FPS", 60));
    let instant = std::env::var_os("DIRI_FADE_INSTANT").is_some();
    let timing = std::env::var_os("DIRI_FADE_TIMING").is_some();
    let mut draws = Vec::new();
    std::fs::create_dir_all(&output).unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::fonts::init(cx));
    let _fades = (!instant).then(crate::app_theme::live::testing::enable_with_manual_clock);

    let services = test_services();
    let store = services.store.clone();
    let workspace = WorkspaceId::new("release-workspace");
    {
        let mut store = store.store.write().unwrap();
        store.hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
        store
            .update_preferences(|prefs| prefs.terminal_theme = from)
            .unwrap();
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot {
            revision: 7,
            workspaces: vec![WorkspaceRecord {
                project_id: None,
                id: workspace.clone(),
                name: "Release room".into(),
                selected_tab: Some(TabId::new("release-tab")),
                tabs: vec![WorkspaceTab {
                    id: TabId::new("release-tab"),
                    title: Some("Implement and verify".into()),
                    focused_pane: PaneId::new("coding"),
                    zoomed_pane: None,
                    layout: LayoutNode::Pane {
                        id: PaneId::new("coding"),
                        session_id: SessionId::new("preview-claude"),
                    },
                }],
            }],
            ..Default::default()
        });
    }
    let window = cx
        .open_window(size(px(1100.0), px(720.0)), |window, cx| {
            cx.new(|cx| {
                let root = RootView::new(services, false, PreviewScenario::Empty, window, cx);
                root.sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .set_tab_orientation(crate::store::TabOrientation::Vertical, cx)
                        .unwrap();
                    sidebar.activate_workspace(Some(workspace), cx);
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
    root!(|root, _window, cx| {
        let workbench = root.workspace_workbench.clone().unwrap();
        workbench.update(cx, |workbench, cx| {
            workbench.seed_pane_grids_for_test(&transcript(), cx);
        });
    });
    if to.is_none() {
        root!(|root, window, cx| {
            let navigation = root.navigation.clone().unwrap();
            navigation.update(cx, |navigation, cx| {
                navigation.open_themes_for_test(window, cx)
            });
        });
    }
    // Entrance motion elsewhere in the window runs on the wall clock. Let it
    // finish so every captured frame differs only by the theme.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        root!(|_root, window, _cx| window.refresh());
        cx.run_until_parked();
    }

    let lead = Duration::from_millis(250);
    let length = lead + cadence * keys.max(1) + Duration::from_millis(700);
    let mut elapsed = Duration::ZERO;
    let mut pressed = 0;
    let mut index = 0;
    while elapsed <= length {
        while pressed < keys.max(1) && elapsed >= lead + cadence * pressed {
            pressed += 1;
            match &to {
                Some(to) => {
                    let to = to.clone();
                    store
                        .store
                        .write()
                        .unwrap()
                        .update_preferences(|prefs| prefs.terminal_theme = to)
                        .unwrap();
                }
                None => root!(|root, _window, cx| {
                    let navigation = root.navigation.clone().unwrap();
                    navigation.update(cx, |navigation, cx| navigation.arrow_for_test(1, cx));
                }),
            }
        }
        root!(|_root, window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
        });
        cx.run_until_parked();
        if timing {
            // Layout, prepaint and paint of the whole window, as a display
            // link tick would run them. Only meaningful under `--release`.
            let started = std::time::Instant::now();
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            draws.push((elapsed, started.elapsed()));
        } else {
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(output.join(format!("frame_{index:04}.png")))
                .unwrap();
        }
        index += 1;
        elapsed += frame;
        if !instant {
            crate::app_theme::live::testing::advance(frame);
        }
    }
    for (at, draw) in &draws {
        println!("theme-fade-draw: at={at:?} draw={draw:?}");
    }
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
