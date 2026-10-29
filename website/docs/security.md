# Security model

> What diri protects and what it does not. Agent privileges, MCP and CLI authority, lineage rules, remote trust, diagnostics and privacy, and how to report a vulnerability.

diri is a local developer tool that launches other powerful developer tools. It helps you avoid orchestration mistakes. It is not a sandbox.

## What agents can do
Shells, coding agents, hooks, MCP servers and browser automation run as your user. They can read any file your account and the operating system allow, use inherited environment variables and configured credentials, and reach the network. diri does not inspect or approve each action they take. Each agent's own permission settings still apply.

- Worktrees keep agents from editing the same files by accident. They are not a security boundary.
- For untrusted code, use a separate OS account, VM or container, with limited credentials and network access.
- Only run agents and MCP servers you trust.

### The Engine and its socket
The app, the CLI and agents talk to the background Engine over a Unix socket. The socket and state files are scoped to your user. Any process already running as your user is inside the same trust boundary and can drive diri.

The [`dirijor` CLI](/docs/cli/) acts with your full authority. It is a tool for you and your scripts.

## MCP authority and lineage
Agents that diri starts can use its [MCP server](/docs/mcp/) to start and coordinate other agents. Writes through MCP are limited by where the calling agent sits in the session tree:

| Caller | Reads | Messages | Stop, wake, resume, fork, integrate |
| --- | --- | --- | --- |
| Root agent (started by you, or from a note) | All sessions | Sessions in its project, and its direct children on any host | Sessions in its project |
| Delegated agent (started by another agent) | All sessions | Only its parent and direct children | Only its direct children |

More rules:

- Every write needs a live diri session identity. A stale or unhosted MCP process is refused.
- An agent cannot target itself, and cannot stop the session waiting on its result.
- Worktree writes must stay inside the caller's project.
- Messages to a session outside the caller's own line are labelled with who sent them.
- A delegated agent can add only to the note it was started from, or to notes that mention it or one of its ancestors.
- Delegation is capped at 3 levels deep and 16 live children per session. Set `DIRIJOR_MAX_SPAWN_DEPTH` and `DIRIJOR_MAX_LIVE_CHILDREN` in diri's environment to change the caps.

Every tool and its arguments are in the [MCP tool reference](/docs/mcp-tools/).

## Remote hosts
Remote sessions run under the account you connect as. diri relies on SSH for host verification, keys and encryption. It does not add its own relay or authorization layer.

- Prefer a dedicated non-admin account with narrowly scoped credentials.
- diri never runs `sudo` and never changes host-wide configuration.
- The remote helper is verified by length, SHA-256, build ID and protocol before use. Its folders are `0700`, its files and sockets `0600`.
- Your local environment, local sockets and credentials are not copied to the server.
- SSH password and host-key prompts are shown by a separate helper with no logging, so answers never reach diri's state or diagnostics.

Details are in [Remote hosts](/docs/remote-hosts/).

## Secrets in terminals
Terminal replay logs and scrollback can contain prompts, output, paths and secrets a program printed. Treat them as sensitive.

A password typed at a prompt that turns echo off, such as `sudo`, `ssh` or `read -s`, is never echoed, so it never reaches the replay log, scrollback or exports. While a local session sits at such a prompt, diri does not use your typing to name the session, hides clipboard text in the paste review, and on macOS turns on Secure Keyboard Entry for that terminal. Remote sessions do not report this state yet. A program that reads a secret in raw mode and draws its own mask looks like any other full-screen program.

## Account credentials
Saved Claude and Codex logins are stored in owner-only files beside the Engine's state. They are never returned in responses, logged or passed as command-line arguments. See [Accounts and usage](/docs/accounts/).

## Updates
On macOS the updater downloads a versioned ZIP from GitHub Releases, checks its SHA-256 against the release feed, verifies the code signature, Team ID, bundle identifier and notarization, and refuses downgrades. Linux packages do not update in place. Each Linux release file has a Sigstore signature you can check with `cosign verify-blob`, as described in [diri/LINUX.md](https://github.com/cristicretu/diri/blob/main/diri/LINUX.md).

## Diagnostics and privacy
diri has no account system and no advertising. It records diagnostics locally so bugs can be fixed from a report. Sharing them with the project is opt-in: a new install asks once and uploads nothing until you choose **Share**. Installs from before opt-in keep the setting they had.

Every diri process keeps a local log in `~/Library/Application Support/Dirijor/telemetry/spool`, capped at 64 MiB.

| Recorded | Never recorded |
| --- | --- |
| App, OS and CPU versions | Terminal output or input |
| Crashes with stack frames, hangs, slow frames | Prompts, pasted or copied text |
| Memory, CPU and open-file counts | File contents |
| Session start, attach and first-draw times | Environment variables |
| Error codes and classes | Command lines |
| Whether copy, paste, file drops and updates worked, with size classes | URLs you open |
| Command names, session ids, agent names, conversation ids | Passwords or keys |
| Folders, only as a one-way hash | Free-form error messages and stderr |

If you share, the Engine uploads the log about once an hour, or within a minute after a crash, to a Cloudflare Worker run by the project, starting from the moment you opted in. Uploads carry a random install id, your short Support ID and the name you set: empty on a new install, your macOS login name on installs from before opt-in. They are kept for 30 days and readable only by the maintainers.

### Your controls
Open **Settings → Privacy**:

| Control | What it does |
| --- | --- |
| **Share diagnostics to help fix bugs** | Turns uploads on or off. Off on a new install until you opt in. Off stops uploads at the next cycle. Recording stays local. |
| Name | Change or clear the name sent with uploads. |
| Support ID | The id to quote in a bug report. |
| **Send now** | Uploads what has been recorded so far, once, even with sharing off. |
| **Show in Finder** | Opens the local log folder. |

To turn recording off entirely, set `DIRI_TELEMETRY=off` in diri's environment. Missing or unreadable privacy settings turn uploads off.

Every other network connection diri makes, what it sends and how to turn it off, is listed in [Privacy and network connections](/docs/privacy/). diri does not proxy agent traffic. The full policy is in [PRIVACY.md](https://github.com/cristicretu/diri/blob/main/PRIVACY.md).

## Report a vulnerability
Use [GitHub private vulnerability reporting](https://github.com/cristicretu/diri/security/advisories/new). Do not put exploits, private terminal output, tokens or personal paths in a public issue.

Include the diri and OS versions, a minimal reproduction, the impact you believe is possible, and any suggested fix. You should get an acknowledgement within seven days. Fixes go into the latest release.

In scope: permission-boundary bypasses, unsafe update or IPC behaviour, credential disclosure, session isolation failures and unintended remote execution. A tool doing something you explicitly allowed is not a diri vulnerability.
