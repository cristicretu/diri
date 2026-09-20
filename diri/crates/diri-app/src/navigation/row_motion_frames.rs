//! Renders a query being typed into the command palette frame by frame against
//! a stepped clock, because an agent session cannot record the screen.
//! `docs/screenshots/palette-rows` was made with it.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/frames DIRI_TYPING_QUERY=sidebar \
//!   cargo test -p diri-app --bin diri -- --ignored render_palette_typing_frames
//! ```
//!
//! `DIRI_TYPING_CADENCE_MS` spaces the keystrokes (80 by default),
//! `DIRI_TYPING_FPS` sets the frame rate, and `DIRI_TYPING_STILL=1` snaps the
//! rows after every key, which is the cut a list without row motion paints.
//! The chat rows' activity marks follow the wall clock, not this one, so they
//! step at whatever pace the frames happen to render.

use std::time::Duration;

use gpui::{Entity, HeadlessAppContext};

use super::row_motion_tests::working_runtime;
use super::*;

struct Harness {
    overlay: Entity<NavigationOverlay>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(self.overlay.read(cx).colors().background)
            .child(crate::root::cached_window_overlay(self.overlay.clone()))
    }
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "writes a typed palette query, one PNG per frame, to the DIRI_VISUAL_OUTPUT directory"]
fn render_palette_typing_frames() {
    let output = PathBuf::from(std::env::var("DIRI_VISUAL_OUTPUT").unwrap());
    let query = std::env::var("DIRI_TYPING_QUERY").unwrap_or_else(|_| "sidebar".into());
    let cadence = Duration::from_millis(env_number("DIRI_TYPING_CADENCE_MS", 80));
    let frame = Duration::from_micros(1_000_000 / env_number::<u64>("DIRI_TYPING_FPS", 60));
    let still = std::env::var_os("DIRI_TYPING_STILL").is_some();
    std::fs::create_dir_all(&output).unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| crate::fonts::init(cx));

    let start = Instant::now();
    let mut palette = None;
    let window = cx
        .open_window(gpui::size(px(640.0), px(460.0)), |_, cx| {
            let overlay = cx.new(|cx| {
                let mut overlay = NavigationOverlay::opened_for_test(working_runtime(), cx);
                overlay.motion_clock = Some(start);
                overlay.refresh_command_items();
                overlay
            });
            palette = Some(overlay.clone());
            cx.new(|_| Harness { overlay })
        })
        .expect("open headless palette window");
    let overlay = palette.expect("palette entity");
    // The surface's entry fade runs on the wall clock; let it finish off camera.
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(250));

    // A beat on the landing page, the query, then long enough to read the
    // settled list before a GIF made of these loops.
    let lead = Duration::from_millis(400);
    let keys: Vec<char> = query.chars().collect();
    let total = lead + cadence * keys.len() as u32 + Duration::from_millis(900);
    let mut typed = 0;
    let mut elapsed = Duration::ZERO;
    let mut index = 0;
    while elapsed <= total {
        cx.update_window(window.into(), |_, window, cx| {
            overlay.update(cx, |overlay, cx| {
                overlay.motion_clock = Some(start + elapsed);
                while typed < keys.len() && elapsed >= lead + cadence * typed as u32 {
                    overlay.query.insert(&keys[typed].to_string());
                    overlay.query_changed(cx);
                    if still {
                        overlay.row_motion.snap();
                    }
                    typed += 1;
                }
                cx.notify();
            });
            window.refresh();
        })
        .expect("step the palette");
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .expect("capture palette frame")
            .save(output.join(format!("frame_{index:04}.png")))
            .expect("save palette frame");
        index += 1;
        elapsed += frame;
    }
}
