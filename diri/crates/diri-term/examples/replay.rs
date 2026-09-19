use std::time::{Duration, Instant};

use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::buffer::GridBuffer;
use diri_term::element::TerminalElement;
use gpui::{
    App, Bounds, Context, FocusHandle, MouseButton, Render, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_platform::application;

const COLS: u16 = 104;
const ROWS: u16 = 28;
const REPLAY_FRAMES: usize = 240;

struct ReplayDemo {
    terminal: TerminalElement,
    focus: FocusHandle,
    _replay_task: Task<()>,
}

impl ReplayDemo {
    // One window and no floating panels, so the view's window never changes.
    #[allow(clippy::disallowed_methods)]
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        let terminal =
            TerminalElement::with_buffer(GridBuffer::default()).focus_handle(focus.clone());
        let first = round_trip(full_snapshot());
        terminal.apply(first, window);
        terminal.reset_stats();

        let replay_task = cx.spawn_in(window, async move |this, cx| {
            let mut update_costs = Vec::with_capacity(REPLAY_FRAMES);
            let replay_started = Instant::now();
            for frame in 0..REPLAY_FRAMES {
                let deadline = replay_started + Duration::from_micros(16_667 * frame as u64);
                let now = Instant::now();
                if deadline > now {
                    cx.background_executor().timer(deadline - now).await;
                }

                let update_started = Instant::now();
                let update = round_trip(animated_diff(frame));
                if this
                    .update_in(cx, |this, window, _cx| {
                        this.terminal.apply(update, window);
                    })
                    .is_err()
                {
                    return;
                }
                update_costs.push(update_started.elapsed());
            }

            let stats = match this.update_in(cx, |this, _window, _cx| this.terminal.stats()) {
                Ok(stats) => stats,
                Err(_) => return,
            };
            let total_update: Duration = update_costs.iter().sum();
            let average_update = total_update.div_f64(update_costs.len() as f64);
            let max_update = update_costs.iter().copied().max().unwrap_or_default();
            let elapsed = replay_started.elapsed();
            println!(
                "replay: {} updates in {:.2}s ({:.1} updates/s), wire+apply avg {:?}, max {:?}",
                update_costs.len(),
                elapsed.as_secs_f64(),
                update_costs.len() as f64 / elapsed.as_secs_f64(),
                average_update,
                max_update,
            );
            println!(
                "renderer: {} frames, avg {:?}, max {:?}, shape cache hits {}, misses {}",
                stats.frames,
                stats.average_frame_time(),
                stats.max_frame_time,
                stats.shape_cache_hits,
                stats.shape_cache_misses,
            );
            println!("wire fidelity: all GridUpdates encoded and decoded byte-for-byte-equivalent");
        });

        Self {
            terminal,
            focus,
            _replay_task: replay_task,
        }
    }
}

impl Render for ReplayDemo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        div()
            .id("terminal-replay")
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&focus, cx);
            })
            .size_full()
            .child(self.terminal.clone())
    }
}

