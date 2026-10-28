//! Every glyph is drawn at its own cell. A cell the shaper gives no advance
//! (U+00AD SOFT HYPHEN, U+2028 LINE SEPARATOR) used to pull every glyph after
//! it one column left, so the cursor and the cell backgrounds sat on
//! different characters than the text: moving through "Don't" showed "DDn't".
//! An invisible cell must paint exactly what a blank cell paints.
#![cfg(target_os = "macos")]

use std::sync::Arc;

use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: u16 = 32;
const ROWS: u16 = 2;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn terminal(text: &str) -> TerminalElement {
    let mut cells: Vec<GridCell> = text
        .chars()
        .map(|ch| {
            GridCell::new(
                u32::from(ch),
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            )
        })
        .collect();
    cells.resize(usize::from(COLS), GridCell::BLANK);
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS))
        .focused(true)
        .reduce_motion(true)
        .font(gpui::font("Menlo"));
    terminal.apply_damage(GridUpdate {
        cols: COLS,
        rows: ROWS,
        // On the apostrophe, the cell the bug drew as the "n" before it.
        cursor_col: 14,
        cursor_row: 0,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows: vec![ChangedRow::new(0, cells)],
        cursor_style: Default::default(),
        history: None,
    });
    terminal
}

#[test]
fn a_cell_without_advance_does_not_shift_the_glyphs_after_it() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let mut draw = |text: &str| {
        let terminal = terminal(text);
        let window = cx
            .open_window(gpui::size(px(300.0), px(40.0)), move |_, cx| {
                cx.new(|_| Fixture(terminal))
            })
            .expect("headless window");
        cx.run_until_parked();
        cx.capture_screenshot(window.into()).expect("screenshot")
    };

    let blank = draw("shy  now. Don\u{2019}t stop this");
    assert!(
        blank.pixels().any(|pixel| pixel != blank.get_pixel(0, 0)),
        "fixture rendered a blank frame"
    );
    for invisible in ['\u{00ad}', '\u{2028}'] {
        let shifted = draw(&format!("shy{invisible} now. Don\u{2019}t stop this"));
        let differing = shifted
            .pixels()
            .zip(blank.pixels())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            differing,
            0,
            "U+{:04X} moved the glyphs after it off their cells",
            u32::from(invisible)
        );
    }
}
