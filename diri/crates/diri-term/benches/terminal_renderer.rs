use diri_proto::grid::{ChangedRow, GridCell, GridRowCodec, GridUpdate, TermColor, TermStyle};
use diri_proto::methods::ReadScrollbackCellsResult;
use diri_term::{buffer::GridBuffer, element::TerminalElement};
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
    terminal_reading_view_redraw
);
gpui::bench_main!(benches);
