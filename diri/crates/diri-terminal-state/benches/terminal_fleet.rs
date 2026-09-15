//! Deterministic allocation and resize probe for twenty resident terminal cores.
//! Counts requested Rust heap bytes, not RSS, GPU memory, or agent processes.
use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use diri_terminal_state::HeadlessScreen;

#[path = "support/allocation.rs"]
mod allocation;
use allocation::{ALLOCS, LIVE};

fn heap(label: &str, baseline: usize) -> usize {
    let bytes = LIVE.load(Relaxed).saturating_sub(baseline);
    println!(
        "{label}: {bytes} live heap bytes ({:.2} MiB)",
        bytes as f64 / 1048576.0
    );
    bytes
}

fn main() {
    // Initialize stdout before taking the allocation baseline.
    println!("terminal-fleet: 20 cores; requested heap, excluding processes/renderer");
    let baseline = LIVE.load(Relaxed);
    let mut screens: Vec<_> = (0..20).map(|_| HeadlessScreen::new(80, 50)).collect();
    assert!(
        heap("fresh 80x50", baseline) < 8 << 20,
        "idle terminal buffers grew"
    );
    let log = "build output with a short hard line\r\n".repeat(6000);
    for screen in &mut screens {
        screen.feed(log.as_bytes());
        black_box(screen.grid_update(true));
    }
    drop(log);
    heap("full history 80x50", baseline);
    let start = Instant::now();
    for screen in &mut screens {
        screen.resize(320, 50);
        black_box(screen.grid_update(false));
    }
    println!("20 resizes 80->320: {:?}", start.elapsed());
    assert!(
        heap("full history 320x50", baseline) < 140 << 20,
        "resizing escaped the fleet heap budget"
    );

    let start = Instant::now();
    for _ in 0..100 {
        for screen in &mut screens {
            screen.resize(320, 50);
        }
    }
    println!(
        "same-size resize: {} ns/op",
        start.elapsed().as_nanos() / 2000
    );

    // Keep the cursor on a row without scrolling, alternate actual text, and
    // include cursor-only damage. Count allocations after all warmup/setup.
    for screen in &mut screens {
        screen.feed(b"\x1b[H");
        black_box(screen.grid_update(false));
    }
    let before = ALLOCS.load(Relaxed);
    let start = Instant::now();
    for _ in 0..100 {
        for screen in &mut screens {
            screen.feed(b"\x1b[C\x1b[D");
            let update = screen.grid_update(false);
            assert!(update.changed_rows.is_empty());
            black_box(update);
        }
    }
    let allocations = ALLOCS.load(Relaxed) - before;
    println!(
        "cursor-only fleet: {} ns/op, {allocations} allocations / 2000 updates",
        start.elapsed().as_nanos() / 2000
    );
    assert_eq!(
        allocations, 0,
        "cursor-only updates must not allocate after warmup"
    );

    let start = Instant::now();
    for step in 0..30 {
        for screen in &mut screens {
            screen.resize(120 + step % 20, 40 + step % 10);
            black_box(screen.grid_update(false));
        }
    }
    let resize_us = start.elapsed().as_micros() / 600;
    println!("resize churn: {resize_us} us/op");
    if std::env::args().any(|arg| arg == "--resize-gate") {
        assert!(
            resize_us <= 1000,
            "unaffected hard-line history makes interactive resize too expensive"
        );
    }
    assert!(
        heap("after resize churn", baseline) < 140 << 20,
        "resize churn retained excessive heap"
    );
    drop(screens);
    assert_eq!(
        heap("after drop", baseline),
        0,
        "terminal cores leaked allocations"
    );
}
