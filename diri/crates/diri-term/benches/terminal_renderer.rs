use diri_proto::grid::{ChangedRow, GridCell, GridRowCodec, GridUpdate, TermColor, TermStyle};
use diri_proto::methods::ReadScrollbackCellsResult;
use diri_term::scrollback::{WheelDelta, WheelEvent};
use diri_term::{buffer::GridBuffer, element::TerminalElement, theme::TermTheme};
use gpui::{
    AppContext as _, BenchAppContext, Context, IntoElement, ParentElement, Render, Styled, Window,
    div,
};

const COLS: u16 = 160;
const ROWS: u16 = 50;
const MIN_GATED_FRAMES: u64 = 32;
const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);

struct TerminalBenchView {
    terminal: TerminalElement,
    frames: [GridUpdate; 2],
    next_frame: usize,
}

impl Render for TerminalBenchView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.terminal.clone())
    }
}

#[gpui::bench(fps = 120)]
fn terminal_build_log_scroll(cx: &mut BenchAppContext) {
    scroll(cx, plain_row);
}

/// An agent TUI rather than a build log: coloured words, syntax-coloured code
/// whose glyphs change colour without a gap, and block-element bars.
#[gpui::bench(fps = 120)]
fn terminal_styled_tui_scroll(cx: &mut BenchAppContext) {
    scroll(cx, styled_row);
}

/// A dashboard rather than a transcript: framed panes whose rules, junctions,
/// braille graphs and powerline segments are all drawn as sprites.
#[gpui::bench(fps = 120)]
fn terminal_framed_tui_scroll(cx: &mut BenchAppContext) {
    scroll(cx, framed_row);
}

/// A reader scrolled 30 rows into history while output keeps arriving under
/// the held view: every frame recomposes the same 50 rows.
#[gpui::bench(fps = 120)]
fn terminal_reading_view_redraw(cx: &mut BenchAppContext) {
    const HISTORY_ROWS: u16 = 30;
    let frames = [
        build_frame(1, false, plain_row),
        build_frame(0, false, plain_row),
    ];
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS)).focused(true);
    terminal.apply_damage(build_frame(0, true, plain_row));
    assert!(terminal.set_view_offset(i64::from(HISTORY_ROWS), usize::from(ROWS)));
    let history = (0..HISTORY_ROWS)
        .map(|row| {
            let mut cells = styled_row(usize::from(row));
            cells.resize(usize::from(COLS), GridCell::BLANK);
            cells
        })
        .collect::<Vec<_>>();
    terminal
        .complete_scrollback_fetch(
            ReadScrollbackCellsResult {
                metadata: Vec::new(),
                payload: GridRowCodec::encode_rows(&history).unwrap(),
                first_row: 1000 - i64::from(HISTORY_ROWS),
                row_count: i64::from(HISTORY_ROWS),
                live_start_row: 1000,
                total_rows: 1000 + i64::from(ROWS),
                cols: i64::from(COLS),
                content_seq: 1,
            },
            usize::from(ROWS),
        )
        .unwrap();

    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_window, _cx| TerminalBenchView {
            terminal,
            frames,
            next_frame: 0,
        })
    });
    cx.bench_renderer(view.clone(), |view, _window, cx| {
        view.terminal
            .apply_damage(view.frames[view.next_frame].clone());
        view.next_frame ^= 1;
        cx.notify();
    });
    let stats = cx.read_entity(&view, |view, _cx| view.terminal.stats());
    if stats.frames >= MIN_GATED_FRAMES {
        eprintln!(
            "terminal-reading: frames={}, average={:?}, shape-cache={}/{}",
            stats.frames,
            stats.average_frame_time(),
            stats.shape_cache_hits,
            stats.shape_cache_hits + stats.shape_cache_misses,
        );
    }
}

