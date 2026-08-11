# Porting the engine to Rust

The goal is a cross-platform diri. The app is already portable — roughly 4% of
`diri-app` is macOS-specific and it is cfg-gated. What binds diri to macOS is
the *engine*: the Swift `dirijord` stack in `Sources/`, which owns PTYs,
sessions, detection and the control socket.

This is the record of replacing it with `crates/diri-engine`.

> **Completed 2026-08-11.** The app now ships only the Rust engine, holder,
> automation CLI, and MCP server. The legacy `Sources/` implementation and its
> build pipeline have been removed. The notes below preserve the port's history.

## Rules this port follows

1. **Additive.** The Swift daemon keeps running and serving live sessions
   throughout. Nothing in `Sources/` is modified or deleted until the Rust
   engine is a proven replacement.
2. **Formats are load-bearing.** A log, socket message or on-disk record
   written by one engine must be readable by the other, or switching strands
   whatever sessions were live at the time.
3. **Rules stay data.** Detection manifests are read from
   `Sources/DirijorCore/Resources/manifests/`, not copied. One source of truth,
   no drift.
4. **Platform gaps are named, not hidden.** Unix is implemented; Windows gaps
   sit behind a `cfg` with the specific API that fills them documented at the
   seam.

## Status

| Layer | State | Notes |
|---|---|---|
| Output log | **done** | Byte-identical format; verified against a real 31 MB log the running Swift daemon wrote |
| PTY | **done** (unix) | Signal reset, `setsid`/`TIOCSCTTY`, fd hygiene, group kill — each with a test. Windows needs ConPTY, shape documented in `pty::unsupported` |
| Detection | **done** | All 19 manifests, 81 rules, 39 patterns compile and evaluate unchanged |
| Status reducer | **done** | Anti-flicker, blocker arbitration, startup grace, subagent isolation, staleness |
| Headless emulation | **done** | `alacritty_terminal`; OSC 9;4 progress scanned by hand |
| End-to-end pipeline | **done** | Real process → PTY → emulator → manifest → reducer → needs-input |
| Session | **done** | Self-driving: polled pump, ticks while quiet, kills its child on drop |
| Registry + persistence | **done** | Reads and round-trips the real `state.json` — 30 sessions, 84 projects preserved |
| Control socket | **core done** | Handshake, spawn, list, send_text, resize, read_screen, kill over NDJSON on an owner-only socket. Unported methods answer `not_found` rather than dropping the connection |
| Agent descriptors | **done** | argv, env scrubbing, colour assertion, resume flags — all read from the manifest |
| Spawn (control + MCP) | **works** | `session.spawn` builds argv from the manifest; hook/MCP injection still missing, so a Claude session started this way is screen-detected rather than hook-driven |
| Hook + notify parsing | **done** | Claude hooks and Codex notify → signals, with identity, titles and needs-input detail |
| Git facts | **done** | Branch and linked-worktree detection by reading `.git` directly; porcelain parsing |
| Worktree operations | **done** | Create, list, remove against real git; paths canonicalized so they match what git reports |
| MCP server | **done** | JSON-RPC stdio protocol + 13 tools executing against the registry |
| Holder (session survival) | **done** | Server, manager, launcher, client — same sockets, NDJSON protocol, pid files, and OSC 777 exit marker as `DirijorHolderKit`. Interop-proven live: the Rust client/launcher drives the real Swift `dirijord-holder` binary (spec encode → Swift decode, stat/write/resize/signal/kill-tree, Swift-written log read back, Swift exit marker parsed) |
| Injection + lifecycle methods | **done** | Spawn-time hook/MCP injection (shim files + per-manifest flags + minted conversation UUIDs); hook.report → reducer with record identity folding; resume / resume_from_history / reopen_last; hibernate/wake via SIGSTOP/SIGCONT tree signals; read_diff (ported WorktreeDiffLoader script); agent.readiness doubling as the agent catalog; project.add with Swift's exact ProjectID hash. set_owner / client.set_active / governor.configure are accepted no-ops (mobile ownership + governor sampling not modeled yet) |
| Attach server | **done** | The binary data channel on the shared socket: first-line sniff, full-grid seed + modes on attach, per-session pump broadcasting 16ms-paced diffs to every sink, input/resize/scroll/ping inbound. Grid extraction ports `gridUpdate(full:)` incl. the diff baseline; scrollback is byte-budgeted (1 MiB) with `read_scrollback`/`read_scrollback_cells`. Known deviation: scrollback row indices slide once the budget evicts (alacritty has no scroll-invariant index) — visible screen and recent history exact |
| Event stream | **done** | `EventBus` port: seq-stamped pub/sub, bounded replay ring, per-subscriber overflow with the one-per-burst `events.dropped` marker, server-side filters. `events.subscribe` streams frames on the control connection; `events.wait` long-polls status targets with the Swift alias table; a registry watcher publishes `session.updated` on live transitions |
| Held sessions + adoption | **done** | `Session::spawn` goes through a holder when a `HolderConfig` is present; `Registry::restore` scans the holders directory and adopts live holders after a restart. Tested: a session survives its session object being dropped and a brand-new registry picks it up mid-flight |
| History / resume | **done** | Claude and Codex transcript stores; verified against the real ones — 500 conversations in 0.9s |
| Remote hosts (ssh + tmux) | **done** | argv, reattach naming, shell quoting verified through a real shell, scp handoff |
| Rust daemon binary | **done** | `dirijord-rs`: same socket, lock singleton, state file, boot-log stamp, holder adoption, manifest bundle + overrides loading. It is the only engine shipped in `Resources/bin`. |
| Legacy daemon retirement | **done** | The app launcher, holder, automation CLI, MCP server, packaging, and CI all use the Rust workspace; `Sources/` and the SwiftPM package have been removed. |

