//! Every SGR attribute and color form a program can write, through the
//! shared parser, the grid wire and the client buffer, checked against what
//! xterm (and libghostty) make of the same bytes. The ignored test renders the
//! same screens headlessly for review:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/matrix cargo test -p diri-term --test style_matrix -- --ignored
//! ```
//!
//! It writes `matrix.png` and `matrix-light.png` (every combination),
//! `recolor.png` (text on screen when a program then redefines red with OSC 4,
//! published only if the Engine's change gate lets it through),
//! `default-colors.png` (OSC 10/11) and `restored-history.png` (decorated
//! history after a checkpoint restore).
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::grid::{ChangedRow, GridUpdate, TermColor, TermStyle};
use diri_proto::terminal::MouseModes;
use diri_term::{buffer::GridBuffer, element::TerminalElement, theme::TermTheme};
use diri_terminal_state::HeadlessScreen;
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: usize = 64;
const ROWS: usize = 14;

use TermColor::{Ansi, Default as Fg, DefaultInverted as Bg, Rgb};

const B: TermStyle = TermStyle::BOLD;
const F: TermStyle = TermStyle::DIM;
const I: TermStyle = TermStyle::ITALIC;
const U: TermStyle = TermStyle::UNDERLINE;
const K: TermStyle = TermStyle::BLINK;
const R: TermStyle = TermStyle::INVERSE;
const H: TermStyle = TermStyle::INVISIBLE;
const S: TermStyle = TermStyle::CROSSED_OUT;
const N: TermStyle = TermStyle::empty();

