# Dual-Lens GUI

One live Agent session, two ways to look at it, both over the same PTY Holder:

- **TUI Lens (Pilot Mode)** is the terminal grid Diri always had.
- **GUI Lens (Conductor Mode)** is an editorial thread: prose, collapsible Work Strips, Diff Cards, images, diagrams, a composer, permission cards.

⌥⌘L (`toggle-lens`, rebindable) or the pane header's *Conversation | Terminal* control flips between them. The binding was checked against the keymap: ⌘. is Activity, ⇧⌘L is the code editor's Select All Occurrences, and ⌥⌘L was free.

## One PTY truth, two observation channels

```
           keystrokes, /model, approve/deny keys
GUI Lens ──────────────────────────────────────────┐
   ▲                                               ▼
   │ thread items (by byte offset)          PTY Holder ── Agent CLI
   │                                               │          │
Engine ◄── kernel watch ◄── Agent's own JSONL ◄────┼──────────┘
   │                                               │
   └──────── grid diffs (unchanged) ───────► TUI Lens
```

- **Input** from the GUI Lens always goes into the real PTY:
  - Prompts go through `session.send_text`.
  - Images go through the terminal's own paste/upload path.
  - Model switches are the CLI's own `/model <alias>`.
  - Approve/Deny goes through `session.answer_blocker`, which writes the manifest's answer keys only if `needsInput.occurredAt` still matches the prompt the user saw, checked under the same Registry lock as the write. A prompt that changed is refused with `blocker_changed` and nothing is written.
- **Observation** never scrapes ANSI. `session.thread_read` tails the Agent's own transcript (Claude Code `~/.claude/projects/**/<id>.jsonl`, Codex rollout files) from a client-held byte cursor:
  - It returns versioned `diri_proto::thread::ThreadItem`s keyed by `(record offset, part)`.
  - It is stateless per request, so Engine restarts, app restarts and reconnects resume from the client's cursor.
  - A cursor that is not a record boundary, or that belongs to another file (`generation` = device + inode), resets to 0. The client merges by key, so a replay is idempotent.
- **Wakeups** come from `notify` (FSEvents/inotify) on the transcript's directory, published as `session.thread_appended`.
  - A read with `watch: true` leases the watch for one hour, and every read renews it. Expired leases are pruned lazily, so nothing polls and there are no timers.
  - The lens also re-reads on its session's `session.updated`. Hooks fire at every turn boundary, so a watch event lost to FSEvents' asynchronous stream start, or to an expired lease, is recovered on the next status change.
- **Toggle cost.** Flipping is a view-state change in `TerminalPane::lens_modes`. The grid's attachment keeps receiving diffs while the GUI Lens shows, and the lens keeps its thread, scroll and draft while the TUI shows. Neither side reconnects or replays anything.
- **PTY portal.** When the Agent asks something only its terminal can answer (a non-permission `needsInput`, a password, a picker, an auth flow), the GUI Lens shows the real grid inline, clipped to its bottom rows. The grid keeps the pane's full size, so the PTY is never resized. Clicking the portal focuses the terminal.
- **Scope.** Shells, unstructured Agents, Agents typed inside a shell (whose transcript identity Diri did not mint) and remote sessions stay in the TUI Lens. Remote transcripts live on the remote host. Serving them is a Helper enhancement outside the `REMOTE_PORT.md` baseline.

## Rendering