/// A trackpad fling up through about two thousand rows of history and back,
/// one precise wheel event per frame the way macOS delivers momentum. Fetches
/// are answered as the viewport asks for them. The cycle sums to zero, so it
/// repeats from the same place for as long as the harness runs.
#[gpui::bench(fps = 120)]
fn terminal_trackpad_fling(cx: &mut BenchAppContext) {
    const LIVE_START: i64 = 5_000;
    const LINE_HEIGHT: f32 = 16.0;
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS)).focused(true);
    terminal.apply_damage(build_frame(0, true, plain_row));
    let serve = |terminal: &TerminalElement| {
        while let Some(request) = terminal.begin_scrollback_fetch(usize::from(ROWS)) {
            let first = request.first_row.clamp(0, LIVE_START);
            let end = (request.first_row + request.max_rows).clamp(first, LIVE_START);
            let rows = (first..end)
                .map(|row| {
                    let mut cells = styled_row(row as usize);
                    cells.resize(usize::from(COLS), GridCell::BLANK);
                    cells
                })
                .collect::<Vec<_>>();
            terminal
                .complete_scrollback_fetch(
                    ReadScrollbackCellsResult {
                        metadata: Vec::new(),
                        payload: GridRowCodec::encode_rows(&rows).unwrap(),
                        first_row: first,
                        row_count: end - first,
                        live_start_row: LIVE_START,
                        total_rows: LIVE_START + i64::from(ROWS),
                        cols: i64::from(COLS),
                        content_seq: 1,
                    },
                    usize::from(ROWS),
                )
                .unwrap();
        }
    };
    assert!(terminal.set_view_offset(300, usize::from(ROWS)));
    serve(&terminal);

    // Momentum decays geometrically: 320 px on the first event, under a
    // pixel on the last, a little over 2,000 rows in total.
    let up = (0..600)
        .map(|event| 320.0 * 0.99f32.powi(event))
        .collect::<Vec<_>>();
    let fling = up
        .iter()
        .copied()
        .chain(up.iter().rev().map(|delta| -delta))
        .collect::<Vec<_>>();
    let mut next = 0;

    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_window, _cx| TerminalBenchView {
            terminal,
            frames: [
                build_frame(0, false, plain_row),
                build_frame(0, false, plain_row),
            ],
            next_frame: 0,
        })
    });
    cx.bench_renderer(view.clone(), move |view, _window, cx| {
        view.terminal.route_wheel(WheelEvent {
            delta: WheelDelta::PrecisePoints(fling[next]),
            col: 0,
            row: 0,
            visible_rows: ROWS,
            line_height: LINE_HEIGHT,
        });
        next = (next + 1) % fling.len();
        serve(&view.terminal);
        cx.notify();
    });
    let stats = cx.read_entity(&view, |view, _cx| view.terminal.stats());
    if stats.frames >= MIN_GATED_FRAMES {
        eprintln!(
            "terminal-fling: frames={}, average={:?}, max={:?}, shape-cache={}/{}",
            stats.frames,
            stats.average_frame_time(),
            stats.max_frame_time,
            stats.shape_cache_hits,
            stats.shape_cache_hits + stats.shape_cache_misses,
        );
        assert!(
            stats.average_frame_time() < FRAME_BUDGET,
            "a fling through history exceeded its {FRAME_BUDGET:?} safety budget: {stats:?}",
        );
    }
}

struct ThemeFadeBenchView {
    terminal: TerminalElement,
    step: u16,
}

impl Render for ThemeFadeBenchView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // A triangle wave over the fade, so no two consecutive frames share a
        // theme and nothing colored survives in a cache.
        const STEPS: u16 = 24;
        let phase = self.step % (2 * STEPS);
        let t = f32::from(if phase < STEPS {
            phase
        } else {
            2 * STEPS - phase
        }) / f32::from(STEPS);
        let theme = TermTheme::TOKYO_NIGHT.mix(&TermTheme::GRUVBOX_LIGHT, t.clamp(0.01, 0.99));
        div().size_full().child(self.terminal.clone().theme(theme))
    }
}

