# MCP task reliability

## Contracts

### Retrying a spawn

`spawn_agent` now uses `session.spawn_tracked`. Supply a stable `operation_id`
for a logical spawn. Identical arguments without one derive the same identity
for the same caller. An intentional second Agent needs a new `operation_id`.
The ordinary user CLI and app retain their existing untracked spawn interface.

The Engine commits a reservation and session ID before worktree creation,
remote bootstrap, or launch. Generated worktree branches derive from that ID
and are included in the receipt. The session record is saved before a Holder
can launch so crash recovery can match its binding. It records `completed` after the original spawn
contract finishes. Repeated calls return the original ID and receipt, including
when the reply was lost. Concurrent retries cannot start another launch.
A conflicting payload is rejected. Records are never evicted to make room for
new identities; the 100,000-operation limit fails closed.

`spawn_receipt.outcome` is `completed`, `failed`, or `unknown`. Unknown includes
an operation still running and one interrupted by an Engine crash. Failed or
unknown may have left a session or worktree; inspect the returned session ID
and workspace before recovery. Neither state automatically relaunches or
retypes an initial prompt. A completed receipt describes the original spawn,
not proof that its Agent is still running today.

### Assigning and completing a task

For work whose completion matters:

1. Spawn an Agent without an initial prompt, using a stable `operation_id`.
2. `submit_task(session_id, text, request_id)` assigns a logical task once.
3. The assigned Agent calls `report_task(task_id, status: "acknowledged")`.
4. It does the work and calls `report_task` with `completed` or `failed`, plus
   a `result` describing the outcome and evidence. `blocked` is nonterminal.
5. The parent calls `wait_for_task(task_id)` and inspects `completed` and the
   returned task result. A timeout is not completion.

Submission adds a short task-ID/reporting instruction to the delivered text.
Use the same `request_id` when a reply is lost; a new ID means new work. Without
an ID, identical target/text from the same caller derive one identity.
`get_task(request_id: "original-id")` recovers the receipt without sending input,
even after the target disappears. Sending, explicit Agent acknowledgement, and an explicit result are separate
facts. Terminal idle, unrelated output, process exit, and disappearance cannot
complete a task. Terminal results are immutable; identical reports are safe to
repeat. A different task cannot inherit another task's completion.

These are Agent reports, not independent verification that the work is correct.
The parent should inspect the returned evidence. Native provider turn events
are not inferred or fabricated. An Agent must have access to these MCP tools
to report; a remote session without MCP connectivity remains unacknowledged.
This change does not implement remote MCP forwarding or install a remote service.

`get_task` and `wait_for_task` are restricted to sender/recipient; `report_task`
is restricted to the recipient. Assignment preserves the existing send policy.
This is the existing trusted local Engine control boundary: caller identity is
bound by the MCP process environment, not a new multi-user authentication system.

Task waits subscribe before refreshing the durable record and use absolute
bounded deadlines. Cancelling a wait closes its sockets and never cancels the
Agent's task. Task cancellation/undo is not implied by MCP request cancellation.

### Storage and compatibility

The Engine owns additive, versioned SQLite journals (`operations-v1.sqlite` and
`tasks-v1.sqlite`) beside its socket. Files are owner-only, reject symlinks, use
full synchronous commits, and store fingerprints instead of prompts/argv/env.
Task results are intentionally stored (bounded to 16 KiB). Task events expose
only task IDs/revisions, not prompts or results. Journals survive restart; a
corrupt/unavailable/full journal prevents untracked effects.

The old message receipt schema and Helper protocol are unchanged. The new MCP
fails closed against an Engine without tracked spawn/task methods. Existing
Holders remain usable. This adds no dependency to the production Helper;
`dirijor-mcp` is a test-only dependency of `diri-remote`.

## Continuous failure testing

The default `diri-remote` integration test runs real Helper/Holder processes
with a disposable fake SSH executable. The opt-in variant uses actual OpenSSH:

```sh
DIRI_REMOTE_SSH_TARGET=user@disposable-host \
DIRI_MCP_SOAK_ROUNDS=5 DIRI_REMOTE_SOAK_SECONDS=2 \
scripts/mcp-remote-soak.sh
```

Run from `diri/`. The remote account must have `/bin/sh`, `stty`, and a compatible
platform. Set `DIRI_REMOTE_HELPER_PATH` to a Helper built for that remote
platform when the local/native artifact differs. Optional overrides are
`DIRI_REMOTE_SSH_EXECUTABLE` and `DIRI_REMOTE_CWD` (default `~`). Rounds are bounded
1–100. Use a disposable account: the normal authenticated bootstrap installs
its versioned Helper cache. Only this test's session is killed during cleanup;
validated Helper binaries are retained. No real provider credentials are needed.

Both variants deliberately discard successful spawn, message, and task replies;
retry messages ten times; cancel a live task wait; tear down the local Engine
instance and its SSH channels; reopen the persisted state; and verify the same
remote process/incarnation, one spawn, one message, and exact task result. The
Agent is a deterministic shell fixture, and the driver exercises explicit Agent
reports through the production MCP bridge. This verifies transport/control
contracts, not Claude/Codex model behavior or a physical WAN outage.

The existing nightly OpenSSH job now runs this suite, including on PRs touching
these remote tests. The existing MCP subprocess tests separately exercise the
stdio dispatcher, bounded concurrency, cancellation, and startup handshake.

## Linux CI regressions

The platform-specific `Project` fixture is now qualified at its macOS-only use.
Fresh held sessions classify output from their launch epoch as live, even if
startup output reaches disk before the Engine pump starts. Previously a fast
Linux child could send `CSI 6n` before attach, have its query discarded as
history, and block waiting for its cursor report. Adoption continues to suppress
historical queries from already-running sessions.
