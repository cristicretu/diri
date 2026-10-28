//! A full-screen program that paints its own background (an editor
//! colourscheme, `htop`, `k9s`) used to sit inside a frame of the theme's
//! colour: the pane's padding. On the alternate screen the program's edge
//! backgrounds now carry out into that padding; a shell's screen keeps the
//! theme padding.
//!
//! `DIRI_VISUAL_OUTPUT=<dir>` also writes the frames as PNGs.
#![cfg(target_os = "macos")]

use std::sync::Arc;

use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement, theme::TermTheme};
use gpui::{
    AppContext as _, Context, Edges, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

const COLS: u16 = 30;
const ROWS: u16 = 8;
const PAD: f32 = 12.0;
const EDITOR: (u8, u8, u8) = (0x1f, 0x3a, 0x5f);
const STATUS: (u8, u8, u8) = (0xd0, 0x8c, 0x30);

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = TermTheme::default();
        div()
            .size_full()
            .bg(theme.background)
            .p(px(PAD))
            .child(div().size_full().child(self.0.clone()))
    }
}

fn line(text: &str, bg: (u8, u8, u8)) -> Vec<GridCell> {
    let bg = TermColor::Rgb(bg.0, bg.1, bg.2);
    let mut cells: Vec<GridCell> = text
        .chars()
        .map(|ch| GridCell::new(u32::from(ch), TermColor::Default, bg, TermStyle::empty()))
        .collect();
    cells.resize(
        usize::from(COLS),
        GridCell::new(u32::from(' '), TermColor::Default, bg, TermStyle::empty()),
    );
    cells
}

fn editor(alt_screen: bool) -> TerminalElement {
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS))
        .focused(false)
        .reduce_motion(true)
        .font(gpui::font("Menlo"))
        .padding_extend(Edges::all(px(PAD)));
    let rows = (0..ROWS)
        .map(|row| {
            let cells = if row + 1 == ROWS {
                line(" NORMAL  main.rs", STATUS)
            } else {
                line(&format!("{:>3} fn main() {{}}", row + 1), EDITOR)
            };
            ChangedRow::new(row, cells)
        })
        .collect();
    terminal.apply_damage(GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: 0,
        cursor_row: 0,
        cursor_visible: false,
        is_full_snapshot: true,
        changed_rows: rows,
        cursor_style: Default::default(),
    });
    terminal.set_modes(alt_screen, Default::default());
    terminal
}

fn rgb(pixel: &image::Rgba<u8>) -> (u8, u8, u8) {
    (pixel[0], pixel[1], pixel[2])
}

fn near(actual: (u8, u8, u8), expected: (u8, u8, u8)) -> bool {
    let d = |a: u8, b: u8| a.abs_diff(b) <= 3;
    d(actual.0, expected.0) && d(actual.1, expected.1) && d(actual.2, expected.2)
}

#[test]
fn a_full_screen_programs_background_fills_the_padding() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let mut draw = |alt_screen: bool| {
        let terminal = editor(alt_screen);
        let window = cx
            .open_window(gpui::size(px(320.0), px(160.0)), move |_, cx| {
                cx.new(|_| Fixture(terminal))
            })
            .expect("headless window");
        cx.run_until_parked();
        cx.capture_screenshot(window.into()).expect("screenshot")
    };
    let shell = draw(false);
    let program = draw(true);
    if let Some(dir) = std::env::var_os("DIRI_VISUAL_OUTPUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        shell.save(dir.join("padding-theme.png")).unwrap();
        program.save(dir.join("padding-extend.png")).unwrap();
    }

    let scale = program.width() as f32 / 320.0;
    let at = |image: &image::RgbaImage, x: f32, y: f32| {
        rgb(image.get_pixel((x * scale) as u32, (y * scale) as u32))
    };
    let theme = shell.get_pixel(1, 1);
    let theme = rgb(theme);
    assert!(
        !near(theme, EDITOR),
        "fixture: the theme must differ from the editor colour"
    );

    // Top-left corner, left side mid-height, right edge mid-height, and the
    // unused width between the last column and the padding, which the theme
    // fill under the grid must not cover.
    for (x, y) in [(2.0, 2.0), (4.0, 60.0), (316.0, 60.0), (280.0, 60.0)] {
        assert!(near(at(&shell, x, y), theme), "shell padding at ({x}, {y})");
        assert!(
            near(at(&program, x, y), EDITOR),
            "program padding at ({x}, {y}) is {:?}",
            at(&program, x, y)
        );
    }
    // The bottom padding follows the status line, not the editor body.
    // Rows below the last whole one count as bottom padding too.
    for x in [2.0, 160.0, 318.0] {
        assert!(
            near(at(&program, x, 145.0), STATUS),
            "sliver below at x={x}"
        );
        assert!(
            near(at(&program, x, 157.0), STATUS),
            "bottom padding at x={x} is {:?}",
            at(&program, x, 157.0)
        );
        assert!(near(at(&shell, x, 157.0), theme), "shell bottom at x={x}");
    }
}
