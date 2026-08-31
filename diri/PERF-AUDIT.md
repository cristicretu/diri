# Performance audit — 2026-08-06

**Status (same day):** fixed and verified — A1, A2 (via A1's caching), A3, A4,
A5, A6, A7, A10, A15 (blast radius now inspector-only); B1, B2, B4, B6, B7, B9;
B3 originally scoped to static coders + off-actor writes/probes; C1, C2
(adaptive tick + attach-driven hot signal, not
kqueue), C3, C4, C5, C7, C10, and the direct pump's 8KiB→64KiB buffer. Plus
`[profile.release]` thin LTO + codegen-units=1 — **later reverted**: both make
the linker reject gpui_macos's Objective-C statics ("pointer not aligned"),
nondeterministically; the release profile is back to defaults.

**Terminal pass (2026-08-13):** B3 is now complete without abandoning old live
Holders. The Rust client negotiates an additive versioned stream over the
legacy request interface; an old Holder's normal rejection selects legacy
JSON/base64 for that process, while a new Holder keeps one binary input/resize
connection with per-operation acknowledgements. A failed ambiguous stream
write is never retried, preventing duplicate keystrokes. A same-process release
fixture measures legacy and stream paths and requires the stream p95 to win.
The persistent lane runs at interactive QoS on Apple platforms. The daemon's
held-output follower adopts that class only during its existing recently-
attached/input hot window and restores default QoS afterward. The release
attach fixture gates 101 writes through the complete input-to-grid path,
including the viewport's scrolling phase, not just socket acknowledgement.

**Second pass (2026-08-07):** instant session switching landed — evicted
sessions park their last-known grid (bounded MRU, ~100KB each) and re-selection
paints it on the first frame while the attach round-trips. Also fixed: A12
(pane events drain per burst, one main-thread hop), A13 (live path paints
straight from the row cache — zero clones on unchanged frames), A11's scan cost
(shared char scratch + exact-match fast path; moving the scan off the main
thread remains open), and A9's record clone (Arc). Still open: A8 (mostly
defused by A1), A11's background-spawn half, A14, B5, B8, B10–B12 (all made
moot if the dirijord-rs flip sticks), C6, C8, C9.

Three parallel deep audits: the GPUI client, the Swift daemon (still what
shipped at audit time), and the Rust engine. Findings ranked per area; file:line
references were verified then. The 2026-08-13 terminal pass supersedes the old
fixed 16 ms client pacer: GPUI's display link now owns client pacing, while the
daemon's leading edge and interactive response bypass background coalescing.

## A. GPUI client (`diri-app`, `diri-term`)

### A1. No view uses `.cached()` — every notify redraws the whole window
Zero hits for `.cached(` across the app. In GPUI an uncached entity re-renders
on every window draw, so any `cx.notify()` re-renders RootView → Sidebar (every
row) → TerminalPane → Inspector → overlays. With the terminal now at 60fps, one
busy agent redraws everything 60×/s. The store comment at `store/mod.rs:46-49`
("terminal grids retain their independent 20fps direct path") is stale.
**Fix:** wrap Sidebar, Inspector, SessionSurfaces, NavigationOverlay,
UtilitySurfaces in `.cached(style)` in `root.rs`. This single change defuses
most of A2, A7, A8, A9 below.

### A2. `sync_status_glyphs` is O(sessions²) per frame
`terminal_pane.rs:777-812`, called from `render` (`:2651`) and store reconcile.
Clones every SessionId String + nested `retain`×`any` scan. 30 sessions ≈ 900
string compares/frame at 60fps. **Fix:** HashSet diff, only on store change.

### A3. Scrollback scrolling is the slowest path in the app
- `element.rs:604` clones the whole `ScrollbackViewport` (up to 512 rows ×
  cols of GridCell, ~1MB) on **every prepaint** — and the cache is only
  trimmed on fetch, so the cost persists after returning to live.
