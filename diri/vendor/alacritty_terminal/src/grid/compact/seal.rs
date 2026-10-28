//! Encoding sealed history blocks on a helper thread.
//!
//! Sealing a block encodes and compresses 64 rows that just scrolled into
//! history. That work only reads the rows, so the storage hands them to a
//! helper thread and keeps parsing. The rows come back with their payload,
//! and the storage collects every outstanding block before it measures
//! history against its byte budget, at the end of each parsed read. History
//! therefore holds exactly what inline encoding would hold, and no encoding
//! is outstanding once a read has been parsed.
//!
//! One helper thread serves the process. It starts with the first sealed
//! block and exits after [`IDLE`] without work, so an idle process has no
//! thread and a session that never fills a block never starts one.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::{EncodeRows, Encoded, Row};

/// How long the helper thread waits for a block before it exits.
const IDLE: Duration = Duration::from_millis(500);

type Job = Box<dyn FnOnce() + Send>;

/// The helper thread's queue, `None` while no helper runs. Jobs are sent
/// while holding the lock, and an idle helper exits only after finding the
/// queue empty while holding it, so no job is left in a queue nobody reads.
static HELPER: Mutex<Option<Sender<Job>>> = Mutex::new(None);

/// One block being encoded: its rows, given back with their payload.
pub(crate) struct SealSlot<T> {
    /// The payload (`None` when encoding panicked) and the block's rows.
    done: OnceLock<(Option<Encoded>, Vec<Row<T>>)>,
}

impl<T> SealSlot<T> {
    /// The finished block, waiting for the helper if needed.
    pub(crate) fn wait(&self) -> (&Encoded, &[Row<T>]) {
        let (encoded, rows) = self.done.wait();
        (encoded.as_ref().expect("history block encoding panicked"), rows)
    }

    /// Whether the helper has finished this block.
    pub(crate) fn is_done(&self) -> bool {
        self.done.get().is_some()
    }

    /// A finished slot holding `encoded` and `rows`.
    pub(crate) fn finished(encoded: Encoded, rows: Vec<Row<T>>) -> Arc<Self> {
        let done = OnceLock::new();
        let _ = done.set((Some(encoded), rows));
        Arc::new(Self { done })
    }

    /// The finished block, once the helper has let go of the slot.
    pub(crate) fn take(mut slot: Arc<Self>) -> (Encoded, Vec<Row<T>>) {
        slot.done.wait();
        // The helper drops its reference right after finishing.
        let slot = loop {
            match Arc::try_unwrap(slot) {
                Ok(slot) => break slot,
                Err(shared) => {
                    slot = shared;
                    std::thread::yield_now();
                },
            }
        };
        let (encoded, rows) = slot.done.into_inner().expect("finished block");
        (encoded.expect("history block encoding panicked"), rows)
    }
}

/// Encode `rows` with `encode` on the helper thread. `Err` gives the rows
/// back when no helper thread could be started.
pub(crate) fn seal<T: Send + Sync + 'static>(
    rows: Vec<Row<T>>,
    encode: EncodeRows<T>,
) -> Result<Arc<SealSlot<T>>, Vec<Row<T>>> {
    let slot = Arc::new(SealSlot { done: OnceLock::new() });
    let mut helper = HELPER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if helper.is_none() {
        let (sender, receiver) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("diri-history".into())
            .stack_size(256 * 1024)
            .spawn(move || run(receiver));
        if spawned.is_err() {
            return Err(rows);
        }
        *helper = Some(sender);
    }
    let shared = slot.clone();
    let job: Job = Box::new(move || {
        let encoded = catch_unwind(AssertUnwindSafe(|| encode(&rows))).ok();
        let _ = shared.done.set((encoded, rows));
        drop(shared);
    });
    // A helper keeps its receiver until it removes its sender under the
    // lock, and jobs never unwind out of it, so the send cannot fail.
    helper
        .as_ref()
        .expect("history helper")
        .send(job)
        .unwrap_or_else(|_| panic!("history helper exited with its queue reachable"));
    Ok(slot)
}

fn run(receiver: mpsc::Receiver<Job>) {
    loop {
        match receiver.recv_timeout(IDLE) {
            Ok(job) => job(),
            Err(RecvTimeoutError::Timeout) => {
                let mut helper = HELPER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                match receiver.try_recv() {
                    Ok(job) => {
                        drop(helper);
                        job();
                    },
                    Err(_) => {
                        *helper = None;
                        return;
                    },
                }
            },
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
