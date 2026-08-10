# Getting started

## Install

Diri requires macOS 15 or newer. Install the signed, notarized universal build:

```sh
brew install --cask cristicretu/diri/diri
```

Alternatively, download the latest DMG from [GitHub Releases](https://github.com/cristicretu/diri/releases/latest),
open it, and drag Diri to Applications. The app checks the same release feed for
updates; it never installs one until you click restart.

## Your first session

1. Open Diri and add a project directory.
2. Create a session and choose Claude Code, Codex, another supported agent, or a
   plain shell.
3. Work in the embedded terminal. The sidebar summarizes whether each agent is
   working, waiting for input, or done.
4. Quit and reopen Diri. The background daemon owns the PTY, so the session and
   its terminal history remain available.

Claude Code and Codex have first-class status detection and resume support.
Other agents may offer partial detection; every agent can still run as a normal
terminal.

## Parallel work with worktrees

Create separate sessions with separate git worktrees when agents may edit the
same repository. This keeps branches and working trees isolated while Diri
gives you one place to monitor them. Treat each worktree as a normal checkout:
commit or move changes before deleting it.

## Agent orchestration

Diri ships a CLI and MCP server. A running agent can create or inspect Diri
sessions when you explicitly configure the MCP server. That capability is
powerful: it can launch local processes with your user privileges. Only expose
it to agents you trust and review requested actions.

## Remote hosts

Remote sessions use SSH and tmux; Diri does not run a hosted relay. Start with
the [remote-node guide](../diri/NODE.md), use a dedicated non-admin account when
possible, and avoid forwarding credentials the remote job does not need.

## Diagnostics

Run this when the CLI is available:

```sh
dirijor doctor
```

Daemon logs and session state live under
`~/Library/Application Support/Dirijor`. Logs may contain terminal output,
paths, or secrets printed by a process, so redact them before filing an issue.
See [SUPPORT.md](../SUPPORT.md) for the details to include.

## Local data and uninstalling

After stopping Diri and its daemon, remove the app and these directories if you
also want to remove its local state:

```text
~/Library/Application Support/Dirijor
~/Library/Application Support/diri
~/Library/Caches/diri/updates
```

Read [PRIVACY.md](../PRIVACY.md) and the [security model](SECURITY-MODEL.md)
before using Diri with sensitive repositories or remote hosts.
