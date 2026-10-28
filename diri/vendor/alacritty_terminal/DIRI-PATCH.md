# Diri terminal extensions

Pinned alacritty_terminal 0.26.0 (Apache-2.0). The terminal metadata extension adds a
PROMPT_START cell flag and Handler::mark_prompt implementation, paired with the
OSC 133 A dispatch in vendored VTE. Markers follow the existing grid erase,
scroll, and reflow lifecycle and are ignored on the alternate screen.

This keeps prompt navigation in the authoritative parser, including synchronized
updates, instead of adding a second escape-sequence parser or guessing boundaries
from terminal text. No new runtime dependency. The source participates in the
Remote Helper Build ID. Revisit this small patch when updating the parser.

## Shell command marks

`Flags` widens from u16 to u32 for four more cell flags set by
`Handler::mark_shell_command`: `INPUT_START` (OSC 133 B), `OUTPUT_START` (C),
and `COMMAND_SUCCEEDED` / `COMMAND_FAILED` (D; both together when the status is
unknown, and a later D on the same cell replaces the earlier one). Like
`PROMPT_START` they mark the cursor cell, are ignored on the alternate screen,
survive overwriting by printed characters (`Flags::SHELL_MARKS`), and follow
erase, scroll and reflow. `Cell` stays 24 bytes (`cell_size_is_below_cap`). The
compact history codec writes style flags as a varint, so ordinary styles still
take one byte. The codec is process-local, so no stored format changes.

## Deferred alternate-screen allocation

The pristine alternate grid is represented by `None` until the first screen
switch. It is created at the current dimensions, then uses the original cursor,
clear, swap, resize, and reset operations. Once created, its allocation is retained
for reuse. The active primary grid and history remain eager and authoritative.

This removes one unused 80×24 grid (46,848 requested heap bytes) from terminals
that have never entered a full-screen program. The tradeoff is allocating that
grid at first entry. There is no wire or checkpoint format change. Parser source
already participates in the Remote Helper Build ID; live Holders keep their binary.

The resource gate and actual-parser screen-switch/resize/reset regressions live
in `diri-terminal-state`; run `cargo bench -p diri-terminal-state --bench
terminal_parity -- --empty-gate` from the workspace for the allocation gate.

## Spare history-row allocation

Storage still grows in batches and reuses cleared rows. Its batch and shrink
cache limit now scales with row byte size: at most 1,000 rows and about 64 KiB
of newly allocated rows, with a minimum of one row. A request for more live rows
is always satisfied. The reserve includes cell and row-descriptor sizes; existing
Vec capacity and rows retained through reflow are not a total-memory guarantee.

Previously, the first history row eagerly allocated 1,000 rows, even at wide
terminal dimensions. The change preserves ring indexing, row identity, history
limits, resize/reflow and serialized formats. It trades smaller growth batches
for lower retained heap. The short-history resource gate covers both sides of
the first-scroll boundary; upstream storage tests cover indexing and rotation.

The opt-in enhanced keyboard parser also keeps direct CSI = mode changes in
the active stack entry. Queries, push/pop, and alternate-screen transitions
therefore observe the same flags. Its bounded overflow evicts keyboard entries
without touching window titles. Diri does not enable negotiation by default.

## Compact history codec

Cold history blocks (the `compact-history` feature used by the Engine and the
Remote Helper) are encoded as a hand-written binary layout and compressed with
a small LZ77 block codec in `grid/compact/lz.rs`. They were previously
`serde_json` values compressed with DEFLATE (`flate2`), which dominated the
cost of streaming output: every line that scrolls into history is encoded once.

Per block: a style table (cells without their character), then per row the
occupancy, width, stored-cell count, the UTF-8 text of the stored cells and
style runs. The suffix of cells equal to `Cell::default()` is implied by the
width. The resize floor is computed during the same pass. The codec is
lossless, process-local and only ever decodes its own output; corruption still
panics, as before. `flate2` and `serde_json` are no longer dependencies of the
feature; no new dependency is added.

