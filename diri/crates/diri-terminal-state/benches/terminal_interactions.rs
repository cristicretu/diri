//! Engine-side cost of the terminal interactions a user feels, other than
//! typing: dragging a window or split (resize reflow + republication),
//! scrolling through history (`scrollback_cells` windows), switching to a
//! session (attach seed) and Find capture, all over 10,000 rows of a coloured
//! build log.
//!
//! Reports per-operation distributions (p50/p95/max) and requested heap
//! allocations. `--gate` enforces the 120 Hz frame budget on the p95 of a
//! scroll page and of an attach seed. A drag step is reported, not gated:
//! reflowing 10,000 wrapped history rows costs several frames today (see
//! PERF.md, "Terminal interactions"). This is parser/grid cost only: no IPC,
//! PTY, renderer or display presentation.
use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use diri_proto::grid::GridRowCodec;
use diri_terminal_state::HeadlessScreen;

#[path = "support/allocation.rs"]
mod allocation;
use allocation::ALLOCS;

const HISTORY_LINES: usize = 10_000;

struct Stats {
    samples: Vec<Duration>,
    allocations: u64,
}

impl Stats {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
            allocations: 0,
        }
    }

    fn time<T>(&mut self, op: impl FnOnce() -> T) -> T {
        let allocations = ALLOCS.load(Relaxed);
        let start = Instant::now();
        let value = black_box(op());
        self.samples.push(start.elapsed());
        self.allocations += ALLOCS.load(Relaxed).saturating_sub(allocations) as u64;
        value
    }

    fn at(&self, q: f64) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort();
        sorted[((sorted.len() as f64 - 1.0) * q).round() as usize]
    }

    fn report(&self, label: &str) -> Duration {
        let p95 = self.at(0.95);
        println!(
            "{label:<44} p50 {:>9.1?}  p95 {:>9.1?}  max {:>9.1?}  allocs/op {:>7}  n={}",
            self.at(0.5),
            p95,
            self.at(1.0),
            self.allocations / self.samples.len().max(1) as u64,
            self.samples.len(),
        );
        p95
    }
}

/// A coloured build log with a CR before each LF, like the recorded payload,
/// and every fourth line long enough to wrap below ~120 columns.
fn build_log(lines: usize, wide: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * 110);
    for line in 0..lines {
        let color = 31 + line % 6;
        let module = "x".repeat(10 + line % 50);
        let tail = if line.is_multiple_of(4) {
            " with an extra long explanation that wraps on narrow panes and splits"
        } else {
            ""
        };
        let text = if wide && line.is_multiple_of(3) {
            format!(
                "\x1b[{color}m[{line:010}] 構築中 crate_{} 🦀\x1b[0m  モジュール {module}{tail}\r\n",
                line % 997
            )
        } else {
            format!(
                "\x1b[{color}m[{line:010}] building crate_{} v0.{}.0\x1b[0m  Compiling module {module}{tail}\r\n",
                line % 997,
                line % 9
            )
        };
        out.extend_from_slice(text.as_bytes());
    }
    out
}

fn fed(cols: usize, rows: usize, wide: bool) -> HeadlessScreen {
    let mut screen = HeadlessScreen::new(cols, rows);
    screen.feed(&build_log(HISTORY_LINES, wide));
    screen.grid_update(true);
    screen
}

fn scenario(cols: usize, rows: usize, wide: bool, gate: bool) -> bool {
    println!(
        "\n== {cols}x{rows}, {HISTORY_LINES} lines of {} build log ==",
        if wide { "CJK/emoji" } else { "ASCII" }
    );
    let mut ok = true;
    let budget = Duration::from_micros(8_333);

    // Live drag: one column per step, 40 columns in and back out, the way
    // the desktop paces a drag (one resize per 8 ms). Each step is what the
    // Engine does per resize: reflow, then publish the (full) grid.
    let mut screen = fed(cols, rows, wide);
    let mut drag = Stats::new();
    for _ in 0..3 {
        for step in (0..40).chain((0..40).rev()) {
            drag.time(|| {
                screen.resize(cols - 40 + step, rows);
                screen.grid_update(false)
            });
        }
    }
    drag.report("drag step (resize cols + grid_update)");
    let mut rows_drag = Stats::new();
    for step in (0..20).chain((0..20).rev()) {
        rows_drag.time(|| {
            screen.resize(cols, rows - 20 + step);
            screen.grid_update(false)
        });
    }
    rows_drag.report("drag step (resize rows + grid_update)");

    // Scrolling: 50-row pages moving 3 rows per event through history
    // (a wheel), then long jumps (scroll-to-top, jump-to-prompt).
    let mut screen = fed(cols, rows, wide);
    let live = screen.scrollback_cells(0, 0).live_start_row;
    let mut wheel = Stats::new();
    let mut first = live - rows as i64;
    while first > live - 3_000 {
        wheel.time(|| screen.scrollback_cells(first, rows as i64));
        first -= 3;
    }
    let p95 = wheel.report("wheel page (scrollback_cells 3-row steps)");
    ok &= p95 <= budget;
    let mut same = Stats::new();
    for _ in 0..200 {
        same.time(|| screen.scrollback_cells(live - 2_000, rows as i64));
    }
    same.report("same page re-read");
    let mut jumps = Stats::new();
    for jump in 0..60 {
        let target = if jump % 2 == 0 { 0 } else { live - rows as i64 };
        jumps.time(|| screen.scrollback_cells(target, rows as i64 * 2));
    }
    jumps.report("jump top<->bottom (2 pages)");
    let mut decode = Stats::new();
    let page = screen.scrollback_cells(live - 500, rows as i64);
    for _ in 0..200 {
        decode.time(|| GridRowCodec::decode_rows(&page.payload, page.row_count as usize).unwrap());
    }
    decode.report("client decode of one page");

    // Switching: the attach seed (a full snapshot) and its wire encoding.
    let mut seed = Stats::new();
    let mut encoded_len = 0;
    for _ in 0..100 {
        seed.time(|| {
            let snapshot = screen.full_snapshot();
            let rows: Vec<_> = snapshot
                .changed_rows
                .iter()
                .map(|row| row.cells.clone())
                .collect();
            encoded_len = GridRowCodec::encode_rows(&rows).unwrap().len();
        });
    }
    let p95 = seed.report("attach seed (full_snapshot + encode)");
    ok &= p95 <= budget;
    println!("  seed payload {encoded_len} bytes");

    // Find: the retained capture a ⌘F opens over.
    let mut find = Stats::new();
    for _ in 0..8 {
        find.time(|| screen.find_capture_cells().unwrap());
    }
    find.report("find capture (all retained rows)");
    if gate && !ok {
        println!("  GATE FAILED: a wheel page or attach seed exceeded {budget:?} at p95");
    }
    ok
}

fn main() {
    let gate = std::env::args().any(|arg| arg == "--gate");
    let mut ok = true;
    ok &= scenario(160, 50, false, gate);
    ok &= scenario(160, 50, true, gate);
    ok &= scenario(380, 110, false, gate);
    if gate {
        assert!(ok, "interaction gate failed");
    }
}
