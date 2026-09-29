//! A terminal that only moves (a sidebar seam or split divider sliding past
//! it) reuses its prepared rows by translating them. This checks the moved
//! frame against a terminal drawn fresh at the destination, pixel for pixel,
//! through the real headless Metal renderer, and that the move prepared no
//! row again.
#![cfg(target_os = "macos")]

use std::sync::{Arc, Mutex};

use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use gpui::{
    AppContext as _, Context, IntoElement, ParentElement, Pixels, Render, Styled, Window, div, px,
};

const COLS: u16 = 48;
const ROWS: u16 = 12;

struct Fixture {
    terminal: TerminalElement,
    offset: Arc<Mutex<(Pixels, Pixels)>>,
}

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (left, top) = *self.offset.lock().unwrap();
        div()
            .size_full()
            .pl(left)
            .pt(top)
            .child(self.terminal.clone())
    }
}

fn cells(text: &str, fg: TermColor, bg: TermColor, style: TermStyle) -> Vec<GridCell> {
    text.chars()
        .flat_map(|ch| {
            let wide = matches!(u32::from(ch), 0x3000..=0x9FFF | 0x1F300..=0x1FAFF);
            let cell = GridCell::new(u32::from(ch), fg, bg, style);
            std::iter::once(cell)
                .chain(wide.then(|| GridCell::new(0, fg, bg, style | TermStyle::WIDE_SPACER)))
        })
        .collect()
}

fn snapshot() -> GridUpdate {
    let bg = TermColor::DefaultInverted;
    let none = TermStyle::empty();
    let rows = [
        cells(
            "plain text on the default background",
            TermColor::Default,
            bg,
            none,
        ),
        [
            cells("error", TermColor::Ansi(1), bg, TermStyle::BOLD),
            cells(": ", TermColor::Default, bg, none),
            cells("highlighted", TermColor::Ansi(0), TermColor::Ansi(3), none),
            cells(" underlined", TermColor::Ansi(4), bg, TermStyle::UNDERLINE),
        ]
        .concat(),
        cells("╭──────────┬─────────╮", TermColor::Ansi(4), bg, none),
        cells("│ 構築 🦀 ok │ ⣿⣶⣤⣀⡀ ▇▆▅ │", TermColor::Ansi(2), bg, none),
        cells("╰──────────┴─────────╯", TermColor::Ansi(4), bg, none),
        cells("inverse run", TermColor::Default, bg, TermStyle::INVERSE),
        cells(
            "italic dim",
            TermColor::Ansi(5),
            bg,
            TermStyle::ITALIC | TermStyle::DIM,
        ),
    ];
    GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: 4,
        cursor_row: 8,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows: rows
            .into_iter()
            .enumerate()
            .map(|(row, mut cells)| {
                cells.resize(usize::from(COLS), GridCell::BLANK);
                ChangedRow::new(row as u16, cells)
            })
            .collect(),
    }
}

fn terminal() -> TerminalElement {
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS))
        .focused(true)
        .font(gpui::font("Menlo"));
    terminal.apply_damage(snapshot());
    terminal
}

#[test]
fn a_terminal_that_only_moved_paints_what_a_fresh_one_paints() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let mut draw = |terminal: TerminalElement, offsets: &[(f32, f32)]| {
        let offset = Arc::new(Mutex::new((px(offsets[0].0), px(offsets[0].1))));
        let window = cx
            .open_window(gpui::size(px(520.0), px(300.0)), {
                let offset = Arc::clone(&offset);
                let terminal = terminal.clone();
                move |_, cx| cx.new(|_| Fixture { terminal, offset })
            })
            .expect("headless window");
        cx.run_until_parked();
        let first = terminal.stats();
        for &(left, top) in &offsets[1..] {
            *offset.lock().unwrap() = (px(left), px(top));
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
        }
        let image = cx.capture_screenshot(window.into()).expect("screenshot");
        (image, first, terminal.stats())
    };

    // Slide right and down in whole pixels, the way a seam animates.
    let (slid, first, last) = draw(
        terminal(),
        &[(0.0, 0.0), (7.0, 0.0), (19.0, 3.0), (24.0, 12.0)],
    );
    let (fresh, _, _) = draw(terminal(), &[(24.0, 12.0)]);
    assert_eq!(slid.dimensions(), fresh.dimensions());
    assert!(
        fresh.pixels().any(|pixel| pixel != fresh.get_pixel(0, 0)),
        "fixture rendered a blank frame"
    );
    let differing = slid
        .pixels()
        .zip(fresh.pixels())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 0, "a moved terminal must paint identical pixels");

    // Every move reused every prepared row.
    assert!(
        first.frames > 0 && last.frames >= first.frames + 3,
        "{first:?} -> {last:?}"
    );
    assert_eq!(
        last.shape_cache_misses, first.shape_cache_misses,
        "a move prepared rows again: {first:?} -> {last:?}"
    );
}
