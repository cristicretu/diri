# Remote Architecture: Bootstrapped PTY Holders

## Status

The remote transport refactor is complete.

As of 2026-08-09, the bootstrapped Rust Remote PTY Holder is Diri's only remote
session transport. The former Rust `ssh -t` + `tmux` implementation, its session
naming, cleanup paths, configuration surface, tests, and runtime fallback have
been removed. Diri never falls back to `tmux`.

This document is the active architecture and maintenance baseline, not an
initial proposal or migration plan. In this document:

- **completed baseline** means the Rust-only bootstrap and PTY replacement that
  define the current remote architecture;
- **future enhancement** means optional functionality that may be built on top
  of that architecture and does not make the remote refactor incomplete;
- **Remote Helper** and **`diri-remote`** refer to the same minimal executable;
- **Holder** means the per-session process that owns the remote PTY and Agent
  process tree.

The completed execution path is:

```text
Diri desktop app
  -> local Rust Engine
  -> ssh -T binary channel
  -> short-lived remote Bridge
  -> per-session diri-remote Holder
  -> Claude Code / Codex / Shell
```

The removed path was:

```text
Diri
  -> local PTY
  -> ssh -t
  -> remote SSH PTY
  -> remote tmux PTY
  -> Agent
```

## Design priorities and hard constraints

The priority order is fixed:

1. correctness and stability;
2. performance among correct designs;
3. least privilege and the smallest remote dependency surface;
4. explicit recovery and diagnostic behavior.

Correctness is a release gate. Session identity, input delivery, PTY draining,
terminal reconstruction, process cleanup, and authentication boundaries must
not be weakened for throughput.

The implementation is entirely Rust-owned under this workspace. For remote
architecture and maintenance decisions, `Sources/`, `Package.swift`, Swift
tests, Swift daemons, Swift Holders, and Swift wire formats are treated as
nonexistent. Historical Swift behavior does not create a compatibility
requirement.

The current baseline:

- uses SSH only as an authenticated, encrypted byte transport;
- translates local process-control signals to the verified Helper target OS before sending the existing numeric signal frames;
- gives Diri direct ownership of the remote Agent PTY lifecycle;
- requires no remote `tmux`, `screen`, `zellij`, Node.js, Python, `socat`, `nc`,
  `curl`, `wget`, or preinstalled Diri service;
- never requests `sudo` or host-wide configuration;
- fails closed with a structured error when no valid Helper artifact or
  capability-compatible Helper is available;
- retains orchestration and user-facing state in the local Rust Engine.

## Account-profile enhancement

The local Engine owns the account-profile catalog and durable per-session
launch binding described in [ACCOUNTS.md](ACCOUNTS.md). A remote profile is scoped
to one Agent and saved host. Its directory resolves against the remote login
environment; credentials never move between machines. The Engine prepares a
missing provider directory with its existing bounded, authenticated fixed-script
SSH seam (the directory travels as stdin data), then sends the selected provider
environment through the existing structured LaunchRequest. Resume and fork use
the recorded binding, not the current catalog default. Cross-host migration of
bound sessions fails until an explicit destination-account mapping exists.

An explicit same-host Claude account continuation is also owned by the local
Engine. It preflights the source and destination, stops the existing Holder's
Agent tree, reads the final main JSONL transcript through bounded fixed-script
SSH, installs it atomically in the selected profile, and resumes the same
conversation through the existing Holder launch path. Only transcript bytes
travel through Engine memory; credentials and provider configuration never move.
The transcript is bounded to 64 MiB. Symlinks and conflicting destination history
fail closed; an existing byte-prefix copy permits switching back. The updated
profile binding is durable before relaunch. Preflight errors leave the original
session running; later failures keep the saved conversation recoverable and
report the stopped session. This does not implement cross-host handoff, file
rewind/subagent checkpoint transfer, or provider usage/authentication discovery.

The Helper protocol and Holder ownership remain unchanged. Account settings,
directory preparation, and profile resolution belong to the local Engine;
the Holder receives only the resulting argv/environment/cwd. This enhancement
adds no remote service, credential store, or transport dependency.

## Why the old transport was replaced

`tmux` provided a practical PTY, process survival, and reconnection mechanism,
but it also imposed a remote package dependency and inserted another terminal
emulation layer:

```text
local PTY -> SSH PTY -> tmux pane PTY -> Agent
```

Each layer could alter `$TERM`, color handling, mouse events, resize behavior,
alternate-screen behavior, control sequences, and TUI layout. The local Engine
also owned only the local `ssh` process, which prevented precise observation and
control of the remote foreground process group, child processes, signals,
resource facts, and exit causes.

Diri already uses Holder processes to decouple PTY, Agent, GUI, and daemon
lifecycles locally. Applying the same ownership model remotely removes the
external multiplexer while keeping SSH-only onboarding.

## Component and authority model

### Local Rust Engine

The local Engine is authoritative for:

- `SessionRecord` and project/worktree identity;
- Agent manifests and structured launch requests;
- status reduction, including Working, Permission, Question, and Done;
- desktop broadcasts and user-visible lifecycle operations;
- host configuration and remote bootstrap orchestration;
- reconnect policy and remote failure classification;
- local-to-remote version and capability gates.

The desktop app and `diri-client` request remote operations through the Engine.
They do not execute SSH directly.

### Remote Helper

`diri-remote` is deliberately not a second Diri Engine. It owns only facts that
cannot remain local:

- one Agent PTY and process tree per session;
- the current terminal grid, cursor, modes, and dimensions;
- bounded raw output and bounded scrollback;
- output sequence and offset;
- process exit facts;
- one controller lease and its monotonically increasing epoch;
- one owner-only Unix domain socket for attachment.

The Helper exposes these subcommands:

```text
diri-remote probe
diri-remote launch
diri-remote attach
diri-remote inspect
diri-remote list
diri-remote kill
diri-remote environment
diri-remote directories
diri-remote persistence
diri-remote gc
```

Each session has exactly one independent Holder and one Unix socket. There is no
multi-session remote Diri supervisor. A Holder may spawn one minimal liveness
guard whose only responsibility is to wait for Holder pipe closure and kill
that session's Agent process group. The guard owns no PTY, socket, terminal
state, or orchestration.

### Shared Rust crates

The completed ownership boundaries are:

