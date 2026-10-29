//! DIRI PATCH: the seam through which idle terminal history leaves memory.
//!
//! History is stored as immutable encoded payloads (compact history blocks
//! today, packed pages in terminal core v2). A [`ColdStore`] takes such
//! payloads out of the process's memory and gives them back on demand; it
//! knows nothing about rows, cells or codecs. The owner decides when history
//! is idle and where the store lives (the Engine seals payloads into an
//! encrypted, owner-only swap file); storage only swaps a payload's bytes for
//! a slot in the store and reads them back a payload at a time.
//!
//! Moving a payload out never changes what history holds or how it is
//! counted: a stored payload counts its full length against the history
//! budget, exactly as it did in memory, so the rows retained are the same
//! whether or when history was paged out.

use std::io;

/// A process-local store of immutable byte payloads.
///
/// Every call may come from any thread, under the owner's own lock on the
/// terminal. A store is only ever read back by the process that wrote it.
pub trait ColdStore: Send + Sync {
    /// Stores every payload and returns one slot per payload, in order.
    /// Either every payload is stored or, on an error, none is.
    fn store(&self, payloads: &[&[u8]]) -> io::Result<Vec<u64>>;

    /// The payload `store` returned `slot` for. `len` is its length.
    fn load(&self, slot: u64, len: usize) -> io::Result<Vec<u8>>;

    /// Frees `slot`. Called exactly once per stored payload, when the last
    /// history range that refers to it is dropped.
    fn release(&self, slot: u64, len: usize);
}

/// What one page-out moved to a [`ColdStore`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageOut {
    /// Payloads newly written to the store.
    pub payloads: usize,
    /// Their bytes, before the store's own framing.
    pub bytes: usize,
    /// Payloads already in the store, left there.
    pub already_stored: usize,
    /// Bytes still in memory because they belong to the recent window.
    pub kept_bytes: usize,
}