Tradeoff: LZ without entropy coding stores more bytes than DEFLATE. On 10,000
rows of 160 columns this measured 54.5 vs 53.0 bytes per row for `git log -p`
output and 41.8 vs 35.4 for a synthetic colored build log. The 4 MiB history
budget is unchanged and measured the same way, so output that is limited by
that budget (not by the 10,000-row limit) can retain fewer rows than before.

## Recycled history rows

When history is full, scrolling moves the oldest row to the bottom of the
screen and resets it. Compact storage used to decode the whole oldest block to
recycle one row. `Grid::scroll_up` now calls `Storage::rotate_reset` when every
rotated row lies inside the region it resets afterwards. Compact storage then
builds each recycled row in its reset state directly from the encoded row:
`Row::reset` only keeps cells above the row's occupancy, and only when the
last cell's background matches the template. Other rows of the block stay
encoded; the oldest block may be partial. Dropping single rows for the byte
budget or truncation likewise no longer decodes. The grid's own reset of those
rows is then a no-op. Dense storage keeps plain rotation.

A decoded block used to count at full cell size toward the history budget
until its rows were recycled. The partially consumed block now counts its
compressed bytes plus one decompressed index.

## Recycling from recipe bits and spare rows

Encoding a block also records, per row, whether `Row::reset` of that row
yields only template cells for every template: the row has the block width,
is not empty, and every cell at or beyond its occupancy is `Cell::default()`
(then a differing template background resets every cell, and an equal one
means the default background, whose kept cells already are reset cells).
Nearly every output line qualifies. Recycling such an oldest row drops it
without decompressing or indexing its block; rows that do not qualify, or
blocks at another width after a resize, take the indexed path above. The 64
recipe bits fit in `Block` without changing its counted size (`start` is now
a `u32`), so the history budget retains exactly the same rows.

The rows a seal has just encoded are kept as spare allocations for the rows
that recycling hands back, so a scroll no longer allocates and frees a row.
A spare row whose reset would keep non-template cells is marked fully
occupied first; the others reset only the cells they wrote. Spares are
released at every `bound_history_bytes` (each `HeadlessScreen` settle),
before stored bytes are measured, so they neither count toward nor outlive
the history budget, and an idle terminal retains none.

`bound_history_bytes` runs after every read that scrolled. It no longer visits
every block: the payload term of `history_storage_bytes` is kept current by
the block pushes and pops at the ends that streaming output uses and is
recomputed from scratch after any other block change (debug builds assert it
equals the recomputation), and read caches are only released when a history
row was read or edited since the last release.

## Line content sources

`Term::damage_content_sources` reports, for full damage, where each screen
line's cells were at the last `reset_damage`: scrolling (`scroll_up_relative`
and `scroll_down_relative`, which cover linefeed, reverse index, SU/SD and
IL/DL) moves the sources with the lines and marks uncovered lines changed,
instead of forgetting them. Any line damage, and every other full damage,
still marks lines changed or the sources unknown. `input` leaves its cells to
cursor damage, so a scroll marks the cursor line first, and full damage marks
the previous and current cursor lines like the partial damage query does.
Clearing a wide character's leading spacer on the line above the cursor was
not damaged at all; it now marks that line changed (renderer damage is
unchanged). `diri-terminal-state` uses the sources to keep moved rows'
fingerprints; rendering is unaffected.

## Printable ASCII input

