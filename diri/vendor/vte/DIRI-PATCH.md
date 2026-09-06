# Diri VTE patch

Source: crates.io `vte` 0.15.0, copied from the Cargo registry. Original
Apache-2.0/MIT licenses and upstream tests are retained. The only production
change is `SyncState::default`: its byte buffer starts empty instead of reserving
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

Verification from `diri/`:

```sh
cargo test -p vte
cargo test -p diri-terminal-state
cargo bench -p diri-terminal-state --bench terminal_fleet --bench terminal_throughput
```
