# Telemetry

Diri records what its processes do so a bug report ("a teammate's tab dropped to
zsh last night") can be investigated from a timeline instead of a
reproduction. This file is the contract between the recorder
(`crates/diri-telemetry`), the instrumentation in each process, the ingest
Worker (`telemetry/worker`) and the investigation CLI (`telemetry/cli`).

## Principles

- **Local first.** Every process records to a spool on disk. Uploading is a
  separate step the Engine performs, and the user can turn it off in
  Settings > General > Privacy. Recording itself costs a channel send per event.
- **No content.** Fields are numbers, booleans, `&'static str`
  literals, `Id`s (`[A-Za-z0-9_.:-]{1,96}`) or scrubbed diagnostic symbols/OS crash facts in `Text`. Never record terminal output, prompts, clipboard or pasted contents, file
  contents, environment variables, command lines, or URLs. Paths are recorded
  only as `path_hash`. Arbitrary error messages, subprocess stderr and panic
  payloads are excluded, not passed through a best-effort scrubber. Conversation UUIDs, session ids, agent ids, error codes
  and RPC method names are allowed: they identify, they don't reveal.
- **Never on the hot path.** No event per PTY read, per byte, or per
  terminal cell. Hot paths accumulate locally and report totals through
  `count`/`observe_ms`, which are drained into one `metrics` event per
  minute. Per frame, per RPC, per keystroke and per paste are fine.
- **Holders stay quiet when idle.** A Holder records facts as they happen
  (spawn, exit, attach, overflow) and never runs the health sampler.
- **Tests and dev builds never upload.** Debug builds upload only when
  `DIRI_TELEMETRY_ENDPOINT` is set at run time. `DIRI_TELEMETRY=off`
  disables recording entirely.

## Files

Under the platform state dir (`~/Library/Application Support/Dirijor`):

| Path | Owner | Content |
|---|---|---|
| `telemetry/install.json` | recorder | random install UUID, created time |
| `telemetry/config.json` | app Settings | `{ "upload": bool, "name": string\|null }` |
| `telemetry/spool/<proc>-<pid>-<start_ms>-<n>.open` | each process | JSONL being written |
| `telemetry/spool/*.jsonl` | each process | rolled at 2 MiB |
| `telemetry/spool/offsets.json` | uploader | bytes acknowledged per file |
| `telemetry/spool/urgent` | recorder | touched on an incident |
| `telemetry/activation/origin.json` | app or Engine, first to start | `{created_ms, preexisting}`: the activation baseline |
| `telemetry/activation/<step>.json` | whichever process reached it | `{t}`: the milestone was recorded (see Activation) |

The spool is capped at 64 MiB; oldest sealed files go first.

The **Support ID** (`D-XXXXXXXX`, Crockford base32 of the first 40 bits of
the install UUID) is shown in Settings > General > Privacy. The **name**
defaults to the macOS login name; the user can edit or clear it.

## Record format

One JSON object per line:

```json
{"t":1790581979447,"seq":42,"p":"engine","pid":812,"k":"session.spawn","s":"info","f":{"session":"s_26bf32debd4c","agent":"claude-code","mode":"resume","conv":"0a40e747-fa0c-4e9a-b755-c195ab079cda"}}
```

- `t` wall-clock ms, `seq` per-process sequence, `p` `app|engine|holder`.
- `k` the event kind, dotted, lower snake case: `<area>.<what>`.
- `s` severity: `debug|info|warn|error|incident`. `incident` means
  user-visible breakage; it triggers an upload within a minute and is
  indexed server-side.
- `f` fields. Well-known keys other tools rely on:
  `session` (session id), `conv` (agent conversation UUID), `agent` (agent
  id), `host` (remote host id), `window` (window id), `code` (error code),
  `ms` (duration), `method` (RPC method).

## Events emitted by the recorder itself

| kind | sev | fields |
|---|---|---|
| `process.start` | info | `recorder_version, os, arch, debug_build` (the app/Engine version is on `app.launch` / `engine.start` and every batch header) |
| `health` | info | `uptime_s, rss_mb, footprint_mb, cpu_pct, cpu_ms, threads, fds, fd_limit` + registered gauges |
| `metrics` | info | `window_s, counters{name:n}, timings{name:{n,avg,p50,p90,p99,max}}` |
| `panic` | incident | `location, signature, frames[]` |
| `telemetry.dropped` | warn | `count` (channel was full) |
| `telemetry.upload_rejected` | warn | `status, lines` |

Each process adds its own catalog section below.

## Engine and Holder events

The Engine (`dirijord-rs`, `p: "engine"`) starts recording after it has
normalized its environment, runs the health sampler every 60 s, the uploader
(when `upload::endpoint()` names one), and the crash-report watcher. It
passes `--telemetry-state-dir <state dir>` to the Holder managers it
launches; a manager given that flag records as `p: "holder"` into the same
spool, and one launched without it (tests, manual `--spec` recovery) records
nothing. Holders record facts as they happen and run no sampler or timer.

`health` from the Engine carries these gauges: `sessions` (`records, live,
held, remote, hibernated, working, needs_input`), `clients` (open control
and data connections) and `attached` (terminal attachments, previews
excluded). `metrics` carries counters `rpc.calls, rpc.errors,
engine.connections, engine.accept_errors, attach.reseeds, attach.lag_reseeds, remote.delta_gaps,
ssh.commands, ssh.channels, hook.queued` (hook reports answered before a busy
Registry was free, applied in order by the hook applier) and timings `rpc,
rpc.hook_report` (the `hook.report` reply an Agent's synchronous hook waits
on), `hook.apply_wait` (queue-to-Registry wait of a queued hook report, i.e.
how stale its status was when it landed), `attach.seed, ssh.command`,
plus the Engine's share of a keystroke's echo (first input of a burst, local
sessions, ≤ 2 s): `input.echo.engine` (input written to the PTY or Holder →
the first output the child produced after it: the Holder hop and the agent's
own reaction time), the same per agent class as
`input.echo.engine.<class>` (`claude`, `codex`, `cursor`, `gemini`, `shell`,
`other`; never the raw agent id) and `input.echo.publish` (that output → the
first grid frame queued to attached clients after it: the Engine's batching
and coalescing).

`modes` fields are `{mouse: "off"|"1000"|"1002"|"1003"|"unknown", sgr,
alt_screen, bracketed_paste, app_cursor, keyboard, focus}` from the Engine's
own emulator (`keyboard`: kitty keyboard enhancements pushed; `focus`: DEC 1004
focus reporting). The
local Holder is a byte pipe with no parser, so terminal-mode facts are
recorded by the Engine, not the Holder.

### Engine process

| kind | sev | fields | catches |
|---|---|---|---|
| `engine.start` | info | `build, fd_soft, exit_when_orphaned` | which build ran; launchd fd limit not raised |
| `engine.login_path` | info | `ok, ms, shell, entries` | agents "not found" because the login PATH capture failed or timed out |
| `engine.duplicate_exit` | debug | | a relaunch racing the singleton lock |
| `engine.catalog` | info | `manifests, failed` | a short or unparsable Agent catalog |
| `engine.no_manifests` | incident | `failed` | the Engine refusing to start with no catalog |
| `engine.state_loaded` | info | `records, ms` | slow or empty state loads |
| `engine.state_quarantined` | incident | `io` | a corrupt state file (records moved aside) |
| `engine.state_unreadable` | incident | `io` | the Engine refusing to start over unreadable state |
| `engine.restore` | info | `adopted, records, live, ms` | sessions not coming back after an Engine restart |
| `engine.holders_lost` | warn | `count, stale, rebooted` | holders that died with the Engine or the machine; `stale` counts sockets left on disk that refused, i.e. killed holders (a reboot clears the usual `/tmp` holder directory); `rebooted` counts sessions untouched since the machine booted, which the computer restarting ended (their exit carries `systemRestart`) |
| `engine.remote_restore` | info | `adopted, ms` | remote sessions not re-adopted after restart |
| `engine.bind_failed` | incident | `io` | the control socket could not be bound |
| `engine.accept_failed` | incident (EMFILE/ENFILE), error | `io` | descriptor exhaustion: blank terminals until resize (at most one a minute) |
| `engine.exit` | info | `reason: shutdown\|idle` | deliberate Engine exits (vs. crashes: no `engine.exit` before the next `process.start`) |
| `crash.report` | incident | `process, app_version, incident_id, crashed_at, timestamp, exception, signal, subtype, termination, namespace, code, thread, thread_name, signature, frames[]` | native crashes (SIGSEGV/abort in GPUI/objc) of `diri`, `dirijord-rs`, `diri-holder`, `dirijor`, `dirijor-mcp`, `diri-ssh-askpass` from `~/Library/Logs/DiagnosticReports/*.ips`, scanned at start and every 10 min past a watermark in `telemetry/crash_watermark.json` (first scan looks back 7 days). Frames are `image!symbol+offset`; no paths, registers or application-specific messages |
| `holder.manager_died` | incident | `pid, code, signal` | the Holder manager exiting abnormally (every local session with it) |

### Control RPC and clients

| kind | sev | fields | catches |
|---|---|---|---|
| `rpc.slow` | warn | `method, ms, ok, session` | any request over 250 ms (except `events.wait`, `task.get`) |
| `rpc.error` | error | `method, code, ms, session` | every error reply, with its structured code (`remote_transport_unavailable`, `initial_prompt_delivery_failed`, ...) |
| `rpc.op` | info | `method, session, ms` | successful lifecycle requests: spawn, kill, remove, archive, resume, fork, migrate, reconnect, hibernate/wake, account switch/login, worktree create/remove, shutdown, send_text |
| `client.hello` | info | `proto, build, ok` | stale or mismatched clients (`build` is the client's identity string) |
| `hook.report` | debug | `session, kind, event, parsed` | agent hooks arriving (or not) |

### Session lifecycle

| kind | sev | fields | catches |
|---|---|---|---|
| `session.spawn` | info | `session, agent, mode: fresh\|history, conv, host, project, worktree, parent, account, prompt, ms` | what was launched, where (`project` is a `path_hash`), with which conversation |
| `session.resume` | info | `session, agent, decision, conv, recorded_conv, transcript, host, status, exit_reason, archived` | resume decisions: `resume_verified` (transcript found), `resume` (id not verified), `fresh_unwritten` (Claude never wrote it; started fresh), `no_conversation`, `remote`, `already_live`; `exit_reason` is `system_restart` for a session a reboot ended |
| `session.launch` | info | `session, agent, transport: held_deferred\|held\|direct\|remote, ms` | every Session start, including resume, fork and account relaunches |
| `session.launch_failed` | incident | `session, agent, transport, stage: spawn\|holder_launch\|holder_wait, io\|kind, ms` | spawns that never produced a child (holder missing, manager down, exec errno) |
| `session.exec` | debug | `session, defer_ms, ms, cols, rows` | a deferred launch waiting on the first client size |
| `session.status` | debug | `session, from, to` | status transitions (`starting, idle, working, needs_input, exited, unknown`) |
| `session.exit` | info | `session, agent, code, signal, requested, runtime_s, adopted, modes` | how every PTY child ended; `requested` distinguishes kills from crashes |
| `session.early_exit` | incident | `session, agent, kind: exit, code, signal, ms` | an unrequested nonzero exit within 10 s of launch. A `returnToLoginShell` agent is `exec`ed by its login shell, so its own status is the PTY's (a resume of a missing conversation: "No conversation found" → exit 1). Earlier engines also sent `kind: returned_to_shell` and a `modes` field, for a wrapped agent already back at its login shell 10 s after launch |
| `session.agent_exited` | info/warn | `session, agent, source: session_end_hook\|wrapper, code, signal, runtime_s, modes` | Earlier engines only: a `returnToLoginShell` agent that returned to its login shell (`session_end_hook`, checked 2 s after Claude's `SessionEnd`) or whose wrapper reported its status (`wrapper`, `OSC 6973;agent-exit;<$?>`). Wrapped agents now end their session, so `session.exit` carries their status |
| `session.agent_relaunch_requested` | info | `session, agent` | an agent exited 0 with its manifest's `relaunchNotice` in the bottom screen lines; that exit is not published (Codex after its startup self-update: "Please restart Codex.") |
| `session.agent_relaunched` / `session.agent_relaunch_failed` | info / warn | `session`; failed adds `code` (hashed control error code) | the Engine replaced that tab's exited agent with a fresh launch of it (resume of a known conversation, otherwise fresh), injection included |
| `session.modes_left_on_exit` | warn | `session, agent, kind: pty_exit, modes` | mouse tracking or bracketed paste still on after the program that enabled it exited: `^[[<35;14;25M` typed into the shell (`kind: returned_to_shell` from earlier engines) |
| `session.conversation` | info | `session, agent, conv, previous, source: hook\|cursor_store\|codex_repair` | conversation ids assigned, discovered or changed |
| `session.conversation_refused` | info | `session, conv, reason: not_session_start\|held_by_other_session, holder` | a hook named another conversation and was not allowed to re-point the tab (only Claude SessionStart may switch it; never onto a conversation another live tab holds) |
| `session.transcript` | debug | `session, path (hash), moved` | the transcript moving (worktree entry) |
| `session.lost` | info | `session, agent, conv, status` | each session whose holder was gone at Engine start |
| `session.adopted` | debug | `session, agent, hibernated, from_capsule` | each holder re-adopted at start |
| `holder.adopt_failed` | warn (stat), error (adopt) | `session, stage, kind\|io` | a live holder the Engine could not re-adopt |
| `session.wake` | info | `session, reason, frozen_s` | a hibernated session thawed |
| `session.migrate` | info | `session, agent, from_host, to_host, transcript_migrated, warnings` | local↔remote handoffs |
| `governor.freeze` | info | `session, reason: idle\|memory_pressure, idle_s, footprint_mb, processes, idle_threshold_s` | sessions frozen and the evidence used (idle status, unattended, quiet CPU/output, no ports) |
| `governor.freeze_vetoed` | debug | `session, reason: listening_port, ports` | a freeze skipped for a serving tree |
| `governor.freeze_failed` | warn | `session, io` | SIGSTOP failing |

### Input and delivery

| kind | sev | fields | catches |
|---|---|---|---|
| `prompt.delivered` | info | `session, delivery: echo_verified\|blind_enter, chars, ms` | initial prompts and how they were confirmed |
| `prompt.delivery_failed` | error | `session, delivery, reason: session_ended\|submission_unconfirmed\|input_failed, chars, ms` | lost or unconfirmed initial prompts |
| `prompt.workspace_trust_accepted` | info | `session` | Claude's trust picker answered on the user's behalf |
| `message.deliver` | info | `session, delivery: sent\|unknown, duplicate, submit, chars` | agent-to-agent messages (MCP `send_prompt`) |

### Attach

| kind | sev | fields | catches |
|---|---|---|---|
| `attach.open` | debug | `session, preview, seed_bytes, ms` | slow seeds (blank pane on tab switch) |
| `attach.close` | debug | `session, preview, attached_s` | attachments ending |
| `attach.rejected` | info (≤ 1/min per session) | `session, reason` (`not_terminal`\|`session_not_found`\|`keyboard_unsupported`), `suppressed` (refusals since the last event) | an attach the Engine refused for good; it answers with an `AttachRejected` frame before closing. Replaces `attach.note_rejected` (before 2026-10-01: one per attempt, tens of thousands from one pane retrying a note every 500 ms) |
| `attach.sink_reseeded` | debug | `session, reason: backlog\|stalled, preview, lagged_ms` | a client that fell behind (descheduled by App Nap or memory pressure): its stale diffs were dropped and, once its socket had room, it got a Full Snapshot on the same connection (counter `attach.lag_reseeds`). Since 2026-10-02 |
| `attach.sink_dropped` | warn | `session, reason: backlog\|stalled, preview, attached_s` | a client that stayed behind for 30 s (before 2026-10-02: 2 s, or one backlog overflow), or a connection no pump serves; it reattaches and is reseeded with a full grid |

### Remote

| kind | sev | fields | catches |
|---|---|---|---|
| `remote.connection` | info (incident when `to: failed`) | `session, from, to` | connecting/connected/reconnecting/failed/exited transitions |
| `remote.control_revoked` | warn | `session` | another controller took the session's lease |
| `remote.helper_error` | error | `session, code, fatal` | structured Helper errors (stale epoch, wrong incarnation, ...) |
| `remote.connection_fatal` | error | `session, reconnects` | protocol violations that fail the transport closed |
| `remote.uncertain_input` | error | `session` | input whose delivery could not be proven; the session fails closed |
| `remote.helper_ready` | info | `host, path: cached\|fused\|bootstrap\|reinstall, target, protocol, ms` | bootstrap and probe latency, artifact selection |
| `remote.helper_failed` | incident | `host, forced, io, ssh, ms` | bootstrap failures (only structured I/O facts and `ssh`, the OpenSSH failure class such as `ssh_auth_failed`; never remote output) |
| `remote.helper_upload` | info | `host, target, bytes, ok, ms` | Helper uploads |
| `remote.persistence` | info | `host, capability: native-detach\|user-supervisor\|non-persistent` | persistence probe outcome |
| `remote.restore_skipped` | warn | `session, host, reason: helper_unavailable\|inspect_failed, io` | remote sessions left behind at Engine start |
| `ssh.command_failed` | warn | `phase, exit, signal, ssh_failure, class` | SSH exit codes per bootstrap/RPC phase (255 = OpenSSH itself: connect, auth, host key); `class` is OpenSSH's stderr classified on the Mac (`ssh_unresolved_host`, `ssh_refused`, `ssh_unreachable`, `ssh_timeout`, `ssh_auth_failed`, `ssh_host_key`, `ssh_host_key_changed`, `ssh_connection_closed`, `ssh_config`, `ssh_control_master`, `ssh_failed`), never the text |
| `ssh.control_master_retry` | warn | | a request refused by an exiting multiplexing master, retried once on a fresh connection |
| `ssh.command_timeout` | warn | `timeout` | SSH commands killed at their deadline |

### Accounts

| kind | sev | fields | catches |
|---|---|---|---|
| `account.switch` | info | `agent, switched, unchanged, deferred, failures, default_changed` | account switch outcomes |
| `account.switch_failed` | warn | `agent, session` | a tab that failed to follow the switch |

### Holder process (`p: "holder"`)

| kind | sev | fields | catches |
|---|---|---|---|
| `holder.manager_start` | info | `guard` | manager (re)starts; `guard: false` means crash cleanup is unavailable |
| `holder.manager_exit` | info | `ok` | idle retirement vs. accept failure |
| `holder.manager_failed` | incident | `kind` | the manager exiting with an error |
| `holder.session_failed` | error | `session, kind` | a session Holder that failed to run |
| `holder.spawn` | info | `session, cols, rows, ms` | the PTY child the Holder started |
| `holder.spawn_failed` | incident | `session, io` | PTY spawn/exec failures with errno |
| `holder.exit` | info | `session, code, signal, runtime_s` | the child's exit as the Holder reaped it |
| `holder.subscriber_dropped` | warn | `session, offset` | an Engine output subscriber too slow to keep up (it falls back to the log) |

## App (`crates/diri-app`, `diri-client`, `diri-term`)

Started in `main` via `telemetry::start` (never in tests, headless previews or
`DIRI_SETTINGS_PREVIEW`): `init_default(App)`, panic hook, 60 s health
sampler. Controls live in Settings › General › Privacy (upload toggle, name,
Support ID, *Send now*, *Show in Finder*) and Help › Report a Problem…. *Send now*
and Report a Problem call the Engine's `telemetry.upload_now` RPC, which runs
one upload immediately (flushing the Engine's own recorder first) and ignores
the sharing toggle: the click is the consent. There is no About
surface, so the Support ID is shown only in Settings. The first run of a
recording build shows one 20 s toast (*diri shares diagnostics*) and writes
the default `telemetry/config.json`; the file's existence is the "seen" mark.

**Health gauges:** `windows_main`, `windows_floating` (menus, palette,
popovers: each is a window), `windows_opened` (lifetime), `terminal_panes`,
`attached_sessions` (mounted session transports), `app_active`.

**Metrics** (`timings` unless noted): `ui.frame` (root render → last paint
of a main window), `term.paint` (one terminal element's prepaint + paint),
`input.echo` (input → next screen change, first input of a burst, ≤ 2 s),
`pane.attach`, `pane.first_grid`, `pane.first_paint`, `client.connect`,
`rpc.<method>` per control method; counters `rpc.calls`, `rpc.errors`,
`rpc.disconnected`, `pane.reseed`, `pane.attach_retries`,
`pane.input_rejected`, `pane.reconnect_silent` (detaches with no typing at risk, which raise no notice).

Frame breakdown, one set per `ui.frame` sample: timings `ui.frame.cpu` (the
main thread's CPU time over the same span; far below `ui.frame` means the
frame waited on a busy Mac rather than computed), `ui.frame.layout` (GPUI:
root and uncached renders plus layout requests), `ui.frame.prepaint` (Taffy
layout, cached views that missed, element prepaint), `ui.frame.paint` (scene
building up to the probe), `ui.frame.terminals` (terminal paints within the
frame) and, on frames with assistive technology attached, `ui.frame.a11y`
(the previous frame's accessibility-tree update); counters
`ui.frame.views_rendered`, `ui.frame.views_reused` (cached views replayed),
`ui.frame.terminal_paints`, `ui.frame.shape_misses` (terminal text-shaping
cache misses) and `ui.frame.a11y_frames`. Divide a counter by `ui.frame.n`
for a per-frame mean. With assistive technology attached (VoiceOver, and
utilities that read other apps' windows: window managers, dictation, text
expanders), GPUI re-renders every cached view nested in one that
re-renders, so `views_rendered` per frame rises.

Keystroke hops, same keystrokes as `input.echo`: `input.echo.transport`
(input queued → the first grid frame after it reached the pane's transport
task: socket, Engine, Holder, PTY and the agent), `input.echo.apply` (that
frame → applied on the main thread), `input.echo.paint` (applied → the
terminal painted it) and `input.echo.<class>` (input queued → painted, per
agent class as above). `transport` minus the Engine's
`input.echo.engine` + `input.echo.publish` is the sockets and the Holder.

**Stall watchdog:** a background thread posts a ping to the main thread once
a second (every 5 s while diri is not frontmost); the answer's latency is the
stall. Idle cost is one wakeup per interval per side and no main-thread timer.
A ping unanswered for 5 s is recorded and flushed before the stall ends, so a
hang that ends in Force Quit still leaves a record. Durations are lower bounds
(± one interval).

| kind | sev | fields | catches |
|---|---|---|---|
| `app.launch` | info | `ms` (main → first painted frame), `version`, `windows` | slow launches; the app's version (`process.start.version` is the recorder crate's) |
| `app.activate` / `app.deactivate` | info | | context for stalls, OSC 52 refusals |
| `app.sleep` / `app.wake` | info | | gaps that are sleep, not hangs; reconnect storms after wake |
| `app.quit` | info | `uptime_s, windows_main, windows_opened` | clean exit vs crash (a timeline that just stops) |
| `window.open` / `window.close` | info (main), debug (floating) | `kind` (`main`\|`floating`), `window`, `lived_s`, `open` | window churn vs RSS growth (closed-window leaks) |
| `ui.frame` → `ui.slow_frame` | warn (≥ 50 ms); debug (≥ 8.3 ms, at most one per 30 s) | `ms, window, surface` (`workbench`\|`settings`\|`palette`\|`launcher`), `workspace, active` (window key), `app_active`, `cpu_ms` (main-thread CPU in the frame), `faults` (process page faults in the frame), `idle_ms` (since the window's previous frame), `layout_ms, prepaint_ms, paint_ms, views, reused, terminals, terminal_ms, shape_misses, windows, a11y` | "diri is slow/janky"; which phase, how many views and terminals, whether assistive technology was attached. `cpu_ms` ≪ `ms` means the thread was starved or paging, not working; many `faults` after a long `idle_ms` means memory the system compressed being paged back in |
| `ui.stall` | warn (1–3 s), incident (≥ 3 s) | `ms, ongoing, active, was_active` (frontmost when it began), `cpu_ms` (main-thread CPU during it), `faults`, `action` (static action name that finished inside it) | beachballs, hangs; `ongoing=true` is written at 5 s while still stuck. `cpu_ms` ≈ `ms`: busy on the main thread; ≈ 0: blocked (lock, synchronous call, AppKit) or not scheduled |
| `ui.action` | debug | `action` (GPUI action name), `source` (`shortcut`\|`palette`) | what the user did just before a failure |
| `ui.toast` | info | `title` (static toast title) | errors the user was shown ("Terminal", "Target unavailable", …) |
| `privacy.notice_shown` / `privacy.upload_changed` | info | `upload` | consent history |
| `settings.privacy_save_failed` | error | `io` (io kind) | failed save; the UI keeps the confirmed value and shows an error |
| `user.report` | incident | `version, support_id` | Help › Report a Problem…: the moment to look around (uploaded immediately) |
| `telemetry.upload_now` | info | `status, batches` | Engine: a user-requested upload and its outcome (`sent`, `up_to_date`, `failed`, `timeout`, `unavailable`) |
| `client.connected` | info | `reconnect, attempts, down_ms, connect_ms, hello_ms, first_failure, engine_build, engine_pid, proto` | slow Engine start, how long an outage lasted |
| `client.disconnected` | warn | `kind, connected_s` | Engine crash/restart seen from the app |
| `client.connect_failing` | error | `attempts, down_ms, kind, handshake` | Engine never came up (≈ 45 s of retries) |
| `client.identity_rejected` | warn (`instance_changed`), error | `reason, engine_kind, engine_build, engine_pid, proto` | stale/foreign daemon on the socket |
| `rpc.error` | error | `method, kind, code, ms` | failing spawn/resume/kill… by method and Engine error code |
| `rpc.slow` | warn | `method, ms` (≥ 2 s; not `events.wait`/`test.run`) | slow Engine operations |
| `attach.closed` | warn, error (decode) | `session, reason` (`eof`\|`read_error`\|`write_error`\|`keepalive_timeout`\|`decode_error`\|`bad_grid`\|`bad_modes`\|`commands_closed`), `live_ms`, `suppressed` | why a terminal connection dropped; protocol corruption. Closes under 1 s are recorded at most once a minute per session (`suppressed` counts the rest); refusals are recorded as `pane.attach_rejected` instead |
| `pane.attached` | info | `session, reconnect, attempts, connect_ms, since_mount_ms` | attach latency, reattach loops. Muted, with `pane.detached` and `pane.drain_interrupted`, after 3 attaches in a row that ended before a grid |
| `pane.attach_flapping` | warn | `session, attaches, since_mount_ms` | a pane whose attaches keep closing before a grid, for no stated reason (it backs off 0.5 s → 30 s) |
| `pane.attach_rejected` | warn | `session, reason, since_mount_ms` | the Engine refused this pane's attach; the pane shows "Terminal unavailable" and waits for the session's status to change |
| `pane.attach_failing` | error | `session, attempts, reason, since_mount_ms` | a session that cannot be attached (3 failures) |
| `pane.first_grid` | debug; warn if not a snapshot | `session, ms, snapshot` | first frame missing or a diff before a seed |
| `pane.first_paint` | debug | `session, ms, grid_ms, shown_ms, parked` | mount (or, for a pane nobody drew at mount, the first frame that showed it: `shown_ms` after mount) → the first frame that drew content, taken inside the terminal element's paint; `grid_ms` stays relative to mount. Once per mount of a resident per view. A pane that is never drawn (the selection pane under a workspace workbench, a warm pane of another tab, a window the system stopped drawing) records none; before 2026-09-30 the blank watchdog recorded those as a ~10 s "first paint" |
| `pane.blank` | incident; warn if live with a blank grid | `session, agent, state, got_grid, content, frames, ms` | "session doesn't render": drawn at least once since mount, running, and no content painted 10 s after mount. `content=true` means the grid holds content that was never painted (a missed repaint: always an incident). A pane never drawn since mount is not reported |
| `pane.detached` | warn | `session, live_ms, grids, reseeds, input_at_risk` | a live attachment ended. `input_at_risk=true` (typed input with no frame since, or within 2 s of the close) is followed by the `ui.toast` "Terminal" notice "Reconnected. Your last keystrokes may not have arrived."; otherwise the pane reconnects silently. Before 2026-10-02 every detach raised "Terminal connection interrupted…" |
| `pane.drain_interrupted` | warn | `session` | input possibly lost on detach |
| `pane.input_rejected` | warn (≤ 1 per 5 s per session) | `session, input` (`input`\|`mouse`\|`mouse_motion`\|`scroll`), `reason` (`passive_view`\|`disconnected`\|`overloaded`) | typing that goes nowhere; lost lease |
| `pane.resize_storm` | warn (≤ 1/min) | `session, flips, cols, rows` | layouts fighting over the PTY size |
| `pane.modes` | debug | `session, mouse, mouse_bits, alt_screen, bracketed_paste` | mouse tracking left on after an agent exits (`^[[<35;…M` in zsh). Once per session per change: every view attached to the session sees the same Modes chunk, and before 2026-09-30 each of them recorded it |
| `pane.drop` | info | `session, files, outcome` (`paste`\|`upload`\|`refused`), `partial, remote` | Finder drops that did nothing |
| `pane.drop_upload_failed` | error | `session` | remote drop copy failed |
| `clipboard.copy` | info | `source` (`selection`\|`osc52`), `outcome` (`ok`\|`not_on_pasteboard`\|`empty_selection`\|`relayed`\|`stale`\|`app_inactive`\|`unknown_session`\|`no_listener`), `size`, `ms`/`age_ms`, `mouse_captured`, `session` | "copy doesn't work" (incl. agent-captured mouse) |
| `clipboard.write_failed` | error | `source, size` | an agent's OSC 52 copy that never reached the pasteboard |
| `clipboard.paste` | info | `outcome` (`sent`\|`review`\|`into_find`\|`image_staged`\|`image_stage_failed`\|`empty_clipboard`\|`no_session`\|`no_terminal`\|`no_text`\|`copy_mode`\|`ignored_in_find`), `kind, size, bracketed, ms` | "paste doesn't work" |
| `clipboard.image_upload_failed` | error | `session` | image paste into a remote session |
| `term.slow_paint` | warn | `ms, cols, rows, shape_misses` | one terminal paint ≥ 50 ms |
| `update.check` / `update.download` / `update.install` | info; error on failure | `outcome, from, to, user_initiated, ms, error_kind, http_status` (network `error_kind`: `dns`, `connect`, `timeout`, `tls`, `not_found`, `rate_limited`, `http_error`, `network`) | updates that fail or never arrive |

Sizes are buckets (`0`, `<64`, `<1k`, `<16k`, `<256k`, `<1m`, `>=1m`); no
clipboard, paste, keystroke or terminal content is ever recorded.



### Notes

Counts only, through `telemetry::notes_event(name, kind)`: a fixed event name
and, where it helps, a fixed family. Never note text, titles, URLs, session or
note ids.

| kind | sev | fields | catches |
|---|---|---|---|
| `notes.link_editor.opened` | info | | ⌘K panel use |
| `notes.link.pasted` | info | `kind` (as `notes.link.set`) | bare links pasted, by tool family |
| `notes.mention.inserted` | info | `kind` (`session`\|`note`) | `@` use |
| `notes.fold.toggled` | info | `kind` (`chevron`\|`keyboard`) | folding by hand (not the to-do handoff's programmatic folds) |
| `notes.link.set` | info | `kind` (`notion`\|`google`\|`linear`\|`hubspot`\|`figma`\|`slack`\|`github`\|`dashboard`\|`mention`\|`web`) | which tools people link, to decide which chips matter |
| `notes.link.removed` | info | | links taken back out |
| `notes.image.added` | info | `kind` (`paste`\|`drop`\|`picker`) | how pictures get into notes |
| `notes.image.failed` | info | `kind` (as above) | pictures refused (format, size, write) |
| `notes.callout.added` | info | | callout use |
| `notes.table.inserted` | info | `kind` (`slash`) | tables made from `/table` |
| `notes.table.pasted` | info | `kind` (`markdown`\|`tsv`\|`csv`) | tables pasted, and from where (Sheets/Excel/Numbers arrive as TSV) |
| `notes.table.row_added` | info | | rows added (menu, ⌃⇧↑/↓, Tab in the last cell, Return in a cell) |
| `notes.table.col_added` | info | | columns added (menu, ⌃⇧←/→, a wider paste) |
| `notes.search.opened` | info | | Search notes page opened (⇧⌘F, ⌘K, To-dos header) |
| `notes.search.result_opened` | info | `kind` (`live`\|`archived`\|`orphan`) | which notes people go back to, and whether archived and Session-less files matter |

## Activation

The new-user funnel: six milestones, each recorded **once per install** by
`diri_telemetry::activation`. A milestone is claimed by publishing its marker
file under `telemetry/activation/` with an exclusive hard link, so when the app
and the Engine race, exactly one records it, and a restart, an upgrade or a
repeat of the trigger never records it again. With `DIRI_TELEMETRY=off`
nothing is recorded and no marker is written.

**Existing users.** The first process of a build with activation tracking
writes `origin.json` before it writes anything else. `preexisting` is true when
Diri was already used on this Mac: `telemetry/config.json` (written on the
first run of any recording build), the Engine's `state.json` or the app's
`prefs.json` exists. An upgrade therefore still records `first_launch` (on the
first launch of the new build) and the later steps as they happen, but every
milestone event carries `preexisting: true`, and the funnel keeps those
installs out of the new-user cohort. The baseline is decided once; files
written after it never change it.

Every milestone event also carries `preexisting` and `since_first_launch_s`
(seconds from the `first_launch` marker, or from the baseline when the app has
not launched yet, e.g. an Engine started by the CLI).

| kind | process | sev | fields | when |
|---|---|---|---|---|
| `activation.first_launch` | app | info | | the app's first launch on this install (from `telemetry::start`) |
| `activation.agent_ready` | app | info | `agent, source: onboarding_install\|preexisting\|manual, agents` | the first local agent catalog with a launchable agent (terminals and notes excluded). `onboarding_install`: the agent the welcome's one-click install was waiting for; `preexisting`: launchable in the first catalog of the first launch; `manual`: became launchable later, installed outside Diri. `agents` is how many are launchable |
| `activation.first_session` | engine | info | `agent, mode: fresh\|history, helper` | the first agent session started (`session.spawn`; terminals and notes do not count). `helper`: started by another agent |
| `activation.second_session` | engine | info | `agent, mode, helper, concurrent` | the second agent session started; `concurrent`: another agent session was live at the time |
| `activation.first_helper` | engine | info | `agent` | the first agent session with a parent session: an agent started it through MCP `spawn_agent`/`spawn_agents` |
| `activation.returned` | app | info | `days` | the first app launch on a later local calendar day than `first_launch` (`days` apart) |

The Worker indexes these into D1 `milestones` (one row per install and step,
`INSERT OR IGNORE`), and `GET /v1/admin/funnel` / `diri-debug funnel` count
them per cohort of installs first launched in a window. `diri-debug local
activation` shows this Mac's markers next to the events in its spool.

## Upload

The Engine's uploader wakes once a minute. If `spool/urgent` exists, or an
hour has passed, it uploads (hourly keeps each install to about ten
batches a day, which is what the Worker's free-tier budget is sized on). It sends every spool file's new complete
lines (across app, Engine and Holders) as gzip NDJSON, at most 1 MiB raw per
request:

```
POST {endpoint}/v1/ingest
Content-Type: application/x-ndjson
Content-Encoding: gzip
X-Diri-Install: <install uuid>
```

The first line is the batch header:

```json
{"v":1,"type":"batch","install":"<uuid>","support_id":"D-7K3MQ9XA","name":"alex","app_version":"0.9.0","build":"<sha>","channel":"stable","os":"macos","os_version":"27.0","arch":"aarch64","sent_at":1790581979447,"lines":1234}
```

The rest are records as above. Responses: `2xx` accepted; `400/413/422`
rejected permanently (the client skips the batch); `429/5xx` retried next
cycle.

## Worker (`telemetry/worker`)

Cloudflare Worker + R2 + D1.

- **Ingest** validates the header, caps sizes (5 MiB compressed, 64 MiB
  raw, 50k lines), throttles by edge-provided source, globally and per install,
  stores the original gzip body in
  R2 at `v1/<install>/<yyyy-mm-dd>/<sent_at>-<rand>.ndjson.gz`, and indexes it
  in D1: the install (upsert), the batch (time range, processes, R2 key),
  incidents and errors (`s` of `error` or `incident`, with a grouping
  signature), sessions seen (`session`, `agent`, `conv`, first/last seen) and
  activation milestones (`activation.*`, first copy per install and step).
- **Admin** endpoints under `/v1/admin/*` require
  `Authorization: Bearer <ADMIN_TOKEN>` (a Worker secret): find installs by
  name, support id or UUID prefix; list incidents (filter by install, kind,
  version, time) and group them; list batches in a time range; fetch a batch
  body; list sessions of an install or find the install that owns a session
  id or conversation UUID.

  Routes:

  | Route | Returns |
  |---|---|
  | `GET /v1/admin/installs?q=` | installs matching a name substring, Support ID (`D-…`, prefix ok) or install UUID prefix; no `q` lists the most recent |
  | `GET /v1/admin/installs/<uuid>` | install row, batch totals, incident counts by severity, session count, versions seen |
  | `GET /v1/admin/incidents?install=&kind=&sev=&version=&session=&conv=&signature=&since=&until=&limit=` | incident rows with the install's `name` and `support_id`, newest first; `kind` accepts `*`/`?` globs |
  | `GET /v1/admin/incidents/summary?…same filters` | groups by signature: `count`, `installs` affected, `first_t`, `last_t`, `versions` |
  | `GET /v1/admin/batches?install=&since=&until=` | batches whose record time range overlaps the window, oldest first |
  | `GET /v1/admin/batch?key=<r2 key>` | the stored gzip body, streamed as `application/gzip` (the caller gunzips) |
  | `GET /v1/admin/sessions?install=` | `(session, conv)` spans with agent, newest first |
  | `GET /v1/admin/find?id=<session id or conversation uuid>` | matching spans joined with the owning install's name and Support ID |
  | `GET /v1/admin/funnel?since=&until=&version=` | the activation funnel of installs whose `first_launch` is in the window (default: the last 7 days), optionally of one first-launch version: `new` and `preexisting` cohorts, each `{installs, steps[{step, installs, of_cohort, of_previous, median_s, sources?}]}` |
- **Admission**: a required Workers Rate Limiting binding throttles each
  edge-provided source to 30 attempts/minute/location, before D1/body processing.
  An atomic D1 reservation limits all sources together to 3,600 attempts per UTC
  hour (config may lower it). No IP is stored in D1/R2. Install IDs remain
  untrusted labels, not authenticated identities.
- **Retention**: the daily cron uses server receipt time for batches, incidents
  and session indexes. Client event clocks never extend retention. Migration
  `0003_server_retention.sql` assigns existing incident/session indexes a zero
  receipt time, so the next sweep expires them conservatively. R2 lifecycle
  expiration remains the backstop for raw batches.

## CLI (`telemetry/cli`)

`diri-debug` answers "what happened to this person" from the admin API, and
reads a local spool with `--local` for the developer's own machine:

```
diri-debug who alex                        # installs matching a name / support id
diri-debug incidents [alex] --since 7d     # recent incidents
diri-debug top --since 7d --version 0.9.0   # grouped incidents across installs
diri-debug timeline alex --around "2026-09-27 23:40" --window 20m [--session s_…] [--kind 'session.*']
diri-debug health alex --since 24h         # memory / fds / cpu / frame-time trends per process
diri-debug sessions alex                   # sessions, agents, conversations
diri-debug find <session id | conversation uuid>
diri-debug funnel --since 14d [--version 0.9.0] [--preexisting]   # activation funnel per cohort
diri-debug local [--since 1h] [...]         # same views over ~/Library/Application Support/Dirijor/telemetry/spool
diri-debug local activation                # this Mac's activation markers and their events
```

Configuration: `DIRI_TELEMETRY_URL` and `DIRI_TELEMETRY_ADMIN_TOKEN`, or
`~/.config/diri-debug/config.json`.

## Security boundary for queued records and privacy settings

New Engine, client, Holder and app events omit free-form errors and panic
payloads. Before upload, the Engine removes retired `message`/`error` fields
from every queued record, including files still being written by old processes.
The upload projection preserves canonical key order and original-file offsets.
Detailed local spool files from old versions are not rewritten; their content
is not permission to transmit the retired fields.

`Config::load` requires an explicit boolean `upload` in a readable, valid file.
Absent or invalid settings disable sharing. The first-run app notice explicitly
persists the ordinary default; the uploader itself never treats a read failure
as consent. Settings edits are committed to UI state only after successful
atomic persistence. A failed toggle or name edit displays an inline error.
