//! The checkpoint's encoded history is one allocation, never regrown.
//!
//! The Engine encodes up to 10k history rows for every screen checkpoint, a
//! second after output settles: megabytes per session. Grown by doubling, each
//! intermediate buffer was freed into the macOS allocator's large-allocation
//! cache and stayed resident, so an idle Engine with ten sessions of styled
//! history held ~90 MB it no longer used (measured with `idlebench`).
//!
//! Its own binary, and one test: the counting allocator is process-wide.

#[path = "../benches/support/allocation.rs"]
mod allocation;

use std::sync::atomic::Ordering::Relaxed;

use allocation::{LARGE, LARGE_ALLOCS};
use diri_terminal_state::HeadlessScreen;

#[test]
fn the_checkpoint_history_payload_is_reserved_once() {
    let mut screen = HeadlessScreen::new(120, 40);
    let mut bytes = Vec::new();
    for line in 1..=20_000 {
        bytes.extend_from_slice(
            format!(
                "\x1b[3{}m{line:>6}\x1b[0m lorem ipsum dolor sit amet {line}\r\n",
                line % 8
            )
            .as_bytes(),
        );
    }
    screen.feed(&bytes);
    drop(bytes);
    assert!(screen.history_rows() >= 9_000, "history is full");

    let before = LARGE_ALLOCS.load(Relaxed);
    let (rows, payload) = screen.history_payload();
    let large = LARGE_ALLOCS.load(Relaxed) - before;
    assert!(
        rows >= 9_000 && payload.len() > 4 * LARGE,
        "{} bytes",
        payload.len()
    );
    assert_eq!(
        large,
        1,
        "a {} byte payload took {large} large allocations",
        payload.len()
    );
}