- `diri-engine`: local session authority, host orchestration, bootstrap, SSH,
  reconnect, and status reduction;
- `diri-proto::remote_pty`: versioned Helper protocol and wire codec;
- `diri-pty`: shared low-level PTY primitives and bounded metadata checkpoint scheduling;
- `diri-terminal-state`: shared headless parser, grid, snapshot, and diff model;
- `diri-remote`: minimal remote Helper executable;
- `diri-client`: app-to-Engine client only;
- `diri-term`: GPUI terminal rendering and input integration;
- `diri-app`: desktop UI and user actions;
- `diri-node`: optional enhanced node mode, never required by SSH bootstrap.

`diri-remote` does not depend on GPUI, `diri-app`, `diri-client`, `diri-node`, or
the full Engine. There is one shared terminal parser implementation rather than
separate local and remote parsers.

## SSH transport

Helper protocol channels use:

```bash
ssh -T
```

SSH performs authentication, encryption, remote command execution, and binary
byte transport. It does not allocate or own the Agent PTY. Helper frames have
exclusive use of protocol stdin/stdout.

OpenSSH configuration is reused. A finite-lived ControlMaster may reduce repeat
authentication and handshake cost, but it is only a performance optimization.
Session survival never depends on the ControlMaster.

All internal remote commands invoke a fixed, internally generated POSIX shell
entry point. User-controlled Agent arguments are never interpolated into shell
strings. On macOS, OpenSSH prompts are routed through the packaged Rust
`diri-ssh-askpass`; the Engine does not parse passwords or host-key answers from
the Helper protocol channel.

Overlong OpenSSH `ControlPath` values are mapped into a short owner-specific
namespace after owner, file type, and symlink validation. This avoids Unix
socket path limits without using a shared untrusted control socket.

## Bootstrap and remote environment initialization

Diri follows the useful part of Zed's remote model: establish SSH, inspect the
remote platform, install an exact server-side executable automatically, and
then speak a structured protocol. The implementation was compared against Zed
at commit `dc2a339`, but Diri keeps its own narrower Holder boundary and does
not install a full remote editor service.

Initialization is an explicit state machine:

```text
resolve host configuration
  -> establish SSH transport
  -> probe OS and CPU architecture
  -> select an exact packaged artifact
  -> probe the installed Build ID and protocol
  -> upload to a nonce staging path when required
  -> verify length and SHA-256
  -> verify Build ID, protocol, and capabilities
  -> activate with an atomic no-replace rename
  -> capture account and cwd environments
  -> probe persistence
  -> report a sanitized readiness result
```

Bootstrap is idempotent and safe under concurrent callers. An interrupted or
failed installation may remove only its own nonce staging file. It must never
delete a validated Helper or live session state.

The packaged Helper catalog is authoritative. Diri never downloads an
executable from a URL selected by the remote host and never runs an arbitrary
installer returned by the remote host. A loose Cargo build may select the exact
current Helper next to the Engine executable, but it must satisfy the same hash,
Build ID, protocol, and capability checks.

Every stateless remote management action performs a version gate. After the app
or Engine is updated, the first remote action installs the matching Helper when
necessary. The host-management UI also exposes **Reinstall Environment**, which
forces the same verified staging and activation path without overwriting or
terminating Helpers still referenced by live sessions.

Versioned Helpers coexist:

```text
~/.cache/diri/bin/
  protocol-<major>/
    <build-id>/
      diri-remote

~/.local/state/diri/sessions/
  <session-id>/
    session.json
    holder.sock
    output.log
```

Existing sessions continue to use their creation Build ID. Garbage collection
retains every Build ID referenced by a live session. A Helper is never replaced
in place.

Required permissions are:

- cache and state directories: `0700`;
- Helper executable: `0700`;
- state/log files and Unix sockets: owner-only, with regular files `0600`.

Bootstrap validates every interpolated path component and rejects untrusted
symlinks. A missing catalog entry, corrupt artifact, unsupported target, build
mismatch, or capability mismatch fails closed and never triggers a `tmux`
fallback.

## Supported platforms

The Remote Helper support matrix is deliberately limited to:

```text
Linux x86_64
Linux aarch64
macOS arm64
```

macOS x86_64 is not built, packaged, tested through Rosetta, or supported.
Intel macOS probes return `unsupported-platform`.

Linux artifacts are static musl executables and are tested in minimal/older
userspaces. The macOS artifact depends only on supported system libraries and
is validated against the minimum supported macOS version. Packaging fails when
any supported artifact is missing or its manifest metadata does not match.

## Agent launch environment

Non-interactive SSH `$PATH` is not assumed to contain `claude`, `codex`, or other
Agents. Remote tools may depend on a login shell, Homebrew, `nvm`, `mise`, or
user-local installation paths.

The Helper resolves the account login shell from the remote user database. It
captures the account-login environment and the target-cwd environment through a
dedicated file descriptor so shell startup noise cannot corrupt protocol
stdout. Capture is bounded by time and size, and failure is reported explicitly.

The local Engine sends a structured launch request containing:

- `argv` as an ordered argument vector;
- `cwd` as a separately validated absolute path;
- a filtered environment map.

The PTY child executes `argv` directly. Agent launches are never assembled by
concatenating shell text.

Local credentials, local Unix sockets, authentication responses, and local
process environment are not copied wholesale. Sensitive or irrelevant local
variables, including local-only `DIRI_` and `SSH_` state, are removed. The
remote account and cwd environment remain authoritative, with only explicitly
allowed launch overrides applied.

## Directory and project model

The Engine provides a unified `host.list_directories` RPC. Remote directory
selection uses the exact installed Helper's read-only `directories` command;
SSH and remote path handling never move into the desktop app.

Each request lists one directory level, returns at most 512 directories, and
bounds total scanning work. The canonical path returned by the Helper is the
authority for later navigation. A host's `defaultCwd` is used only for the
initial location and never overwrites a user-selected absolute subdirectory.
The default remote directory is the account home directory (`~`) unless the
host configuration explicitly specifies another valid directory.

Project identity includes both execution location and directory. Every session
belongs to exactly one top-level Project. The same path on two SSH hosts is two
different Projects, and project-level Agent creation inherits that Project's
host and directory.

## Agent executable discovery

