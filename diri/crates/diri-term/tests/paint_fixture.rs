//! Raw-pixel fixture for paint-path changes.
//!
//! Renderer optimizations must not move a pixel. This renders one screen that
//! exercises every paint layer (backgrounds, selection and find overlays, text
//! in adjacent colors and styles, decorations, block elements, the cursor) in
//! both the live and the reading path, and dumps the RGBA bytes so two builds
//! can be compared with `cmp`:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/before cargo test -p diri-term --test paint_fixture -- --ignored
//! ```
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::grid::{
    ChangedRow, GridCell, GridRowCodec, GridUpdate, RowMetadata, TermColor, TermStyle,
};
use diri_proto::methods::ReadScrollbackCellsResult;
use diri_term::{buffer::GridBuffer, element::TerminalElement, find::FindSpan};
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: u16 = 60;
const ROWS: u16 = 22;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn styled(text: &str, fg: TermColor, bg: TermColor, style: TermStyle) -> Vec<GridCell> {
    text.chars()
        .map(|ch| GridCell::new(u32::from(ch), fg, bg, style))
        .collect()
}

fn fg(text: &str, color: u8) -> Vec<GridCell> {
    styled(
        text,
        TermColor::Ansi(color),
        TermColor::DefaultInverted,
        TermStyle::empty(),
    )
}

fn plain(text: &str) -> Vec<GridCell> {
    styled(
        text,
        TermColor::Default,
        TermColor::DefaultInverted,
        TermStyle::empty(),
    )
}

fn wide(text: &str) -> Vec<GridCell> {
    text.chars()
        .flat_map(|ch| {
            [
                GridCell::new(
                    u32::from(ch),
                    TermColor::Default,
                    TermColor::DefaultInverted,
                    TermStyle::empty(),
                ),
                GridCell::new(
                    0,
                    TermColor::Default,
                    TermColor::DefaultInverted,
                    TermStyle::WIDE_SPACER,
                ),
            ]
        })
        .collect()
}

fn rows() -> Vec<Vec<GridCell>> {
    let default_bg = TermColor::DefaultInverted;
    let none = TermStyle::empty();
    let mut rows = vec![
        plain("plain default text, selected across two rows"),
        [
            fg("red", 1),
            plain(" "),
            fg("green", 2),
            plain("  "),
            fg("blue", 4),
        ]
        .concat(),
        // Adjacent glyphs in different colors: paint order between them is
        // observable wherever their antialiased edges share a pixel.
        [
            fg("call", 3),
            fg("(", 1),
            fg("arg", 6),
            fg(")", 1),
            fg("[", 5),
            fg("WMW", 2),
            fg("]", 5),
        ]
        .concat(),
        [
            styled("bold", TermColor::Ansi(2), default_bg, TermStyle::BOLD),
            styled("fWf/", TermColor::Ansi(1), default_bg, TermStyle::ITALIC),
            styled("W/fW", TermColor::Ansi(4), default_bg, TermStyle::ITALIC),
            styled("dim", TermColor::Default, default_bg, TermStyle::DIM),
            styled(
                "both",
                TermColor::Ansi(3),
                default_bg,
                TermStyle::BOLD | TermStyle::ITALIC,
            ),
        ]
        .concat(),
        [
            styled(
                "under gjpqy",
                TermColor::Ansi(6),
                default_bg,
                TermStyle::UNDERLINE,
            ),
            plain(" "),
            styled(
                "struck",
                TermColor::Ansi(1),
                default_bg,
                TermStyle::CROSSED_OUT,
            ),
            styled(
                "both",
                TermColor::Default,
                default_bg,
                TermStyle::UNDERLINE | TermStyle::CROSSED_OUT,
            ),
        ]
        .concat(),
        [
            styled(
                "inverse",
                TermColor::Default,
                default_bg,
                TermStyle::INVERSE,
            ),
            styled(" on blue ", TermColor::Ansi(15), TermColor::Ansi(4), none),
            styled(
                "rgb",
                TermColor::Rgb(10, 20, 30),
                TermColor::Rgb(200, 180, 40),
                none,
            ),
            // Trailing blanks that still own a background.
            styled(
                "              ",
                TermColor::Default,
                TermColor::Ansi(5),
                none,
            ),
        ]
        .concat(),
        [
            fg("████████", 2),
            fg("████", 1),
            fg("▀▀▀▀▄▄▄▄", 3),
            fg("█▌█▐█▚▞█", 6),
        ]
        .concat(),
        [fg("░░▒▒▓▓", 1), fg("▓▓▒▒░░", 4), fg("█", 2), fg("▓", 3)].concat(),
        [fg("┌──┬──┐", 4), fg("╭─╮", 2)].concat(),
        [
            fg("│", 1),
            plain("ab"),
            fg("│", 2),
            plain("cd"),
            fg("│", 4),
            fg("│ │", 2),
        ]
        .concat(),
        [fg("└──┴──┘", 4), fg("╰─╯", 2)].concat(),
        [wide("日本語"), plain("text"), wide("🙂"), fg("ab", 1)].concat(),
        plain("cafe combining"),
        [
            plain("under"),
            // Trailing blanks that still own a decoration.
            styled(
                "          ",
                TermColor::Ansi(3),
                default_bg,
                TermStyle::UNDERLINE,
            ),
        ]
        .concat(),
        [
            plain("hidden>"),
            styled(
                "secret",
                TermColor::Ansi(1),
                default_bg,
                TermStyle::INVISIBLE,
            ),
            plain("<"),
        ]
        .concat(),
        plain("find match here and here"),
        fg("   X cursor sits on the X", 2),
        // Right-to-left text: its glyph positions depend on the whole line,
        // trailing blanks included.
        [fg("שלום", 3), plain(" rtl "), fg("مرحبا", 6)].concat(),
    ];
    for row in &mut rows {
        row.resize(usize::from(COLS), GridCell::BLANK);
    }
    rows.resize(usize::from(ROWS), vec![GridCell::BLANK; usize::from(COLS)]);
    // A full-width run of blocks on the last row reaches the right edge.
    rows[usize::from(ROWS) - 1] = fg(&"█".repeat(usize::from(COLS)), 5);
    rows
}

