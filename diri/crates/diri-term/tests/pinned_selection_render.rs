//! Headless renders of a selection while output streams past it, from an
//! Engine with and without `grid-history-v1`:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/pinned cargo test -p diri-term --test pinned_selection_render -- --ignored
//! ```
//!
//! Writes `selected.png` (the selection as made), `streamed-without-history.png`
//! and `streamed-with-history.png` (after three more lines scroll it up), and
//! `scrolled-back.png` (with history, three lines later, scrolled back to it).
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::grid::GridUpdate;
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use diri_terminal_state::HeadlessScreen;
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: usize = 34;
const ROWS: usize = 6;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

struct Pane {
    screen: HeadlessScreen,
    element: TerminalElement,
    history: bool,
}

impl Pane {
    fn new(history: bool) -> Self {
        let mut pane = Self {
            screen: HeadlessScreen::new(COLS, ROWS),
            element: TerminalElement::with_buffer(GridBuffer::new(COLS as u16, ROWS as u16))
                .focused(false)
                .font(gpui::font("Menlo")),
            history,
        };
        pane.print(b"$ cargo build\r\n   Compiling diri-proto v0.1.0\r\n");
        pane
    }

    fn print(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        let update = self.screen.grid_update(false);
        let bytes = if self.history {
            update.encode()
        } else {
            update.encode_without_history()
        }
        .unwrap();
        self.element
            .apply_damage(GridUpdate::decode(&bytes).unwrap());
    }

    fn stream(&mut self, names: &[&str]) {
        for name in names {
            self.print(format!("   Compiling {name} v0.1.0\r\n").as_bytes());
        }
    }
}

fn capture(cx: &mut gpui::HeadlessAppContext, element: TerminalElement, name: &str) {
    element.pin_selection_shimmer_clock(None);
    let size = gpui::size(px(COLS as f32 * 9.1), px(ROWS as f32 * 18.3));
    let window = cx
        .open_window(size, |_, cx| cx.new(|_| Fixture(element)))
        .expect("headless window");
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    image.save(output(name)).expect("write png");
}

fn output(name: &str) -> PathBuf {
    let directory = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(PathBuf::from)
        .expect("DIRI_VISUAL_OUTPUT names the output directory");
    std::fs::create_dir_all(&directory).expect("output directory");
    directory.join(name)
}

// One test: two headless platforms cannot start concurrently.
#[test]
#[ignore = "writes PNG renders for review"]
fn render_pinned_selection_screenshot() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let select = |pane: &Pane| {
        pane.element.begin_selection(3, 1);
        pane.element.drag_selection_to_edge(29, 30, 1);
    };

    let pane = Pane::new(true);
    select(&pane);
    eprintln!("selected: {:?}", pane.element.selected_text());
    capture(&mut cx, pane.element.clone(), "selected.png");

    for history in [false, true] {
        let mut pane = Pane::new(history);
        select(&pane);
        pane.stream(&["diri-pty", "diri-terminal-state", "diri-engine"]);
        eprintln!(
            "streamed (history {history}): {:?}",
            pane.element.selected_text()
        );
        let name = if history {
            "streamed-with-history.png"
        } else {
            "streamed-without-history.png"
        };
        capture(&mut cx, pane.element.clone(), name);
        if history {
            pane.stream(&["diri-term", "diri-app", "diri"]);
            // Scroll back to the line: two steps, the first learns how
            // much history there is.
            for offset in [3, 5] {
                pane.element.set_view_offset(offset, ROWS);
                while let Some(request) = pane.element.begin_scrollback_fetch(ROWS) {
                    let reply = pane
                        .screen
                        .scrollback_cells(request.first_row, request.max_rows);
                    pane.element.complete_scrollback_fetch(reply, ROWS).unwrap();
                }
            }
            capture(&mut cx, pane.element.clone(), "scrolled-back.png");
        }
    }
}