Agent availability and executable overrides are target-specific: local and
each configured SSH host have independent catalog state. The Engine owns the
catalog and preferences; the Helper only reports filesystem facts from the
remote account.

Local desktop discovery and launches share a normalized PATH: captured login
shell entries first, inherited entries next, then user package-manager and
standard executable directories. Fallbacks include pnpm's old home-directory
shims and pnpm 11's `bin` layout, `PNPM_HOME`, `XDG_DATA_HOME`, Bun, Cargo,
mise, and Volta. They also apply when local shell capture fails or times out.
These local fallbacks are never added to remote launch environments.

Protocol 1.3 adds the required `executable-discovery` capability. One bounded
`executables` request carries every bundled manifest binary and any configured
override. The Helper captures the login environment exactly once, resolves all
queries directly from that PATH without spawning `which` per Agent, validates
manual paths as executable regular files, and returns both the detected and
configured resolution. An Agent launch reuses that same captured environment
and cwd, so discovery does not add a second login-shell startup.

The Engine caches each target catalog for five minutes and single-flights
concurrent scans per target. Menus render only cached facts and never execute
filesystem or SSH work. Missing Agents stay in Settings for discovery and
manual binding but do not appear in quick-create menus. A valid manual path has
precedence over PATH; an invalid override is reported while a valid PATH result
remains usable. Executable preferences and quick-create visibility are stored
in an owner-only, additive Engine configuration file.

## Holder and process lifecycle

The Holder owns the PTY master, the Agent child/process group, terminal state,
controller state, and the session socket. A Bridge is a short-lived adapter:

```text
SSH stdin/stdout <-> remote Unix socket <-> Holder
```

When an SSH channel disconnects, the Bridge exits. If host policy permits
detached user processes, the Holder and Agent continue independently.

Application exit follows explicit lifecycle rules:

- with no active sessions and no other control responsibility, the app asks the
  Engine to persist and exit; idle local holder-management processes also exit;
- with a live local session, the local Engine, Holder, and Agent remain because
  they are necessary session owners;
- with a live remote session, the remote Holder, guard, and Agent remain, while
  the local Engine remains only when required for local orchestration, status,
  or an active Bridge;
- no process is retained merely to make the next app launch faster;
- cached Helpers and manifests are files, not background services;
- `diri-node` runs only when the user explicitly enables that separate mode.

First-party Claude and Codex manifests execute the Agent directly. After the
Agent exits, the Holder does not fall back into a login shell. Normal exit,
signal exit, external exit, and Holder failure are reported distinctly so the
desktop can detach the terminal and remove the corresponding Agent row. Engine
restart/adoption preserves still-running sessions.

## Persistence capability

`setsid()` or double-forking does not guarantee survival after SSH logout on
every Linux host. PAM or `systemd-logind` policy may kill all processes from a
login session.

Each host is therefore probed rather than assumed. Diri closes any finite-lived
bootstrap ControlMaster, launches a temporary Holder over a non-multiplexed SSH
connection, waits for that underlying connection to close, reconnects over a
second non-multiplexed connection, checks the process identity, and cleans up
the test session. The result is one of:

```text
native-detach
user-supervisor
non-persistent
```

Behavior is fixed:

- `native-detach`: use the ordinary lightweight Holder;
- `user-supervisor`: use only an already available, no-configuration transient
  user supervisor;
- `non-persistent`: allow the session but display a persistent **No detach**
  warning because SSH disconnect may terminate it.

Diri never installs a service, persistent user unit, or LaunchAgent; never calls
`sudo`; never changes PAM, `sshd`, or linger configuration; and never falls back
to `tmux`.

## Terminal state and reconnection

A bounded raw output log is not sufficient to reconstruct arbitrary full-screen
terminal applications. The Holder therefore maintains authoritative terminal
state continuously:

```text
PTY bytes -> shared terminal parser -> Grid + Cursor + Modes
```

On attach, the Holder sends a `FullSnapshot`, followed by sequenced incremental
updates. A snapshot contains only the visible grid, cursor, modes, dimensions,
and sequence. Mouse state preserves the selected tracking regime (off, DECSET
1000, 1002, or 1003) independently from legacy/SGR coordinate encoding. The
wire mode byte retains its historical any-mouse bit and uses previously unused
bits for those details. A live protocol 1.3 Holder exposes only the historical
bit, so the Engine preserves those details as unknown: it does not synthesize
button or motion reports, while wheel intent remains encoded by that Holder's
authoritative parser. Scrollback is bounded to 4 MiB and served on demand
through `Scroll`. Raw output is bounded to 32 MiB.

On-demand history reads use the Engine's bounded background-request pool. The
Engine pins a read handle to the original Session and releases the Registry
before sending or waiting for the remote request. A slow history reply must
not hold up the control connection, terminal input, or grid publication for
any session. Removing or replacing a Session does not retarget an in-flight
read to the replacement. This is an Engine scheduling rule; live Helpers need
no protocol or binary update.

The PTY reader must never block on a client. The Holder uses bounded queues. It
coalesces background output for no more than 8 ms, while up to two grid
publications after interactive input bypass that wait (one trailing publication
may already be in flight before the actual response). If a destructive repaint
temporarily removes screen content, the Holder instead gives its redraw bytes up
to 16 ms to arrive, returning immediately when they do. That longer ceiling is
isolated from typed echo and additive scrolling. When no client is attached, it
continues parsing terminal state but does not construct or serialize diffs. If
an attached client falls behind, stale updates are discarded and the connection
is reseeded from a complete snapshot after reconnect.

The Engine reconciles raw-output offsets before feeding any local observer:
duplicates are skipped, overlaps feed only the unseen suffix, and a forward gap
on the live stream forces reconnect. Bounded replay may contain a gap because
the Holder follows it with an authoritative `FullSnapshot`; replay bytes are
logged for continuity but never treated as a second live status observation.

One owner/event loop handles PTY drain, terminal parsing, diff construction, and
attach writes. The hot path does not put an `Arc<Mutex<Terminal>>` across tasks.
Buffers are reused where practical, and idle Holders do not poll, heartbeat, or
run GC.

### Remote responsiveness under contention

