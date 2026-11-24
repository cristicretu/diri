# Dual-Lens GUI

One live Agent session, two ways to look at it, both over the same PTY Holder:

- **TUI Lens (Pilot Mode)** is the terminal grid Diri always had.
- **GUI Lens (Conductor Mode)** is an editorial thread: prose, collapsible Work Strips, Diff Cards, images, diagrams, a composer, permission cards.

⌥⌘L (`toggle-lens`, rebindable) or the pane header's lens icon (a speech bubble from the terminal, a prompt from the conversation) flips between them. The binding was checked against the keymap: ⌘. is Activity, ⇧⌘L is the code editor's Select All Occurrences, and ⌥⌘L was free.

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
- **Wakeups** have three independent sources, so one missed signal never leaves a turn unseen:
  - `notify` (FSEvents/inotify) on the transcript's directory, published as `session.thread_appended`. A read with `watch: true` leases the watch for one hour, and every read renews it. Expired leases are pruned lazily.
  - Every Agent hook (PreToolUse, PostToolUse, Stop, Notification, UserPromptSubmit) calls `ThreadWatches::poke`, which re-announces the leased transcript.
  - PTY output in a session with the GUI Lens open schedules a re-read 120 ms and 900 ms after the screen goes quiet, for a CLI that flushes its JSONL after drawing the turn. A read with nothing new is a stat and a few bytes.
  - None of these poll: an idle session with idle output schedules nothing.
- **Local command output.** Claude records `/model`, `/cost`, `/status` output as `<local-command-stdout>`; it reaches the thread as `ThreadBody::CommandOutput`. `/model` never draws a chip: the CLI's confirmation becomes one *Switched to …* divider, repeated switches with nothing between collapse into one, and the next request's usage record does not draw a second divider for the same switch.
- **Optimistic send.** Enter shows the message in the same frame (with the working line) and re-follows the tail; the transcript record replaces it when it lands (whitespace-insensitive match, so a replay never duplicates it).
- **Toggle cost.** Flipping is a view-state change in `TerminalPane::lens_modes`. The grid's attachment keeps receiving diffs while the GUI Lens shows, and the lens keeps its thread, scroll and draft while the TUI shows. Neither side reconnects or replays anything.
- **PTY portal.** Pickers never reach the transcript (Claude's `/model`, `/config`, `/mcp`, `/permissions`, `/login`; Codex's model and approval pickers; trust dialogs; `fzf`). `thread_lens::portal::picker_rows` reads the live screen and finds one by its footer (`Esc to cancel`, `Enter to confirm`, `↑/↓ to navigate`, …) or by a highlighted numbered list, bounded by the rule or blank rows above it. While one is open:
  - a live slice of the real grid showing exactly the picker's rows docks above the (dimmed) composer, aligned on both axes from the terminal's painted geometry; the grid keeps the pane's full size, so the PTY is never resized;
  - keyboard focus moves to the terminal and the lens lets every key bubble past it, so arrows, Enter, Esc and letter shortcuts (`s` for "this session only") go straight to the CLI;
  - when the picker closes, the slice folds away and focus returns to the composer.
  A non-permission `needsInput` with no picker shape (a password, a free-text question) shows the screen's last rows the same way.
- **Scope.** Shells, unstructured Agents, Agents typed inside a shell (whose transcript identity Diri did not mint) and remote sessions stay in the TUI Lens. Remote transcripts live on the remote host. Serving them is a Helper enhancement outside the `REMOTE_PORT.md` baseline.

## Rendering

