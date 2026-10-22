//! Exactly-once delivery for phone keystroke batches.
//!
//! A phone on a cell link often loses the reply to a request that did reach
//! the Mac. Keystrokes are not idempotent, so the phone may only resend a
//! batch if the gateway can recognise it: each batch carries an id, and an id
//! is written to the PTY at most once. A resend of a delivered batch answers
//! success without writing; a resend that arrives while the original is still
//! being written waits for it; a resend of a batch that failed writes it.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

/// Ids remembered. A phone resends within seconds, one batch in flight per
/// terminal, so this covers far more than any retry window.
const REMEMBERED: usize = 512;

#[derive(Default)]
pub struct InputLedger {
    entries: Mutex<Entries>,
}

#[derive(Default)]
struct Entries {
    cells: HashMap<String, Arc<OnceCell<()>>>,
    order: VecDeque<String>,
}

impl InputLedger {
    /// Runs `write` unless a batch with this id was already delivered.
    pub async fn deliver<F, Fut, E>(&self, id: &str, write: F) -> Result<(), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let cell = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            match entries.cells.get(id) {
                Some(cell) => Arc::clone(cell),
                None => {
                    let cell = Arc::new(OnceCell::new());
                    entries.cells.insert(id.to_owned(), Arc::clone(&cell));
                    entries.order.push_back(id.to_owned());
                    while entries.order.len() > REMEMBERED {
                        if let Some(old) = entries.order.pop_front() {
                            entries.cells.remove(&old);
                        }
                    }
                    cell
                }
            }
        };
        // A failed write leaves the cell empty, so the next attempt with the
        // same id writes for real.
        cell.get_or_try_init(write).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn a_delivered_id_is_never_written_again() {
        let ledger = InputLedger::default();
        let writes = AtomicUsize::new(0);
        for _ in 0..3 {
            let result: Result<(), ()> = ledger
                .deliver("a", || async {
                    writes.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            assert!(result.is_ok());
        }
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_id_is_written_on_resend() {
        let ledger = InputLedger::default();
        let failed: Result<(), &str> = ledger.deliver("a", || async { Err("down") }).await;
        assert_eq!(failed, Err("down"));
        let writes = AtomicUsize::new(0);
        let retried: Result<(), &str> = ledger
            .deliver("a", || async {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(retried.is_ok());
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_resend_during_the_original_write_waits_instead_of_writing() {
        let ledger = Arc::new(InputLedger::default());
        let writes = Arc::new(AtomicUsize::new(0));
        let slow = {
            let (ledger, writes) = (Arc::clone(&ledger), Arc::clone(&writes));
            tokio::spawn(async move {
                ledger
                    .deliver("a", || async {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        writes.fetch_add(1, Ordering::SeqCst);
                        Ok::<(), ()>(())
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let resend = ledger
            .deliver("a", || async {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            })
            .await;
        assert!(resend.is_ok());
        assert!(slow.await.unwrap().is_ok());
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn old_ids_are_forgotten() {
        let ledger = InputLedger::default();
        for n in 0..=REMEMBERED {
            let _: Result<(), ()> = ledger.deliver(&n.to_string(), || async { Ok(()) }).await;
        }
        let entries = ledger.entries.lock().unwrap();
        assert_eq!(entries.cells.len(), REMEMBERED);
        assert!(!entries.cells.contains_key("0"));
    }
}
