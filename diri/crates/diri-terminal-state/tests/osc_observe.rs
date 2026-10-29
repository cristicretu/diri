//! The parser's test-only OSC totals that `vt_workloads` checks against its
//! golden reference: every dispatched OSC body is counted, and summed when
//! asked, whatever the read boundaries.
#![cfg(feature = "bench-verify")]

use diri_terminal_state::{HeadlessScreen, osc_observe};

const STREAM: &[u8] = b"\x1b]6;abc\x07x\x1b]0;t;u\x1b\\y\x1b]6;\x07";

fn totals(chunks: &[&[u8]], checksum: bool) -> osc_observe::OscTotals {
    let mut screen = HeadlessScreen::new(40, 10);
    osc_observe::reset(checksum);
    for chunk in chunks {
        screen.feed(chunk);
    }
    osc_observe::totals()
}

#[test]
fn every_osc_body_is_counted_and_summed() {
    let whole = totals(&[STREAM], true);
    assert_eq!(whole.dispatches, 3);
    assert_eq!(whole.params, 2 + 3 + 2);
    assert_eq!(whole.param_bytes, (1 + 3) + (1 + 1 + 1) + 1);
    assert_eq!(whole.bell_terminated, 2);
    assert_ne!(whole.checksum, 0);

    let bytes: Vec<&[u8]> = STREAM.chunks(1).collect();
    assert_eq!(totals(&bytes, true), whole);
    for split in 1..STREAM.len() {
        assert_eq!(
            totals(&[&STREAM[..split], &STREAM[split..]], true),
            whole,
            "split at {split}"
        );
    }

    // One body byte changed: same counts, different sum.
    let changed = totals(&[&STREAM.to_vec().replace_byte(5, b'X')], true);
    assert_eq!(changed.param_bytes, whole.param_bytes);
    assert_ne!(changed.checksum, whole.checksum);

    // A body moved across a parameter boundary changes the sum too.
    let moved = totals(&[b"\x1b]6;ab;c\x07x\x1b]0;t;u\x1b\\y\x1b]6;\x07"], true);
    assert_ne!(moved.checksum, whole.checksum);

    // Without the checksum the counts are still kept.
    let counted = totals(&[STREAM], false);
    assert_eq!(counted.checksum, 0);
    assert_eq!(counted.param_bytes, whole.param_bytes);
}

trait ReplaceByte {
    fn replace_byte(self, at: usize, byte: u8) -> Vec<u8>;
}

impl ReplaceByte for Vec<u8> {
    fn replace_byte(mut self, at: usize, byte: u8) -> Vec<u8> {
        self[at] = byte;
        self
    }
}
