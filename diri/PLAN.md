# diri — GPUI Migration Plan

A ground-up rewrite of the Dirijor **client app** in Rust + [GPUI](https://gpui.rs) (Zed's GPU-accelerated UI framework). Same architecture, same design, same daemon — a new binary and a new `.app` named **diri** that coexists with the shipping Dirijor without touching it.

This document is the single source of truth for the migration. Every task below is scoped to be delegatable to an independent agent (worktree-friendly, with acceptance criteria). Read §2 and §3 before doing anything.

---

## 1. Goals & non-goals

**Goals**
- Fastest possible rendering: GPU-drawn UI end to end, ~zero idle CPU, 120 Hz-capable terminal grid, minimal memory.
- Pixel-faithful port of the existing design: vibrant left sidebar, same tokens, same animations, same behaviors.
- Zero interference with the running Dirijor installation and its live agent sessions.
- New app identity: binary `diri`, bundle `diri.app`, its own preferences domain.

**Non-goals (v1)**
- No daemon rewrite. `dirijord` (Swift) stays exactly as-is; diri is a pure client.
- No new features. Parity first; ideas go to a backlog.
- No iOS/remote client changes.
- No Windows/Linux (GPUI supports them; not in scope).

---

## 2. The architecture insight that shapes everything

Dirijor already uses a **mosh/tmux model** (verified in source):

- The **daemon owns everything stateful**: PTYs, VT emulation (SwiftTerm headless, `DirijorDaemonKit/HeadlessScreen.swift`), scrollback (2000 lines), status detection, persistence, hibernation, injection, worktrees, resource governor, MCP.
- Clients receive a **rendered grid of cells** — RLE-encoded row diffs (`DirijorProtocol/Grid.swift`) — over a unix socket, and send back input bytes, resize, and scroll.
- The SwiftUI app performs **no VT parsing at all**. `GridTerminalView` is a dumb cell painter.

Therefore diri is: **a protocol client + a GPU cell renderer + the app shell.** No `alacritty_terminal`, no PTY code, no emulator. This is why the port is tractable and why diri inherits every session (including MCP-spawned ones) for free — they arrive as `session.updated` events from the shared daemon.

**Reference reading order for any task agent:**
1. `Sources/DirijorProtocol/{Frames,Grid,ControlMessage,Methods,Paths}.swift` — the wire format (port spec).
2. `Sources/DirijorClient/{DaemonClient,SessionAttachment}.swift` — the client behavior spec.
3. `Sources/DirijorTerminalHost/GridTerminalView*.swift` + `TerminalKeyEncoding.swift` — renderer + input spec.
4. `Sources/DirijorCore/*.swift` — the data model (`SessionRecord`, `SessionStatus`, `AgentKind`, `AttentionLevel`, …).
5. `Sources/DirijorMac/*` — UI spec. Treat `DirijorDaemonKit` as a black box.

> Sources 3 and 5 were the Swift client, removed once the port landed. The
> behavior they specified now lives in `crates/diri-term`, `crates/diri-ui`,
> and `crates/diri-app`.

---

## 3. Coexistence rules (HARD CONSTRAINTS — violating these kills the user's live sessions)

The daemon is built for multiple concurrent clients (per-connection control channels, per-sink grid broadcast, ref-counted `client.set_active`). But:

1. **Never ship or spawn a different `dirijord`.** The Swift app compares `hello.executableHash` against its bundled daemon and restarts on mismatch (`AppModel.swift:655`). If diri did the same with a different binary, the two apps would ping-pong-restart the daemon forever. diri therefore:
   - **Does not bundle a daemon.** It connects to the existing socket (`~/Library/Application Support/Dirijor/daemon.sock`).
   - **Never performs the stale-daemon restart dance.** The sole user-initiated exception is Settings → Remote: after changing `remote.json`, diri calls `daemon.shutdown` once so the listener reloads; holder-owned sessions survive and diri relaunches only if launchd has not already done so.
   - If the socket is dead, diri shows a "Dirijor daemon not running" state with a button that launches the installed Dirijor daemon binary (resolve via the installed app bundle) — or simply tells the user to open Dirijor. v1: connect-only + spawn the installed `dirijord` if found (same singleton `flock` makes double-spawn safe).
2. **Geometry arbitration is desktop+mobile, not desktop+desktop.** One PTY per session, one effective size. Two `.desktop` attachments with different window sizes will fight over resize (SIGWINCH thrash). Mitigation for v1: diri attaches as `role: "desktop"` like the Swift app, which is fine as long as the *same session* isn't focused in both apps at once (Dirijor only keeps 3 resident attachments — MRU). Document this in diri's README; a proper multi-desktop role is a later additive protocol change. Do **not** attach as `mobile` — that flips `remoteActive` and Dirijor shows an "Active on iPhone" takeover card.
3. **Input is interleaved, not arbitrated** — typing into the same session from both apps interleaves bytes. Acceptable; same as two ssh clients in tmux.
4. **Wire compatibility:** `hello` with `proto: 1` (`WireVersion.current`), `build: "diri-<semver>"`, `token: null` on local UDS. Protocol is additive-only within major 1 — decode leniently (unknown fields ignored, unknown enum values mapped to a fallback).
5. **Separate app identity:** bundle id `com.dirijor.diri` (or similar), its own UserDefaults domain. diri must not read/write Dirijor's preferences. Shared on-disk things it may **read**: agent transcripts (`~/.claude/projects`, `~/.codex/sessions`) for usage/history — both read-only scans.

---

## 4. Tech stack (decided)

| Concern | Choice | Notes |
|---|---|---|
| UI framework | `gpui` — **git dependency pinned to a rev** of `zed-industries/zed` | crates.io 0.2.2 is a stale snapshot; the NSVisualEffectView-based blur path needs git HEAD-era gpui. Pin one rev; upgrade deliberately. Apache-2.0. Rust ≥ 1.95. |
| Widgets | `gpui-component` (longbridge) where it saves time (inputs, popovers, virtualized list) | Apache-2.0. Note: it targets crates.io gpui 0.2.x — if it fights our git pin, vendor the few widgets we need. Do **not** copy Zed's `ui`/`picker`/`terminal_view` crates (GPL-3.0). |
| Terminal emulation | **None.** Custom GPUI `Element` painting the daemon's `GridUpdate` buffer | Batched quads for backgrounds + `text_system().shape_line` runs, modeled on Zed's TerminalElement *pattern* (pattern only — no GPL code). |
| Async / daemon IO | tokio (`UnixStream`) bridged with a vendored copy of Zed's `gpui_tokio` (~100 lines, Apache) | Control channel is NDJSON (line-framed JSON), data channel is `[type u8][len u32 BE][payload]` — hand-rolled codecs, no LengthDelimitedCodec needed. |
| JSON | `serde` + `serde_json` | Dates are **milliseconds since 1970** (`JSONEncoder.dirijor`); sorted keys not required for a client. |
| Vibrancy | `WindowBackgroundAppearance::Blurred` + translucent sidebar paint over an opaque content pane | See §7. |
| macOS glue | `objc2` + `objc2-app-kit` (NSStatusItem, NSSound, traffic lights), `objc2-user-notifications` | GPUI runs a real NSApplication loop; main-thread AppKit calls are fine. |
| Packaging | `cargo-packager` (universal binary, .app assembly, entitlements) + existing Apple ID / notarytool creds from Dirijor's release flow | Updater: a minimal self-updater (`crates/diri-updater`) reading a JSON feed published with each GitHub Release; Sparkle's appcast was not reusable, and neither was its keypair model. |
| Repo location | `diri/` — a Rust workspace **inside this repo** | Keeps the Swift protocol files adjacent as the porting reference; the Swift build ignores it; agents can worktree it. |

Community prior art worth skimming (not depending on): **tty7** (GPU terminal client for a daemon — architecturally closest to diri), **Loungy** (status item, panel windows, packaging), **Arbor / Rabbitty / hunk** (GPUI agent orchestrators).

---

## 5. Workspace layout

```
diri/
  Cargo.toml            # workspace
  PLAN.md               # this file
  crates/
    diri-proto/         # wire types: ControlMessage, Methods params/results, SessionRecord &
                        # friends, Frames codec, GridUpdate RLE codec, Paths. Pure, no IO. Heavily unit-tested.
    diri-client/        # tokio: DaemonClient (control), SessionAttachment (data channel),
                        # reconnect/backoff/heartbeat/event-resume. No gpui dependency.
    diri-term/          # the grid renderer: GridBuffer, TerminalElement (gpui), input encoding,
                        # selection, local scrollback viewport, find highlighting.
    diri-ui/            # design system (tokens from §8), StatusGlyph, BrandLogos (SVG paths),
                        # shared components: rows, palette chrome, hover cards.
    diri-app/           # binary: window/chrome, sidebar, terminal pane, palettes, settings,
                        # menu bar extra, notifications, sounds, usage store, updater.
  assets/               # icons, Info.plist template, entitlements
  scripts/              # build-app / package / notarize
```

Dependency direction: `diri-app → {diri-ui, diri-term, diri-client} → diri-proto`. `diri-proto` and `diri-client` compile fast and are testable headless against a real daemon.

---

## 6. Protocol port spec (what `diri-proto` + `diri-client` implement)

Everything below is verified against the Swift source; cite these files in code comments.

**Transport** — one unix socket, role decided by the first line of each connection (`ConnectionHub.swift:223`):
- Control channel: newline-delimited JSON `ControlMessage` — `request{id,method,params}` / `response{id,ok|err{code,message}}` / `event{event,seq,params}`. Max line 4 MiB.
- Data channel: first line is NDJSON `AttachRequest{attach: <sessionID>, fromOffset?, token?, role}`, then binary frames both ways: `[type u8][len u32 BE][payload]`, max 16 MiB. Types: `input=2`, `resize=3` (cols u16 + rows u16 BE), `ping=6`/`pong=7`, `grid=8`, `scroll=9` (dir u8, lines u16, col u16, row u16 BE), `modes=10` (bit0 altScreen, bit1 mouseReporting). Types 1/4/5 are legacy byte-replay — ignore.
- Port-forward channel exists (`ForwardRequest`) — v1 skip.

**Grid codec** (`Grid.swift:100-234`) — must match bit-for-bit, big-endian throughout:
`cols u16, rows u16, cursorCol u16, cursorRow u16, flags u8 (bit0 cursorVisible, bit1 fullSnapshot), rowCount u16`, then per row: `y u16, runCount u16`, runs of `[repeat u16, scalar u32, fgPacked u32, bgPacked u32, style u16]`. `TermColor` packs default/defaultInverted/ansi(u8)/rgb into u32; `TermStyle` is a u16 bitset (bold, underline, blink, inverse, invisible, dim, italic, crossedOut — SwiftTerm bit layout). The same row codec is reused by `session.read_scrollback_cells` (base64 in JSON).

**Methods** (full list in `Methods.swift`; client wrappers in `DaemonClient.swift:222-399`): `hello`, `session.{spawn,list,kill,remove,rename,resume,send_text,resize,set_owner,read_screen,read_scrollback,read_scrollback_cells,mark_seen,hibernate,wake,archive,unarchive,reopen_last,history,resume_from_history}`, `worktree.{create,list,remove,overview}`, `project.add`, `client.set_active`, `governor.configure`, `agent.readiness`, `events.subscribe`, `events.wait`, `hook.report`, `test.run`. `daemon.shutdown` is scoped to the explicit remote-config reload described in §3; `daemon.prepare_shutdown` remains unused.

**Client behaviors to replicate** (`DaemonClient.swift`, `SessionAttachment.swift`):
- Request correlation by monotonic u64 id; auto-reconnect with 0.5→8 s exponential backoff; idle heartbeat = cheap `hello` every 25 s.
- Event resume: track max `seq`; on (re)subscribe send `events.subscribe{sinceSeq}` — the daemon replays from a 4096-event ring, giving gapless reconnect.
- Attachment keepalive: ping after 20 s idle, declare dead after 30 s; on stream end, the *caller* re-attaches after ~500 ms (daemon repaints a full snapshot on attach, so recovery is always clean).
- Resize debounce 180 ms, but the **first** size report fires immediately (deferred agent launch waits for it — `AgentSession.swift:204`: the PTY isn't exec'd until the first client resize).
- Backpressure is daemon-side (desync + full-snapshot re-seed); the client just applies whatever arrives — `isFullSnapshot` replaces the buffer, otherwise patch `changedRows`.

---

## 7. Rendering & performance strategy

**Budgets (acceptance for the perf phase):** input→glyph latency < 8 ms at 120 Hz on Apple Silicon; idle CPU 0.0 % (no timers when nothing changes — caret blink only when focused); full-screen `yes`/`cat`-storm stays at display refresh without UI-thread stalls; memory < 150 MB with 10 sessions (3 resident renderers, like today's `TerminalResidency(capacity: 3)`); cold launch < 300 ms to first frame.

**Terminal element** (`diri-term`):
- Keep a plain `Vec<GridCell>` (cols×rows) + cursor state as the model. Applying a `GridUpdate` is a memcpy of changed rows — mark dirty, request a frame. Never render on a timer.
- Paint pass per frame: walk visible rows, coalesce same-bg runs into quads (`window.paint_quad`), coalesce same-style text runs and shape with `text_system().shape_line`. Cache shaped lines keyed by (row content hash, font) — rows unchanged since last frame reuse the cached `ShapedLine`.
- Cell metrics: derive `cell_width` from the monospace advance and lay the grid out ourselves (`cols = floor(width/cell_width)`), exactly like `GridTerminalView.swift:803` — this keeps client and daemon geometry in agreement.
- Font fallback cascade for spinner/box-drawing/emoji glyphs via gpui `FontFallbacks` (Swift used Menlo→AppleSymbols→STIXTwoMath→system).
- Scrollback: on wheel-up outside alt-screen, fetch history via `session.read_scrollback_cells` (coalesced, cached by absolute row index) and compose a local viewport; wheel events inside mouse-reporting/alt-screen forward as `scroll` frames (daemon encodes the right escape). Port `+Scroll/+Scrollback` behavior.
- Selection: client-side over composed grid+scrollback window; ⌘C copies. Port `+Selection`.
- Input: port `TerminalKeyEncoding.swift` — control chars, arrows/function keys, modifiers, bracketed paste. (Daemon handles bracketed-paste wrapping for `send_text`; direct typing goes raw.)
- Hidden panes: suspend rendering and drop GPU-side caches (Swift shrinks drawables to 1×1 to free ~33 MB each — mirror the idea).

**App-level:** sidebar is a virtualized list (uniform_list); all animations run off GPUI's frame clock with **absolute wall-clock phase** so every status glyph breathes in phase (Swift uses `timeIntervalSinceReferenceDate` — same trick); event-driven invalidation only.

**Vibrancy** (§ the one place we accept "close, not identical"): GPUI blur is per-window — set `WindowBackgroundAppearance::Blurred`, paint the sidebar with a translucent tint (tune alpha ~0.6–0.8 per light/dark to visually match `NSVisualEffectView .sidebar`), paint the terminal card fully opaque with `Radius.card` rounded left corners + 1 px hairline, matching `RootView.swift:144`. GPUI's blur deliberately strips the material tint, so we control tint ourselves. If fidelity is insufficient, escalate: host a real `NSVisualEffectView` behind the window's Metal layer via objc2 (same trick GPUI itself uses internally).

---

## 8. Design fidelity spec

The two UI exploration reports are distilled here; when in doubt, the Swift file is the spec.

**Tokens** (`DesignSystem.swift`):
- Radii (all continuous/squircle): chip 5, badge 6, row 7, card 10, panel 12.
- Type ramp: meta 11 med · sectionHeader 11 semibold · row 13 · rowEmphasized 13 med · title 13 semibold · displayTitle 15 semibold · metaMono 11 med mono.
- Ink: attention rgb(0.961,0.651,0.137) · danger rgb(0.961,0.271,0.227) · fresh rgb(0.204,0.780,0.349) · working(kind) = brand color (clay #D97757 for Claude, primary@0.82 codex/cursor, #4E82EE gemini).
- Fills over `primary`: hover 0.06 · multi-select 0.08 · selected 0.10 · subtle 0.06. Text: selected 1.0, unselected 0.75, labels 0.85.
- Space: indent 12, rowH 8, inset 10. Metrics: titleBar 42, rowHeight 28, traffic-light offset +12x/−6y.
- Motion: snap spring(0.32,0.74) · pop(0.40,0.60) · settle(0.55,0.82) · rowSelect snappy 0.16 · overlay fades easeOut 0.12 · breathe 2.6 s · sweep 2.4 s · pulse 1.8 s.

**Window chrome:** hidden titlebar, full-size content, transparent titlebar, manually repositioned traffic lights (+12,−6, reapplied on resize/focus), custom HStack split (NOT a native splitview): sidebar width @ 248 default (drag 200–400, double-click reset), 9 pt invisible resize strip, sidebar slides in/out (move+opacity, easeInOut 0.20). Min window 900×560.

**Sidebar:** structure = top bar (42) → New Agent row (⌘T) → scroll list (LazyVStack spacing 2, inset 10) of Pinned section + project sections + archived bucket → account footer pinned bottom. Rows 28 pt, radius 7; status glyph 16 pt fades to close button on hover; ⌘n hints; project headers with folder badge 18 pt + hover-only actions; drag-reorder with live preview; multi-select (⌘/⇧ click, drag batch); archive bucket as drop target; hover cards (delayed 700 ms, width 260, click-through panel); account footer with usage cost (numeric content transition) + update pill. Full details: `SidebarView.swift` (1556 lines) — the port task must read it directly.

**Status glyphs** (`StatusGlyph.swift`): always the agent's brand mark (SVG paths in `BrandLogos.swift` — Simple Icons paths for claude/openai/cursor/gemini, parsed from `d` strings; port the paths, render as gpui `Path`). Working = rotating angular-gradient sweep (2.4 s, codex reversed) + breathe 1+0.055·sin(t·2π/2.6); needsInput = opacity pulse 0.5+0.5·sin(t·2π/1.8), amber (danger red if destructive); doneUnseen = solid fresh green; idle 0.42 secondary; hibernated 0.28. Shell = blinking caret rect. 10 fps tick, paused offscreen/reduced-motion.

**Terminal pane:** header 42 (centered title + branch, trailing glyph + agent name), chips row (artifacts/ports/PR pills with state tints, max 4 + overflow menu), grid padded (2,12,10,12) on `theme.background`, card clipped with left corners 10 + hairline. Overlays: find bar (360 w, panel recipe), scrolled pill ("N lines · Return to live", material capsule), exited overlay (resume button / "Resuming conversation…"), archived overlay, remote-active takeover card.

**Floating-surface recipe** (shared component): theme.background fill, radius 12, stroke white@0.08 (dark) / black@0.10, shadow black@0.4 r24 y12, content in terminal-theme color scheme. Used by ⌘K palette (560 w, pinned 15 % from top, scrim black@0.25), ⌘P quick open, find bar.

**Other surfaces:** ⌘K command palette (actions + sessions, subsequence/word-prefix filter), ⌘P quick open (folder index 4 levels deep, git-aware ranking, ⏎ default agent / ⌘⏎ shell), Ctrl-Tab switcher (620×348 live preview + filmstrip, clay borders — preview via offscreen render of the grid buffer), ⌘⇧O overview (board 272-wide columns / list, lanes running/needsInput/done/asleep/ended, bulk close), ⌘⇧H history (transcript scan resume), worktrees sheet (620×460 cards + Clean Up), new-session popover (244 w, agent rows with availability dimming), settings window 420×520 (General/Terminal/Resources/Remote tabs — Remote can be v1.1), menu bar extra (300 w, attention-driven waveform icon), notifications (needs-input with Approve/Deny actions driving per-kind keystrokes), sounds (synthesized WAVs in code: needsInput two D5 taps, done E5→B5 rising fifth, frozen A3 — port the synth, ~50 lines), theme catalog (17 themes: 12 dark and 5 light, including Vesper; Dirijor Dark default bg rgb(0.071,0.075,0.094)), usage store (ccusage-style transcript scanner with byte-offset incremental cache — pure Rust port, no daemon involvement).

**Keyboard map:** ⌘T new default agent · ⌘⇧N codex · ⌥⌘T terminal · ⌘W close (confirm pref) · ⌘⇧T reopen · ⌘K palette · ⌘P quick open · ⌘⇧O overview · ⌘⇧H history · ⌘B sidebar · ⌥⌘W worktrees · ⌘,/⌘F/⌘G/⇧⌘G · ⌘1–8 jump · ⌘9 last session · ⌘↑/⌘↓ (and ⌘←/⌘→, ⌘[/⌘]) prev/next session, wrapping · ⌃⌘↑/⌃⌘↓ reorder within project · ⌘J next session needing input · ⌘R rename selected · ⌘⇧W archive selected · Ctrl-Tab switcher (release-to-commit, Shift reverses) · Esc cascade · middle-click row closes.

---

## 9. Phases & delegatable tasks

Dependencies are noted; tasks with the same phase and no dep arrow are parallel. Each task = one agent, one worktree, PR into `main` touching only `diri/`. **Every task's acceptance includes: `cargo build && cargo test && cargo clippy -- -D warnings` clean, and no files outside `diri/` modified.**

### Phase 0 — Foundation (serial, 1 task)
- **T0 Workspace bootstrap.** Create the workspace per §5, pin gpui git rev, `create-gpui-app`-style hello window opening with `Blurred` background + hidden titlebar, CI-less build script (`diri/scripts/build.sh`). Commit a `rust-toolchain.toml` (1.95+). Acceptance: `cargo run -p diri-app` shows a blurred empty window with repositioned traffic lights.

### Phase 1 — Protocol core (parallel after T0)
- **T1 diri-proto: control types.** All `Methods.swift` params/results + `SessionRecord`/`SessionStatus`/`AgentKind`/`AttentionLevel`/`Project`/etc as serde types (ms-epoch dates, lenient unknown-variant handling), `ControlMessage` envelope. Acceptance: round-trip tests against JSON fixtures **captured from the live daemon** (`nc -U` the socket or replay `state.json`), including a real `session.list` response.
- **T2 diri-proto: frames + grid codec.** Binary frame codec + `GridUpdate` RLE decode/encode + scrollback-cells block decode. Acceptance: byte-exact round-trip property tests; a fixture captured from a live attach decodes to sensible cells.
- **T3 diri-client: DaemonClient.** tokio UnixStream, NDJSON framing, request correlation, backoff reconnect, heartbeat, `events.subscribe` seq-resume, typed method wrappers, connection-state stream. **Hard rule from §3: no shutdown methods, no daemon spawning logic beyond "launch installed dirijord if socket absent".** Acceptance: integration test against the real local daemon — hello, list sessions, subscribe, spawn+kill a `shell` session in a temp dir.
- **T4 diri-client: SessionAttachment.** Attach handshake, frame read/write loop, ping/pong keepalive, chunk stream (grid/modes), input/resize/scroll senders, caller-driven re-attach. Acceptance: headless integration test — spawn shell session, attach, send `echo hi\n`, assert the decoded grid contains `hi`; resize and assert reflow.

### Phase 2 — Terminal renderer (after T2; T5 can start on fixtures before T4 lands)
- **T5 diri-term: GridBuffer + TerminalElement.** Cell model, GridUpdate application, batched quad/text painting, shaped-line cache, cursor (block + blink-on-focus), theme (16-color ANSI + default fg/bg/cursor/selection from §8 catalog), cell-metric layout, font fallbacks, rendering-suspended mode. Acceptance: a demo binary replaying a captured frame dump renders correctly (screenshot-compared against the Swift app for the same dump); idle draws zero frames.
- **T6 diri-term: input encoding.** Port `TerminalKeyEncoding.swift` (+ paste). Acceptance: unit tests enumerating the Swift file's mappings; interactive check — fish, vim arrows, ctrl-c, opt-arrow word jump work in a live session.
- **T7 diri-term: scroll + scrollback + selection + find.** Wheel routing (modes-aware), local scrollback viewport with `read_scrollback_cells` fetch/cache, selection + ⌘C, find model (debounced 200 ms, cap 500, live+history, highlight spans) + scrolled pill. Depends on T4, T5. Acceptance: scroll through 1000 lines of history, select across the seam, find wraps and anchors like the Swift app.

### Phase 3 — App shell (after T1, T3; parallel tracks)
- **T8 diri-ui: design system.** Tokens (§8) as Rust constants/helpers, squircle backgrounds, hover/selected fill components, shared floating-surface recipe, motion (shared wall-clock phase), brand marks: port the 4 SVG `d` strings + a tiny path parser (or pre-convert to gpui `Path` ops at build time), `StatusGlyph` with all states/animations, `AgentLogo`. Acceptance: a gallery demo view showing every token/glyph state side-by-side with screenshots of the Swift originals.
- **T9 AppModel + session store.** Event-driven store mirroring `AppModel.swift` behaviors: sessions/projects maps, sidebar projection (ordering rules incl. manual ranks + createdAt-desc fallback), selection + multi-select, MRU, terminal residency (cap 3), attention rollup, mark-seen, auto-resume-on-daemon-restart, close confirmation flow, preferences (own defaults domain, same keys/semantics). Depends on T3. Acceptance: headless tests driving fake events assert ordering/selection/residency invariants match the Swift rules documented here.
- **T10 Window chrome + sidebar.** RootView equivalent: blurred window, translucent sidebar (width persistence, drag handle, toggle animation), opaque terminal card with rounded left corners, top bar, New Agent row + popover, project sections (collapse, hover actions, reorder), session rows (glyph/close hover swap, rename inline, hover cards, ⌘n hints), pinned + archived sections, drag-drop reorder/archive, account footer + usage display, empty state. Depends on T8, T9. **The big one — budget accordingly.** Acceptance: side-by-side screenshot match with the Swift app in the preview fixture scenarios (`SidebarPreview.swift` mock data reproduced as a diri fixture).
- **T11 Terminal pane assembly.** Pane header + chips row (artifact/port/PR pills + checks popover), grid mount with residency, overlays (exited/resuming, archived, remote-active), find bar UI, zoom (⌘+/-/0 → font size 10–20). Depends on T5–T7, T9. Acceptance: full interactive session lifecycle in diri: spawn → work → needs-input glyph → resume after daemon restart.

### Phase 4 — Navigation & surfaces (parallel, after T9 + T8)
- **T12 Command palette (⌘K) + fuzzy scorer + quick open (⌘P).** Port `FuzzyScore` exactly (bonuses 6/8/12, boundary chars), palette actions list, quick-open folder index (off-thread scan, 4 levels, git bonus, debounce 25 ms). Acceptance: same ranking as Swift for a table of test queries.
- **T13 Ctrl-Tab switcher + session overview (⌘⇧O).** Switcher with live grid previews (offscreen render of resident buffers, logo fallback), release-to-commit key handling; overview board/list with lanes, filters, bulk close. Acceptance: keyboard semantics identical (Shift reverse, Esc cancel, arrows).
- **T14 History (⌘⇧H) + worktrees sheet + settings.** Transcript scanners (claude/codex dirs), resume-from-history; worktrees cards + cleanup confirm; settings tabs General/Terminal/Resources/Remote, including execution hosts and iOS companion access. Acceptance: resuming a real historical Claude conversation works end-to-end.
- **T15 Menu bar extra + notifications + sounds + usage store.** NSStatusItem (objc2) with attention icon + popup window (300 w), UNUserNotificationCenter with Approve/Deny category driving `send_text` per-kind keystrokes, WAV synth port, usage store port (incremental transcript parser + pricing table + 5-h session window). Acceptance: needs-input in a hidden session chimes, notifies, Approve types the right keys; usage numbers within 0.1 % of the Swift app for the same transcripts.

### Phase 5 — Performance & fidelity hardening
- **T16 Perf pass.** Instruments/Metal HUD profiling against the §7 budgets; shaped-line cache hit rates; frame-time traces under `yes`-storm ×3 sessions; memory audit; fix regressions. Acceptance: all budget numbers recorded in `diri/PERF.md` and met.
- **T17 Fidelity pass.** Pixel-diff sweep across every surface vs the Swift app (use `DIRIJOR_PREVIEW_SNAPSHOT` on the Swift side for reference PNGs where applicable); tune vibrancy tint per appearance; animation phase/feel review. Acceptance: annotated before/after gallery; sign-off list of intentional deviations (expected: blur tint nuance).
- **T18 Coexistence soak.** Run Dirijor + diri simultaneously for a day of real work: no daemon restarts, no resize thrash when different sessions are focused, documented behavior when the same session is focused in both, event-resume across daemon restart. Acceptance: written soak report; any protocol-level fix filed.

### Phase 6 — Ship
- **T19 Packaging.** cargo-packager config: universal binary, icons (new diri icon), Info.plist, entitlements, codesign + notarize with existing creds, DMG; `scripts/release.sh` parallel to the Swift one. Acceptance: notarized diri.app launches clean on a second Mac.
- **T20 Updater + docs.** Updater **done**: `crates/diri-updater` + `crates/diri-app/src/updates.rs`, JSON feed at `/diri/appcast.json` on the existing worker, update pill in the sidebar footer, Settings → General → Updates, `⌘K → Check for Updates…`, published by `diri/scripts/release.sh`. Trust is Developer ID + notarization pinned to the running app rather than a Sparkle-style keypair — see [UPDATING.md](UPDATING.md). Remaining: `diri/README.md` (install, coexistence rules, known limitations), and the acceptance run — an old signed build updating itself to a new one, which needs a real notarized release.

**Critical path:** T0 → T2 → T5 → T11 → T16/T17 → T19. Total ~20 tasks; T1–T4 and T8 can all run in parallel immediately after T0; the sidebar (T10) is the largest single task and can start as soon as T8+T9 land.

---

## 10. Risks & mitigations

| Risk | Mitigation |
|---|---|
| GPUI API churn (pre-1.0, git pin) | Pin one rev in T0; single owner for upgrades; wrap gpui-isms behind `diri-ui` helpers where cheap. |
| Blur ≠ `.sidebar` material exactly | Tunable tint layer; escalation path: host a real NSVisualEffectView via objc2 (§7). Accept minor deviation, get user sign-off in T17. |
| gpui-component targets crates.io gpui, we pin git | Use it only for leaf widgets; vendor/rewrite if versions fight. Nothing in the plan hard-depends on it. |
| Desktop/desktop resize thrash (same session in both apps) | Documented limitation v1 (§3); later: additive protocol role extension. |
| Grid codec drift | Byte-exact fixtures captured from the live daemon in T2; codec is frozen (additive-only wire). |
| Swift app updates daemon while diri is open | diri tolerates disconnect (backoff + seq-resume + full-snapshot re-attach) — explicitly tested in T18. |
| Missing kitty-keyboard protocol (daemon side uses SwiftTerm) | Non-issue for diri: key encoding targets the same daemon the Swift app talks to today. |

## 11. Later (post-v1 backlog)
Remote/TCP endpoint + token auth (iOS parity), port-forward channel UI, a Rust daemon (only if ever needed — would ship behind the same wire protocol), Linux/Windows builds, per-session split panes, GPU screenshot-based switcher previews, multi-desktop geometry roles.
