//! Headless render of emoji clusters as an agent lays them out.
//!
//! Each row prints clusters separated by `|`; the row under it puts a `|` at
//! the column `Bun.stringWidth` (Claude Code) expects. When the parser agrees
//! with Bun the bars line up and each cluster is one glyph across its cells.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/clusters cargo test -p diri-term --test grapheme_clusters -- --ignored
//! ```
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_term::buffer::GridBuffer;
use diri_term::element::TerminalElement;
use diri_terminal_state::HeadlessScreen;
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: usize = 44;
const ROWS: usize = 12;

/// Clusters and the columns Bun 1.4.4 gives them.
const ROWS_OF_CLUSTERS: &[&[(&str, usize)]] = &[
    &[
        ("❤️", 2),
        ("✔️", 2),
        ("⚠️", 2),
        ("☀️", 2),
        ("▶️", 2),
        ("✅", 2),
    ],
    &[("👨‍👩‍👧‍👦", 2), ("🧑‍💻", 2), ("🏳️‍🌈", 2), ("❤️‍🔥", 2), ("🐻‍❄️", 2)],
    &[
        ("🇷🇴", 2),
        ("🇺🇸", 2),
        ("🇯🇵", 2),
        ("👍🏽", 2),
        ("🧑🏽‍💻", 2),
        ("1️⃣", 2),
    ],
    &[
        ("你好", 4),
        ("각", 2),
        ("e\u{301}", 1),
        ("রগুলো", 3),
        ("⌚︎", 2),
    ],
];

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn screen() -> HeadlessScreen {
    let mut screen = HeadlessScreen::new(COLS, ROWS);
    screen.feed(b"\x1b[2m0123456789012345678901234567890123456789\x1b[0m\r\n");
    for clusters in ROWS_OF_CLUSTERS {
        let mut printed = String::from("|");
        let mut expected = String::from("|");
        for (text, width) in *clusters {
            printed.push_str(text);
            printed.push('|');
            expected.push_str(&" ".repeat(*width));
            expected.push('|');
        }
        screen.feed(format!("{printed}\r\n\x1b[2m{expected}\x1b[0m\r\n").as_bytes());
    }
    // A Claude Code prompt line: typed, never submitted, cursor after the
    // last cluster, then moved back over the flag the way Ink repaints it.
    screen.feed("\x1b[1m>\x1b[0m ship it ✔️ 🇷🇴 👨‍👩‍👧‍👦 ok".as_bytes());
    screen.feed(b"\r\x1b[13C");
    screen
}

fn element(screen: &HeadlessScreen) -> TerminalElement {
    let element = TerminalElement::with_buffer(GridBuffer::new(COLS as u16, ROWS as u16))
        .focused(true)
        .font(gpui::font("Menlo"));
    element.apply_damage(screen.full_snapshot());
    element
}

fn capture(element: TerminalElement, output: PathBuf) {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let window = cx
        .open_window(gpui::size(px(440.0), px(220.0)), |_, cx| {
            cx.new(|_| Fixture(element))
        })
        .expect("headless window");
    cx.run_until_parked();
    // A second frame: the first can land before the window has content.
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    image.save(output).expect("write screenshot");
}

fn bar_columns(screen: &HeadlessScreen, row: usize) -> Vec<usize> {
    screen.full_snapshot().changed_rows[row]
        .cells
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.scalar == u32::from('|'))
        .map(|(column, _)| column)
        .collect()
}

#[test]
fn clusters_end_where_claude_code_expects() {
    let screen = screen();
    for (index, clusters) in ROWS_OF_CLUSTERS.iter().enumerate() {
        let printed = 1 + 2 * index;
        assert_eq!(
            bar_columns(&screen, printed),
            bar_columns(&screen, printed + 1),
            "{clusters:?}"
        );
    }
    // The cursor sits on the flag's first cell after the repaint.
    assert_eq!(screen.cursor().0, 13);
}

#[test]
#[ignore = "renders a PNG screenshot of emoji clusters"]
fn render_grapheme_clusters_screenshot() {
    let directory = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(PathBuf::from)
        .expect("DIRI_VISUAL_OUTPUT names the output directory");
    std::fs::create_dir_all(&directory).expect("output directory");
    capture(element(&screen()), directory.join("grapheme-clusters.png"));
}
