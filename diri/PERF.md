# diri performance record

## Stable reading during streaming redraws (2026-09-10)

An absolute scroll anchor did not protect text still backed by the live grid.
The desktop now captures one grid when scrolling away from live, and retains
already fetched history rows within the existing 512-row cache until returning
to live. Live damage still updates the authoritative mirror. No new lock,
worker, parser, or Holder allocation is involved; the extra screen copy exists
only for a scrolled terminal.

Run `cargo bench -p diri-term --bench reading_view -- --sample-size 10
--measurement-time 1 --warm-up-time 1` from this workspace. On macOS arm64 in
release mode, the 160×50 case measured capture/release at 1.41–1.42 µs,
live full-screen damage at 5.97–6.09 µs, and damage while reading at
6.01–6.14 µs. Damage timings include cloning each input frame. These are local
mirror/capture microbenchmarks, not display-presentation or PTY latency gates.

## Local mouse-driven redraw publication (2026-09-10)

The local Holder output follower waited for its ordinary 100 ms quiet tick
after parsing a short redraw before notifying attached clients. Claude's
mouse-driven transcript scrolling exposed this because every wheel response
can end in silence. The wait now uses the remaining 8 ms output-batch budget
while publication is pending; an idle follower keeps its existing blocking
wait. No Holder/protocol changes or additional idle polling are introduced.

`cargo test -p diri-engine --test held_scroll -- --nocapture` exercises an
SGR wheel frame through the actual local Holder, PTY, Engine and binary
attachment. A fullscreen fixture redraws once per wheel report, then waits.
On macOS, the debug-build median of ten measured turns fell from 103.51 ms
to 9.39 ms. The regression requires a median below 50 ms, leaving scheduler
headroom while catching the former 100 ms wait. This measures arrival at the
client socket, not display presentation or Claude's own scrolling speed.

## Live-session CPU and local Holder memory (2026-09-09)

The reported Activity Monitor usage was reproduced on the installed 0.6.5
app: app CPU commonly 8–15% (one 28% interval), Engine 3–6%, local Holder
1–2.5%; physical footprints approximately 419–424, 186–191 and 126 MB.
The workload included 20 local and 56 remote sessions and mouse interaction;
these are not idle acceptance measurements. App stack samples primarily showed
rendering, and about 266 MB of its memory map was graphics-related. This pass
does not claim to resolve or reduce that graphics allocation.

Three Engine/local Holder changes address directly observed waste:

- A local Holder that predates output streaming is negotiated once per adopted
  session. Previously each quiet tick/log wake reconnected and elicited the
  same rejection. The socket/Session regression first failed with 12
  negotiations across ten real log updates, then passed with one. A second
  test drops the first connection and verifies transport failures still retry.
  No protocol, input-delivery, or session-survival behavior changes.
- The local Holder opens its append-only log with no in-memory raw-output ring.
  No Holder operation reads that ring; the Engine reads the durable file and
  the independent bounded live queue serves output subscribers. A regression
  verifies zero retained ring bytes and exact tail replay after file rotation.
  Terminal scrollback and Engine/Remote Helper output budgets are unchanged.
- Codex title refreshes batch sessions by their exact account-profile directory.
  Each pass opens each candidate database, inspects its schema, and reads the
  bounded title index once for the group. Statements are reused within the pass
  and all resources then drop. There is no persistent title cache or change to
  the one-second cadence. Tests preserve explicit/index/generated title priority,
  immediate observation of renames, missing fields, and account isolation.

Measurements use Apple Silicon/macOS 26.5.2 and the repository's current pinned
Rust 1.97.1 release toolchain. The new `holderbench` launches a private manager
with 20 real PTYs, drains 5 MiB per session, waits two seconds, then measures the
manager's physical footprint and five seconds of idle CPU. It excludes the
Engine, desktop and synthetic producer processes. The same benchmark executable
was run against the saved pre-change Holder and the changed Holder.

