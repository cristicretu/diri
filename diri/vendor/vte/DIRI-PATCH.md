# Diri VTE patch

Source: crates.io `vte` 0.15.0, copied from the Cargo registry. Original
Apache-2.0/MIT licenses and upstream tests are retained. Each production change has
a section below. The first is `SyncState::default`: its byte buffer starts empty instead of reserving
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

## Printable ASCII runs

Ground-state text is still validated and dispatched in order, but a run of
printable ASCII (`0x20..=0x7e`) now reaches the performer as one
`Perform::print_ascii(&str)` call instead of one `print` per character. The
ANSI performer forwards it as `Handler::input_ascii` and records the run's
last character for REP. Both trait methods default to the per-character loop,
so other performers and handlers are unchanged. Controls, DEL, C1 and every
non-ASCII character still take `execute`/`print` individually.

This lets the terminal write a row segment at a time (see the alacritty patch
notes). Test: `ascii_runs_print_every_character_in_order_and_feed_repeat`.

## Completed synchronized updates

`Processor::synchronized_updates_completed` counts synchronized updates
(DECSET 2026) the child closed with its ESU. Updates that end by timeout or
by the 2 MiB limit are not counted. Diri's Engine uses the count to tell a
complete redraw from a half-drawn one: a keystroke answered by a closed
synchronized update is published at once even when the redraw removed cells.
Parsing is unchanged; the counter is one field and one increment per ESU.
Test: `completed_synchronized_updates_are_counted`.

## Cell size query

`CSI 16 t` (XTWINOPS: report the cell size in pixels) dispatches to a new
`Handler::cell_size_pixels`, whose default does nothing, beside the existing
14 (text area in pixels) and 18 (text area in cells). Image tools such as
`chafa` and `timg` size their output from it. Only Diri's local Engine answers,
and only for terminals that place inline images; others ignore it as before.

## Character after a split UTF-8 sequence

Upstream bug, fixed here. When a read ended inside a multi-byte character,
`advance_partial_utf8` copied up to three bytes of the next read to finish it.
If those bytes held the rest of the character, an ASCII byte and the start of
another multi-byte character (`e\u{301}a\u{308}` split inside `\u{301}`), it
printed the first character but reported all the valid bytes as consumed, so
the ASCII byte was never parsed: the letter vanished, and only when that read
boundary fell there. It now consumes only the first character's bytes.
Test: `partial_utf8_then_ascii_then_partial_utf8`.

## Mode 1016

`NamedPrivateMode::SgrPixelMouse` names DECSET 1016 (SGR mouse reports in
pixels); `alacritty_terminal` sets, resets and reports it.

## Mode 2027

`NamedPrivateMode::GraphemeClustering` names DECSET 2027 so `Term` can set,
reset and report it (DECRQM answers `1`/`2` instead of `0`). Parsing is
otherwise unchanged; the width rules live in `alacritty_terminal`'s
`term/grapheme.rs`.

## Single-scan ground text and CSI parameters

Ground text before the next ESC was read three times: `memchr` for the ESC,
`str::from_utf8` over all of it, then a byte loop splitting printable runs
from controls. `advance_ground` now reads it once. Printable ASCII runs are
found eight bytes at a time (no byte below 0x20 or above 0x7E) and go to
`print_ascii`; C0 controls execute and DEL prints, as before; only stretches
of bytes at or above 0x80 are UTF-8 validated. A sequence cut short by the
ASCII byte after it prints one replacement character there, which is what
validating the whole text gave (`error_len` of the truncated sequence), and a
sequence cut by ESC or the buffer end is handled exactly as before.

Characters beyond ASCII between ASCII bytes and C1 controls go over as one
`Perform::print_text(&str)` (default: `print` per character), forwarded as
`Handler::input_text` (default: `input` per character), so a terminal can
write a CJK run without a call per character.

CSI parameter bytes (digits, `;`, `:`) are consumed in one loop that takes
the same action per byte as `advance_csi_param`; the byte after them goes
through `change_state`. None of those bytes dispatches, so
`advance_until_terminated` still stops exactly where it did. (The loop is now
`advance_csi`, which also takes the first parameter byte in CSI entry; see
"String bodies and CSI parameters in runs".)

Test: `single_scan_ground_and_csi_match_the_bytewise_parser` keeps the old
parser as `advance_reference` and compares every dispatch over 400 random
soups of printable runs, controls, DEL, C1 in both encodings, valid,
truncated and invalid UTF-8 and escapes of every kind, fed whole and in
pieces of 1 to 40 bytes. `printable_runs_end_at_the_first_byte_outside_space_to_tilde`
checks the word test for every byte value at every offset.

## String bodies and CSI parameters in runs

Upstream walks every byte of an OSC, DCS, SOS, PM or APC body through the
state dispatch, one call each. A Kitty image chunk or an 8 KiB OSC cost a
dispatch per byte. Now:

- **SOS/PM/APC and ignored DCS** skip to the next CAN, SUB or ESC with
  `memchr3`; every other byte was ignored anyway.
- **OSC** stores the body up to the next C0 byte as one copy. With `std`,
  `osc_raw` keeps the `;` separators and `osc_end` splits the first
  `MAX_OSC_PARAMS` of them, instead of recording each as it arrives. What is
  dispatched is unchanged: the same parameters, C0 controls still dropped,
  later separators still ignored. The 2 MiB limit still counts body bytes
  without separators (exactly, once a body nears it), and separators past the
  sixteenth are dropped, so the buffer stays below the limit plus 16. Without
  `std` the fixed buffer keeps the per-byte rules.
