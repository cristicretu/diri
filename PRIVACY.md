# Privacy

Diri has no account system, advertising, analytics, or telemetry. The project
does not run a service that receives your terminal contents or session history.

## Data stored on your Mac

Diri stores session state, terminal replay logs, host configuration, preferences,
usage summaries, and search/index data under these locations:

- `~/Library/Application Support/Dirijor`
- `~/Library/Application Support/diri`
- `~/Library/Caches/diri/updates`

Terminal logs can contain prompts, command output, repository paths, and secrets
printed by a process. Treat them as sensitive. Before attaching diagnostics to
an issue, review and redact them. Archiving can intentionally preserve session
metadata. Deleting the directories above removes all Diri-managed local data
after Diri and its daemon are stopped.

## Network activity

Diri connects to GitHub Releases to check for and download updates. It may also
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
