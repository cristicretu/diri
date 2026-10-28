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
//! The parser never waits for the helper: a block it needs before the helper
//! has finished it (the helper is behind, or descheduled on a busy machine)
//! it encodes itself, from the same rows. At worst that is inline encoding,
//! and the helper's copy of the work is discarded.
//!
//! Up to [`HELPERS`] helper threads serve the process. They start with the
//! first sealed block and exit after [`IDLE`] without work, so an idle
//! process has no helper thread and a session that never fills a block never
//! starts one.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::{EncodeRows, Encoded, Row};

/// How long the helper thread waits for a block before it exits.
const IDLE: Duration = Duration::from_millis(500);

type Job = Box<dyn FnOnce() + Send>;

/// The most helper threads. Encoding and compressing a row is about as much
/// work as parsing it, so one helper keeps up with plain text at about half
/// the parser's pace; a flood needs two.
const HELPERS: usize = 2;

/// The helpers' shared queue. Jobs are sent while holding the lock, and the
/// last idle helper removes the queue only after finding it empty while
/// holding the lock, so no job is left in a queue nobody reads.
struct Pool {
    sender: Option<Sender<Job>>,
    receiver: Option<Arc<Mutex<Receiver<Job>>>>,
    helpers: usize,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    sender: None,
    receiver: None,
    helpers: 0,
});

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One sealed block: its rows, read-only until it is collected, and their
/// payload once the helper or the owner has encoded them.
pub(crate) struct SealSlot<T> {
    rows: Vec<Row<T>>,
    encode: EncodeRows<T>,
    done: OnceLock<Encoded>,
}

impl<T> SealSlot<T> {
    /// The payload, encoded here unless the helper has finished it.
    pub(crate) fn encoded(&self) -> &Encoded {
        if let Some(encoded) = self.done.get() {
            return encoded;
        }
        let encoded = (self.encode)(&self.rows);
        // The helper may have finished meanwhile; both payloads are equal.
        let _ = self.done.set(encoded);
        self.done.get().expect("encoded block")
    }

    /// The block's rows.
    pub(crate) fn rows(&self) -> &[Row<T>] {
        &self.rows
    }

    /// Whether the block is encoded.
    pub(crate) fn is_done(&self) -> bool {
        self.done.get().is_some()
    }

    /// A slot already holding `encoded`.
    pub(crate) fn finished(
        encoded: Encoded,
        rows: Vec<Row<T>>,
        encode: EncodeRows<T>,
    ) -> Arc<Self> {
        let done = OnceLock::new();
        let _ = done.set(encoded);
        Arc::new(Self { rows, encode, done })
    }

    /// The payload, and the rows unless the helper is still holding them
    /// (then it frees them when it is done).
    pub(crate) fn take(slot: Arc<Self>) -> (Encoded, Option<Vec<Row<T>>>) {
        let encoded = slot.encoded().clone();
        let rows = Arc::try_unwrap(slot).ok().map(|slot| slot.rows);
        (encoded, rows)
    }

    pub(crate) fn encoder(&self) -> EncodeRows<T> {
        self.encode
    }
}

/// Queue `rows` for encoding with `encode` on the helper thread. `Err`
/// gives the rows back when no helper thread could be started.
pub(crate) fn seal<T: Send + Sync + 'static>(
    rows: Vec<Row<T>>,
    encode: EncodeRows<T>,
) -> Result<Arc<SealSlot<T>>, Vec<Row<T>>> {
    let mut pool = lock(&POOL);
    if pool.sender.is_none() {
        let (sender, receiver) = mpsc::channel::<Job>();
        pool.sender = Some(sender);
        pool.receiver = Some(Arc::new(Mutex::new(receiver)));
    }
    while pool.helpers < HELPERS {
        let receiver = pool.receiver.clone().expect("helper queue");
        let spawned = std::thread::Builder::new()
            .name("diri-history".into())
            .stack_size(256 * 1024)
            .spawn(move || run(receiver));
        if spawned.is_err() {
            break;
        }
        pool.helpers += 1;
    }
    if pool.helpers == 0 {
        return Err(rows);
    }
    let slot = Arc::new(SealSlot {
        rows,
        encode,
        done: OnceLock::new(),
    });
    // The helper only borrows the slot: a block the owner already collected
    // or dropped (a closed terminal) is skipped.
    let weak: Weak<SealSlot<T>> = Arc::downgrade(&slot);
    let job: Job = Box::new(move || {
        let Some(slot) = weak.upgrade() else {
            return;
        };
        if slot.done.get().is_none() {
            // A panic leaves the block to the owner, which then panics
            // where inline encoding would have.
            if let Ok(encoded) = catch_unwind(AssertUnwindSafe(|| (slot.encode)(&slot.rows))) {
                let _ = slot.done.set(encoded);
            }
        }
    });
    // A helper keeps its receiver until it removes its sender under the
    // lock, and jobs never unwind out of it, so the send cannot fail.
    pool.sender
        .as_ref()
        .expect("history helper")
        .send(job)
        .unwrap_or_else(|_| panic!("history helper exited with its queue reachable"));
    Ok(slot)
}

fn run(receiver: Arc<Mutex<Receiver<Job>>>) {
    loop {
        // One idle helper waits on the queue; the others wait for it.
        let queue = lock(&receiver);
        let job = match queue.recv_timeout(IDLE) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                let mut pool = lock(&POOL);
                match queue.try_recv() {
                    Ok(job) => job,
                    Err(_) => {
                        pool.helpers -= 1;
                        if pool.helpers == 0 {
                            pool.sender = None;
                            pool.receiver = None;
                        }
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        drop(queue);
        job();
    }
}