fn full_snapshot() -> GridUpdate {
    let mut rows = vec![vec![GridCell::BLANK; usize::from(COLS)]; usize::from(ROWS)];
    write_text(
        &mut rows[0],
        0,
        "diri-term GPU replay — synthetic protocol frames only",
        TermColor::Default,
        TermColor::DefaultInverted,
        TermStyle::BOLD,
    );
    write_text(
        &mut rows[1],
        0,
        "ANSI normal 0–7",
        TermColor::Default,
        TermColor::DefaultInverted,
        TermStyle::empty(),
    );
    for index in 0_u8..8 {
        let start = 20 + usize::from(index) * 10;
        write_text(
            &mut rows[1],
            start,
            &format!(" {index} ANSI "),
            TermColor::Ansi(index),
            TermColor::DefaultInverted,
            TermStyle::empty(),
        );
    }
    write_text(
        &mut rows[2],
        0,
        "ANSI bright 8–15",
        TermColor::Default,
        TermColor::DefaultInverted,
        TermStyle::empty(),
    );
    for index in 8_u8..16 {
        let start = 20 + usize::from(index - 8) * 10;
        write_text(
            &mut rows[2],
            start,
            &format!(" {index} ANSI"),
            TermColor::Ansi(index),
            TermColor::DefaultInverted,
            TermStyle::empty(),
        );
    }
    for (column, index) in (16_u8..=255).step_by(3).take(80).enumerate() {
        rows[4][column + 20] = GridCell::new(
            u32::from('●'),
            TermColor::Ansi(index),
            TermColor::DefaultInverted,
            TermStyle::empty(),
        );
    }
    write_text(
        &mut rows[4],
        0,
        "xterm 16–255",
        TermColor::Default,
        TermColor::DefaultInverted,
        TermStyle::empty(),
    );
    write_gradient(&mut rows[6], 0);
    write_text(
        &mut rows[8],
        0,
        "bold",
        TermColor::Ansi(10),
        TermColor::DefaultInverted,
        TermStyle::BOLD,
    );
    write_text(
        &mut rows[8],
        10,
        "italic",
        TermColor::Ansi(14),
        TermColor::DefaultInverted,
        TermStyle::ITALIC,
    );
    write_text(
        &mut rows[8],
        22,
        "underline",
        TermColor::Ansi(11),
        TermColor::DefaultInverted,
        TermStyle::UNDERLINE,
    );
    write_text(
        &mut rows[8],
        38,
        "inverse",
        TermColor::Ansi(15),
        TermColor::Ansi(4),
        TermStyle::INVERSE,
    );
    write_text(
        &mut rows[8],
        52,
        "strike",
        TermColor::Ansi(9),
        TermColor::DefaultInverted,
        TermStyle::CROSSED_OUT,
    );
    write_text(
        &mut rows[8],
        65,
        "dim",
        TermColor::Ansi(13),
        TermColor::DefaultInverted,
        TermStyle::DIM,
    );
    write_text(
        &mut rows[8],
        75,
        "invisible text",
        TermColor::Ansi(9),
        TermColor::DefaultInverted,
        TermStyle::INVISIBLE,
    );
    write_text(
        &mut rows[10],
        0,
        "fallbacks: ⏸ ⏵ ⎿ ☰ ✻ ✽ ✔ ✘  braille: ⠋⠙⠹⠸⠼⠴⠦⠧  emoji: 🚀",
        TermColor::Default,
        TermColor::DefaultInverted,
        TermStyle::empty(),
    );
    write_text(
        &mut rows[12],
        0,
        "scrolling output begins here",
        TermColor::Ansi(12),
        TermColor::DefaultInverted,
        TermStyle::BOLD,
    );
    for (row_index, row) in rows.iter_mut().enumerate().skip(14) {
        write_text(
            row,
            0,
            &format!("row {row_index:02}  cache-stable terminal content"),
            TermColor::Default,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        );
    }

    GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: 0,
        cursor_row: 12,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows: rows
            .into_iter()
            .enumerate()
            .map(|(y, cells)| ChangedRow::new(y as u16, cells))
            .collect(),
    }
}

fn animated_diff(frame: usize) -> GridUpdate {
    let mut scrolling = vec![GridCell::BLANK; usize::from(COLS)];
    let message = format!(
        "frame {frame:03}  The quick brown fox scrolls smoothly across a damage-driven GPU grid  «{}»",
        "─".repeat(44)
    );
    let chars: Vec<char> = message.chars().collect();
    for col in 0..scrolling.len() {
        scrolling[col] = GridCell::new(
            u32::from(chars[(col + frame) % chars.len()]),
            TermColor::Ansi(usize::wrapping_add(col, frame) as u8 % 16),
            TermColor::DefaultInverted,
            if col % 23 == 0 {
                TermStyle::UNDERLINE
            } else {
                TermStyle::empty()
            },
        );
    }

    let mut gradient = vec![GridCell::BLANK; usize::from(COLS)];
    write_gradient(&mut gradient, frame as u8);

    GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: (frame % usize::from(COLS)) as u16,
        cursor_row: 12 + ((frame / usize::from(COLS)) % 2) as u16,
        cursor_visible: true,
        is_full_snapshot: false,
        changed_rows: vec![ChangedRow::new(6, gradient), ChangedRow::new(12, scrolling)],
    }
}

fn write_gradient(row: &mut [GridCell], phase: u8) {
    for (column, cell) in row.iter_mut().enumerate() {
        let position = column as u8;
        *cell = GridCell::new(
            u32::from('█'),
            TermColor::Rgb(
                position.wrapping_mul(3).wrapping_add(phase),
                220_u8.wrapping_sub(position.wrapping_mul(2)),
                position.wrapping_mul(5).wrapping_add(phase),
            ),
            TermColor::DefaultInverted,
            TermStyle::empty(),
        );
    }
}

fn write_text(
    row: &mut [GridCell],
    start: usize,
    text: &str,
    foreground: TermColor,
    background: TermColor,
    style: TermStyle,
) {
    for (offset, ch) in text.chars().enumerate() {
        let Some(cell) = row.get_mut(start + offset) else {
            break;
        };
        *cell = GridCell::new(u32::from(ch), foreground, background, style);
    }
}

fn round_trip(update: GridUpdate) -> GridUpdate {
    let encoded = update.encode().expect("synthetic update must encode");
    let decoded = GridUpdate::decode(&encoded).expect("synthetic update must decode");
    assert_eq!(decoded, update, "grid wire round-trip changed the update");
    decoded
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(980.0), px(540.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_background: WindowBackgroundAppearance::Opaque,
                titlebar: None,
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ReplayDemo::new(window, cx)),
        )
        .expect("failed to open terminal replay window");
        cx.activate(true);
    });
}