- `element.rs:617-630` — while scrolled, the row-shape cache is bypassed:
  every visible row re-shaped every frame, plus a full `GridBuffer` clone.
- `terminal_pane.rs:1576` — `window.refresh()` per wheel event bypasses view
  caching entirely (`refreshing = true`).
**Fix:** copy only needed fields under the lock; key the shape cache by
absolute row so history rows cache; `cx.notify()` instead of `window.refresh()`.

### A4. Selection drag calls `window.refresh()` per mouse-move
`terminal_pane.rs:2023-2039` (also `:2020`, `:1428`, `:1437`). Selection math
itself is O(1)/cheap; the cost is the caching-bypassed full redraw at ~120Hz.
**Fix:** `cx.notify()`.

### A5. Sidebar drag-reorder writes prefs JSON to disk every frame
`sidebar/view.rs:1053-1074` (`drag_over`) → `set_session_order` →
`persist_preferences` → `to_vec_pretty` + write + rename, per frame while
hovering. Same for projects (`:770`) and archive buckets (`:1272`).
**Fix:** mutate in-memory in `drag_over`; persist once in `on_drop`.

### A6. Menu bar rebuilds all NSViews every 50ms publish
`root.rs:387-396` → `menu_bar.update` → `rebuild_body` (`macos/menu_bar.rs:319`)
removes and recreates every subview with no visibility guard, no row diff.
**Fix:** early-return when panel hidden; diff rows before rebuilding.

### A7. Snapshot publish deep-clones every SessionRecord twice per 50ms tick
`store/mod.rs:1068-1089` clones all records + sorts; `root.rs:389` clones the
snapshot again — solely to feed A6. Resource-only events also invalidate the
sidebar projection cache (`:668-697`) forcing re-group/re-sort.
**Fix:** `Vec<Arc<SessionRecord>>` in the snapshot; don't invalidate projection
for resource deltas.

### A8. Sidebar unvirtualized, per-row store locks + allocations
`sidebar/view.rs:2994-3011` (no `uniform_list`); per row: a store **write**
lock (`:903`), a read lock (`:915`), 3-4 `format!` allocs, ~8 boxed listeners.
30 rows × 60fps ≈ 1800 write-locks/s. Mostly defused by A1; beyond that hoist
one snapshot per render and intern element ids.

### A9. TerminalPane render clones the selected record twice per frame
`terminal_pane.rs:1034-1043` full `SessionRecord` clone ×2 (`:2652`, `:2654`);
`PaneChip::for_session` reallocates chips per frame (`:147-197`).
**Fix:** `Arc<SessionRecord>`; memoize chips on `(id, updated_at)`.

### A10. SessionSurfaces allocates a HashSet of all ids even when hidden
`session_surfaces.rs:90-99` — runs before the visibility check. Move it behind.

### A11. Find rescans full scrollback on the main thread
`find.rs:125-131` + `terminal_pane.rs:912-926`: whole-scrollback rescan every
100ms while output flows; `append_matches` allocates `Vec<char>` per line and
`line.to_owned()` per match; O(n·m) per-char lowercase compare. Scheduling
(200ms debounce, 500-match cap) is good — the scan cost is the problem.
**Fix:** `cx.background_spawn`, byte-wise case-folded search, range instead of
owned line.

### A12. One main-thread hop per grid frame
`terminal_pane.rs:568-579`: one `update_in` per `PaneEvent::Chunk`;
`apply_grid_updates` already accepts a batch. **Fix:** drain with `try_recv`
into a Vec, apply once.

### A13. Prepaint re-clones composed quads/lines on 100% cache hit
`element.rs:673-678`, `:605-608`: fresh Vecs + `ShapedLine` clones (~768B
SmallVec each) every prepaint even with zero changed rows. **Fix:** persist the
composed lists on `ElementSharedState`, invalidate on damage only.

