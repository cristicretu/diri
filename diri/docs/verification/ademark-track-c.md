# ADEMark Track C reliability

The reference run uses ADEMark commit `119a5f7b9` and its Diri 0.11.2 profile. The headless checks below use Diri `323a10ea5`, the current `origin/main` at the start of the experiment. They exercise local Rust paths on macOS. ADEMark has no scores for this build in this experiment.

## Harness and scoring

Each task gets a fresh arena and thread. The profile seeds the project with `dirijor session spawn claude-code --cwd <arena> --title seed`, opens a default Claude Code tab with Cmd+T, pastes a prompt, and submits with Ctrl+M. A window recorder and OCR observe the 1600 × 1000 desktop. Track C requires a live model with network capture. The grader copies the finished worktree, restores pristine tests, adds hidden tests and runs `TZ=UTC npm test`. The C2 and C4–C6 task files set a 900-second cap even though TESTS.md describes 10 minutes.

| Test | What the runner does | Best published result |
| --- | --- | --- |
| C2 Message Delivery | At 30, 90 and 150 seconds, send one follow-up to the same thread. Pause typing probes for a three-second window around each send. Watch the thread for the first 24 characters for up to five seconds. After a miss, retry at one-second intervals with a ten-second give-up window. Save a queue screenshot. Search subsequent captured request bodies for each follow-up and grade its hidden test group. | 0/3 dropped, 3/3 applied. The capture predicate checks presence and cannot establish multiplicity. |
| C3 Chat Lock | During C1 and C2, the native timer types successive letters from `ademarkqz` roughly every two seconds (±20% jitter), checks pixel changes or OCR in the composer, and clears with Ctrl+U at segment boundaries. TESTS.md describes deletion after each character. Consecutive missing echoes count as one lock, ending at the next echo or the last probe. Missing probe data invalidates the row. | 0 locks, 0 seconds longest lock. |
| C4 Agent Killed | At 90 seconds, match exact executable basenames and working directories via ps/lsof and send SIGKILL to matching agent PIDs. Save a screenshot. If the fault found no agent, report N/A. | Survival 2, resumed 1, manual-continue 0. |
| C5 Network Cut | At 90 seconds, resolve the A and AAAA records of the configured API hosts three times, block those addresses in the benchmark machine’s pf anchor for 30 seconds, then clear it. | Survival 2, resumed 1, manual-continue 0. |
| C6 App Restart | At 90 seconds, quit the app, wait up to ten seconds for exit, relaunch and invoke the adapter’s calibrated return to the interrupted thread. The supplied Diri profile omits `steps.returnToThread`, so the runner emits invalid rows before starting the task. | With calibration, Survival 2, resumed 1, manual-continue 0. |
| A12 Leak | Sample the sum of the app group’s physical footprint at 4 Hz during Track C. The runner selects the first 30 minutes, takes medians of complete minutes, then fits ordinary least squares in MiB/hour with a 95% interval of ±1.96 standard errors. A shorter window is disclosed and fewer than three full minutes is invalid. | A continuous slope. There is no pass threshold or defined perfect score. A steady footprint gives zero. |

C4–C6 poll for changes to repository files or the agent session log every two seconds for up to 60 seconds after the fault ends. Without evidence of progress, the runner sends one manual `continue`. A finished task with the W3-cursor grade group passing and no manual input scores 2. A finished task with manual input or lost work scores 1. An incomplete task or crashed app scores 0. A failed survival run gets one repeat. Invalid grades invalidate Survival.

## Headless checks

The official CLI dry run finished with 14 rows for these six tests. All three C6 variants returned `not calibrated: returnToThread`. A measured run requires the dedicated macOS account named `ademark`. The official adapter opens and quits Diri by bundle identity and resets user-specific data. Its network fault changes the machine firewall. This experiment used private sockets, temporary state, PTYs and loopback replay. A terminal grid observation covers Engine parsing and attach delivery. Desktop focus, OCR, live-model work and hidden grading require a separate benchmark machine.

