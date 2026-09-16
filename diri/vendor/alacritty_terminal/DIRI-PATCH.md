# Diri terminal extensions

Pinned alacritty_terminal 0.26.0 (Apache-2.0). The terminal metadata extension adds a
PROMPT_START cell flag and Handler::mark_prompt implementation, paired with the
OSC 133 A dispatch in vendored VTE. Markers follow the existing grid erase,
scroll, and reflow lifecycle and are ignored on the alternate screen.

This keeps prompt navigation in the authoritative parser, including synchronized
updates, instead of adding a second escape-sequence parser or guessing boundaries
from terminal text. No new runtime dependency. The source participates in the
Remote Helper Build ID. Revisit this small patch when updating the parser.

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
