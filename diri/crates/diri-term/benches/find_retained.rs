//! Optimized retained-search cost at a full 4 MiB parser history budget.
//! Run with `cargo bench -p diri-term --bench find_retained`.
use diri_proto::{CaptureFindResult, FIND_CAPTURE_MAX_CELLS, FIND_CAPTURE_MAX_ROWS, SessionId};
use diri_term::buffer::GridBuffer;
use diri_term::find::{
    FindCapturePermit, FindSnapshot, RetainedFindSnapshot, SEARCH_DEBOUNCE, TerminalFindModel,
};
use diri_term::scrollback::ScrollbackViewport;
use diri_terminal_state::HeadlessScreen;
use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

#[path = "../../diri-terminal-state/benches/support/allocation.rs"]
mod allocation;
use allocation::{ALLOCS, LIVE, PEAK};

fn main() {
    println!(
        "Retained Find: requested Rust heap; excludes IPC/RSS/GPU. Full 4 MiB parser history before bounded capture."
    );
    for cols in [40, 120, 320] {
        for styled in [false, true] {
            let mut screen = HeadlessScreen::new(cols, 40);
            let line = if styled {
                "\x1b[38;2;28;180;240mneedle 界 e\u{301} \x1b[0m ordinary output\r\n"
            } else {
                "needle ordinary output for capture cost\r\n"
            };
            screen.feed(line.repeat(12000).as_bytes());
            let geometry = screen.scrollback_cells(0, 0);
            let count = (FIND_CAPTURE_MAX_CELLS / cols)
                .min(FIND_CAPTURE_MAX_ROWS)
                .min(geometry.total_rows as usize);
            let first = geometry.total_rows - count as i64;
            let mut live = GridBuffer::default();
            live.apply(screen.full_snapshot());
            let baseline = LIVE.load(Relaxed);
            let allocations = ALLOCS.load(Relaxed);
            PEAK.store(baseline, Relaxed);
            let start = Instant::now();
            let permit = FindCapturePermit::acquire().unwrap();
            let mut model = TerminalFindModel::retained();
            let reservation = model.reservation().unwrap();
            let source = RetainedFindSnapshot::decode(
                CaptureFindResult {
                    owner: "benchmark-session-owner".into(),
                    capture_revision: 1,
                    session_id: SessionId::new("benchmark"),
                    is_alt_screen: false,
                    visible_rows: 40,
                    partial: first > 0,
                    cells: screen.find_capture_cells().unwrap(),
                },
                reservation,
            )
            .unwrap();
            let captured_us = start.elapsed().as_micros();
            drop(permit);
            let retained = source.retained_bytes();
            let count = source.row_count();
            model.set_query("needle", Duration::ZERO);
            let request = model.take_due_search(SEARCH_DEBOUNCE).unwrap();
            let start = Instant::now();
            let result = model
                .prepare_search(&request, FindSnapshot::from(source), &live)
                .unwrap()
                .run();
            let mut viewport = ScrollbackViewport::default();
            assert!(model.apply_result(result, &mut viewport));
            let scan_us = start.elapsed().as_micros();
            let peak = PEAK.load(Relaxed) - baseline;
            let actual = LIVE.load(Relaxed) - baseline;
            let allocations = ALLOCS.load(Relaxed) - allocations;
            let start = Instant::now();
            for _ in 0..1000 {
                black_box(model.visible_spans_with_live(&viewport, &live));
            }
            let proof_ns = start.elapsed().as_nanos() / 1000;
            println!(
                "cols={cols} styled={styled} history={} captured={count} partial={} capture_decode_us={captured_us} scan_us={scan_us} retained_accounted={retained} actual_find_heap={actual} peak_heap={peak} allocations={allocations} highlight_proof_ns={proof_ns}",
                geometry.live_start_row,
                first > 0
            );
            drop((model, viewport, request));
            assert_eq!(
                LIVE.load(Relaxed),
                baseline,
                "Find releases all retained rows"
            );
        }
    }
}