/// One labelled word per entry: its SGR, then the wire colors and style
/// bits xterm semantics give it. Rows are separated by `None`.
type Case = (&'static str, &'static str, TermColor, TermColor, TermStyle);

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn cases() -> Vec<Option<Case>> {
    let mut rows: Vec<Option<Case>> = vec![
        Some(("pl", "0", Fg, Bg, N)),
        Some(("bold", "1", Fg, Bg, B)),
        Some(("faint", "2", Fg, Bg, F)),
        Some(("ital", "3", Fg, Bg, I)),
        Some(("under", "4", Fg, Bg, U)),
        Some(("blink", "5", Fg, Bg, K)),
        Some(("inv", "7", Fg, Bg, R)),
        Some(("hid", "8", Fg, Bg, H)),
        Some(("strike", "9", Fg, Bg, S)),
        Some(("b+f", "1;2", Fg, Bg, B | F)),
        None,
        Some(("b+inv", "1;7", Fg, Bg, B | R)),
        Some(("f+inv", "2;7", Fg, Bg, F | R)),
        Some(("h+inv", "7;8", Fg, Bg, R | H)),
        Some(("h+st", "8;9", Fg, Bg, H | S)),
        Some(("h+ul", "4;8", Fg, Bg, U | H)),
        Some(("f+st", "2;9", Fg, Bg, F | S)),
        Some(("21", "4;21", Fg, Bg, U)),
        Some(("22", "1;2;22", Fg, Bg, N)),
        None,
    ];
    let row = |prefix: &str, base: u8| {
        (0..8u8)
            .map(|n| {
                (
                    leak(format!("{prefix}{n}")),
                    leak(format!("{}", base + n)),
                    n,
                )
            })
            .collect::<Vec<_>>()
    };
    for (label, sgr, n) in row("F", 30) {
        rows.push(Some((label, sgr, Ansi(n), Bg, N)));
    }
    for (label, sgr, n) in row("L", 90) {
        rows.push(Some((label, sgr, Ansi(n + 8), Bg, N)));
    }
    rows.push(None);
    // Bold does not brighten 0-7, as in libghostty's default: SGR 1;31 is
    // palette 1 in a bold face. Faint dims whatever color it is given.
    for (label, sgr, n) in row("b", 30) {
        let sgr = leak(format!("1;{sgr}"));
        rows.push(Some((label, sgr, Ansi(n), Bg, B)));
    }
    for (label, sgr, n) in row("d", 30) {
        let sgr = leak(format!("2;{sgr}"));
        rows.push(Some((label, sgr, Ansi(n), Bg, F)));
    }
    rows.push(None);
    for (label, sgr, n) in row("K", 40) {
        rows.push(Some((label, sgr, Fg, Ansi(n), N)));
    }
    for (label, sgr, n) in row("J", 100) {
        rows.push(Some((label, sgr, Fg, Ansi(n + 8), N)));
    }
    rows.push(None);
    for n in [0u8, 7, 8, 15, 16, 17, 21, 46, 196, 231, 232, 244, 255] {
        let label = leak(format!("{n}"));
        let sgr = leak(format!("38;5;{n}"));
        rows.push(Some((label, sgr, Ansi(n), Bg, N)));
    }
    rows.push(None);
    for n in [0u8, 7, 8, 15, 16, 17, 21, 46, 196, 231, 232, 244, 255] {
        let label = leak(format!("{n}"));
        let sgr = leak(format!("48;5;{n}"));
        rows.push(Some((label, sgr, Fg, Ansi(n), N)));
    }
    rows.push(None);
    rows.extend([
        Some(("rgb0", "38;2;0;0;0", Rgb(0, 0, 0), Bg, N)),
        Some(("rgb1", "38;2;1;2;3", Rgb(1, 2, 3), Bg, N)),
        Some(("rgbW", "38:2::255:255:255", Rgb(255, 255, 255), Bg, N)),
        Some(("bg0", "48;2;0;0;0", Fg, Rgb(0, 0, 0), N)),
        Some(("bgW", "48;2;255;255;255", Fg, Rgb(255, 255, 255), N)),
        Some(("dT", "2;38;2;200;100;50", Rgb(200, 100, 50), Bg, F)),
        Some(("d196", "2;38;5;196", Ansi(196), Bg, F)),
        Some(("bT", "1;38;2;205;0;0", Rgb(205, 0, 0), Bg, B)),
        None,
        // Inverse with default and explicit colors; explicit black is not
        // the default.
        Some(("i", "7", Fg, Bg, R)),
        Some(("i30", "7;30", Ansi(0), Bg, R)),
        Some(("i39", "7;39", Fg, Bg, R)),
        Some(("i40", "7;40", Fg, Ansi(0), R)),
        Some(("i3142", "7;31;42", Ansi(1), Ansi(2), R)),
        Some(("iTb", "7;48;2;0;0;255", Fg, Rgb(0, 0, 255), R)),
        Some(("i2", "7;2", Fg, Bg, R | F)),
        Some(("i831", "7;8;31", Ansi(1), Bg, R | H)),
        Some(("k/k", "30;40", Ansi(0), Ansi(0), N)),
        Some(("d/d", "39;49", Fg, Bg, N)),
        None,
    ]);
    rows
}

fn matrix_bytes() -> Vec<u8> {
    let mut bytes = b"\x1b[H\x1b[2J".to_vec();
    let mut first = true;
    for case in cases() {
        match case {
            Some((label, sgr, ..)) => {
                if !first {
                    bytes.push(b' ');
                }
                first = false;
                bytes.extend(format!("\x1b[{sgr}m{label}\x1b[0m").as_bytes());
            }
            None => {
                bytes.extend(b"\r\n");
                first = true;
            }
        }
    }
    bytes
}

fn host_screen() -> HeadlessScreen {
    HeadlessScreen::new(COLS, ROWS).with_host_replies("matrix")
}

/// The grid as the client holds it: encoded, decoded and applied.
fn through_the_wire(update: &GridUpdate, buffer: &mut GridBuffer) {
    let decoded = GridUpdate::decode(&update.encode().expect("encode")).expect("decode");
    buffer.apply(decoded);
}