/// The worst frame a theme crossfade can ask for: a full 200x60 screen of
/// colored text with nothing else changing, recolored on every frame. Colors
/// are part of a shaped run, so each frame reshapes every row.
#[gpui::bench(fps = 120)]
fn terminal_theme_fade_redraw(cx: &mut BenchAppContext) {
    const FADE_COLS: u16 = 200;
    const FADE_ROWS: u16 = 60;
    let changed_rows = (0..FADE_ROWS)
        .map(|row| {
            let mut cells = styled_row(usize::from(row));
            cells.resize(usize::from(FADE_COLS), GridCell::BLANK);
            ChangedRow::new(row, cells)
        })
        .collect();
    let terminal =
        TerminalElement::with_buffer(GridBuffer::new(FADE_COLS, FADE_ROWS)).focused(true);
    terminal.apply_damage(GridUpdate {
        cols: FADE_COLS,
        rows: FADE_ROWS,
        cursor_col: 0,
        cursor_row: FADE_ROWS - 1,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows,
    });

    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_window, _cx| ThemeFadeBenchView { terminal, step: 0 })
    });
    cx.bench_renderer(view.clone(), |view, _window, cx| {
        view.step = view.step.wrapping_add(1);
        cx.notify();
    });
    let stats = cx.read_entity(&view, |view, _cx| view.terminal.stats());
    if stats.frames >= MIN_GATED_FRAMES {
        eprintln!(
            "terminal-theme-fade: frames={}, average={:?}, max={:?}",
            stats.frames,
            stats.average_frame_time(),
            stats.max_frame_time,
        );
        assert!(
            stats.average_frame_time() < FRAME_BUDGET,
            "a theme fade must fit the {:?} frame budget: {:?}",
            FRAME_BUDGET,
            stats.average_frame_time(),
        );
    }
}

fn scroll(cx: &mut BenchAppContext, row: fn(usize) -> Vec<GridCell>) {
    let initial = build_frame(0, true, row);
    let frames = [build_frame(1, false, row), build_frame(0, false, row)];
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, ROWS)).focused(true);
    terminal.apply_damage(initial);

    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_window, _cx| TerminalBenchView {
            terminal,
            frames,
            next_frame: 0,
        })
    });

    // The update changes every row, as one newline does to a full terminal.
    // The renderer must reuse shaping by content as surviving rows move.
    cx.bench_renderer(view.clone(), |view, _window, cx| {
        view.terminal
            .apply_damage(view.frames[view.next_frame].clone());
        view.next_frame ^= 1;
        cx.notify();
    });

    let stats = cx.read_entity(&view, |view, _cx| view.terminal.stats());
    eprintln!(
        "terminal-renderer: frames={}, average={:?}, max={:?}, shape-cache={}/{}",
        stats.frames,
        stats.average_frame_time(),
        stats.max_frame_time,
        stats.shape_cache_hits,
        stats.shape_cache_hits + stats.shape_cache_misses,
    );
    // Criterion begins calibration with batches of only 2, 3, 5… frames. On
    // fresh CI machines those batches include one-time font and Metal startup,
    // which is not scrolling throughput. Enforce the budget as soon as the
    // batch is large enough to represent steady state; every measured sample
    // is comfortably above this boundary.
    if stats.frames >= MIN_GATED_FRAMES {
        assert!(
            stats.shape_cache_hits * 10 > (stats.shape_cache_hits + stats.shape_cache_misses) * 9,
            "scrolling must reuse at least 90% of surviving row shapes: {stats:?}",
        );
        assert!(
            stats.average_frame_time() < FRAME_BUDGET,
            "terminal renderer CPU exceeded its {:?} safety budget across {} frames: {:?}",
            FRAME_BUDGET,
            stats.frames,
            stats.average_frame_time(),
        );
    }
}

