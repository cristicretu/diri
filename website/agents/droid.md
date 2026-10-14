# Run Droid in parallel

> Run several Droid sessions side by side in diri, each in its own git worktree, with live status, alerts when it needs you, and review in one place.

diri runs `droid` exactly as you would in a terminal, with your own install and sign-in. What it adds is everything one terminal window can't give you: several Droid sessions at once, each in its own git worktree, a sidebar that shows which ones are working and which need you, and one place to review what they changed.

## What diri adds for Droid

| | |
| --- | --- |
| Live status | Read from the screen with rules written for its interface. |
| Needs-you alerts | A notification and an amber mark when Droid asks a question or wants permission. |
| Own worktree | Optional per session, so parallel runs never edit the same checkout. |
| Review | Diffs, staging, commits and pull request checks beside the session. |
| Survives restarts | Yes. Each session is owned by its own process, so quitting diri never stops it. |
| Resume after exit | Yes, through the agent's own continue flag. |
| Fork a conversation | Not supported. |
| Approve from a notification | Yes. Permission prompts get an Approve button on the notification. |
| Starts other agents | No built-in connection. Claude Code, Codex and Cursor can start Droid sessions for you. |

## Set up

1. Install Droid with Factory's installer or a supported package manager. See the [Droid install guide](https://docs.factory.ai/cli/getting-started/quickstart).

2. Run droid, then use /login if prompted.
3. Install diri with `brew install --cask cristicretu/diri/diri`, or [download it](https://github.com/cristicretu/diri/releases/latest).
4. Open **New Agent** in the sidebar and choose **Droid**. If you installed Droid while diri was open, click **Refresh** in **Settings → Agents** first.

The [quickstart](/docs/quickstart/) covers the rest: picking a folder, giving a task and answering questions.

## Run several at once

Start more sessions from **New Agent** with a fresh worktree each, or let an agent do it. Claude Code, Codex and Cursor get diri's [MCP server](/docs/mcp/) automatically and can start Droid sessions (kind `droid`). Ask one of them:

> Start two Droid agents in their own worktrees: one fixes the failing tests, the other updates the docs. Tell me when both are done.

Each session shows up in the sidebar under the agent that started it. [Worktrees and review](/docs/worktrees/) explains how to bring the work back.

## Questions

### Does diri need its own Droid account?
No. diri starts the `droid` you installed, which uses its own sign-in. diri has no account of its own and never sees your password.

### Can I pick a Droid session up again later?
Yes, through the agent's own continue flag. It reopens the most recent conversation in that folder, which can be a different one if you also ran the agent there outside diri. Quitting diri never ends a session in the first place.

### Can Droid run on a server?
Yes. Install and sign in to Droid on any machine you reach with `ssh`, then pick it as the machine in **New Agent**. diri needs no tmux, sudo or service there. See [Remote hosts](/docs/remote-hosts/).

### What does diri cost?
Nothing. diri is free and open source under AGPL-3.0. You pay for Droid as you do today.

## Other agents

[Claude Code](/agents/claude-code/) · [Codex](/agents/codex/) · [Antigravity](/agents/antigravity/) · [Cursor](/agents/cursor/) · [Gemini](/agents/gemini/) · [Aider](/agents/aider/) · [Amp](/agents/amp/) · [Cline CLI](/agents/cline/) · [All agents](/agents/)