#[test]
fn every_style_and_color_reaches_the_client_as_written() {
    let mut screen = host_screen();
    screen.feed(&matrix_bytes());
    let mut buffer = GridBuffer::new(COLS as u16, ROWS as u16);
    through_the_wire(&screen.full_snapshot(), &mut buffer);
    let (mut row, mut col) = (0usize, 0usize);
    let mut checked = 0;
    for case in cases() {
        let Some((label, sgr, fg, bg, style)) = case else {
            row += 1;
            col = 0;
            continue;
        };
        let cells = buffer.row(row).expect("row");
        for offset in 0..label.len() {
            let cell = cells[col + offset];
            let paint = TermStyle::from_bits_retain(cell.style.bits() & 0xff);
            assert_eq!(
                (cell.fg, cell.bg, paint),
                (fg, bg, style),
                "`{label}` (SGR {sgr}) at row {row} col {}",
                col + offset
            );
        }
        // The separating space is unstyled.
        if let Some(space) = cells.get(col + label.len()) {
            assert_eq!((space.fg, space.bg, space.style.bits() & 0xff), (Fg, Bg, 0));
        }
        col += label.len() + 1;
        checked += 1;
    }
    assert!(checked > 90);
}

#[test]
fn renderer_resolves_the_matrix_like_xterm() {
    let theme = TermTheme::default();
    let resolve =
        |fg, bg, style| theme.resolve_cell(diri_proto::grid::GridCell::new(65, fg, bg, style));
    // Inverse swaps the two colors, defaults included.
    let inverse = resolve(Fg, Bg, R);
    assert_eq!(
        (inverse.foreground, inverse.background),
        (theme.background, theme.foreground)
    );
    let inverse_black = resolve(Ansi(0), Bg, R);
    assert_eq!(
        (inverse_black.foreground, inverse_black.background),
        (theme.background, theme.ansi[0])
    );
    // Hidden hides the glyph and its decorations, not the (inverse) cell.
    let hidden = resolve(Ansi(1), Bg, R | H | U | S);
    assert_eq!(
        (hidden.visible, hidden.underline, hidden.strikethrough),
        (false, false, false)
    );
    assert_eq!(hidden.foreground.a, 0.0);
    assert_eq!(hidden.background, theme.ansi[1]);
    // Bold keeps its color; the 256-color cube and ramp are exact.
    assert_eq!(resolve(Ansi(1), Bg, B).foreground, theme.ansi[1]);
    for (index, rgb) in [
        (17u8, 0x00005f),
        (196, 0xff0000),
        (232, 0x080808),
        (244, 0x808080),
    ] {
        let color = resolve(Ansi(index), Bg, N).foreground;
        let byte = |channel: f32| (channel * 255.0).round() as u32;
        assert_eq!(
            byte(color.r) << 16 | byte(color.g) << 8 | byte(color.b),
            rgb,
            "{index}"
        );
    }
    // Faint dims every kind of color, and only the foreground.
    for fg in [Fg, Ansi(1), Ansi(196), Rgb(200, 100, 50)] {
        let normal = resolve(fg, Bg, N);
        let faint = resolve(fg, Bg, F);
        assert_ne!(faint.foreground, normal.foreground, "{fg:?}");
        assert_eq!(faint.background, normal.background);
    }
}

#[test]
fn a_program_recolor_reaches_cells_already_on_screen() {
    let mut screen = host_screen();
    screen.feed(b"\x1b[31mred\x1b[0m \x1b[41mbg\x1b[0m plain");
    let mut buffer = GridBuffer::new(COLS as u16, ROWS as u16);
    through_the_wire(&screen.grid_update(true), &mut buffer);
    let published = screen.content_seq();
    screen.feed(b"\x1b]4;1;rgb:12/34/56\x1b\\");
    // The Engine publishes a frame only when the content sequence moved.
    assert_ne!(screen.content_seq(), published);
    through_the_wire(&screen.grid_update(false), &mut buffer);
    let cells = buffer.row(0).unwrap();
    assert_eq!(cells[0].fg, Rgb(0x12, 0x34, 0x56));
    assert_eq!(cells[4].bg, Rgb(0x12, 0x34, 0x56));
    assert_eq!(cells[7].fg, Fg);
}

// ---------------------------------------------------------------------------
// Headless renders
// ---------------------------------------------------------------------------

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn output(name: &str) -> PathBuf {
    let directory = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(PathBuf::from)
        .expect("DIRI_VISUAL_OUTPUT names the output directory");
    std::fs::create_dir_all(&directory).expect("output directory");
    directory.join(name)
}

