//! Frame cost of the terminal interactions other than typing, through GPUI's
//! production text system and the real headless Metal renderer: sliding a
//! seam or divider past a terminal, a live resize, switching to a session
//! (a fresh element's first frame), zooming, a selection drag and scrolling a
//! full-screen grid (a 4K display at 2x: 1920x1080 points). Each bench prints the element's own frame statistics; the
//! GPUI report after it gives p50/p95 draw time per frame.
use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use gpui::{
    AppContext as _, BenchAppContext, Context, IntoElement, ParentElement, Pixels, Render, Styled,
    Window, div, px,
};

const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_micros(8_333);
const MIN_GATED_FRAMES: u64 = 32;

struct View {
    terminal: TerminalElement,
    left: Pixels,
    width: Option<Pixels>,
    font_size: Pixels,
}

impl Render for View {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut container = div().h_full().pl(self.left);
        container = match self.width {
            Some(width) => container.w(width),
            None => container.w_full(),
        };
        container.child(self.terminal.clone().font_size(self.font_size))
    }
}

fn cells(text: &str, fg: TermColor) -> impl Iterator<Item = GridCell> + '_ {
    text.chars().flat_map(move |ch| {
        let wide = unicode_wide(ch);
        let cell = GridCell::new(
            u32::from(ch),
            fg,
            TermColor::DefaultInverted,
            TermStyle::empty(),
        );
        std::iter::once(cell).chain(wide.then_some(GridCell::BLANK))
    })
}

fn unicode_wide(ch: char) -> bool {
    matches!(u32::from(ch), 0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFF00..=0xFF60 | 0x1F300..=0x1FAFF)
}

/// A coloured build-log line; `wide` mixes CJK and emoji into every third.
fn log_row(line: usize, wide: bool) -> Vec<GridCell> {
    let color = TermColor::Ansi(1 + (line % 6) as u8);
    let module = "x".repeat(10 + line % 50);
    let head = if wide && line.is_multiple_of(3) {
        format!("[{line:010}] 構築中 crate_{} 🦀", line % 997)
    } else {
        format!(
            "[{line:010}] building crate_{} v0.{}.0",
            line % 997,
            line % 9
        )
    };
    cells(&head, color)
        .chain(cells("  Compiling module ", TermColor::Default))
        .chain(cells(&module, TermColor::Ansi(8)))
        .collect()
}

fn full_frame(cols: u16, rows: u16, offset: usize, wide: bool) -> GridUpdate {
    GridUpdate {
        cols,
        rows,
        cursor_col: 0,
        cursor_row: rows - 1,
        cursor_visible: true,
        is_full_snapshot: true,
        changed_rows: (0..rows)
            .map(|row| {
                let mut cells = log_row(usize::from(row) + offset, wide);
                cells.resize(usize::from(cols), GridCell::BLANK);
                ChangedRow::new(row, cells)
            })
            .collect(),
    }
}

fn open(cx: &mut BenchAppContext, terminal: TerminalElement) -> gpui::Entity<View> {
    let mut window = cx.add_empty_window();
    window.update(|window, cx| {
        window.replace_root(cx, |_window, _cx| View {
            terminal,
            left: px(0.0),
            width: None,
            font_size: px(13.0),
        })
    })
}

fn report(cx: &mut BenchAppContext, view: &gpui::Entity<View>, label: &str, gate: bool) {
    let stats = cx.read_entity(view, |view, _cx| view.terminal.stats());
    if stats.frames >= MIN_GATED_FRAMES {
        eprintln!(
            "{label}: frames={}, average={:?}, max={:?}, shape-cache={}/{}",
            stats.frames,
            stats.average_frame_time(),
            stats.max_frame_time,
            stats.shape_cache_hits,
            stats.shape_cache_hits + stats.shape_cache_misses,
        );
        if gate {
            assert!(
                stats.average_frame_time() < FRAME_BUDGET,
                "{label} exceeded the {FRAME_BUDGET:?} frame budget: {stats:?}",
            );
        }
    }
}

fn terminal(cols: u16, rows: u16, wide: bool) -> TerminalElement {
    let terminal = TerminalElement::with_buffer(GridBuffer::new(cols, rows)).focused(true);
    terminal.apply_damage(full_frame(cols, rows, 0, wide));
    terminal
}

/// A sidebar or inspector seam sliding past an unchanged terminal: only the
/// terminal's origin moves, one pixel per frame.
#[gpui::bench(fps = 120)]
fn interaction_seam_slide(cx: &mut BenchAppContext) {
    let view = open(cx, terminal(160, 50, false));
    let mut step = 0u32;
    cx.bench_renderer(view.clone(), move |view, _window, cx| {
        step = (step + 1) % 400;
        let x = if step < 200 { step } else { 400 - step };
        view.left = px(x as f32);
        cx.notify();
    });
    report(cx, &view, "seam-slide", true);
}

