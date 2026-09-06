//! Deterministic allocation and resize probe for twenty resident terminal cores.
//! Counts requested Rust heap bytes, not RSS, GPU memory, or agent processes.
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use diri_terminal_state::HeadlessScreen;

struct CountingAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every operation delegates to System with the original layout. The
// counters allocate nothing and never affect the returned pointer or layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Relaxed);
            ALLOCS.fetch_add(1, Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            LIVE.fetch_add(size, Relaxed);
            LIVE.fetch_sub(layout.size(), Relaxed);
            ALLOCS.fetch_add(1, Relaxed);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

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
    println!("resize churn: {} us/op", start.elapsed().as_micros() / 600);
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