| Measurement | Before / individual reads | After / batched reads |
| --- | ---: | ---: |
| Holder physical footprint, median of three runs | 150.1 MiB | 7.3 MiB |
| Holder footprint range | 146.0–155.1 MiB | 7.1–8.1 MiB |
| Twenty Codex titles, median of 21 passes | 1.77 ms | 0.19 ms |

The Holder memory reduction is approximately 95%. Idle Holder CPU was noisy
(0.30–0.37% before and 0.17–0.38% after), so no idle-CPU percentage improvement
is claimed. The title comparison measures twenty individual calls versus one
shared pass in the same release build, not total Engine CPU. Launch-plus-drain
timings also varied; they are not a terminal-throughput comparison.

A separate `fleetbench` comparison releases 20 sessions together to drain
16 MiB of generated colored log lines each, using the same Engine executable
and changing only `DIRI_HOLDER_BIN`. All 120 sessions across three paired runs
finished. Median aggregate throughput was 116.5 MiB/s before and 118.8 MiB/s
after (ranges 111.5–118.7 and 114.7–124.3 MiB/s). This supports comparable
throughput while removing the retained ring, not a broad throughput-speedup
claim. There was no desktop attached during either fixture.

Reproduce from `diri/`:

```sh
cargo build --release -p diri-engine --bin diri-holder --example holderbench
DIRI_HOLDER_BIN="$PWD/target/release/diri-holder" target/release/examples/holderbench 20
cargo test --release -p diri-engine --lib codex_title_batch_timing -- --ignored --nocapture
cargo test -p diri-engine --test holder_output_compat -- --nocapture
```

All fixtures use temporary state and terminate only their own sessions. Existing
production processes were not replaced or stopped. The Engine improvements need
an Engine update; the memory saving requires an updated Holder process. The
long-lived local manager cannot pick up new code until its sessions end and it
restarts, including sessions subsequently launched through that old manager.

Validation in the original development workspace: formatting, Clippy with
`-D warnings`, all workspace tests
(1,349 passed, 26 intentionally ignored), release build, and terminal performance
gate passed. The release gate measured local input-to-grid median 113 µs and
persistent input p95 16 µs. No UI/rendering behavior or production dependency
was changed by this pass. That workspace also contained separate terminal/parser
optimizations; those edits are not included in this CPU/memory PR. The numbers
above describe that measured workspace, not a newly measured desktop release.

## Historical baseline

Historical measurements in this file are from the T16 release build on Apple
Silicon, macOS 26.5.2, on 2026-07-23. They predate subsequent UI/font changes
and are context, not proof that the current release passes. Release acceptance
now comes from the packaged-artifact gate described below.

The current optimized universal bundle was measured on Apple Silicon on
2026-07-29 with the deterministic stress fixture:

| Packaged 0.2.0 scenario | Physical footprint | Mean idle CPU | Peak idle CPU |
| --- | ---: | ---: | ---: |
| Normal 1100×700 window | 62.4 MB | 0.533% | 0.600% |
| Large 1800×1100 window | 102.4 MB | 0.317% | 0.400% |

These are physical-footprint measurements, not Activity Monitor's larger
virtual-memory figure. A follow-up 10-second stack sample found the main thread
blocked in AppKit for 7,600 of 7,698 samples and only 15 GPUI window steps, with
no app-owned periodic render task.

```sh
export PATH=/tmp/diri-cargo-home/bin:$PATH
export CARGO_HOME=/tmp/diri-cargo-home
export RUSTUP_HOME=/tmp/diri-rustup-home
export CARGO_TARGET_DIR=/tmp/diri-shared-target
cargo build -p diri-app --release
```

The shared target is measurement/build cache only. It must never be packaged or
shipped.

## Twenty-terminal performance pass (2026-09-06)

Measured on Apple Silicon, macOS 26.5.2, Rust 1.95.0 release builds. These are
workload-specific results, not proof that Diri is universally faster than
Ghostty, Kitty, or every other terminal.