- **DCS passthrough** hands runs of `put` bytes to a new
  `Perform::put_slice`, whose default calls `put` per byte. The ANSI
  performer appends the run to its request with the same 1 KiB cap.
- **CSI** digits, `;` and `:` update the parameters in a tight loop
  (`advance_csi`, which replaces `advance_csi_params` and also covers the
  first parameter byte in CSI entry) instead of a dispatch per byte. Any other byte goes through
  the state machine as before, and the loop returns after it, so
  `advance_until_terminated` still stops right after a synchronized-update
  CSI.
- **REP** (`CSI b`) of a printable ASCII character goes to `input_ascii` in
  runs of up to 256 instead of `input` per repetition; `input_ascii` is
  defined as equal to `input` per character.
- An unhandled OSC no longer formats its whole body for a debug log line
  nobody enabled (`log_enabled!` guard).

The runs are found by `scan::run` (new module, public so Diri's own string
scanners use the same code): NEON on aarch64 and SSE2 on x86_64, both
baseline features of those targets, checking 64 bytes per step; a byte loop
elsewhere. `scan::find_pair` finds a two-byte opener (`ESC ]`, `ESC _`) the
same way. No new dependency and no per-parser memory.

Tests: `osc_bodies_in_runs_dispatch_what_per_byte_storage_did` (random OSCs,
controls, C1 bytes and every terminator, in random reads, against a model of
the per-byte storage), `the_osc_limit_counts_body_bytes_not_separators`
(bodies of the limit minus one, the limit and plus one, with 3, 40 and
300,000 separators), and `scan`'s vector-against-byte-loop tests. Diri's
`chunking_differential` tests compare one-shot, byte-at-a-time and every
split offset.

## One C0 index per read, and OSCs dispatched in place

`scan::c0_positions` lists the offsets of every byte below 0x20 in a read,
and `scan::C0Index` answers "the next C0 byte (of these values) from here".
Diri builds one per read that contains string sequences (capped at 4,096
entries, beyond which every layer scans as before). The progress, notification
and image scanners and this parser then all find string terminators there, so
an OSC or APC body is scanned once per read instead of once per layer.
`Processor::advance_indexed` and `Parser::advance_until_terminated_indexed`
take the index; string states end their runs where it says. Bytes held for a
synchronized update are parsed without it.

With `std`, an OSC that starts with nothing stored and ends in the same read
is dispatched straight from the read: its parameters are slices of the
caller's buffer, split at the first 16 separators exactly as `osc_end` splits
`osc_raw`, and the terminator then does what `advance_osc_string` does after
`osc_end`. Bodies over the 2 MiB limit, and OSCs that span reads, are stored
as before.

Tests: `indexed_parsing_dispatches_what_scanning_does` (2,000 random soups of
every escape kind, string bodies, controls, C1 and UTF-8, in small and whole
reads, with and without the index); the OSC oracle tests also run indexed.

## Test-only OSC observation

Feature `observe` (off by default; no product crate enables it) adds
`vte::observe`: per-thread totals of `osc_dispatch` calls, their parameters
and parameter bytes, and, when turned on with `observe::reset(true)`, an
order-sensitive FNV-1a checksum of every parameter byte. `vt_workloads`
compares these with a reference parse, because an OSC nothing reads (the
long-escapes workload is OSC 6) leaves no terminal state that would expose a
parser skipping its body. Both dispatch paths record: `osc_dispatch` and
the in-place `osc_finish_in_place`. Without the feature the dispatch paths
are unchanged.

Test: `diri-terminal-state`'s `tests/osc_observe.rs`
(`cargo test -p diri-terminal-state --features bench-verify --test osc_observe`).

## C1 control completed across reads

Upstream bug, fixed here. A C1 control sent as UTF-8 (`C2 80` to `C2 9F`)
executes when both bytes arrive in one read, but `advance_partial_utf8`
printed it when a read ended between them. The printed C1 then became the
character `REP` repeats, so `5 C2 9C ESC [ b` showed `55` in one read and
`5` split. A character completed across reads now goes through the same
C1 check as the whole-text path. Test: `split_utf8_c1_control_executes_as_whole`.

## Ground-state text and REP hooks (core v2 prototype)

Two additive hooks for the core v2 prototype (`docs/core-v2.md`), with
defaults that keep every existing performer and handler unchanged:

- `Perform::ground_bytes(&[u8]) -> usize`, called at the start of each
  ground-state slice before the parser scans it. The ANSI performer forwards
  it as `Handler::input_bytes(&[u8]) -> (usize, Option<char>)` and records the
  returned last printed character for REP. A handler may consume a prefix of
  printable text and the C0 controls HT, BS, CR, LF, VT and FF, with exactly
  the effect of the calls the parser would make, and must stop before ESC,
  other C0, DEL, C1, or invalid or incomplete UTF-8. The defaults consume
  nothing, so the parser handles every byte as before.
- `Handler::input_repeat(char, usize)` for REP (`CSI b`). Its default is the
  REP run described under "String bodies and CSI parameters in runs": a
  printable ASCII character goes to `input_ascii` in runs of up to 256, any
  other character to `input` `count` times.

No new dependency; the Remote Helper Build ID changes with the source.
Tests: `diri-terminal-state` `core_v2::tests` (feature `core-v2`) feed the
prototype one-shot, byte by byte, at every split and in random reads and
compare with the current core's byte-at-a-time state; `cargo test -p vte`
covers the unchanged defaults.