### A14. Command palette re-clones every session per keystroke
`navigation.rs:605-627` write-locks the store, deep-clones all records;
`palette.rs:252-308` rebuilds `PreparedText` per candidate per keystroke.
**Fix:** cache `PreparedText` like `quick_open::RankCandidate` does; rank over
`Arc<SessionRecord>`.

### A15. Inspector notifies at 20Hz even when its content didn't change
`inspector.rs:227-238` — bare `cx.notify()` fallthrough. Compare projected
fields first.

Verified fine: row shape cache; bg/underline run-length quad batching; damage
tracking in `buffer.rs`; nucleo matcher reuse + Arc'd quick-open pool; sidebar
projection memoization; static status glyphs; virtualized diff view; resize
pacing + reflow hold; offscreen repaint suppression.

## B. Swift daemon (ships today; fixes worth it until the Rust swap)

### B1. Input filter allocates once per CSI byte
`HeadlessScreen.swift:513-554` — `case .csi(let prefix, var body)` +
`body.append(byte)` triggers a COW malloc+memcpy **per escape-sequence byte**
(state still holds the buffer). A TUI repaint ≈ 16k mallocs/frame. The whole
filter also walks Data byte-by-byte, then `feed` copies again (`:75`).
**Fix:** stored `csiBody: [UInt8]` (or clear state before mutating); process
runs of non-ESC bytes with `append(contentsOf:)`.

### B2. Grid diff converts every cell of the whole screen per flush
`HeadlessScreen.swift:324-398` — allocations are damage-proportional (good) but
CPU walks all cells: `cd.getCharacter()` builds a `Character` then decomposes
it, ~23k cells per flush, ≤60 flushes/s ≈ 1.4M Character constructions/s per
busy session. **Fix:** compare raw `CharData`/`Attribute` first; build
`GridCell`s only for differing rows.

### B3. Every keystroke used to open a fresh UDS connection
`HolderClient.swift:40-52` — per call: connect, new JSONEncoder, blocking
readLine, new JSONDecoder; `write()` base64-expands the payload (`:12`). Called
from `writeRaw` per keystroke (`AgentSession.swift:977`) — stalls log draining
behind the round trip. Also `foregroundAgentKind()` (stat + libproc per ~2s per
session) runs serially **inside** `StatusEngine.tickAll`, so one slow holder
stalls status for all sessions; `agentWorkingDir()` similar every 5s.
**Fix:** persistent connection per holder, length-prefixed binary write frames;
move stat/ProcessTree probes off the actor.

**Implemented:** the current Rust Holder uses the additive negotiated binary
stream described above. Control operations remain independent legacy requests,
and old live Holder processes continue to work until they naturally exit.

### B4. Dead `.output` frames still built and sent per chunk
`AgentSession.swift:536-543` — the GPUI client discards `FrameType::Output`
(`attachment.rs:320`), yet the daemon copies each chunk twice and sends it per
sink; it also counts against the pending watermark, pushing real grid frames
into the desync path. Frame is built even with zero sinks.
**Fix:** stop emitting `.output` on the data channel (keep for CLI replay);
at minimum guard on `!sinks.isEmpty`.

### B5. Log→emulator path: three extra copies per chunk
`HolderServer.swift:109-121` fresh 64KB heap array per read + materialized
`prefix` slice; `HolderProtocol.swift:252-285` full 64KB `range(of:)` search +
two more copies for an exit marker that appears once per session.
**Fix:** reuse read buffer; fast-path chunks containing no `0x1B`.

### B6. Checkpoint writes unconditionally when it fires
`AgentSession.swift:437-478` — debounce is excellent (1s quiet window), but no
dirty check: interactive typing ≈ one full snapshot + plist encode + atomic
write per second. **Fix:** skip when payload hash + logOffset unchanged.

