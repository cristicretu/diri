//! DIRI PATCH: page-backed memory for history payloads.
//!
//! Encoded history payloads are small (a few KiB per 64 rows) and live as long
//! as their history. Allocated one by one from the general allocator they end
//! up interleaved with the terminal's other long-lived allocations (rows,
//! above all), so freeing them (eviction, or paging idle history out) leaves
//! the allocator's pages pinned by those neighbours and the memory resident:
//! measured, an Engine that paged 368 MB of history out gave 190 MB of it
//! back. Payloads are therefore copied into chunks of their own, mapped
//! straight from the system and owned by one history: a chunk returns to the
//! system as soon as no payload in it is left, and history is evicted (and
//! paged out) oldest first, so chunks empty whole.
//!
//! Only pages a payload was written to are ever touched, so a chunk's unused
//! tail costs address space, not memory.

use std::ptr::NonNull;
use std::sync::Arc;

/// Bytes mapped per chunk. A payload larger than a quarter of it gets a
/// chunk of its own size.
const CHUNK_BYTES: usize = 128 << 10;

/// One mapping. Bytes below the arena's fill mark are immutable once
/// written; the arena that owns the chunk is the only writer of the rest.
pub(super) struct Chunk {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: a chunk is plain memory. Written bytes are never written again and
// are only read through `Slice`s; unwritten bytes are only written through
// the one `Arena` that owns the chunk (see `Arena::alloc`), never read.
unsafe impl Send for Chunk {}
// SAFETY: as above.
unsafe impl Sync for Chunk {}

impl Chunk {
    fn mapped(len: usize) -> Chunk {
        Chunk::new(len).unwrap_or_else(|| {
            std::alloc::handle_alloc_error(
                std::alloc::Layout::from_size_align(len, 4096).expect("chunk layout"),
            )
        })
    }

    fn new(len: usize) -> Option<Chunk> {
        #[cfg(test)]
        LIVE_CHUNKS.with(|live| live.set(live.get() + 1));
        #[cfg(unix)]
        {
            // SAFETY: an anonymous private mapping; failure is checked.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return None;
            }
            NonNull::new(ptr.cast()).map(|ptr| Chunk { ptr, len })
        }
        #[cfg(not(unix))]
        {
            let layout = std::alloc::Layout::from_size_align(len, 4096).ok()?;
            // SAFETY: a non-zero layout; failure is checked.
            NonNull::new(unsafe { std::alloc::alloc(layout) }).map(|ptr| Chunk { ptr, len })
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Chunks mapped and not yet unmapped on this thread.
    pub(super) static LIVE_CHUNKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Drop for Chunk {
    fn drop(&mut self) {
        #[cfg(test)]
        LIVE_CHUNKS.with(|live| live.set(live.get() - 1));
        #[cfg(unix)]
        // SAFETY: the mapping made in `Chunk::new`, unmapped once.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
        #[cfg(not(unix))]
        // SAFETY: the allocation made in `Chunk::new` with this layout.
        unsafe {
            std::alloc::dealloc(
                self.ptr.as_ptr(),
                std::alloc::Layout::from_size_align_unchecked(self.len, 4096),
            );
        }
    }
}

/// Immutable bytes in a chunk; keeps its chunk mapped.
#[derive(Clone)]
pub(super) struct Slice {
    chunk: Arc<Chunk>,
    offset: u32,
    len: u32,
}

impl Slice {
    pub(super) fn as_slice(&self) -> &[u8] {
        // SAFETY: `offset..offset + len` lies in the chunk and was written
        // before this slice was made; it is never written again.
        unsafe {
            std::slice::from_raw_parts(
                self.chunk.ptr.as_ptr().add(self.offset as usize),
                self.len as usize,
            )
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    /// The bytes' address: equal for clones, distinct for distinct slices.
    pub(super) fn as_ptr(&self) -> *const u8 {
        // SAFETY: in bounds of the chunk (see `as_slice`).
        unsafe { self.chunk.ptr.as_ptr().add(self.offset as usize) }
    }
}

/// Where one history copies its payloads: the chunk being filled.
///
/// A clone starts empty, so two histories never write into one chunk.
#[derive(Default)]
pub(super) struct Arena {
    head: Option<Arc<Chunk>>,
    used: usize,
}

impl Clone for Arena {
    fn clone(&self) -> Self {
        Arena::default()
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena").field("used", &self.used).finish()
    }
}

impl Arena {
    /// A copy of `bytes` in this arena's memory. Like any allocation, it
    /// aborts when the system has no memory left.
    pub(super) fn alloc(&mut self, bytes: &[u8]) -> Slice {
        // Every slice takes at least a byte, so distinct slices have
        // distinct addresses (the history budget tells payloads apart by
        // address).
        let len = bytes.len().max(1);
        let fits = self
            .head
            .as_ref()
            .is_some_and(|head| head.len - self.used >= len);
        if !fits {
            if len > CHUNK_BYTES / 4 {
                // Its own chunk; the head keeps filling.
                let chunk = Arc::new(Chunk::mapped(len.next_multiple_of(4096)));
                return Self::write(chunk, 0, bytes);
            }
            self.head = Some(Arc::new(Chunk::mapped(CHUNK_BYTES)));
            self.used = 0;
        }
        let head = Arc::clone(self.head.as_ref().expect("arena head"));
        let offset = self.used;
        self.used += len;
        Self::write(head, offset, bytes)
    }

    fn write(chunk: Arc<Chunk>, offset: usize, bytes: &[u8]) -> Slice {
        debug_assert!(offset + bytes.len() <= chunk.len);
        // SAFETY: `offset..offset + len` is in the chunk and was never handed
        // out (the fill mark only grows), so no `Slice` reads it; only this
        // arena writes, through `&mut self`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                chunk.ptr.as_ptr().add(offset),
                bytes.len(),
            );
        }
        Slice {
            chunk,
            offset: offset as u32,
            len: bytes.len() as u32,
        }
    }

    /// Stop filling the current chunk: the next payload starts a new one.
    /// Payloads already in it keep it mapped until they are gone.
    pub(super) fn retire(&mut self) {
        self.head = None;
        self.used = 0;
    }
}
