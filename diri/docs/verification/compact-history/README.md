# Compact terminal history verification

Local macOS arm64 / Apple M4 Max measurements, 2026-09-16, optimized Rust builds.
The screenshot is an HTML benchmark report, not application UI.

## Results

Three alternating dense/compact pairs, with other implementation builds paused.
Dense includes lazy alternate-screen allocation and row-byte reserve sizing.
Both variants receive identical input; compact history retains more physical rows.

| Measurement | Dense | Compact |
| --- | ---: | ---: |
| 20 cores, 80×50, 6,000 short hard lines | 90,359,180 B | 6,549,580 B |
| Same fleet after 320×50 resize | 98,308,940 B | 17,163,340 B |
| Typing, median | 976 ns/op | 999 ns/op |
| Scrolling, median | 42,682 ns/op | 45,133 ns/op |
| Cursor-only, median | 941 ns/op | 927 ns/op |
| Resize churn, median | 65 µs/op | 40 µs/op |

The 20-core full-history heap decreases 92.8%. Scrolling costs about 5.7% more
in this fixture; typing differs by 23 ns. Both remain within existing gates.
No cursor-only allocations or leaked core allocations were observed.

Separately, the 80×24 numbered 10,000-line prose workload retains 9,977 history
rows plus 24 visible rows (including the final blank row) using 278,251 requested
heap bytes after grid publication. Full text, styled-cell and metadata reads
preserve every numbered line and return to the same heap after responses drop.
Dense retained 2,184 history rows under its width-derived allowance.

Memory counts requested Rust heap for terminal cores and the fleet container.
It excludes RSS, allocator overhead, PTYs, processes, logs, renderer and GPU.
The 4 MiB stored-history cap excludes visible cells, cell-extra heap, transient
codec/reflow/read work and caller-owned response buffers. The 407 KiB full-history
benchmark gate is specific to the prose fixture, not a universal per-core cap.

## Correctness and reproduction

The parser suite compares compact and dense rows, occupancy, cursor, styles,
wide/combining text, links, partial scroll regions, alternate screens, reset,
height/width reflow and repeated full-history rotation. Tests also cover dirty
cold rows, high-entropy budget eviction, reduced subsequent budgets, compressed
resize range splitting/coalescing and checkpoint metadata restoration.

Run from `diri/`:

```sh
cargo test -p diri-terminal-state
cargo bench -p diri-terminal-state --bench terminal_parity -- --full-history-read-gate
cargo bench -p diri-terminal-state --bench terminal_fleet -- --resize-gate
cargo bench -p diri-terminal-state --bench terminal_throughput
# Dense baseline for comparison only:
cargo bench -p diri-terminal-state --no-default-features --bench terminal_throughput
```

Raw per-run outputs are in `paired-results.jsonl`, `full-history.txt` and
`resize.txt`. Parser storage is process-local; this change does not implement
full-parser parking or alter a checkpoint/protocol format.
