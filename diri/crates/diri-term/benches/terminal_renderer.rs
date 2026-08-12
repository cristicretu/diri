use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use gpui::{
    AppContext as _, BenchAppContext, Context, IntoElement, ParentElement, Render, Styled, Window,
    div,
};

const COLS: u16 = 160;
const ROWS: u16 = 50;

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
    let initial = build_frame(0, true);
    let frames = [build_frame(1, false), build_frame(0, false)];
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
    assert!(
        stats.average_frame_time() < std::time::Duration::from_millis(8),
        "terminal renderer CPU exceeded its 120 Hz safety budget: {:?}",
        stats.average_frame_time()
    );
}

fn build_frame(offset: usize, is_full_snapshot: bool) -> GridUpdate {
    let changed_rows = (0..ROWS)
        .map(|row| {
            let line_id = usize::from(row) + offset;
            let line = format!(
                "[{line_id:02}] Compiling terminal renderer target {line_id:05} with cached dependencies",
            );
            let mut cells = line
                .chars()
                .map(|ch| {
                    GridCell::new(
                        u32::from(ch),
                        TermColor::Default,
                        TermColor::DefaultInverted,
                        TermStyle::empty(),
                    )
                })
                .collect::<Vec<_>>();
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

gpui::bench_group!(benches, terminal_build_log_scroll);
gpui::bench_main!(benches);