Remote lifecycle RPCs run in the existing bounded control-worker pool. The
Registry reserves a launch identity, releases its lock for SSH and Session
construction, and installs the completed Session under the lock. Stop pins the
original Session, performs SSH and pump cleanup outside the Registry, and
removes only that same owner. Existing per-session operation guards serialize
resume, fork, archive, removal, migration, account continuation, and startup
restore. Restore rechecks the current record under that reservation. Unrelated
input, screen reads, and Hello requests remain available during these operations.

Interactive SSH stdin is nonblocking. The existing remote pump polls pending
writes alongside stdout, using a wake socket when an interactive caller queues
work. Each flush writes at most 64 KiB. The queue bounds encoded frames to 1 MiB
plus 4 KiB of framing allowance, with a bounded copy of wholly unwritten input
for reconnect. Queue overflow rejects the new operation before accepting bytes;
the binary attach closes on input errors rather than silently swallowing them.
Pending frames retain their exact written prefix. Input received during the
Hello handshake stays behind the reconnect batch until control is granted.
Wholly unwritten input may
be replayed after reconnect; a partially written effect is never replayed and
an uncertain asynchronous delivery fails the session transport explicitly.
Wheel intent is ephemeral; resize retains the latest pending size. This needs
no new writer thread or wire version and supports existing Helper builds.

A Holder drains at most 64 KiB per owner-loop turn and yields when two
milliseconds have elapsed between reads. Input, attach writes, and due grid
publication are serviced between turns even if PTY output stays readable.
On Agent exit, the Holder reaps its guarded process group but retains the PTY
reader until its remaining output is consumed. The final output and grid
precede the exit event.

Routine offset, dimensions, and controller-epoch checkpoints now use a shared
bounded persistence primitive in `diri-pty`. Each remote Engine client and
Holder has one sleeping worker with a 128 KiB stack, one immutable snapshot in
flight, and at most one latest snapshot pending. No PTY, parser, grid, or
connection moves to that worker. It does not hold the submission lock during
filesystem operations and has no idle timer or periodic wakeup. Holder write
failures wake the owner through a pollable socket and fail closed. Engine
binding write failures retain the existing best-effort recovery-offset policy.

Initial Holder identity is durable before launch succeeds. Routine on-disk
inspection metadata may lag the authoritative in-memory state while disk I/O
is in flight. Final exit waits for the ordered checkpoint fence; Engine close
fences its binding writer before removal or replacement. A stale client cannot
submit after that fence, and binding updates validate the incarnation. The
metadata worker shares the existing launch lock with launch, kill, and GC.
Management kill acquires that lock before signaling the Holder, so it cannot
interrupt a checkpoint and leave a nonce file behind. It publishes final state
only after the old Holder has released its ownership lock, then revalidates
the incarnation under that lock. An
old Running checkpoint cannot overwrite the final management exit.

These scheduling changes add no protocol or on-disk schema. Durable output-log
writes and rotation retain their existing policy. Engine improvements apply
to surviving remote sessions after an Engine update; Holder scheduling changes
apply to newly launched Helper processes. Live Helpers retain their original
Build IDs and are never overwritten or restarted to apply an optimization.
See the 2026-09-15 regression measurements in [PERF.md](PERF.md).

The shared terminal core recomputes its 4 MiB history-cell allowance when the
column count changes, including when the primary screen is inactive. Narrowing
increases the row allowance before reflow; widening trims after reflow so rows
that merge are not prematurely discarded. The allowance applies to retained
history cells, not total process memory, allocator capacity, or visible grids.

VTE 0.15.0 is pinned under `vendor/vte` with a single allocation change: its
synchronized-update buffer grows on first use instead of reserving 2 MiB for
every terminal at construction. It retains capacity for subsequent frames.
The byte limit, timeout, parsing, and synchronization semantics are unchanged;
the tradeoff is allocation during the first synchronized frame. The vendored
source and workspace dependency configuration participate in the default
Helper Build ID. This adds no parser implementation or runtime dependency.
See `vendor/vte/DIRI-PATCH.md` and the 2026-09-06 measurements in `PERF.md`.

### Local Holder input compatibility

The durable local Holder is outside the remote Helper wire protocol, but it
shares the survival invariant: an application upgrade must not abandon a live
Holder and Agent. Local input therefore starts with an additive `streamVersion`
negotiation over the legacy JSON request interface. A new Holder accepts version
1 and keeps the Unix connection open for bounded binary input and resize frames,
acknowledging each operation. An old live Holder rejects the unknown operation
in its normal way; the new client then pins that Holder to legacy JSON/base64
requests. Control and lifecycle operations remain independent request/response
connections. A stream error after a frame may have reached the PTY is reported
without retry, so a keystroke can never be duplicated. On Apple platforms the
dedicated input thread uses interactive QoS so persistence does not trade a
faster socket acknowledgement for slower end-to-grid delivery. The local
daemon's held-output follower is raised only while the session is recently
attached or receiving input, then returns to default QoS.

Local output-stream negotiation also respects surviving Holder versions. A
completed rejection is remembered for the attached session lifetime, and the
Engine keeps following its durable log without probing on every wakeup.
Transport failures and interrupted supported streams remain retryable. This
changes no local or remote wire format and never replaces a live Holder.

The local Holder's `OutputLog` writer retains no raw-output ring: no Holder
operation reads it. Durable file output, offsets, rotation and exit markers
remain unchanged; the separate bounded live-output queue still serves attached
Engines. The Engine and remote Helper retain their existing history/output
budgets. Existing Holder processes keep their original allocations until their
sessions end naturally.

## Controller lease

The completed baseline permits exactly one live attach/controller. A new attach
atomically increments the controller epoch and revokes the previous attach.

Only the current epoch may send:

- `Input`;
- `Resize`;
- `Signal`;
- `Scroll`;
- session termination requests.

Input and signals are at-most-once effects. If a write fails before accepting
any frame byte, input may be retained for a later controller lease. A partial
write or lost flush acknowledgement has an unknown outcome: the Engine surfaces
the transport error and never queues or replays that effect.

Stale epochs fail with a structured protocol error. Multiple read-only observers
are a future enhancement and are not part of the completed baseline.

## Terminal interaction metadata (September 2026)

Terminal quality-of-life interactions remain desktop-owned: link discovery and
activation, selection, copying, menus, paste review, export, and keyboard modes
run in `diri-app` / `diri-term`. They do not execute SSH or change controller
ownership. The existing local Engine RPC serves retained terminal rows.