`Term::input_ascii` (the vendored VTE's `Handler::input_ascii`) writes a run of
printable ASCII a row segment at a time. It is exactly `input` per character:
the segment stops at a wide character or spacer, and wrapping, insert mode and
a pending wrap fall back to `input`. Charset mapping, prompt marks, template
colors, flags and hyperlinks, occupancy and the final cursor/wrap state match.
Visible-row indexing also has an inlined fast path.

Tests: `ascii_runs_match_per_character_input` (randomized against `input`),
`recycled_rows_equal_decoded_rows_after_reset` (also checks the recipe bits
and spare-row resets), `streaming_full_history_recycles_without_decoding`,
`recycle_bits_keep_the_counted_block_size`,
`random_storage_operations_match_an_uncompressed_sequence` (now with
recycling and byte bounds), `scrolling_moves_content_sources`,
`full_history_recycling_matches_dense_storage`,
`every_cell_attribute_round_trips_through_the_codec` and
`history_compression_round_trips_arbitrary_bytes`. There is no CI job for this
excluded crate; run them from this directory with the vendored VTE:

```sh
cargo --config 'patch.crates-io.vte.path="../vte"' test --features compact-history
```

(Remove the generated `Cargo.lock` afterwards.) While testing this patch, dense
and compact storage were found to diverge after resizing a full history; that
difference exists on the base revision too and is not changed here.

## Deferred column changes

A column change decoded, reflowed and re-encoded all compact history whenever
any history row soft-wrapped: 8 ms per change for 10,000 rows at 160 columns,
so a window drag kept a core busy. (Hard lines that fit the new width already
kept their payloads.) History reflow never moves cells across a hard line
break, and history rows are never the cursor's, so each logical history line
reflows on its own. `CompactRows::prepare_reflow` now keeps older lines back
("deferred history", between the blocks and `oldest`) and hands only the
editable rows (the screen, the newest history and any line ending there) to
`Grid::resize`'s reflow loops.

- A **greedy** line is one whose eager reflow always yields its content laid
  out greedily at the new width: rows break where the next cell or wide
  character does not fit, a wide character that would start in the last
  column leaves a leading spacer, and the last row is padded with default
  cells. That holds, whatever widths the line goes through, when its wide
  characters are whole, a leading spacer only ends a wrapping row and is
  followed by a wide character, every cell after the content and above each
  row's occupancy is a default cell, and no continuation row is blank. Such a
  line is 8 bytes (source rows, content cells), plus skipped cells and
  wide-character runs when it has them. Its rows at any width are counted
  from those without decoding, and built from the source payloads when read.
- Any other line (styled blanks after the content, tabs, broken wide
  characters) is kept as its rows and reflowed at each column change by
  `Grid::resize` itself, on a one-line grid, so the result is upstream's.
  Beyond 256 KiB of such rows the deferred history is reflowed and stored.
- A soft wrap that is not a row's last cell (it can join lines), a
  `display_offset` inside deferred history, a width below 2 or a resize
  without reflow reflow everything eagerly, as before.
- Reading deferred history builds every row at the current width once, and the
  next read-cache release stores them as blocks: a history that is read is
  reflowed once, not per read. Editing a deferred row does the same. A caller
  about to read every history row in order (`Grid::prepare_history_read`: a
  screen checkpoint, a link scan) has the deferred history reflowed straight
  into blocks one block at a time instead, so the read never holds every row
  decoded at once (58 MB → 1 MB at 10,000 rows of 97 columns). Dropping
  the oldest rows (history limit, byte budget) skips content cells without
  decoding. Recycling a greedy line's oldest row at full history needs no
  payload either: it resets to template cells, as a row with recipe bits does.
- Rows kept back count towards the "viewport filled" and cursor pull-down
  steps of growing, which take them (newest first, already at the new width)
  when the reflowed rows are too few. This also fixes the hard-line case:
  growing used to append blank rows and move the cursor up when merged screen
  rows outnumbered the editable history rows, where dense storage brings
  history rows down into the screen.

Tradeoffs: a deferred history keeps its 8-byte line index (80 KB per 10,000
lines; about 110 KB measured with an ASCII log, 190 KB with every third line
holding wide characters) until it is read or edited. Its stored bytes, the
index and eager rows count towards the history byte budget, so a history
limited by that budget (not by its row limit) can retain different rows after
a resize than before; a row-limited history retains exactly the same rows.
There is no wire, checkpoint or protocol change.

Tests: `deferred_reflow_matches_dense_through_random_resizes` (random output
and resize sequences, compared row by row with dense storage; odd seeds keep
every line greedy, `DIRI_REFLOW_SEEDS=<n>` fuzzes more),
`deferred_reflow_matches_eager_compact_reflow_through_random_resizes` (the
same against compact storage that reflows eagerly, including scrolling with a
coloured template: dense storage recycles rows beyond its length, whose cells
such a reset can keep, so it is no oracle there),
`greedy_lines_stay_greedy_through_any_widths` (arbitrary cells; every scan also
checks each greedy line against `Grid::resize`),
`grow_pulls_kept_history_into_the_viewport_like_dense_storage` and
`column_changes_after_the_first_decode_only_new_history`.

## Cell size request

`Term` implements `Handler::cell_size_pixels` (`CSI 16 t`, see the VTE patch)
by sending `Event::CellSizeRequest` with a formatter for `CSI 6 ; height ;
width t`. It is a separate event from `TextAreaSizeRequest` so a host that
does not know its cell size (the Remote Helper, a terminal without inline
images) can leave the query unanswered exactly as before. No wire, checkpoint
or protocol change.

## Grapheme cluster widths

`Term::input` measures grapheme clusters the way Claude Code does. Claude Code
is a Bun binary and lays out its screen with `Bun.stringWidth`; any column we
disagree on shifts its relative cursor moves and leaves fragments of earlier
frames on screen (the spacing-mark table this replaces fixed one case of it).

- `term/grapheme_table.rs` is Bun's per-codepoint classification (grapheme
  break class with Indic conjunct classes, width class, Emoji property),
  regenerated by `diri/scripts/gen-grapheme-width-table.mjs` from Bun's
  `stringWidthTables.h` for the Bun release in the current Claude Code
  (1.4.4, Unicode 17). Two-stage lookup, 37 KiB of static data, O(1).
