//! Headless renders of double and triple click selections over soft-wrapped
//! and double-width text, from real escape sequences through the shared
//! parser and the grid wire:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/copy cargo test -p diri-term --test copy_selection_render -- --ignored
//! ```
//!
//! Writes `word-wrapped-url.png`, `line-wrapped-command.png` and
//! `word-cjk.png`, each with the selection a click made.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_term::{buffer::GridBuffer, element::TerminalElement};
use diri_terminal_state::HeadlessScreen;
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: usize = 24;
const ROWS: usize = 5;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn element(bytes: &[u8]) -> TerminalElement {
    let mut screen = HeadlessScreen::new(COLS, ROWS);
    screen.feed(bytes);
    let element = TerminalElement::with_buffer(GridBuffer::new(COLS as u16, ROWS as u16))
        .focused(false)
        .font(gpui::font("Menlo"));
    element.apply_damage(screen.full_snapshot());
    element
}

fn capture(cx: &mut gpui::HeadlessAppContext, element: TerminalElement, name: &str) {
    // The sheen is a gesture's; a still frame shows the settled selection.
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
fn render_copy_selection_screenshot() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );

    let url = element(b"see https://example.com/docs/selection/words ok\r\n$ ");
    url.select_word(3, 1);
    eprintln!("word-wrapped-url: {:?}", url.selected_text());
    capture(&mut cx, url, "word-wrapped-url.png");

    let line = element(b"$ cargo test --workspace --all-targets\r\nok\r\n$ ");
    line.select_line(1);
    eprintln!("line-wrapped-command: {:?}", line.selected_text());
    capture(&mut cx, line, "line-wrapped-command.png");

    let cjk = element("echo 日本語テキスト done\r\n$ ".as_bytes());
    cjk.select_word(9, 0);
    eprintln!("word-cjk: {:?}", cjk.selected_text());
    capture(&mut cx, cjk, "word-cjk.png");
}
