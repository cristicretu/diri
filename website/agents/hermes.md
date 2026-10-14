# Run Hermes in parallel

> Run several Hermes sessions side by side in diri, each in its own git worktree, with live status, alerts when it needs you, and review in one place.

diri runs `hermes` exactly as you would in a terminal, with your own install and sign-in. What it adds is everything one terminal window can't give you: several Hermes sessions at once, each in its own git worktree, a sidebar that shows which ones are working and which need you, and one place to review what they changed.

## What diri adds for Hermes

| | |
| --- | --- |
| Live status | Read from the screen with rules written for its interface. |
| Needs-you alerts | A notification and an amber mark when Hermes asks a question or wants permission. |
| Own worktree | Optional per session, so parallel runs never edit the same checkout. |
| Review | Diffs, staging, commits and pull request checks beside the session. |
| Survives restarts | Yes. Each session is owned by its own process, so quitting diri never stops it. |
| Resume after exit | Yes, through the agent's own continue flag. |
| Fork a conversation | Not supported. |
| Approve from a notification | No. Answer permission prompts in the terminal. |
| Starts other agents | No built-in connection. Claude Code, Codex and Cursor can start Hermes sessions for you. |

## Set up

1. Install Hermes with the official Nous Research installer. See the [Hermes install guide](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/getting-started/quickstart.md).

2. Run hermes setup --portal or hermes model to configure a provider.
3. Install diri with `brew install --cask cristicretu/diri/diri`, or [download it](https://github.com/cristicretu/diri/releases/latest).
4. Open **New Agent** in the sidebar and choose **Hermes**. If you installed Hermes while diri was open, click **Refresh** in **Settings → Agents** first.

The [quickstart](/docs/quickstart/) covers the rest: picking a folder, giving a task and answering questions.

## Run several at once

Start more sessions from **New Agent** with a fresh worktree each, or let an agent do it. Claude Code, Codex and Cursor get diri's [MCP server](/docs/mcp/) automatically and can start Hermes sessions (kind `hermes`). Ask one of them:

> Start two Hermes agents in their own worktrees: one fixes the failing tests, the other updates the docs. Tell me when both are done.

Each session shows up in the sidebar under the agent that started it. [Worktrees and review](/docs/worktrees/) explains how to bring the work back.

## Questions

### Does diri need its own Hermes account?
No. diri starts the `hermes` you installed, which uses its own sign-in. diri has no account of its own and never sees your password.

### Can I pick a Hermes session up again later?
Yes, through the agent's own continue flag. It reopens the most recent conversation in that folder, which can be a different one if you also ran the agent there outside diri. Quitting diri never ends a session in the first place.

### Can Hermes run on a server?
Yes. Install and sign in to Hermes on any machine you reach with `ssh`, then pick it as the machine in **New Agent**. diri needs no tmux, sudo or service there. See [Remote hosts](/docs/remote-hosts/).

### What does diri cost?
Nothing. diri is free and open source under AGPL-3.0. You pay for Hermes as you do today.

## Other agents

[Claude Code](/agents/claude-code/) · [Codex](/agents/codex/) · [Antigravity](/agents/antigravity/) · [Cursor](/agents/cursor/) · [Gemini](/agents/gemini/) · [Aider](/agents/aider/) · [Amp](/agents/amp/) · [Cline CLI](/agents/cline/) · [All agents](/agents/)