The shared terminal parser additionally retains OSC 8 targets, soft-wrap facts,
wide-cell continuation facts, combining characters, and OSC 133 A prompt-start
marks. These are terminal screen facts; the Holder does not infer commands,
execute shell hooks, collect exit-code histories, or orchestrate workflows.
Prompt marks are ignored on the alternate screen. Shell prompt navigation is a
client interpretation of retained marks, not Agent status or hook ingestion.
Shells must emit OSC 133; no remote shell configuration is installed.

Protocol 1.6 advertises optional `terminal-annotations-v1`. Grid flag bit 2 adds
an extension version byte (1), a big-endian u32 byte length, and bounded JSON
row annotations after the unchanged RLE rows. Unknown versions, invalid spans,
control characters in destinations, and oversized metadata fail decoding.
The extension is capped at 256 KiB, targets at 2048 bytes. Exporters budget
annotations per response; targets exceeding the available annotation budget
remain ordinary text. Semantic wrap/prompt/wide bits are additive style bits.
Scrollback carries optional row-aligned metadata under the same negotiated
version. A pre-1.6 controller receives the original grid form; a new Engine
attaching to an older live Holder sees missing annotations as unavailable,
never as fabricated link or prompt facts. Required transport capabilities and
all existing fail-closed bootstrap checks are unchanged.

Full snapshots still contain only the visible grid and its annotations, cursor,
modes, dimensions and sequence. History remains on demand. GridMirror, deltas,
coalescing and slow-client full reseeds preserve annotation changes even when
visible labels do not change. Checkpoint version 4 persists visible and history
annotations; versions 2/3 remain readable with unavailable optional metadata.
Oversized checkpoint annotations cause a cache-write failure, retaining the
existing raw-log recovery path rather than persisting partial link state.

VTE's OSC dispatch calls one new `mark_prompt` Handler method. The vendored
alacritty_terminal 0.26.0 adds one cell flag and marks the cursor cell in that
handler; its existing erase, scroll, resize and synchronized-update processing
own marker lifetime. This small parser extension avoids a second parser or
raw-output cursor guesses. Both vendored sources participate in the Helper
Build ID. There is no new runtime dependency.

The desktop's existing reading cache is bounded by 4 MiB of cells instead of
512 rows, so selection can span the Engine's retained history. URI metadata is
bounded on transport and pruned alongside cached rows. Only active edge drags
run an autoscroll timer; hover keys use cell/content/viewport revisions without
cloning the reading cache. No idle Holder timer or new Holder task is added.

Acceptance adds metadata codec rejection and compatibility tests, annotation-
only deltas, synchronized prompt marks, erase/scroll/resize and checkpoint
round trips, selection and paste tests, and desktop interaction verification.
The existing release performance, persistence, lease, and real-SSH gates remain
mandatory.

## Engine-owned orchestration message delivery (September 2026)

Initial prompts are pasted and submitted at most once. Screen echo, composer
changes, and fresh Agent signals may confirm acceptance; their absence cannot
prove that input was discarded. The Engine never clears/retypes a prompt or
sends another Enter because a confirmation timed out. An uncertain initial
delivery identifies the existing session and instructs the caller to inspect
it rather than resend or spawn a replacement. This deliberately replaces the
old screen-based retry policy, including retries intended to recover swallowed
startup input. Startup readiness still gates the first attempt; PTY input alone
cannot guarantee exactly-once application consumption.

MCP `send_prompt` and `report_to_parent` use the additive local Engine method
`session.deliver_message`. It reserves a receipt durably before writing input.
Sender session, target session, and message ID define one logical message.
The MCP bridge derives a stable ID from normalized content when omitted; an
explicit `message_id` allows an intentional repeat. Returned IDs can be passed
back unchanged. Repeating the same identity returns its receipt; changed text
or submit mode with that identity fails with `message_id_conflict`. Attribution
uses stable session IDs so renaming a sender cannot change a retried payload.

Receipts survive MCP and Engine restarts in owner-only
`message-receipts-v1.sqlite` beside the Engine socket. The versioned table stores
only identity/content hashes and `sent`/`unknown` outcomes, never prompt bodies.
A reservation interrupted by a crash, partial input, or lost acknowledgement
remains unknown and is never replayed. `sent` confirms input transport, not Agent
execution or completion. Receipts do not expire; at 100,000 entries new messages
fail closed instead of forgetting old identities. Existing identities remain
queryable. Missing Engine support, corrupt storage, and unsafe paths fail
closed; the MCP bridge never falls back to untracked text input.

This is local Engine orchestration over the existing local and remote input
paths, not remote MCP forwarding. Raw interactive `session.send_text`, Helper
protocols, controller leases, and Holder ownership are unchanged. SQLite and
SHA-256 reuse workspace dependencies; the MCP bridge adds the existing `sha2`
dependency for stable content identity. There is no Holder cache, timer, or lock.
The receipt work uses the existing Engine input serialization. A 100-message
debug measurement on macOS recorded median 5.0 ms / p95 9.7 ms receipt overhead
and a 36 KiB database; the existing 30 ms paste/Enter settle is separate.

Acceptance covers accepted input with no visible echo, delayed composer
repaint without extra Enter, repeated and concurrent MCP calls, intentional
repeats, conflicting identities, durable reopen/crash reservations, and
corrupt/symlinked storage. Existing remote at-most-once transport gates remain.

### MCP request lifecycle and waits

The Rust MCP bridge verifies `Hello.proto` and the explicit Rust `engineKind`
on every Engine connection before issuing a tool request. Missing, legacy, and
unknown identities fail closed. This verification shares the operation's deadline
and cancellation scope. Tool arguments are validated against the advertised
schema before discovery, authorization, or effects; invalid optional values do
not silently choose defaults such as submitting text or selecting a local host.

Each stdio MCP process admits at most eight concurrent read calls and an ordered
mutation worker with eight queued mutations. Excess calls fail before dispatch.
The input loop remains available for protocol startup, ping, and cancellation.
Cancellation closes a read call's Engine sockets or suppresses a queued mutation
before it starts. Once a mutation starts, cancellation cannot promise to undo its
effects and does not interrupt it. EOF cancels reads and drains accepted mutations.
These are MCP cold-path resources; they add no Holder task, parser, or lock.