### Retained heap and resizing

`cargo bench -p diri-terminal-state --bench terminal_fleet` counts requested
live Rust heap bytes for twenty real `HeadlessScreen` instances. It fills each
80×50 terminal with 6,000 hard-newline log lines, widens all to 320 columns,
then performs 600 mixed width/height resizes. The initial baseline used the
same harness before the production changes. Values exclude agents, Holders,
output logs, the desktop, and GPU resources. Reserved heap is **not** physical
footprint; allocator row slack also means a 4 MiB history-cell allowance does
not imply a 4 MiB terminal process.

| Twenty terminal cores | Before | After |
| --- | ---: | ---: |
| Fresh, 80×50 | 43.88 MiB | 3.88 MiB |
| Full history, 80×50 | 157.49 MiB | 117.49 MiB |
| Full history after widening to 320×50 | 381.00 MiB | 101.06 MiB |
| After repeated resizing | 373.49 MiB | 92.55 MiB |
| Live allocations after dropping all cores | 0 bytes | 0 bytes |
| Allocations across 2,000 cursor-only updates | 4,000 | 0 |

The original implementation retained the construction-time history row limit
through width changes. A regression test first failed with 2,184 history rows
after widening to 320 columns; the current-width allowance is 546. The fix
updates primary history even while the alternate screen is active, preserves
reflow ordering, and removes the minimum-64-row exception that could exceed
the budget at very wide dimensions. Same-size resizes return immediately.
Tests reconstruct incremental updates through split UTF-8, ANSI styles,
alternate screens, insert/delete lines, synchronization, and resizing and
compare them with independent full snapshots.

Cursor-only publications now compare/update the existing cell baseline in
place, allocating outgoing rows only when cells differ. VTE's unconditional
2 MiB synchronization reserve is now lazy; see
[vendor/vte/DIRI-PATCH.md](vendor/vte/DIRI-PATCH.md). All 54 upstream VTE tests
pass, including split, nested, and oversized synchronized updates. First-use
synchronized frames can allocate; subsequent frames reuse their capacity.
The default Remote Helper Build ID includes the vendored sources.

The fleet benchmark gates fresh heap below 8 MiB, widened and churned heap
below 140 MiB, zero warmed cursor-update allocations, and zero leaked heap.
It runs from `scripts/terminal-perf-gate.sh`.

### Scrolling renderer

The production Metal benchmark exposed zero reused shapes on full-height
scrolling: the cache followed screen row numbers rather than surviving rows.
The renderer now rotates its existing cache with a detected scroll, verifies
complete cell equality before reuse, and translates backgrounds and decorations
with their text. Render-context changes still force rebuilding. No new cache,
protocol, lock, or dependency was introduced for rendering.

