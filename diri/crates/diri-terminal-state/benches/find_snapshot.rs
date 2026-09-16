//! Capture cost at the parser's full 4 MiB history-cell budget.
//! Requested Rust allocation bytes; excludes IPC, renderer, and process RSS.
use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use diri_proto::grid::GridRowCodec;
use diri_terminal_state::HeadlessScreen;

#[path = "support/allocation.rs"]
mod allocation;
use allocation::{ALLOCS, LIVE, PEAK};

fn measure<T>(name: &str, capture: impl FnOnce() -> T) -> T {
    let baseline = LIVE.load(Relaxed);
    let allocations = ALLOCS.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let start = Instant::now();
    let value = black_box(capture());
    let elapsed = start.elapsed();
    println!(
        "{name}: elapsed_us={} retained_bytes={} peak_bytes={} allocations={}",
        elapsed.as_micros(),
        LIVE.load(Relaxed).saturating_sub(baseline),
        PEAK.load(Relaxed).saturating_sub(baseline),
        ALLOCS.load(Relaxed).saturating_sub(allocations),
    );
    value
}

fn main() {
    println!("Find capture: full 4 MiB history-cell budget; requested heap, no IPC/RSS");
    for cols in [40, 120, 320] {
        for styled in [false, true] {
            let mut screen = HeadlessScreen::new(cols, 40);
            let line = if styled {
                "\x1b[38;2;28;180;240mneedle 界 e\u{301} \x1b[0m ordinary output\r\n"
            } else {
                "needle ordinary output for capture cost\r\n"
            };
            screen.feed(line.repeat(12_000).as_bytes());
            let retained = screen.scrollback().visible_start_row;
            println!("cols={cols} styled={styled} history_rows={retained}");
            let text = measure("text", || screen.scrollback());
            drop(text);
            let cells = measure("styled-capture", || screen.scrollback_cells(0, i64::MAX));
            let decoded = measure("styled-decode", || {
                GridRowCodec::decode_rows(&cells.payload, cells.row_count as usize).unwrap()
            });
            assert_eq!(decoded.len() as i64, cells.row_count);
            black_box((&decoded, &cells));
        }
    }
}
