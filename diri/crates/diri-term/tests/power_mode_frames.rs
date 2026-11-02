//! Power mode through the real headless renderer: typing, deleting and a
//! cursor jump paint particles over the grid, and once they burn out the
//! frame is exactly what a terminal without power mode paints.
//!
//! `DIRI_VISUAL_OUTPUT=<dir>` also writes a scripted session as numbered PNG
//! frames at 30 fps, for turning into a GIF:
//! `cargo test -p diri-term --test power_mode_frames -- --ignored`.
#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use gpui::{AppContext as _, Context, IntoElement, Render, Window, px};

const COLS: u16 = 44;
const ROWS: u16 = 8;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.0.clone()
    }
}

/// The grid as a line editor sees it: rows of text and a cursor.
struct Screen {
    rows: Vec<String>,
    col: u16,
    row: u16,
}

impl Screen {
    fn update(&self, full: bool, changed: &[u16]) -> GridUpdate {
        let rows: Vec<u16> = if full {
            (0..ROWS).collect()
        } else {
            changed.to_vec()
        };
        GridUpdate {
            cols: COLS,
            rows: ROWS,
            cursor_col: self.col,
            cursor_row: self.row,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows: rows
                .into_iter()
                .map(|row| {
                    let text = self.rows.get(usize::from(row)).map_or("", String::as_str);
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
                    ChangedRow::new(row, cells)
                })
                .collect(),
            cursor_style: Default::default(),
            history: None,
        }
    }
}

enum Step {
    Type(char),
    Backspace,
    Enter(&'static str),
    Jump(u16, u16),
}

/// Applies one key the way a PTY echo would: the key, then the redraw.
fn press(terminal: &TerminalElement, screen: &mut Screen, step: &Step) {
    let row = usize::from(screen.row);
    let changed = match step {
        Step::Type(ch) => {
            terminal.note_power_input(ch.to_string().as_bytes());
            screen.rows[row].push(*ch);
            screen.col += 1;
            vec![screen.row]
        }
        Step::Backspace => {
            terminal.note_power_input(b"\x7f");
            screen.rows[row].pop();
            screen.col -= 1;
            vec![screen.row]
        }
        Step::Enter(indent) => {
            terminal.note_power_input(b"\r");
            screen.row += 1;
            screen.rows.push((*indent).to_owned());
            screen.col = indent.len() as u16;
            vec![screen.row]
        }
        Step::Jump(col, row) => {
            terminal.note_power_input(b"\x1b[A");
            (screen.col, screen.row) = (*col, *row);
            Vec::new()
        }
    };
    let _ = terminal.note_user_input();
    terminal.apply_damage(screen.update(false, &changed));
}

fn terminal(power: bool) -> (TerminalElement, Screen) {
    let screen = Screen {
        rows: vec![String::new()],
        col: 0,
        row: 0,
    };
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS))
        .focused(true)
        .font(gpui::font("Menlo"))
        .font_size(px(15.0))
        .power_mode(power);
    terminal.apply_damage(screen.update(true, &[]));
    (terminal, screen)
}

fn headless() -> gpui::HeadlessAppContext {
    let platform = gpui_platform::current_platform(true);
    gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    )
}

/// Runs `steps` one every `gap`, and returns the frame `after` the last key.
fn render(
    cx: &mut gpui::HeadlessAppContext,
    power: bool,
    steps: &[Step],
    after: Duration,
) -> image::RgbaImage {
    let start = Instant::now();
    let (terminal, mut screen) = terminal(power);
    terminal.set_cursor_clock(Some(start));
    let window = cx
        .open_window(gpui::size(px(440.0), px(170.0)), {
            let terminal = terminal.clone();
            move |_, cx| cx.new(|_| Fixture(terminal))
        })
        .expect("headless window");
    cx.run_until_parked();
    let gap = Duration::from_millis(60);
    let mut now = start;
    for step in steps {
        now += gap;
        terminal.set_cursor_clock(Some(now));
        press(&terminal, &mut screen, step);
    }
    terminal.set_cursor_clock(Some(now + after));
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    cx.capture_screenshot(window.into()).expect("screenshot")
}

fn hot_pixels(image: &image::RgbaImage) -> usize {
    // Sparks are saturated cyan or gold; the default theme paints neither.
    image
        .pixels()
        .filter(|pixel| {
            let [r, g, b, _] = pixel.0;
            let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
            (b > 150 && g > 150 && r < 160) || (r > 180 && g > 90 && b < 90)
        })
        .count()
}

#[test]
fn power_mode_paints_while_hot_and_nothing_once_burnt_out() {
    let mut cx = headless();
    let typing = [
        Step::Type('l'),
        Step::Type('s'),
        Step::Backspace,
        Step::Enter(""),
    ];
    let hot = render(&mut cx, true, &typing, Duration::from_millis(60));
    let plain = render(&mut cx, false, &typing, Duration::from_millis(60));
    assert!(
        hot_pixels(&hot) > hot_pixels(&plain) + 40,
        "power mode painted no particles: {} vs {}",
        hot_pixels(&hot),
        hot_pixels(&plain)
    );

    let cold = render(&mut cx, true, &typing, Duration::from_secs(2));
    let rested = render(&mut cx, false, &typing, Duration::from_secs(2));
    assert!(
        cold.pixels().eq(rested.pixels()),
        "burnt-out power mode left something on screen"
    );
}

#[test]
#[ignore = "writes frames for a GIF; set DIRI_VISUAL_OUTPUT"]
fn render_power_mode_frames() {
    let Ok(dir) = std::env::var("DIRI_VISUAL_OUTPUT") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    let mut script = Vec::new();
    let mut gap = |step: Step, ms: u64| script.push((step, Duration::from_millis(ms)));
    for ch in "const std = @import(\"std\");".chars() {
        gap(Step::Type(ch), 85);
    }
    gap(Step::Enter(""), 320);
    gap(Step::Enter(""), 260);
    for ch in "pub fn main() void {".chars() {
        gap(Step::Type(ch), 85);
    }
    gap(Step::Enter("    "), 320);
    for ch in "oops typo".chars() {
        gap(Step::Type(ch), 80);
    }
    for _ in 0.."oops typo".len() {
        gap(Step::Backspace, 110);
    }
    gap(Step::Jump(0, 0), 500);
    gap(Step::Jump(20, 3), 500);

    let mut cx = headless();
    let start = Instant::now();
    let (terminal, mut screen) = terminal(true);
    terminal.set_cursor_clock(Some(start));
    let window = cx
        .open_window(gpui::size(px(440.0), px(170.0)), {
            let terminal = terminal.clone();
            move |_, cx| cx.new(|_| Fixture(terminal))
        })
        .expect("headless window");
    cx.run_until_parked();

    let frame = Duration::from_millis(33);
    let mut next_key = start + Duration::from_millis(400);
    let mut steps = script.into_iter().peekable();
    let mut now = start;
    let mut index = 0;
    let end_after = Duration::from_millis(900);
    let mut finished_at = None;
    loop {
        while let Some((step, wait)) = steps.peek()
            && next_key <= now
        {
            terminal.set_cursor_clock(Some(next_key));
            press(&terminal, &mut screen, step);
            next_key += *wait;
            steps.next();
        }
        if steps.peek().is_none() {
            let done = *finished_at.get_or_insert(now);
            if now > done + end_after {
                break;
            }
        }
        terminal.set_cursor_clock(Some(now));
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        let image = cx.capture_screenshot(window.into()).expect("screenshot");
        image.save(format!("{dir}/frame-{index:04}.png")).unwrap();
        index += 1;
        now += frame;
    }
}