Since ported into `dirijord-rs` (2026-08-07 second pass): artifacts scanning
+ PR enrichment + listening ports, the full resource governor with all three
auto-hibernation policies, spawn-on-host + remote revive over ssh+tmux,
`host.sync_prefs`, `host.locate_repo`, `session.migrate` (WIP-commit handoff
with transcript shuttle, prepare flow tested against two local checkouts),
`worktree.overview`, and the Playwright browser pool (`test.run` /
`browser.act` via the node sidecar).

Third pass (2026-08-07, retirement checklist): wake-on-input is now proven
end to end — `tests/wake.rs` freezes a real held session and shows that the
only triggers the app actually uses (control `send_text`, a bare data-channel
attach) SIGCONT the tree, deliver the input, and clear the hibernation
record; the app never calls `session.wake` itself. The polish trio landed the
same day: screen-checkpoint restore, deferred launch at the settled client
size, and verified initial-prompt injection (notes below).

Remaining gaps in `dirijord-rs` (all answer clean errors):

- **Mobile companion stack**: the remote TCP listener + token gate, and
  role-based geometry arbitration (`session.set_owner` is an accepted no-op;
  a phone attach behaves like a second desktop sink). One stack — without
  the listener the phone can't reach this daemon, so the arbitration is
  unreachable until both land together.
- **Port forwarding**: the data-channel `ForwardRequest` mode (phone preview
  tunnels) is unported — same mobile stack.
- ~~Polish, not parity blockers~~ — the polish trio (screen-checkpoint
  restore, deferred launch, verified prompt injection) is ported; see the
  holder notes below.

Holder-port notes, for whoever wires the daemon:

- **The mixed-fleet upgrade works by construction**: paths, spec JSON, and the
  log format are shared, so a Rust daemon adopts Swift-spawned holders (this
  direction is what `tests/holder_interop.rs` proves against the real Swift
  binary) and a Swift daemon would adopt Rust-spawned ones. The reverse
  direction has no automated test — it only matters for a rollback, and would
  need Swift-side test infrastructure.
- **Screen checkpoints are ported** (2026-08-07). The held pump writes
  `<id>.screen.plist` after a 1s output settle — same binary-plist format
  and keys as `ScreenCheckpoint.swift`, proven against Apple's own parser
  (`plutil`) in both directions — and adoption restores from a fresh-enough
  checkpoint, falling back to the bounded raw-tail replay (256 KiB) for
  anything stale, malformed, or geometry-mismatched. A Rust daemon adopting
  a Swift-spawned fleet therefore seeds from Swift's checkpoints, and a
  rollback reads ours.
- **Deferred launch is ported** (2026-08-07). `SessionSpec.defer_launch`
  (set on every daemon spawn path, not on adoption) holds the exec until the
  first client size settles — 120ms debounce per proposal, 400ms fallback
  for viewless spawns — so TUI banners render at the real width. Input
  typed before the exec queues and flushes right after it, and a kill
  inside the window means the child never exists.
- **A markerless holder death** (SIGKILL of the holder itself) is caught by a
  ~2s liveness probe in the held pump, so a session cannot look alive forever.
- **Initial-prompt injection is verified** (2026-08-07), porting Swift's
  `injectInitialPrompt`: readiness-gated (bracketed-paste is the composer
  tell, screen-stability the fallback), then up to three attempts, each
  checked on screen — retried only when the screen shows no evidence at all
  that input landed, so a swallowed prompt is rescued and a delivered one is
  never duplicated.

## What the risky parts turned out to be

- **Regex dialect.** Swift used `NSRegularExpression` (ICU); Rust's `regex` has
  no backreferences or lookaround. Every shipped pattern compiles unchanged —
  this was the main unknown and it is retired.
- **PTY details.** The signal-mask reset in the child is not incidental: leave
  `SIGWINCH` ignored and agents never repaint after a resize. Tested directly.
- **Emulation.** Grepping the byte stream would misread erased text as present.
  A real emulator is not optional, which is why a dependency is justified here.
- **Authority was code when it should have been data.** The port briefly
  hardcoded "claude-code" to pick the hooks-led reducer. Every manifest already
  declares `agent.statusAuthority`; reading it means a new agent gets the right
  behavior by existing as a file.

## Windows, specifically

Not attempted yet, and it is a genuine implementation task rather than a port:
ConPTY replaces the fd model, there are no process groups, and job objects take
over kill-tree duty. `pty::unsupported` lists the exact calls. Linux should work
today apart from being untested — nothing in the engine is Darwin-specific
beyond what `cfg(unix)` already covers.
