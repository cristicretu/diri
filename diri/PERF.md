# diri performance record

## Holder PTY drain: blocking reads while output streams (2026-10-28)

After the parser work, a 100 MB `cat` spent most of the Holder's time in
system calls: a macOS PTY master returns about 1 KiB per read whatever the
buffer size (the terminal driver's output queue; the line speed does not
change it), and the pump paid a `poll`, a read and an `EAGAIN` read for each
kilobyte, plus a condvar wakeup of the subscriber's writer per kilobyte.

A C replica of the read loop on the same payload (105.8 MB with ONLCR, 104k
reads) isolates the wait itself:

| Read loop | Drain | CPU (sys) |
| --- | ---: | ---: |
| blocking `read` | 0.57 s | 0.28 s (0.27) |
| `poll` + read until `EAGAIN` | 0.73-0.75 s | 0.44 s (0.40) |
| `poll` + one read | 0.73-0.75 s | 0.41 s (0.38) |
| persistent `kqueue` + read until `EAGAIN` | 0.70-0.74 s | 0.39 s (0.35) |

The read count is the same in every row; what differs is how fast a sleeping
reader is woken. Waiting in `read` is the only fast path.

**What changed.**

- `holder/pty_read.rs`: after four reads in a row of at least 512 bytes (the
  kernel queue was full), the pump clears `O_NONBLOCK` and sleeps in `read`.
  The first read shorter than that puts it back in `poll` on the PTY and the
  exit pipe, so a quiet session waits exactly as before (no deadline, no
  wakeups). macOS only; Linux keeps the polling path.
- `O_NONBLOCK` is on the shared file description, so `write_pty` sets it
  around each write while the pump streams (under the PTY lock it already
  held) and the 1 s give-up for a child that stops reading input still holds.
  While the pump polls, writes are unchanged.
- The exit path cannot reach a pump asleep in `read` through the pipe. The
  leader's exit revokes its terminal, which normally ends the read; otherwise
  a no-op `SIGURG` handler without `SA_RESTART` interrupts it, repeated every
  millisecond until the pump returns.
- `FrameQueue::pop_within`: a subscriber writer lingering for the next frame
  of a stream is no longer woken per pushed frame, only at its 100 µs
  deadline or once 64 KiB are waiting, and gets everything joined in one
  frame.

**Measured** (M4 Max, load 2.5-4, 6 interleaved rounds, medians; reference
terminal multiplexer on an isolated server, one attached stream):