fn snapshot() -> GridUpdate {
    let changed_rows = rows()
        .into_iter()
        .enumerate()
        .map(|(y, cells)| {
            let mut row = ChangedRow::new(y as u16, cells);
            if y == 12 {
                row.metadata = RowMetadata {
                    links: Vec::new(),
                    graphemes: vec![(3, "\u{301}".into())],
                };
            }
            row
        })
        .collect();
    GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: 3,
        cursor_row: 16,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows,
    }
}

fn element() -> TerminalElement {
    let element = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS)).focused(true);
    element.apply_damage(snapshot());
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
        .open_window(gpui::size(px(520.0), px(400.0)), |_, cx| {
            cx.new(|_| Fixture(element))
        })
        .expect("headless window");
    cx.run_until_parked();
    // The second frame paints entirely from the row cache.
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    assert!(
        image.pixels().any(|pixel| pixel != image.get_pixel(0, 0)),
        "fixture rendered a blank frame"
    );
    let mut bytes = Vec::with_capacity(image.as_raw().len() + 8);
    bytes.extend_from_slice(&image.width().to_le_bytes());
    bytes.extend_from_slice(&image.height().to_le_bytes());
    bytes.extend_from_slice(image.as_raw());
    std::fs::write(output, bytes).expect("write fixture pixels");
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
#[ignore = "writes raw fixture pixels for before/after comparison"]
fn render_paint_fixtures() {
    // The app's terminal font, then the proportional fallback a missing font
    // resolves to: forced onto the cell grid, its glyphs overlap their
    // neighbours, which is the harshest case for paint order.
    render_live(element().font(gpui::font("Menlo")), "live.rgba");
    render_live(element(), "live-overlapping.rgba");
    render_reading();
}

fn render_live(element: TerminalElement, name: &str) {
    element.begin_selection(6, 0);
    element.drag_selection(9, 1);
    element.set_find_highlights(vec![
        FindSpan {
            row: 15,
            start_col: 0,
            end_col_exclusive: 4,
            is_current: true,
        },
        FindSpan {
            row: 6,
            start_col: 4,
            end_col_exclusive: 14,
            is_current: false,
        },
    ]);
    capture(element, output(name));
}

fn render_reading() {
    let element = element().font(gpui::font("Menlo"));
    let visible = usize::from(ROWS);
    assert!(element.set_view_offset(4, visible));
    let history = rows()[2..6].to_vec();
    element
        .complete_scrollback_fetch(
            ReadScrollbackCellsResult {
                metadata: Vec::new(),
                payload: GridRowCodec::encode_rows(&history).unwrap(),
                first_row: 96,
                row_count: 4,
                live_start_row: 100,
                total_rows: 100 + i64::from(ROWS),
                cols: i64::from(COLS),
                content_seq: 1,
            },
            visible,
        )
        .unwrap();
    element.begin_selection(2, 1);
    element.drag_selection(12, 5);
    capture(element, output("reading.rgba"));
}
