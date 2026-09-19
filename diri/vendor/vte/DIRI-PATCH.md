# Diri VTE patch

Source: crates.io `vte` 0.15.0, copied from the Cargo registry. Original
Apache-2.0/MIT licenses and upstream tests are retained. There are two
production changes. The first is `SyncState::default`: its byte buffer starts empty instead of reserving
the 2 MiB synchronization limit for every terminal, including idle shells.

Original crates.io archive SHA-256:
`a5924018406ce0063cd67f8e008104968b74b563ee1b85dde3ed1f7cb87d3dbd`.

The existing `extend` grows the buffer on demand and retains its capacity for
later frames. Parsing, the 2 MiB limit, nested synchronization, split escapes,
and the 150 ms timeout remain unchanged. This avoids 40 MiB of requested heap
for twenty terminal cores that have not used synchronized output. Requested
heap is not equivalent to resident physical memory.

Tradeoff: the first synchronized frame can allocate/reallocate while arriving;
subsequent frames of that size reuse the allocation. This is a focused fork of
an existing dependency, not an additional parser or a new runtime dependency.
Recheck or remove the patch whenever upgrading VTE.

## Bounded heap OSC buffer

With `std`, upstream's `osc_raw` is a `Vec<u8>` with no maximum: the `is_full`
guards are compiled only for the fixed-size no-std buffer. A program that
prints `ESC ]` and never terminates it grows that buffer by every byte it
prints afterwards, inside the daemon and the Remote Helper, for as long as the
session lives.

The buffer is now capped at 2 MiB (`MAX_OSC_RAW_STD`), the same figure as the
synchronized-output limit, which still fits a large OSC 52 copy. An OSC that
passes the cap is dropped whole rather than dispatched truncated, because a
cut-off title, hyperlink or clipboard payload is worse than none; its buffer is
freed at once and the parser returns to ground at the terminator as before.
After any dispatch the buffer keeps at most 64 KiB of capacity, so one large
clipboard copy does not stay allocated per terminal.

Tests: `an_osc_past_the_heap_limit_is_dropped_and_frees_its_buffer`,
`a_large_osc_under_the_limit_is_whole_and_its_capacity_is_returned`.

Verification from `diri/`:

```sh
cargo test -p vte
cargo test -p diri-terminal-state
cargo bench -p diri-terminal-state --bench terminal_fleet --bench terminal_throughput
```
