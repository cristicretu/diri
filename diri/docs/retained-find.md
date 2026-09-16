# Retained local terminal search

Find previously invalidated an in-flight history read on every terminal update. At 50 Hz, a history read taking 40 ms could never produce results. Local Find now captures immutable styled rows, scans off the UI thread, and refreshes at most once per second while following output. Query edits retain the existing 200 ms debounce.

Explicit navigation into historical text pauses the source. Further output, history eviction, reflow, and overlapping viewport fetches cannot replace the selected text. Query edits while paused search those same rows. **Refresh results** and **Return to live** resume current output. Live highlights require exact current row cells and annotations, checked again during terminal prepaint.

## Boundaries and ownership

`session.capture_find` is an additive local Engine API. It acquires a reader under the Registry lock, releases that lock, and captures the cells, annotations, and geometry under the existing screen lock. The reply identifies the Engine-owned Session object, a capture revision, and its content sequence. A same-ID replacement cannot redirect an existing reader or silently replace a Find source.

Capture limits are 160,000 cells, 8,192 rows, and 64 KiB of annotation accounting. The newest contiguous tail is retained. Every included combining mark and link span is complete; if even the visible rows cannot fit, capture fails explicitly. These bounds keep the response within the existing 4 MiB control-line limit. Partial captures and the 500-match navigation cap show **Searching recent output**, including their scope when paused.

The app admits four retained Find views across all windows, up to 8 MiB per decoded source and 32 MiB of actual retained source allocations in total. Accounting includes cells, row arrays, strings, annotation vectors, and source storage. Refresh overlap counts against the same total. Only one control/decode capture is admitted at a time; waiting searches have a two-second admission deadline. Shared `Arc` references let the model, highlights, and reading viewport use the same rows. Find close or residency removal releases these references; in-flight work retains its bounded admission until it finishes. No new mutex or parser hot-path cache is introduced.

Remote sessions retain their existing search path. This API explicitly rejects remote capture. Consistent remote history transfer and searches beyond the retained recent tail remain separate work; this change does not claim complete remote or full-history search coverage.

## Evidence

Deterministic tests cover continuous-output completion, slow same-query reads, changed queries and Session owners, immutable selection through trim/reflow, complete combining metadata, oversized output, resource admission/release, and Registry replacement during capture. Native Metal tests use continuously updating disposable PTYs at narrow/wide widths and nondefault font sizes. A pointer regression exercises relocated Refresh and Close controls without resizing the terminal.

Run the allocation probes with:

```sh
cargo bench -p diri-terminal-state --bench find_snapshot
cargo bench -p diri-term --bench find_retained
```

An optimized local sample at a full 4 MiB parser history budget, across 40/120/320 columns and plain/styled Unicode output, measured:

| Operation / allocation | Range |
| --- | --- |
| Bounded capture plus decode | 1.26–2.69 ms |
| Search | 0.98–1.23 ms |
| Retained source | 2.59–3.08 MB |
| Peak requested capture/search heap | 2.93–7.04 MB |
| Highlight proof, average of 1,000 calls | 2.5–13.9 µs |

These are requested Rust allocation counts and informational timings collected with other Cargo jobs active. They exclude IPC serialization, RSS, GPU memory, and display presentation. Both probes are reproducible; the retained probe also asserts all Find allocations are released after close.
