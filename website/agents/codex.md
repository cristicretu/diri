# Run Codex in parallel

> Run several Codex sessions side by side in diri, each in its own git worktree, with live status, alerts when it needs you, and review in one place.

diri runs `codex` exactly as you would in a terminal, with your own install and sign-in. What it adds is everything one terminal window can't give you: several Codex sessions at once, each in its own git worktree, a sidebar that shows which ones are working and which need you, and one place to review what they changed.

## What diri adds for Codex

| | |
| --- | --- |
| Live status | Read from the screen, plus a callback when each turn finishes. |
| Needs-you alerts | A notification and an amber mark when Codex asks a question or wants permission. |
| Own worktree | Optional per session, so parallel runs never edit the same checkout. |
| Review | Diffs, staging, commits and pull request checks beside the session. |
| Survives restarts | Yes. Each session is owned by its own process, so quitting diri never stops it. |
| Resume after exit | Yes, the exact conversation. |
| Fork a conversation | Yes. Branch a conversation to try another approach. |
| Approve from a notification | Yes. Permission prompts get an Approve button on the notification. |
| Starts other agents | Yes. diri's MCP server is added at launch. |

## Set up

1. Install Codex with its official installer. diri can also run this for you: **Settings → Agents → Install** shows the command and runs it only after you confirm.

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

2. Run codex and choose Sign in with ChatGPT or another offered method.
3. Install diri with `brew install --cask cristicretu/diri/diri`, or [download it](https://github.com/cristicretu/diri/releases/latest).
4. Open **New Agent** in the sidebar and choose **Codex**. If you installed Codex while diri was open, click **Refresh** in **Settings → Agents** first.

The [quickstart](/docs/quickstart/) covers the rest: picking a folder, giving a task and answering questions.

## Run several at once

Start more sessions from **New Agent** with a fresh worktree each, or let an agent do it. Codex gets diri's [MCP server](/docs/mcp/) automatically, so you can ask it in plain words:

> Split this into three parts and start a Codex agent for each, in its own worktree. Wait for them, review each diff, and merge the ones that pass the tests.

Each session shows up in the sidebar under the agent that started it. [Worktrees and review](/docs/worktrees/) explains how to bring the work back.

## Questions

### Does diri need its own Codex account?
No. diri starts the `codex` you installed, which uses its own sign-in. diri has no account of its own and never sees your password.

### Can I pick a Codex session up again later?
Yes, the exact conversation. diri records its id at launch and reopens that one. Quitting diri never ends a session in the first place.

### Can Codex run on a server?
Yes. Install and sign in to Codex on any machine you reach with `ssh`, then pick it as the machine in **New Agent**. diri needs no tmux, sudo or service there. See [Remote hosts](/docs/remote-hosts/).

### What does diri cost?
Nothing. diri is free and open source under AGPL-3.0. You pay for Codex as you do today.

## Other agents

[Claude Code](/agents/claude-code/) · [Antigravity](/agents/antigravity/) · [Cursor](/agents/cursor/) · [Gemini](/agents/gemini/) · [Aider](/agents/aider/) · [Amp](/agents/amp/) · [Cline CLI](/agents/cline/) · [Copilot CLI](/agents/copilot/) · [All agents](/agents/)
