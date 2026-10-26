# Quickstart

> Go from a fresh install to a running agent in diri: install an agent, pick a folder, start a session, read its status, and answer when it asks.

This page takes you from a fresh install to a working agent in a few minutes. If you have never used a coding agent before, the slower walkthrough in [Your first agent](/guides/first-agent/) explains each step.

## 1. Install diri
Install with Homebrew or the DMG. You need macOS 15 or newer. See [Install](/docs/install/) for Linux and details.

```sh
brew install --cask diri
```

## 2. Add an agent
diri runs the agent CLIs already on your machine. On first launch it checks which ones are installed.
- **If none are found**, the welcome screen says "Install a coding agent to get started." and lists a few agents with an **Install** button.
- **Click Install.** diri shows the agent's official install command in a confirmation sheet. Click **Install** again and it runs in a new terminal tab you can watch.
- **Sign in once.** Start the agent and follow its own sign-in steps. diri never handles agent passwords.

The one-click install covers Claude Code, Codex, Gemini CLI and OpenCode. **More agents…** opens **Settings → Agents**, which lists every supported agent. If you install an agent while diri is open, click **Refresh** there. If one still is not detected, use **Add…** on its row to choose the executable.

> [!TIP]
> Claude Code and Codex have the deepest status and resume support. Every other agent still runs in a real terminal.

## 3. Pick a folder
An agent works inside one folder and can change files there.
- On the welcome screen, click **Choose a folder…**.
- Or open **New Agent** at the top of the sidebar, click the folder row at the bottom (the **Where** panel), and choose a recent folder or **Choose Folder…**.

diri remembers every folder you use, so next time it is one click. A folder that is a git repository can also give each agent its own worktree; see [Worktrees and review](/docs/worktrees/).

## 4. Start a session
| Way to start | What it does |
| --- | --- |
| **New Agent** in the sidebar | Pick an agent from the ones installed |
| <kbd>⌘</kbd><kbd>T</kbd> | Start the default agent right away |
| <kbd>⌘</kbd><kbd>N</kbd> | Open the launcher: pick project and agent, type the first prompt |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>T</kbd> | Start a plain terminal |

Set the agent ⌘T uses in **Settings → General → Default agent**. The new session opens on the right with the agent's own screen, and a row appears in the sidebar. Type your task and press Return.

## 5. Read the sidebar
The mark on the left of each row shows what the session is doing. The agent's logo is on the right.

| Mark | Meaning |
| --- | --- |
| Spinning | Working. Leave it alone. |
| Amber | Needs you: a question or a permission prompt |
| Green | Done, and you have not looked yet |
| Grey | Idle, or done and already seen |
| Moon | Hibernated to save memory. Open it to wake it. |

[Sessions](/docs/sessions/) covers the full status model.

## 6. Answer when it asks
When an agent needs you, its row turns amber and diri sends a notification.
1. Press <kbd>⇧</kbd><kbd>⌘</kbd><kbd>J</kbd> to jump to the next session that needs you, or click the row.
2. Answer in the agent's screen. Permission prompts usually take the arrow keys and Return.
3. For a plain question, you can reply from the macOS notification with **Reply**.

The bell (<kbd>⇧</kbd><kbd>⌘</kbd><kbd>I</kbd>) collects every "needs you" and "finished" moment.

## 7. Quit without losing anything
Each session runs in its own background process, not inside the window. Quit diri (<kbd>⌘</kbd><kbd>Q</kbd>) and reopen it later: agents that were working are still working, with their full screen history. The same holds when diri updates its background Engine.

A restart of your Mac does stop agents. Their sessions then offer **Resume**, which continues the same conversation for agents that support it. [Never lose your work](/guides/never-lose-work/) explains what survives and what does not.

## Next steps
- [Sessions](/docs/sessions/): status, sidebar, history, resume and terminal features.
- [Worktrees and review](/docs/worktrees/): parallel agents on separate branches, and reviewing their changes.
- [Keyboard shortcuts](/docs/keyboard-shortcuts/): every binding.
- [Run several agents at once](/guides/parallel-agents/) and [Let agents work as a team](/guides/agent-teams/).
