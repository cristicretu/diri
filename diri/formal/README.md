# Diri formal models

Run `lake build` from this directory with the pinned Lean toolchain (`export PATH="$HOME/.elan/bin:$PATH"`). Each section below is one model.

## Claude Code prompt cache

A Claude Code login can put its per-conversation scratchpad path in a cached message. Three fresh interactive Opus 5.5 sessions with Diri injection reproduced the cache writes. Replacing the 36-byte conversation UUID in the scratchpad path made the recorded tools, system blocks and messages byte-identical across all three. The Diri session ID, recovery directory, socket path and bearer token did not appear in the compared prompt content.

### Measurements

All rows used `hi`, `claude-opus-5-5` and high effort. Each entry is cache creation / cache read, in input tokens.

| Run | Session 1 | Session 2 | Session 3 |
| --- | ---: | ---: | ---: |
| ADEMark B1, Diri 0.11.2 | 13,075 / 26,124 | 10,465 / 28,963 | 10,461 / 28,963 |
| ADEMark B1, bare Claude Code | 11,180 / 24,701 | 11,489 / 26,657 | 8,629 / 29,514 |
| ADEMark B1, Paseo | 9,951 / 12,184 | 0 / 22,135 | 0 / 22,135 |
| Live reproduction, Diri stdio injection | 33,563 / 26,124 | 15,251 / 44,435 | 15,245 / 44,435 |

The reproduction used the ADEMark manifest's Claude Code 2.1.303, one worktree, the installed Diri hook settings, stdio MCP config and skills plugin, distinct conversation and Diri IDs, and a native Claude.ai login. The user profile and repository differed from the benchmark. The live row measures the first main response in each session. ADEMark's B1 rows summarize main-thread usage over a turn. Its published rows and manifest contain no original request bodies, so the byte offsets below apply to the reproduction.

#### First request differences

* **Loopback fake Messages API, fixture API key.** Two interactive bare launches yielded 81,781-byte requests with 23 tools. Two injected launches yielded 135,855-byte requests with 78 tools, including 55 from Diri. The sole decoded difference within each pair was the session UUID in the JSON string at `metadata.user_id`. The first differing raw byte was at offset 79,071 bare and 133,140 injected. Claude minted the bare ID and used the Diri-supplied `--session-id` in the injected case. Tools and their ordering, system text, cache markers and every message matched within each pair. The markers were on `system[1]`, `system[2]` and `messages[1].content[0]`. The fake returned a Messages response and did no token accounting.
* **Compact MCP at `d79f4325b`.** Two more fixture launches used the generated compact stdio config and plugin. Each first request was 82,959 bytes and contained 25 tools, including both Diri tools. Masking the one occurrence of the metadata UUID made the entire raw HTTP request body byte-identical. The first differing byte was at offset 80,255. This measures request stability without estimating a token charge.
* **Native authenticated runs.** Claude Code's own first-request records preserved the system and tool shapes, ordered message blobs and cache markers. Across three runs the sole prompt difference was bytes **[1052, 1088)** in the UTF-8 text of `messages[1].content[0]`, a 41,663-byte block. The containing path was `/private/tmp/claude-<uid>/<project-slug>/<conversation-uuid>/scratchpad`. The environment reminder, date, Git state, skills, hook text, tool definitions and MCP instructions otherwise matched. All three markers had a one-hour TTL, with global scope on the first system marker. The scratchpad UUID occurs before the last marker and changes the cache key for that prefix. The timing metadata on hook records was outside the prompt. [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
* **Why authentication matters.** Anthropic documents the scratchpad for Claude.ai logins on its first-party API. An API-key fixture omits it. The supported `enableArtifact: false` setting removes eligibility for the scratchpad and also turns off Claude's Artifact tool. `CLAUDE_CODE_TMPDIR` moves the parent directory and retains the session component. Diri preserves the user's artifact preference and Claude's conversation identity. [Scratchpad directory](https://code.claude.com/docs/en/claude-directory#session-scratchpad-directory) · [Artifact setting](https://code.claude.com/docs/en/settings-reference#enableartifact)