- **Virtualized at block granularity.** Rows render through GPUI's `list`, so only visible rows lay out and shape text, and off-viewport rows hold no glyph runs. An assistant turn is one row per Markdown block (`(item key, block)`): a 200-line reply lays out only the paragraphs, code blocks and diagrams on screen. Transcript records are written whole, never as growing partial text, so there is no tail re-parse: each turn is parsed once and its blocks are cut from that parse.
- **Arrivals fade in.** Rows that land while the lens is open fade in over 320 ms on opacity alone (a paint property; nothing re-lays out), once each.
- **Parse once.** Transcript records are immutable once written, so each assistant turn is parsed to Markdown exactly once (`ThreadModel::markdown`, keyed by item key and text hash) and reused across resets.
- **Splice the tail.** Appends splice only the tail rows (`RowChange::Tail`).
- **Cached pieces.** Diff cards and decoded images are cached per item key. Mermaid blocks reuse the notes renderer (`notes/editor_view/diagram.rs`) off the main thread.
- **One text per paragraph.** Bold, italic, struck, `code`, links and `path:line` references are runs of a single shaped text, so a period after a bold phrase or code span never wraps onto its own line.
- **Selection.** Every paragraph, code block, tool output and diff row is a selectable segment. Drag across rows (the list scrolls under the pointer), double-click a word, triple-click a block; ⌘C copies the selection as it reads. Code blocks and replies have a hover *Copy*.
- **Composer.** One column with the thread (same 720 pt measure and gutters, so a user bubble's right edge meets the composer's). A neutral hairline surface (no accent halo), a 1.5 pt caret in the text color (macOS's 530 ms blink, solid while typing, no timer unfocused), growth with its text up to 240 pt, and a toolbar that is there from frame 0:
  - `+` attaches files as `@path` mentions; `/` opens the menu of the Agent's own and the user's custom commands;
  - the model pill names the model the CLI will use before the first reply (Claude `~/.claude/settings.json` and the project's settings; Codex `~/.codex/config.toml`), then what the last request used, with its effort and a cache-warmth dot. It opens a picker of real models (Claude's `/model` list; Codex's cached catalog) with per-model warmth and an effort row. Claude switches in place with `/model` and `/effort`; Codex switches in its own picker, which docks as the PTY portal;
  - the mode pill (Claude) shows the mode Claude's footer shows (`auto mode on`, `plan mode on`, …) and sends Shift+Tab, the CLI's own key, to cycle it;
  - a context ring with the share of the window the last request filled (Codex reports its window; Claude's is 200k or 1M);
  - the primary action: **Send** (↑) when idle, **Stop** (■, Esc into the PTY) while the Agent works or owes a reply with nothing typed, **Steer** while it works and you type. Drafts live in the session's lens entity, so they survive tab switches and lens flips.
- **Background.** The lens paints exactly what the terminal grid paints over the pane's surface (`lens_background`, sharing `grid_fill_opacity` with the grid): the theme's background on an opaque window, nothing under glass. Flipping lenses never shifts the shade.
- **Model and effort follow the CLI.** A pick shows at once; Claude's own `Set model to …` / `Set effort level to …` lines then confirm it, and the startup default never overrides either.
- **Never dead after Enter, never stuck after the reply.** The message shows at once; while a reply is owed, a working line counts the seconds (`Thinking · 4s`) and turns into the tool in flight, and the action is Stop. The line is tied to the thread, not the status: it goes the moment the reply row lands. Every wake-up (file watch, hook, status change) reads at once and again at +60 and +200 ms, for a JSONL flushed just after the Stop hook.
- **Anchoring.** A short thread starts under the header (top-aligned list); once it overflows, the lens switches to a bottom-aligned list that follows the tail, which on a 3,200-row thread lays out about 0.5 ms cheaper per frame than tail-following a top-aligned one.
- **Docked slot.** A permission card and the PTY portal share one slot directly above the composer, so an interactive control never scrolls away with the thread.
- **Tail.** The list follows the tail while streaming, stops the moment the user scrolls up, and offers a jump-to-latest control.
- **The flip.** 240 ms on a lightly underdamped spring: the incoming lens rises 14 pt as it fades in, the outgoing one sinks and fades, with a soft bloom from the header control. One timer per flip removes the outgoing layer at t = 1; reduced motion cuts instantly. It is GPUI composition (opacity, offset, shadow), not a Metal blur pass: GPUI has no element blur, and the precompiled metallib would need a new render pass.
- **No idle wakeups.** The only animations are the live Work Strip's breathing dot (while the session works) and the caret (while the composer has focus).
- **Pieces:**
  - User bubbles with image thumbnails.
  - Proportional prose at a 720 pt measure, with tight mono for code, paths and metrics.
  - Clickable `path:line` pills with a hover preview of the file.
  - Work Strips named by real tools and targets: `Read view.rs · Ran cargo test · 3.4s`, `Read 3 files · Edited model.rs`, `Used Computer, Browser`; while running, the call in flight (`Running cargo test`, `Using Computer · Trash`).
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
| 3 · agent | The model picker lists every installed Agent (Claude Code, Codex, Antigravity, from the Engine's readiness scan); a model on another Agent calls `session.handoff`: the Engine builds the Handoff Capsule, spawns that Agent in the same directory on that model (`--model` / `-m`) with the capsule as its first prompt, and writes a `Handoff` segment on the new session. The lens opens the new session on its Conversation view, under a *Continued from …* divider. | Stored `Handoff` segment pointing at the source session. | The source session is left running (parked): its conversation is untouched, so returning to it reuses its prefix. |

The Handoff Capsule is a pure function of the thread, goal and git state. It contains:

- the goal and branch;
- touched files, sorted;
- the last 12 command outcomes;
- the last 6 turns;
- the bounded `git diff`.

It carries no timestamps and no session or provider identities, so the same state always yields the same bytes. The new session's thread starts at its own transcript; the source thread stays one click away in its own tab.

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
  - `control::tests::answer_blocker_refuses_a_prompt_that_is_no_longer_live`;
  - `thread::watch::tests::a_hook_poke_announces_only_the_leased_session`;
  - `terminal_pane::tests::a_picker_on_screen_opens_the_portal_and_hands_back_the_keys`;
  - `thread_lens::view::tests::a_sent_message_shows_at_once_and_reconciles_with_its_record`, `selecting_the_thread_copies_its_text`, `a_period_after_bold_stays_in_the_same_text`;
  - `thread_lens::model::tests::model_commands_collapse_into_one_divider`, `local_command_output_stays_with_its_command`.
- Screenshots: `thread_lens::fixture::screenshots::render_thread_lens_screenshot` with `DIRI_VISUAL_LIGHT`, `DIRI_VISUAL_EXPAND`, `DIRI_VISUAL_PERMISSION`, `DIRI_VISUAL_LIVE`, `DIRI_VISUAL_SLASH`, `DIRI_VISUAL_EMPTY` and `DIRI_VISUAL_HEIGHT`; the pane scenes `DIRI_QOL_SCENE=lens-tui|lens-gui|lens-portal` of `render_terminal_qol_screenshot`.
- Benchmarks: `thread::bench::bench_thread_tail` and `thread_lens::fixture::screenshots::bench_thread_lens`. Results are in `PERF.md`.
