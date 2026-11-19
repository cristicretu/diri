//! As scrollback grows, successive durable checkpoints should reuse large
//! allocation sizes. macOS retains freed large blocks in its allocator.

use diri_terminal_state::HeadlessScreen;
use std::collections::BTreeSet;

#[test]
fn growing_history_uses_a_bounded_set_of_large_payload_sizes() {
    let mut screen = HeadlessScreen::new(120, 40);
    let mut sizes = BTreeSet::new();
    let mut history = 0;
    for batch in 0..80 {
        let output: String = (0..100)
            .map(|row| format!("completed step {}\r\n", batch * 100 + row))
            .collect();
        screen.feed(output.as_bytes());
        let (rows, payload) = screen.history_payload();
        assert!(rows > history);
        history = rows;
        if payload.capacity() >= 1024 * 1024 {
            sizes.insert(payload.capacity());
        }
    }
    assert!(
        sizes.len() <= 6,
        "{} distinct large reservations across 80 growing checkpoints",
        sizes.len()
    );
}