/// A live window or split resize: the pane narrows and widens by a pixel a
/// frame while the Engine's reflowed grid arrives on its own cadence. Here
/// the grid stays 160 columns; the pane clips it, which changes the visible
/// column count every few frames.
#[gpui::bench(fps = 120)]
fn interaction_resize_drag(cx: &mut BenchAppContext) {
    let view = open(cx, terminal(160, 50, false));
    let mut step = 0u32;
    cx.bench_renderer(view.clone(), move |view, _window, cx| {
        step = (step + 1) % 400;
        let delta = if step < 200 { step } else { 400 - step };
        view.width = Some(px(1600.0 - delta as f32));
        cx.notify();
    });
    report(cx, &view, "resize-drag", true);
}

/// The reflowed grid itself arriving during a drag: every other frame the
/// Engine publishes a full snapshot one column narrower or wider.
#[gpui::bench(fps = 120)]
fn interaction_reflow_arrival(cx: &mut BenchAppContext) {
    let frames: Vec<GridUpdate> = (0..40u16)
        .chain((0..40u16).rev())
        .map(|step| full_frame(120 + step, 50, 0, false))
        .collect();
    let view = open(cx, terminal(160, 50, false));
    let mut next = 0;
    cx.bench_renderer(view.clone(), move |view, _window, cx| {
        view.terminal.apply_damage(frames[next].clone());
        next = (next + 1) % frames.len();
        cx.notify();
    });
    report(cx, &view, "reflow-arrival", true);
}

fn switch(cx: &mut BenchAppContext, cols: u16, rows: u16, wide: bool, label: &str) {
    let seeds: Vec<GridUpdate> = (0..8)
        .map(|session| full_frame(cols, rows, session * 1000, wide))
        .collect();
    let view = open(cx, terminal(cols, rows, wide));
    let mut next = 0;
    let mut total = diri_term::element::RendererStats::default();
    cx.bench_renderer(view.clone(), |view, _window, cx| {
        let stats = view.terminal.stats();
        total.frames += stats.frames;
        total.total_frame_time += stats.total_frame_time;
        total.max_frame_time = total.max_frame_time.max(stats.max_frame_time);
        let fresh = TerminalElement::with_buffer(GridBuffer::new(cols, rows)).focused(true);
        fresh.apply_damage(seeds[next].clone());
        next = (next + 1) % seeds.len();
        view.terminal = fresh;
        cx.notify();
    });
    if total.frames >= MIN_GATED_FRAMES {
        eprintln!(
            "{label}: first frames={}, average={:?}, max={:?}",
            total.frames,
            total.average_frame_time(),
            total.max_frame_time,
        );
    }
}

/// Switching sessions: a fresh element seeded with a full snapshot draws its
/// first frame with nothing cached.
#[gpui::bench(fps = 120)]
fn interaction_switch_160x50(cx: &mut BenchAppContext) {
    switch(cx, 160, 50, false, "switch-160x50");
}

#[gpui::bench(fps = 120)]
fn interaction_switch_160x50_cjk(cx: &mut BenchAppContext) {
    switch(cx, 160, 50, true, "switch-160x50-cjk");
}

#[gpui::bench(fps = 120)]
fn interaction_switch_240x66(cx: &mut BenchAppContext) {
    switch(cx, 240, 66, false, "switch-240x66");
}

/// Zoom: the font size steps every frame between two sizes whose glyphs are
/// already in the atlas, so this is reshaping and relayout, not rasterization.
#[gpui::bench(fps = 120)]
fn interaction_zoom(cx: &mut BenchAppContext) {
    let view = open(cx, terminal(160, 50, false));
    cx.bench_renderer(view.clone(), |view, _window, cx| {
        view.font_size = if view.font_size == px(13.0) {
            px(14.0)
        } else {
            px(13.0)
        };
        cx.notify();
    });
    report(cx, &view, "zoom", false);
}

/// A selection drag down and up the screen, one row per frame.
#[gpui::bench(fps = 120)]
fn interaction_selection_drag(cx: &mut BenchAppContext) {
    let terminal = terminal(160, 50, false);
    terminal.begin_selection(10, 0);
    let view = open(cx, terminal);
    let mut step = 0usize;
    cx.bench_renderer(view.clone(), move |view, _window, cx| {
        step = (step + 1) % 98;
        let row = if step < 49 { step } else { 98 - step };
        view.terminal.drag_selection(40 + step % 60, row);
        cx.notify();
    });
    report(cx, &view, "selection-drag", true);
}

/// Output scrolling a full-screen grid (a 4K display at 2x: 1920x1080 points): every row moves up by one.
#[gpui::bench(fps = 120)]
fn interaction_scroll_240x66(cx: &mut BenchAppContext) {
    let frames = [full_frame(240, 66, 1, true), full_frame(240, 66, 0, true)].map(|mut frame| {
        frame.is_full_snapshot = false;
        frame
    });
    let view = open(cx, terminal(240, 66, true));
    let mut next = 0;
    cx.bench_renderer(view.clone(), move |view, _window, cx| {
        view.terminal.apply_damage(frames[next].clone());
        next ^= 1;
        cx.notify();
    });
    report(cx, &view, "scroll-240x66-cjk", true);
}

gpui::bench_group!(
    benches,
    interaction_seam_slide,
    interaction_resize_drag,
    interaction_reflow_arrival,
    interaction_switch_160x50,
    interaction_switch_160x50_cjk,
    interaction_switch_240x66,
    interaction_zoom,
    interaction_selection_drag,
    interaction_scroll_240x66
);
gpui::bench_main!(benches);
