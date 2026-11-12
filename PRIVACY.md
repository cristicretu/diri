# Privacy

Diri has no account system or advertising. It records diagnostics (crashes,
hangs, errors and timings) so bugs can be fixed from a report instead of a
reproduction, and shares them with the project unless you turn that off. The
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

**Where it goes:** unless you turn sharing off, the Engine uploads the log
about once an hour (within a minute after a crash or other incident) to a
Cloudflare Worker operated by the project. Uploads carry a random install id,
the short Support ID derived from it, and the name you chose (your macOS login
name unless you change or clear it). They are kept for 30 days and then
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

Diri connects to GitHub Releases to check for and download updates, and to the
project's diagnostics service unless you turn sharing off (see above). When a
note shows a picture linked by web address, Diri downloads that picture from
its address once and keeps it in a local cache; the request carries no cookies
or credentials, and the address is not recorded in diagnostics. It may also
make network connections when you explicitly use remote hosts, PR monitoring,
browser automation, or a tool/agent that uses the network. Those tools and
services have their own privacy practices. Diri does not proxy their traffic
through a Diri-operated server.

Remote-node credentials remain in the mechanisms you configure (for example,
SSH configuration and your keychain); they are not sent to the Diri project.

## Process access

Diri is not sandboxed because its core function is to launch shells and coding
agents, create worktrees, and communicate with local tools. Child processes run
with your macOS user privileges and may inherit environment variables. Only run
agents and MCP servers you trust, and review their permissions separately.

For vulnerability reports, follow [SECURITY.md](SECURITY.md).
