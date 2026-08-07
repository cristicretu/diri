# Security model

Diri is a local developer tool that deliberately launches other powerful local
developer tools. It reduces orchestration mistakes; it is not a sandbox.

## Trust boundaries

### Desktop app and daemon

The app talks to a background daemon over local Unix sockets. The daemon owns
PTYs, terminal replay logs, worktrees, child processes, and persistent session
state. Socket and state paths are scoped to the current user. Another process
already running as that user should be treated as inside the same trust boundary.

The PTY holder lets sessions survive daemon restarts. Compatibility changes to
the holder protocol or on-disk registry must preserve existing sessions or ship
an explicit migration.

### Child tools

Shells, coding agents, hooks, MCP servers, and browser automation run with the
macOS user's privileges. They can read any files that user and macOS privacy
controls allow, use inherited environment variables, access configured
credentials, and make network requests. Diri does not inspect or approve each
operation they perform.

Use separate worktrees to avoid accidental edit collisions, not as a security
boundary. For untrusted code, use a dedicated OS account, VM, or container and
restrict credentials and network access there.

### Remote nodes

Remote sessions cross the SSH boundary and run under the configured remote
account. Diri relies on SSH host verification, keys, and configuration; it does
not provide a separate encrypted relay or authorization layer. Prefer a
dedicated non-admin user and narrowly scoped credentials.

### Updates

The updater downloads a versioned ZIP from GitHub Releases, checks its SHA-256
from the release feed, verifies the code signature, requires the running app's
Team ID and bundle identifier, validates notarization, and refuses downgrades.
Published release assets are treated as immutable. Details are in
[UPDATING.md](../diri/UPDATING.md).

## Sensitive data

Terminal replay logs can contain prompts, output, paths, and secrets emitted by
tools. PR monitoring, remote hosts, and third-party agents can send data to their
own services. Diri itself has no account, analytics, or telemetry service; see
[PRIVACY.md](../PRIVACY.md).

## Security assumptions

Diri assumes:

- macOS and the current user account are not already compromised;
- installed agents, MCP servers, hooks, and shell configuration are trusted;
- GitHub, Apple code-signing/notarization, Homebrew, SSH, and dependency sources
  provide the guarantees documented by those systems;
- contributors and release operators protect their GitHub and Apple credentials.

## Reporting

Report boundary bypasses, unsafe IPC/update behavior, credential disclosure,
and unintended code execution privately through [SECURITY.md](../SECURITY.md).
