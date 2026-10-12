---
title: Sessions
description: How diri sessions work: status colors, notifications, the sidebar, searching and resuming past chats, persistence across quits, hibernation and terminal features.
---
A session is one agent, terminal or note running in diri. Each gets a row in the sidebar and its own terminal. Sessions run in background processes, so they keep going when you close the window or quit the app.

## Status
The mark on the left of a row shows what the session is doing; the agent's logo is on the right.

| Mark | Meaning |
| --- | --- |
| Spinner | Working |
| Amber warning sign | Needs you: a question or a permission prompt. It turns red when the prompt looks destructive |
| Green check | Done, and you have not looked yet |
| No mark | Idle, or done and already seen |
| Moon | Hibernated to save memory; opening it wakes it |

Claude Code reports its status through hooks, which is the most reliable signal. Codex also reports each finished turn. For every other agent, diri reads the status from its screen. Every agent still runs as a normal terminal.

Any program can also report its own status with [OSC 7501](https://www.superlogical.com/rex/docs/build/program-status), in an agent tab or a plain terminal tab. While it reports, diri shows what it says instead of what it reads from the screen: working (with progress), waiting on you, or done. This works in remote tabs too, and an agent that reports this way is believed over its own hooks. Its reports end when it exits or its shell returns to the prompt.

Right-click a row to **Mark as Read** or **Mark as Unread**.

## Notifications
When a session needs you or finishes, diri sends a macOS notification.
- **Reply.** For a plain question, answer straight from the notification banner.
- **Inbox.** <kbd>⇧</kbd><kbd>⌘</kbd><kbd>I</kbd> opens the bell, which collects every "needs you" and "finished" moment. The Dock icon shows the unread count.
- **Jump.** <kbd>⇧</kbd><kbd>⌘</kbd><kbd>J</kbd> selects the next session that needs you.
- **Sounds.** **Settings → General → Gentle status chimes** plays quiet cues for input, completion and memory pauses.

## The sidebar
Sessions are grouped by project folder. Sessions started by another agent nest under it; selecting a session highlights its parent and children (turn off **Highlight parent and children** in Settings → General).

| Task | How |
| --- | --- |
| Rename | Double-click the row, <kbd>⌘</kbd><kbd>R</kbd>, or **Rename…** |
| Pin | **Pin Session** in the row's menu |
| Reorder | Drag between rows, or <kbd>⌃</kbd><kbd>⌘</kbd><kbd>↑</kbd> / <kbd>↓</kbd> |
| Reorder projects | Drag the project header |
| Hand off work | Drop a session onto another session's row |
| Start a sibling with the same prompt | Drop a session on the zone below the last project |
| Archive | **Archive Session**, or <kbd>⌥</kbd><kbd>⇧</kbd><kbd>⌘</kbd><kbd>W</kbd> |
| Remove | **Remove from Sidebar**, or <kbd>⌘</kbd><kbd>W</kbd> |
| Close a project | **Close All Sessions** on the project |

Escape cancels a drag. Select several rows to act on them together. <kbd>⇧</kbd><kbd>⌘</kbd><kbd>S</kbd> moves tabs from the sidebar to the top of the window and back.

### Archive, remove and reopen
- **Archive Session** stops the agent and keeps the session in an **Archived** group under its project. Right-click it and choose **Revive** to continue the conversation. For an agent that cannot resume, the menu item reads **Archive (won't be resumable)**.
- **Remove from Sidebar** stops the agent and removes the session and its screen history. With **Confirm before closing a session** on, diri asks first if a process is still running.
- <kbd>⇧</kbd><kbd>⌘</kbd><kbd>T</kbd> reopens the most recently closed session.

Neither touches your files.

## Search past chats
Press <kbd>⇧</kbd><kbd>⌘</kbd><kbd>H</kbd> for **Search chats**. It lists Claude Code and Codex conversations on this machine, including ones started outside diri. Type to filter and press Return to continue that conversation in a new session.

## Resume and fork
After your Mac restarts, sessions whose agent stopped offer **Resume**. Resuming continues the same conversation for agents that support it. A local terminal shows **Restart** instead and starts a fresh shell in the same folder. Both are also in the row's menu.

Forking copies a conversation into a new session so you can try another approach. It works for Claude Code and Codex, from an agent's `fork_agent` tool or the CLI:

```sh
dirijor session fork <session>
```

## Persistence
Each session is owned by its own holder process, not by the window. The background Engine keeps the record of every session.

| Event | What happens |
| --- | --- |
| Close the window or quit diri | Agents keep running with their full screen history |
| diri updates its Engine | Sessions are picked up again where they were |
| Mac restarts | Agents stop; sessions offer **Resume** |

## Hibernation
Idle agents still use memory. In **Settings → Resources**, **Hibernate idle sessions** freezes a session after it has had no output or CPU activity for a set time (1 hour by default, or off), and **Memory limit** freezes an idle session that grows past a size you choose. Frozen sessions show a moon and are never killed; opening one wakes it exactly where it was.

## Terminal features
| Feature | How |
| --- | --- |
| Find, including scrollback | <kbd>⌘</kbd><kbd>F</kbd>, then <kbd>⌘</kbd><kbd>G</kbd> / <kbd>⇧</kbd><kbd>⌘</kbd><kbd>G</kbd> |
| Insert a file path | <kbd>⌘</kbd><kbd>E</kbd> opens a picker under the session's folder |
| Drop files | Drop files from Finder on the terminal to paste their escaped paths |
| Paste images | <kbd>⌘</kbd><kbd>V</kbd> |
| Quote a selection into your reply | <kbd>⇧</kbd><kbd>⌘</kbd><kbd>C</kbd> |
| Open scrollback in your editor | <kbd>⇧</kbd><kbd>⌘</kbd><kbd>E</kbd> |
| Jump between shell prompts | <kbd>⇧</kbd><kbd>⌘</kbd><kbd>↑</kbd> / <kbd>↓</kbd> |
| Auxiliary terminal | <kbd>⌘</kbd><kbd>J</kbd> opens a shell below the session |
| Split panes | <kbd>⌘</kbd><kbd>D</kbd> splits right, <kbd>⌥</kbd><kbd>⇧</kbd><kbd>⌘</kbd><kbd>D</kbd> splits below |

Links in the output open with a click, and `file:line` links open in your editor at that line (set it in **Settings → Appearance → Open file links in**). Clipboard writes from programs over OSC 52, such as Codex copying text, reach your clipboard. Themes, terminal font and line height are in **Settings → Appearance**.

See [Keyboard shortcuts](/docs/keyboard-shortcuts/) for every binding and [Never lose your work](/guides/never-lose-work/) for a beginner's view of persistence.
