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
history.

**How it was measured:**

- Machine: Apple M4 Max with 14 cores. Load average was 26–34 from other agents.
- Payloads: the kitten `__benchmark__` captures (`ascii.bin`, `unicode.bin`, `csi.bin`), repeated to at least 64 MiB, the same repetitions as `vt_workloads`.
- Reads: 64 KiB.
- Screen: 120×80.
- Rounds: 7. Each round runs v2, then current-dense, then current-compact.
- Figures: medians, with min..max in brackets. Base is `3fc4feb66`.

**Every timed v2 run passes two checks**, or it reports no number:

1. **The golden digests** of `vt_workloads.golden` (branch `test/vt-bench-golden`).
   - These are digests of a known-good byte-at-a-time parse at `3fc4feb66`: bytes fed, the full snapshot, the newest 62,401 / 53,361 / 0 history rows as scrollback text, cells and metadata, links, and the cursor.
   - They are computed on the v2 state through `HeadlessScreen::from_core_v2` with the same encoders the bench uses.
2. **A cell-for-cell comparison** with a byte-at-a-time parse by the current core.
   - It covers the screen and all retained history: character, cluster codepoints, width flags, wrap, style flags, foreground, background, underline color, cursor and pending wrap.
   - It also asserts that every payload byte was fed.

Both checks were mutation-tested: one flipped bit in the prototype's writes fails them.

```sh
cargo run --release -p diri-terminal-state --features core-v2 --example core_v2_bench -- <kitten dir>
CORE_V2_GOLDEN=<vt_workloads.golden> CORE_V2_HISTORY=64000 cargo run ...   # as measured
CORE_V2_HISTORY=0 cargo run ...                                             # history off
```

All figures are MB/s.

**History on: 64,000 rows retained by every core.** That is about what the Engine's 4 MiB compact history keeps of this output, and enough for the golden check.

| Workload | v2 | Current, dense history | Current, compact history (Engine) | Reference terminal |
|---|---:|---:|---:|---:|
| ASCII | **758** (708..800) | 186 (131..208) | 130 (129..132) | 620 (545..641) |
| Unicode | **626** (615..645) | 156 (155..158) | 125 (124..128) | 928 (855..945) |
| CSI | **289** (284..291) | 106 (104..107) | 102 (101..103) | 203 (169..208) |

**History off.**

| Workload | v2 | Current, dense | Current, compact |
|---|---:|---:|---:|
| ASCII | 820 (736..833) | 403 (400..406) | 387 (381..392) |
| Unicode | 644 (641..679) | 227 (224..233) | 224 (220..225) |
| CSI | 291 (285..295) | 106 (103..108) | 104 (100..106) |

**Reading the tables honestly:**

- **The reference ran differently.**
  - It ran as its own benchmark binary, `+terminal-stream`, on the same 64 MiB files in 64 KiB reads, at commit `6ba1441`.
  - Each figure is the median of 7 runs. Process start-up was measured with an empty file (10 ms) and subtracted.
  - Its scrollback is its benchmark default of 10 KB, about one page. v2 keeps 64,000 rows in the "history on" table, so v2 does more history work, not less.
  - It runs a complete terminal, while v2 handles only the sequences above. On these three payloads that is everything they contain: the prototype counts any sequence it does not implement, and the checks would fail.
- **Unicode is still behind.** v2 reaches 0.67× the reference on Unicode. It is ahead on ASCII (1.22×) and CSI (1.42×).
- **Where the Unicode time goes:**
  - About a third of v2's Unicode time is the general cluster path for combining marks and emoji, which are about 2% of the characters.
  - Most of that cost is the per-page grapheme `HashMap`.
  - The fix is the grapheme arena described below.
- **v2 history is dense.** 64,000 rows at 120 columns are about 62 MB of pages. The Engine's compact history holds the same rows in 4 MiB.
  - Engine parity therefore needs cold-page packing ("Compression" below). That work is not in these figures.
  - The "history on" column is the hot path plus page history, not the Engine configuration.
- **Part of the CSI gain is not v2-specific.** Part of it is the new `Handler::input_repeat` hook: REP writes a row segment instead of 100 single-character calls. The current core could adopt that hook too.
- **Images and long escapes were not measured.** Those workloads are escape-body bound, which #922 addresses. v2 does not change them.

## Design

### Cell: 8 bytes

