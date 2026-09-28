//! Renders hold-⌘ shortcut hints headlessly, because an agent session cannot
//! record the screen. `docs/screenshots/held-command-hints` was made with it.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/hints DIRI_HINTS_SCRIPT=hold \
//!   cargo test -p diri-app --bin diri -- --ignored render_held_hint_frames
//! ```
//!
//! `DIRI_HINTS_SCRIPT` picks what happens on a stepped clock:
//! - `still` (default): one capture with ⌘ held and the hints settled, plus
//!   `rest.png` before ⌘ goes down.
//! - `hold`: hold ⌘, the hints fade in, press 3, session 3 is selected and the
//!   hints leave, then ⌘ is released.
//! - `chord`: a quick ⌘T with ⌘ held on past the delay; nothing may flash.
//!
//! `DIRI_HINTS_ORIENTATION=horizontal` uses the tab strip,
//! `DIRI_VISUAL_THEME=<id>` picks the theme, `DIRI_HINTS_FPS` the frame rate,
//! and `DIRI_HINTS_STYLE` one of the rejected label treatments.

use std::time::{Duration, Instant};

use gpui::{HeadlessAppContext, Modifiers, size};

use super::tests::test_services;
use super::*;
use crate::SidebarPreviewFixture;
use crate::held_hints::{HOLD_DELAY, HeldHintsState};

#[test]
#[ignore = "writes hold-⌘ hint captures to the DIRI_VISUAL_OUTPUT directory"]
fn render_held_hint_frames() {
    let output = std::path::PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    let script = std::env::var("DIRI_HINTS_SCRIPT").unwrap_or_else(|_| "still".into());
    let horizontal = std::env::var("DIRI_HINTS_ORIENTATION").as_deref() == Ok("horizontal");
    let fps: u64 = std::env::var("DIRI_HINTS_FPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    let frame = Duration::from_micros(1_000_000 / fps);

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        crate::fonts::init(cx);
        crate::commands::bind_keys(cx, &Default::default());
        cx.set_reduce_motion(false);
    });
    let services = test_services();
    let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
    {
        let mut store = services.store.store.write().unwrap();
        store.hydrate(fixture.list);
        store.select(fixture.selected_session_id.unwrap());
        store
            .update_preferences(|prefs| {
                prefs.sidebar_visible = true;
                if let Ok(theme) = std::env::var("DIRI_VISUAL_THEME") {
                    prefs.terminal_theme = theme;
                }
            })
            .unwrap();
    }
    let window = cx
        .open_window(size(px(1100.0), px(700.0)), |window, cx| {
            cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
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
    root!(|root, window, cx| root.run_command(
        if horizontal {
            CommandId::HorizontalTabs
        } else {
            CommandId::VerticalTabs
        },
        window,
        cx
    ));
    // Real terminal text under the header, so the labels that hang below
    // the title bar are judged against a busy surface.
    root!(|root, _window, cx| {
        if let Some(terminal) = root.terminal.clone() {
            terminal.update(cx, |terminal, cx| {
                terminal.seed_preview_grid_for_test(super::theme_fade_frames::transcript(), cx)
            });
        }
    });
    // Entrance motion elsewhere runs on the wall clock; let it land so frames
    // differ only by the hints.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(150));
        root!(|_root, window, _cx| window.refresh());
        cx.run_until_parked();
    }

    let t0 = Instant::now();
    let command = Modifiers {
        platform: true,
        ..Modifiers::default()
    };
    let released = Modifiers::default();
    // Feeds one event through RootView's real handlers at a synthetic time.
    macro_rules! at {
        ($elapsed:expr, |$root:ident, $window:ident, $cx:ident, $now:ident| $body:expr) => {{
            let $now = t0 + $elapsed;
            cx.update(|cx| HeldHintsState::freeze_clock(Some($now), cx));
            root!(|$root, $window, $cx| $body);
            cx.run_until_parked();
        }};
    }
    macro_rules! capture {
        ($name:expr) => {{
            root!(|_root, window, _cx| window.refresh());
            cx.run_until_parked();
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(output.join($name))
                .unwrap();
        }};
    }
    let modifiers = |modifiers| ModifiersChangedEvent {
        modifiers,
        capslock: Default::default(),
    };
    // The hold timer is the executor's; on a stepped clock the fixture fires
    // it itself, exactly when the delay ends.
    macro_rules! fire_delay {
        ($elapsed:expr) => {
            at!($elapsed, |root, window, cx, now| {
                if let Some((generation, _)) = root.held_hints.armed() {
                    let effect = root.held_hints.delay_elapsed(generation, now);
                    root.apply_held_hint_effect(effect, window, cx);
                }
            })
        };
    }

    match script.as_str() {
        "still" => {
            at!(Duration::ZERO, |_root, _window, _cx, _now| ());
            capture!("rest.png");
            at!(Duration::ZERO, |root, window, cx, _now| root
                .on_modifiers_changed(&modifiers(command), window, cx));
            fire_delay!(HOLD_DELAY);
            at!(HOLD_DELAY + Duration::from_millis(400), |_r, _w, _c, _n| ());
            capture!("held.png");
        }
        "hold" | "chord" => {
            let press = Duration::from_millis(200);
            let length = Duration::from_millis(if script == "hold" { 2_000 } else { 1_600 });
            let key_at = if script == "hold" {
                press + HOLD_DELAY + Duration::from_millis(600)
            } else {
                press + Duration::from_millis(140)
            };
            let release_at = if script == "hold" {
                key_at + Duration::from_millis(250)
            } else {
                length - Duration::from_millis(200)
            };
            let (mut pressed, mut fired, mut keyed, mut let_go) = (false, false, false, false);
            let mut elapsed = Duration::ZERO;
            let mut index = 0;
            while elapsed <= length {
                if !pressed && elapsed >= press {
                    pressed = true;
                    at!(press, |root, window, cx, _now| root.on_modifiers_changed(
                        &modifiers(command),
                        window,
                        cx
                    ));
                }
                if !keyed && elapsed >= key_at {
                    keyed = true;
                    at!(key_at, |root, window, cx, now| {
                        let effect = root.held_hints.key_down(now);
                        root.apply_held_hint_effect(effect, window, cx);
                        root.run_command(
                            if script == "hold" {
                                CommandId::SelectSession3
                            } else {
                                CommandId::NewDefaultSession
                            },
                            window,
                            cx,
                        );
                    });
                }
                if !fired && elapsed >= press + HOLD_DELAY {
                    fired = true;
                    fire_delay!(press + HOLD_DELAY);
                }
                if !let_go && elapsed >= release_at {
                    let_go = true;
                    at!(release_at, |root, window, cx, _now| root
                        .on_modifiers_changed(&modifiers(released), window, cx));
                }
                at!(elapsed, |_r, _w, _c, _n| ());
                capture!(format!("frame_{index:04}.png"));
                index += 1;
                elapsed += frame;
            }
        }
        other => panic!("unknown DIRI_HINTS_SCRIPT {other}"),
    }
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}
