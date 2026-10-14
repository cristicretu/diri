# diri vs herdr

> herdr is a cross-platform terminal multiplexer for agents with a socket API; diri is a native GUI with review, notes and MCP tasks, and sessions that survive Engine restarts.

herdr is an open-source terminal multiplexer for coding agents: one Rust binary with a TUI, a background server that keeps agent terminals alive when you detach, status detection for over 20 agent CLIs, SSH machines in the same window, plugins, and a socket API agents can drive. diri is an open-source native desktop app with the same core idea, agents that keep running and tell you when they need you, plus a Review panel, notes, account switching and an MCP server for agent-to-agent tasks. Both are open source (diri AGPL-3.0, herdr Apache-2.0) and written in Rust; the main split is TUI versus GUI.

## At a glance
| | diri | herdr |
| --- | --- | --- |
| Interface | Native desktop app (GPUI) | TUI in any terminal, keyboard and mouse |
| Platforms | macOS 15 or newer (universal), Linux beta on x86_64 Ubuntu, iPhone companion beta | macOS, Linux, Windows |
| License | AGPL-3.0 | Apache-2.0 |
| Agents | 20 agents by manifest, plus custom manifests | Over 20 agent CLIs detected by screen manifests, plus agents that self-report their state |
| Status | Hooks for Claude Code, turn notify for Codex, screen rules for the rest; working, needs you, done | Screen reading, replaced by integration reports where available; working, blocked, idle |
| Isolation | Optional git worktree per agent | Worktree methods in the socket API open checkouts as workspaces |
| Review | Review panel: diff, stage, discard, commit, PR checks and comments | Not documented |
| Remote hosts | SSH; uploads a verified per-session helper, no service install | SSH; herdr package and background server on the remote, installable during setup |
| Agents controlling agents | MCP server: spawn agents in worktrees, tracked tasks with results, wait_any, get_diff, integrate | CLI and socket API: start, prompt and wait on agents, split panes, subscribe to events; agent skill files |
| Persistence | Holder per session; agents survive app quit and Engine restarts and updates | Processes survive detach; after a server restart, layout returns and supported agents resume, other processes do not |
| Extensibility | Custom agent manifests | Agent manifests, plugins and a marketplace |
| Notes and planning | Notes with to-dos that start agents and receive reports | Not documented |

## Where diri is different

### The Engine can restart without killing agents
diri splits the background Engine from the terminals. Each session has its own holder process, so when the Engine restarts or updates, every agent keeps running with its screen intact and the Engine picks it up again. herdr documents that when its server restarts, running pane processes are gone and come back as fresh shells or resumed agent conversations. Both lose processes on a machine reboot. See [Sessions](/docs/sessions/#persistence).

### Review and bring work back
diri's Review panel shows each session's changes against the default branch or HEAD, with stage, discard, commit, and pull request checks and comments. A lead agent can merge a helper's branch into its own checkout with `integrate`, which leaves everything unchanged on conflict. See [Worktrees and review](/docs/worktrees/).

### Tasks, not just prompts
herdr's API can prompt an agent and wait until it is blocked or idle. diri adds tracked tasks: a task completes only when the helper calls `report_task`, can be held to a JSON schema, and is delivered at most once even when the caller retries. Write permissions follow the session tree. See [MCP server](/docs/mcp/).

### Remote without a remote server
diri installs no service or shared background server on the host. Each remote session gets one small helper process that owns its terminal, uploaded and checked by length, SHA-256 and build ID. diri also tests whether the host keeps sessions alive after logout and reports native detach, user supervisor or non-persistent. See [Remote hosts](/docs/remote-hosts/).

### A GUI for the rest of the work
Notes whose to-dos start agents, a notification inbox with reply from the banner, Claude Code and Codex account switching across every open tab, and usage estimates. See [Notes](/docs/notes/) and [Accounts and usage](/docs/accounts/).

## Where herdr is a better fit
- **Windows, and anywhere a terminal runs.** herdr runs on macOS, Linux and Windows and lives inside the terminal you already use. diri needs macOS, or the Linux beta on x86_64 Ubuntu, and a display.
- **Multiplexer model.** If you think in tmux-style sessions, panes and tabs with detach and reattach, herdr gives you that model with agent awareness on top.
- **Plugins.** herdr has executable workflow plugins with event hooks and a marketplace to share them. diri's extension point is the agent manifest.
- **Broader agent detection.** herdr's list includes agents diri has no manifest for, such as Qwen Code and Letta Code, and supports agents that report their own state.

## Switching
diri launches the same agent CLIs on the same logins, so the agents carry over unchanged. Your herdr worktrees are ordinary git checkouts you can open as folders in diri.
- Install diri: [Quickstart](/docs/quickstart/).
- Bring your herdr sessions over: open **Settings → General → Import** and click **Import…**, or use the import link on diri's welcome screen. Every pane herdr would restore becomes a diri session in the same folder. A pane whose agent reported its conversation id resumes that exact conversation, a pane that only knows its agent starts it fresh, and a plain pane becomes a terminal. diri only reads herdr's files and never changes them.
- Add the SSH machines you use with herdr under **Settings → Remote**: [Remote hosts](/docs/remote-hosts/).
- Scripts against herdr's socket map to the [dirijor CLI](/docs/cli/); agent-side automation maps to the [MCP tools](/docs/mcp-tools/).
- Agent pages: [Claude Code](/agents/claude-code/), [Codex](/agents/codex/), [Cursor](/agents/cursor/), [OpenCode](/agents/opencode/), [Amp](/agents/amp/).

## Sources
- [herdr homepage](https://herdr.dev/) — checked 2026-10-01
- [herdr on GitHub](https://github.com/ogulcancelik/herdr) — checked 2026-10-01
- [herdr docs](https://herdr.dev/docs) — checked 2026-10-01
- [herdr docs: agents](https://herdr.dev/docs/agents/) — checked 2026-10-01
- [herdr docs: session state](https://herdr.dev/docs/session-state/) — checked 2026-10-01
- [herdr docs: connecting machines](https://herdr.dev/docs/connecting-machines/) — checked 2026-10-01
- [herdr docs: socket API](https://herdr.dev/docs/socket-api/) — checked 2026-10-01