```
bits  0..21  codepoint (0 = never written; reads as a space)
bits 21..26  WIDE, SPACER, LEADING_SPACER, GRAPHEME, HYPERLINK
bits 26..31  shell marks (OSC 133 A, B, C, D-ok, D-failed)
bits 32..48  style id in the owning page's style table (0 = default)
bits 48..64  reserved: image placement / extra id
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
  - The extra codepoints of a cluster live in a per-page table keyed by cell index, valid while the cell's GRAPHEME bit is set.
  - Overwriting a cell clears the bit. The stale entry is replaced the next time that cell becomes a cluster.
  - The prototype uses a `HashMap<u32, Vec<char>>`. The production version is an arena: `u32` offsets into a per-page `Vec<char>`, compacted with the page. This removes the allocation and hashing that dominate the remaining Unicode profile.
- **Hyperlinks** (OSC 8, and inline-image anchors, which are hyperlinks) use the same shape: a per-page span table keyed by cell, flagged by HYPERLINK.

### Pages: screen and history in one structure

- **Shape.** A page is a fixed block of rows: a power of two near 64 KiB of cells, at least 16 rows. At 120 columns that is 64 rows, or 61 KiB.
  - `cells: [Cell; rows × cols]`
  - `meta: [RowMeta; rows]`, where `RowMeta` is `{occ: u16, fill: u16, wrapped, dirty}`
  - the style table, the grapheme table and the link table
- **The screen is the newest rows.** The page list is a `VecDeque<Page>`, and the screen is its newest `lines` rows.
- **Scrolling is nearly free.** A linefeed at the bottom of a full-screen scroll region appends one row by writing its `RowMeta`. No cell is touched. The oldest screen row becomes history by definition.
  - Scrolling into history costs nothing.
  - Viewing history is reading the same pages, with no decode while they are dense.
- **Eviction and recycling.** Whole pages older than the history limit are recycled as the next tail page, keeping one spare. Steady-state output allocates nothing.
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

- **Pages are the unit, and compressing them is optional.**
  - The hot path never compresses.
  - A page that falls more than K pages behind the screen becomes cold and may be packed. Its codepoints become a byte stream when the page holds only ASCII, and style ids are run-length encoded.
- **Sealing is deterministic.** It is triggered by page sequence, not by timers or read boundaries.
- **Unpacking.** A cold page is unpacked into a scratch page on demand, for `Scroll`, find capture or a checkpoint export.
- **What remains to measure.** Compressing a page of 8-byte cells is one pass over 61 KiB, against today's row-by-row encode of 24-byte cells. Its cost per byte of output still has to be measured before the Engine switches. The budget rule must count only live bytes (#921).

### Footprint

- Per terminal at 120×80: the screen spans two or three pages (122–184 KiB) against 230 KiB of 24-byte cells today.
- History is 61 KiB per 64 rows dense (64,000 rows ≈ 62 MB), or less when packed. Packing is required before the Engine switches.
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
   - What lands: the vendored `vte` hooks (`ground_bytes`/`input_bytes`, `input_repeat`; defaults keep today's behavior), the public grapheme module, the `core_v2` prototype, its differential tests, the `HeadlessScreen::from_core_v2` bridge and the bench with the golden check.
2. **Complete grid semantics.**
   - Implement every `Handler` method with the current semantics: scroll regions, `IL`/`DL`/`ICH`/`DCH`/`ECH`, `ED` variants (including Diri's "bare `ED 2` scrolls into history"), save/restore, charsets, origin mode, insert mode, alternate screen, tabs, resets, OSC 8 links and OSC 133 marks.
   - The `unsupported` counter must reach zero on the whole terminal-state test corpus. Each sequence family gets a differential test against the current core.
3. **Grapheme arena and link table.** Replace the per-page `HashMap`s. Re-measure Unicode (target: at least 0.85× the reference).
4. **Boundary adapter.**
   - Implement `HeadlessScreen` over v2 (`core-v2` feature).
   - Run the whole `diri-terminal-state` test suite and benches on both cores in CI.
   - Snapshot/diff/scrollback wire output must be byte-identical on the vt workloads and the parity bench.
5. **Reflow.** Page rewrap plus deferred cold reflow. Run the resize tests, and the #921 narrowing test on both cores.
6. **Cold-page packing and the budget.**
   - Measure the packing cost per byte. Gate: the Engine-config throughput, with packing inside the timer, beats current compact by at least 3×.
   - Prove deterministic retention under the #921 differential test. Show retained rows at 4 MiB are at least today's.
7. **Inline images and checkpoints** on v2, with the existing image and checkpoint tests.
8. **Switch.** Enable `core-v2` for the Engine and the Remote Helper after a soak on the nightly channel. Holder→Engine vtbench before/after; `PERF.md` gates unchanged or better.
9. **Remove** the old grid, compact history and the unused vendored selection/search/vi-mode code.

## Risks

- **Reflow** is the largest semantic surface: clusters and leading spacers at row ends, and wrapped wide characters. It gets its own differential suite in step 5 before anything ships.
- **Per-page style ids** make cross-page row moves remap ids. They are rare: scroll regions only.
- **NEON is aarch64-only.** x86_64 Linux Helpers use the scalar fused path until an SSE4.1/AVX2 version lands with the same tests. The scalar path already removes the double decode.
- **Dense history costs memory until packing lands.** That is why the switch is gated on step 6.
