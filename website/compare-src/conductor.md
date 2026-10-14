---
title: diri vs Conductor
description: Conductor runs four agents behind a Mac chat interface with a PR flow and paid cloud workspaces; diri runs 21 agent CLIs as real terminals, free and open source.
competitor: Conductor
checked: 2026-10-01
---
Conductor is a Mac app for running Claude Code, Codex, Cursor and OpenCode in parallel, each in its own workspace built on a git worktree, with a chat composer, diff viewer, pull request flow and optional cloud workspaces on paid plans. diri is a free, open-source native app that runs 21 terminal coding agents as real terminals side by side, tracks which ones need you, and lets agents start and coordinate other agents through a local MCP server.

## At a glance
| | diri | Conductor |
| --- | --- | --- |
| Platforms | macOS 15 or newer (universal), Linux beta on x86_64 Ubuntu, iPhone companion beta | Mac; a mobile app is listed on the Pro plan |
| Price | Free | Free plan; Pro $50/mo; Teams $60/mo per user (invite-only); Enterprise custom |
| License | AGPL-3.0, open source | Not documented on its site |
| Agents | 21 terminal agents, including Claude Code, Codex, Cursor, Gemini, Copilot CLI, OpenCode, Amp and Aider, plus custom manifests | Claude Code, Codex, Cursor and OpenCode |
| How agents appear | The agent's own terminal UI | A chat composer with model picker and modes, plus a terminal for ad hoc commands |
| Isolation | Optional git worktree per agent (from an agent, a recipe or the CLI) | A git worktree and branch for every workspace, created by default |
| Review | Review panel: diff against default branch or HEAD, stage, discard, commit, PR checks and comments | Diff viewer with comments, agent review, create PR, Checks tab for CI and review threads, merge |
| Remote hosts | Your own servers over SSH, no tmux, sudo or service install | Conductor Cloud workspaces on Pro and above, running on Conductor-managed infrastructure |
| Agents controlling agents | Built-in local MCP server: spawn agents in worktrees, tracked tasks, wait_any, get_diff, integrate | Conductor API and hosted MCP server (beta) that manage cloud workspaces |
| Session persistence | Per-session holder processes; agents keep running when the app quits or the Engine updates | Archived workspaces restore with chat history; behavior on app quit not documented |
| Notes and planning | Notes with to-dos that start agents and receive reports | A per-workspace `.context` folder for notes and handoffs; plan mode |
| Scheduling | Schedules that start an agent with a prompt at a set time, catch up after sleep and can wake the Mac | Routines, listed in its changelog |
| Account | No diri account; bring your own CLIs and logins | Bring your own subscriptions and keys; its hosted MCP server uses a Conductor sign-in |

## Where diri is different

### Real terminals, many agents
diri does not put a chat layer between you and the agent. Each session is the agent CLI itself in a terminal, so every slash command, permission prompt and TUI feature works as the agent's authors intended. That is also why diri can support 21 agents, from Claude Code and Codex to Aider, Amp, Copilot CLI and Kiro, and why you can add one with a JSON manifest. See [Supported agents](/docs/agents/).

### Sessions outlive the window
Each session is owned by its own holder process. Quit diri, or let it update its Engine, and agents keep working with their full screen history. Idle sessions can hibernate to save memory and wake where they were. See [Sessions](/docs/sessions/#persistence).

### Remote on your own machines
diri runs agents on any server you already reach with `ssh`. It uploads a small, verified helper and needs no `tmux`, `sudo`, Node.js or installed service. It also tests whether sessions survive a disconnect on that host and tells you. Your code stays on hardware you control. See [Remote hosts](/docs/remote-hosts/).

### Agents that run other agents, locally
Claude Code, Codex and Cursor get diri's MCP server automatically. A lead agent can start helpers in their own worktrees, assign tracked tasks with structured results, wait for whichever finishes first, read each diff and merge the good ones. Writes follow the session tree, with caps on depth and number of children. It works on the free app with no account. See [MCP server](/docs/mcp/).

### Notes that start work
A diri note is a Markdown plan whose to-dos can each start an agent with the note as context. Progress shows live under the to-do, and the agent writes its findings back. See [Notes](/docs/notes/).

## Where Conductor is a better fit
- **A guided, opinionated workflow.** Every workspace gets a worktree and branch by default, and the path from issue to pull request to merge is built in: create a workspace from a branch, PR, GitHub issue or Linear issue, review in the diff viewer, open the PR, and watch CI and review threads in the Checks tab. In diri, worktrees are opt-in and merging a PR opens GitHub.
- **A chat-first interface.** If you prefer a composer with a model picker, plan and fast modes, and attachments over a raw terminal, Conductor is built around that.
- **Cloud workspaces and collaboration.** Pro adds Conductor Cloud workspaces, multiplayer with live collaboration, an API and a mobile app. diri has no hosted compute and no multi-user features.

## Switching
diri runs the same agent CLIs with the same logins you already use, so there is nothing to migrate for the agents themselves.
- Install diri and point it at your repository: [Quickstart](/docs/quickstart/).
- Your existing worktrees are ordinary git checkouts. Open their folders in diri, or let agents create new ones: [Worktrees and review](/docs/worktrees/).
- Claude Code and Codex conversations on your machine show up in **Search chats** (⇧⌘H), including ones started elsewhere, so you can continue them in a diri session.
- Agent pages: [Claude Code](/agents/claude-code/), [Codex](/agents/codex/), [Cursor](/agents/cursor/), [OpenCode](/agents/opencode/).

## Sources
- [Conductor homepage](https://www.conductor.build/) — checked 2026-10-01
- [Conductor pricing](https://www.conductor.build/pricing) — checked 2026-10-01
- [Conductor docs: introduction](https://www.conductor.build/docs/) — checked 2026-10-01
- [Conductor docs: isolated workspaces](https://www.conductor.build/docs/concepts/workspaces-and-branches) — checked 2026-10-01
- [Conductor docs: workflow](https://www.conductor.build/docs/concepts/workflow) — checked 2026-10-01
- [Conductor docs: agent modes](https://www.conductor.build/docs/concepts/agent-modes) — checked 2026-10-01
- [Conductor docs: checks](https://www.conductor.build/docs/reference/checks) — checked 2026-10-01
- [Conductor docs: API](https://www.conductor.build/docs/api) — checked 2026-10-01
- [Conductor docs: MCP server](https://www.conductor.build/docs/api/mcp) — checked 2026-10-01
- [Conductor docs: privacy](https://www.conductor.build/docs/reference/privacy) — checked 2026-10-01
- [Conductor changelog](https://www.conductor.build/changelog) — checked 2026-10-01