- `term/grapheme.rs` ports Bun's UAX #29 break rules (GB6-GB13 including GB9c)
  and its cluster width rule: regional indicator pair 2, keycap 2, emoji with
  ZWJ or skin tone 2, VS16 on an Emoji base 2, otherwise the sum of the
  codepoint widths.
- A codepoint that cannot start a cluster on its own reads the cluster back
  from the cell before the cursor (no state outlives a cursor move). When no
  boundary separates them it joins that cell's zerowidth list and the cell
  widens (with a spacer, wrapping like a wide character at the last column)
  or, under 2027, narrows. A joined codepoint with columns of its own outside
  an emoji rule keeps its own cells, which is the sum rule. Joins stop at 32
  codepoints per cell.
- DECSET/DECRST 2027 (`TermMode::GRAPHEME_CLUSTERING`) switches to the
  clustering rule: first-codepoint width, VS16 widens and VS15 narrows an
  emoji. DECRQM reports 1/2; RIS clears it.

A sweep of 100,564 strings (every assigned codepoint after `a`, emoji,
VS15/VS16, keycaps, skin tones, ZWJ sequences, flags, CJK, Hangul jamo,
combining marks, Indic) matches Bun 1.4.4 exactly
(`cargo run --release -p diri-terminal-state --example width_sweep`). No wire,
checkpoint or protocol change; the parser source is part of the Remote Helper
Build ID. Tests: `grapheme::tests`, and in `diri-terminal-state`
`grapheme_clusters_take_the_columns_claude_code_gives_them` and its neighbours.

## History limit without side effects

`Term::set_scrolling_history` changes only the primary screen's history row
limit, which `set_options` also does, but without its full damage and title
event, and `Term::primary_history_size` reads the primary history while the
alternate screen is shown. `diri-terminal-state` uses both around every resize
so that re-wrapping retained history into more rows never evicts it: the
reflow runs under a larger allowance and the limit then holds the rows it
produced. Both are O(1) unless the limit drops below the rows held. No wire,
checkpoint or protocol change; the parser source is part of the Remote Helper
Build ID. Test: `a_resize_at_capacity_keeps_every_retained_line`.