- **C2/C3.** The `composer_delivery` test sends binary attach frames through the production key and paste encoders. A PTY repaints a progress row while a separate file records delivered input. It checks all three echo deadlines and exact submitted bytes, including order, delimiters and multiplicity. The fixture passed 20 repeated runs. With the worktree-built Holder, Claude Code 2.1.328 and the upstream task wording, the three follow-ups appeared above the composer 514, 511 and 512 ms after paste, including the profile’s 500 ms paste-to-submit delay. The next main-thread model request contained each exactly once. All 69 C2 probes echoed in the desktop attachment’s grid within two seconds (maximum 713 ms). A separate diagnostic that cleared each probe individually missed one echo. C1 probes and native window/OCR timing were outside this run.
- **C4.** A fixture kills the wrapped child inside a real PTY. The Engine records status 137 and the original login shell accepts a command on the same PID. Real Claude Code also returned exit status 0 on `/exit`. The Registry still reported a live session and the same shell executed a command. Continuing the interrupted Claude task automatically would change the existing exit policy.
- **C5.** The default PTY regression drops a private Unix connection after one prompt, observes the retry screen, and checks the pending request and PID at recovery. The opt-in Claude Code test uses a fresh home and an Anthropic-compatible loopback server. It drops model connections for 30 seconds and checks for a visible retry or connection-error message. The known reply arrived on the same session and PID without another prompt.
- **C6.** A held child finishes work while all app clients are closed. A fresh control client sees the original session ID, PID and result. The desktop store also has a test that reloads its persisted selection. A separate interrupted-Holder-probe reproducer covers an Engine restart, which is a different fault from quitting the desktop app.
- **A12.** The deterministic allocator test feeds 7,200 unique, linked composer repaints at fixed geometry, applying each grid update to the desktop render buffer and requesting its row damage. It bounds live heap growth after warm-up at 64 KiB and checks the final composer and primary screen. With the optional page-based core enabled, Linux CI measured 640,128 additional live bytes on the initial PR. The page kept each superseded OSC 8 URL in its lookup table. The fix periodically discards targets that no cell maps to and preserves shared links. The same allocator bound applies to both cores.

The formal model, counterexamples and Rust trace comparisons are documented in [the formal verification README](../../formal/README.md).

## Thirty-minute memory sample

| Private Engine and Holder | Physical-footprint slope, MiB/hour | 95% interval | Peak MiB | Raw samples |
| --- | ---: | ---: | ---: | --- |
| Before checkpoint allocation change | +19.44 | −5.99 to +44.86 | 49.27 | [Baseline CSV, gzip](ademark-track-c/before.csv.gz) |
| After checkpoint allocation change | +7.01 | +2.48 to +11.53 | 18.45 | [Patched CSV, gzip](ademark-track-c/after.csv.gz) |

Each 30-minute run recorded 7,201 samples. ADEMark’s original `firstWindowMinutes`, `minuteMedians` and `leakSlope` functions selected 7,200 ticks and 29 complete minute medians per run. The observed workload accumulated bounded scrollback. The two runs started at different times, overlapped other builds, and their intervals overlap. The patched trace still has a positive slope. One pair of observations cannot establish a leak-free steady state or predict the official desktop A12 result. The workload used plain text and did not exercise the optional core’s hyperlink fix.

## Reproduction

Run from `diri/` with a private Cargo target directory. The regular tests use temporary state and do not need an API key.

```sh
cargo test -p diri-engine --test composer_delivery
cargo test -p diri-engine --test session killing_the_agent
cargo test -p diri-engine --test session a_model_connection_cut
cargo test -p diri-engine --test holder_session work_finishes_in_the_original_session
cargo test -p diri-term --test composer_heap_growth
cargo test -p diri-term --features diri-terminal-state/core-v2-screen --test composer_heap_growth
cargo test -p diri-terminal-state --test growing_checkpoints
```

The optional real-CLI tests need Claude Code and Python 3 on PATH. Each test runs a temporary home and a loopback model server. Run them individually with one test thread. To use the upstream task wording for the delivery test, set `DIRI_DELIVERY_TASK_DIR` to the harness clone’s `tasks/C2-message-delivery` directory. The synthetic response exercises queued input and retry. It supplies no solution to the graded task.

```sh
cargo test -p diri-engine --test agent_messages_real claude_queues_followups -- --ignored --test-threads=1
cargo test -p diri-engine --test agent_messages_real claude_finishes_after_a_model_connection_cut -- --ignored --test-threads=1
cargo test -p diri-engine --test agent_messages_real claude_exit_returns_to_a_live_shell -- --ignored --test-threads=1
```

For a 30-minute headless memory sample on macOS, build the Engine binaries in the private target directory and run the sampler below. It starts its own Engine, Holder and shell with fresh state, polls the visible screen every two seconds, writes 4 Hz physical-footprint samples, and removes its private session on exit. The child emits a numbered line every 100 ms. The sample covers the Engine and Holder. The official A12 includes the desktop and runs a different workload.

```sh
cargo build --locked -p diri-engine --bins
python3 scripts/track-c-memory.py "$CARGO_TARGET_DIR/debug" samples.csv
```