### Checking Diri's contribution

From this directory run `lake build` with the pinned Lean toolchain. From `diri/` run `cargo test -p diri-engine --test mcp_http fresh_claude_spawns_share_prompt_visible_injection`.

`PromptCache.lean` models the Claude injection boundary. With a fixed manifest, effective preferences and project catalog, changing the session changes only the runtime projection. The ordering theorem states that MCP schemas and plugin skills retain their declared catalog sequence. The model treats resource contents as inputs. It does not model Anthropic's renderer or compute token counts.

The Rust regression starts two Claude stand-ins through the Engine in the same project. It checks for distinct CLI conversation UUIDs, compares the remaining launch arguments, hook settings, emitted skill files and the authenticated MCP initialize and tool-list response bytes. Effective preferences include whether Diri reads the tab aloud. Selecting one tab for voice intentionally changes its prompt hook output. The guarantee is for sessions with the same effective preference.

## Ordered local hook delivery

Run `PATH="$HOME/.elan/bin:$PATH" lake build` in this directory. Lean 4.36 checks the model using the standard library.

| Lean | Rust counterpart |
| --- | --- |
| `issued`, `Ticket` | For one session in one Engine lifetime, `HookOrder::reserve` links each sequence to the previous reservation. Other sessions can create gaps in the numeric IDs. A ticket describes its eventual validated commit or cancellation. |
| `Run.receive` | `ControlServer::serve_reserved_hook` reads the first commit on the connection and checks its sequence and intent. EOF or an invalid frame resolves the reservation as a cancellation. Readiness may occur in any order. Repeated receipts cannot create reservations. |
| `Run.finish` | `Reservation::finish` waits for the predecessor before handing the event to `ControlServer::apply_hook_report`. `Reservation::drop` releases the successor after handoff or cancellation. One connection consumes at most one frame. |
| `applyTicket` | A cancelled slot leaves state unchanged. A validated report enters `HookQueue::submit`, whose inline and queued paths preserve submission order, then `Registry::apply_hook_report` and `StatusReducer::reduce`. The model quantifies over the transition function, including stateful hooks that leave the parent status unchanged. |

`permutation_drains_in_order` shows that any permutation of fully delivered reservations can drain. `final_in_order` covers all completed interleavings of receipt and handoff. `at_most_once` covers incomplete runs and repeated receipts as well. The checked increment in `reserve` supplies the unique-sequence premise. The proof compares the same event values and clock inputs. It models one session's hook stream and assumes handlers able to run eventually do. Process crashes, unrelated terminal signals, and wall-clock-dependent changes to other Engine state are outside the model.

The Rust test `shuffled_commits_match_in_order_status` uses the production reservation, Claude parser and status reducer with fixed timestamps.

## Diri reliability model

Run from this directory with the pinned Lean 4.36.0 toolchain. The project uses Lean's standard library and has no package dependencies.

```sh
export PATH="$HOME/.elan/bin:$PATH"
lake build
lake env lean --run ReceiptTraces.lean | diff - ../crates/diri-engine/tests/fixtures/receipt-model.tsv
lake env lean --run TaskTraces.lean | diff - ../crates/diri-engine/tests/fixtures/task-model.tsv
```

### What the proofs cover