## Byte-bounded history depth

With compact history the configured `scrolling_history` is only a ceiling.
`Grid::bound_history_storage(budget, ceiling)` lowers the row limit to the rows
retained once the byte budget evicts, so new lines recycle the oldest row as at
a fixed cap, and restores the ceiling when stored history falls 1/16 below the
budget. `Term::resize` restores the ceiling before reflow so rows are never
dropped by count that the byte budget would keep. Growing history reuses the
cells of the newest sealed block (`RowCodec::clear_row`). Column changes keep
stored rows when only the oldest block is partial, and the deferred line index
reserves exact capacity and tracks its extras' heap incrementally (the history
bound measures storage after every eviction).

## History kept is independent of read boundaries

The history budget used to measure allocation capacities, the rows not yet
sealed into a block as they stood when the bound ran, and the oldest block's
decode index; and `Grid::bound_history_storage` lowered the row limit to the
rows kept after an eviction until storage fell 1/16 below the budget. All of
these depend on when the bound runs, which is once per read, so how much
history a terminal kept depended on how its output was split into reads (a
64 KiB-read parse and a byte-at-a-time parse of the same 64 MB differed by
hundreds of rows). Capacities also never shrank as narrowed deferred lines were
evicted, so narrowing deep wide-character history evicted most of it.

`CompactRows::history_storage_bytes` now counts live bytes as a function of the
history alone: index lengths instead of capacities, the unsealed rows as the
full block they grow into between seals (once history exists), and not the
decode index (a cache that exists only while full history recycles rows). The
bound shrinks the block and deferred-line indexes when their capacity exceeds
twice their length plus 64, so uncounted slack stays bounded. The row limit
stays at the ceiling. The kept rows are then the newest that fit, whenever the
bound runs. Tests: `bounded_history_does_not_depend_on_read_boundaries`, and
`narrowing_deep_wide_character_history_keeps_it` in `diri-terminal-state`.

## Cheaper history encoding

History encoding was half of the session thread while output streamed. Four
changes, measured on a 100 MB mixed log at 160 columns:

- **Wide characters as pairs.** A run of wide characters alternated with their
  spacers, so every CJK cell was its own run and its own style lookup. A run
  whose code has the low bit set ("paired") now alternates wide characters of
  its style with the exact spacers `Term::input` writes after them (same
  colors, wide flag swapped for the spacer flag, a blank character, no extra
  storage); each pair takes one character of text. Anything else (a spacer
  with its own style or character, a lone spacer, a wide character with marks)
  is encoded as before.
- **Stored cells bounded by occupancy.** Finding a row's last non-default
  cell scanned every blank column of a short line in a wide row. Every cell at
  or after `Row::occ` equals the last cell, so `Row::content_len` starts at
  `occ` when that cell and the one at `occ` are default cells. `Row::grow`
  now counts the old columns as occupied when the last cell is not a default
  cell (blanks a coloured template left), which keeps that rule true after a
  widening; before, `Row::reset` left those coloured blanks behind on a
  recycled line. Debug builds assert the rule wherever it is used.
- **Style lookups in one probe.** A 32-slot direct-mapped cache sits in front
  of the block's style table (a colored log cycles through more than the 16
  styles the linear scan held), and styles with extra storage are found by
  hash and comparison without cloning the cell.
- **Fewer passes and allocations.** Runs and text are found in one walk per
  row, the runs are written as they end (they stop where they cover the
  stored cells, so no run count), and the style table follows the rows behind
  a `u32` body length instead of being copied in front of them. The grapheme
  break helpers that `Term::input` replays per joining codepoint are inlined.

Payloads never leave the process, so the layout change needs no version. The
layout is documented above `FxHasher` in `grid/compact.rs`.