Wait tools use a bounded event subscription followed by an authoritative snapshot,
then re-read on updates/removals/drop markers. This closes the snapshot/subscribe
race. A removed child is reported separately and does not settle another working
child. Single-session waits stop on exit/removal even when the requested status
cannot be reached. Waits observe current status, not a particular message's
completion. Requests use absolute deadlines across partial frames and event
traffic, and cancellation releases the local subscription connection.

Acceptance adds real MCP subprocess tests for ping during blocked calls,
cancellation, mutation ordering, overload, initialization, version negotiation,
and oversized-frame recovery; fixture Engine tests cover wait races, validation,
identity rejection, and deadline enforcement. A 50-ping debug sample while a tool
was blocked measured approximately 37 microseconds median / 77 microseconds p95.
See [the MCP reliability audit](docs/mcp-reliability-audit.md) for scope and limits.

### Durable spawn and explicit task completion

The local Engine now owns additive tracked-spawn and task receipts, as specified
in [MCP task reliability](docs/mcp-task-reliability.md). It reserves a session ID
before a tracked spawn has effects; retries return that identity and never
repeat a launch whose outcome is uncertain. Worktree/bootstrap interruption is
reported as failed/unknown and requires inspection, not automatic recreation.

Task assignment, delivery, explicit Agent acknowledgement, and explicit task
results are distinct. Only the assigned Agent can report that task's outcome;
terminal status never substitutes for a task result. These journals, MCP tools,
and task events belong to the local Engine. This is a separate orchestration
enhancement, not a Holder task queue, remote hook service, or MCP forwarding.
Existing Helper protocol/artifacts and controller ownership remain unchanged.

The continuous remote gate adds dropped-reply retries, cancelled waits, and
Engine-instance/SSH teardown and adoption through a disposable real OpenSSH
endpoint. Deterministic Agent fixtures exercise exact task IDs and preserve the
same remote process/incarnation across restart; no native provider completion
is inferred. Native provider behavior and physical WAN outages remain distinct
manual acceptance checks.

## Wire protocol

`diri-proto::remote_pty` is the versioned protocol authority. Protocol 1.3
declares terminal, session management, environment capture, directory listing,
batched executable discovery, persistence probing, and atomic activation as
required capabilities. Protocol 1.4 additively preserves granular mouse
tracking/encoding bits and the raw mouse-input frame while retaining the old
any-mouse compatibility bit. Protocol 1.5 additively reports the PTY foreground
process group (`HelloAck.foregroundPid` and `ForegroundProcess`) so the Engine
can distinguish an idle shell from a foreground job. Plain-shell sidebar
indicators remain hidden; foreground Agent indicators follow normal attention
rules. Older Helpers omit the field; older Engines ignore the extra JSON and
never see the new frame. Foreground probes are armed only for an attached
compatible controller and are cleared when it disconnects, so a detached
Holder sleeps between PTY, lifecycle, and attach events.

The protocol includes:

```text
Hello
HelloAck
Launch
Attach
FullSnapshot
Grid
Scroll
Modes
Input
Mouse
Resize
Ping
Pong
ProcessExit
ForegroundProcess
Signal
AcquireControl
ControlGranted
ControlRevoked
ReleaseControl
Error
```

All frames have hard size limits. Authentication tokens are redacted from Debug
output and cleared on drop. Encoders and decoders validate terminal dimensions,
total cell counts, cursor positions, row indices, and exact row widths before
allocation or state mutation.

The receiver rejects incompatible protocol majors, missing required
capabilities, incorrect Build IDs, wrong session incarnations, oversized frames,
and stale controller epochs. Unknown optional fields may be ignored; unknown
required capabilities fail closed. Protocol stdin is never reinterpreted as raw
terminal input after an error.

## Security and durability

The implementation must preserve these invariants:

- verify artifact length, SHA-256, Build ID, protocol, and required capabilities
  before activation;
- never overwrite a live Helper version;
- never follow an untrusted cache/state symlink;
- never log credentials, authentication responses, complete environments,
  Agent prompts, or unredacted protocol payloads;
- keep Helper frames separate from OpenSSH authentication UI;
- bind session attachment to owner-only state and authenticated bearer material;
- clean up only resources created by the failing operation;
- execute Agent arguments structurally, not through shell interpolation;
- request no elevation or host-wide configuration.

## Performance requirements

Correctness precedes performance. Among designs that preserve the invariants,
Diri prefers lower latency, CPU usage, memory use, copies, wakeups, and idle
work. A supervisor, fan-out mechanism, lock, cache, or background poller requires
measurement evidence.

Release builds must satisfy the local Helper/UDS gates:

```text
FullSnapshot p90          <= 100 ms
input-to-PTY p95          <= 10 ms
output-to-diff p90        <= 8 ms
loopback interaction p50 <= 75 ms
loopback interaction p90 <= 150 ms
```

The 2026-08-11 local release sample measured:

```text
FullSnapshot p90          0 us
input-to-PTY p95          138 us
output-to-diff p90        13 us
loopback interaction p50 76 us
loopback interaction p90 99 us
```

Measured values are printed in CI so regressions are visible rather than hidden
behind pass/fail status.

The Engine integration regression also pauses a disposable remote Holder for
800 ms during a history read. Local Hello, input forwarding (including the
desktop binary attach channel), and screen reads must complete within 400 ms,
before the history response arrives. The test verifies the input reaches the
same remote Agent after the pause. This checks Engine contention separately
from the Helper/UDS gates and does not claim to measure real WAN latency.

## Desktop integration

While the desktop terminal is scrolled back, its renderer retains one local
screen snapshot and preserves already fetched rows in its bounded 512-row
history cache. This keeps Agent redraws and overlapping history replies from
replacing text under the reader. The live grid continues receiving every
update. Returning to live or entering the alternate screen releases the reading
view; the next scroll fetches fresh history. This is client presentation state,
not another terminal parser, Holder snapshot, or protocol change.

The desktop app initializes a newly added SSH host immediately and displays the
bootstrap state. The Engine returns only sanitized facts such as Build ID,
protocol version, cwd, shell, and persistence level. It does not expose the full
remote environment or authentication data to the UI.

