//! Headless renders of every underline kind, underline color, overline and
//! DECSCUSR cursor shape, from real escape sequences through the shared
//! parser and the grid wire:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/styles cargo test -p diri-term --test style_goldens -- --ignored
//! ```
//!
//! Writes `styles.png` (the screen as this build draws it), `styles-legacy.png`
//! (the same screen as a client without `terminal-styles-v1` receives it),
//! `cursor-{block,underline,bar}.png`, and `editor-insert.png` when the
//! real-editor check left its screen in the same directory.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::grid::GridUpdate;
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use diri_terminal_state::HeadlessScreen;
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: usize = 44;
const ROWS: usize = 12;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

/// Every decoration as a program writes it, one per row.
fn styles_screen(cursor: &[u8]) -> GridUpdate {
    let mut screen = HeadlessScreen::new(COLS, ROWS);
    screen.feed(b"\x1b[H");
    for (label, sgr) in [
        ("single     ", "4"),
        ("double     ", "4:2"),
        ("curly      ", "4:3"),
        ("dotted     ", "4:4"),
        ("dashed     ", "4:5"),
        ("curly red  ", "4:3;58:2::224:108:117"),
        ("dotted 58;5", "4:4;58;5;3"),
        ("overline   ", "53"),
        ("over+under ", "53;4:2;58;5;4"),
    ] {
        screen.feed(format!("{label} \x1b[{sgr}mThe quick brown fox\x1b[0m  plain\r\n").as_bytes());
    }
    // neovim's LSP diagnostic: an undercurl in the error color mid-line.
    screen.feed(
        b"let \x1b[38;2;97;175;239mrequest\x1b[0m = \x1b[4:3;58:2::224:108:117mrequset\x1b[0m(req);\r\n",
    );
    screen.feed(b"$ ");
    screen.feed(cursor);
    screen.full_snapshot()
}

fn capture(update: GridUpdate, name: &str) {
    capture_sized(update, name);
}

fn capture_sized(update: GridUpdate, name: &str) {
    let (cols, rows) = (update.cols, update.rows);
    let window_size = gpui::size(px(f32::from(cols) * 9.1), px(f32::from(rows) * 18.3));
    let element = TerminalElement::with_buffer(GridBuffer::new(cols, rows))
        .focused(true)
        .font(gpui::font("Menlo"));
    element.apply_damage(update);
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let window = cx
        .open_window(window_size, |_, cx| cx.new(|_| Fixture(element)))
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
fn render_style_goldens() {
    capture(styles_screen(b""), "styles.png");
    let mut legacy = styles_screen(b"");
    legacy.strip_styles();
    capture(legacy, "styles-legacy.png");
    for (name, sequence) in [
        ("cursor-block.png", &b"\x1b[2 q"[..]),
        ("cursor-underline.png", b"\x1b[4 q"),
        ("cursor-bar.png", b"\x1b[6 q"),
    ] {
        capture(styles_screen(sequence), name);
    }
    // The real-editor check (`diri-terminal-state --test real_editor`) leaves
    // its insert-mode screen here; render it at its own size.
    let editor = output("editor-insert.grid");
    if let Ok(bytes) = std::fs::read(&editor) {
        let update = GridUpdate::decode(&bytes).expect("editor grid");
        capture_sized(update, "editor-insert.png");
    }
}
