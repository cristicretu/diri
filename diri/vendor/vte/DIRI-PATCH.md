# Diri VTE patch

Source: crates.io `vte` 0.15.0, copied from the Cargo registry. Original
Apache-2.0/MIT licenses and upstream tests are retained. There are six
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

CSI parameter bytes (digits, `;`, `:`) are consumed in one loop
(`advance_csi_params`) that takes the same action per byte as
`advance_csi_param`; the byte after them goes through `change_state`. None of
those bytes dispatches, so `advance_until_terminated` still stops exactly
where it did.

Test: `single_scan_ground_and_csi_match_the_bytewise_parser` keeps the old
parser as `advance_reference` and compares every dispatch over 400 random
soups of printable runs, controls, DEL, C1 in both encodings, valid,
truncated and invalid UTF-8 and escapes of every kind, fed whole and in
pieces of 1 to 40 bytes. `printable_runs_end_at_the_first_byte_outside_space_to_tilde`
checks the word test for every byte value at every offset.
