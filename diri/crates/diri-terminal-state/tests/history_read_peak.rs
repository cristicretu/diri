//! Reading every history row after a width change must not first rebuild all
//! of it at the new width. The Engine reads full history for each screen
//! checkpoint (about a second after output settles) and for link scans, so
//! with that rebuild every resize of a full-history session cost ~60 MB of
//! transient heap, per session, and the allocator kept it.
//!
//! Its own binary, and one test: the counting allocator is process-wide.

#[path = "../benches/support/allocation.rs"]
mod allocation;

use std::sync::atomic::Ordering::Relaxed;

use allocation::{LIVE, PEAK};
use diri_terminal_state::HeadlessScreen;

/// Generous next to the ~1 MB a block-at-a-time read takes, far below the
/// 30–60 MB of a whole-history rebuild at these widths.
const MAX_READ_PEAK: usize = 8 << 20;

fn peak_of(work: impl FnOnce()) -> usize {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    work();
    PEAK.load(Relaxed).saturating_sub(base)
}

fn full_history_screen() -> HeadlessScreen {
    let mut screen = HeadlessScreen::new(120, 40);
    let mut bytes = Vec::new();
    for line in 1..=20_000 {
        bytes.extend_from_slice(format!("{line} \x1b[32mok\x1b[0m\r\n").as_bytes());
    }
    screen.feed(&bytes);
    assert!(screen.history_rows() >= 9_000, "history is full");
    screen
}

#[test]
fn full_history_reads_after_resizes_stay_small() {
    let mut screen = full_history_screen();
    for (step, cols) in [97usize, 171, 128, 80, 145].into_iter().enumerate() {
        screen.resize(cols, 30 + step);
        // The payload is reserved at its bound up front (see
        // `history_payload_growth.rs`); only what the read needs beyond its
        // result counts.
        let mut reserved = 0;
        let encoded = peak_of(|| {
            let (rows, payload) = screen.history_payload();
            assert!(rows > 0 && !payload.is_empty());
            reserved = payload.capacity();
        });
        let encoded = encoded.saturating_sub(reserved);
        assert!(
            encoded < MAX_READ_PEAK,
            "checkpoint read at {cols} columns peaked at {encoded} bytes"
        );
        screen.resize(cols + 3, 30 + step);
        let rows = screen.history_rows();
        let metadata = peak_of(|| {
            let _ = screen.history_metadata();
        });
        // Its result is one record per row, whatever the read costs.
        let result = rows * std::mem::size_of::<diri_proto::grid::RowMetadata>();
        assert!(
            metadata < MAX_READ_PEAK + result,
            "metadata read at {} columns peaked at {metadata} bytes",
            cols + 3
        );
        screen.resize(cols + 1, 30 + step);
        let links = peak_of(|| {
            let _ = screen.link_source(400);
        });
        assert!(
            links < MAX_READ_PEAK,
            "link scan at {} columns peaked at {links} bytes",
            cols + 1
        );
    }

    // The encoded read is the cell snapshot, encoded.
    screen.resize(97, 30);
    let (rows, payload) = screen.history_payload();
    let snapshot = screen.history_snapshot();
    assert_eq!(rows, snapshot.len());
    assert_eq!(
        payload,
        diri_proto::grid::GridRowCodec::encode_rows(&snapshot).expect("encode")
    );
}