fn capture(cx: &mut gpui::HeadlessAppContext, buffer: GridBuffer, theme: TermTheme, name: &str) {
    let (cols, rows) = (buffer.cols, buffer.rows);
    let window_size = gpui::size(px(f32::from(cols) * 9.1), px(f32::from(rows) * 18.3));
    let element = TerminalElement::with_buffer(buffer)
        .focused(true)
        .theme(theme)
        .font(gpui::font("Menlo"));
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

fn buffer_of(update: &GridUpdate) -> GridBuffer {
    let mut buffer = GridBuffer::new(update.cols, update.rows);
    through_the_wire(update, &mut buffer);
    buffer
}

// One test: two headless platforms cannot start concurrently.
#[test]
#[ignore = "writes PNG renders for review"]
fn render_style_matrix() {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );

    let mut screen = host_screen();
    screen.feed(&matrix_bytes());
    screen.feed(b"$ ");
    let matrix = screen.full_snapshot();
    capture(
        &mut cx,
        buffer_of(&matrix),
        TermTheme::default(),
        "matrix.png",
    );
    capture(
        &mut cx,
        buffer_of(&matrix),
        TermTheme::DIRIJOR_LIGHT,
        "matrix-light.png",
    );

    // Text already on screen, then a program redefines red. The client only
    // gets what the Engine's gate publishes: a frame when `content_seq` moves.
    let mut screen = HeadlessScreen::new(40, 4).with_host_replies("matrix");
    screen.feed(
        b"\x1b[31merror:\x1b[0m build failed \x1b[41m  \x1b[0m\r\n\x1b[4:3;58;5;1mundercurl\x1b[0m",
    );
    let mut client = buffer_of(&screen.grid_update(true));
    let published = screen.content_seq();
    screen.feed(b"\x1b]4;1;rgb:40/a0/ff\x1b\\");
    if screen.content_seq() != published {
        through_the_wire(&screen.grid_update(false), &mut client);
    }
    capture(&mut cx, client, TermTheme::default(), "recolor.png");

    // A program sets the default colors (base16-shell, `tinted-shell`).
    let mut screen = HeadlessScreen::new(40, 4).with_host_replies("matrix");
    screen.feed(b"plain text \x1b[7minverse\x1b[0m \x1b[32mgreen\x1b[0m\r\n");
    let mut client = buffer_of(&screen.grid_update(true));
    let published = screen.content_seq();
    screen.feed(b"\x1b]10;rgb:3c/38/36\x1b\\\x1b]11;rgb:fb/f1/c7\x1b\\");
    if screen.content_seq() != published {
        through_the_wire(&screen.grid_update(false), &mut client);
    }
    capture(&mut cx, client, TermTheme::default(), "default-colors.png");

    // Decorated history through a checkpoint: what the reading view shows
    // after an Engine restart adopted the session.
    let mut source = HeadlessScreen::new(40, 3);
    source.feed(
        b"\x1b[5mblink\x1b[0m \x1b[4:3;58:2::255:80:80mcurly red\x1b[0m \x1b[53;4:2movers\x1b[0m\r\n\r\n\r\n\r\n",
    );
    let history = source.history_snapshot();
    let metadata = source.history_metadata();
    let mut restored = HeadlessScreen::new(40, 3);
    assert!(restored.restore(
        &history,
        &source.full_snapshot(),
        false,
        false,
        MouseModes::OFF
    ));
    restored.restore_history_metadata(&metadata);
    let rows: Vec<ChangedRow> = restored
        .history_snapshot()
        .into_iter()
        .zip(restored.history_metadata())
        .enumerate()
        .map(|(y, (cells, metadata))| {
            let mut row = ChangedRow::new(y as u16, cells);
            row.metadata = metadata;
            row
        })
        .collect();
    let update = GridUpdate {
        cols: 40,
        rows: rows.len() as u16,
        cursor_col: 0,
        cursor_row: 0,
        cursor_visible: false,
        is_full_snapshot: true,
        changed_rows: rows,
        cursor_style: Default::default(),
        history: None,
    };
    capture(
        &mut cx,
        buffer_of(&update),
        TermTheme::default(),
        "restored-history.png",
    );
}
