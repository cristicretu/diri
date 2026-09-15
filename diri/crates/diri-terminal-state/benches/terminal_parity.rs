//! Reproducible resource samples for 80x24 terminal fleets.
//! Counts requested Rust heap, not RSS, allocator overhead, PTYs, or renderers.
//! Always report retained history separately from the amount of input fed.

#[path = "support/allocation.rs"]
mod allocation;

use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use allocation::{ALLOCS, LIVE, PEAK};
use diri_terminal_state::HeadlessScreen;

const COLS: usize = 80;
const ROWS: usize = 24;
const SAMPLES: usize = 7;

#[derive(Clone, Copy, Default)]
struct Sample {
    fresh_bytes: usize,
    fed_bytes: usize,
    published_bytes: usize,
    peak_bytes: usize,
    allocations: usize,
    history_rows: i64,
    construct_ns: u128,
    feed_ns: u128,
    publication_ns: u128,
}

fn sample(count: usize, lines: usize) -> Sample {
    let baseline = LIVE.load(Relaxed);
    let allocations = ALLOCS.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let started = Instant::now();
    let mut screens: Vec<_> = (0..count)
        .map(|_| HeadlessScreen::new(COLS, ROWS))
        .collect();
    let construct_ns = started.elapsed().as_nanos();
    let fresh_bytes = LIVE.load(Relaxed) - baseline;

    // Exactly 72 ASCII columns plus CRLF; the six-digit prefix lets the probe
    // check that the final output was parsed instead of measuring discarded work.
    let mut line = [b' '; 74];
    line[72..].copy_from_slice(b"\r\n");
    let prose = b"A terminal keeps the work alive while people read and organize it. ";
    for (index, byte) in line[7..72].iter_mut().enumerate() {
        *byte = prose[index % prose.len()];
    }
    let started = Instant::now();
    for index in 0..lines {
        let mut number = index;
        for digit in line[..6].iter_mut().rev() {
            *digit = b'0' + (number % 10) as u8;
            number /= 10;
        }
        for screen in &mut screens {
            screen.feed(black_box(&line));
        }
    }
    let feed_ns = started.elapsed().as_nanos();
    let fed_bytes = LIVE.load(Relaxed) - baseline;
    let started = Instant::now();
    for screen in &mut screens {
        let update = screen.grid_update(true);
        assert_eq!(usize::from(update.cols), COLS);
        assert_eq!(usize::from(update.rows), ROWS);
        if lines > 0 {
            let last = &update.changed_rows[(lines - 1).min(ROWS - 2)].cells;
            for (cell, byte) in last.iter().zip(&line[..72]) {
                assert_eq!(cell.scalar, u32::from(*byte));
            }
        }
        black_box(update);
    }
    let publication_ns = started.elapsed().as_nanos();
    let published_bytes = LIVE.load(Relaxed) - baseline;
    let peak_bytes = PEAK.load(Relaxed) - baseline;
    let allocations = ALLOCS.load(Relaxed) - allocations;

    // Inspect the actual retained range only after resource/timing samples.
    // A zero-row read reports history metadata without cloning the history.
    let history_rows = screens[0].scrollback_cells(0, 0).live_start_row;
    for screen in &screens {
        assert_eq!(screen.scrollback_cells(0, 0).live_start_row, history_rows);
    }
    drop(screens);
    assert_eq!(
        LIVE.load(Relaxed),
        baseline,
        "terminal core allocations leaked"
    );
    Sample {
        fresh_bytes,
        fed_bytes,
        published_bytes,
        peak_bytes,
        allocations,
        history_rows,
        construct_ns,
        feed_ns,
        publication_ns,
    }
}

fn percentile(mut values: [u128; SAMPLES], percent: usize) -> u128 {
    values.sort_unstable();
    values[(percent * SAMPLES).div_ceil(100).saturating_sub(1)]
}

fn main() {
    // Warm stdout and allocator/library one-time initialization before sampling.
    println!(
        "{{\"type\":\"metadata\",\"schema\":1,\"cols\":{COLS},\"rows\":{ROWS},\"samples\":{SAMPLES},\"metric\":\"requested_rust_heap\",\"profile\":\"{}\",\"arch\":\"{}\",\"os\":\"{}\",\"input_columns\":72,\"includes\":\"terminal cores and fleet container\",\"excludes\":\"RSS, allocator overhead, processes, PTYs, logs, renderer, GPU\"}}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "optimized"
        },
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    black_box(sample(1, 1));
    if std::env::args().any(|arg| arg == "--short-history-gate") {
        for lines in [23, 24, 25] {
            let measured = sample(1, lines);
            println!(
                "{lines}-line core: {} bytes after publication, {} history rows",
                measured.published_bytes, measured.history_rows
            );
            assert_eq!(measured.history_rows, (lines + 1 - ROWS) as i64);
            assert!(
                measured.published_bytes <= 192 * 1024,
                "short terminal history exceeds 192 KiB requested-heap budget"
            );
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--empty-gate") {
        let measured = sample(1, 0);
        println!("empty core requested heap: {} bytes", measured.fresh_bytes);
        assert!(
            measured.fresh_bytes <= 68 * 1024,
            "empty core exceeds 68 KiB requested-heap budget"
        );
        return;
    }
    for count in [1, 10, 100] {
        for lines in [0, 10_000] {
            let mut samples = [Sample::default(); SAMPLES];
            for item in &mut samples {
                *item = sample(count, lines);
            }
            for (index, item) in samples.iter().enumerate() {
                println!(
                    "{{\"type\":\"sample\",\"cores\":{count},\"input_lines\":{lines},\"sample\":{index},\"fresh_bytes\":{},\"fed_bytes\":{},\"published_bytes\":{},\"peak_bytes\":{},\"allocations\":{},\"history_rows_per_core\":{},\"visible_rows_per_core\":{ROWS},\"construct_ns\":{},\"feed_ns\":{},\"publication_ns\":{}}}",
                    item.fresh_bytes,
                    item.fed_bytes,
                    item.published_bytes,
                    item.peak_bytes,
                    item.allocations,
                    item.history_rows,
                    item.construct_ns,
                    item.feed_ns,
                    item.publication_ns
                );
            }
            println!(
                "{{\"type\":\"summary\",\"cores\":{count},\"input_lines\":{lines},\"fresh_bytes_p50\":{},\"fed_bytes_p50\":{},\"published_bytes_p50\":{},\"peak_bytes_p50\":{},\"construct_ns_p50\":{},\"construct_ns_p90\":{},\"feed_ns_p50\":{},\"feed_ns_p90\":{},\"publication_ns_p50\":{},\"publication_ns_p90\":{}}}",
                percentile(samples.map(|s| s.fresh_bytes as u128), 50),
                percentile(samples.map(|s| s.fed_bytes as u128), 50),
                percentile(samples.map(|s| s.published_bytes as u128), 50),
                percentile(samples.map(|s| s.peak_bytes as u128), 50),
                percentile(samples.map(|s| s.construct_ns), 50),
                percentile(samples.map(|s| s.construct_ns), 90),
                percentile(samples.map(|s| s.feed_ns), 50),
                percentile(samples.map(|s| s.feed_ns), 90),
                percentile(samples.map(|s| s.publication_ns), 50),
                percentile(samples.map(|s| s.publication_ns), 90)
            );
        }
    }
}
