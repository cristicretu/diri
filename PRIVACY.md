# Privacy

Diri has no account system or advertising. It records diagnostics (crashes,
hangs, errors and timings) so bugs can be fixed from a report instead of a
reproduction. Sharing them with the project is opt-in for new installs. The
project does not run a service that receives your terminal contents or session
history.

## Diagnostics

Every Diri process (the app, its Engine, and each session's Holder) keeps a
flight recorder: a local log of what it did, in
`~/Library/Application Support/Dirijor/telemetry/spool`, capped at 64 MiB.

**What is recorded:** app, macOS and CPU versions; crashes with their stack
frames; hangs and slow frames; memory, CPU and open-file counts; how long
sessions take to start, attach and first draw; errors and their codes;
whether copy, paste, file drops and updates worked (with size classes such as
"under 1 KB", never contents); which commands ran (by name); once-only setup
milestones (first launch, first agent ready and whether it was installed from
the welcome, first and second agent session, first agent started by another
agent, a launch on a later day) with the time since first launch; and identifiers
that let a report be followed: session ids, agent names, and agent
conversation ids. Folders are recorded only as a one-way hash. Errors are
recorded as codes and classes. Free-form error messages,
subprocess stderr and panic payloads are excluded; stack symbols and source
locations remain available for diagnosing crashes.

**What is never recorded:** terminal output or input, prompts, pasted or copied
text, file contents, environment variables, command lines, URLs you open,
passwords or keys.

**Where it goes:** only if you share. A new install asks once, in a
non-blocking notice, and uploads nothing until you choose *Share*; ignoring
the notice keeps sharing off and it is not shown again. Installs from before
opt-in keep the setting they already had. When sharing is on, the Engine
uploads the log about once an hour (within a minute after a crash or other
incident) to a Cloudflare Worker operated by the project; after an opt-in,
only what was recorded from that moment on. Crash reports follow the same
rule. Uploads carry a random install id, the short Support ID derived from
it, and the name you chose (empty on a new install; your macOS login name on
installs from before opt-in, unless you changed or cleared it). They are kept for 30 days and then
deleted; only the maintainers can read them.

**Your controls:** Settings › General › Privacy has the switch (*Share
diagnostics to help fix bugs*), the name, your Support ID, *Send now* and a
button that shows the local folder. Turning sharing off stops uploads at the
next cycle; recording stays local. Missing, unreadable or malformed settings
disable uploads. A settings change only appears saved after it has been
written successfully. *Send now* uploads what has been recorded so far
immediately, even with sharing off: clicking it is a one-time choice to send.
Setting `DIRI_TELEMETRY=off` in Diri's environment turns recording off
entirely. Help › Report a Problem… marks the moment in the log, sends it right
away the same way, copies your Support ID and opens a GitHub issue with it
filled in.

## Data stored on your Mac

Diri stores session state, terminal replay logs, host configuration, preferences,
usage summaries, and search/index data under these locations:

- `~/Library/Application Support/Dirijor` (including the diagnostics log under
  `telemetry/`)
- `~/Library/Application Support/diri`
- `~/Library/Caches/diri/updates`

Terminal logs can contain prompts, command output, repository paths, and secrets
printed by a process. Treat them as sensitive. Before attaching diagnostics to
an issue, review and redact them. Archiving can intentionally preserve session
metadata. Deleting the directories above removes all Diri-managed local data
after Diri and its daemon are stopped.

## Network activity

Every outbound connection, with what it sends, when, and how to turn it off,
is listed at [diri.sh/docs/privacy](https://diri.sh/docs/privacy/) and in
Settings › Privacy (source: `website/docs-src/connections.json`). In short:
update checks (GitHub Releases, no identifiers), diagnostics (opt-in, see
above), and connections for features you use: agent usage limits, Cursor
usage, pull request status through `gh`, your SSH hosts and git remotes, agent
and computer-use installers, voice model downloads, the API tab and the
browser. Those tools and services have their own privacy practices. Diri does
not proxy their traffic through a Diri-operated server.

Remote-node credentials remain in the mechanisms you configure (for example,
SSH configuration and your keychain); they are not sent to the Diri project.

## Process access

Diri is not sandboxed because its core function is to launch shells and coding
agents, create worktrees, and communicate with local tools. Child processes run
with your macOS user privileges and may inherit environment variables. Only run
agents and MCP servers you trust, and review their permissions separately.

For vulnerability reports, follow [SECURITY.md](SECURITY.md).