fn cells(text: &str, fg: TermColor) -> impl Iterator<Item = GridCell> + '_ {
    text.chars().map(move |ch| {
        GridCell::new(
            u32::from(ch),
            fg,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        )
    })
}

fn plain_row(line_id: usize) -> Vec<GridCell> {
    let line = format!(
        "[{line_id:02}] Compiling terminal renderer target {line_id:05} with cached dependencies",
    );
    cells(&line, TermColor::Default).collect()
}

fn styled_row(line_id: usize) -> Vec<GridCell> {
    let id = format!("{line_id:05}");
    match line_id % 3 {
        0 => [
            "Updated",
            &id,
            "files",
            "in",
            "crates/diri-term",
            "with",
            "cached",
            "shapes",
        ]
        .into_iter()
        .enumerate()
        .flat_map(|(word, text)| {
            cells(text, TermColor::Ansi(1 + (word % 6) as u8)).chain(cells(" ", TermColor::Default))
        })
        .collect(),
        1 => [
            "let", " ", "row", "_", &id, "=", "cache", ".", "get", "(", "absolute", ")", "?", ";",
        ]
        .into_iter()
        .enumerate()
        .flat_map(|(token, text)| cells(text, TermColor::Ansi(1 + (token % 6) as u8)))
        .collect(),
        _ => cells(&"█".repeat(40 + line_id % 20), TermColor::Ansi(2))
            .chain(cells(&"░".repeat(20), TermColor::Ansi(8)))
            .chain(cells(&format!(" {id}%"), TermColor::Default))
            .collect(),
    }
}

fn framed_row(line_id: usize) -> Vec<GridCell> {
    let id = format!("{line_id:05}");
    let frame = TermColor::Ansi(4);
    match line_id % 4 {
        0 => cells(
            &format!("├─┤ {id} ├{}┬{}┤", "─".repeat(90), "─".repeat(40)),
            frame,
        )
        .collect(),
        1 => cells("│ ", frame)
            .chain(cells(
                &format!("session {id} running cargo test"),
                TermColor::Default,
            ))
            .chain(cells(&" ".repeat(69), TermColor::Default))
            .chain(cells("│", frame))
            .chain(cells(&format!(" cpu {id}% "), TermColor::Ansi(2)))
            .collect(),
        2 => {
            let graph: String = (0..96)
                .map(|col| {
                    char::from_u32(0x2800 + ((line_id * 7 + col * 13) % 256) as u32).unwrap()
                })
                .collect();
            cells("│", frame)
                .chain(cells(&graph, TermColor::Ansi(2)))
                .chain(cells("│", frame))
                .chain(cells(&format!(" {id} "), TermColor::Default))
                .collect()
        }
        _ => cells(&format!("╰{}┴{}╯ ", "─".repeat(98), "─".repeat(30)), frame)
            .chain(cells(
                &format!("\u{e0b2} {id} \u{e0b3} main \u{e0b0}"),
                TermColor::Ansi(5),
            ))
            .collect(),
    }
}

fn build_frame(
    offset: usize,
    is_full_snapshot: bool,
    content: fn(usize) -> Vec<GridCell>,
) -> GridUpdate {
    let changed_rows = (0..ROWS)
        .map(|row| {
            let mut cells = content(usize::from(row) + offset);
            cells.resize(usize::from(COLS), GridCell::BLANK);
            ChangedRow::new(row, cells)
        })
        .collect();
    GridUpdate {
        cols: COLS,
        rows: ROWS,
        cursor_col: 0,
        cursor_row: ROWS - 1,
        cursor_visible: true,
        is_full_snapshot,
        changed_rows,
    }
}

gpui::bench_group!(
    benches,
    terminal_build_log_scroll,
    terminal_styled_tui_scroll,
    terminal_framed_tui_scroll,
    terminal_reading_view_redraw,
    terminal_trackpad_fling,
    terminal_theme_fade_redraw
);
gpui::bench_main!(benches);
