# diri performance record

## A paste waits for a busy program instead of being lost (2026-09-29)

**What was wrong.** A paste into a program that was not reading yet (an agent
mid-turn, `sleep 5; cat`) filled the PTY's input buffer. The Holder gave up
once the PTY had taken nothing for one second, the Engine dropped the attach,
and the unread tail of the paste was lost. For that second the Engine held the
Registry lock, so every other session's typing and screen updates waited too.
A large paste into a program that *was* reading held the same lock for the
whole delivery: 1.4–1.7 s for 16 MiB. The remote Helper had the same loss in
another form: more than 1 MiB of unread input closed the attach with a
protocol error.

**What changed.**
- Local Holder input stream version 2. An input frame is acknowledged once
  the Holder has queued it, and the Holder delivers the queue in order for as
  long as the program lives. The queue is bounded at 32 MiB (two of the
  largest attach frames). A frame that does not fit is refused whole with a
  distinct answer, the stream stays open, and the Engine reports
  `input_queue_full` (control) or ends the attach with `attach.input_refused`
  telemetry. With nothing queued, a keystroke is written straight to the PTY
  as before; only what the PTY has no room for goes to a drainer thread, which
  waits in `poll` with no lock held and exits when the queue is empty. The
  pump reads output on its own thread throughout, so a program that is blocked
  writing output while its input waits is always drained.
- Version 1 and the legacy JSON write keep their semantics for old Engines
  (reply after the PTY took the bytes, fail after a second without progress),
  but the bytes of a write that gives up stay queued instead of being lost.
  A new Engine asks for version 2 and falls back to version 1 on a Holder
  started by an older build.
- The Engine writes input outside the Registry lock. `Session` is now a
  cloneable `SessionCore` plus its pump thread; attach and control take a
  clone under the lock (after the keyboard-controller check and the wake) and
  write after releasing it. Order within a session is kept by the Holder
  client's input lock. `session.send_text`'s 30 ms submit settle and the
  message-receipt SQLite writes no longer run under the lock either.
- Remote Helper: past 1 MiB of unread input it stops handling controller
  messages, keeps them in order, and stops reading the socket until the
  program catches up, so the backpressure reaches the Engine through SSH. The
  Engine's own 1 MiB remote queue is unchanged and still refuses explicitly
  beyond it, which old live Helpers need.

**Measured.** A real Engine with real Holders (release), one attach frame
pasted into a raw-mode `head -c N > file` while a second session is typed
into every 10 ms. "Busy" means the program starts reading 1.5 s after the
paste lands. Exact means the file equals the paste byte for byte. Three
alternating base/branch runs per case; base is main with #560 and #561 merged.
Load average 6–10, and 20–290 during the busy cases (another agent's build).

| Case | main: delivered | main: other echo max | branch: delivered | branch: other echo max |
|---|---|---:|---|---:|
| 1 MiB, reading | exact, 63–65 ms | 48–56 ms | exact, 62–67 ms | 3–33 ms |
| 4 MiB, reading | exact, 221–230 ms | 208–215 ms | exact, 225–229 ms | 5–14 ms |
| 16 MiB, reading | exact, 1.42–1.72 s | 1.38–1.66 s | exact, 1.47–2.00 s | 10–148 ms |
| 1 MiB, busy 1.5 s | lost 2 of 3 | 0.74–1.10 s | exact, 1.02–1.54 s | 0.4–153 ms |
| 4 MiB, busy 1.5 s | lost 3 of 3 | 0.99–1.31 s | exact, 0.98–1.69 s | 0.5–12 ms |
| 16 MiB, busy 1.5 s | lost 3 of 3 | 0.99–1.00 s | exact, 2.21–2.25 s | 2–8 ms |

Other-session echo p50 was 0.1–0.6 ms on main and 0.10–0.13 ms on the branch.
CPU for a delivered paste, from `getrusage` (Engine) and `ps` (Holder
manager, 10 ms grain): 16 MiB reading, Engine 57–62 ms on main against 77–90
ms on the branch, Holder 0.81–1.05 s against 0.87–0.89 s. The Engine figure
includes the echo probe, which completed 6× more keystrokes on the branch
because they were no longer stalled, so it is not a like-for-like cost.
Keystroke input over the Holder stream (`holder_input_latency_is_reported`,
two alternating runs each): p50 10–11 µs / p95 13 µs on main, 10–11 µs / 13–15
µs on the branch.

**Not claimed.** Nothing about remote throughput: the remote change was
verified for correctness only
(`input_for_a_busy_program_waits_instead_of_closing_the_attach`). A refused paste in the desktop ends and re-opens the
attach rather than showing a toast; that needs an attach frame for Engine-side
refusals. Pastes over 1 MiB from the desktop are still refused by the
client's own 1 MiB command budget, so the 4 and 16 MiB cases here are the
Engine path (attach frames and `session.send_text`), not the app's paste.
Old live Holders still give up after a second and lose the tail; only
Holders started by this build queue. The Engine's 1 MiB remote queue still
refuses larger remote pastes, explicitly.

Reproduce from `diri/`:

```sh
cargo test -p diri-engine --test holder -- paste_into_a_program full_input_queue
cargo test -p diri-engine --test interactions a_paste_into_a_busy_program
cargo test -p diri-engine --lib negotiation_tests
cargo test -p diri-remote --test holder_e2e input_for_a_busy_program
DIRI_PASTE_CASE=16777200,1500 DIRI_PASTE_RUNS=3 \
  cargo test --release -p diri-engine --test interactions -- --ignored --nocapture --test-threads=1 paste_measurement
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
cargo test -p gpui_macos --release --lib echo_frame_scheduling -- --ignored --nocapture
cargo build --release -p diri-engine --bin diri-holder
DIRI_HOLDER_BIN=$PWD/target/release/diri-holder \
  cargo test --release -p diri-app --bin diri keystroke_latency -- --ignored --nocapture
cargo test --release -p diri-app --bin diri workspace_terminal_echo_redraw_cpu -- --ignored --nocapture
```

`only_a_keystroke_echo_asks_for_an_immediate_frame`,
`a_keystroke_buys_one_echo_frame` and
`immediate_frames_wait_until_the_last_present_is_on_screen` guard the policy.

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
nothing, so a terminal left alone still paints zero frames. Fades repaint in
33 ms steps from a one-shot wake instead of the display link: 12 frames per
cycle, 121 frames in total over the 12 s after the last activity (measured
headlessly against the element's `RendererStats`; the static cursor painted 1).
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
