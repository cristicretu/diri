# Idle terminal history on disk

An idle session's terminal history leaves the Engine's memory for an
encrypted, owner-only swap file, and comes back a block at a time when
something reads it.

## Why

Compact history keeps each session's scrollback as LZ-compressed blocks of 64
rows, bounded at 4 MiB of live bytes (#830, #921). 100,000 styled lines take
about 3.7 MB of it, held in the Engine for as long as the session lives. Most
sessions are idle most of the time, and nobody reads their history while they
are. At 100 sessions with 100,000 styled lines each, the Engine measured
4.95 MB per session, against 1.4 MB for a reference multiplexer that suspends
idle terminals to an encrypted file.

## When history pages out

The page-out is one more rung on the held pump's idle ladder (#839, #846).
No timer, thread or wakeup is added per session:

1. Output stops. The pump settles (status, checkpoint) and parks on the
   reactor, releasing caches (`compact_idle`), as before.
2. On that park, if the session's history holds at least 64 KiB, the pump
   notes the time and the screen's history read count, and parks with its
   backstop timer set to the page-out deadline (60 s) instead of the usual
   300 s. A session with less history parks as before and never pages out.
3. When the timer fires, the pump checks the read count. If history was read
   meanwhile (scroll, Find, link detection, a checkpoint), the clock restarts
   and it parks again. Otherwise it pages history out and parks with the
   300 s backstop.
4. Output resets the rung: the next park starts the clock again.

So an idle session costs at most one extra wakeup per idle period, and none
once its history is on disk. A session the user typed into stays on the fast
tick for 30 s (`HOT_WINDOW_SECS`) and only parks after that, so the page-out
comes about 90 s after the last keystroke.

`DIRI_HISTORY_SWAP=0` turns paging off; `DIRI_HISTORY_SWAP_IDLE_MS` changes
the 60 s (tests and benchmarks).

## What moves, and what stays

- **Moves:** every sealed history block's payload except the recent window,
  and the source blocks of a deferred reflow (#921 narrowing).
- **Stays in memory:** the visible grid, rows not yet sealed into a block, the
  newest blocks holding at least 256 history rows (two screens if that is
  more), the block index (`VecDeque<Block>`, about 100 bytes per 64 rows), a
  48-byte record per stored payload, and eagerly reflowed rows.
- **Minimum:** when less than 64 KiB would move, nothing moves. A second idle
  period after a few commands therefore writes nothing at all.

## Freed memory goes back to the system

Paging out frees the payloads, but freed memory only lowers the Engine's
footprint if the allocator can return its pages. Payloads used to be
individual `Arc<[u8]>` allocations of about 2.3 KB, interleaved in the
allocator's small regions with the terminal's long-lived rows (2.9 KB at
120 columns). Measured at 100 sessions × 100,000 styled lines, paging 368 MB
out lowered the footprint by only 190 MB and left 223 MB of the small zone
free but dirty, pinned by about 6,500 rows.

So each history now copies its payloads into chunks of its own
(`grid/compact/arena.rs`): 128 KiB anonymous mappings (`mmap`), one writer,
filled in order. Eviction and page-out free payloads oldest first, so chunks
empty whole and are unmapped at once; a page-out also moves the recent window
to a fresh chunk so nothing pins the old ones. Only written pages are ever
touched, so a short history costs one page. The copy replaces the copy into
an `Arc<[u8]>` the encoder already made, and the encoder's buffer is reused,
so sealing does no more work than before. Payload length, sharing and the
budget are unchanged.

## Reads

A paged-out block is read back when a row of it is read: `Payload::bytes`
loads and opens the one record, the block decodes into its existing read
cache, and the loaded bytes are dropped. The read cache is released at the
same batch boundaries as before (`finish_history_read_batch`), so a scroll,
Copy, link scan or checkpoint export never brings history back into memory:
a stored payload stays stored until history evicts it or the session ends.
Find's capture reads through `&self` and used to leave up to 8,192 decoded
rows behind until the session's next output; the Engine now releases them
right after the capture (`HeadlessScreen::release_history_read_cache`). Recycling the oldest row when history is full reads the oldest
block once (the eviction index), as it decodes it today.

Nothing a reader sees changes. `history_storage_bytes` counts a stored
payload at its full length, exactly as in memory, and ranges split from one
payload keep sharing one identity, so the 4 MiB live-byte budget evicts the
same rows whether or when history was paged out (#921). `Block` keeps its
size (`Payload` is the size of the `Arc<[u8]>` it replaces), so the budget's
per-block term is unchanged and the vt_workloads golden is untouched.

If a record cannot be read back (the file was deleted, the disk failed), the
block reads as blank rows of its width and the Engine logs
`session.history_swap_read_failed`: a read never panics or blocks on it.

## The swap file

- **Path:** `<logs>/history-swap/<pid>-<random>/<random 64-bit id>.swap`,
  one file per session that paged out. Directories are created `0700` and
  checked to be real directories owned by the user (a symlink is refused);
  files are created `0600` with `O_EXCL | O_NOFOLLOW`, and reopened with
  `O_NOFOLLOW`.
- **Descriptors:** none held while idle. A page-out opens the file once for
  its writes; a scroll or Find read opens it per record; a checkpoint export
  holds one read descriptor (and the last record it opened, since read
  batches do not end on block boundaries) for the length of the export.
- **Records:** `counter (8 bytes) | ciphertext | GCM tag (16)`, 24 bytes of
  framing per 64-row block (0.6%).
- **Sealing:** AES-256-GCM (RustCrypto `aes-gcm` 0.11, which uses the ARMv8
  AES and PMULL instructions, detected at run time, with no build flag).
  The key is 32 bytes from the OS random source, made once per Engine
  process and held only in memory; it is never written anywhere.
  The 96-bit nonce is a random 32-bit per-process prefix and a 64-bit
  process-wide counter, so no nonce repeats under the key. The file's random
  id, the record's offset and its length are authenticated with each record:
  a record that is modified, truncated, moved within the file or copied from
  another file fails to open.
- **Space:** freed records (history evicted, a re-encoded block) go to a free
  list and are reused first-fit before the file grows; a free tail of 1 MiB
  or more is truncated, and a file with no live record is removed. Live data
  is bounded by the history budget, so a file stays at about the session's
  compact history size.
- **Writes:** sealed into a 64 KiB staging buffer and written with `pwrite`,
  one call per 64 KiB of contiguous records. No `fsync`: the file is only
  ever read by the process that wrote it, through the same page cache.

## Lifetime, crashes and restarts

- A swap file is removed when its session's history is dropped (the session
  ends or is removed), and emptied files are removed as soon as their last
  record is freed.
- A record is handed out only after its write completed, and only the writing
  process (which alone has the key) ever reads it, at offsets it recorded. A
  crash therefore leaves nothing that is trusted: the next Engine cannot read
  it, and its startup sweep (`history_swap::sweep_stale`, in the existing
  one-shot orphan sweep thread) removes every `<pid>-<random>` directory whose
  process is gone, including one left by an earlier process with this pid.
  Directories of another live Engine (a development build) are kept. Only
  names of that form are touched, links are removed as links and never
  followed, and only regular files are deleted.
- **Engine restart keeps history exactly as before.** The swap is a memory
  extension, never a recovery source. Durable history is still the screen
  checkpoint (`<id>.screen.plist`, the newest 10,000 rows), written when
  output settles, a minute before any page-out, and rewritten after later
  output by reading paged-out history back block by block. An adopting
  Engine restores from the checkpoint and the log tail, as today, and pages
  out again once the session idles.

## The seam, and terminal core v2

`alacritty_terminal::grid::cold::ColdStore` (re-exported by
`diri-terminal-state`) is a store of opaque, immutable byte payloads:
`store(&[&[u8]]) -> slots`, `load(slot, len)`, `release(slot, len)`. It knows
nothing about rows, cells or codecs. Compact history uses it through
`CompactRows::page_out` and `Payload`; `HeadlessScreen::page_out_history`
is the entry point, and the Engine's `SwapFile` is the one implementation.

Core v2 (`docs/core-v2.md`) replaces compact history with pages, packing
cold pages into immutable byte strings. Those packed pages page out through
the same trait: a packed page becomes a stored payload, the one-page unpack
cache reads it back, and its live-byte budget counts the stored length.
The pump's rung, the swap file and the sweep stay as they are.

## Measurements

Full tables and commands are in `PERF.md` ("Idle history on disk"). At 100
sessions × 100,000 styled lines the Engine went from 4.95 to 1.13–1.53 MB per
session (`phys_footprint`, 80–125 s idle), with 1.5 MB of history left in
memory for the whole fleet and 373 MB on disk. The first screen of
paged-out history reads in 0.12–0.40 ms at p95 (0.07 ms resident); a
checkpoint export of 10,000 paged-out rows takes 9.8–10 ms at p50, as from
memory (12.7–13.5 ms p95).
An idle transition writes the history sealed since the last one: 3.7 MB for
a fresh 100,000-line session, nothing after a few commands.

## Not changed

- Remote sessions (history lives in the remote Holder's screen; the local
  mirror is not paged), direct (non-held) local pumps, and the Remote Helper
  itself.
- The 4 MiB history budget, the 150,000-row ceiling, checkpoint contents and
  format, and read results.

## Follow-ups

- A second compression stage at page-out (payloads are only LZ-compressed per
  block) would cut disk use several times, at the cost of reading more than
  one block for a screen.
- Paging the remote mirror and the Remote Helper's own screens.