The host editor supports environment reinstallation. Progress indicators use a
shared reduced-motion-aware component. Successful completion is transient;
failure remains visible with a retry action.

Working-tree inspection follows the session's execution location. Local paths
are inspected locally. Remote paths use a fixed no-PTY SSH script with cwd and
comparison values sent separately. Response markers isolate login-shell noise.
A non-Git directory or a host without Git is a compatible unavailable state,
not a recurring UI error.

The Engine's `Hello` includes an explicit Rust daemon identity and executable
hash. The app and client reject missing, old, or unknown daemon identities. A
confirmed Rust Engine whose hash differs from the bundled executable is upgraded
without abandoning live Holder/Agent state, ensuring subsequent remote actions
use the current Helper catalog.

An inherited `DIRIJOR_SOCKET` equal to the app's ordinary socket does not bypass
this startup verification: Agents launched by Diri inherit that path, and an
app started from their environment must still refresh an outdated Engine.
Only a different, explicitly supplied socket skips app-owned supervision.

## Tailscale, iPhone Companion, and `diri-node`

These features are separate from Remote Holder transport:

- Tailscale may provide network reachability, but Diri does not configure it and
  remote sessions do not depend on it;
- `diri-node` is an optional, explicitly configured enhanced mode and is not a
  bootstrap dependency;
- the old iPhone companion path is not part of the Rust remote architecture and
  its obsolete UI entry points are removed;
- the phone gateway below is a separate client feature, not a new Remote
  Holder capability or a dependency of SSH sessions.

### Phone gateway and workspace creation (September 2026)

The Mac app embeds `diri-web` as an opt-in Settings → Phone access service.
The SwiftUI iPhone client uses authenticated HTTP JSON and SSE through this
gateway. The local Rust Engine remains authoritative for sessions, host
catalog, installed-agent discovery, folder browsing, worktrees and input.
Phone access never attaches directly to a Holder, so it does not introduce a
second controller lease or read-only observer protocol.

Slow spawn/bootstrap/folder/diff RPCs and remote agent scans run outside the
control connection's read loop, with at most 32 background requests per Engine.
Excess requests fail with `busy` before dispatch. This preserves Hello, screen
reads and input while a first prompt or SSH operation is pending; it adds no
Holder threads, supervisor or terminal hot-path fan-out. A socket regression
test requires Hello to overtake a deliberately slow Git worktree spawn.

The app binds only the connected Tailscale IPv4 address reported by the local
Tailscale client, never a wildcard, LAN, or public interface. Tailscale provides
encrypted transport; Diri does not install, configure or alter it. An additional
256-bit bearer token is minted in memory for each enable. Its QR is generated
locally (`qrcode`, with default features disabled); no pairing secret goes to
an image service, log, or preferences file. The iPhone stores its credential
in Keychain and refuses HTTP redirects. Anyone with this credential and
tailnet reachability can control all sessions exposed by that Engine.

Turning access off aborts the listener and all accepted HTTP/SSE connections.
Re-enabling rotates the credential. The gateway lives only as long as the Mac
app; disabling it does not kill sessions. On macOS an app-lifetime `caffeinate`
child prevents idle sleep while enabled, but cannot promise connectivity after
closing the lid, explicit sleep, loss of power or a network outage. No service
or login item is installed by phone setup. Phone distribution/signing and
Tailscale enrollment remain external setup requirements. Push notifications,
shared users, unattended gateway startup, and rich terminal rendering are
not included in this feature.

Setup guides users through Mac readiness, iPhone Tailscale sign-in and QR
pairing. The Mac performs a read-only, bounded status check and distinguishes
missing installation, sign-in, administrator approval, disconnection and an
eligible private IPv4 address. Install/open links hand off to Tailscale; Diri
does not approve permissions, change routes, enroll devices or handle account
credentials. The iPhone verifies authenticated gateway access after scanning;
its checklist alone never claims verified connectivity. Release preparation,
owner signing requirements and physical-device gates are in
`../ios/TESTFLIGHT.md`; unsigned archives do not satisfy distribution gates.

`host.list` is a read-only Engine catalog projection (id, name, defaultCwd),
excluding SSH/node credentials. `/api/agents?host=…` and
`/api/directories?host=…&path=…` use existing Engine discovery/browse operations.
`session.spawn.worktreeBase` is additive: absent retains HEAD behavior; the
phone explicitly selects `main` for a separate workspace. Git resolves and
pins that ref on the selected host; missing refs fail, without silently using
HEAD. No fetch, pull, checkout/reset of the original tree, or remote repository
clone is performed. Branch names and framed fields are validated; Git receives
argv values, not caller-generated shell code.

Remote worktree creation is Engine-owned workspace policy using the existing
bounded `RemoteManager::run_fixed_script` SSH seam, independent of Helper
versions. The fixed script receives validated cwd/branch/base/slug fields over
stdin and emits a bounded marker-framed canonical path. The resulting session
records its remote host, worktree path and branch, and launches via the existing
verified Helper. No workspace orchestration is added to `diri-remote`. A failed
or interrupted launch may leave the new worktree for recovery; do not delete
user data or automatically retry an ambiguous mutation.

Acceptance: authenticated catalog/browse/spawn contract tests, main-versus-HEAD
worktree tests (local and remote script), hostile field rejection, gateway
revocation tests and iOS build/tests. Real-device camera pairing and a cellular
round trip through Tailscale require a signed device build and an enrolled
phone; they must be checked before claiming a distributable phone release.

## Verification and release gates

Deterministic tests use fake SSH executables, fixture homes, Unix sockets, and
spawned test PTYs rather than a developer's personal remote host. Regression
coverage includes:

- noisy shell startup and environment capture timeout/failure;
- supported and unsupported platform parsing;
- concurrent and interrupted bootstrap;
- upload, launch, and attach channel interruption;
- corrupt artifacts and build/protocol/capability mismatch;
- cache symlink rejection and owner-only permissions;
- detach/reconnect with unchanged process identity and incarnation;
- terminal snapshot restoration and continued input;
- slow-attach recovery from stale diffs;
- controller lease revocation and stale writes;
- normal exit, signal exit, and Holder failure;
- directory pagination/bounds and canonical navigation;
- all three persistence outcomes;
- complete `list`, `inspect`, `kill`, and `gc` lifecycle behavior.