- **Virtualized rows.** Rows render through GPUI's `list`, so only visible rows lay out and shape text, and off-viewport rows hold no glyph runs.
- **Parse once.** Transcript records are immutable once written, so each assistant turn is parsed to Markdown exactly once (`ThreadModel::markdown`, keyed by item key and text hash) and reused across resets.
- **Splice the tail.** Appends splice only the tail rows (`RowChange::Tail`).
- **Cached pieces.** Diff cards and decoded images are cached per item key. Mermaid blocks reuse the notes renderer (`notes/editor_view/diagram.rs`) off the main thread.
- **No idle wakeups.** The only animation is the live Work Strip's breathing dot, and it exists only while the session is working.
- **Pieces:**
  - User bubbles with image thumbnails.
  - Proportional prose at a 720 pt measure, with tight mono for code, paths and metrics.
  - Clickable `path:line` pills with a hover preview of the file.
  - Work Strips, for example `Explored 3 files · Ran 2 commands · 4.2s` with a failure count and edited-file chips.
  - Per-call rows with their output.
  - Diff Cards with word-level emphasis (`git_ui::words`) and *Quote in reply*.
  - Before/after wipes when two screenshots land in one strip.
  - Segment dividers, a permission card, and the composer.

## Threads, segments and cache-aware switching

A Diri thread is an ordered chain of `ThreadSegment`s. Each segment records the Agent kind, session, model, account and its first transcript byte.

| Tier | What happens | Where the boundary comes from | Prompt cache |
| --- | --- | --- | --- |
| 1 · model | The composer's model pill sends `/model opus\|sonnet\|haiku` into the PTY. | Derived: consecutive usage records change model (`segment_chain`). | Per model. The pill shows `warm 4m12s` or `cold write` per model, from observed `cache_read`/`cache_creation` and the 5 m/1 h TTL tier of the latest request to that model. |
| 2 · account | The account pill opens Diri's existing account-continuation sheet (`session.continue_with_account`). | Stored: the handoff records an `Account` segment at the copied transcript's byte length. The copy is byte-identical, so every item key survives and the lens draws one divider. | Unchanged bytes, but a different account's cache. |
| 3 · agent | `session.handoff_capsule` renders a deterministic Handoff Capsule. | Stored `Handoff` segment (type and divider in place). | The parked Agent's conversation is untouched while parked. Returning appends to it, so its last request stays a prefix of the next one. |

The Handoff Capsule is a pure function of the thread, goal and git state. It contains:

- the goal and branch;
- touched files, sorted;
- the last 12 command outcomes;
- the last 6 turns;
- the bounded `git diff`.

It carries no timestamps and no session or provider identities, so the same state always yields the same bytes. Finishing Tier 3 in the UI means spawning the target Agent with the capsule as its prompt and recording a `Handoff` segment that links both sessions. Both are additive: the wire types, divider and capsule exist already.

Observation adds zero bytes to any prompt. Lens state, cursors and segment metadata never reach injection (`PromptCache.observation_independent`).

## Proofs and checks

- `formal/DualLens.lean`:
  - `read_monotone`, `read_preserves_boundary`;
  - `read_append_split` and `tail_chunking`: any chunking of appends into reads yields exactly one read's lines;
  - `read_keys_nodup`;
  - `replay_idempotent` and `synced_reconnect`: reconnecting from any older or garbage cursor converges, with no duplicate or dropped turns;
  - `flips_never_drift`.
- `formal/PromptCache.lean`:
  - `observation_independent`;
  - `parked_prefix_preserved`;
  - `model_switch_prefix`.
- Rust conformance tests:
  - `thread::tail::tests::tail_chunking_matches_one_read`;
  - `thread::tests::stale_cursor_replay_is_idempotent`;
  - `thread_lens::model::tests::chunked_and_replayed_ingest_matches_one_read`;
  - `control::tests::answer_blocker_refuses_a_prompt_that_is_no_longer_live`.
- Screenshots: `thread_lens::fixture::screenshots::render_thread_lens_screenshot` with `DIRI_VISUAL_LIGHT`, `DIRI_VISUAL_EXPAND`, `DIRI_VISUAL_PERMISSION` and `DIRI_VISUAL_HEIGHT`.
- Benchmarks: `thread::bench::bench_thread_tail` and `thread_lens::fixture::screenshots::bench_thread_lens`. Results are in `PERF.md`.
