# Terminal core v2

Status: design and prototype. The prototype is `diri-terminal-state/src/core_v2`
behind the off-by-default `core-v2` feature. Neither the Engine nor the Remote
Helper uses it.

## Why

Today every byte a program prints goes through the vendored `vte` parser into
the vendored `alacritty_terminal` grid: 24-byte `Cell`s in a `Storage` ring of
`Row`s. Scrollback goes to the compact history, which LZ-encodes every row as
it leaves the screen. Measured headless at 120×80 in 64 KiB reads, a reference
terminal reaches about 620 MB/s on ASCII, 930 MB/s on Unicode and 200 MB/s on
CSI. In the Engine's configuration (compact history), Diri's current core
reaches 130 / 125 / 102 MB/s. With history off it reaches 400 / 230 / 106 MB/s.

Three costs dominate:

1. **Cell size.** A 24-byte cell (`char`, two 4-byte colors, 4-byte flags and an
   `Option<Arc<CellExtra>>`) means every printed character writes 24 bytes and
   every scrolled row resets 120 × 24 bytes. An 8-byte cell measured 1.6× on
   ASCII and 1.35× on Unicode in isolation, on the same parser and grid.
2. **Text decoding.** The parser validates UTF-8 (`str::from_utf8`), splits runs,
   and hands over `&str`. The grid then decodes each character again and looks
   up its width in a two-stage table, plus the cluster test. On Unicode text
   this is about half of what remains once cells are small.
3. **History encoding.** Each row is encoded once when it scrolls off, so the
   Engine is capped near 230 MB/s whatever the parser does.

