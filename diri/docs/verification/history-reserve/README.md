# History reserve allocation

The parser previously allocated 1,000 history rows on first scroll. Reserve rows
now scale with row byte size, targeting 64 KiB of cell/row storage, capped at 1,000
rows and with a minimum of one. Required live rows are always allocated. History
retention and the 4 MiB history-cell allowance are unchanged.

| Local workload | Before | After |
| --- | ---: | ---: |
| 80×24 core, first history row, after publication | 2,037,667 B | 150,083 B |
| 20 cores, 80×50, filled history | 121,296,780 B | 90,359,180 B |
| 160×50 scrolling | 41,283 ns/op | 40,797 ns/op |
| Fleet resize churn | 229 µs/op | 58 µs/op |

Before includes the deferred alternate-screen change. Requested heap excludes
process RSS, allocator overhead, PTYs, renderer and GPU. Three alternating pairs
of cached optimized binaries ran on macOS arm64 while the other implementation
agents held builds. Table timings are medians of the three benchmark results;
these are local workload measurements, not a universal speedup claim.
`paired-results.jsonl` retains stdout, exit status and elapsed time. All twelve
benchmark invocations passed their existing gates, including zero cursor-only
allocations and complete deallocation. Filled 20-core history after widening to
320 columns still measured 98,308,940 bytes; this change does not remove retained
row capacity from reflow or compact the history's per-cell representation.

```sh
cargo bench -p diri-terminal-state --bench terminal_parity -- --short-history-gate
cargo bench -p diri-terminal-state --bench terminal_parity
cargo bench -p diri-terminal-state --bench terminal_throughput
cargo bench -p diri-terminal-state --bench terminal_fleet
cargo test -p diri-terminal-state
```

The short-history gate covers 23/24/25 hard-newline input lines, spanning the first
history allocation, and asserts 192 KiB requested heap plus correct history rows.
Before: 23 lines used 85,667 bytes; 24/25 used 2,037,667 bytes and failed the budget.
After: 24/25 use 150,083 bytes. Existing parser tests plus row-width coverage test
ring storage, resize and reflow. The reserve may contain one row larger than
64 KiB at extreme widths; existing vector capacity is separate from this target.

The original 1/10/100-core resource matrix also completed with all deallocation
assertions passing (`resources.jsonl`). At 80×24 after 10,000 numbered 72-column
ASCII lines, requested heap after publication fell from 5,975,971 to 4,446,755 bytes;
retention remains 2,184 history rows plus 24 visible rows. Timing fields in this
matrix were collected alongside other builds and are not comparison evidence.
