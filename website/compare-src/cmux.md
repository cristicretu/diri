---
title: diri vs cmux
description: cmux is a free, scriptable Ghostty-based Mac terminal for agents; diri adds agent status, worktrees, review and agent-to-agent tasks, with sessions that outlive the app.
competitor: cmux
checked: 2026-10-01
---
cmux is a free, open-source macOS terminal built on libghostty, with vertical tabs, split panes, a scriptable browser, notification rings when an agent needs you, and a CLI and socket API for everything. diri is a free, open-source native app built specifically around coding agents: 21 agent CLIs with live status, optional git worktrees, a Review panel, notes, and an MCP server that lets agents start and coordinate other agents. Both are native Mac apps that run agents in real terminals; cmux is a general terminal first, diri is an agent manager first.

## At a glance
| | diri | cmux |
| --- | --- | --- |
| Platforms | macOS 15 or newer (universal), Linux beta on x86_64 Ubuntu, iPhone companion beta | macOS only for now; iOS app in TestFlight beta |
| Price | Free | Free; a Founder's Edition offers early access features |
| License | AGPL-3.0 | GPL-3.0-or-later for the client; server components under BSL 1.1 |
| Built with | Rust and GPUI | Swift and AppKit on libghostty |
| Agents | 21 agents with manifests for launch, resume and status, plus any shell | Any agent that runs in a terminal |
| Status and attention | Working, needs you, done per session, from hooks or screen rules; notifications and an inbox | Notification rings and unread badges, triggered by OSC 9/99/777, the CLI or agent hooks |
| Isolation | Optional git worktree per agent | Not documented |
| Review | Review panel: diff, stage, discard, commit, PR checks and comments | Sidebar shows git branch and PR status; no diff panel documented |
| Remote hosts | SSH hosts with a per-session helper; no tmux, sudo or service install | `cmux ssh` workspaces, with browser panes routed through the remote network; can attach to remote tmux sessions |
| Agents controlling agents | MCP server: spawn agents in worktrees, tracked tasks, wait_any, get_diff, integrate | CLI and socket API: create workspaces, split panes, send input, read screens; Claude Code teammates as native splits |
| Session persistence | Holder process per session; agents keep running when the app quits | Restores layout, directories and best-effort scrollback on relaunch; does not keep live processes, supported agents can resume via hooks |
| Notes and planning | Notes with to-dos that start agents and receive reports | Not documented |

## Where diri is different

### Agents keep running when you quit
In diri each session lives in its own holder process, not in the window. Quit the app or update the Engine and every agent keeps working with its full screen. cmux restores your layout and scrollback on relaunch, and resumes supported agents from saved session ids, but says it does not checkpoint live processes. See [Sessions](/docs/sessions/#persistence).

### It knows what each agent is doing
diri ships a manifest for each of 21 agents describing how to launch, resume and fork it and how to read its state. The sidebar shows working, needs you and done for every session, turns red on destructive-looking prompts, and ⇧⌘J jumps to the next one waiting. See [Supported agents](/docs/agents/).

### Work isolation and review built in
Agents can each get a git worktree, and the Review panel shows what a session changed against the default branch or HEAD, with stage, discard, commit, and PR checks and comments. See [Worktrees and review](/docs/worktrees/).

### Orchestration with tracked results
cmux's socket API drives terminals: panes, keystrokes, screen reads. diri's MCP server works one level up. A lead agent assigns tasks that complete only when the helper reports back, optionally against a JSON schema, waits on whichever finishes first, reads the helper's final message from its transcript, and merges its branch. See [MCP server](/docs/mcp/).

### Remote without tmux
diri's remote sessions use a small verified helper per session instead of `tmux`, and test whether the host keeps sessions alive after a disconnect. See [Remote hosts](/docs/remote-hosts/).

## Where cmux is a better fit
- **A general-purpose terminal.** If you want one terminal for everything, with agents as one use among many, cmux is a full Ghostty-based terminal with vertical and horizontal tabs and splits.
- **A scriptable browser next to the terminal.** cmux splits a real browser pane beside your shell and lets scripts navigate, snapshot the DOM, click, type and evaluate JavaScript. diri gives agents a browser tool, not a browser pane.
- **Agent-agnostic by design.** cmux needs no per-agent support: anything that prints a standard notification escape sequence lights up. diri reads status through per-agent manifests, so a brand-new agent needs a manifest for accurate status.
- **Fits a tmux workflow.** If you already keep work in tmux on remote boxes, cmux can attach to those sessions directly.

## Switching
diri runs the same agent CLIs with your existing logins, and nothing in your repositories changes.
- Install diri and open a folder: [Quickstart](/docs/quickstart/).
- For agents cmux would show as plain terminals, check whether diri has a manifest: [Supported agents](/docs/agents/). If not, a short JSON file adds one.
- Replace `cmux ssh` hosts with the same destinations under **Settings → Remote**: [Remote hosts](/docs/remote-hosts/).
- Scripts written against cmux's socket can move to the [dirijor CLI](/docs/cli/) or, from inside agents, the [MCP tools](/docs/mcp-tools/).
- Agent pages: [Claude Code](/agents/claude-code/), [Codex](/agents/codex/), [Gemini](/agents/gemini/), [OpenCode](/agents/opencode/).

## Sources
- [cmux homepage](https://cmux.com/) — checked 2026-10-01
- [cmux on GitHub](https://github.com/manaflow-ai/cmux) — checked 2026-10-01
- [cmux README](https://github.com/manaflow-ai/cmux/blob/main/README.md) — checked 2026-10-01
