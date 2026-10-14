# Run Maki in parallel

> Run several Maki sessions side by side in diri, each in its own git worktree, with live status, alerts when it needs you, and review in one place.

diri runs `maki` exactly as you would in a terminal, with your own install and sign-in. What it adds is everything one terminal window can't give you: several Maki sessions at once, each in its own git worktree, a sidebar that shows which ones are working and which need you, and one place to review what they changed.

## What diri adds for Maki

| | |
| --- | --- |
| Live status | Read from the screen with rules written for its interface. |
| Needs-you alerts | A notification and an amber mark when Maki asks a question or wants permission. |
| Own worktree | Optional per session, so parallel runs never edit the same checkout. |
| Review | Diffs, staging, commits and pull request checks beside the session. |
| Survives restarts | Yes. Each session is owned by its own process, so quitting diri never stops it. |
| Resume after exit | Yes, through the agent's own continue flag. |
| Fork a conversation | Not supported. |
| Approve from a notification | Yes. Permission prompts get an Approve button on the notification. |
| Starts other agents | No built-in connection. Claude Code, Codex and Cursor can start Maki sessions for you. |

## Set up

1. Follow Maki's official installation guide and add maki to PATH. See the [Maki install guide](https://github.com/tontinton/maki#installation).

2. Configure a supported provider API key; OpenAI users can run maki auth login openai.
3. Install diri with `brew install --cask cristicretu/diri/diri`, or [download it](https://github.com/cristicretu/diri/releases/latest).
4. Open **New Agent** in the sidebar and choose **Maki**. If you installed Maki while diri was open, click **Refresh** in **Settings → Agents** first.

The [quickstart](/docs/quickstart/) covers the rest: picking a folder, giving a task and answering questions.

## Run several at once

Start more sessions from **New Agent** with a fresh worktree each, or let an agent do it. Claude Code, Codex and Cursor get diri's [MCP server](/docs/mcp/) automatically and can start Maki sessions (kind `maki`). Ask one of them:

> Start two Maki agents in their own worktrees: one fixes the failing tests, the other updates the docs. Tell me when both are done.

Each session shows up in the sidebar under the agent that started it. [Worktrees and review](/docs/worktrees/) explains how to bring the work back.

## Questions

### Does diri need its own Maki account?
No. diri starts the `maki` you installed, which uses its own sign-in. diri has no account of its own and never sees your password.

### Can I pick a Maki session up again later?
Yes, through the agent's own continue flag. It reopens the most recent conversation in that folder, which can be a different one if you also ran the agent there outside diri. Quitting diri never ends a session in the first place.

### Can Maki run on a server?
Yes. Install and sign in to Maki on any machine you reach with `ssh`, then pick it as the machine in **New Agent**. diri needs no tmux, sudo or service there. See [Remote hosts](/docs/remote-hosts/).

### What does diri cost?
Nothing. diri is free and open source under AGPL-3.0. You pay for Maki as you do today.

## Other agents

[Claude Code](/agents/claude-code/) · [Codex](/agents/codex/) · [Antigravity](/agents/antigravity/) · [Cursor](/agents/cursor/) · [Gemini](/agents/gemini/) · [Aider](/agents/aider/) · [Amp](/agents/amp/) · [Cline CLI](/agents/cline/) · [All agents](/agents/)
