//! Retained heap while a terminal agent continuously redraws its composer.
//! One test per binary because the allocator measures all Rust allocations.

#[path = "../../diri-terminal-state/benches/support/allocation.rs"]
mod allocation;

use std::sync::atomic::Ordering::Relaxed;

use allocation::LIVE;
use diri_term::buffer::GridBuffer;
use diri_terminal_state::HeadlessScreen;

#[test]
fn repeated_composer_repaints_release_superseded_text_and_frames() {
    let mut screen = HeadlessScreen::new_with_keyboard_enhancements(120, 40)
        .with_notifications()
        .with_inline_images();
    screen.feed(b"primary transcript\x1b[?1049h");
    let mut grid = GridBuffer::default();
    let mut generations = Vec::new();
    let mut repaint = |screen: &mut HeadlessScreen, tick| {
        // Unique text and OSC 8 targets on a fixed set of rows. A cached
        // copy per generation leaks even though history never advances.
        let output = format!(
            "\x1b[H\x1b[2K\x1b]8;;https://example.invalid/run/{tick:08}\x1b\\working {tick:08}\x1b]8;;\x1b\\\x1b[2;1H> reply"
        );
        screen.feed(output.as_bytes());
        let frame = screen.grid_update(false);
        assert!(grid.apply(frame).changed);
        let damage = grid.snapshot_damage(&mut generations, 40, 120, false);
        assert!(!damage.changed_rows.is_empty());
    };

    for tick in 0..256 {
        repaint(&mut screen, tick);
    }
    let warm = LIVE.load(Relaxed);
    // 30 minutes at four redraws per second, advanced without a timer.
    // One retained previous row or URL per repaint would exceed this bound.
    for tick in 256..7456 {
        repaint(&mut screen, tick);
    }
    let retained = LIVE.load(Relaxed).saturating_sub(warm);
    assert!(
        retained <= 64 * 1024,
        "retained an additional {retained} bytes"
    );
    assert!(screen.row_text(0).contains("working 00007455"));
    screen.feed(b"\x1b[?1049l");
    assert!(screen.row_text(0).contains("primary transcript"));
}
