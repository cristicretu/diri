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
- gives Diri direct ownership of the remote Agent PTY lifecycle;
- requires no remote `tmux`, `screen`, `zellij`, Node.js, Python, `socat`, `nc`,
  `curl`, `wget`, or preinstalled Diri service;
- never requests `sudo` or host-wide configuration;
- fails closed with a structured error when no valid Helper artifact or
  capability-compatible Helper is available;
- retains orchestration and user-facing state in the local Rust Engine.

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
- `diri-pty`: shared low-level PTY primitives;
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

## Wire protocol

`diri-proto::remote_pty` is the versioned protocol authority. Protocol 1.3
declares terminal, session management, environment capture, directory listing,
batched executable discovery, persistence probing, and atomic activation as
required capabilities. Protocol 1.4 additively preserves granular mouse
tracking/encoding bits and the raw mouse-input frame while retaining the old
any-mouse compatibility bit.

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

## Desktop integration

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

## Tailscale, iPhone Companion, and `diri-node`

These features are separate from Remote Holder transport:

- Tailscale may provide network reachability, but Diri does not configure it and
  remote sessions do not depend on it;
- `diri-node` is an optional, explicitly configured enhanced mode and is not a
  bootstrap dependency;
- the old iPhone companion path is not part of the Rust remote architecture and
  its obsolete UI entry points are removed;
- any future companion implementation requires a separately designed protocol,
  security model, lifecycle, and product scope.

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

## Deferred enhancements

The following are independent product enhancements, not unfinished remote
refactor work:

- remote Claude hooks and Codex notifications;
- offline structured Agent-event buffering;
- remote conversation/thread identifiers;
- MCP forwarding;
- artifact, port, usage, and resource discovery;
- handoff and checkpoint migration;
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