| | Reference | Base (#907) | Branch |
| --- | ---: | ---: | ---: |
| `catbench` drain | 0.664 s | 0.716 s | **0.640 s** |
| `catbench` last line at client | 0.664 s (raw bytes) | 0.724 s | **0.667 s** |
| `catbench` Holder CPU | — | 0.78 s | **0.38 s** |
| `drainbench` drain, no subscriber | — | 0.702 s | **0.587 s** |
| `drainbench` drain, discarding subscriber | — | 0.747 s | **0.592 s** |
| `drainbench` CPU, discarding subscriber | — | 0.88 s | **0.41 s** |
| `drainbench` involuntary switches, subscriber | — | 172k | **5.5k** |

Blocking `read` floor (C, same run): 0.574 s. Every Diri run parsed every
byte (`skipped-MB 0.00`). End to end, the Engine session thread (0.65 s CPU)
is now the limit, not the Holder.

Footprint: no thread, timer or allocation per session; one `AtomicBool` per
Holder and one process-wide signal handler. Idle sessions are unchanged.

```sh
cargo build --release -p diri-engine --bin diri-holder --example catbench --example drainbench
target/release/examples/drainbench <log> attach 3
```

## History encoding and per-read costs: Engine parse +25% (2026-10-27)

The Engine session thread is the limit when output floods a held session: on
the `catbench` 100 MB mixed log (plain, SGR/truecolor and CJK lines, 160×50)
it was busy for the whole run, about half of it encoding scrollback history.

**What changed.**

- History encoding (`vendor/alacritty_terminal/src/grid/compact.rs`; see
  DIRI-PATCH "Cheaper history encoding"): wide characters and their spacers
  are encoded as pairs, so a CJK line is a few runs instead of one per cell;
  the search for a row's last non-default cell starts at its occupancy, so
  a short line in a wide row is not scanned to its end (`Row::grow` now keeps
  that rule true, which also fixes coloured blanks surviving a recycled line);
  style lookups hit a 32-slot cache; runs, text and styles are written without
  intermediate copies.
- Per-read costs (`diri-terminal-state`): the Engine keeps the rows history
  recycles across reads and drops them when the checkpoint timer fires after
  output goes quiet (they used to be dropped after every read, so most lines
  allocated a fresh row); row fingerprints stop at a row's last non-default
  cell and cache the two styles a wide-character run alternates between; the
  notification scanner skips CSI introducers and DCS/APC bodies with `memchr`
  instead of a state step per byte (a string that never ends no longer makes
  it scan every later byte).

**Same-machine feed, 100 MiB files, `feedbench … 64k`**, median of 3
interleaved runs, CPU time (instructions):

| Log | main (before #840) | #830 + #840 + #845 | branch |
| --- | ---: | ---: | ---: |
| mixed (`big.log`) | 871 ms (19.8 G) | 871 ms (19.8 G) | **692 ms (15.6 G)** |
| emoji-dense | 1239 ms (28.2 G) | 1592 ms (32.9 G) | **1065 ms (24.9 G)** |
| Indic | 1279 ms (29.0 G) | 1509 ms (31.8 G) | **959 ms (26.2 G)** |

**End to end, `catbench`** (held session, Holder, Engine, attached client
until its grid shows the last line), 6 interleaved rounds at load 3–5, next
to a reference terminal multiplexer on an isolated server with one attached
stream (its column is the last raw byte reaching that client, unparsed). The
instrumented builds also count the bytes the Engine parsed:

| Path | PTY drain, median (min) | Last line at client, median (min) | Engine parsed | Engine thread CPU | Parse rate |
| --- | ---: | ---: | ---: | ---: | ---: |
| Reference | 0.662 s (0.655) | 0.661 s (0.654) | all, in its server | server 0.75 s | — |
| main (before #840) | 0.487 s (0.483) | 0.696 s (0.689) | 80.6 of 105.8 MB, every run | 0.66 s | 122 MB/s |
| #830 + #840 + #845 | 0.504 s (0.474) | 0.693 s (0.679) | 80.6 MB, every run | 0.65 s | 123 MB/s |
| branch, all parsed (3 runs) | 0.508 s | 0.722 s (0.717) | 105.8 MB | 0.69 s | 153 MB/s |
| branch, as main (3 runs) | 0.476 s | 0.542 s (0.538) | 80.6 MB | 0.52 s | 154 MB/s |

When the Engine falls further behind than the session log keeps, it resumes
at the oldest byte kept and skips the rest (25.2 MB here); main does that in
every run. The branch parses 25% more per second, so in half the runs it keeps up and
parses everything. **Parsing everything, it still trails the reference by
about 9% (0.72 against 0.66 s)**; skipping the same bytes as main, it finishes
first (0.54 s). The reference spends 0.75 s of server CPU on the same log.

Footprint: no thread, timer or idle work. While output streams, the Engine
keeps at most one block of recycled rows (128 KiB of cells) between reads;
they are dropped a second after output stops. The Remote Holder keeps the
old behaviour. History stays bounded at 4 MiB; rows retained under it
(`history_depth`): agent transcript 80 cols 127,548 → 124,799, 200 cols
70,543 → 72,479, cargo 200 cols 117,487 → 117,583, others unchanged. Feed rate
in the same bench: cargo 121 → 141 MB/s, `seq` 51 → 95 MB/s at 80 columns.

`scripts/terminal-perf-gate.sh` passes; held input-to-grid median 385–414 µs
on both the base and the branch (3 runs each), Holder input stream p50 14 µs /
p95 15 µs.

Measured and not done: moving block encoding to another thread (a fifth of the
session thread) would break the one-owner rule for terminal state, hold raw
rows across threads and add a thread per streaming session; a smaller LZ hash
table for small blocks, a single-pass field-compare run scan, and a scalar ESC
search before `memchr` changed no cycle counts.

## History depth bounded by stored bytes (2026-10-27)

History used to stop at 10,000 rows even though compressed rows used under
10% of the 4 MiB budget. Depth is now bounded by that 4 MiB of stored
(compressed) history, with a 150,000-row ceiling for near-empty output.
`cargo bench -p diri-terminal-state --bench history_depth` feeds 300,000 lines
(150,000 for the worst case) into one `HeadlessScreen`; base = origin/main.

| content | cols | rows base → branch | stored bytes/row | RSS growth |
|---|---|---|---|---|
| agent transcript (truecolor, box drawing, diffs, CJK) | 80 | 10,000 → 127,548 | 32.2 | 6.2 MiB |
| agent transcript | 200 | 10,000 → 70,543 | 59.0 | 7.2 MiB |
| `cargo build` log | 80 | 10,000 → 150,000 (ceiling) | 23.0 | 5.1 MiB |
| `cargo build` log | 200 | 10,000 → 117,487 | 35.7 | 6.5 MiB |
| `seq` | 80 | 10,000 → 150,000 (ceiling) | 8.1 | 2.5 MiB |
| random truecolor per cell | 80 | 3,839 → 3,812 | 1,096 | 12.8 MiB |

Reference multiplexer, isolated server, 80×24: `cat` of 150,000 numbered lines
kept 96,219 (oldest `053782`), and server RSS grew 42 → 130 MB.

Unchanged under the lock: the Engine checkpoint and `session.read_scrollback` now
export only the newest 10,000 history rows (12–35 ms; at 127k rows they took
160–370 ms). Find capture is bounded by `FIND_CAPTURE_MAX_CELLS` and costs the
same (1–2 ms). Feed throughput, oldest-page fetch (50–150 µs at any depth),
attach seed and the wheel are within noise of base.

Costs that scale with depth (`terminal_interactions`, 100,000 lines, 160×50):
the first column change after a burst scans the new history once, 7 → 61 ms
(ASCII) and 18 → 74 ms (CJK); later drag steps go from p50 55 to 184 µs because
deferred lines are re-counted per width. The first deep read after a column
change reflows deferred history once: 91 ms at 127k agent-transcript rows. Every
gated p95 stays under 8.3 ms. History grows by recycling the cells of the
newest sealed block, so the growth phase does not allocate a row per line.

A column change now keeps stored rows when only the oldest block is partial.
Before this, a terminal at capacity indexed every line, and that index could
evict half of its history. Deferring wrapped lines still costs about 8 bytes
per line within the budget: narrowing and widening an 80-column agent
transcript at capacity kept 96,015 of 127,548 rows (base kept 6,429 of 10,000).
Hard-line output loses nothing. Measured while other agents loaded the machine
(load average 18–30); timings are single runs, alternated base/branch.
## `cat` of a 100 MB log, end to end through a held session (2026-10-27)

`catbench` (new example) measures the path a user waits on: a private Engine
`ControlServer`, a private Holder manager, a held session that `cat`s the
file, and an attached data-channel client that decodes every grid frame
until its grid shows the file's last line. The child waits on a FIFO before
`cat` and reports `cat`'s return on another, so the PTY drain and the
client's final grid are timed separately. Nothing touches an installed app.

Payload: 104,857,622 B, plain, SGR/truecolor and CJK lines in rotation, at
160×50. Apple M4 Max, release builds, load average 15–27 from unrelated
builds. Eight interleaved rounds; each row ran once per round. "Reference"
is a reference terminal multiplexer on an isolated server with one attached
data stream; its stream carries raw PTY bytes, so its column is the time the
last byte reached the client, before that client parses anything.

| Path | Drain, median (min) | Last line at client, median (min) | CPU |
| --- | ---: | ---: | --- |
| Raw PTY, Python 64 KiB reads, no parsing | 0.560 s (0.528) | — | reader 0.33–0.37 s |
| Reference, attached | 0.687 s (0.664) | 0.687 s (0.664), unparsed | server 0.78 s |
| Diri `main` | 0.690 s (0.609) | 0.793 s (0.714) | Engine 0.79 s, Holder 0.53 s, client 0.002 s |
| Diri branch | 0.675 s (0.605) | 0.847 s (0.718) | Engine 0.83 s, Holder 0.53 s, client 0.002 s |

The branch difference is inside the noise: the medians moved both ways
across repeated A/B/B/A batches, and the minimums agree. Earlier figures of
"~71 MB/s through a PTY" and "~118 MB/s parse" came from a single-thread
harness on a machine at load 60–140; the real path, quiet, finishes in
0.71–0.73 s (144–147 MB/s).

**Where the time goes.** The Engine session thread is the bottleneck: its
CPU time equals the time to the final grid (0.71 s of 0.72 s in quiet runs),
while the Holder drains the PTY ahead of it.

- Engine session thread, `sample` of the real path: 91% in
  `feed_with_history`, 3% in the OSC notification scan, 2.6% in the exit-marker
  scan (removed below), the rest in frame reads.
- Inside the feed, history storage is half the work. `feedbench` on the same
  log: 19.8 G instructions, 110 MB/s CPU with the 10,000-row compact history;
  10.0 G, 212 MB/s with history off. Of the history half, timed per call:
  row encoding 332 ms, LZ 64 ms, recycling resets 58 ms per 100 MB. Every row
  that scrolls past is encoded once and evicted 10,000 rows later, about 7 ms
  into this burst.
- Parsing proper (VT state, SGR, UTF-8, damage and fingerprints) is the other
  half; `Term::input_ascii`, `Term::input` and `from_utf8` lead it.
- Attach pump: about 2% of one thread, 40–52 grid frames (2.1–2.8 MB) per
  100 MB. Client decode: 2–3 ms. Diff construction is not a cost here.
- Holder: 0.45–0.55 s, 85% of it system time. A macOS PTY read returns
  about 1 KiB, so 103k batches each paid one `poll`, one read and one
  `EAGAIN` read (instrumented count), plus a queue wake.

**Changed.** The exit-marker scan that every output byte passes through in
the Engine pump now finds its `]` anchor with `memchr` instead of a
byte-at-a-time `position`; it leaves the profile. `memchr` was already in the
dependency graph through `diri-terminal-state` and the vendored parser. A
randomized test checks the scan against a window-by-window search.

**Measured and not done.**

- Single-pass history encoding (runs, text and UTF-8 in one walk, cheaper
  default-cell and same-style checks): −2.7% instructions, +1% cycles. No gain.
- Encoding history lazily cannot skip work unless all 10,000 rows stay raw
  (about 39 MB at 160 columns per streaming session), which is the per-session
  memory this design exists to avoid.
- Skipping diff construction while output outruns publication: diffs already
  cost ~2% and only the latest grid is published.
- Fewer Holder syscalls: blocking reads would lose the exit wake; lingering
  for more bytes adds output latency.

Footprint: none. No thread, timer, buffer or idle work was added.

`scripts/terminal-perf-gate.sh` passes on the branch: held input-to-grid
median 177 µs, local 72 µs, Holder input stream p50 12 µs / p95 19 µs.

```sh
cargo build --release -p diri-engine --bin diri-holder --example catbench
DIRI_HOLDER_BIN=target/release/diri-holder target/release/examples/catbench <log> 160 50 3
```

## Resource growth over a long session: explained, not leaked (2026-09-30)

Local telemetry from one 0.8.10 install (4.7 h, one `health` sample a
minute) showed the App footprint 394 → 446 MB, Engine footprint 62 → 105 MB,
Engine threads 47 → 59 and descriptors 176 → 204. Only aggregates were read.

### What the telemetry says

Engine threads and descriptors track the live gauges exactly. A least-squares
fit over all 282 samples:

- threads = 10.2 + 1.00 × records + 1.83 × clients + 0.23 × attached
- fds = −22.6 + 6.05 × records + 2.46 × clients + 1.55 × attached

Residuals stay within ±0.8 for the whole run apart from single-minute
transients (worst 3.7, gone the next minute). They do not drift with time or
with the ~2,770 `client.hello` connections, 57 Helper probes, 14 freezes or 8
wakes. Start: 31 records, 3 clients, 2 attachments. End: 32 records, 8 clients,
7 attachments. That accounts for all of +12 threads and +28 fds. Each warm
terminal pane is one attachment: roughly 2 Engine threads and 4 descriptors.

Engine footprint swings from 80 to 175 MB within minutes. Its 10th
percentile per half hour rises 62 → 80 MB over the first 2.5 h and then stays
at 80–82 MB. That is caches filling to their bounds (below), not a slope.

App threads (12–17) and descriptors (16–22) are flat. Its footprint median
per half hour goes 384 → 414–473 MB as `terminal_panes` goes 4 → 8, while RSS
falls (median 130 → ~100 MB) and the floor twice drops to ~150–200 MB when the
system reclaims. The growth follows warm workspace panes, which are capped.

### Reproduction: every churn path, private Engines

Release `dirijord-rs`, `diri-holder` and `dirijor` from origin/main
(`ab2b715e`), each run with its own `HOME` under `/private/tmp` and uploads off.
Threads from `ps -M`, descriptors from `lsof`, live heap from `heap -s`, and
growth attributed by diffing `malloc_history -allByCount` snapshots under
`MallocStackLogging=1`. Counts are after a warm-up. "Heap" is live
allocations added per cycle.

| Churn | Cycles | Threads | fds | Heap per cycle | Where the heap went |
| --- | ---: | --- | --- | ---: | --- |
| Connect, Hello, `hook.report`, close | 1,000 | 9 → 9 | 8 → 8 | 0 | — |
| `dirijor hook` processes | 500 | 9 → 9 | 8 → 8 | 0 | — |
| `events.subscribe`, close | 500 | 9 → 9 | 8 → 8 | ~0 | — |
| Spawn, exit, remove | 100 | 10 → 10 | 9 → 9 | ~1 KB | event ring |
| Spawn, kill, remove | 1,000 | 9 → 9 | 9 → 9 | ~1.2 KB | event ring (all 5,399 net allocations) |
| Attach/detach, producing session | 500 | 14 → 14 | 29 → 29 | ~1 KB | event ring, terminal history |
| Attach/detach, idle / hibernated session | 250 each | 14 → 14 | 29 → 29 | ~0.2 KB | event ring (`mark_seen` update) |
| Preview + preview-set open/close | 500 | 14 → 14 | 30 → 30 | ~0.6 KB | terminal history only |
| Hibernate + wake | 500 | 14 → 14 | 30 → 30 | ~2 KB | event ring (all 2,432 net allocations) |
| Real-session hooks (`dirijor hook`) | 1,000 | 11 → 11 | 15 → 15 | up to 10 KB | event ring; each `session.updated` grows with the record's attention history |
| List + workspace snapshot + history per connection | 500 | 18 → 18 | 50 → 50 | none (fluctuates) | — |
| Remote spawn/exit over a fake `ssh` + Helper re-probe (in-process) | 100 | 1 → 1 | 4 → 4 | — | — |
| Holder manager over 150 spawn/exit | 150 | 3 → 3 | 6 → 6 | 0 | — |

No path left a thread or descriptor behind. All heap growth was attributed to
bounded structures.

### Bounds (what the growth converges to)

| Structure | Bound | Notes |
| --- | --- | --- |
| Engine event replay ring | 4,096 events / 8 MiB | Every `session.updated` is retained until evicted, so the ring's resident size depends on record size |
| Engine terminal history per session | 4 MiB compressed | `HISTORY_STORAGE_BUDGET_BYTES` |
| Attention history per record | 200 events (~125 B JSON each, ~25 KB) | Serialized into `state.json` and into every `session.updated` for that session |
| Warm workspace panes per window | 16 | Each is an App pane plus an Engine attachment (~2 threads, ~4 fds) that keeps streaming |
| App parked grids / resident terminals | 12 / 1 | `PARKED_GRID_CAP`, `TerminalResidency` |
| Engine per live record | 1 thread, ~5–6 fds | pump thread; output log (2), attention SQLite, holder socket, kqueue |
| Message receipts | 100,000 rows | SQLite, on disk |

### Guard

`cargo test -p diri-engine --test resource_growth` runs 40 cycles of hook
connections, attach/detach on an idle and a producing session, subscriptions,
hibernate/wake and spawn/kill/remove against a holder-backed control server.
It requires threads and descriptors to return to their post-warm-up baseline.
It passes on main because nothing leaks there. As a check that it can fail, one
leaked descriptor per connection (`mem::forget` of a stream clone in `serve`)
fails it with "descriptors grew from 32 to 152". A forwarder that ignores its
stop flag does not fail it, because the next published event's failed write
ends that thread anyway.

Not claimed: anything about GPU/IOSurface memory in the App footprint (the
test platform has none). There is no heap assertion in the guard, because the
event ring legitimately grows for its first 4,096 events. Not done: shrinking
the bounds above. The warm-pane cap and the attention history size are product
decisions.
Related observation, by design: in the remote probe, dropping an exited
remote Session without terminating it left its Holder running with the final
grid (100 cycles, 100 idle Holders on the loopback host). A Holder is meant to
outlive a detach. The Engine's `session.kill`/`session.remove` path
(`Session::terminate`) stops it explicitly, including when the Agent already
exited. The user's run had no remote sessions (`sessions.remote` = 0
throughout).
## Real-use frames and keystroke echo, from telemetry (2026-09-30)

**What the telemetry says.** 4.7 hours of the author's installed 0.8.10
(local spool, aggregates only):

- `ui.frame`: 264,441 frames, mean 4.3 ms weighted by frames. In busy
  minutes p50 is in the ≤ 4 ms bucket and p90 in ≤ 8–12 ms. About 2,000
  frames a minute arrive steadily, GPUI's 30 fps cap for a window that is not
  key, fed by busy terminals (`term.paint` 0.8 per frame, 0.9 ms mean). The
  worst per-minute p90, 22 ms, was a minute with 3 frames. Two frames
  reached 50 ms (70 and 229 ms). The mean is the same frontmost (4.15 ms) and
  not (4.26 ms).
- `input.echo`: 2,419 keys, mean 7.2 ms weighted by keys. Minutes with 20 or
  more keys have means of 4.7–12.5 ms and p90 ≤ 8–16 ms. The minutes with
  p90 of 45–123 ms had 1–8 keys each: an agent answering after a pause, not
  typing. (17 ms is the unweighted mean of per-minute means.)

Neither metric could say how much was Diri: `input.echo` ran from input to
grid applied, through the agent, and `ui.frame` was one number.

**Where the installed app's frame time goes.** A 10 s, 1 ms `sample` of the
installed app (read-only, drawing about 33 fps with 8 panes mounted): the
main thread spent 1.29 s in GPUI's `Window::draw`, plus 0.37 s presenting,
0.28 s of it waiting in `-[CAMetalLayer nextDrawable]` (outside `ui.frame`).
Within `Window::draw`:

| | share |
| --- | ---: |
| terminal element prepaint + paint | 20% |
| Taffy layout of the root tree | 14% |
| `Scene::finish` sorting glyph sprites | 11% |
| `SidebarRow::eq`, walking pull requests field by field | 8% |
| accessibility tree (AccessKit) | < 1% |

AccessKit frames in the sample mean assistive technology is attached: any
app that reads other windows (window managers, dictation tools) turns it on.
Then GPUI re-renders every cached view nested in one that re-renders, so
each sidebar tick rebuilds all 51 rows, not just the working ones.

**A fixture that looks like that.** `real_use_frame_distribution` (root.rs,
ignored, macOS) mounts the real RootView under headless Metal at 1728×1080
(2×). It has 51 sessions, four working, every third with one to three pull
requests (description, 10 checks, 40 comments). A workspace tab is split into
three busy agent panes: one streaming a scrolling screen at 30 Hz, two
spinners at 10 and 8 Hz. The sidebar ticks its 125 ms activity mark, and the
store publishes every 200 ms. Everything is due on a 120 Hz tick, and each
tick with work draws one frame. `DIRI_BENCH_A11Y=1` attaches pretend
assistive technology through a new vendored-GPUI test hook;
`DIRI_BENCH_TABS=horizontal` uses the strip. GPUI's new `FrameStats` splits
each frame by phase.

The fixture's frames cost what the live frames cost in composition, but not
in time. On the performance cores it draws in about 1 ms. Under
`taskpolicy -c background`, which confines it to the efficiency cores, it
reproduces the live distribution's shape: p50 6.6–8.4 ms, p90 17–19 ms, p99
27–39 ms, max 50–104 ms. The author's Mac runs at load average 30–120 from
agents and builds. So the likely explanation for the rest of the gap is
where and when the main thread runs, not a structure the fixture lacks. This
is a hypothesis. The new `ui.frame.cpu` tests it (below).

With pretend assistive technology, views rendered per frame rise from 6.8 to
39.0, CPU per frame by 16–22% and p90 by 29–43% (base binary).

**Frame changes.** Both leave every comparison result and draw order as they
were.

- `SidebarRow` equality tests the record's `Arc` pointer first. Every
  sidebar render compares each row's props with the last ones, and an
  unchanged session keeps its `Arc`. Before, each comparison walked the
  record, including every pull request's description, checks and review
  threads.
- `Scene::finish` sorts `(key, index)` pairs and moves each sprite once
  along the permutation's cycles, instead of stable-sorting the 100+ byte
  sprites themselves. Ties keep their paint order, as the stable sort did. A
  frame already in order moves nothing.

Base is this branch's instrumentation commit, so the benches are identical.
Base and branch alternated three times each. Release builds, load average
78–120 (unrelated builds). Draw time is GPUI's whole `Window::draw` per
frame, 1,440 frames per run:

| Real-use fixture | Base | Branch |
| --- | ---: | ---: |
| Sidebar, draw p50 / p90 | 1.11–1.39 / 1.82–2.53 ms | 0.98–1.42 / 1.65–2.80 ms |
| Sidebar, CPU per frame | 1.27–1.53 ms | 1.13–1.57 ms |
| Sidebar + a11y, draw p50 / p90 / p99 | 1.16–1.40 / 2.57–3.26 / 3.19–7.25 ms | 1.01–1.12 / 2.38–2.62 / 2.83–3.41 ms |
| Sidebar + a11y, CPU per frame | 1.56–1.87 ms | 1.40–1.52 ms |
| Horizontal strip, CPU per frame | 0.94–1.02 ms | 0.86–1.01 ms |
| `finish` phase (scene sort) per frame, all three | 0.24–0.40 ms | 0.15–0.29 ms |
| Efficiency cores + a11y, draw p90 / p99 (720 frames) | 17.0–19.3 / 26.5–39.1 ms | 15.7–16.5 / 25.1–36.2 ms |
| Whole-process user CPU, sidebar + a11y run | 2.49–3.05 s | 2.26–2.46 s |

In stack samples of the fixture, `Scene::finish` fell from 13.6% to 8.3% of
the draw. The fixture's pull requests are illustrative: there, row comparison
was 0.6% of the draw, where the live sample put it at 8%.

Final frames of all three modes are pixel-identical between base and branch
(3456×2160). `the_sprite_sort_matches_a_stable_sort_by_key` checks the sort
against the standard library's stable sort over 36 size and key mixes.
`a_row_holding_the_same_record_is_equal_without_walking_it` uses a record
that is not equal to itself field by field (a NaN timestamp) to show the
walk is skipped.

**Keystroke echo: a TUI answers in parts.** Claude Code and Codex both wrap
each redraw in a synchronized update (DECSET 2026). Both binaries contain
`\e[?2026h`. The held Session pump published only output that answered a
keystroke without losing cells, and only its first write. So:

- A synchronized redraw that removed cells (the placeholder and "? for
  shortcuts" vanish as typing starts, a completion menu closes) looked like
  a half-erased repaint and waited out the 8 ms output batch.
- A redraw in several writes (erase, redraw, status row) published its first
  answering write at once. The rest waited out the batch, and then the
  attach pump's two-publication interactive budget.

Now output that closes a synchronized update, and leaves none open, answers
the keystroke whatever it erased. (The vendored vte counts updates ended by
their ESU; see `vendor/vte/DIRI-PATCH.md`.) The direct-PTY batch stops
settling on such output too. A keystroke buys up to four immediate
publications within its 100 ms window instead of one, and the attach pump's
interactive budget grows to five. Streaming output after those is batched as
before.

`multi_chunk_redraw_timing` (tests/attach.rs, ignored) types into a fake
TUI in a real Holder, from key to the frame that completes the redraw.
Release, base and branch alternated six times each, load 24–80:

| Fake TUI answer | Base median | Branch median |
| --- | ---: | ---: |
| One synchronized redraw that removes 1 cell net | 9.15–9.30 ms | 0.17–0.30 ms |
| Three writes 0.5 ms apart (last lands ≈ 1.0 ms) | 10.31–10.50 ms | 1.36–1.58 ms |
| Three writes 2 ms apart (last lands ≈ 4.1 ms) | 14.01–14.25 ms | 5.13–6.12 ms |
| Three writes back to back | 0.19–0.33 ms | 0.21–0.64 ms |

The "last lands" times include Perl's `usleep` overshoot.
`a_synchronized_redraw_that_removes_cells_answers_a_keystroke_at_once`
asserts a median of 5 ms or less (base: 9.2 ms).

**New telemetry** (details in `TELEMETRY.md`). App: `input.echo.transport`,
`.apply`, `.paint` and `input.echo.<agent class>`; `ui.frame.cpu`,
`.layout`, `.prepaint`, `.paint`, `.terminals`, `.a11y`; counters
`ui.frame.views_rendered`, `.views_reused`, `.terminal_paints`,
`.shape_misses`, `.a11y_frames`. `ui.slow_frame` carries the breakdown, and
one over-budget frame (≥ 8.3 ms) is recorded every 30 s at debug severity.
Engine: `input.echo.engine`, `input.echo.engine.<agent class>`,
`input.echo.publish`.

**How to confirm after upgrading.** Read the `metrics` events in
`~/Library/Application Support/Dirijor/telemetry/spool` (`app-*` and
`engine-*`), or run `diri-debug local --since 1h`:

- The agent's share: `input.echo.engine.claude` / `.codex` (Engine wrote
  the key → the agent answered). Diri's share: `input.echo.publish` (expect
  a p50 in the ≤ 1 ms bucket), plus `input.echo.apply` and
  `input.echo.paint`, plus the sockets and the Holder
  (`input.echo.transport` minus `input.echo.engine` minus
  `input.echo.publish`).
- `ui.frame.cpu` against `ui.frame`. If they are close at about 4 ms, the
  main thread is doing the work on slower cores. If CPU is well below wall
  time, it is waiting behind other work.
- `ui.frame.a11y_frames / ui.frame.n` is the share of frames drawn with
  assistive technology attached. `ui.frame.views_rendered / ui.frame.n`
  shows what that costs.
- `ui.slow_frame` events name the phase (`layout_ms`, `prepaint_ms`,
  `paint_ms`), `terminals`, `views` and `a11y` of any frame over 50 ms.

**Not claimed:**

- any change measured on the installed app;
- GPU or present time, including the `nextDrawable` waits;
- the efficiency-core explanation (it is a hypothesis for `ui.frame.cpu` to
  test);
- a fix for assistive-technology frames. Keeping nested views cached then
  needs GPUI to replay their accessibility nodes; forcing the re-render
  (upstream's rule) is what keeps rows in the accessibility tree;
- Taffy layout and terminal paint, the two largest shares, which are
  unchanged;
- Engine echo timing for remote sessions (their grid path records none).

Reproduce from `diri/`:

```sh
cargo test --release -p diri-app --bin diri real_use_frame_distribution -- --ignored --nocapture
DIRI_BENCH_A11Y=1 cargo test --release -p diri-app --bin diri real_use_frame_distribution -- --ignored --nocapture
DIRI_BENCH_TABS=horizontal cargo test --release -p diri-app --bin diri real_use_frame_distribution -- --ignored --nocapture
cargo test --release -p diri-engine --test attach multi_chunk_redraw_timing -- --ignored --nocapture
```

`DIRI_BENCH_TICKS`, `DIRI_BENCH_PRS=0` and `DIRI_BENCH_SCREENSHOT=<png>`
tune the fixture.
## Remote usage polls only while the Usage page is open (2026-09-30)

`rpc.slow method=host.usage` was the author's most frequent slow RPC: 161 in
3 days (0.8.10, one host), p50 966 ms, p90 1.8 s, p99 4.7 s, max 11.9 s (a
one-off Helper upload after an app update). It is not a stall: `host.usage`
already runs on a background request thread, holds no lock across SSH, and
the client multiplexes requests, so nothing else waited on it. It was waste:
the App polled every host every five minutes for as long as it ran, while
remote usage is shown only on Settings > Usage (the sidebar's cost is local
only). Each poll paid a cold SSH connection, since the 60 s ControlPersist
expires between five-minute polls.

Measured from the author's Mac against the real host over Tailscale:

| | wall |
|---|---|
| cold `ssh true` | 410–630 ms (p50 430) |
| multiplexed `ssh true` | 130–150 ms |
| one fused probe + `usage` poll (after #577), cold | 516–564 ms |
| pre-#577 poll: cached probe, then `usage` (telemetry) | p50 670 + 285 ms |

The fused poll is still above the Engine's 250 ms `rpc.slow` threshold on
every call, and ~80% of it is the SSH handshake, which only a permanent
ControlMaster could remove (not permitted: masters are finite-lived).

Change: a view holds a `RemoteUsageViewer` while it renders the Usage tab
(released on tab change, close or drop). The poller waits for a viewer,
refreshes at once when the previous refresh is at least five minutes old,
and repeats every five minutes only while a viewer remains.

| App running, Usage page closed | before | after |
|---|---|---|
| SSH commands per host per day | 288 (576 before #577) | 0 |
| `host.usage` calls / `rpc.slow` per day | 288 | 0 |
| Data age when the page opens | ≤ 5 min (always polled) | shown from cache; refreshed at once if ≥ 5 min |
| Refresh cadence while the page is open | 5 min | 5 min |

Verified with paused-time pacer tests (`usage::remote::tests`) and the
Usage settings UI test (viewer held on the tab, released on leaving it).

## Agent hooks and remote usage polls (2026-09-30)

Real telemetry (4.7 h of the author's 0.8.10 use, aggregates only): 1,928
`hook.report`s and 2,844 `client.hello`s (one `dirijor hook` process and
Engine connection per Claude callback, 87% of them Pre/PostToolUse); six slow
`hook.report`s at 263–461 ms, **each** coinciding with a 530–556 ms
`session.remove`; `host.usage` 57 calls (the App's five-minute poll, one host)
at avg 1.15 s / max 2.6 s, and 119 `ssh.command`s at p50 500 ms — two per poll:
the exact-build Helper probe (`remote.helper_ready path=cached`, p50 733 ms)
and then the `usage` RPC.

Findings:

- `session.remove` holds the Registry through `terminate`, where the Holder
  sends TERM and waits up to 500 ms for the whole tree before KILL. The
  wrapper's exec'd interactive shell survives TERM, so every close takes
  ~550 ms. Claude runs its SessionEnd hook on TERM; `hook.report` waited on the
  Registry lock, so the hook never finished before the KILL (its SessionEnd was
  lost), and any other Agent's hook in that window stalled that Agent too.
- Uncontended, the Engine answers Hello in ~0.02 ms and `hook.report` in
  0.3–0.5 ms p50 (p95 5–10 ms when the debounced persist ran inline under the
  lock). The rest of a ~9 ms hook is `dirijor` process spawn/exec: `dirijor`
  retires ~20 M instructions, a trivial Rust `main` ~15 M, `/usr/bin/true`
  ~8 M. Dropping Hello would save ~0.02 ms and break the identity check, so it
  stays.
- The CLI read at most 1 MiB of hook stdin; a larger PostToolUse (a Read of a
  big file) became `{"raw": ...}`, so the report was dropped and the recovery
  seed lost its conversation id. Every Pre/PostToolUse also re-encoded
  `tool_input`/`tool_response` to the Engine, which never reads them there.
- A usage poll ran two login shells on the host (account layer + `~` layer)
  for three variables (`HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`).

Changes:

- `hook.report` applies inline only when the Registry is free and nothing is
  queued; otherwise it queues to one `diri-hook-applier` thread (bounded
  1,024, blocking backpressure, never a drop) and answers at once. Order per
  Agent is preserved (the inline path cannot overtake a queued report). The
  Registry write moves from `persist()` (inline fsync once 500 ms have passed)
  to `persist_deferred()` (the flusher, within the same 500 ms bound); the
  CLI still writes the lifecycle seed to the recovery store before delivery.
  New counter `hook.queued`.
- `dirijor hook` reads up to 8 MiB and forwards neither `tool_response` nor,
  except for PermissionRequest (the one event whose input the Engine
  summarizes), `tool_input`.
- A warm `host.usage` is one SSH command: `probe` (stdin closed) and, only if
  it exits 0, `exec usage` in the same `ssh -T` channel; the probe line is
  verified exactly like the separate probe before the response is accepted,
  and anything unverifiable takes the full bootstrap path (`path=fused` in
  `remote.helper_ready`).
- The Helper caches those three login-environment values in owner-only
  `usage-v1/login-environment.json`, valid while the account shell, HOME, the
  stat stamps of 27 standard shell startup files and a one-hour age match.

### Measurements

Apple M4 Max, macOS 27.0, release builds, load average 6–17. Base is
`ab2b715e` (origin/main).

`scripts/hook-bench.py <bin-dir> --pad 800` (private Engine, 40 Claude
sessions, 1.1 MB state.json, 400 `dirijor hook` processes, wall = fork to
exit; base and branch alternated 3× each, ranges):

| | Base | Branch |
| --- | ---: | ---: |
| Hook wall p50 / p95 | 8.8–9.2 / 13.9–14.8 ms | 9.2–9.6 / 14.8–15.5 ms |
| Hook wall p99 | 17.2–22.8 ms | 17.4–23.4 ms |
| Engine `hook.report` reply p50 / p95 (`--phases`, 2×) | 0.33–0.50 / 5.5–9.8 ms | 0.013–0.017 / 0.027–0.034 ms |
| Engine Hello p50 | 0.020–0.024 ms | 0.018–0.023 ms |

Same, with 8 sessions closed during the run (`--bg-remove 8`, 600 hooks, 2×
each):

| | Base | Branch |
| --- | ---: | ---: |
| Hook wall p99 / max | 549–558 / 566–573 ms | 16.2–16.4 / 23–27 ms |
| Closing Agent's SessionEnd hook completed | 0 of 6 (killed) | 6 of 6 (8–114 ms) |
| `session.remove` | 543–569 ms | 550–593 ms (unchanged: TERM escalation) |

A re-run of `scripts/hook-bench.py` at load average 300–400 reproduced it:
hook wall p99 564 ms (base) vs 27 ms (branch).

`cargo test --release -p diri-remote --test usage_bench -- --ignored
--nocapture` (fake `ssh`, 400 Claude + 100 Codex transcripts × 200 lines,
Helper preinstalled; remote CPU is every child's user+sys; five unchanged
polls, then one append; `USAGE_BENCH_RC_SLEEP=0.15` puts `sleep 0.15` in the
shell startup files):

| | Base | Branch |
| --- | ---: | ---: |
| SSH commands per poll | 2 | 1 |
| Warm poll, bare shell: wall / remote CPU | 85–106 / 48–58 ms | 38–40 / 25–26 ms |
| Warm poll, 150 ms startup files: wall / remote CPU | 390–402 / 48–53 ms | 53–65 / 29–38 ms |
| Poll after an append | 90–391 ms | 49–64 ms |
| Cold poll (first scan) | 277–597 ms | 291–624 ms |

Totals are identical between base and branch and between cold and warm polls
(the bench asserts warm = cold and append = cold + 7 tokens).

Not claimed: SSH round-trip time. The fixture has none; on the real host each
`ssh.command` was ~0.5 s (a new master connection per poll, since the
five-minute poll outlives `ControlPersist=60`), so the removed round trip is
the larger real saving. Not changed: the five-minute App poll itself (the App
polls even with the Usage page closed; left to the App), `session.remove`
holding the Registry through the 500 ms TERM escalation (hooks no longer wait
on it; other Registry users still do), and process spawn cost per hook.

### Fleet follow-up: every slow `hook.report` is a Registry wait

Three days of uploaded telemetry from three installs (0.8.9 and 0.8.10, both
before the change above) hold 52 `rpc.slow method=hook.report`, 260–688 ms,
median ~450 ms. Each one lines up with something else holding the Registry:
50 finish within one `session.remove` (43), `session.kill` (5) or
`session.archive` (2), the TERM escalation above, which archive and kill also
go through. The remaining two (giga,
496 ms and 688 ms) overlap no RPC: one is the UserPromptSubmit of a session
whose tracked spawn was still delivering its prompt, the other a lone
PreToolUse. The pre-change handler waited on the Registry mutex whoever held
it, and then ran `persist()` under it. The change above never waits for any
holder, so it covers these two as well. No other cost showed up: on this Mac
`fdatasync` on the state volume is ~0.02 ms p50 / 0.1 ms max over 300 appends
(so the activity log's per-transition `sync_data` is noise). Hello is
~0.02 ms, and Hello plus `hook.report` measured from a raw client *during*
the background removes stays at or under 17 ms at the maximum.

A/B with `scripts/hook-bench.py --pad 800 --sessions 30 --bg-remove 8 --hooks
300` on a loaded machine (other agents compiling), `f47c8203^` vs `f47c8203`,
three runs each:

| | Before | After |
| --- | ---: | ---: |
| Hook wall p99 | 567–653 ms | 65–118 ms |
| Hook wall max | 589–902 ms | 127–271 ms |
| Engine `rpc.slow method=hook.report` in the fixture spool | present | none |

The remaining wall tail after the change is process spawn on a loaded machine
(the same run's raw-client Hello + `hook.report` p99 was 14 ms). Quiet runs
(no removes) are equal: p99 24 vs 27 ms.

Uploaded metrics had only the all-methods `rpc` timing, so the full
hook distribution was invisible below the 250 ms `rpc.slow` bar. The Engine
now also records `rpc.hook_report` (the reply the Agent waits on) and
`hook.apply_wait` (how long a queued report waited for the Registry, which is
now status staleness rather than Agent latency).

In a deliberately extreme run of the same bench (`--hooks 3500 --bg-remove
60`, so the Registry is held about two thirds of the time), one 60 s metrics
window read `rpc.hook_report` n 3,896, p99 8 ms, max 54 ms, with no
`rpc.slow` for it. It also read `hook.apply_wait` n 3,430, avg 421 ms, max
4.0 s. Once one report is queued, every later one queues behind it until the
applier catches up. The Agent no longer waits, but status can lag by the
length of a close. This goes away only when `session.remove` stops holding
the Registry through the TERM escalation.

## Telemetry truth: `pane.first_paint` tail and `workspace.mutate` errors (2026-09-30)

Read from 4.7 h of the owner's local telemetry spool (0.8.10), aggregates only.

**`pane.first_paint` at ~10 s (14 of 57) was a measurement artifact.** The
pane recorded first paint from its own `render` and from the 10 s blank
watchdog (`check_blank` called `trace_first_paint`). A resident can be
mounted by a pane that is never drawn: the selection-following pane stays
alive and attached while a workspace workbench is shown in its place, warm
workbench panes of other tabs, a window the system stopped drawing. Such a
pane never renders, so at 10 s the watchdog found content in the shared grid
and recorded a "first paint" nobody saw. Evidence in the data: in 4 of the
14, the same session had already recorded a first paint 0.4–0.6 ms after its
mount in another pane, with `grid_ms` of 8–32 ms in the undrawn one; the other
10 were remounts onto parked grids (`parked=true`) with no render for 10 s.
Headless repro on origin/main: a `TerminalPane` held by a view that does not
draw it, with content in its grid, reports `painted=true` after the watchdog
while its element has 0 painted frames and the pane 0 renders.

Now first paint is taken inside `TerminalElement::paint` (a one-shot callback
armed per mount, fired by the first frame that draws a non-blank grid), and
the watchdog never records a paint. `pane.blank` is only reported for a pane
drawn at least once since its mount, and with `content=true` when the grid
holds content that was never painted, which is the signature of a real
missed repaint (the class of the old "blank until resize" bug). Cost per
paint: one relaxed atomic add and one uncontended mutex lock; the grid is only
scanned while a callback is armed, once per mount, and the scan stops at the
first non-blank cell. Not claimed: that no real stall happened in that data;
only that none of the 14 samples shows one, and that one would now be
reported as `pane.blank content=true`.

**Follow-up (the one `pane.blank frames=0` incident, 0.8.9).** A reopened
Codex session was mounted by two panes sharing one grid 19 ms apart: one
painted it 2.4 ms after its mount, the other received the grid and was never
drawn with it (`frames=0`), and 0.8.9's watchdog reported that one. The #576
"drawn since mount" test covers a pane never drawn, but not one drawn once
while its grid was still blank and covered since: headless, that pane was
reported as `content=true`, a false missed-repaint incident. The watchdog now
asks for a frame at 10 s and reports only if that frame draws the pane within
500 ms; a real missed repaint is still reported, with `redrawn=true` when the
requested frame put the waiting content on screen.

**`workspace.mutate` → `invalid_workspace` (19 of 57 mutations in the current
spool files) was the 256-tab limit, already fixed on main by #575.** Every failure
coincided with attaching one of 5 sessions, each failing on nearly every
activation (9 of 10, 6 of 6, 3 of 3, …); the 8 sessions that never failed
were ones the layout already held. Activation sends `openProjectAgent`,
which only adds a tab for a session without one, and in 0.8.10 a tab count
over `MAX_WORKSPACE_TABS` was reported as `invalid_workspace`. Tabs of deleted
sessions were never reclaimed, so new agents could not be placed. #575
reclaims them at the limit and reports a real limit as
`workspace_limit_reached`. A randomized sequence of app-shaped mutations
(openProjectAgent with preferred layouts, create/split/move/swap/zoom/focus/
remove; 400 seeds × 60 edits) against the current Engine found no other
`invalid_workspace` besides a self-dock the app already refuses to send. One
more app-side sender was found by reading: dropping a tab on its own
workspace's header sent `index = tabs.len()`, which is out of range once the
Engine takes the tab out; it now sends the last slot.

## Cheap column changes: deferred history reflow (2026-09-29)

#561 made a drag stop blocking other sessions, but each column change still
reflowed 10,000 wrapped history rows by decoding, reflowing and re-encoding
the whole compact history (8 ms p50, `encode_cells`/`lz::compress` ~57%,
decode ~20%, memmove ~20%). A window drag kept the Engine about a core busy,
and a Remote Helper did the same for each Resize it received.

Changes:

- History reflow never moves cells across a hard line break, so compact
  storage now reflows only the editable rows (screen, newest history, any
  line ending there) and **defers** older logical lines. A line whose reflow
  is greedy placement of its content (almost every output line, including
  wide characters) is stored as 8 bytes and its rows at any width are counted
  without decoding. Other lines (styled trailing blanks, tabs, broken wide
  characters) are kept as rows and reflowed by `Grid::resize` itself on a
  one-line grid, up to 256 KiB. Rows are built from the compressed payloads
  when history is read or edited, once. Recycling and dropping the oldest rows
  need no decoding. Details and the exact conditions are in
  `vendor/alacritty_terminal/DIRI-PATCH.md` ("Deferred column changes").
- Growing now takes kept-back history into the screen when merged rows leave
  too few, as dense storage does. The hard-line case used to append blank rows
  instead (a pre-existing compact/dense divergence).
- The Remote Helper applies only the last of adjacent valid Resize frames in
  one read, like the Engine's attach reader.

### Measurements

Apple M4 Max, macOS 27.0, release builds, load average 4.5–9.4. Base is
`923a98a` (origin/main + #554 + #561). Base and branch alternated three times
each; ranges over the runs are shown.

`cargo bench -p diri-terminal-state --bench terminal_interactions` (160×50,
10,000 rows of the coloured build log with wrapped lines, one resize + grid
update per step; CPU is this process's user+sys from `getrusage`):

| | Base | Branch |
| --- | ---: | ---: |
| Drag step, ASCII, p50 / p95 | 7.8–8.1 / 9.9–10.1 ms | 54–62 / 60–69 µs |
| Drag step, CJK/emoji, p50 / p95 | 8.9–9.8 / 11.2–11.7 ms | 95–97 / 100–104 µs |
| First column change (max), ASCII / CJK | 16.5–17.4 / 13.3–15.2 ms | 5.4–6.8 / 6.6–6.7 ms |
| Drag 160→100→160 as 60 changes, CPU, ASCII | 417–424 ms | 8.3–8.6 ms |
| same, CJK/emoji | 523–527 ms | 12.1 ms |
| Allocations per step, ASCII / CJK | 26,598 / 27,756 | 154 / 173 |
| 20,000 lines streamed after the drag, CPU, ASCII / CJK | 15.5–16.1 / 18.8–19.7 ms | 15.7–16.1 / 19.3–19.6 ms |
| 380×110 (no history row wraps), 60-change drag CPU | 11.5–11.8 ms | 11.3–11.4 ms |

The first change of a drag scans history once (decode, no encode); later
changes only count lines. `--gate` now also gates the drag step p95 at the
8.3 ms frame budget.

Engine, real Holders (`cargo test --release -p diri-engine --test
interactions -- --ignored --nocapture --test-threads=1 drag`: 161 Resize
frames over 1.3 s while another session is typed into every 20 ms):

| | Base | Branch |
| --- | ---: | ---: |
| Engine CPU for the drag | 1.21–1.24 s | 129–160 ms |
| Last resize → grid at final size | 7.7–17.2 ms | 0.37–12.2 ms |
| Other session's echo, p95 | 6.7–10.4 ms | 0.38–1.2 ms |
| Grids published to the dragged pane | 88–94 | 128–136 |

Memory: `terminal_fleet` gates pass (fresh/full-history heap +80 bytes per
core). While history is deferred it keeps its line index: +110 KB for 10,000
ASCII log lines (compressed history 375 KB), +190 KB when every third line
has wide characters, until history is read or edited.

Equivalence: `transcript_digest` and the new `reflow_digest` probe (drags
160↔100, jumps 300/40/13/2 columns, output and coloured-template scrolling
between resizes, alternate screen, paged, full and no history reads) print the
same digests on base and branch. The vendored tests compare against dense
storage over random output/resize sequences (600 seeds fuzzed) and against
eager compact reflow.

Not claimed: RSS or renderer effects; byte-budget-limited histories (their
retained rows can differ after a resize, since the stored representation
differs). Not done: a payload-level shape scan to make the first change of a
drag cheaper (~5 ms); per-thread stack samples of the Engine drag.
## Keystroke echo draws without waiting for the display link (2026-09-29)

**Where a keystroke's time goes.** `DIRI_LATENCY_TRACE=1` now stamps every
hop of a keystroke in the desktop client, from GPUI delivering the key to the
drawable reaching the screen (see `diri_client::latency_trace`; zero cost when
unset). The headless harness drives the real `TerminalPane` key handler,
the production client and transport, a private Engine with a real Holder
running `cat`, and GPUI's real headless Metal renderer. Release build, 300
paced keys (every third on a line a backspace), three runs, load average 7–9:

| hop | p50 | p95 |
| --- | ---: | ---: |
| key down → input queued (encode, lease, `try_send`) | 0.009–0.010 ms | 0.013–0.014 ms |
| input queued → socket written (attachment task) | 0.019 ms | 0.031–0.046 ms |
| socket written → echo decoded (Engine, Holder, PTY) | 0.184–0.190 ms | 0.30–0.36 ms |
| echo decoded → pane mailbox | 0.005 ms | 0.006–0.007 ms |
| mailbox → grid applied (GPUI thread) ¹ | 0.005 ms | 0.008–0.011 ms |
| grid applied → pane notified | 0.004 ms | 0.006 ms |
| pane notified → draw start ² | 0.001 ms | 0.001 ms |
| draw (pane-only window) | 0.216–0.232 ms | 0.29–0.31 ms |
| draw end → Metal commit | 0.035–0.036 ms | 0.045–0.062 ms |
| commit → GPU completed | 0.37–0.41 ms | 0.83–0.94 ms |
| **key down → GPU completed** | **0.90–0.94 ms** | **1.97–3.26 ms** |

¹ The harness pumps a test dispatcher in a busy loop, so this is not the main
run loop's queueing. ² A headless window draws in the effect flush that dirtied
it; the real app waits for the display link here (below).

`workspace_terminal_echo_redraw_cpu` lands the same echo in a whole
1600×1000 workspace window (sidebar, strip, workbench): 0.88 ms apply+draw
p50, 1.0 ms CPU per echo. It is not a whole-window re-render: the sidebar and
strip replay from cache, and a sample puts 56% of the loop in
`TerminalElement::paint` shaping and placing every glyph of the 160×50 grid,
which GPUI repaints in full each frame.

So Diri's own path from key to a finished frame is about 1–2 ms. The rest is
waiting: the echo is applied at a random moment and GPUI drew only on the next
`CVDisplayLink` tick, then the compositor shows the frame.

**What changed.** A keystroke's first screen change asks for its frame at
once. `Window::request_immediate_frame` (vendored GPUI) merges one request
into the window's display-link dispatch source, so the frame runs as soon as
the main thread is free, through the same `step` path. macOS refuses it while
the last present is less than two refresh intervals old: with two drawables
that frame may still be queued, and a new one could block in `nextDrawable` or
stack two frames into one refresh. `AttachmentControl::take_echo` grants one
immediate frame per keystroke, within the 350 ms `KEYSTROKE_WINDOW`. Output
nobody typed for, streams, and animations stay on the display link. A cursor
glide in progress keeps frames flowing, so the request is refused then and the
echo rides the next tick as before.

**How it was measured.** `echo_frame_scheduling_against_the_display_link`
(gpui_macos, no window) ticks a dispatch source from a real `CVDisplayLink` on
this MacBook's 120 Hz panel (8.30 ms measured). A typist thread applies echoes
60–240 ms apart, each followed by an 80 ms glide of frames. Six alternating
runs of 150 keys:

| echo applied → frame starts drawing | run p50 | run p95 | all p50 | all p95 |
| --- | --- | --- | ---: | ---: |
| display link (before) | 5.28 / 4.57 / 4.28 ms | 8.81 / 8.33 / 8.36 ms | 4.67 ms | 8.43 ms |
| immediate (after) | 0.001 ms ×3 | 5.67 / 7.10 / 6.50 ms | 0.001 ms | 6.56 ms |

387 of 450 echoes (86%) drew immediately. The others landed during the
previous echo's glide. On a 60 Hz display the wait removed is twice as long.

**What the display pipeline already does.** The layer keeps two drawables
(`interactive_windows_keep_two_frames_in_flight`), presents without a
transaction except while resizing, and keeps `displaySyncEnabled`; turning
sync off would tear. `CVDisplayLink` ticks at the panel's 120 Hz with no
frame-rate request. #540 covers animations that ran below it.

**Not claimed.**

- Time on screen. The frame starts about 4.7 ms sooner at the median. When
  that lands it a refresh earlier depends on where WindowServer's compositing
  deadline falls in the interval, which only an on-screen window can measure.
  To measure it, run a dev build with `DIRI_LATENCY_TRACE=1` and type in a
  visible window. The `presented` hop, from `addPresentedHandler`, is printed
  every 100 keys. It was not run here: these agents may not open windows on
  this Mac.
- Main-thread queueing in the real app. The controller and the view each take
  one main-queue hop. Both are microseconds when idle.
- Any change to the Engine path or draw cost.

Reproduce from `diri/`:

```sh
cargo bench -p diri-terminal-state --bench terminal_interactions -- --gate
cargo test --release -p diri-terminal-state --test transcript_digest -- --ignored --nocapture
cargo test --release -p diri-engine --test interactions -- --ignored --nocapture --test-threads=1 drag
cd vendor/alacritty_terminal && DIRI_REFLOW_SEEDS=600 cargo --config 'patch.crates-io.vte.path="../vte"' test --release --features compact-history reflow
```

cargo test -p gpui_macos --release --lib echo_frame_scheduling -- --ignored --nocapture
cargo build --release -p diri-engine --bin diri-holder
DIRI_HOLDER_BIN=$PWD/target/release/diri-holder \
  cargo test --release -p diri-app --bin diri keystroke_latency -- --ignored --nocapture
cargo test --release -p diri-app --bin diri workspace_terminal_echo_redraw_cpu -- --ignored --nocapture
```

`only_a_keystroke_echo_asks_for_an_immediate_frame`,
`a_keystroke_buys_one_echo_frame` and
`immediate_frames_wait_until_the_last_present_is_on_screen` guard the policy.
## Horizontal strip tabs re-render only when they change (2026-09-28)

**Where the time went.** With horizontal tabs, the sidebar is hidden and
`RootView` paints the tab strip inline. RootView is the window root and is
never cached, so every window frame rebuilt, laid out, prepainted and painted
every tab of the selected project. That includes each terminal output frame,
each 125 ms working-mark tick and each store publication. #517 moved sidebar
rows onto cached views and left the strip as it was.

**What changed.** Each strip tab is now a cached view (`StripTabView`,
`sidebar/view/strip_tabs.rs`) that renders from a `StripTabProps` snapshot. It
uses the same pattern and the same vendored-GPUI nested cache as the sidebar
rows. A tab re-renders only when:

- its props change: selection, activity state or mark frame, title, kind,
  theme, held-⌘ hint, ordering mode, or whether the shared selection pill
  (#533) stands in for its fill;
- it is lifted or sliding in a drag reorder, or its title is settling. These
  tabs are forced every frame while they move;
- its own hover changes;
- the sidebar is notified for anything other than a store publication or its
  own animation tick. This is the `tabs_stale` safety net.

The selection pill is its own layer, drawn inline beside the tabs. Its glide
keeps moving on reused tabs, and its frames notify the sidebar without
marking tabs stale.

GPUI's cache key still re-renders tabs whose bounds or clip change, for
example while the strip scrolls or slides in. A drag refreshes the window on
every pointer move. In horizontal mode the activity tick no longer marks tabs
stale: the new mark frame reaches the working tabs through their props.

**How it was measured.** `strip_fleet_render_cost` (root.rs, ignored,
macOS) mounts the real RootView under headless Metal. It uses the fleet from
`sidebar_fleet_render_cost`: 51 sessions over five projects, four of them
working. Tabs are horizontal, the sidebar is hidden, and `bench-0` is
selected, so its project shows 11 tabs, one of them working. Brand marks use
the same cached blank raster stand-in.

A step is one notify followed by the frames it causes. At this bench that is
two root renders. The terminal output frame notifies the primary terminal
pane. "Strip render" is the strip's own render call plus the cached tab
renders.

Numbers are release builds, 1,000 steps, with base (`main` at `a1219ae`,
after #533, plus the bench commit) and branch binaries alternated three times
each. Load average during the runs was 40–70, which shows in the first two
base runs' p90 and CPU.

| 51 sessions, 11 tabs | Tabs built per step | Step median | CPU per step |
| --- | ---: | ---: | ---: |
| Terminal output frame | 22 → 0 | 0.596–0.600 → 0.310–0.312 ms | 0.653–0.752 → 0.322–0.395 ms |
| Activity tick | 22 → 1 | 0.595–0.601 → 0.330–0.331 ms | 0.615–0.767 → 0.350–0.446 ms |
| Store publication (nothing changed) | 22 → 0 | 0.593–0.597 → 0.309–0.311 ms | 0.617–0.752 → 0.320–0.381 ms |
| Root-only frame | 22 → 0 | 0.588–0.593 → 0.308–0.309 ms | 0.599–0.731 → 0.320–0.361 ms |

Whole-process CPU time (`/usr/bin/time -p`, user) for the full bench, which
is setup plus 4 × 1,020 steps, fell from 2.64–3.18 s to 1.45–1.76 s.

Most of the saving is in layout, prepaint and paint of the tab elements, not
in the render call: strip render time per step fell from 0.06 ms to 0.02 ms, a small part of the step.

Scaling was measured before #533 merged, with one alternating run per size,
terminal output frame step medians:

| Sessions (tabs shown) | Before | After |
| --- | ---: | ---: |
| 10 (2) | 0.276 ms | 0.222 ms |
| 51 (11) | 0.578 ms | 0.296 ms |
| 100 (20) | 0.852 ms | 0.361 ms |
| 250 (50) | 1.870 ms | 0.588 ms |

What remains per tab is computing and comparing props, a view and its
wrapper node, and replaying cached ranges.

**Visual checks.** Pixels match `main` exactly in these cases:

- `render_tab_orientation_screenshots` horizontal-dark, horizontal-light and
  horizontal-narrow, for the typical, stress and projects scenarios;
- the strip bench's final frame, after about 1,300 steps.

The fixture's `vertical-light` frame differs from run to run on `main` itself,
in the selected row's band (up to 6/255). It is excluded.

`reused_strip_tabs_paint_like_a_full_render` checks the reuse path. It runs 11
ticks, each interleaved with no-op publications or root frames and a terminal
frame. Midway it selects another tab and draws the pill's whole glide over
reused tabs. It then compares against a `window.refresh()` rebuild and
requires zero differing pixels. It passed 12 of 12 runs. With `force_render_if(changed)`
disabled, 216 pixels differ.

`strip_tabs_rerender_only_when_they_change`,
`sliding_strip_tabs_render_every_frame_until_they_rest`,
`the_pill_glides_over_reused_strip_tabs` and
`held_hints_show_and_leave_on_cached_strip_tabs` pin the re-render rules.

**Not claimed:**

- any installed-app CPU change;
- GPU or present cost;
- the strip chrome: the project control, peek and new-tab buttons, and the
  hosted title-bar actions are still rebuilt with every root frame;
- the rest of RootView's per-frame work.

## Terminal feed: scrolling without decompression, moved-row fingerprints (2026-09-28)

After #529, a stack sample of four Engine sessions draining a colored build log
through `fleetbench` put 44–56% of pump time in `HeadlessScreen::feed`, most of
it scrolling: `rotate_reset` decompressed the oldest history block to recycle
one row, built a new `Vec<Cell>` for it, and encoded each block as rows left
the screen. At small reads every scroll re-fingerprinted the whole screen, and
every settle walked every history block to measure stored bytes.

Changes. No wire, checkpoint or history-budget accounting change:

- Encoding a block records one bit per row: whether `Row::reset` of that row
  yields only template cells. Nearly all output lines qualify, and recycling
  them no longer decompresses or indexes the block. The bits fit in `Block`
  without changing its counted size.
- Recycled rows reuse the allocations of the rows the last seal encoded. Spare
  rows are dropped at each settle's history bound, so they are never counted
  and an idle terminal holds none.
- The payload bytes in `history_storage_bytes` are kept current as blocks are
  pushed and popped. Read caches are released only after a history read. The
  per-settle history bound no longer visits every block.
- The vendored `Term` tracks where each screen line's cells came from. With
  that, full damage from scrolling moves line sources instead of discarding
  them. `HeadlessScreen::settle` keeps the fingerprints of rows that only
  moved and hashes rows that anything wrote.
  - Because `input` relies on cursor damage, the cursor line is marked before
    a scroll moves it.
  - A wide character's leading-spacer edit on the line above the cursor is
    now marked; it was never damaged.
  - Rows left stale by such undamaged edits are re-hashed at the next full
    damage, exactly as before.

Details are in `vendor/alacritty_terminal/DIRI-PATCH.md`. VTE is unchanged.

**Lazy compaction (encoding history in larger batches, or when idle) was
measured and not done.** In a sustained stream every row that enters history
is encoded exactly once before the 10,000-row limit evicts it. Batching
therefore cannot remove encode work; it only holds more raw rows (about
3.9 KB each at 160 columns) against the memory gates, and idle-time compaction
would need a timer. Doubling blocks to 64 rows measured +2–5% cycles at 4 KiB
reads, because more spare rows were dropped per settle, and −1% at 64 KiB.

### Measurements

Apple M4 Max, macOS 27.0, release builds, load average 45–70 from unrelated
builds. Wall time was 2–4× CPU time, so this reports hardware counters from
`/usr/bin/time -l` (cycles and instructions retired) and process CPU time.
`feedbench` gained an optional fourth argument that runs one configuration,
so the process counters describe it; the base ran the same harness. Values
are medians of three alternating base/branch runs at 160×50. The base is
`f44bcf0`.

| Payload, read size | Gcycles, base → branch | CPU MB/s, base → branch |
| --- | ---: | ---: |
| Colored build log (64 MiB), 4 KiB | 2.94 → 2.53 (−14%) | 86 → 101 |
| same, 16 KiB | 2.34 → 1.92 (−18%) | 109 → 133 |
| same, 64 KiB | 2.17 → 1.72 (−21%) | 117 → 147 |
| same, one call | 2.14 → 1.65 (−23%) | 118 → 155 |
| same, Engine config (notifications), 4 KiB | 3.03 → 2.56 (−16%) | 83 → 99 |
| same, Engine config, 64 KiB | 2.26 → 1.81 (−20%) | 112 → 139 |
| `git log -p --color` (32 MiB), 4 KiB | 2.93 → 2.31 (−21%) | 44 → 55 |
| same, 64 KiB | 2.53 → 1.85 (−27%) | 50 → 68 |

Instructions retired, colored log: 15.46 → 12.96 G at 4 KiB and
10.63 → 8.58 G at 64 KiB.

A 64 KiB sample of the branch attributes feed time as follows:

- `encode_cells`: about 23%.
- LZ compression: about 16%.
- Resetting recycled rows: about 15%. The base also paid this, inside
  `reset_cell_row`.
- The parser's writes: about 15%.

Decompression and row allocation no longer appear. At 4 KiB, fingerprints of
newly written rows are about a quarter of feed time. With about 40 new lines
per 4 KiB read on a 50-row screen, 10 rows are reused.

**`fleetbench`** (4 sessions × 64 MiB, 160×50, sampled for the whole run,
three alternating runs):

| Metric | Base | Branch |
| --- | ---: | ---: |
| Benchmark process user CPU | 3.87–4.08 s | 3.34–3.44 s |
| Instructions retired | 57.3–58.6 G | 50.9–51.6 G |
| Cycles | 18.0–18.7 G | 16.2–16.7 G |
| `feed` share of pump-thread busy samples | 76–78% | 68–72% |
| `rotate_reset` share of `feed` | 68–69% | 55–58% |

- Aggregate throughput did not change measurably (97–102 vs 84–108 MB/s). That
  fixture is bound by the PTY and Holder path.
- System time rose from 0.95–1.05 s to 1.06–1.11 s, and voluntary context
  switches from 48–54k to 55–59k. This is consistent with faster pumps waiting
  on the PTY more often; it was not investigated further.

**`terminal_throughput`** (whole-binary counters, three runs): instructions
32.3 → 25.0 G (−23%), cycles 4.27–4.38 → 3.11–3.25 G (−26%). Its wall-clock
scrolling budget (150 µs) failed on both revisions under this load: base
196–269 µs, branch 137–186 µs, against 36 µs recorded unloaded.

**`terminal_parity`**, 10,000 lines per 80×24 core: feed p50 34.1/46.9 ms →
7.4/8.6 ms (two loaded runs each; one line per read, so skipping the
per-settle block walk and moved-row hashing both apply).

- Retained heap per core: 259,699 → 260,427 bytes.
- Peak heap per core: 324,211 → 324,939 bytes.

**`terminal_fleet`** gates pass. For 20 cores, fresh heap is 2,120,620 →
2,125,620 bytes and full history is 6,388,780 → 6,397,780 bytes; those are the
new per-row source and fingerprint vectors. Other results:

- Widened: 16.21 → 16.22 MiB.
- After churn: 11.46 → 11.47 MiB.
- Warmed cursor updates allocate nothing, and nothing leaks.

`scripts/terminal-perf-gate.sh`:

- `terminal_fleet`, the `vte` tests and the Holder latency, attach and
  output-compat tests pass.
- The `terminal_throughput` scroll budget and the `diri-term` renderer fling
  budget (8 ms; frames of 239–278 ms under load) fail on base and branch
  alike on this loaded machine.

### Equivalence

- `transcript_digest` prints the same final digest on base and branch
  (`96018b6b3bdda84f`), with identical checkpoints. Retained history rows are
  unchanged because accounting is unchanged.
- `fingerprints_after_scrolling_match_the_cells` runs 20,000 random multi-piece
  reads with and without moved-fingerprint reuse, then compares fingerprints,
  fill counts and `content_seq` after every read. Four injected bugs fail it:
  no cursor-line mark on scroll, no cursor marks on full damage, no
  leading-spacer mark, and no stale-row carry.
- `scrolling_moves_content_sources` checks line sources through LF, SU/SD,
  IL/DL and regions.
- Vendored storage tests check that recipe-bit rows and spare rows reset to
  template cells for every background.
- The randomized storage test now includes recycling and byte bounds. Debug
  builds assert the kept payload equals a recomputation, and a broken pop fails
  three tests.
- `streaming_full_history_recycles_without_decoding` fails if recycling decodes
  or stops reusing sealed rows.

Not claimed: GUI rendering, latency, SSH, or any installed-app CPU change. The
Remote Helper Build ID changes because it hashes vendored parser sources.

```sh
cargo build --release -p diri-engine --example feedbench
/usr/bin/time -l target/release/examples/feedbench <payload> 160 50 4k   # or 16k, 64k, whole, engine4k, engine64k
cargo bench -p diri-terminal-state --bench terminal_parity
cargo test --release -p diri-terminal-state --test transcript_digest -- --ignored --nocapture
```

## Terminal interactions other than typing (2026-09-29)

Every interaction except keystroke echo was measured separately: scrolling
history, dragging a window or split, sliding a seam, switching sessions,
selection drags, large pastes, Find, zoom and theme changes. The content was
10,000 rows of coloured build log with wrapped lines, and CJK/emoji variants.
Client frames went through GPUI's production text system and the headless
Metal renderer. Anything that crosses a socket ran against a private Engine
and real Holders. The renderer met the 120 Hz budget on every interaction.
The Engine did not:

- **Dragging stalled the whole Engine.** A column change reflows the
  emulator's history. Over 10,000 wrapped rows that took 8 ms at p50 and
  43 ms at p95 per step. It ran on the attach thread while that thread held
  the Registry lock, so every other session's input and publication queued
  behind it. The desktop sends a resize every 8 ms and the Engine applied
  each one, so steps backed up for the length of the drag.
- **Pastes over 1 MiB were lost.** The Holder input stream bounds a frame at
  1 MiB and rejects a larger one whole. The Engine then dropped the attach,
  so the paste disappeared and the client had to reseed.

Changes:

- `Session::resize_pty` resizes the PTY (a syscall or one Holder stream
  write) under the Registry lock and returns the emulator reflow still owed.
  The attach and control paths run that reflow after releasing the lock. The
  owed reflow always applies the newest size the PTY was given. Racing
  resizes therefore still leave the emulator at the PTY's size, and a reflow
  that finds a newer one already applied does nothing.
- The attach reader skips a Resize frame when the next frame in the same
  read is also a Resize. Only an adjacent resize supersedes one, so input and
  mouse frames keep their order. Each skipped frame saves a reflow and a
  SIGWINCH repaint.
- `HolderClient` splits input larger than one stream frame into chunks. They
  are sent back to back under the input-stream lock, so no other input can
  land between them. There is no Holder or wire change: old live Holders
  accept every chunk.
- When a terminal's origin moves and nothing else about it changes (a
  sidebar or inspector seam, or a split divider, sliding past), the renderer
  translates its cached rows instead of preparing every row again. It does
  this only when the move is a whole number of device pixels, so snapping
  stays exact.

### Measurements

Apple M4 Max, macOS 27.0, release builds, load average 4–11 during the A/B
(25–80 earlier). Base is `42bf00a`. Engine runs alternated base and branch,
3 invocations each of 3 runs, and medians of all 9 are shown. The drag steps
160→120→160 columns twice, then ends at 150×49, sending one Resize frame
every 8 ms (1.3 s). A second session is typed into every 20 ms throughout.
Engine CPU is this process's user+sys time; Holders are separate processes
and do not reflow.

| Interaction (Engine, real Holders) | Base | Branch |
| --- | ---: | ---: |
| Drag: last resize → grid at final size | 57.0 ms | 10.6 ms |
| Drag: other session's echo, p50 | 23.8 ms | 0.29 ms |
| Drag: other session's echo, p95 | 382 ms | 7.7 ms |
| Drag: grids published to the dragged pane | 20 | 93 |
| Drag: Engine CPU for the drag | 1.32 s | 1.21 s |
| Paste 1 MiB into a reading program | lost, attach dropped | 74 ms |
| Paste 100 KiB into a reading program | 10.1–11.3 ms | 9.4–11.8 ms |

With load average 25–80, the base drag settled in 140 ms–1.06 s, the other
session's echo reached p95 0.54–1.8 s, and only 5–12 grids were published.

The renderer (headless Metal, 120 fps harness, 3 alternating runs, draw time
per frame from the GPUI report):

| Interaction (renderer) | Base p50 / p95 | Branch p50 / p95 |
| --- | ---: | ---: |
| Seam slide past a 160×50 terminal, 1 px/frame | 0.566 / 0.594 ms | 0.424 / 0.456 ms |
| Seam slide: rows reused | 0% | 99% |
| Pane narrowing 1 px/frame (clips the grid) | 0.42 / 1.73 ms | unchanged |
| Reflowed full snapshot arriving each frame | 0.56 / 0.60 ms | unchanged |
| Switch: first frame of a fresh 160×50 element | 1.72 / 1.86 ms | unchanged |
| Switch: 160×50 CJK/emoji | 2.47 / 2.68 ms | unchanged |
| Switch: 240×66 (4K display at 2×) | 2.21 / 2.36 ms | unchanged |
| Zoom: font size changes every frame | 1.70 / 1.81 ms | unchanged |
| Selection drag, one row per frame | 0.48 / 0.51 ms | unchanged |
| Output scrolling a 240×66 CJK grid | 0.64 / 0.68 ms | unchanged |
| Theme crossfade, 200×60 (existing bench) | 0.62 / 0.69 ms | unchanged |
| Trackpad fling through history (existing) | 0.60 / 1.49 ms | unchanged |

Engine-side costs per operation (`terminal_interactions` probe, 10,000 rows,
160×50). These did not change:

| Operation | p50 | p95 |
| --- | ---: | ---: |
| Column-change reflow + grid update, ASCII | 8.0 ms | 43.2 ms |
| Column-change reflow + grid update, CJK/emoji | 9.5 ms | 11.0 ms |
| Rows-only resize + grid update | 34 µs | 54 µs |
| Reflow at 340–380 columns (no history row wraps) | 0.19 ms | 0.21 ms |
| 50-row history page (wheel, 3-row steps) | 65 µs | 72 µs |
| Client decode of one page | 28 µs | 30 µs |
| Attach seed (full snapshot + encode, 48 KB) | 41 µs | 45 µs |
| Find capture of all retained rows | 1.1 ms | 1.5 ms |

Over the real sockets, a 50-row history page round trip measured 0.21–0.48 ms
at p50 and at most 1.0 ms at p95. Attach to seed grid measured 0.84–2.4 ms at
p50.

### Not fixed

- **Reflow cost.** Reflowing 10,000 wrapped history rows still decodes and
  re-encodes the whole compact history on every column change (8–43 ms). A
  drag is now bounded by one reflow at a time and no longer blocks other
  sessions, but the Engine spends about a core for the length of the drag.
  The fix belongs in `vendor/alacritty_terminal` compact history, for
  example deferring history reflow until a gesture settles.
- **Paste into a program that is not reading.** The Holder gives up on a PTY
  that stays unwritable for 1 s and rejects the rest of the input. The Engine
  then drops the attach, and the unread tail of the paste is lost. Input is
  still written while the Registry lock is held, so meanwhile every other
  session's input waits. This measured 0.99 s with 100 KiB into a program
  that reads only after 1.5 s. A 1 MiB paste into a program that is reading
  holds that lock for about 70 ms (other sessions' echo max 56–64 ms). Fixing
  this means moving input writes off the Registry lock and changing the
  Holder's give-up policy.
- The Remote Helper still reflows and re-snapshots every Resize it receives.
  Coalescing in the Engine reduces how many reach it.
- `cargo bench -p diri-term --bench find_retained` panics on `main`
  ("Find releases all retained rows"). It was left as it is.

Reproduce from `diri/`:

```sh
cargo test --release -p diri-engine --test interactions -- --ignored --nocapture --test-threads=1
cargo bench -p diri-term --bench terminal_interactions
cargo bench -p diri-terminal-state --bench terminal_interactions -- --gate
cargo test -p diri-term --test moved_pixels
cargo test -p diri-engine --lib resize_tests
cargo test -p diri-engine --test holder a_paste_larger_than_one_input_frame_arrives_whole
```

The Engine measurements start their own Engine and Holders under `/tmp` and
never touch an installed app. `DIRI_PASTE_CASE=<bytes>,<busy_ms>` runs one
paste case. The renderer bench gates seam slide, pane resize, reflow arrival,
selection drag and grid scrolling at the 8.3 ms frame budget. The
terminal-state probe's `--gate` covers history pages and attach seeds; the
reflow is reported but not gated. `moved_pixels` requires the moved frame to
match a fresh render pixel for pixel and to prepare no row again. The
existing `paint_fixture` raw pixels (live, overlapping and reading) were
byte-identical between base and branch.
## Busy local fleets: Holder output path stops serializing (2026-09-28)

**Where the time went.** `fleetbench` (N sessions each `cat`-ing a 64 MiB
colored log through real PTYs, a private Holder manager and Engine sessions)
plateaued near 115 MB/s aggregate from 4 sessions up. Stack samples of both
processes during a 4-session run (base = main with #542):

| Thread | Base | Branch |
| --- | --- | --- |
| Holder log writer, in `write(2)` | 75% | ~5% busy in total |
| Holder PTY pump, blocked on subscriber queue / log queue | 24% / 19% | 7% / 0% |
| Engine pump, waiting on a Holder `stat` reply | 40% | 1% |
| Engine pump, tailing the log after a dropped subscription | 8% | 1% |
| Engine pump, parsing (`feed_with_history`) | 9% | 85% |

Five serialization points were behind that:

- A macOS PTY read is exactly 1 KiB (measured, at any baud rate), so the
  log queue carried one `write(2)` per KiB and its 256-entry bound was only
  256 KiB. A stat took the log lock, which the writer holds across each disk
  write, and did so while holding the PTY lock. The Engine's fact sample
  (every 100 ms since #542) queued behind the disk, and so did keystrokes.
- The subscriber queue was bounded at 16 frames, meaning 16 KiB rather than
  the 1 MiB its comment intended, so the pump waited on the Engine per few KiB.
  An Engine stuck in `stat` then overran the 50 ms patience and was dropped
  to log tailing.
- The output stream ran over macOS's default 8 KiB AF_UNIX buffers, so
  every 8 KiB cost a sender/receiver ping-pong. In a microbenchmark, 256 KiB
  buffers moved 16.8 GB/s against 1.06 GB/s, for an eighth of the CPU.

**What changed.**

- Stat reads an atomic log tail and no longer takes the log lock.
- The pump hands the log writer a byte buffer, which the writer swaps out
  whole. The writer lingers at most 4 ms for a 64 KiB batch, and only while
  bytes are pending; an idle writer parks with no deadline. The pending
  bound is 4 MiB and the pump waits beyond it, as before. The log is
  still fsynced every 2 s. No durability promise changes.
- Subscriber queues are bounded in bytes (1 MiB). Contiguous waiting bytes
  join one frame of at most 256 KiB, below the 1 MiB wire maximum every
  Engine accepts. The wire format is unchanged.
- Both ends of the output stream get 256 KiB socket buffers. The Engine reads
  the subscription response through the frame buffer. Before, a frame sent
  right behind the response was discarded and desynchronized the stream. The
  Engine also sets the socket timeout only when a read will reach the socket.
- A read-only `OutputLog` asked for a window that a rewrite had moved below
  the file's base underflowed into a capacity-overflow panic of the session
  pump. It now resumes at the base. The faster Holder made this reachable
  in `fleetbench` with a 256 MiB payload.
- The exit-marker scan anchors on the OSC `]` instead of ESC, which colored
  output repeats every few bytes.

**Numbers.** Alternating release builds, three runs each, 64 MiB per session,
load average 86–165 from unrelated builds. Wall throughput is not a reliable
measure here. CPU time is: user+sys of the fleetbench process (Engine) and of
the private Holder manager (`ps` utime+stime), as medians.

| Sessions | Holder CPU base → branch | Engine CPU base → branch | Aggregate MB/s base → branch |
| --- | --- | --- | --- |
| 1 | 0.81 → 0.51 s | 1.15 → 1.03 s | 16.9 → 20.4 |
| 4 | 4.05 → 2.51 s | 3.73 → 3.65 s | 80.4 → 75.4 |
| 8 | 11.75 → 8.65 s | 9.41 → 8.86 s | 119.0 → 111.6 |
| 16 | 21.58 → 17.76 s | 18.45 → 17.92 s | 99.5 → 110.2 |

At a lower load (26–50), 4 × 32 MiB runs went from Engine 3.02 → 2.28 s and
Holder 2.70 → 1.56 s.

`holderbench 20` (20 sessions × 5 MiB, no subscriber): idle CPU was 0% on
both builds. Manager footprint went from 12.6–23.4 MiB to 10.1–12.0 MiB,
because an idle session no longer holds a 1 MiB write batch.

**What is not claimed.** No aggregate-throughput gain. Raw macOS PTYs with a
trivial Python reader and no diri code reach only 94 / 133 / 117 MB/s at
4 / 8 / 16 sessions on this machine, and reader sys time per KiB grows with
N. That plateau is the kernel's PTY layer, and the branch now sits at it.
Of the remaining Holder CPU, 90% is sys time in `poll`/`read` on the PTY.
The Engine is now bound by terminal parsing, which is the terminal-feed work.
Under heavy load its 100 ms `stat` sample still waits on Holder scheduling
latency (about 15% of pump wall time at load 40+), but costs no CPU.

**The one stalled holderbench run, root-caused.** A loop of holderbench's
drain phase stalled 11 of 350 rounds on the branch and 11 of 550 on base. The
pattern was the same on both: every running session had drained all 5 MiB,
and each stalled one had never started. Its log held only the 16-byte
header, with no socket and no child. The manager's stderr (normally
`/dev/null`) held `PTY spawn: Unknown error: -6`. That is XNU's
kernel-private `EREDRIVEOPEN` escaping `openpty(3)` under concurrent PTY
creation and teardown. Sixteen processes doing `openpty`/`close` reproduce
it with no diri code, a few times per 32,000 opens. The manager had already
acknowledged the launch, so the session vanished silently. `Pty::spawn` now
retries that code (and `EINTR`) up to 8 times. Retrying never lets the
error through: 0 failures in 64,000 churned opens, against 22–32 without
the retry.

Separately, LogFeed and the byte-bounded subscriber queue each ran 80,000
randomized rounds, with tiny thresholds, random delays on both sides and
random hangups. None hung, and none lost or reordered a byte.

Reproduce: `DIRI_HOLDER_BIN=target/release/diri-holder
target/release/examples/fleetbench /private/tmp/dperf-payload.txt <n> 160 50`.
The Holder manager is the `diri-holder --manager` whose parent is fleetbench.
It lingers 30 s idle, so its CPU time can be read after the run.

## Animations at the display rate (2026-09-28)

GPUI paces windows with one `CVDisplayLink` per display. On this MacBook Pro
(built-in ProMotion panel, `maximumFramesPerSecond` 120) a link created the
way `gpui_macos` creates it ticked 239 times in 2 s. GPUI does not cap an
active window. It does cap two other cases whenever a frame callback is
pending: a window that is not key, at one frame per 33.3 ms, and thermal
pressure, at one per 16.7 ms. Replaying 5 s of real link ticks (hopped to the
main queue as GPUI does) through those rules gave 120.1 fps uncapped,
26.3 fps not key (a tick's jitter often pushes the gap from four vsyncs to
five), and 46.1 fps under thermal pressure.

Some motion was driven by 16 ms timers instead of frame requests. Measured
against the same link, a one-shot 16 ms timer re-armed by each frame it causes
landed 3 vsyncs apart: 40 fps. A free-running 16 ms loop drew 53 fps with 2/3
vsync judder. `request_animation_frame` drew 119.8 fps, 97% of gaps one vsync.
The sidebar's row, disclosure, title and number motion, the usage chart and
selection autoscroll now request display-link frames. The cursor blink's fades
do too, while its holds still sleep on one wake (see the cursor paragraph
below). Every one of them is sampled from elapsed time, stops requesting frames
when it lands, and schedules nothing when idle. Harness test counts at 120 Hz:
a 180 ms title settle paints 22 frames (12 before), a row arrival 25 (10–14).

Not fixed: Diri's floating panels are non-activating and never key, so GPUI
treats every menu, popover and the palette as inactive and caps its animations
at the 26 fps above. The cap is in `gpui`'s frame callback, not in the vendored
`gpui_macos`. Reporting such panels as active from `gpui_macos` lifts it, but
GPUI also sets the app-wide cursor from any active window, so the panel and
the window behind it would take turns setting it. That needs its own change.

## GPUI scenes give back a large frame's storage (2026-09-28)

`vmmap`/`heap` on the installed app attributed about 36 MB of live heap to GPUI
scene vectors held at their high-water capacity: `Scene::clear` empties the
primitive vectors each frame but never frees them, so one very large frame
pinned its peak for the life of the window. The vendored GPUI `Scene` now
shrinks each vector to twice the latest frame's length after 120 consecutive
frames that used under a quarter of at least 1 MiB of reserved storage. Steady
frames never reallocate. Covered by
`a_scene_gives_back_capacity_a_single_large_frame_left_behind`; no installed-app
footprint change is claimed yet (it depends on how large a user's largest frame
was).
## One GraphQL request per PR sweep (2026-09-28)

**Before.** The PR monitor ran one `gh pr view <url> --json …` per due PR:
one process, about 75 ms CPU, about 1 s wall, and one GraphQL request each.
When review threads were due (every 30 min) it also ran a second
`gh api graphql` per PR. With 26 open PRs on screen, that was 26 processes a
minute, or 52 when threads were due.

**What changed.** A sweep iteration now sends one `gh api graphql` request
per host for up to 25 due PRs. Each PR is an aliased
`repository(owner:$oN,name:$nN){pullRequest(number:$pN)}`. Names travel as
variables. The request asks for the same connections `gh pr view` does:
comments and reviews `first:100`, `commits(last:1)` → rollup `contexts(first:100)`.
It asks for `reviewThreads(first:100)` only for PRs whose thread TTL is due.
`gh_view_from_graphql` turns each node into the JSON `gh pr view --json`
prints, following gh 2.101.0's `api/export_pr.go` and its Go structs:

- GraphQL nulls become Go zero values.
- A PR author with no User id becomes `app/<login>`.
- A check run always has `workflowName`, empty when it has no workflow.
- Comments and reviews keep only `author.login`.
- An empty comment `url` is omitted.

The existing `parse` then reads that JSON. Some PRs fall back to the per-PR
path in the next iteration, at most two per iteration as before:

- the request fails or times out;
- an alias comes back null, such as a deleted repository;
- a connection has another page, which gh would have fetched.

An iteration runs either one batch or the per-PR fetches, never both. A slow
batch therefore costs one 15 s watchdog. That is less than the 2 × (15 + 15) s
per-PR bound, which is unchanged. Cadence, backoff, forced refresh and
settled-PR handling are unchanged. A chunk shares one attempt time, so it
comes due again as one request.

`run_gh` also had a latent hang: it polled for exit before reading stdout.
Any reply bigger than the pipe buffer blocked gh on write until the 15 s
watchdog killed it. A 26-PR batch reply is about 230 KB. Stdout is now
drained on a thread.

**Equivalence.** `examples/prbatch.rs capture` records `gh pr view` and the
thread query for each PR. It then records the batch, then the per-PR pair
again. A PR counts only when both per-PR snapshots agree. The run covered
49 real PRs from cristicretu/diri, cli/cli and kubernetes/kubernetes, all
read-only:

- 45 were field-for-field identical to `parse(gh pr view)` + thread counts.
  They cover open, draft, merged, closed, failing, pending, no checks,
  StatusContext, workflow-less check runs, bot authors, every review
  decision, conflicting PRs and resolved threads.
- 3 changed between the two per-PR snapshots because CI moved. For each of
  them the batch matched one snapshot.
- 1, a PR with 185 reviews, correctly fell back.

An earlier run found no mapping differences either. Its only mismatches
were CI moving and GitHub computing `mergeable` lazily on first ask.

`batch_payloads_parse_exactly_like_gh_pr_view` replays 16 of these pairs,
with bodies replaced by placeholders on both sides. Each is parsed with and
without threads, and the test fails if the fixtures stop covering any of
those cases. `graphql_nulls_become_what_gh_exports` covers the null shapes
no recorded PR had.

**Measured.** One sweep over the first 26 open cristicretu/diri PRs, release
`prbatch sweep-old|sweep-new` under `/usr/bin/time -p`. The CPU figures
include the gh children, and a PATH shim counted spawns. Old and new
alternated for three rounds, with the machine at load 30+:

| 26 PRs, one sweep | gh processes | gh + probe CPU (user+sys) | wall |
| --- | ---: | ---: | ---: |
| per-PR, threads due | 52 | 3.81 / 4.03 / 3.84 s | 44.7–45.8 s |
| per-PR, steady state | 26 | 2.03 / 1.96 / 1.89 s | 27.6–29.0 s |
| batched, threads due | 2 | 0.18 / 0.13 / 0.14 s | 4.7–5.9 s |
| batched, steady state | 2 | 0.17 / 0.14 / 0.15 s | 4.2–5.5 s |

Two processes, because 26 PRs is one chunk of 25 plus one of 1. GitHub's own
`rateLimit{cost nodeCount}` gives the request cost:

| Request | GraphQL points | nodes |
| --- | ---: | ---: |
| one PR (`gh pr view`'s connections) | 1 | 301 |
| one PR's thread query | 1 | 100 |
| 26 PRs batched, with threads | 1 | 10,426 |
| 26 PRs batched, without threads | 1 | 7,826 |
| 38 PRs batched, with threads | 2 | 15,238 |

At the 60 s foreground cadence, the steady state drops from 26 points and
26 processes a minute to 2 and 2. With threads due, it drops from 52 to 2.

**Not claimed.** The REST `gh api rate_limit` graphql bucket did not track
these requests: it read `used: 1` while GraphQL's own `rateLimit` said 1,013.
The same token also served other agents at the same time, so the point
figures come from `rateLimit` per request, not from before/after deltas.
Wall times are dominated by GitHub and the loaded machine. A batch moves
more bytes per request than one `gh pr view`, since bodies are included.
PRs whose refresh phases differ, such as after a forced refresh of one
session, go out as separate batches rather than being pulled forward.
## Busy shells stop blocking on Holder facts (2026-09-28)

`fleetbench` with four sessions draining colored logs showed the aggregate
throughput of four sessions no higher than one (≈73 vs ≈79 MB/s). A 5 s stack
sample of the Engine put 30% of every session pump's time inside
`sample_held_pty_facts` → `HolderClient::stat`: after every output frame from a
shell session, the pump made a synchronous round trip to the Holder manager,
which every local session shares, to read the foreground process and the
termios secret-input state.

While output streams, the pump now samples at most every 100 ms. The settle
path after output stops is unchanged and still samples at once, which is when
a password prompt or a new foreground program becomes visible. Agent sessions
already skipped most samples; plain terminals running builds or `cat` paid
them all.

After the change the same sample shows 0% of pump time in Holder stats. Wall
throughput could not be compared reliably: the machine was shared with
unrelated Rust builds (load average 34–77), and alternating runs swung more
than the effect. Engine CPU per 4 × 32 MiB run went from 2.07–2.42 s to
1.99–2.20 s. No throughput number is claimed.

## Sidebar rows re-render only when they change (2026-09-28)

**Where the time went.** A live sample of the installed app showed the main
thread about 20% busy with 51 sessions, about four of them working. Roughly
45% of that was `Sidebar` render, layout, prepaint, and paint. The sidebar is
cached in RootView, but every 125 ms activity-mark tick notified it, and each
re-render rebuilt all 51 rows.

**Why a row cache needed GPUI changes.** Upstream GPUI (zed `dc2a339`, #21165)
re-renders every cached view nested inside a cached view that missed its
cache. It records cache ranges as absolute frame indices, which go stale
while an ancestor is reused wholesale.

**What changed.**

- GPUI is now vendored in `vendor/gpui`, with the patch described in
  `vendor/gpui/DIRI_PATCHES.md`. Cached views record ranges relative to their
  nearest cached ancestor, so non-dirty nested views are reused. Opacity is
  part of the cache key.
- Each session row is a cached view that renders from a props snapshot. The
  activity tick notifies only the working rows.
- A store publication re-renders only rows whose props changed.

Store churn was never the driver. The app already drops byte-identical
`session.updated` events without publishing, and resource samples arrive
every 30 s.

**How it was measured.** `sidebar_fleet_render_cost` mounts the real RootView
under headless Metal with five projects and four working sessions. It stands
in cached blank rasters for brand marks, which production draws as CoreGraphics
images on the main thread. One step is one notify plus the frame it causes.
The numbers are release-build medians of 1,000 steps. Before and after ran as
alternating binaries three times each, with load average ~15–20 from other
agents:

| 51 sessions | Rows built, before → after | Step median, before → after |
| --- | ---: | ---: |
| Activity tick | 51 → 4 | 1.53–1.61 → 1.05–1.06 ms |
| Store publication (nothing changed) | 51 → 0 | 1.53–1.56 → 0.99–1.01 ms |
| Root-only frame (sidebar reused) | 0 → 0 | 0.39–0.40 → 0.41–0.42 ms |

Subtracting the root-only frame, a tick's sidebar share falls from about
1.15 ms to 0.65 ms. Per-row growth falls from about 20 µs to about 6 µs:
single runs at 5/25/51/100 sessions measured a tick at 0.72/1.09/1.75/2.70 ms
before and 0.74/0.93/1.14/1.33 ms after. What remains per row is computing and
comparing props, the list wrapper, and replaying cached ranges.

A root-only frame costs about 0.03 ms more (+8%). Each row adds a view and a
wrapper node that a reused sidebar replays.

**Visual checks.** Pixels are identical to `main`:

- 20 headless sidebar fixtures, light and dark: typical, fleet, stress,
  projects, hover, recency, filter, session menu, hover card, lineage.
- The bench's final frame, after roughly 3,000 steps.

`reused_sidebar_rows_paint_like_a_full_render` checks the reuse path. After 11
ticks interleaved with no-op publications and root frames, it compares against
a `window.refresh()` rebuild and requires zero differing pixels. It fails if a
working mark stops advancing.

**Not claimed:**

- any installed-app CPU change;
- GPU or present cost;
- the horizontal strip. It is rendered inline by RootView and still rebuilds
  every tab.
## Terminal feed path, 4.1–4.7× faster per core (2026-09-28)

Every local session's Engine and every remote session's Helper parse all PTY
output through `HeadlessScreen::feed`: VTE, the alacritty grid, compact
scrollback and per-row change fingerprints. At `main`, a 64 MiB colored build
log parsed at about 25 MB/s on one core. A stack sample showed the escape parser
itself was a minor cost. Most time went to scrollback: each 32-row history block
was serialized as JSON and DEFLATE-compressed. Once history was full, the oldest
block was also decoded again to recycle its rows.

Changes, with no protocol, checkpoint or wire change:

- History blocks use a binary row encoding (style table, text, style runs,
  implied default suffix) and a small in-tree LZ77 block codec instead of
  `serde_json` + `flate2`. No dependency is added.
- With full history, recycled rows are built in their reset state directly
  from the encoded row. Evicting history never decodes a block.
- VTE hands printable ASCII runs to the terminal, which writes a row segment at
  a time. The result is identical to per-character input (wide cells, wrap,
  insert mode, charsets, prompt marks and links fall back or match).
- Row fingerprints hash the same wire projection in four multiply lanes. They
  derive the wire style only when a cell's raw style changes.
- Notification and progress scans use `memchr`, which VTE already links.
  Visible-row indexing has an inlined fast path.

Details and tests are in `vendor/alacritty_terminal/DIRI-PATCH.md` and
`vendor/vte/DIRI-PATCH.md`.

### Measurements

Apple M4 Max (Mac16,5), macOS 27.0, Rust 1.97.1 release builds. The base is
`daa570e` with the same `feedbench` source. The machine was shared with other
work (load average 7–25), so `feedbench` now also reports process CPU time.
The table gives CPU-time medians of three alternating base/branch runs at
160×50 (MB = MiB).

| Payload, read size | Base | Branch | Speedup |
| --- | ---: | ---: | ---: |
| Colored build log (64 MiB), 4 KiB | 21.7 MB/s | 88.8 MB/s | 4.1× |
| same, 16 KiB | 24.7 MB/s | 110.6 MB/s | 4.5× |
| same, 64 KiB | 26.0 MB/s | 117.5 MB/s | 4.5× |
| same, one call | 26.9 MB/s | 120.1 MB/s | 4.5× |
| same, Engine config (notifications), 4 KiB | 21.5 MB/s | 87.5 MB/s | 4.1× |
| same, Engine config, 64 KiB | 25.9 MB/s | 115.3 MB/s | 4.5× |
| `git log -p --color` (32 MiB), 4 KiB | 13.5 MB/s | 58.4 MB/s | 4.3× |
| same, 64 KiB | 15.1 MB/s | 70.8 MB/s | 4.7× |

At 4 KiB reads, row fingerprints of the fully damaged screen are now the
largest single cost. The remaining time is split between the parser and
history encoding.

`terminal_throughput` (160×50 per-operation, base → branch): typing
1,773 → 1,023 ns, scrolling 63,335 → 36,330 ns, cursor-only 1,313 → 784 ns.
`terminal_parity`, 10,000 lines per 80×24 core: feed time p50 87.4 → 34.5 ms
per core. Retained heap is 246,403 → 259,699 bytes and peak heap is
635,191 → 324,211 bytes; the base's peak included JSON/DEFLATE buffers.

`terminal_fleet` gates pass (base → branch): fresh 2.02 → 2.02 MiB, full
history 6.25 → 6.09 MiB, widened 16.37 → 16.21 MiB, after churn
11.62 → 11.46 MiB, zero warmed cursor allocations and zero leaked bytes.
Compressed history stored 10,000 rows of 160 columns in 54.5 bytes/row
(base 53.0) for `git log -p` and 41.8 bytes/row (base 35.4) for the synthetic
log. The 4 MiB history budget and its accounting are unchanged, except that a
partially recycled oldest block counts compressed bytes plus one index instead
of fully decoded rows. **Output limited by the byte budget, not the
10,000-row limit, can retain fewer rows than before.**

`fleetbench` (20 sessions × 16 MiB, three alternating runs) kept aggregate
throughput at 91–98 MB/s (base) versus 95–105 MB/s (branch). That fixture is
bound by the PTY, Holder and log path, not parsing. For the same 320 MiB, the
benchmark process's CPU time fell from 17.8–18.2 to 9.0–9.4 seconds
(user + sys). System time rose from about 1.1 to 2.4 seconds; that was not
investigated. No desktop was attached.

### Equivalence

- `crates/diri-terminal-state/tests/transcript_digest.rs` (opt-in) drives
  60,000 steps through random read splits: styles, wide/combining text, links,
  prompts, scroll regions, erase, insert mode, charsets, alternate screen,
  synchronized output, resizes and full 10,000-row history. It hashes every
  diff, snapshot, history, scrollback, `content_seq`, `filled_cells`, cursor,
  title and progress. Both revisions print identical digests at every
  checkpoint. Deliberately broken fingerprints and recycled rows change the
  digest.
- Randomized vendored-crate tests compare `input_ascii` with per-character
  `input` and full-history recycling with dense storage. Other tests check
  recycled rows against decode-then-`Row::reset` and codec/LZ round trips.
  Each test was confirmed to fail on an injected bug.
- While testing, dense and compact storage were found to diverge after
  resizing a *full* history. The base revision diverges the same way. This
  change does not address it.

Not claimed: GUI rendering, input-to-photon latency, SSH or WAN behavior, or a
whole-application CPU reduction of any particular size.

```sh
cargo build --release -p diri-engine --example feedbench
target/release/examples/feedbench <payload> 160 50
cargo bench -p diri-terminal-state --bench terminal_throughput
cargo bench -p diri-terminal-state --bench terminal_fleet
cargo bench -p diri-terminal-state --bench terminal_parity
cargo test --release -p diri-terminal-state --test transcript_digest -- --ignored --nocapture
```

The 64 MiB log has lines like
`ESC[3Nm[0000000123] building crate_N v0.N.0ESC[0m  Compiling module xxxx…\r\n`.
The Remote Helper Build ID changes because it hashes vendored parser sources.
Live Helpers keep their binaries.
## Desktop memory attribution (2026-09-28)

The installed 0.8.7 app (30 sessions, one window, 1.5 days up) measured a
516 MB physical footprint, 942 MB peak. `footprint`/`vmmap` (read-only, the
installed app was not restarted) split it as: owned unmapped (graphics)
207 MB, Malloc Small 115 MB, IOSurface 83 MB, IOAccelerator (graphics) 60 MB,
Malloc Large 25 MB. A new retained switch, `DIRI_GPU_DIAG=1`, prints the
renderer's Metal allocation, instance-buffer pool, atlas pages, path targets,
drawable pixels and frames every five seconds to stderr; its counters are
relaxed atomics and no thread starts when it is off.

What each part is:

- **~194 MB owned unmapped (graphics) is the Metal driver, not diri.** A
  standalone 45-line Swift Metal app on the same machine (macOS 27, M4 Max)
  carries 193–202 MB whenever it has submitted a command buffer in the last
  ~2 s: at 60, 8, 2 and 0.5 fps, with a 64×64 drawable, rendering offscreen
  without presenting, and with `maxCommandBufferCount` 1. Two seconds after
  the last submission the same memory is reported reclaimable. The only lever
  is not submitting frames while nothing changes; in the dev app an idle
  window drew 0–2 frames per 5 s and the pool became reclaimable. The
  sidebar's 8 Hz Working tick is handled separately.
- **IOSurface is the two window drawables** (2 × 41.7 MB at 4112×2580).
  `MAXIMUM_DRAWABLE_COUNT` is already 2. Inherent to the window size.
- **IOAccelerator held six 8 MiB instance buffers, five swapped out.** The
  shared instance-buffer pool kept every buffer that had ever been in flight
  at once, and its size only doubled. Fixed below.
- **Heap.** A MallocStackLogging (lite) attribution of a dev app after a
  30-session scenario found 44 MB live: GPUI scene vectors 12 MB, the usage
  ledger 8.5 MB, text shaping 2.3 MB, then small items. The installed app's
  scene vectors are 36 MB (two scenes at 65,536 sprites); that is the known
  `Scene::clear` high-water in upstream GPUI (not vendored) and is not
  changed here. `malloc_zone_pressure_relief` released 0 bytes, so no
  allocator trimming was added. Atlas pages (one 1 MiB monochrome page) and
  path targets (lazy, 0 in the scenario) were not factors, and the renderer
  count returned to one after floating panels closed (no #425 regression).
- **The 942 MB peak is the usage scan.** Each transcript tail was read into
  one `Vec`; this machine has a 757 MB Codex rollout and lines up to 24 MB.

Changes:

- The instance-buffer pool keeps at most three idle buffers, halves a grown
  size after 10 s in which every frame fit in a quarter of it (leaving 2×
  headroom), and drops idle buffers and refits the size when a window stops
  drawing (occlusion). Growth is unchanged: an overflowing scene is
  re-encoded once with a doubled buffer.
- Transcripts are streamed one complete line at a time. A partial trailing
  line is neither consumed nor counted, as before. A read error after some
  lines stops at the last complete line instead of discarding the file's
  progress. A borrowed probe of the `type`/`payload.type` tags skips lines
  that only mention a usage tag (compacted history) without building a
  `serde_json::Value` of the whole line; anything the probe does not model
  falls through to the unchanged full parse.

| Measurement | Before | After |
| --- | ---: | ---: |
| Cold usage scan of this machine, 12.1 GB / 2,529 transcripts, peak RSS | 996–1,196 MiB | 209–222 MiB |
| Same scan, wall time | 15.8–15.9 s | 12.4–12.6 s |
| Dev app cold start + 30-session scripted scenario, peak footprint (one run each) | 1.5 GB | 643 MB |
| Instance-pool probe after dense frames in four windows, IOAccelerator (graphics) | 26 MB (3 × 8 MiB kept) | 1.8 MB (none kept) |

The scan comparison ran the old and new parser on the same APFS clone of
`~/.claude/projects` and `~/.codex/sessions`; the written ledgers were
byte-identical and the snapshots differed only in clock-derived fields. The
dev-app scenario (fresh support dir, 26 idle sessions with 3,000 lines of
history, 4 streaming, 60 session switches, palette/quick-open/history
toggles, overview/peek, zoom) used a local, uncommitted harness; with a warm
usage ledger its idle footprint was 186–203 MB on both builds, so no
steady-state change is claimed for that scenario. The installed app's
instance-buffer saving (up to five idle 8 MiB buffers) is inferred from its
`vmmap`, not re-measured on a new install. No change reduces the driver pool
or drawables.
## Hibernated agents outliving their sessions (2026-09-28)

A `ps` of the author's Mac found 33 stopped processes parented to launchd,
6–13 days old, holding about 153 MB resident: 7 Codex node wrappers
(`node …/bin/codex -c notify=[…dirijor notify] …`), 13 Claude Code
processes, and 13 Chrome DevTools MCP telemetry watchdogs. Each wrapper and
Claude process sat in state `T` in the process group of a leader that no
longer existed (`fish -i -l -c codex …`), with its own children as unreaped
zombies and fds 0–2 on a revoked terminal. Each watchdog was stopped in a
session of its own. Nothing continues them, so they last until reboot.

The shape is a hibernated (SIGSTOPped) tree whose leader died. On macOS a
stopped process with default disposition dies at once from the hangup that
follows, even while stopped, which is why the native Codex binary and the MCP
servers are zombies. A stopped process that handles SIGHUP — Codex's node
wrapper forwards it to its child, Claude Code handles it — keeps it pending
forever. Two paths lead there: the leader dies while its holder lives (the
holder reaped it and left the rest), or the holder manager process dies (a
crash, `kill -9`, reinstall), which hangs every PTY up at once. The
manager for the installed app was replaced on 2026-09-23, after every
installed-app orphan was created; the dev-build orphans belong to dev
managers that no longer exist.

Fix, in the local Holder only:

- The exit watcher waits for the leader's exit without reaping it (a kqueue
  `NOTE_EXIT`/pidfd readiness fd; macOS `waitid(WNOWAIT)` also returns for a
  stop, so it would read a hibernation as an exit), then SIGKILLs the
  leader's process group, everything still descended from its members, and
  every identity the holder's last SIGSTOP froze, re-verifying each pid's
  start time. The zombie leader pins the group id until then.
- A single `diri-holder --group-guard` per manager (about 1.3 MB phys
  footprint, measured with `footprint`) reads `+pgid`/`-pgid` and
  `s`/`c` frozen-identity lines from a pipe only the manager holds, and on
  EOF kills what is still registered. It wakes only when a session starts,
  ends, hibernates or wakes.

Before/after, from deterministic fixtures of the Codex shape (`sh -c` leader
forking a wrapper that traps TERM/HUP and waits on a `sleep` child, plus a
`setsid` helper):

| Scenario | Survivors before | After |
| --- | --- | --- |
| hibernated, leader SIGKILLed, holder alive | wrapper (`T`), setsid helper (`Ts`) | none |
| running, leader exits, group member ignores HUP/TERM | that member | none |
| hibernated, manager SIGKILLed | wrapper (`T`), setsid helper (`Ts`) | none |

The before columns were produced by disabling the sweep and the guard in the
same tests. Reproduce from `diri/`:

```sh
cargo test -p diri-engine --lib holder::server::tests::a_leader
cargo test -p diri-engine --lib holder::guard
cargo test -p diri-engine --test holder a_dead_manager_leaves_no_hibernated_agent_behind
```

Not claimed: this does not clean up processes that are already orphaned, and
it does not cover a tree whose manager died before this build was running.
It does not change explicit kill/close (`kill_tree` already escalated to
SIGKILL) or the remote Helper, whose per-session guard already kills the
Agent group before the leader is reaped. A group member that ignores the
hangup no longer outlives a leader that exits normally; that matches the
remote Helper. Out-of-group descendants of a leader that exits normally
while running are still left to their own lifecycle.
## Per-session files no longer leak; startup orphan sweep (2026-09-28)

An installed Engine's `logs/` held 949 MB in 1,440 files while `state.json`
held 30 session records. Only 90 files (256 MB) belonged to a record. The rest
were 1,013 `s_<id>.screen.plist` checkpoints, 257 `s_<id>.attention.sqlite`
stores, and 78 `s_<id>.bin` output logs up to 49 days old.

Cause: `Registry::remove`, the only path that drops a record, unlinked
`<id>.bin` and nothing else. The screen checkpoint and the attention store of
every closed tab stayed forever, and nothing collected files orphaned by
crashes, state loss, or older builds. Removal now calls one helper,
`session_files::remove_log_files`, over one suffix list (`.bin`,
`.screen.plist[.tmp]`, `.attention.sqlite[-journal|-wal|-shm]`). Recovery
directories were already cleaned by `remove_owned_files`.

The daemon also runs one bounded sweep per start, on its own thread after the
socket is bound. It never repeats and never runs while idle. It keeps every id
referenced by a loaded record, a live or launching session, the in-memory
reopen-closed stack, a holder socket or pid file, or a remote binding file
name. It only considers regular files named exactly `s_<12 lowercase hex>`
plus a known suffix, never follows symlinks, and skips anything modified in
the last 15 minutes. In `sessions/` it removes only the Engine-owned recovery
files. The sweep is skipped when the state file failed to load or holds no
records.

Measured with `examples/logsweep.rs` (release, Apple silicon, APFS):

| | files | bytes |
|---|---:|---:|
| installed `logs/` before | 1,438 | 974 MB |
| dry run: would remove | 1,324 (+19 recovery dirs) | 701.7 MB |
| kept (referenced / recent / foreign) | 138 / 4 / 2 | ~272 MB |

Sweeping a replica with the same names, sizes, and mtimes (content not
copied) took 48–68 ms when the files were sparse and 237–294 ms when they were
dense (`LOGSWEEP_DENSE=1`). Both runs were off the accept path.

Not claimed: the dry run is a snapshot of one machine at one moment. The
dense replica approximates, but is not, the user's real extents. Bytes are
logical sizes. The workspace layout still names 191 dead session ids. Those
tabs have no record and are not treated as references, and that leak is not
fixed here.
## Settled pull requests (2026-09-28)

The Engine's PR monitor refetches every pull request linked from an attached
session once a minute while the App is in front, one `gh pr view` process
(about 60 ms CPU, 1 s wall, one GitHub GraphQL call) per PR. On the installed
App a session that had shipped 26 PRs kept that up for all of them: 51 `gh`
processes in 90 s, nearly all for merged PRs whose state can no longer change.

A PR whose cached state is `MERGED` or `CLOSED` now refreshes on the 30-minute
background ceiling whatever its session's visibility. Selecting the session
still forces an immediate refetch, exactly as before, and open PRs keep the
60 s foreground cadence. For the observed session that is 26 `gh` calls a
minute becoming fewer than one. `a_merged_or_closed_pr_on_screen_is_not_polled_every_minute`
pins the cadences; it measures scheduling, not GitHub latency.
## Idle attach pumps (2026-09-28)

Every attached session has one Engine pump thread. Idle, it woke once a
second, took the Registry lock and looked its Session up again. That tick was
the only way it learned that a restart had replaced the Session, or that its
last sink had gone. A 5 s `sample` of the installed Engine found 13 such
threads, so an idle Engine woke 13 times a second for them, growing with every
open tab.

A pump now sleeps on its grid wake source alone. Output wakes it as before.
Dropping a Session notifies its wake source, so a pump whose Session was
replaced re-seeds immediately instead of within a second. A departing sink
wakes the pump so it can see whether it was the last one. A 30 s ceiling
remains as a safety net. While the Session is absent mid-restart, nothing else
will wake the pump, so that state keeps the 1 s retry.

`an_idle_pump_stops_as_soon_as_its_last_sink_leaves` requires the pump to exit
within 300 ms of an idle client leaving; with the departure wake removed it
fails. Idle wakeups fall from one per second per attached session to one per
30 s. No throughput, latency or protocol change is claimed or intended.
## Engine state persistence against a 1.5 MB state.json (2026-09-28)

A live sample of the installed Engine (51 sessions, `state.json` 1.5 MB, almost
all of it `sessions` pull-request bodies and discussion) spent ~115 ms per 5 s
in `Registry::persist_now`, plus ~30 ms in `workspace.mutate` and ~11 ms in
`WorkspaceStore::snapshot`. Every persist re-read and parsed the whole file
into a `serde_json::Value`, cloned the record table and the value tree,
re-serialized everything and fsynced, even when nothing had changed.
`session.mark_seen` and every attach did this on the control connection thread.

`JsonStateFile` still does locked read-modify-write with atomic rename, because
the Registry (`version`/`projects`/`sessions`), the workspace store
(`workspaceState`) and any other compatible process share one file and must
not clobber each other's keys. What changed:

- The document is kept as top-level sections of raw JSON text. Sections a
  writer does not own are carried byte for byte, never parsed into a tree.
- The last image read or written is cached with an open handle to its file.
  Under the lock, an update `stat`s the path; while device, inode, length and
  mtime still match, the image is the file and nothing is re-read. Any other
  writer's rename (or in-place write) misses and reloads as before. Holding
  the handle keeps the inode allocated, so its number cannot be recycled. The
  Registry and workspace store share one handle.
- A persist whose sections are byte-identical does not write or fsync.
- The Registry serializes records straight to text, one folded record at a
  time, instead of `Vec<SessionRecord>` → `Value` → clone → bytes.
- The flusher serializes under the Registry lock and writes after releasing
  it. Per-owner sequence numbers stop an older snapshot from landing over a
  newer synchronous one.
- `session.mark_seen` and the attach path mark the Registry dirty instead of
  persisting on the request thread; the flusher writes within 500 ms.
  `publish_updated` folds one record instead of cloning the whole table, and
  `workspace.mutate` reads session ids without cloning records.

Durability is unchanged. `persist_now`, `persist_for_shutdown` and lifecycle
persists still fsync the file before returning, and workspace mutations still
fsync the directory entry too, including when the bytes are unchanged.
`mark_seen` was already debounced, so it was never durable before reply. The
on-disk format is the same JSON object with the same keys; key order is still
sorted at the top level, and record fields now follow struct order.

`statebench` (release, macOS 27, APFS, Apple silicon) builds a 1,528 KB fixture:
50 sessions, 5 of them with 26 PRs each (2 KB body, six ~1.2 KB discussion
items, 8 checks); 20 projects; a 63 KB `workspaceState` made through real
mutations. Wall time includes F_FULLFSYNC. CPU is process user+system per
operation. Medians of two runs each:

| operation (n)               | before wall | after wall | before CPU | after CPU |
|-----------------------------|------------:|-----------:|-----------:|----------:|
| persist, unchanged (40)     | 16.2 ms     | 1.0 ms     | 7.4–8.6 ms | 1.0 ms    |
| persist, one field (40)     | 13.0–16.2 ms| 8.1 ms     | 6.4–7.9 ms | 1.6 ms    |
| `session.mark_seen` RPC (20)| 19.7–21.5 ms| 5.2–6.6 ms | 7.2–8.2 ms | 0.44 ms   |
| `workspace.mutate` RPC (40) | 17.0–19.6 ms| 10.9–11.9 ms| 5.8–7.7 ms| 1.3–1.4 ms|
| `workspace.snapshot` RPC (40)| 2.8–5.0 ms | 0.55 ms    | 2.9–4.4 ms | 0.60 ms   |

What remains in a changed persist is the fsync. In `workspace.mutate` it is
the file fsync plus the directory fsync. The remaining `mark_seen` wall time,
with no disk I/O, is inside `EventBus::publish_encoded`. That path is being
changed in the separate event-bus work and was not touched here.

Not claimed: this does not measure the installed app's end-to-end CPU, and
it does not measure real disks other than the local APFS volume. The
"unchanged" row is a best case: a real change still pays serialization of
all sessions (~1 ms here). Per-record serialization caching was not added.

Reproduce from `diri/`:

```sh
cargo test --release -p diri-app --bin diri sidebar_fleet_render_cost -- --ignored --nocapture
cargo test -p diri-app --bin diri reused_sidebar_rows_paint_like_a_full_render -- --ignored
cargo test -p diri-app --bin diri gpui_view_cache
```

`DIRI_BENCH_SESSIONS` and `DIRI_BENCH_ITERATIONS` scale the bench.
cargo test -p gpui_macos --lib instance_buffer_pool
cargo test -p diri-usage --test scan_peak_memory
cargo run --release -p diri-usage --example usage_scan_bench
```

`usage_scan_bench` reads the transcripts read-only into a private temporary
ledger; point `HOME` at an APFS clone to compare builds on identical bytes.
The pool row came from a local, uncommitted GPUI probe (60,000 2 px glyphs
in a main window and three popups, then 25 s of near-empty frames, `vmmap`
of IOAccelerator regions). It opens real windows, so it was not added to
the repository; the unit tests pin the pool policy instead.
cargo test -p diri-engine --test session_files
cargo run --release -p diri-engine --example logsweep -- dry-run \
    "$HOME/Library/Application Support/Dirijor"
LOGSWEEP_DENSE=1 cargo run --release -p diri-engine --example logsweep -- \
    simulate "$HOME/Library/Application Support/Dirijor" /tmp/sweep-replica
```

`removing_a_held_session_deletes_every_sidecar` fails on the old removal path
(`.screen.plist` and `.attention.sqlite` are left).

cargo run --release -p diri-engine --example statebench -- 40
cargo test -p diri-engine --lib -- state_file unchanged_persist sections_other_writers older_snapshot mark_seen_replies
## Engine event delivery (2026-09-28)

A 20-second capture of `dirijor events subscribe` on the installed app (51
sessions, four agents working) carried 119 events and 10 MB of JSON. 83 of the
98 `session.updated` events were byte-identical to the previous one for the
same session: the status watcher, resource sweep, PR monitor and control
mutations each publish a whole record, and most restate it unchanged. Records
are large because tracked pull requests carry body, checks and discussion; one
session with 26 PRs encodes to 272 KB and was republished 27 times.

The App already discarded identical records, but only after decoding them.
Each publication also cost the Engine a `Value` build, a clone, a re-decode
into `SessionRecord` for the activity log, one encode for the replay ring, and,
per subscriber, a deep clone plus a full re-encode, which
`ControlMessage::serialize` itself preceded by another deep clone.

Changes:

- The bus drops a `session.updated` whose encoded bytes equal the last one
  published for that session. The comparison is exact (bytes, not a hash); the
  entry is forgotten on `session.removed`; no other event kind is affected.
  A suppressed restatement takes no sequence number and no activity append.
- Params are encoded once, at publish, straight from the typed payload. The
  ring, every subscriber queue and the control writer share those bytes; the
  writer splices them into the frame (byte-identical to the old line, pinned
  by a test).
- `ControlMessage` serializes field by field, without the intermediate object;
  this applies to every response too, including `session.list`.
- Eleven single-session lookups (`publish_updated`, `events.wait`, spawn,
  resume, …) use `Registry::record(id)` instead of cloning, folding and
  sorting every record to find one.

`eventbench` replays that capture's publication mix with synthetic text of the
same sizes through the production `ControlServer` over a socket pair, and the
client decodes each frame as the App does. Release build, 20 rounds (each round
= 20 s of live traffic), three runs each, Apple Silicon:

| Per 20 s of live traffic | Before | After |
| --- | ---: | ---: |
| Engine CPU | 24.7–26.6 ms | 3.5–3.8 ms |
| Client decode CPU | 14.8–17.2 ms | 1.2–1.4 ms |
| Bytes delivered | 3.9–4.3 MB | 0.36 MB |

With `distinct` (every publication a real change, so nothing is suppressed)
Engine CPU per 20 s fell from 23.2–23.9 ms to 10.3–10.5 ms and client decode is
unchanged in kind. The bench publishes flat out, far burstier than live
traffic: both builds sometimes exceeded the 16 MiB subscriber bound and
delivered an `events.dropped` marker (which makes the App resynchronize), so
frame counts in those runs differ. At the recorded rate (~0.5 MB/s) neither is
near the bound. This measures the event path only; the App's own rendering of a
real change and state persistence are not covered here.

```sh
cargo build --release -p diri-engine --example eventbench
target/release/examples/eventbench 20
target/release/examples/eventbench 20 distinct
```

## Workspace terminal redraw isolation (2026-09-16)

A live sample of installed Diri 0.7.4 reproduced 23–31% app CPU, with
most sampled work in GPUI terminal glyph painting, scene sorting, and layout.
The Engine was approximately 1.2% and the Holder 0.2% in a separate process
sample. This was an active session workload, not an idle release gate.

The saved-workspace view was mounted without GPUI view caching. A regression
through the real RootView, Sidebar, WorkspaceWorkbench, and TerminalPane
confirmed that eight sidebar invalidations rendered an unchanged terminal
eight times. Each mounted terminal now has a cache boundary with definite
bounds.
Its own notifications invalidate it for terminal updates, and bounds changes
invalidate it for resize. Regressions require zero redundant renders for both
sidebar updates and output in a sibling pane, and check that terminal
notification and resize still propagate.

A macOS headless Metal fixture paints a dense 160×50 terminal in a 1600×1000
window, warms twenty updates, then measures process user+system CPU across
200 sidebar notifications. Three alternating before/after debug-build runs
measured CPU per update at 8.879/8.622/8.515 ms before and
3.848/3.872/3.747 ms after: a 55.4% reduction at the median. Terminal
render calls fell from 200 to zero. The resulting 3200×2000 screenshots were pixel-identical. This isolates
unchanged-terminal redraw cost; it is not a claim that total installed-app
CPU falls by 55.4%, or a release-profile performance gate.

Reproduce from `diri/`:

```sh
cargo test -p diri-app --bin diri sidebar_updates_do_not_render_unchanged_workspace_terminal
cargo test -p diri-app --bin diri workspace_sidebar_redraw_cpu -- --ignored --nocapture
```

The second command is opt-in on macOS and uses real Metal with synthetic
session data and an inert runtime. It does not connect to the user's Engine.
Set `DIRI_REDRAW_SCREENSHOT` to export the resulting frame.

Validation on the original integration base: workspace tests passed (1,966
tests); the final per-pane change passed all 755 app tests, including the
sidebar and split-pane regressions. Workspace formatting, Clippy with
`-D warnings`, and release build passed.

## Large preview frames keep draining (2026-09-16)

A 160×50 receive-only preview could disconnect despite continuously reading.
The default macOS Unix socket send buffer is 8 KiB; the output loop waited a
fixed millisecond between partial writes and could also suspend draining during
its 8 ms publication coalescing window. The resulting backlog overflowed even
though the producer and terminal parser kept up. A real-PTY release regression
received only 18 of 120 images before EOF, while the producer and Engine reached
the final frame. The same 80×24 workload stayed connected.

Queued output now waits for `POLLOUT` readiness, bounded to one millisecond, and
resumes immediately when the reader frees capacity. The publisher remembers a
pending grid change while servicing partial writes. Existing frame offsets,
recipient ordering, queue limits, fairness budgets, and the single writer owner
remain intact. Empty queues retain the existing GridWake sleep. No new remote
Holder attachment or preview visibility/activity side effect is introduced.

### Verification

`cargo test -p diri-engine --release --test preview_progress` exercises both
sizes with an 8 KiB send buffer. Each fixture produces 120 dense colored redraws;
the preview must stay connected, show progress, and receive the final frame.
The full Engine suite passed 526 tests with 6 ignored; strict all-target Engine
Clippy, formatting, and the release build passed. Existing slow-reader,
partial-frame, registration-order, and interactive-input tests also passed.

A quiet reference-machine run used 16 previews (one deliberately stalled), plus
one normal active attachment. All 15 drained previews survived each complete
three-second sample; the stalled preview disconnected and reconnect seeded the
same child PID. The production cap remains 16.

| Requested rate | 80×24 updates/s | 160×50 updates/s | Output p90, small / large | Input p95 | Combined CPU cores | Loaded RSS |
| --- | ---: | ---: | --- | ---: | ---: | ---: |
| Idle | 0 | 0 | — | — | 0.0036 | 20.5 MiB |
| 10 Hz | 10.01 | 10.01 | 4.91 / 6.21 ms | 0.917 ms | 0.241 | 33.9 MiB |
| 60 Hz | 58.95 | 58.10 | 4.47 / 5.83 ms | 1.745 ms | 1.082 | 58.5 MiB |

Updates/s counts distinct producer timestamps reaching decoded client grids,
not monitor presentation. At the 60 Hz target, producers completed 173–180 frames
in the sample; the large-grid maximum observed latency was 19.74 ms. These are
local Engine/socket results, excluding GUI, SSH, and producer CPU. The benchmark
combines Engine and blocking decoder threads: 19 threads before attachment,
68 after. The 49-thread increase includes one benchmark reader per drained
preview; actual desktop preview clients use Tokio tasks. Idle CPU includes the
benchmark readers’ 100 ms deadline checks, so it is not an Engine-only idle cost.

[Raw samples and commands](docs/perf/preview-progress-2026-09-16.json) include
per-size connection lifetimes, producer/PTY counters, memory, and input samples.
Run `cargo run -p diri-engine --release --example previewbench -- 16 60 3 diagnose`
to reproduce using disposable local PTYs. The optional `diagnose` argument
records synthetic child write progress; all fixture processes and files are
cleaned up. The harness fails on any unexpected drained-reader EOF. Earlier
48/64-preview loaded samples ended connections early and cannot justify a cap
increase; larger demand requires a fresh measurement of the corrected path.

## A stalled desktop client cannot block another client (2026-09-15)

The Engine-local AttachHub wrote each client's socket synchronously. A client
that stopped reading could block the shared publisher indefinitely. A new
private-socket test reproduces the baseline failure at its fourth redraw:
the active reader times out after 750 ms, even while the PTY keeps draining.

The existing one pump per Session now owns bounded nonblocking output queues.
It encodes each frame once and shares it across sinks, preserves partial-frame
offsets, and disconnects only an overflowing or stalled sink. There is no new
writer thread. The initial seed is queued outside the Registry lock; pongs use
the same ordered output. Normal drained connections keep the existing idle wait.

Run `cargo test -p diri-engine --release --test attach -- --nocapture`. On a
Mac16,5 with 36 GiB RAM, macOS 27.0 and Rust 1.97.1, the 80×24 fixture deliberately
requests a 1 KiB socket send buffer and stalls the first reader. Forty redraws
still reach the other reader: p50 32.98 ms, p90 51.43 ms, maximum 61.84 ms. The
separate ordinary-input test has a 72 µs median across 101 turns. These measure
arrival at the local client socket, not display presentation, SSH or an Agent's
application latency. [Raw fixture samples](docs/perf/attach-output-2026-09-15.json)
include the machine, command and sample values.

Queue tests retain a 2 MiB frame through partial writes and verify exact bytes,
mode-frame ordering and complete allocation release. Already-written prefixes
remain included in the retained-byte count. Ordinary backlog is bounded to
1 MiB/64 references; one larger valid frame permits only 64 additional bytes for
modes/control until it drains. Overflow closes the stream instead of splicing a
new frame into a partially transmitted one. A reconnect receives a FullSnapshot
from the unchanged process. These are explicit queue-allocation bounds, not RSS
or whole-application memory measurements.


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

## Remote history contention (2026-09-15)

A release-mode Engine regression at `main` (`0f5905f`) pauses a disposable
remote Holder for 800 ms while the desktop requests history. Before the fix,
the request blocked the control connection and held the Registry lock used by
terminal input and screen publication. Moving just the request to a background
worker freed Hello but left input blocked on that lock.

The fixed Engine uses its existing bounded background pool and pins the
history reader to the original session before releasing the Registry. In the
same fixture, binary attach input followed by a local Pong took **748 ms on
main and 34 µs with the fix**. The Pong measures forwarding through the Engine,
not remote PTY echo; the test separately checks that the complete input reaches
the same Agent after the Holder resumes. The history response still takes
about 805 ms. No real VPS/network latency is included in these measurements.

```sh
cargo test --locked --release -p diri-remote --test engine_remote_e2e \
  remote_scrollback_does_not_block_input_or_screen_reads -- --exact --nocapture
```

This local fixture creates and cleans up its own Helper, Agent, sockets, and
fake SSH home. It requires no configured SSH host. Live Helper versions and
the remote protocol are unchanged.

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
current implementation goes further: status glyphs are static, so they never
create autonomous frame tasks. Real status changes and terminal grid damage
still repaint immediately.

The terminal cursor is the one bounded exception (`diri-term/src/cursor_motion.rs`).
It is solid while in use, and only a focused, visible cursor that has been idle
for 500 ms blinks: ten eased 1.2 s cycles, then it rests solid and schedules
nothing, so a terminal left alone still paints zero frames. Each hold sleeps
on one one-shot wake aimed at the fade that follows it; the fades themselves
ride the display link (since 2026-09-28; they were 33 ms steps before, an
estimated 24–30 fps once vsync rounded them). At 120 Hz that is 52 frames per cycle,
521 in total over the 12 s after the last activity, against 121 for the
stepped fades (`an_idle_cursor_paints_a_bounded_number_of_frames_then_none`).
A glide is 80 ms of display-rate frames after a keystroke that was going to
repaint the row anyway. Neither invalidates the row cache: the cursor is
sampled after rows are prepared and painted last. Under Reduce Motion the
cursor is the static block it was before. The window and root surface
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

## 2026-09-15 — Remote responsiveness under contention

The remote responsiveness regressions use local disposable Helpers, fake SSH,
Unix sockets, and PTYs. They isolate Engine/Holder scheduling from WAN latency.
Baseline is `436a6a0` (the remote scrollback fix with main merged).

| Trigger | Before | After |
| --- | ---: | ---: |
| SSH launch delayed 800 ms; input to another session | 923 ms | 42 ms |
| SSH kill delayed 800 ms; input to another session | 935 ms | 32 ms |
| Paused SSH bridge; accept a 256 KiB paste | 802 ms | 5–10 ms |
| Continuously readable 8 MiB source; one owner turn with 1 ms consumption per read | 215 ms | 1.6 ms |
| Checkpoint submission with an injected 200 ms persistence callback | 202 ms synchronous | < 1 µs queued |

The first three rows exercise the real control/SSH paths. The last two are
controlled scheduling harnesses at the production drain and persistence seams;
their injected delays are not measurements of a particular VPS or disk. The
paste test verifies all 262,144 accepted bytes arrive after SSH resumes without
requiring another keystroke to wake the writer. Archive and remove keep peer
input below 40 ms under the same delayed management command. A failed live
stop retains the tracked session.

On the local release Helper fixture, input during continuous output reached
the grid in 11.4 ms. A 2 MiB output burst, including its final grid before exit,
drained in 342 ms during a parallel test run. These are regression observations,
not a controlled throughput comparison or WAN performance promise.

The existing release latency gate passed with 12 snapshot and 32 interaction
samples: snapshot p90 1 µs, input-to-PTY p95 3.65 ms, output-to-diff p90 13 µs,
loopback median 74 µs / p90 471 µs. Timing varies with host load; the assertions
retain the existing release budgets.

The deferred SSH queue uses the existing pump rather than another thread. Each
Engine remote client and Holder adds one metadata worker with a 128 KiB stack,
one active immutable checkpoint and one pending latest checkpoint. Workers
sleep on condition variables and generate no idle timer wakeups. The tradeoff
is bounded thread/stack and snapshot storage to remove filesystem sync from
interactive processing. Disk errors, ordered final writes, coalescing, stale
incarnations, partial input writes, reconnect ordering across Hello/control
grant, overflow, and exact exit-tail preservation
have regression coverage. Management stop fences checkpoint writes using the
existing launch lock; a repeated output/kill/GC test checks that interrupted
metadata cannot leave nonce files or a stale final output offset.

Commands (run from `diri/`):

```sh
cargo test --locked --release -p diri-remote --test responsiveness -- --nocapture
cargo test --locked --release -p diri-remote --lib continuously_readable_output -- --nocapture
cargo test --locked --release -p diri-pty checkpoint -- --nocapture
scripts/remote-perf-gate.sh
cargo test --locked --release -p diri-remote --test holder_e2e slow_attach_never_blocks_pty_and_reconnects_from_full_snapshot -- --ignored --exact --nocapture
```

Helper environment-capture fixtures require debug assertions; run the complete
remote suite through `cargo test -p diri-remote`, not an unfiltered release run.
New Holder scheduling applies to new sessions. Existing remote Helpers retain
their original Build IDs; the Engine-side fixes apply after the Engine updates.


## Remote disk and network follow-up — 2026-09-15

`diri-remote::output_log::tests::append_latency_gate` appends 128 MiB in 2,048
64-KiB chunks through the actual Holder log, including multiple capacity wraps.
It fails if an append takes 16 ms or longer. On this Mac the original copy/sync
rotation reached 20.88 ms (median 6.75 µs). Rename-based prototypes still paused
20–60 ms on Forge's disk; reducing segment sizes did not remove those stalls.
The final circular format avoids file creation/deletion during append. Two
Forge disk runs measured maxima **0.732 ms / 0.174 ms**, p99 **91.6 / 71.8 µs**,
and medians **53.8 / 52.9 µs**. Checksumming increases ordinary append CPU work;
there is no extra logging worker, queue, or retained-payload copy. These are
observed host timings, not a guarantee against arbitrary storage/scheduler stalls.
Use `TMPDIR` on the filesystem being evaluated: Forge's `/tmp` is RAM-backed.

Reproduce from `diri/`:

```sh
cargo test -p diri-remote --release --lib append_latency_gate -- --ignored --nocapture
scripts/remote-netem-gate.sh
DIRI_REMOTE_SSH_TARGET=disposable-host DIRI_REMOTE_SOAK_SECONDS=5 scripts/remote-ssh-soak.sh
```

The Linux-only netem test creates its own user/network namespace and verifies
both differ from the caller's before touching its sole loopback interface.
It requires `unshare`, permitted unprivileged namespaces, and kernel netem;
it installs no packages or services and never changes host networking.
The Rust fixture forwards the real Helper protocol across TCP with 70 ms delay
per direction, 15 ms jitter, and 2% loss. Kernel counters must confirm actual
dropped packets. It verifies exact replay, input acknowledgements, on-demand
history, and reconnect with unchanged Agent identity, then reaps its fixture.
The default test suite never runs this test or contacts a real SSH host.

Measured fixture results: baseline median/p90 **0.195 / 0.344 ms**; impaired
median/p90/max **140 / 570 / 721 ms**, history **322 ms**, **19 packet drops**.
This TCP fixture isolates loss/retransmission behavior; the separate real SSH
soak exercises OpenSSH, bootstrap, Engine mirroring, and detach persistence.
Over the actual Mac-to-Forge connection, 32 input-to-grid samples measured
median/p90/max **187 / 304 / 357 ms**, and history retrieval **220 ms**. The
sampler polls every 20 ms and does not measure display/input-to-photon latency.

For a cross-platform real SSH run, `DIRI_REMOTE_ARTIFACT_MANIFEST` selects the
complete verified packaged catalog instead of executing a native test Helper.
`DIRI_REMOTE_HELPER_PATH` can select a native Helper when running copied Holder
test executables on another machine. The SSH soak installs a versioned Helper
beside existing builds, creates a nonce-named shell session, and kills only that
session on completion/failure. Installed Helper builds remain available for GC.

Final verification after merging current `main` repeated the same gates with
format 3: Forge append max **0.813 ms** (median **53.6 µs**), impaired TCP
median/p90/max **144 / 165 / 493 ms**, history **141 ms**, and **9 packet drops**.
The real SSH rerun measured median/p90/max **138 / 191 / 275 ms** and history
**197 ms**. These differing WAN/loss samples are repeatability checks, not a
controlled before/after network-speed comparison. Workspace validation passed
**1,612 tests** (34 intentionally ignored), formatting, clippy, and release build;
Linux-specific remote clippy, local Holder latency/load/slow-attach gates, and
the signed three-platform app bundle/catalog verification also passed.


## Multiplexed local preview capacity — 2026-09-16

The same `previewbench` release binary compares separate preview sockets with a
single receive-only connection for the active preview set. This experiment
compiled the admission constant at 64 in a disposable build; production remains
at 16. Each run lasts three seconds after a one-second unattached idle sample,
uses alternating 80×24 and 160×50 grids, and retains one deliberately stalled
single-session preview plus one normal interactive attachment in both modes.
The shared queue's overflow/partial-frame and reseed behavior has separate tests.

| Previews / Hz | CPU cores separate → mux | RSS MiB separate → mux | Input p95 ms separate → mux |
|---|---:|---:|---:|
| 48 / idle | 0.010 → 0.005 | 43.6 → 35.6 | — |
| 48 / 10 | 0.720 → 0.671 | 77.8 → 65.6 | 7.866 → 6.326 |
| 48 / 60 | 2.241 → 1.671 | 122.7 → 116.3 | 3.456 → 1.000 |
| 64 / idle | 0.013 → 0.006 | 54.6 → 44.2 | — |
| 64 / 10 | 0.938 → 0.840 | 96.1 → 83.6 | 9.106 → 6.615 |
| 64 / 60 | 3.299 → 2.194 | 167.3 → 148.2 | 2.256 → 1.224 |

All continuously drained readers survived all twelve runs; the stalled legacy
preview disconnected and reseeded without changing its process identity. At
64/60, small/large grids delivered 54.4/53.1 distinct producer images per second
with multiplexing, versus 53.2/52.0 separately. Producers completed 159–170 frames
in the multiplexed interval, so this is not evidence of sustained 60fps output.
At 64/10, multiplexed output p90 was 20.2/19.9 ms versus 16.8/17.3 ms separately;
lower aggregate cost does not imply lower latency for every workload.

The boundary includes Engine and blocking benchmark decoder threads, not GUI,
SSH, or producer CPU. With 64 previews, unconnected baseline was 67 threads;
separate connections reached 260, multiplexing 137. Of the 123-thread reduction,
62 are benchmark decoder threads (the production client already uses Tokio),
and 61 are Engine connection threads. Existing per-session publishers remain.
Idle decoder deadline checks also contribute to the reported CPU, so idle values
are not Engine-only wakeup measurements. Runs were paired while other builds
were held on Mac16,5 / 36 GiB / macOS 27.0 / Rust 1.97.1.

Full per-dimension timing, producer progress, reader survival, and allocation
samples are in `docs/perf/preview-multiplex-2026-09-16.json`. Reproduce the checked-in
16-subscription configuration from `diri/`:

```sh
cargo build --release -p diri-engine --example previewbench
./target/release/examples/previewbench 16 60 3 diagnose
./target/release/examples/previewbench 16 60 3 diagnose mux
```

## Keystroke echo in held sessions — 2026-09-28

Every local session is Holder-backed, and every keystroke's echo waited out
the Session pump's 8 ms output batch before it was published. The pump reads
the Holder's output stream and treats only an empty poll as proof that a burst
has ended, so a lone echo sat until `OUTPUT_BATCH_CEILING` expired (plus the
PTY-fact `stat` round trip that runs between reads). The Direct-PTY path never
had this; the existing input-to-grid test only covered that path.

The pump now publishes output that answers input written in the last 100 ms
(`ECHO_WINDOW`) as soon as it is parsed, unless the screen lost cells doing it,
which is the half-erased repaint batching exists to hide. An editing key
(DEL, BS, ^W, ^U) may clear up to one row. Streaming output and repaints keep
the existing batching; each keystroke buys at most one immediate publication.

Measured with `tests/keystroke_latency.rs` (debug build, `cat` echo through a
real Holder, 300 keys paced 40 ms apart, loaded machine):

| hop | before p50 | after p50 |
| --- | ---: | ---: |
| client send → input frame decoded | 0.04 ms | 0.04 ms |
| decoded → Holder acknowledged the write | 0.06 ms | 0.05 ms |
| decoded → echo received from Holder | 0.12 ms | 0.13 ms |
| echo received → grid published | **9.09 ms** | **0.09 ms** |
| published → frame queued to client | 0.07 ms | 0.07 ms |
| queued → client decoded | 0.03 ms | 0.02 ms |
| **end to end (send → grid decoded)** | **9.33 ms** | **0.29–0.36 ms** |

Reproduce from `diri/` (`DIRI_KEY_LATENCY_CHILD=zsh` for a real line editor):

```sh
cargo test -p diri-engine --features latency-trace --test keystroke_latency \
    -- --ignored --nocapture
```

`attach::a_held_session_publishes_an_echo_without_waiting_out_the_batch`
guards the regression (median 9.3 ms before, sub-millisecond after, asserts
≤ 5 ms).

## Working marks without rendering — 2026-10-02

The installed 0.9.1 app sat at 15–23% CPU with two agents working and nothing
else moving. An Instruments trace showed about 23 frames a second at about
5 ms each, and the sidebar re-rendering in about 14 of them: every 125 ms
working-mark step notified the working rows, a notify marks every ancestor
dirty, and the sidebar re-ran taffy layout over its whole tree to swap one
14px sprite.

The eight working frames are now rasterized once and swapped inside the
already-drawn scene (`Window::paint_animated_svg` in the vendored GPUI, see
`vendor/gpui/DIRI_PATCHES.md`). On an idle display refresh the window only
checks the clock; on a step boundary it rewrites the marks' tiles and
presents the same scene again. Nothing renders, lays out or paints, and the
sidebar's 8 Hz tick now runs only for indeterminate progress sweeps.

| per working-mark step, 51 sessions / 4 working, release | main thread |
| --- | ---: |
| before: sidebar render (cached rows reused) | 1.10 ms |
| after: tile swap in the drawn scene | 0.0001 ms |

Presenting the frame (GPU encode) is the same in both. Reproduce from `diri/`:

```sh
cargo test --release -p diri-app --bin diri -- --ignored --nocapture \
    --test-threads=1 working_mark_step_cost
```

`swapped_working_mark_frames_paint_like_a_full_render` checks the swapped
frames pixel for pixel against full renders under headless Metal, and
`working_marks_advance_without_rendering_anything` asserts that a step
renders no view.

## Sidebar rows outside the viewport are not built (2026-10-04)

**Where the time went.** Session rows are cached views (#517), but the list
still mounted every row each sidebar render: props computed and compared, a
view and its wrapper laid out, prepainted, and its cached ranges replayed. A
notify the props cannot account for (`rows_stale`) re-rendered every row, and
GPUI notifies the sidebar on every scroll-wheel step, so scrolling a long
list rebuilt all of it per frame. Even a root-only frame replayed every row.

**What changed.** The list walks its rows top to bottom with a content-space
cursor (`sidebar/row_window.rs`, the idea of Ely GPUI's `VirtualList`) and
builds only the rows within the viewport plus overscan (half a viewport,
at least 240 px, each side). Each skipped run becomes one spacer of the same
height, so the column lays out exactly as before and the scroll container,
section lifts and shifts, disclosures, drops and edge fades are unchanged.
Held rows are always built: selection, keyboard cursor, hover and its card,
rename, delegation mark, rows a drag carries, and settling titles. Motion
that moves rows off their walked slot (a disclosure, a section shift, a
lifted project, a row growing in) builds every row while it runs. The
keyboard reveals unbuilt rows from their walked slots.

**How it was measured.** `sidebar_fleet_render_cost` (root.rs, ignored,
macOS, headless Metal) now takes `DIRI_BENCH_PROJECTS` and has three more
cases: `sidebar-notify` (a notify that stales every row), `scroll-step` (one
wheel step: offset moved and the sidebar notified) and `full-refresh`
(`window.refresh()`). Release build, 150 steps, 4 working sessions, 20
projects, step median (render + layout + prepaint + paint):

| Case | 50 sessions | 200 sessions | 1,000 sessions |
| --- | ---: | ---: | ---: |
| Scroll step | 2.67 → 2.51 ms | 5.83 → 2.93 ms | 32.75 → 5.28 ms |
| Sidebar notify (rows stale) | 2.61 → 2.21 ms | 5.84 → 2.54 ms | 33.10 → 4.87 ms |
| Full refresh | 2.92 → 2.59 ms | 6.28 → 2.82 ms | 34.52 → 6.20 ms |
| Activity tick | 1.98 → 1.84 ms | 2.90 → 2.10 ms | 9.26 → 4.13 ms |
| Store publication (nothing changed) | 1.93 → 1.84 ms | 2.90 → 2.22 ms | 9.94 → 4.16 ms |
| Root-only frame | 0.36 → 0.33 ms | 0.79 → 0.43 ms | 3.69 → 2.24 ms |

Rows built per stale step fell from 50 / 200 / 1,000 to 28 / 36 / 39. At a
realistic 50 sessions the win is small; it grows with the list, and at 1,000
sessions a scroll step goes from below 30 fps to well inside one frame.
What remains at 1,000 is O(sessions) work outside the rows (projection,
focus rows, the walk itself).

**Visual checks.** `windowed_sidebar_rows_paint_like_every_row_built`
captures 1,000 sessions scrolled into the middle with windowing on and off
and requires identical pixels. Unit and GPUI tests in `row_window.rs` and
`sidebar/view/windowing.rs` check that walked slots match painted bounds in
both groupings (archives, collapsed projects, the preview fleet), that every
row in view is built after scrolling, keyboard reveal of a never-built row,
shift-click ranges across unbuilt rows, and a drag whose source scrolls out
of view.

```sh
DIRI_BENCH_SESSIONS=1000 DIRI_BENCH_PROJECTS=20 DIRI_BENCH_ITERATIONS=150 \
  cargo test --release -p diri-app --bin diri sidebar_fleet_render_cost -- --ignored --nocapture
cargo test --release -p diri-app --bin diri -- --ignored --exact \
  root::tests::windowed_sidebar_rows_paint_like_every_row_built
```

## One process coalition per Agent, at no memory cost (2026-10-04)

Force-quitting a Chrome that an Agent's browser MCP server had launched
SIGTERMed 184 processes, every session included: LaunchServices force-quits
an app's whole process coalition, and every Agent descended from the one
Holder manager, so all of them, and that Chrome, shared one.

On macOS a bundled Engine now has the manager start each Agent as a launchd
job of its own (a fresh coalition) through diri.app's own executable, which
forks, takes the PTY as its controlling terminal and execs the Agent. The
job's process exits at once; TCC keeps crediting diri.app through the fork
and the exec. The manager keeps the PTY and is itself a launchd job, outside
any app launch's coalition.

**Memory and idle cost: none.** The process tree per session is unchanged
(the Agent and what it starts); no trampoline, Holder or guard is added. A
per-session-process design measured 6.2 MB a session (trampoline 3.3, holder
1.6, guard 1.3) against the shared manager's 4.2 MB for a whole fleet, and was
dropped for this one.

**Launch cost** (15 launches each, release, `/bin/sleep` Agent):

| Spawn | p50 | p90 | first (cold) |
| --- | ---: | ---: | ---: |
| detached (launchd job) | 31.1 ms | 33.9 ms | 752 ms |
| direct (Holder's child) | 1.0 ms | 1.3 ms | 1.6 ms |

Paid once per Agent start, which itself takes seconds.

```sh
cargo build -p diri-app --bin diri
DIRI_AGENT_LAUNCHER=$PWD/target/debug/diri \
  cargo test -p diri-engine --test detached_agent -- --ignored
```


## Output reaches the screen a frame sooner (2026-10-27)

**What was measured.** One byte written to a `cat` terminal's TTY from
outside (no key event, no input frame), timed to the screen. A capture of
the installed 0.10.1 app put that at 38.9 ms median (p90 53.7, min 25.0); a
reference terminal on the same Mac draws it in 18.5 ms. Both present at
about 9 ms per frame, so Diri was spending about two extra frames.

`examples/output_photon_probe.rs` (diri-term) reproduces the path with
production code except the app chrome: a private in-process Engine, a real
Holder, data-channel grid diffs, `TerminalElement`, the vendored GPUI window
with its `CVDisplayLink`, and Metal, with `addPresentedHandler` for the time
on glass. Its window is a non-activating panel kept almost entirely off
screen. It alternates `x` with `\b \b`, like the original capture. The
Engine's own hops come from `output_to_grid_latency_by_hop`
(tests/keystroke_latency.rs).

**Where the time went** (probe, before, p50):

| hop | `x` | `\b \b` |
| --- | ---: | ---: |
| TTY write → Holder → Engine received | 0.06 ms | same |
| Engine received → grid published | **9.0 ms** | **16–18 ms** |
| published → client decoded → applied | 0.05 ms | same |
| applied → frame starts (display-link wait) | **5–6 ms** | same |
| draw + commit | 0.2 ms | same |
| commit → presented (compositor) | 19 ms | same |

- The held pump read a lone write off the Holder's stream and then waited
  out the whole 8 ms batch ceiling for the empty poll that proves the burst
  ended. Keystroke echoes skip that wait; output nobody typed for did not.
- `\b \b` removes a cell, so it is held as a possible half-erased repaint
  for up to 16 ms (`RepaintHold`). That hold is the flicker protection from
  #67/#762 and is unchanged.
- Only a keystroke's echo asked for an immediate frame; other output waited
  for the next display-link tick.
- Commit to on-glass is about two refreshes for any present on this Mac.
  Neither `presentsWithTransaction` nor a third drawable changed it (both
  19 ms). Turning `displaySyncEnabled` off cut it to ~5–10 ms but, under
  continuous output, presented frames bunched (1.5 ms apart) with long gaps
  in between, which would show as judder. That change was not made.

**What changed.**

- The held pump publishes a pending, non-destructive batch after 1 ms of
  silence (`OUTPUT_QUIET`) instead of waiting out the 8 ms ceiling, provided
  the last publication is at least one ceiling old. Continuous output is
  still published at most once per 8 ms, and anything that lost content
  still waits for its redraw. Writes that span several PTY reads, or several
  writes issued back to back, still fold into one publication.
- Every visible terminal change asks GPUI for an immediate frame, not only
  a keystroke's echo. The platform still refuses while the last present is
  under two refresh intervals old, so streams stay on the display link and
  no frame is drawn that the link would not have drawn.

**Results.** Engine, `output_to_grid_latency_by_hop`, release, 200 writes:
TTY write → grid at client 9.11 → **1.57 ms** p50 (p95 9.42 → 4.93 ms).

Probe, three interleaved rounds of 60 writes each, before / after (p50 per
round; load average 170–410 from other builds):

| | before | after |
| --- | --- | --- |
| `x`: write → presented | 37.4 / 40.3 / 37.3 ms | **27.5 / 26.5 / 27.3 ms** |
| `\b \b`: write → presented | 48.1 / 46.9 / 46.0 ms | 43.7 / 43.6 / 43.8 ms |
| all: write → presented | 43.0 / 43.5 / 43.2 ms | 37.2 / 36.5 / 35.9 ms |
| `x`: write → grid decoded | 10.2 / 10.5 / 10.3 ms | **1.7 / 1.5 / 1.5 ms** |
| applied → committed | 6.1 / 6.2 / 5.3 ms | **0.2 / 0.2 / 0.2 ms** |
| commit → presented | 19.0 / 19.0 / 19.0 ms | 23.0 / 23.2 / 24.1 ms |

The same probe filmed by ScreenCaptureKit at 120 Hz (the method of the
original 38.9 ms figure: SCK `displayTime` minus write time), panel on screen
and non-activating, blurred background like the default Glass window, three
interleaved rounds of 80 writes each, p50 / p90 / min:

| | before | after |
| --- | --- | --- |
| `x` | 21.9–22.3 / 26.4–28.8 / 15.3–16.1 ms | **9.9–11.2 / 14.3–16.9 / 4.9–5.8 ms** |
| `\b \b` | 27.8–29.7 / 31.7–34.2 / 23.1–23.5 ms | 25.1–28.1 / 30.3–31.6 / 20.9–21.3 ms |
| all | 25.0–25.9 / 31.2–32.3 ms | 21.3–23.3 / 29.1–29.9 ms |

The probe has no app chrome, so it starts ~13 ms below the installed app
(25.5 vs 38.9 ms before). That difference (a busy main thread, animations
keeping the display link warm so immediate frames are refused) is not
attributed here.

The immediate frame removes the display-link wait, but a frame committed
mid-interval still reaches the glass at the compositor's next deadline, so it
gains ~2 ms net, not the ~5 ms wait. The Engine change is the larger part.

**Footprint.** No per-session state beyond one `Option<Instant>` on the
pump's stack, no new thread, timer or wakeup. An idle session's pump takes
the same blocking wait as before (the quiet window applies only while a
batch is pending), and an idle window draws nothing new: the immediate
frame replaces the display-link frame that output would have caused anyway.

**Not changed.** The 16 ms erase hold (flicker protection). The remote
Helper's own 8 ms coalescing of unsolicited output (`diri-remote`
`DIFF_COALESCE`) has the same leading-edge wait and is left for a Helper
change. Display sync, drawable count and transaction presents stay as they
were.

```sh
cargo build --release -p diri-engine --bin diri-holder
DIRI_HOLDER_BIN=$CARGO_TARGET_DIR/release/diri-holder \
  cargo run --release -p diri-term --example output_photon_probe -- --samples 60 --frames immediate --blur
cargo test -p diri-engine --release --features latency-trace \
  --test keystroke_latency output_to_grid -- --ignored --nocapture
```


## Output to the screen through the real app window (2026-10-27)

**Question.** `output_photon_probe` draws a bare `TerminalElement` in a
small panel. The installed app measured 38.9 ms median for the same TTY
write, against 25.5 ms for the probe on the same build, so about 13 ms was
attributed to "the app": the glass material, main-thread work, re-rendering
the whole window for a terminal frame, or the build.

**Harness.** `tests/app_photon_appkit.rs` (`src/app_photon_tests.rs`) runs
the full `RootView` (workspace workbench with a split, title bar, tab strip,
sidebar) in a 2056x1290 pt window with the app's own title-bar options and
material, over a private in-process Engine with real Holders. The window is
a non-activating panel at the right screen edge with a 6 pt strip visible,
opened only after launch so AppKit never activates the process; a guard
aborts the run if the process becomes active or owns the key window. Each
TTY write starts a `diri_client::latency_trace` record (the pane, transport
and GPUI frame observer stamp the rest) and is logged in `CLOCK_UPTIME_RAW`
ns for joining against a ScreenCaptureKit film of that window.

**Results** (SCK: write → first changed captured frame; two interleaved
rounds of 60 writes per build; load average 5–23):

| p50 / p90 ms | `x` | `\b \b` | all |
| --- | --- | --- | --- |
| bare panel, #833 | 11.8 / 17.0 | 28.9 / 33.2 | 21.7 / 31.6 |
| full app window, #833 | 16.8 / 21.2 | 32.7 / 37.2 | 24.6 / 35.5 |
| full app window, main | 30.2 / 35.7 | 36.6 / 43.0 | 33.6 / 42.0 |

Hops in the full window, `x`, p50:

| hop | main | #833 |
| --- | ---: | ---: |
| write → grid decoded | 9.0 ms | 1.3 ms |
| decoded → pane notified | 0.05 ms | 0.05 ms |
| notified → draw start | 0.02 ms | 0.02 ms |
| draw (layout + paint, whole window) | 0.55 ms | 0.5–0.8 ms |
| draw end → committed | 4.1–6.1 ms | 0.1 ms |
| committed → GPU completed | 3.5 ms | 2.9–3.9 ms |
| GPU completed → presented | 19–23 ms | 19–21 ms |

- The window costs about 5 ms over the bare panel with #833 (8 ms on main,
  where draws also wait for the display link). The 38.9 → ~33.6 ms rest of
  the installed figure was not reproduced here: the installed 0.10.1 build,
  on-screen composition of a fully visible window, or a busier app.
- Glass vs opaque material: no difference (`x` 16.1 vs 18.4 ms SCK, GPU 3.0
  vs 4.2 ms, within run-to-run noise). Sidebar on vs off: no difference.
- No main-thread stall: a notified pane starts drawing within 0.02 ms (the
  immediate frame is granted), and building the whole window's frame takes
  0.5–1 ms.
- The GPU is the measurable app cost. A frame draws 49 quads covering about
  35 Mpx of a 10.6 Mpx drawable (window fill, workbench fill, two pane
  surfaces, the unfocused pane's dim). Dropping the five largest (an
  experiment, not a change) cut committed → GPU completed from ~3.0 to
  ~1.0 ms and SCK `x` by ~2 ms (n=15). A 1100x700 window takes 1.5 ms.
  Those layers are the design's glass composition, so they were left.
- The test build links GPUI `test-support` (`framebufferOnly = NO`). A/B
  with the shipped setting showed no difference beyond noise.

```sh
cargo build --release -p diri-engine --bin diri-holder
DIRI_TEST_APP_PHOTON=1 DIRI_HOLDER_BIN=$CARGO_TARGET_DIR/release/diri-holder \
  DIRI_PHOTON_LOG=/tmp/app-photon.csv \
  cargo test --release -p diri-app --test app_photon_appkit
```

## Idle sessions hold no threads (2026-10-27)

With #839 an idle session cost no wakeups, but it still kept threads blocked
for its whole life: seven in the Holder manager (control socket, PTY reader,
log writer, exit watch, input lane, and an output writer plus a hangup
watcher per subscriber) and one Engine pump. At 100 idle shells that was 702
manager threads and 102 Engine threads.

**What changed.** Each process has one readiness thread (`holder/reactor.rs`:
kqueue on macOS, epoll on Linux, one-shot registrations). A Holder lane keeps
its thread while it has work and hands its descriptor to the reactor after
2 s idle (500 ms for the control socket, whose requests are one-shot); the
next event starts a thread again. The exit watch is a reactor registration
from the start. An idle held Engine pump (the #839 park point) moves its loop
state off the stack and hands its own kqueue to the reactor, with the 300 s
backstop as a kqueue timer; a frame, a hangup, the doorbell or the timer
resumes it on a new thread. At any moment exactly one party owns each lane's
descriptor or a pump's state. The input lane keeps interactive QoS; other
lanes run at the QoS sessions were started with, not the reactor's.

**Numbers.** `idlebench <n> 0 30 40` (interactive `zsh -f`, real
`diri-holder --manager`), base (#839) and branch alternated, two rounds, load
2.6–8. Idle values after 35 s; latencies are p95 of both rounds.

| | n=1 base → branch | n=10 | n=100 |
|---|---|---|---|
| Engine threads | 3 → 3 | 12 → 3 | 102 → 3 |
| Manager threads | 9 → 3 | 72 → 3 | 702 → 3 |
| Manager footprint | 1.6 → 1.4 MB | 4.3 → 2.3 MB | 28.2–28.4 → 6.8 MB |
| Engine footprint | 5.3–5.8 → 5.6–5.9 MB | 7.7–8.8 → 7.8–8.0 MB | 32.2–32.5 → 32.7 MB |
| Manager fds | 15 → 15 | 105 → 96 | 1005 → 906 |
| Idle 30 s: syscalls Engine / manager | 13 / 0 → 13 / 0 | same | 13 / 0 → 13 / 0 |
| Echo after 40 s idle | 0.05–0.40 → 0.39–0.49 ms | 0.33–1.25 → 0.36–0.52 ms | 0.46–0.47 → 0.52–0.57 ms |
| Command output after idle | 0.33–0.40 → 0.42–0.48 ms | 0.52–1.02 → 0.45–0.66 ms | 0.54–0.64 → 0.79–0.84 ms |
| Echo while typing (keys 50 ms apart) | 0.70–0.79 → 0.53–0.70 ms | 0.57–0.73 → 0.63–0.72 ms | 0.79 → 0.86 ms |

The first keystroke after an idle period pays for thread starts in both
processes: about +0.06–0.1 ms on echo p95 and +0.2 ms on the first command
output at n=100. Active typing is within run-to-run noise. Engine memory does
not move: its pump threads' stacks were mostly untouched pages. Parking is a
one-time transition (about 5 syscalls per lane), not a tick.

`fleetbench payload 32 MiB, 160×50`, three alternating rounds: 1 session
84.0/94.3/90.4 → 94.0/92.6/95.1 MB/s, 4 sessions 137.2/142.4/140.5 →
138.0/142.8/137.5 MB/s. Throughput is unchanged; streaming lanes never park.

Not changed: remote and direct (non-held) Engine pumps, and the ~2 Engine
threads per attached pane.

## Line edits skip the erase hold (2026-10-28)

**Premise check.** A full-window probe (#863) put an unsolicited `\b \b`
written to a TTY at 32.7 ms to the screen against 16.8 ms for `x`, almost all
of it the held pump's 16 ms erase hold (`RepaintHold`). A *typed* backspace,
^W or ^U was already exempt (`EchoRequest::erases`, 0.2–0.3 ms to the attach),
so the hold did not cost every typed backspace. It did cost every key whose
answer shortens the line without being an editing key, and every line edit
nobody typed for:

**Rule.** A batch whose only row holding fewer cells than at the batch's start
is the visible cursor's row, and that row still holds something, is a line
edit (`HeadlessScreen::lost_cells_only_on_cursor_row`). It is batched like
output that adds and counts as any key's answer. A hidden cursor
(full-screen programs mid-draw), a cursor row cleared to nothing (`less`
clearing `:` before its next page, a progress line cleared before its text),
a scroll that shortened another row, or a resize keeps the hold. The remote
Holder applies the same predicate to its 8 ms destructive coalescing.

**Results.** `tests/erase_hold_probe.rs` (ignored; private Engine, real
Holder, 100x30, data-channel attach, input → first changed published frame;
two interleaved rounds per build, load average 14–31):

| event | main p50 | branch p50 |
| --- | ---: | ---: |
| `\b \b` written to a `cat` TTY after a prompt | 18.2–18.3 ms | **1.4 ms** |
| progress line rewritten shorter (one write) | 18.2–18.3 ms | **1.5–1.6 ms** |
| bash / zsh Up onto a shorter history entry | 18.3–18.7 ms | **0.2–0.3 ms** |
| fish letter that ends an autosuggestion | 18.3–18.5 ms | **0.3–0.5 ms** |
| typed backspace, ^W, ^U (bash, zsh, fish) | 0.2–1.1 ms | 0.2–0.3 ms |
| `clear`, `less` page, vim `dd` / `x` / ^F | unchanged (held where they lose rows) | unchanged |

Half-repaint frames (a short-lived frame a row emptier than both neighbours,
or a row that blinked): 0 on both builds for cat, bash, zsh, fish, vim, top
and Claude Code; `less` 3 and 2 of ~38 frames on both builds (a page published
mid-scroll, pre-existing). A first version without the "row still holds
something" condition published `less`'s cleared prompt alone (6 extra
flashes); `line_edit.rs` `a_prompt_row_cleared_for_its_redraw_is_still_held`
guards that.

**Footprint.** One `Vec<usize>` of row counts per screen (8 bytes per row,
~240 B at 30 rows), refilled without allocating when a batch opens; the
O(rows) check runs only once a batch has lost cells. No thread, timer or
wakeup.

```sh
DIRI_ERASE_PROBE_CLAUDE=/path/to/isolated-claude.sh \
  cargo test -p diri-engine --release --test erase_hold_probe -- --ignored --nocapture --test-threads=1
```

## Idle history on disk (2026-10-29)

**Question.** An idle session keeps its whole compact history (up to 4 MiB of
live bytes, about 3.7 MB per 100,000 styled lines) in the Engine. At 100
sessions × 100,000 styled lines the Engine measured 4.95 MB per session,
against 1.4 MB for a reference multiplexer that suspends idle terminals to an
encrypted file.

**What changed.** After 60 s with no output and no history read, a held
session's pump pages its sealed history blocks out to an AES-256-GCM sealed,
owner-only swap file, on the idle park's own timer (no new timer, thread or
periodic wakeup). Reads load one block at a time and keep nothing. Payloads
now live in per-history 128 KiB mappings, so the freed memory leaves the
process. Design, file format and lifetime: `docs/idle-history-swap.md`.

**Numbers.** `idlebench <n> <lines> 75` with `IDLE_FILE` set to a styled log
(dim line numbers, 4 SGR colors, a 256-color module path, bold numbers;
11.4 MB per 100,000 lines), 120×40, real `diri-holder --manager`; Engine
`phys_footprint` 80 s after the fleet went quiet (the page-out runs at about
62 s).

| sessions × lines | history in RAM, base → branch | Engine per session, base → branch | swap on disk | written at the idle transition |
| --- | ---: | ---: | ---: | ---: |
| 10 × 10k | 5.9 → 0.2 MB | 11.1–13.4 → 5.1–11.8 MB | 5.7 MB | 0.57 MB/session |
| 10 × 100k | 37.0 → 0.2 MB | 7.9–15.9 → 2.0–11.3 MB | 37.3 MB | 3.68 MB/session |
| 100 × 10k | 58.1 → 1.5 MB | 1.91 → 1.25 MB | 57.3 MB | 0.57 MB/session |
| 100 × 100k | 369.8 → 1.5 MB | 4.95 → 1.53 MB (1.13–1.53 in 3 runs) | 372.8 MB | 3.68 MB/session |

- At 10 sessions the per-session figure is dominated by 50–130 MB that the
  macOS allocator keeps cached after the replay and checkpoint transients, in
  both builds and different from run to run (three runs each shown as a
  range); the history column is the deterministic part. At 100 sessions that
  cache is amortized.
- Before payloads had their own mappings, paging 368 MB out of the 100 ×
  100k fleet lowered the footprint by only 190 MB (3.3–3.5 MB per session):
  the allocator's small regions stayed pinned by about 6,500 live rows
  interleaved with the freed payloads (`vmmap`: 223 MB free but dirty, 71%
  fragmentation). With the mappings the footprint falls by the history
  moved, at the page-out (483 → 118 MB in one trace).
- A second idle period after a wake phase (keys and `echo` in every session)
  wrote 0 bytes: less than 64 KiB of new history stays in memory.

**Reading paged-out history** (100 × 100k, after the page-out; resident is
the same build with `DIRI_HISTORY_SWAP=0`):

| read | resident p50 / p95 | paged out p50 / p95 |
| --- | ---: | ---: |
| oldest screen (40 rows, `read_scrollback_cells`) | 0.05 / 0.07 ms | 0.11–0.24 / 0.13–0.40 ms |
| newest history screen | 0.05 / 0.06 ms | 0.04–0.08 / 0.05–0.16 ms |
| checkpoint export (newest 10,000 rows) | 9.8 / 10.1 ms | 13.0 / 14.4 ms |
| Find capture | 1.5 / 1.9 ms | 2.2 / 2.4 ms |

The first fetched screen of paged-out history is under 0.5 ms at p95 in
every 100-session run. With ten sessions (ten samples, so p95 is the
slowest) it was 0.2–1.6 ms, and once 7.6 ms for a single read. One record
costs about 24 µs opened per read, 13.5 µs through a held descriptor
(`history_swap::tests::record_load_cost`); AES here is the crate's portable
implementation (no `aes_armv8` cfg). A page-out seals about 6.6 ms of CPU per
MB on the pump thread, once per idle period.

**Wake.** After 90 s idle with history paged out: keystroke echo p95
0.28 ms, command output p95 0.80 ms (10 × 100k), as before.

**Footprint of the change.** Per paged session: a 48-byte record per stored
64-row block (about 75 KB at 100,000 rows) and the block index, which was
already resident; no descriptor while idle; one extra pump wakeup per idle
period for sessions with at least 64 KiB of history. Disk: the compact
history size plus 24 bytes per block (0.6%), removed with the session. Every
history with sealed blocks maps one 128 KiB chunk, of which only written
pages are resident.

```sh
cargo build --release -p diri-engine --bin diri-holder --example idlebench
IDLE_FILE=styled-100k.txt DIRI_HOLDER_BIN=$CARGO_TARGET_DIR/release/diri-holder \
  $CARGO_TARGET_DIR/release/examples/idlebench 100 100000 75
# with IDLE_TRACE=1 for the footprint every 10 s, DIRI_HISTORY_SWAP=0 for resident reads
cargo test --release -p diri-engine --lib record_load_cost -- --ignored --nocapture
```