The spare rows a seal leaves for recycling are still released at every history
bound by default. An owner that bounds history after every small read can keep
them (`Grid::keep_history_spare_rows`) and release them once output goes quiet
(`Grid::release_history_spare_rows`); otherwise most lines allocate and clear a
fresh row. They stay uncounted, and at most one block of rows.

Tests: `wide_character_pairs_round_trip_with_their_spacers`,
`spare_rows_survive_history_bounds_until_released`,
`a_coloured_line_widened_and_then_recycled_clears_its_old_columns` (fails
before the `grow` change), and the existing randomized codec and storage
tests, whose synthetic rows now keep the occupancy rule (`obey_occupancy`).

## Fewer passes per printed cell

Measured on a 107 MB log of plain, truecolor SGR and CJK lines (`feedbench`,
64 KiB feeds): 14.4 G instructions before, 11.5 G after, for the same grid.

- **`input_ascii` in one pass.** The segment used to be scanned for wide cells
  and then written; now each cell is checked and written in the same loop
  (`Row::write_from` raises occupancy only over the cells written, as indexing
  each written range did). The ASCII charset skips the mapping call, and a
  cell whose old and new extra storage are both empty skips the reference
  count.
- **Wide characters written as pairs.** `Term::write_wide_pair` writes a wide
  character and its spacer with one row lookup, when neither cell belongs to
  an earlier wide character (otherwise the old two `write_at_cursor` calls
  clear its partner). `Handler::input_text` (from the vendored VTE) loops over
  a run of characters beyond ASCII and takes that path for each wide
  character that continues no cluster, without a call per character;
  everything else goes to `input`.
- **History encoding.** An ASCII run of one plain style is copied into the row
  text through reserved capacity (`extend_ascii_run`), keeping the length out
  of memory between characters. The payload, text, run and compressed-payload
  buffers are reused across blocks (thread-local), and a block's bytes are
  one exact-size `Arc<[u8]>` instead of a shrinking reallocation and a copy.
- **LZ bounds checks.** `read_u32` is one checked load, and `match_length`
  slices both runs once so its loops index without checks. Its result is
  unchanged (`match_length_agrees_with_a_bytewise_scan`), so payloads are
  byte-identical.

Tests: `non_ascii_runs_match_per_character_input` compares `input_text` with
`input` per character, with the second terminal writing wide characters the
old way (`reference_wide_writes`), across wrapping, insert mode, charsets,
hyperlinks, prompt marks, mode 2027, scroll regions and the alternate screen;
`ascii_runs_match_per_character_input` covers the one-pass writer.

## Program colors: a generation, and RIS resets them

`Term` keeps `colors_generation`, advanced whenever `set_color` or
`reset_color` changes a color cells are painted with (0–255 and the dynamic
foreground and background; not the cursor). A program that recolors with
OSC 4/10/11 changes no cell, so without it the host's change detection saw no
change and never republished the screen. `reset_state` (RIS, `ESC c`) also
clears every color a program set, as xterm and libghostty do; upstream kept
them across `reset`. No wire, checkpoint or protocol change.

## Faster skipping through text that does not repeat

The history LZ codec steps one byte further per 32 consecutive missed
matches, instead of per 64, as LZ4's acceleration does. On floods of text
that repeats little, compressing took a third of the parser thread; this
makes ASCII floods about 10% faster (Unicode 5%). Payloads stay valid LZ
streams, decoded unchanged, but find slightly fewer matches: the 4 MiB budget
keeps 0.35% fewer rows of kitten ASCII, 0.9% of kitten Unicode and 0.4% of
`git log -p` at 160 columns (outputs at the 150,000-row ceiling are
unchanged).

## Test-only `observe` feature

`observe = ["vte/observe"]` only forwards the vendored VTE's test-only OSC
totals (see its DIRI-PATCH.md) so `diri-terminal-state`'s `bench-verify`
feature can reach them. No source change.
