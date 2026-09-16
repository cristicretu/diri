# Terminal core probes

Run from `diri/` using optimized builds:

```sh
cargo bench -p diri-terminal-state --bench terminal_throughput
cargo bench -p diri-terminal-state --bench terminal_fleet
cargo bench -p diri-terminal-state --bench terminal_parity > /tmp/terminal-resources.jsonl
```

`terminal_parity` reports seven samples for each 1/10/100-core fleet with either
empty terminals or 10,000 numbered lines of 72-column ASCII text. Every core is
80×24. It records requested live Rust heap after construction, after feeding,
and after initial grid publication, plus peak requested heap and allocations.
The probe checks final output and releases all core allocations after each sample.

Output is JSONL schema 1: one metadata record, 42 sample records and 6 summaries.
Summaries use nearest-rank p50/p90; with seven samples p90 is the largest sample.
Timings include allocator instrumentation and correctness assertions. Record the
machine, OS, compiler, commit and competing load separately when collecting data.

Requested heap excludes allocator overhead/fragmentation, RSS, PTYs, child
processes, raw logs, desktop buffers and GPU resources. Each sample reports actual
retained history rows: feeding 10,000 lines does not imply retaining 10,000 lines.
These probes do not measure input-to-visible latency or full-application memory.

`terminal_fleet` uses the same allocation counter and retains its existing gates
for heap bounds, resize churn, zero warmed cursor allocations and zero leaked heap.
