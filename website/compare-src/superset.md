---
title: diri vs Superset
description: Both run many terminal agents in parallel; Superset adds an in-app browser, a hosted MCP and paid remote access, while diri is native, Apache-2.0 and uses plain SSH.
competitor: Superset
checked: 2026-10-01
---
Superset is a desktop workspace for running many coding agents in parallel, each in its own git worktree, with a diff viewer, in-app browser, CLI, SDK, hosted MCP server and paid remote access and automations. diri covers much of the same ground as a native app: 21 terminal agents side by side with live status, optional worktrees, a Review panel, and a local MCP server that lets agents start and coordinate other agents. The biggest differences are license, how remote machines are reached, and how far each goes beyond the terminal.

## At a glance
| | diri | Superset |
| --- | --- | --- |
| Platforms | macOS 15 or newer (universal), Linux beta on x86_64 and arm64 Ubuntu, iPhone companion beta | macOS (Apple silicon or Intel), experimental Linux x64 AppImage, iPhone app with Pro; no Windows |
| Price | Free | Free tier; Pro $20/mo, or $15 per user per month billed yearly; Enterprise custom |
| License | Apache-2.0, open source | Elastic License 2.0, source-available (not OSI-approved) |
| App stack | Rust and GPUI | Electron and React |
| Agents | 21 terminal agents plus custom JSON manifests | 21 listed agents; says any CLI agent works without configuration; built-in chat pane too |
| Isolation | Optional git worktree per agent | A git worktree for every workspace |
| Review | Review panel: diff, stage, discard, commit, PR checks and comments | Diff viewer to stage, commit, push and open PRs |
| Remote hosts | Any SSH host; verified helper upload, no tmux, sudo or service install | Remote access through the Superset Relay on Pro; the host runs Superset and must stay awake |
| Agents controlling agents | Local MCP server bundled with the app: spawn agents in worktrees, tracked tasks, wait_any, get_diff, integrate | Hosted MCP server with OAuth: create workspaces, launch agent sessions, tasks, automations |
| Session persistence | Per-session holder processes; agents survive app quit and Engine updates | Sessions survive app restarts with output and scrollback |
| Notes and planning | Notes with to-dos that start agents and receive reports | Tasks through its MCP and CLI; Linear integration on Pro |
| Scheduling | Local schedules in Settings or from an agent; catch up after sleep and can wake the Mac | Automations on Pro |

## Where diri is different

### Plain SSH for remote work
diri reaches a server the way you already do, with your OpenSSH config, keys and `ProxyJump`. It uploads a small helper built for that platform, checks its length, SHA-256 and build ID, and needs no `tmux`, `sudo`, Node.js or service on the host. There is no relay in between, and it is free. diri also tests whether a host keeps sessions alive after you disconnect and labels the result. See [Remote hosts](/docs/remote-hosts/).

### Coordination without an account
diri's MCP server is a local binary inside the app, connected to Claude Code, Codex and Cursor automatically. Agents can fan work out to helpers in separate worktrees, wait for whichever finishes, review each diff and merge it with `integrate`. Writes follow the session tree with depth and child caps, and nothing goes through a hosted service. See [MCP server](/docs/mcp/) and the [security model](/docs/security/).

### Native, and permissive license
diri is a native app written in Rust with GPUI, licensed Apache-2.0, which allows any use including commercial redistribution. Superset's ELv2 lets you fork and self-host for your team but not offer it as a service.

### Status and accounts
diri reads status through Claude Code hooks, Codex turn notifications, and screen rules for every other agent, and flags destructive-looking permission prompts in red. It can also keep several Claude Code or Codex logins and switch every open tab to another account in one click. See [Sessions](/docs/sessions/) and [Accounts and usage](/docs/accounts/).

### Notes that start work
A diri note is a Markdown plan whose to-dos each start an agent with the note as context, show its progress live, and collect its report. See [Notes](/docs/notes/).

## Where Superset is a better fit
- **More around the terminal.** An in-app browser with automatic port detection, setup and teardown scripts per workspace, a built-in chat pane, and a TypeScript SDK. diri has a browser tool for agents, but not a browser pane with port detection.
- **Worktree per task by default.** Every Superset workspace is its own worktree, and you can push and open a PR from the diff viewer. In diri worktrees are opt-in and merging a PR opens GitHub.
- **Automations and team features.** Pro adds automations, Linear and Slack integrations, and unlimited users, with SSO and audit logs on Enterprise. diri's [schedules](/docs/scheduled-tasks/) start an agent with a prompt at a set time on your own Mac while diri is running; diri has no Linear or Slack triggers and no team features.
- **Reach your workspaces from anywhere.** The relay lets you open a workspace on another machine without setting up SSH or a VPN. diri's phone companion uses Tailscale instead.

## Switching
Both apps launch the agent CLIs you already have, on your own subscriptions, so the agents and their logins carry over unchanged.
- Install diri and open your repository: [Quickstart](/docs/quickstart/).
- Superset workspaces are git worktrees. Open their folders in diri or let agents make new ones: [Worktrees and review](/docs/worktrees/).
- Claude Code and Codex conversations already on your machine appear in **Search chats** (⇧⌘H), so you can continue them in diri.
- For a remote box, add it under **Settings → Remote** with the same `ssh` destination you use today.
- Agent pages: [Claude Code](/agents/claude-code/), [Codex](/agents/codex/), [OpenCode](/agents/opencode/), [Gemini](/agents/gemini/), [Amp](/agents/amp/).

## Sources
- [Superset homepage](https://superset.sh/) — checked 2026-10-01
- [Superset pricing](https://superset.sh/pricing) — checked 2026-10-01
- [Superset on GitHub](https://github.com/superset-sh/superset) — checked 2026-10-01
- [Superset docs: overview](https://docs.superset.sh/) — checked 2026-10-01
- [Superset docs: install](https://docs.superset.sh/install) — checked 2026-10-01
- [Superset docs: the Superset model](https://docs.superset.sh/superset-model) — checked 2026-10-01
- [Superset docs: agent integration](https://docs.superset.sh/agent-integration) — checked 2026-10-01
- [Superset docs: remote access](https://docs.superset.sh/remote-access) — checked 2026-10-01
- [Superset docs: MCP server](https://docs.superset.sh/mcp-server) — checked 2026-10-01