CI builds and executes the exact Helper artifact natively on Linux x86_64,
Linux aarch64, and macOS arm64. It also runs a disposable, ordinary-user
OpenSSH detach/reconnect soak. These are mandatory release gates and do not use
Rosetta or a developer's real SSH host.

The acceptance suite validates release-mode UDS performance, a 23 MiB slow-
attach recovery case, transient user-supervisor behavior, and an actual Holder
PTY's `isatty`, canonical editing, resize, and `SIGWINCH` behavior. The real SSH
soak verifies bootstrap, login-shell handling, persistence probing, Bridge
disconnection, explicit ControlMaster teardown, same-PID/same-incarnation
reconnection, snapshot restoration, continued input, and cleanup. A separate
PAM/logind endpoint with logout process cleanup enabled verifies that such a
host is classified as non-persistent rather than receiving a false detach
guarantee.

An optional manual soak uses:

```bash
DIRI_REMOTE_SSH_TARGET=user@disposable-host \
DIRI_REMOTE_SOAK_SECONDS=180 \
scripts/remote-ssh-soak.sh
```

Optional variables are `DIRI_REMOTE_HELPER_PATH`,
`DIRI_REMOTE_SSH_EXECUTABLE`, and `DIRI_REMOTE_CWD`. The test prints its unique
session ID and performs authenticated cleanup on success, assertion failure, and
panic unwind.

## Completion record

The completed refactor includes all of the following:

- removal of the Rust SSH PTY + `tmux` transport and every fallback;
- a Rust-owned, versioned Helper catalog for all supported targets;
- authenticated, bounded, capability-negotiated `remote_pty` frames;
- shared PTY and terminal-state crates used by Engine and Helper;
- one independent Holder, UDS, PTY, and process guard per session;
- nonblocking PTY drain, bounded output/scrollback, snapshot recovery, and
  controller-epoch revocation;
- remote spawn, reconnect, daemon-restart adoption, Agent resume, exit
  attribution, and authenticated attachment in the Engine;
- host initialization, exact-version environment reinstall, and automatic
  Helper synchronization after updates;
- account/cwd environment capture and structured `argv`/`cwd`/environment
  execution;
- bounded remote directory selection and host-aware project identity;
- target-aware local/remote Agent discovery, manual executable binding, and
  shared availability-filtered quick-create surfaces;
- location-aware working-tree inspection with non-Git compatibility;
- explicit persistence probing with no privilege escalation;
- native artifacts and release gates for Linux x86_64, Linux aarch64, and
  macOS arm64;
- performance, slow-client, lifecycle, bootstrap, protocol, security, and real
  OpenSSH soak coverage.

The completed acceptance scenario is:

```text
1. Start an interactive Agent on a supported remote host.
2. Disconnect the network for several minutes.
3. Reconnect to the same session incarnation.
4. Restore the authoritative terminal screen.
5. Continue interacting with the same Agent process.
```

## Conversation title authority

The local Engine separates terminal presentation titles from native conversation
names. `SessionRecord.titleSource` additively assigns value `5` to `TerminalTitle`;
existing values retain their meaning and older readers decode new values as
unknown. A terminal title is provisional and may follow later OSC updates.
Confirmed native names take precedence; manual and Diri-assigned names remain
authoritative. A stored first-prompt preview cannot replace a real name.

Live prompt capture only fills an unnamed record (placeholder or unknown).
After adoption or resume, the first input observed by a new Engine Session can
be a follow-up, so it must not replace an established first-prompt title. An
identity-bound provider read may repair that fallback to the conversation's
actual first prompt. Subsequent live folds preserve the repaired value across
list/inspect responses, update events and persistence.

Codex activity, pending-name labels and unnamed placeholders are excluded from
conversation names, and a matching cwd suffix and activity spinner are removed.
The Engine repairs previously persisted transient Agent titles on load. Local
native names still come from the exact profile and thread identity; remote
sessions use the existing terminal output and captured-prompt path. No remote
store reads, thread-ID discovery, Helper behavior or protocol changes are added.

## Terminal notification ingestion

The local Engine optionally extracts bounded OSC 9, OSC 777 and textual OSC 99
notifications from the existing live raw-output stream. It emits a local
`session.notification` event; the app owns notification history, read state,
macOS delivery and navigation. Notifications do not change execution status.
The Holder does not enable notification extraction, store notification objects,
run hooks, or interpret actions. No Helper protocol or capability changes are
required. Replayed output must not redeliver notifications. Alerts produced
while the Engine is disconnected are not recovered from replay; reliable
offline notification delivery remains an independent enhancement.

The local Rust Engine persists causal attention identities and native-source
receipts in a per-session SQLite journal. Adoption retains the journal namespace;
a newly launched process creates a new namespace. The app consumes the additive
`SessionRecord.attentionState` snapshot and owns its own durable inbox/interruption
receipts. No journal, hook adapter or notification policy runs in the Holder, and
no Helper protocol or required capability changes. The corresponding release
gates include redraw/replay deduplication, restart/adoption identity, cancellation
and receipt survival after history pruning; see `docs/notification-architecture-review.md`.

## Deferred enhancements

The following are independent product enhancements, not unfinished remote
refactor work:

- remote Claude hooks and Codex notifications;
- offline structured Agent-event buffering;
- remote conversation/thread identifiers;
- MCP forwarding;
- artifact, port, usage, and resource discovery;
- cross-host handoff and checkpoint migration;
- cross-host or post-reboot process recovery;
- multiple read-only observers;
- deeper, explicitly configured `diri-node` integration.

Adding any of these requires an explicit proposal update. They must not enlarge
the Helper into a second Engine or weaken the current security and lifecycle
boundaries.

## Non-goals

The current architecture does not attempt to:

- replace SSH as the transport and authentication layer;
- build a complete remote Diri daemon;
- preserve Swift compatibility;
- keep a process alive across a remote machine reboot by default;
- implement a general-purpose terminal multiplexer;
- require `diri-node`;
- move the local state Engine to the remote host;
- install packages, services, or privileged host configuration;
- support Intel macOS or Rosetta for Remote Helper execution.

## Core principle

> The remote host keeps only state that cannot remain local: the PTY, Agent
> process, and current terminal screen. Session orchestration and product logic
> remain in the local Rust Engine.