Core v2 replaces the grid and the ground-state text path. The escape-sequence
state machine stays `vte`. Its escape bodies are already indexed once per read
(#922). The wire protocol, the renderer and every client stay as they are.

## Prototype results

The prototype covers printable text (ASCII and UTF-8, grapheme clusters,
wrap), C0 controls, SGR, REP, cursor moves, `EL`, and linefeed into page
history. Its history is bounded like the Engine's: at most 150,000 rows and
4 MiB of live bytes, with cold pages packed (see "Compression").

**How it was measured:**

- Machine: Apple M4 Max with 14 cores. Load average was 25–42 from other agents, and it moved enough between runs that only same-run comparisons are meaningful.
- Payloads: the kitten `__benchmark__` captures (`ascii.bin`, `unicode.bin`, `csi.bin`), repeated to at least 64 MiB, the same repetitions as `vt_workloads`.
- Reads: 64 KiB.
- Screen: 120×80.
- Rounds: 7. Each round runs v2, then the Engine's `HeadlessScreen` (compact history bounded at 4 MiB, the real Engine configuration).
- Figures: medians, with min..max in brackets. Base is `3fc4feb66`.

**Every timed v2 run passes two checks**, or it reports no number:

1. **The golden digests** of `vt_workloads.golden` (branch `test/vt-bench-golden`).
   - These are digests of a known-good byte-at-a-time parse at `3fc4feb66`: bytes fed, the full snapshot, the newest 62,401 / 53,361 / 0 history rows as scrollback text, cells and metadata, links, and the cursor.
   - They are computed on the v2 state, packed pages included, through `HeadlessScreen::from_core_v2` with the same encoders the bench uses.
2. **A cell-for-cell comparison** with a byte-at-a-time parse by the current core keeping as many history rows.
   - It covers the screen and all retained history: character, cluster codepoints, width flags, wrap, style flags, foreground, background, underline color, cursor and pending wrap.
   - It also asserts that every payload byte was fed.

Both checks, and the packed-history differential tests, were mutation-tested: one flipped write fails them.

```sh
cargo run --release -p diri-terminal-state --features core-v2 --example core_v2_bench -- <kitten dir>
CORE_V2_GOLDEN=<vt_workloads.golden> cargo run ...       # as measured: 150,000 rows, 4 MiB
CORE_V2_BUDGET=none CORE_V2_HISTORY=0 cargo run ...      # history off
CORE_V2_RSS=v2|engine CORE_V2_ONLY=ascii cargo run ...   # peak RSS, one core per process
```

All figures are MB/s.

**History at budget.** A sustained flood keeps 4 MiB of live history from the second repetition on.

| Workload | v2 | Engine today | Reference terminal | v2 rows kept | Engine rows kept |
|---|---:|---:|---:|---:|---:|
| ASCII | **590** (562..604) | 125 (118..127) | 630 (622..636) | 62,705 | 63,647 |
| Unicode | **436** (430..453) | 118 (117..123) | 948 (920..959) | 53,424 | 54,207 |
| CSI | **274** (271..280) | 101 (98..104) | 210 (207..211) | 0 | 0 |

**History off.**

| Workload | v2 | Engine today |
|---|---:|---:|
| ASCII | 777 (757..784) | 125 (123..126) |
| Unicode | 775 (754..778) | 122 (113..123) |
| CSI | 270 (229..279) | 97 (95..100) |

**Peak RSS of one 64 MiB flood**, each core alone in a fresh process whose start-up RSS is 4.1–4.3 MB:

| Workload | v2 | Engine today |
|---|---:|---:|
| ASCII | 11.3 MB | 10.9 MB |
| Unicode | 9.3 MB | 10.8 MB |

**Reading the tables honestly:**

- **v2 is slower than the reference with history on: 0.94× on ASCII and 0.46× on Unicode.** It is ahead only on CSI (1.30×).
- **Where the history cost goes.** History costs v2 about a quarter on ASCII and close to half on Unicode, against history off. The cost is mostly keeping each row's packed tokens as it is written (the "text log" below), and the general path for rows with clusters.
- **v2 keeps slightly fewer rows** than the Engine in the same 4 MiB: 1.5% fewer on ASCII and 1.4% fewer on Unicode. The golden check covers the newest 62,401 and 53,361 rows, which both keep.
- **Peak RSS is about the same as today.** ASCII is 0.4 MB higher and Unicode 1.5 MB lower. In both cores, the rest beyond the 4 MiB history is allocator slack and the screen.
- **The reference ran differently.**
  - It ran as its own benchmark binary, `+terminal-stream`, on the same 64 MiB files in 64 KiB reads, at commit `6ba1441`.
  - Each figure is the median of 7 runs. Process start-up was measured with an empty file (10 ms) and subtracted.
  - Its scrollback is its benchmark default of 10 KB, about one page: it keeps no history at budget and compresses nothing.
  - It runs a complete terminal, while v2 handles only the sequences above. On these three payloads that is everything they contain: the prototype counts any sequence it does not implement, and the checks would fail.
- **Part of the CSI gain is not v2-specific.** It comes from the new `Handler::input_repeat` hook (REP as row segments), which the current core could adopt too.
- **Images and long escapes were not measured.** They are bound by escape bodies (#922). v2 does not change them.

## Design

### Cell: 8 bytes

```
bits  0..21  codepoint (0 = never written; reads as a space)
bits 21..26  WIDE, SPACER, LEADING_SPACER, GRAPHEME, HYPERLINK
bits 26..31  shell marks (OSC 133 A, B, C, D-ok, D-failed)
bits 32..48  style id in the owning page's style table (0 = default)
bits 48..64  cluster index in the page's arena (with GRAPHEME)
```

- **What a cell holds.** Everything that was in `Cell::extra` (cluster codepoints, hyperlink, underline color) moves out of the cell. Underline color goes into the style. Clusters and links become page tables, flagged by one bit each.
- **Row facts move to the row.** Soft wrap (`WRAPLINE`) becomes a row flag.
- **Writing stays in place.** A write is a single `u64` store. Shell marks are kept by `old & MARKS`, exactly as `Flags::SHELL_MARKS` is today.

### Style and grapheme tables, per page

- **Styles.**
  - A `Style` is `{fg, bg, underline color, style flags}`. Each page interns styles in its own table, so a cell carries a 16-bit id.
  - The pen caches `(page serial, id)`. SGR only invalidates the cache, and the next write re-interns.
  - Per-page tables mean eviction frees styles with the page, with no reference counts.
  - Styles are interned only when a cell or a row fill is written, so a page never holds more styles than cells plus rows, well under `u16::MAX`. An SGR flood that writes nothing interns nothing.
- **Clusters.**
  - The extra codepoints of a cluster live in a per-page arena: a `Vec<char>` and `(start, len)` spans. The cell's top 16 bits index its span while the GRAPHEME bit is set.
  - Adding a codepoint appends in place when the span is last in the arena, and otherwise moves the span to the end.
  - Overwriting a cell clears the bit and leaves a dead span. The arena is compacted (rebuilt from the cells that still name a span) when it reaches 65,535 spans or 1 Mi codepoints, and it is dropped when the page is packed.
  - A zero-width mark after a cell that is not a lone regional indicator, keycap or emoji modifier goes straight onto that cell, skipping the cluster replay that `continue_cluster` would decline anyway.
- **Hyperlinks** (OSC 8, and inline-image anchors, which are hyperlinks) use the same shape: a per-page span table keyed by cell, flagged by HYPERLINK.

### Pages: screen and history in one structure

- **Shape.** A page is a fixed block of rows: a power of two near 64 KiB of cells, at least 16 rows. At 120 columns that is 64 rows, or 61 KiB.
  - `cells: [Cell; rows × cols]`
  - `meta: [RowMeta; rows]`, where `RowMeta` is `{occ, fill, lead: u16, wrapped, dirty, simple}`
  - the style table, the grapheme table and the link table
- **The screen is the newest rows.** The page list is a `VecDeque<Page>`, and the screen is its newest `lines` rows.
- **Scrolling is nearly free.** A linefeed at the bottom of a full-screen scroll region appends one row by writing its `RowMeta`. No cell is touched. The oldest screen row becomes history by definition.
  - Scrolling into history costs nothing.
  - Viewing history is reading the same pages, with no decode while they are dense.
- **Eviction and recycling.** Whole pages past the row limit, or (oldest first) while history exceeds its byte budget, are dropped. A packed page's cell buffer is reused for the next tail page, so steady-state output allocates only the packed bytes.
- **Partial scroll regions.** `DECSTBM`, `IL`/`DL` and `RI` inside a region move rows within the screen.
  - This copies cells between row slots and remaps style, grapheme and link ids when the slots are in different pages.
  - It costs about the same as today's `Storage::swap`, and is the only path that needs remapping.

### Row reuse without per-cell resets

- **Lazy clear.** Each row has an occupancy `occ`. Cells at or past `occ` read as blanks of the row's erase `fill` style, and whatever bytes they hold are never read.
- **Writes.**
  - Appending a row sets `occ = 0` and `fill = pen background`.
  - A write past `occ` first blanks the gap, then writes. Writes into fresh cells skip the wide-character checks entirely.
- **Erase.** `EL`/`ED` to the end of a row with the row's own fill only lowers `occ`. A whole-row erase also sets `fill`.
- **Result.** The 120-cell reset per scrolled row disappears. It was about a quarter of the ASCII profile left in the 8-byte-cell experiment.
- **Generation counters were not needed.** `occ` is the generation: one `u16` per row.

### The text path

- **One new hook in the vendored parser.**
  - `Perform::ground_bytes` and `Handler::input_bytes` offer each ground-state slice to the terminal before the parser scans it.
  - The terminal consumes a prefix of printable text and the C0 controls HT, BS, CR, LF, VT and FF, with exactly the effect of the calls the parser would make. It returns the last printed character, so REP stays correct.
  - It stops before ESC, other C0 controls, DEL, C1, or invalid or incomplete UTF-8. The parser handles those exactly as before, including UTF-8 split across reads.
  - The default consumes nothing, so the current core is unaffected.
- **Printable ASCII runs.** The end of a run is found 16 bytes at a time with NEON (with a scalar fallback elsewhere). Each run is written as row segments of `byte | style | old marks`.
- **Validate, decode and measure in one pass.**
  - Characters are decoded from the lead byte with continuation, overlong, surrogate and range checks.
  - The class (two bits per BMP codepoint: general path, one column, or two columns, never joining) comes from one 16 KiB table indexed by the codepoint.
  - That table is built once per process from the same grapheme table #840 uses, and a unit test checks the two agree for every BMP codepoint.
  - This replaces `from_utf8` followed by the second decode, the two-stage lookup, and the join test.
- **CJK, kana and Hangul with NEON.** Runs of three-byte characters are classified 16 at a time:
  - `vld3q_u8` splits lead and continuation bytes.
  - Two `vqtbl4q_u8` lookups read a 1024-bit map of 64-codepoint blocks that are entirely two-column and non-joining.
  - The leading accepted lanes are written as wide-plus-spacer pairs.
  - Overlong forms and surrogates fall in blocks the map rejects, so the vector path also validates.
- **The general path.** Clusters, zero-width marks, emoji, 4-byte characters, wrap-pending, insert mode and overwritten wide characters go through a port of `Term::input` / `continue_cluster` / `widen_cluster` / `narrow_cluster`. It uses the shared grapheme module, so Bun-compatible widths and mode 2027 are unchanged.
- **REP.** `Handler::input_repeat` writes printable ASCII repeats as row segments.

### Compression

- **When a page is packed.** A page whose rows are all older than the newest history row is packed at once. Writes at the top of the screen can still touch that one row, so it stays dense. Packing is deterministic: it happens when the row count crosses a page boundary, never on a timer or at a read boundary.
- **Budget.** History is bounded by live bytes, following #921's semantics:
  - the packed bytes, row metadata and style table of each packed page;
  - plus every dense page that holds a history row, counted at its full size.

  While that total exceeds the budget, the oldest packed page is dropped. The kept history is a function of the output alone, and the differential tests check it is identical for every read split.
- **Packed format**, per row from `lead` to `occ`, as tokens:
  - A row written only by the text paths in its fill style is a text row: a header with `lead`, then one byte per ASCII cell, blank runs, a leading-spacer byte, and a character per glyph. Two-column characters imply their spacer, and U+4E00 onwards takes two bytes instead of three.
  - Any other row uses general tokens: a style change, a blank run, a cluster prefix carrying its codepoints, the same character tokens, and an eight-byte literal for anything else (marks, odd spacers). So packing is lossless.
- **The text log: packing without reading cells back.**
  - While a row is written left to right by the text paths, the writers also append its tokens to a per-page log: the bytes they just wrote, a CJK code, or a blank run for a gap.
  - Any other write invalidates the row's log, and that row is packed from its cells instead.
  - When every written row of a page is logged, in order and contiguously, the log is the packed page and packing copies nothing.
  - Debug builds decode every logged row and compare it with the cells.
  - This cut packing from about 0.07 s to 0.01 s per 64 MiB of ASCII. The remaining cost is appending to the log as text is written.
- **Unpacking.** A packed page is unpacked into a one-page cache on demand, for `Scroll`, find capture or a checkpoint export. The cache is not counted in the budget, and readers release it after a batch.
- **Idle pages on disk.** A packed page is an immutable byte string, so it pages out through the same seam compact history uses today (`grid::cold::ColdStore`, see `docs/idle-history-swap.md`): the Engine's idle ladder hands a quiet session's packed pages to its encrypted swap file, the one-page unpack cache loads a stored page back, and the budget counts a stored page at its packed length so retention never depends on paging.

### Footprint

- Per terminal at 120×80: the screen spans two or three pages (122–184 KiB) against 230 KiB of 24-byte cells today.
- History is bounded at 4 MiB of live bytes, packed. Measured peak RSS for a 64 MiB flood is 11.3 MB (ASCII) and 9.3 MB (Unicode), against 10.9 and 10.8 MB for today's Engine, with process start-up at about 4.2 MB.
- The class tables are 16 KiB per process, not per session.
- The prototype adds no idle work: no timers, no background threads.

## Migrating the consumers

Only `diri-terminal-state` (`HeadlessScreen`, `GridMirror` helpers, `inline_images`) touches `alacritty_terminal` types. Everything downstream reads the wire types in `diri_proto::grid` (`GridCell`, `GridUpdate`, `ChangedRow`, `RowMetadata`, `GridRowCodec`) and `remote_pty` (`FullSnapshot`, `GridDelta`, `GridResume`, `ScrollbackResponse`). The migration therefore converts at that boundary, and the wire stays byte-identical.

| Consumer | Today | With v2 |
|---|---|---|
| **Snapshots and diffs** (`grid_update`, `full_snapshot`) | Walk damaged rows; `wire_cell` maps `c/fg/bg/flags`; annotated rows produce `RowMetadata` | Walk rows with `dirty` set (row bit instead of `TermDamage`); `wire_cell` reads the cell plus the page's style; metadata from the page's grapheme and link tables. Fingerprints and the `last_cells` baseline are unchanged. Gate: snapshots/diffs equal on every existing test and the vt workloads. |
| **Scrollback** (`scrollback`, `scrollback_cells`, `find_capture_cells`) | `grid[Line(-n)]` with compact read caches | The same rows from pages; a cold page is unpacked once per batch. `GridRowCodec` output is identical. |
| **Renderer** (`diri-term`) | Wire `GridCell` only | Unchanged. |
| **Selection, find** | Client-side on wire cells (`diri-term/selection.rs`, `find/*`) | Unchanged. The vendored `selection.rs`, `search.rs` and `vi_mode.rs` are unused by Diri and are not ported. |
| **Reflow** (`grid/resize.rs` + compact deferred reflow) | Row re-wrap with compact hooks | Rewrap page by page into new pages: logical lines are rows joined by `wrapped`. Clusters, links and styles are re-interned. Deferred reflow of cold pages keeps resize O(screen). |
| **Inline images** | Covered cells are blanks whose OSC 8 URI is an `ImageAnchor` | Same anchors in the page link table, so images scroll, erase and evict exactly like text. |
| **Hyperlinks** | `CellExtra.hyperlink` (an `Arc`) | Page link table plus the HYPERLINK bit; `MAX_LINK_URI_BYTES` and metadata budgets unchanged. |
| **Grapheme widths (#840)** | `grapheme::Info`, `Cluster`, `continue_cluster` | Same module (now public in the vendored crate); the fused class table is generated from it, and the general path is a port of the same functions. Differential tests cover clusters, VS15/VS16, ZWJ, flags, keycaps and Indic. |
| **Remote Helper** | `HeadlessScreen` inside `diri-remote` | Same crate and feature switch. The protocol and capabilities are unchanged. The Helper Build ID changes with the binary, as for any parser change; live Holders keep their binary. |
| **Checkpoints** (`history_snapshot`, `history_metadata`, `restore`) | Read and write `Cell`s | Export reads pages. Import writes through the general cell setter. The format is unchanged. |
| **Deterministic budget (#921)** | History bound once per read on live bytes | Retention is counted in whole rows and pages, and sealing is driven by page sequence. The kept history is therefore identical for any read split. Gate: the #914/#921 differential runs (one-shot, byte-by-byte, every split, random reads) include history. |

## Migration plan

Each PR lands on its own, behind the single `core-v2` cargo feature of `diri-terminal-state`, which the Engine and the Remote Helper leave off until the last PR. The current core stays until parity is proven, and is removed in a separate PR afterwards.

1. **This PR (design + prototype).**
   - What lands: the vendored `vte` hooks (`ground_bytes`/`input_bytes`, `input_repeat`; defaults keep today's behavior), the public grapheme module, the `core_v2` prototype, its differential tests, the `HeadlessScreen::from_core_v2` bridge, the bench with the golden check (v2 and, switch off, the Engine on all six workloads), and CI running the switch both ways (arm64 NEON and x86_64 scalar).
2. **Complete grid semantics.**
   - Status: done on `core/v2-step2`.
   - Every `Handler` method that changes the grid or its cursor now has the current core's semantics, ported method by method into `core_v2/ops.rs`:
     - scroll regions, `IL`/`DL`/`SU`/`SD`/`RI`;
     - `ICH`/`DCH`/`ECH`, and the `ED` variants, including "bare `ED 2` scrolls into history";
     - the three alternate-screen modes, DECSC/DECRC, charsets;
     - origin, insert, wrap and newline modes, tab stops, RIS, DECSTR, DECALN, DECCOLM;
     - OSC 8 links (a per-page link table) and OSC 133 marks.
   - The `unsupported` counter never counts any more.
   - Not in this step: handler effects that are events rather than grid state (titles, colors, clipboard, replies, keyboard protocol, cursor style). They reach `HeadlessScreen` through its event collector and come with the adapter in step 4.
   - Gates:
     - per-family differential tests against the current core, at every split and under four history budgets;
     - a random mix of 90 sequence families, run 3,000 times in local runs and 24 times in CI;
     - the adversarial corpus of #927 when present, fed the same reads to both cores.
3. **Link table and the Unicode gap.** The grapheme arena has landed in the prototype. Add the link table, and close the Unicode gap: history-on Unicode is 0.46× the reference.
   - Status: closed (#980). Core-only, checked against the golden and cell for cell, history at 4 MiB, cycles per byte, v2 vs the reference:

     | workload | v2 | reference |
     |---|---:|---:|
     | Unicode | 4.02 | 4.21 |
     | ASCII | 6.09 | 6.40 |
     | CSI | 14.1 | 19.5 |

     - v2 runs are 7 interleaved rounds. Reference runs are 9 runs of `+terminal-stream`, cycles minus the median of the warm empty-file runs, at load 9–13.
     - The last 1.5% on Unicode came from CJK prose: punctuation every few ideographs sent most 16-character runs to the mixed token path.
     - History tokens now give two-byte codes to 100 pages through a page table: U+3000..U+33FF, U+4E00..U+9FFF, U+AC00..U+B8FF and U+FF00..U+FFFF, where they had covered U+4E00..U+B1FF.
     - That gave 6% fewer Unicode instructions, and kitten Unicode keeps 61,872 rows (the current core keeps 53,648).
4. **Boundary adapter.** Split into three PRs.
   - **4a, the event layer.** Status: done on `core/v2-step4`.
     - Ported: titles and the title stack, OSC 4/10/11/12 colors and their resets, OSC 52 store and load, every reply (DA1, DA2, DSR, DECRQM, XTWINOPS, XTVERSION, XTGETTCAP, DECRQSS), the kitty keyboard stack (swapped with the screens), the cursor style, the bell, and the events modes send.
     - They reach the same `EventListener` as the current core.
     - A differential test compares the event sequence, after applying reply closures to fixed inputs, plus colors, the cursor style and every mode bit, in one read, byte by byte and in 7-byte reads.
   - **4b, a pure refactor.** `HeadlessScreen` reads its emulator through one internal interface (cells, cursor, modes, damage, history management, checkpoint writes) implemented by the current core. No behavior change, and every existing test is the gate.
   - **4c, the second implementation.** The `core-v2` switch implements that interface with v2. That needs damage tracking (rows written or scrolled since the last frame, and the content sources `grid_update` reuses) and checkpoint writes.
     - The whole `diri-terminal-state` suite runs both ways in CI.
     - The wire output must be byte-identical on the vt workloads and the golden digests, and the diri-term screenshots must be pixel-identical.
     - Resize is the one gap until step 5: with the switch on it must refuse rather than guess.
     - Status: implemented on `core/v2-step4c` behind its own switch, `core-v2-screen` (it implies `core-v2`; nothing enables it). `core_v2/screen.rs` is the adapter:
       - The interface reads rows and cells through views (`Emulator::RowRef`, `CellRef`) and writes through `edit_row`. For the current core they are its own references and indexing, so that build is unchanged.
       - Rows and cells are read as current-core cells, converted from pages, so every encoder runs unchanged.
       - Damage is the rows with `RowMeta::dirty` plus the cursor rows; it is full when rows moved (a row appended, history cleared, a screen swap). No content sources: moved rows are hashed again.
       - Checkpoint writes convert back to page cells. A packed history page is thawed for the edit and packed again before rows move or history is bounded.
       - Resize to another size panics ("resize needs reflow, step 5"); the tests that resize are ignored with the switch on.
       - Rows kept at the budget: see step 6 (v2 now keeps at least the current core's rows).
5. **Reflow.** Page rewrap plus deferred cold reflow. Run the resize tests, and the #921 narrowing test on both cores.
   - Status: rewrap done on `core/v2-step5`; deferred cold reflow is not.
     - `core_v2/resize.rs` is a port of `Term::resize` and `Grid::resize`. Each screen's rows stream oldest first through the current core's algorithm, as current-core rows, so `Row::shrink`, `is_clear` and `Cell::is_empty` are the vendored functions. The rows are written into new pages of the new size.
     - Packed history is unpacked a page at a time by the read cache, and the new pages pack as they fill. A resize keeps only the screen and a few rows decoded, but it costs O(history): every resize rewrites all of it.
     - What it covers:
       - lines first: growing pulls rows back from history, shrinking pushes rows into history under the row limit;
       - then columns: reflow on the primary screen and none on the alternate screen, as the current core does;
       - wide characters, leading spacers, clusters, links, marks and styled blanks;
       - the cursor, pending wrap and saved cursor of both screens; tab stops and the scroll region;
       - the history origin (`Pages::history_origin`). Like the current core's, it does not count rows that a narrowing re-wraps past the row limit, or rows of a single scroll larger than the limit. The second rule also fixes 4c's origin outside resize.
     - Gates:
       - `core_v2::tests`: a deterministic resize suite, plus 48 + 24 random runs (3,000 each locally with `CORE_V2_FUZZ`). Each run interleaves random output with eight random resizes. v2 is fed whole, byte by byte, in random reads, and on packed four-row pages; the current core gets the same bytes.
       - The comparison covers cells, cursor, history and origin after every step, and the saved cursors, the other screen, tab stops and the scroll region through probes at the end.
       - Every `HeadlessScreen` resize test runs with `core-v2-screen` on, and so does diri-term's suite.
     - **One deliberate difference.** At one column, the current core never finishes narrowing a wide character: each round pushes another row. v2 leaves the character in the column and wraps its spacer to the next row. The differential tests use at least two columns whenever wide characters are present.
     - The tests step 5 left gated (`a_raised_history_limit_follows_history_back_down` and the four depth tests, the cold-paging test, moved-row fingerprints) run with the switch on since step 6.
6. **History parity** (`core/v2-history`). The rule: v2 keeps at least as many history rows as the current core in the same 4 MiB of live bytes.
   - **LZ under budget pressure.** Sealed pages are packed as tokens. When history passes its budget, the oldest pages not yet considered are compressed with the current core's LZ codec (`grid::compact::lz`), before any page is dropped. An `LzPolicy` compresses every page while pages shrink by a third, and otherwise probes a 1 KiB sample of every 8th page. Text that barely repeats (the kitten ASCII and Unicode captures) therefore costs about 1% more instructions, not a full LZ pass. What is compressed depends only on the sequence of sealed pages, so the history kept is the same for any read split, and with or without paging.
   - **Packed layout.** A packed page is its row metadata, then the style ids of its style changes as their own stream, then the row tokens, then its style table. A page keeps no table outside its bytes, so a styled page costs what its text and styles compress to. Ids and characters repeat at different rates (a colour per cell, text shifting between rows); kept apart, both repeat, and LZ finds them. A capture coloured cell by cell keeps 119,921 rows on v2 against 106,769 on the current core.
   - **Cold paging (#958).** Packed pages older than the newest `keep_rows` history rows move to the `ColdStore` (`Pages::page_out`, through `Emulator::page_out_history`). A stored page counts its full length against the budget, reads load it into the one-page cache, a store failure reads as blank rows, and a page is released with its screen.
   - **Moved-row fingerprints.** `Emulator::moved_line_sources` reports, for each screen line, the line its cells came from when rows only moved by appends. Dirty and new rows report `CONTENT_CHANGED`. `fingerprints_after_scrolling_match_the_cells` now runs on v2 and checks this is exact.
   - **Golden on v2.** `vt_workloads` with `core-v2-screen` requires at least the golden's rows and compares the newest of them exactly. `core_v2_bench` compares every row v2 keeps, cell for cell.
7. **Inline images and checkpoints** on v2, with the existing image and checkpoint tests.
   - Engine-path parity with the current core (#978 and #982). These read paths had cost v2 more than the current core:
     - Settling a read that changed nothing no longer hashes the cursor rows. Every v2 write marks its row dirty; the cursor rows are reported only after a recolor.
     - With nothing moved, the dirty scan walks the screen pages' metadata slices, and the moved-line sources are a kept identity.
     - Image deletes read link targets through `Emulator::each_link_target` instead of converting every screen row.
   - Result, `vt_workloads` golden-checked, 5 interleaved rounds at load about 9, median MB/s:

     | workload | v2 | current core |
     |---|---:|---:|
     | images | 6,181 | 6,089 |
     | long escape codes | 26,123 | 25,706 |

     Instructions per 100 feeds: images 1.343G vs 1.351G, long escapes 0.522G vs 0.528G.
7b. **The Engine and the Remote Helper on v2.** Status: done on `core/v2-helper`.
   - With `diri-terminal-state/core-v2-screen` on, the Remote Helper's whole suite passes (`diri-remote`), and so does the Engine's: attach, mirrors, checkpoints and sessions.
   - The one exception is paging idle history out to the cold store (#958), which is step 6.
   - CI's Linux x86_64 job runs both suites with the switch on.
8. **Switch.** Enable `core-v2` for the Engine and the Remote Helper after a soak on the nightly channel. Holder→Engine vtbench before/after; `PERF.md` gates unchanged or better.
9. **Remove** the old grid, compact history and the unused vendored selection/search/vi-mode code.

## Risks

- **Reflow** is the largest semantic surface: clusters and leading spacers at row ends, and wrapped wide characters. It gets its own differential suite in step 5 before anything ships.
- **Per-page style ids** make cross-page row moves remap ids. They are rare: scroll regions only.
- **NEON is aarch64-only.** x86_64 Linux Helpers use the scalar fused path until an SSE4.1/AVX2 version lands with the same tests. The scalar path already removes the double decode.
- **The text log adds writer state per hot page** (a token arena of a few KiB). A row rewritten in place forever cannot grow it without bound: past 1 KiB per row, the page drops its logs and packs from cells.