The same 160×50 benchmark improved from a Criterion estimate of 845.48 µs to
795.43 µs (Criterion's paired estimate: 7.2% faster). Steady scrolling reuses
49 of 50 row shapes; a new gate requires over 90% reuse once startup amortizes.
Tests cover both scroll directions, multi-row movement, sparse damage,
background/decoration positions, and an edited cell that must be reshaped.
This measures the real headless Metal renderer, not input-to-photon latency.
The after run had one 120 Hz overrun during calibration; steady-state medians
are not a guarantee about every frame.

The older `terminal_throughput` typing fixture repeatedly overwrote `x` with
`x`. It now alternates `x` and `y` so every operation changes a real cell.
Do not compare its updated typing time directly with the historical table.

### Sessions and remote latency

Twenty simultaneously released local sessions each drained 16 MiB of colored
build logs: **108.2 MiB/s aggregate**, 2.96 s wall time, fastest/median/slowest
2.79/2.95/2.96 s. This measures production PTYs, the Holder manager, output
logging, parsing, and status handling, with no desktop attached. The revised
`fleetbench` uses a shared start gate, passes payload paths as argv data,
fails on unfinished sessions, and terminates owned sessions on failure.

The corrected `sessionbench idle 20` measured **59.68 MiB physical footprint
and 0.97% of one CPU core** over ten seconds across 22 processes: the benchmark
engine, its private Holder manager, and twenty sleeping children. Real Agent
runtimes and model workloads are excluded. The old `RUSAGE_CHILDREN` metric
missed live children; the probe now samples the actual process set through the
Engine's platform resource collector. These are observations, not portable
hard gates or before/after daemon comparisons.

The release Remote Holder UDS gate passed:

| Metric | Measured | Architecture ceiling |
| --- | ---: | ---: |
| Snapshot p90 | 10 µs | 100 ms |
| Input-to-PTY p95 | 131 µs | 10 ms |
| Output-to-diff p90 | 23 µs | 50 ms |
| Loopback median / p90 | 102 / 148 µs | 75 / 150 ms |

These are same-host UDS timings, not SSH/WAN or display latency. The real SSH
soak and native Linux architecture jobs remain CI release gates.

### Comparison scope and reproduction

`cargo build --release -p diri-engine --example termcompare` builds a Rust
probe that writes identical 64 KiB chunks to the terminal in raw mode and waits
for a cursor report behind the payload. It records five runs after warmup and
the terminal dimensions, and fails rather than substituting drain-only timing
for an unanswered query. Run `termcompare <payload> <results.json>` inside each
terminal at the same geometry; compare medians, versions, and configurations.

Exploratory local runs were made with Diri, Ghostty 1.2.3, and Kitty 0.48.2.
They are **not an accepted ranking**: Diri's run was headless, competitor GUI
launch/exit was inconsistent, and initial records lacked verified geometry.
Use a separate input-to-photon/resize capture and comparable rendered workloads
before publishing superiority claims. Actual 20-Agent CPU, long-duration
memory behavior, and sustained loaded input latency require broader workloads
than twenty synthetic terminal producers.

### Verification of this pass

- `cargo fmt --all -- --check` and workspace Clippy with `-D warnings`: pass.
- `cargo test --workspace`: 1,336 passed, zero failed, 24 intentionally ignored.
- `cargo build --workspace --release`: pass.
- `scripts/terminal-perf-gate.sh`: pass, including the added heap/allocation and
  shape-reuse gates, VTE tests, persistent input, and attach tests. The final
  state fixture measured typing 753 ns, scrolling 30,889 ns, and cursor-only
  721 ns per operation. Local input-to-grid median was 337 µs.
- Release Remote Holder UDS gate: pass. After including the vendored parser in
  the Helper source identity, remote package tests passed again: 40 passed,
  zero failed, five opt-in tests ignored. The native macOS arm64 Helper probe
  reports protocol 1.4 and all required capabilities.
- The inert packaged-process gate passed on a disposable, ad-hoc-signed copy
  containing the new release executable: normal/large footprints 33.8/33.6 MB,
  sampled idle CPU 0%. Window visibility was not independently verified in
  this run, so these process-only readings are not accepted visible-window
  memory comparisons. No production bundle was replaced or published.

PTY/UDS and Metal tests required running outside the execution sandbox. Initial
sandbox-only attempts could not launch their private Holder sockets or macOS
graphics services; the authorized runs above completed successfully.

## Terminal interaction hot path (2026-08-13)

Release-mode measurements below compare untouched `main` at `39af365` with the
terminal performance branch on the same Apple Silicon Mac running macOS 26.5.2.
The terminal-state fixture is 160×50 and reports the median of five 5,000-
interaction rounds. The renderer fixture scrolls the same 160×50 build log
through GPUI's production text system and the real headless Metal renderer.
The input-to-grid fixture reports the median of 101 echoed writes, long enough
to include the viewport's scrolling phase.

| Interaction | `main` | Optimized | Change |
| --- | ---: | ---: | ---: |
| Prompt typing, parser through grid diff | 36,322 ns | 771 ns | 47.1× faster |
| Cursor-only traffic | 34,814 ns | 796 ns | 43.7× faster |
| Full-height build-log scroll | 34,543 ns | 32,242 ns | 6.7% faster |
| Metal scrolling frame, Criterion estimate | 881.59 µs | 868.24 µs | 1.5% faster |
| Metal scrolling frame p95 | 872 µs | 869 µs | no regression |
| Holder write p50, legacy vs persistent stream | 32 µs | 12 µs | 2.7× faster |
| Holder write p95, legacy vs persistent stream | 40 µs | 15 µs | 2.7× faster |
| Local input-to-grid median, including scroll | 75 µs | 64 µs | 15% faster |

The Metal sample recorded one invalidation per frame and zero 120 Hz frame-
budget overruns. The renderer benchmark rejects a steady-state average CPU
frame cost at or above 8 ms; Criterion's tiny cold-start calibration batches
are excluded until at least 32 frames have been observed. The terminal-state
benchmark has absolute budgets for typing,
scrolling, and cursor traffic, while the Holder and attach tests enforce their
release latency ceilings.

The measured changes are deliberately distributed along the existing deep
terminal interface rather than hidden behind another wrapper:

- Alacritty damage is preserved at row granularity, so typing and cursor motion
  no longer hash and compare the entire viewport. Grid publication still
  compares actual cells before sending a row.
- Adjacent grid frames coalesce before one authoritative buffer mutation and
  one selected-pane notification. The client handoff is bounded; offscreen
  terminals remain current without invalidating the window.
- The daemon publishes the leading edge immediately, lets two interactive
  response publications bypass coalescing, and caps only continuous output at
  8 ms (120 Hz). A destructive erase may wait up to 16 ms for its redraw bytes,
  but the wait ends as soon as they arrive and never applies to typed echo or
  additive scrolling. GPUI's display link is the sole client-side repaint
  pacer.
- Held sessions negotiate an additive persistent binary input/resize stream.
  Old live Holders reject the optional negotiation and continue over the exact
  legacy JSON/base64 request path. On Apple platforms the dedicated input lane
  uses interactive QoS. The daemon's held-output follower uses the same class
  only during its existing recently-attached/input hot window, then restores
  default QoS; together they improve end-to-grid latency without elevating idle
  or background sessions indefinitely.
- Font metrics are retained by font and size, and ordinary undecorated rows
  skip two independent quad scans through a single plain-row check.
- Frame decoding advances a read cursor and compacts occasionally instead of
  shifting the receive buffer after every decoded frame.

Run all terminal-specific gates with:

```sh
diri/scripts/terminal-perf-gate.sh
```

## Memory

`DIRI_PERF_LARGE_WINDOW=1` is a retained profiling switch that starts the app at
1800×1100 instead of 1100×700. Samples use `/usr/bin/vmmap -summary <pid>` after
startup work and geometry have settled.

### Before and after

| Release-build sample | Physical footprint | IOSurface resident | owned unmapped (graphics) resident | MALLOC_LARGE resident | MALLOC_SMALL resident |
| --- | ---: | ---: | ---: | ---: | ---: |
| Handoff baseline (prior run) | ~429 MB | ~125 MB | not recorded | ~111 MB | ~52 MB |
| Reproduced large-window baseline, before geometry quiescence | 411.6 MB | 92.5 MB | 242.6 MB | 4.0 MB | 41.7 MB |
| Large window, after fix and settle | 202.3 MB | 92.5 MB | 50.6 MB | 4.0 MB | 39.2 MB |
| Normal window, after fix and settle | 109.2 MB | 36.3 MB | 20.5 MB | absent | 37.4 MB |
| Normal window after 100 normal↔large resize cycles | 110.6 MB | 36.3 MB | 0.1 MB resident / 20.4 MB swapped | absent | 25.2 MB |
| Normal window after 40 min / 27 periodic large→normal transitions | 116.3 MB | 0 MB resident / 36.3 MB swapped | 0.1 MB resident / 20.4 MB swapped | absent | 23.5 MB |

The reproduced baseline was captured after merging current `main`, so it
includes the earlier system-font fix. That fix removed the original persistent
~111 MB `MALLOC_LARGE` font copy. The remaining large-window regression was GPU
retirement, not a CPU heap leak.

The red/green probe was a three-second `SIGSTOP` of only the locally built test
process. Physical footprint fell from 411.6 MB to 223.8 MB and graphics
residency from 242.6 MB to 90.6 MB without changing model state. Resuming did
not recreate the retired burst (204.8 MB). This isolated continuous decorative
frames as the condition preventing Metal from retiring resize-era resources.

That historical fix paused decorative repainting after geometry settled. The
current implementation goes further: status glyphs and the terminal cursor are
static, so they never create autonomous frame tasks. Real status changes and
terminal grid damage still repaint immediately. The window and root surface
are opaque, avoiding a persistent WindowServer backdrop/blur composition pass.

### CPU allocation retention

- Store events are applied immediately, but `StoreSnapshot` cloning and
  UI/menu publication are coalesced to one update per 16 ms display interval.
  The watch channel retains only the latest snapshot.
- Startup previously launched independent recurring usage scans over 4,729
  transcript files (about 2.4 GB on this machine). Usage now scans once at
  startup and refreshes only after store-change events, debounced by two
  seconds. Its persisted transcript ledger resumes from validated append
  offsets, handles truncation/replacement, and does no timer-driven idle work.
- Each resident terminal's decoded scrollback cache is capped at 512 rows near
  the current viewport. Evicted rows remain daemon-owned and are fetched again
  if revisited. Three resident terminals therefore cannot retain an entire
  repeatedly traversed history indefinitely.
- Every incoming terminal diff still updates its authoritative buffer, but a
  receiver burst folds adjacent rows into one final update and one selected-
  session notification. Background residents never invalidate the window. An
  active find arms only one output-rescan timer.
- The daemon skips screen-to-cell extraction entirely while no output sink is
  attached. With a sink, leading-edge and interactive output is immediate;
  continuous output is capped at 120 publications per second. A later
  attachment receives a fresh full grid.

### Acceptance

- Historical idle target, under 250 MB: **pass**, 109.2 MB at normal size and 202.3 MB at
  the 1800×1100 stress size.
- Historical resize-churn target, under 300 MB: **pass**, 110.6 MB after 100 alternating
  geometry transitions.
- Historical long-use target, under 300 MB: **pass**, 116.3 MB after 40 minutes. The same
  process ran 27 minute-spaced large→normal transitions and the normal
  90-second usage refresh loop. Its physical peak remained launch-bound at
  318.4 MB; the footprint at the long-use checkpoint was 183.7 MB below budget.

## Packaged-release gate

`scripts/perf-gate.sh` launches the executable inside `dist/diri.app` directly
with the deterministic stress sidebar fixture, records the exact PID it owns,
waits 30 seconds for startup and Metal resource retirement to settle, then
measures:

- physical footprint from `vmmap -summary`;
- mean and peak idle CPU across interval samples from `top`;
- both the normal 1100×700 window and the retained 1800×1100 stress switch.

The fixture contains working, starting, and needs-input status rows—the states
that triggered the original decorative repaint loop—without attaching to or
resizing the user's selected live PTY. Preview mode also uses inert store and
updater handles: it does not connect to the daemon, scan transcripts, or check
the network. Pass `--live-daemon` only for an intentional follow-up measurement
of the real local session set.

It does not use `pkill`, name-based termination, or touch an existing Diri. On
cleanup it verifies the original process start time and terminates only the PID
it launched. Default release ceilings are 80 MB normal, 140 MB large, 0.75%
mean idle CPU, and 1% peak idle CPU. These bounds retain practical machine
variance while catching a meaningful regression well before the reproduced
~500 MB / ~29% failure.

After packaging, run the same gate the release script runs:

```sh
diri/scripts/perf-gate.sh --app diri/dist/diri.app --scenario all
```

Budgets are configurable for deliberate tightening:

```sh
DIRI_PERF_NORMAL_MAX_MB=80 \
DIRI_PERF_LARGE_MAX_MB=140 \
DIRI_PERF_IDLE_AVG_CPU=0.75 \
DIRI_PERF_IDLE_PEAK_CPU=1 \
  diri/scripts/perf-gate.sh --app diri/dist/diri.app
```

`diri/scripts/release.sh` runs this after the final bundle is signed, notarized,
and stapled but before copying or publishing artifacts. `SKIP_PERF_GATE=1` is
an explicit escape hatch for a non-GUI release host; any such release needs the
same packaged bundle measured manually on a Mac before publication.

For final release sign-off, first run the deterministic gate. Then close
unrelated high-load apps and optionally repeat with `--live-daemon` to sample
the normal local session set. Record the printed PID, footprint, average CPU,
peak CPU, macOS version, and hardware next to the release notes. Do not reuse
historical development-binary numbers.

## Validation

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
diri/scripts/terminal-perf-gate.sh
diri/scripts/perf-gate.sh --app diri/dist/diri.app --scenario all
```

The packaged probe is the release acceptance authority; the historical command
results above apply only to the dated T16 sample.

## Sidebar activity marks (2026-09-08)

Session rows separate activity (left) from agent identity (right). Working
marks use eight embedded SVG frames, shared through GPUI's existing SVG atlas.
The sidebar samples one phase per existing render, quantized to 125 ms; it
never requests another render for the activity mark. There is no timer per
row or per sidebar, and no animated-image decoder. Between existing sidebar
repaints the mark stays still. Reduce Motion fixes the phase at zero.
Sleeping and ended rows have no animated mark. This deliberately preserves
the no-periodic-wake contract instead of promising a continuous spinner.

The following measurements are from the initial layout at `1ca2efb`, before
aligning the leading activity column with the project icon and moving parent
fold controls to the trailing edge. That refinement removes the empty leading
fold slot; the animation policy is unchanged.

On this Apple Silicon workstation running macOS 26.5.2, an optimized native
headless benchmark with **30 visible working rows**, a 360×1120 pt window,
32 warmup repaints, and 500 measured forced repaints produced:

| Three alternating runs | Median repaint (ms) | p90 repaint (ms) |
| --- | --- | --- |
| `main` at `5b6b46e` | 1.011 / 1.018 / 1.014 | 1.059 / 1.091 / 1.050 |
| Separate activity/identity | 1.043 / 1.041 / 1.045 | 1.153 / 1.109 / 1.105 |

The additional mark costs approximately **0.03 ms per forced full sidebar
repaint** in this fixture. Whole-process CPU time (including startup, warmup,
and PNG capture) was 0.710–0.739 s before and 0.726–0.759 s after. These are
rendering measurements, not a packaged idle-CPU gate or a guarantee about
live terminal workloads. No added timer means the activity marks introduce
no autonomous wakeups, including with 30 working sessions.

Reproduce from `diri/` using an isolated Cargo target directory:

```sh
DIRI_VISUAL_SCENARIO=fleet DIRI_VISUAL_WIDTH=360 \
DIRI_VISUAL_POPOVER=none DIRI_VISUAL_BENCH=1 \
DIRI_VISUAL_OUTPUT=/tmp/sidebar-fleet.png \
cargo test --release -p diri-app render_sidebar_preview_screenshot -- --ignored --nocapture
```

For a before/after comparison, use the same fixture and screenshot benchmark
harness on both revisions. `DIRIJOR_SIDEBAR_PREVIEW=fleet` also opens the
30-session fixture interactively without an Engine connection. Use
`DIRI_VISUAL_SCENARIO=stress` and `DIRI_VISUAL_LIGHT=1` for layout checks of
loading, sleeping, nested, and long-title rows.