### B7. Backpressure re-seed spins at 60Hz
`AgentSession.swift:603-630` — while any sink is over the 8MB watermark, every
16ms does a full diff walk + full snapshot + RLE per desynced sink — exactly
when the client is slowest. **Fix:** back off to ~250ms while backpressured.

### B8. Detection: cost is in region extraction, not cadence
Cadence verified fine (200ms/1s ticks, contentSeq gating, first-match-wins,
2-11 rules per agent). Per evaluation: `snapshot()` re-extracts every row via
SwiftTerm's unreserved Character-append; `Regions.nonBlank` allocates a String
per row (`Regions.swift:16-22`); `.lineRegex` runs NSRegularExpression per line
(~180 calls on an idle claude screen); `.contains` does ICU case-insensitive
scans over ~28KB per failing predicate.
**Fix:** cache snapshot keyed by contentSeq (shared with artifact scanner);
scalar-scan blank check; lowercase region text once per cache entry.

### B9. Artifact scan runs for detached sessions and re-extracts the screen
`AgentSession.swift:606` sits before the `sinks.isEmpty` guard; duplicates the
snapshot extraction; 5 regex passes + O(matches²) overlap check.
**Fix:** move after the guard; reuse cached snapshot; short-circuit when no
`://` in text.

### B10. Event publish serializes three times with fresh coders
`JSONValue.swift:60-79` — encode → decode to boxed tree → re-encode in
`EventBus.archive`; `JSONEncoder.dirijor`/`JSONDecoder.dirijor` are computed
properties constructing new instances per access. Happens with zero subscribers.
**Fix:** `static let` coders; `JSONValue.preEncoded(Data)` case.

### B11. ResourceGovernor per-pass syscall storm (cadence fine)
`ResourceGovernor.swift:89-187` + `ProcessTree.swift:108-118` — per pid:
listchildpids + getpgid + two-call sysctl allocating kinfo_proc buffers, all
serial in one actor turn. 30 sessions × ~10 procs = thousands of syscalls.
**Fix:** detached task; one shared enumerate per pass.

### B12. Smaller
- `GridRowCodec.appendRow` temp Data per row (`Grid.swift:116-136`).
- `OutputLog` write path is dead in production (`readOnly: true`) → syncPoints
  always empty → replay-start optimization never fires; 4MB ring dead weight.
- `refreshFromDisk` 3 syscalls on every fs event + per drain + per offset
  access — `fstat` would be one.
- `scrollbackCells` re-counts scrollback rows via up to 512 line calls per
  scroll request — cache origin/total, invalidate on scroll.

Verified fine: flush pacing (post-fix), damage-proportional diff allocations,
checkpoint debounce, resize/trimOnly handling, detection structure, 1Hz
output-activity coalescing, holder log durability (no per-chunk fsync),
500ms-debounced persistence, bounded EventBus.

## C. Rust engine (fix before the daemon swap)

### C1. Registry watcher busy-diffs the whole session table 6.7×/s
`events.rs:378-421` — every 150ms deep-clones every SessionRecord (live and
archived), 4 mutex hops per live session, then **two serde_json::to_string
allocations per record** for the fingerprint — all under the global registry
Mutex. ~30 sessions ≈ 1500 clones + 3000 JSON serializations per second.
**Fix:** per-session AtomicU64 version bumped in `apply()`; watcher diffs
versions. No clones, no JSON.

### C2. 30 idle sessions = ~600 wakeups/s across 60 threads/processes
`session.rs:40` TICK_INTERVAL=100ms; held pump does `refresh_from_disk` (3
syscalls) + read attempt per tick even when quiet (`:617-625`); holder process
polls its PTY at 100ms too. `handle_tick` early-returns unless Working, so idle
ticks are pure waste. **Fix:** kqueue EVFILT_VNODE on the log (or holder pushes
a "grew" byte); adaptive tick — 100ms while Working, 1-2s otherwise.

