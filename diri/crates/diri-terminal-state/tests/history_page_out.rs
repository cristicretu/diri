//! Idle history paged out to a cold store reads, resizes, evicts and
//! checkpoints exactly like history kept in memory.
#![cfg(feature = "compact-history")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use diri_terminal_state::{ColdStore, HeadlessScreen};

#[derive(Default)]
struct MemoryStore {
    slots: Mutex<HashMap<u64, Vec<u8>>>,
    next: AtomicU64,
    loads: AtomicUsize,
}

impl ColdStore for MemoryStore {
    fn store(&self, payloads: &[&[u8]]) -> std::io::Result<Vec<u64>> {
        let mut slots = self.slots.lock().unwrap();
        Ok(payloads
            .iter()
            .map(|payload| {
                let slot = self.next.fetch_add(1, Ordering::Relaxed);
                slots.insert(slot, payload.to_vec());
                slot
            })
            .collect())
    }

    fn load(&self, slot: u64, _len: usize) -> std::io::Result<Vec<u8>> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(self.slots.lock().unwrap()[&slot].clone())
    }

    fn release(&self, slot: u64, _len: usize) {
        assert!(self.slots.lock().unwrap().remove(&slot).is_some());
    }
}

/// Links carry explicit ids: an id-less link gets a process-wide counter's
/// next value, which would differ between two screens fed in turn.
fn styled(burst: usize, lines: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in 0..lines {
        bytes.extend_from_slice(
            format!(
                "\x1b[2m{burst}:{line:06}\x1b[0m \x1b[38;5;{}mmodule::{}\x1b[0m wrapped text {} \x1b]8;id={line};https://example.invalid/{line}\x07link\x1b]8;;\x07 界\r\n",
                16 + line % 200,
                line % 97,
                "x".repeat(line % 150),
            )
            .as_bytes(),
        );
    }
    bytes
}

fn assert_same(paged: &mut HeadlessScreen, resident: &mut HeadlessScreen, step: &str) {
    assert_eq!(paged.history_rows(), resident.history_rows(), "{step}");
    assert_eq!(
        paged.history_storage_bytes(),
        resident.history_storage_bytes(),
        "{step}: the budget counts paged history the same"
    );
    assert_eq!(paged.full_snapshot(), resident.full_snapshot(), "{step}");
    assert_eq!(paged.scrollback(), resident.scrollback(), "{step}");
    assert_eq!(
        paged.find_capture_cells(),
        resident.find_capture_cells(),
        "{step}"
    );
    // The oldest rows, which are paged out, page by page as a scroll reads.
    assert_eq!(
        paged.scrollback_cells(0, 600),
        resident.scrollback_cells(0, 600),
        "{step}"
    );
    assert_eq!(
        paged.history_payload(),
        resident.history_payload(),
        "{step}"
    );
    assert_eq!(
        paged.history_metadata(),
        resident.history_metadata(),
        "{step}"
    );
}

#[test]
#[cfg_attr(
    feature = "core-v2-screen",
    ignore = "core v2 keeps history in memory: cold paging, and resize, are later steps"
)]
fn paged_out_history_matches_resident_history_through_output_resizes_and_reads() {
    let store = Arc::new(MemoryStore::default());
    let cold: Arc<dyn ColdStore> = store.clone();
    let mut paged = HeadlessScreen::new(120, 40);
    let mut resident = HeadlessScreen::new(120, 40);
    let sizes = [(120, 40), (80, 30), (200, 50), (60, 20), (120, 40)];
    let mut moved = 0;
    for (burst, &(cols, rows)) in sizes.iter().enumerate() {
        let bytes = styled(burst, 4_000);
        paged.feed(&bytes);
        resident.feed(&bytes);
        // The Engine releases idle caches before every page-out, which
        // re-encodes edited history; the resident screen does the same.
        let report = paged.page_out_history(&cold).expect("page out");
        resident.compact_idle();
        moved += report.payloads;
        assert!(paged.history_residency().stored > 0, "burst {burst}");
        assert_same(&mut paged, &mut resident, &format!("burst {burst}"));
        // A resize reflows paged history (deferred, then read).
        paged.resize(cols, rows);
        resident.resize(cols, rows);
        assert_same(&mut paged, &mut resident, &format!("resize {burst}"));
        // Paging out deferred reflow sources too.
        paged.page_out_history(&cold).expect("page out");
        resident.compact_idle();
        assert_same(&mut paged, &mut resident, &format!("deferred {burst}"));
    }
    assert!(moved > 0);
    assert!(store.loads.load(Ordering::Relaxed) > 0);

    // A checkpoint restored from paged history equals one from memory.
    let (count, payload) = paged.history_payload();
    let (resident_count, resident_payload) = resident.history_payload();
    assert_eq!((count, &payload), (resident_count, &resident_payload));

    drop(paged);
    assert!(
        store.slots.lock().unwrap().is_empty(),
        "every stored payload is released with its screen"
    );
}

/// Core v2 (no resize until reflow lands): paged-out pages read, checkpoint,
/// LZ-compress under budget pressure and evict exactly like resident ones.
#[test]
#[cfg(feature = "core-v2-screen")]
fn paged_out_v2_history_matches_resident_history_through_output_eviction_and_reads() {
    let store = Arc::new(MemoryStore::default());
    let cold: Arc<dyn ColdStore> = store.clone();
    let mut paged = HeadlessScreen::new(120, 40);
    let mut resident = HeadlessScreen::new(120, 40);
    let mut moved = 0;
    // Enough output to pass the 4 MiB budget: older pages are compressed
    // and evicted while some of them are in the store.
    for burst in 0..8 {
        let bytes = styled(burst, 12_000);
        paged.feed(&bytes);
        resident.feed(&bytes);
        let report = paged.page_out_history(&cold).expect("page out");
        resident.compact_idle();
        moved += report.payloads;
        assert!(paged.history_residency().stored > 0, "burst {burst}");
        assert_same(&mut paged, &mut resident, &format!("burst {burst}"));
    }
    assert!(moved > 0);
    assert!(store.loads.load(Ordering::Relaxed) > 0);
    assert!(
        paged.history_rows() < 8 * 12_000,
        "the budget evicted the oldest rows"
    );
    let (count, payload) = paged.history_payload();
    let (resident_count, resident_payload) = resident.history_payload();
    assert_eq!((count, &payload), (resident_count, &resident_payload));
    drop(paged);
    assert!(
        store.slots.lock().unwrap().is_empty(),
        "every stored page is released with its screen"
    );
}

#[test]
fn history_reads_are_counted_and_short_history_stays_in_memory() {
    let store: Arc<dyn ColdStore> = Arc::new(MemoryStore::default());
    let mut screen = HeadlessScreen::new(120, 40);
    screen.feed(&styled(0, 300));
    let reads = screen.history_reads();
    let _ = screen.scrollback();
    let _ = screen.find_capture_cells();
    assert_eq!(screen.history_reads(), reads + 2);
    // A few hundred lines are under the minimum: nothing is written.
    let report = screen.page_out_history(&store).unwrap();
    assert_eq!(report.payloads, 0);
    assert_eq!(screen.history_residency().stored, 0);
}