| Property | Lean result | Rust boundary |
| --- | --- | --- |
| One effect per retained identity | `at_most_once` proves a bound over every finite trace from an empty ledger. `sent_survives_app_restart` proves the confirmed row survives an app restart with exactly one historical effect. | `message_delivery::deliver_at` and `operations::{reserve, finish}` commit a reservation before an input write or launch. Only the original caller proceeds. |
| Task acknowledgement across app and Engine restart | `task_ack_survives` preserves the fact of acknowledgement across a trace. `task_terminal_stable` preserves a terminal task. | `tasks::{reserve, report, cancel, get}` reopen the SQLite journal. An assignee reports acknowledgement explicitly. A sender can cancel before that acknowledgement. |
| Surviving Holder | `live_holder_never_reported_dead` proves the specification given an initially sound view. It permits an unknown view during reconnection. | `Registry::{restore, adopt_live_holders, reap_orphans}` reconstruct local session state from Holder evidence. The mismatch at commit `323a10ea` is reproducible below. |
| New controller revokes older epochs | `attach_revokes` proves strict increase and rejection of the previous epoch on successful admission. `stale_rejected` covers every earlier epoch. | `Holder::handshake` assigns the epoch and `Holder::handle_message` checks it. A full u64 counter refuses another lease. |

The delivery proof projects an arbitrary message or operation onto its key. Message keys include sender, target and message ID. Spawn keys include sender and operation ID. Distinct keys have disjoint rows. The ghost counter counts attempts with a possible effect, including partial writes. Reservation, effect and confirmation are separate transitions. A crash before confirmation leaves the database outcome unknown and removes the original caller's ability to act. Only a failure known to precede the first effect can release a reservation. On a failed operation the durable `failed` outcome likewise prevents a new launch. A `sent` receipt certifies a transport write. Agent acknowledgement of a tracked task lives in a separate record.

The model assumes SQLite serializes immediate reservations, committed rows survive process restarts on intact storage, identity hashes do not collide, and an Engine crash halts its pending invocation. A Holder keeps its PTY and epoch across client restarts. An authenticated positive/negative observation refers to the same session incarnation, and an exit fact refers to the owned PTY child. After that child exits a Holder may remain to drain output. Task history in the model includes whether acknowledgement ever happened. The on-disk audit trail retains only its most recent 32 updates. The proofs use standard Lean foundations and introduce no custom axioms or proof placeholders.

### Two counterexamples on main at 323a10ea

1. `REMOTE_PORT.md` specifies permanent receipts and refusing new identities at 100,000 entries. Commit `0169b68a6` changed `message_delivery::reserved_count` to prune entries aged seven days. A successful send, a sweep triggered by a different identity, and a retry of the original identity can produce two writes. The same applies to tracked spawn operations. The executable `expire` example in Lean proves the count is two in that trace. An app restart itself never prunes those rows. Task submission has its own permanent journal. The bounded permanent-receipt design can enforce lifetime deduplication by refusing new IDs once full. Supporting unlimited new arbitrary IDs also requires more durable storage or a different identity protocol (for example ordered sender sequence numbers with a persistent watermark and explicit rejection of old requests).
2. A local Holder can own a live child while a single `stat` fails. The baseline labels the unadopted record exited. `baselineRestoreOnFailedStat` falsifies the safety property for that transition. The Rust test `ambiguous_stat_during_restore_cannot_end_a_surviving_holder` checks the real child PID on both sides of a failed stat and reads the status recorded by `Registry::restore`.

### Rust conformance checks

Run from `diri/` with a private `CARGO_TARGET_DIR`.

```sh
cargo test -p diri-engine --lib match_lean_restart_traces
cargo test -p diri-remote --lib controller_epoch_model_rejects_old_claims_and_exhaustion
```

`ReceiptTraces.lean` enumerates all 25 pairs of confirmed, rejected-before-write, interrupted-before-write, interrupted-after-write and uncertain actions, with app and Engine restart between attempts. Rust drives the real message and spawn receipt functions and compares the persisted outcome and number of effect callbacks against the generated table. Reopening the database represents an Engine restart at the storage boundary. A dropped invocation has no volatile ownership on retry. The companion task table enumerates 27 three-action traces combining acknowledgement, completion and cancellation. Rust reads each fresh journal view and verifies the acknowledgement history, terminal classification and immutability of an already terminal record.

The Holder test uses a real PTY and authenticated connections, starting at zero and two epochs below the maximum. After each attempted attach it exercises the input path of every prior connection, including the lease kept when the next epoch cannot be represented.