### C3. Detection runs per PTY chunk, not per screen change
`session.rs:542`, `:686` — `engine.evaluate(&screen.snapshot(), ...)` per chunk
under the screen Mutex; snapshot allocates the full grid as Strings; regions
clone lines twice more; the reducer then discards unchanged content_seq *after*
all that (`status/mod.rs:504`). `content_seq()` exists and has no non-test
caller. **Fix:** gate on `content_seq()` before snapshotting.

### C4. `contains` predicates lowercase the whole region per predicate
`detect/manifest.rs:68` — fresh case-folded copy of ~12KB text per predicate +
re-lowercase of a constant needle; kimi/qoder manifests have 15 each. A
non-matching screen ≈ 96KB case-folded per evaluation, per chunk (see C3).
**Fix:** lowercase needle at deserialize; cache lowercased region text.

### C5. state.json fully rewritten on every mutation — including mark_seen
`registry.rs:96-107` — clones all records + projects, `to_vec_pretty`, temp
write + rename, on spawn/kill/rename/**tab-switch**. 200-500KB per UI
interaction at fleet scale. **Fix:** dirty flag + 500ms trailing-edge flush
(+on quit); `to_vec` not pretty.

### C6. diri-node re-parses whole transcripts from byte 0 every 30s
`node/usage.rs:137-155, 306-342` — any changed file is re-read entirely; an
active session's multi-MB transcript re-parses every tick; inserts are one WAL
commit per row (no transaction). The app-side store (`usage/store.rs`) already
does this right with per-file offsets. **Fix:** add offset column, parse
appended tail only, wrap per-file inserts in one transaction.

### C7. `publish_updated` clones every record to find one
`control.rs:536-548` (also `:402-406`, `:248-253` in a loop).
**Fix:** `Registry::record(&self, id)` single-record accessor.

### C8. Attachment frames: 4 copies + 2 unbounded channels, no coalescing
`frames.rs:250, 271-274` (fresh Vec per frame), `attachment.rs:186` unbounded,
`terminal_pane.rs:2948-2951` clones SessionId String per frame onto a second
unbounded channel. No backpressure anywhere. **Fix:** recv_many + batch apply;
bound both channels; borrowed/pooled frame payloads; Arc<SessionId>.

### C9. Read-only OutputLog never uses its ring
`log.rs:171` — held-session reads always hit disk (seek + read + alloc per
chunk) while the 4MiB ring sits empty. **Fix:** append discovered tail to ring
in `refresh_from_disk`; serve in-range reads from memory.

### C10. Smaller
- Per-chunk title String alloc + mutex (`session.rs:541`, `:685`) — compare
  first or fold behind the C3 gate.
- Holder liveness probe: fresh socket connect per session every 2s
  (`session.rs:49, 634-649`) — fold into adaptive tick.
- Direct pump reads 8KiB where holder uses 64KiB (`session.rs:494`) —
  multiplies C3 by 8 on bursts.

Verified fine: git facts request-driven (no polling, no subprocess in loops);
history scanning on-demand and bounded; regexes compiled once at load;
first-match-wins rule evaluation with region caching; buffered log writes with
2s sync interval; well-built EventBus; app-side usage scanner correctly
incremental; sound control-socket read loops; right I/O mechanisms in both
pumps (cadence, not mechanism, is the issue).

## Suggested attack order

1. **A1** `.cached()` on the five top-level views — one small diff, defuses
   half the client list.
2. **A3 + A4** kill `window.refresh()` on wheel/drag paths, fix scrolled-mode
   shape-cache bypass — scrolling and selecting are the two big remaining
   "feels slow" gestures.
3. **B1 + B2 + B3** input filter, grid diff walk, per-keystroke holder
   round-trip — the daemon-side latency/CPU that ships today.
4. **A5, A6, B4, B6, B9** — isolated wins, each a small diff.
5. **C1-C5** before the daemon swap; they define how the Rust daemon behaves
   at fleet scale.
