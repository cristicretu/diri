---
title: Keyboard shortcuts
description: Every diri keyboard shortcut grouped by task, plus which surface wins a contested key, how to rebind, and the Linux equivalents.
---
diri is keyboard-first. This page groups every default binding by what you are trying to do. Unless a row says otherwise, a shortcut works whenever the main window is focused, including while you type in a terminal.

Modifiers use the macOS glyphs: ⌘ Command, ⇧ Shift, ⌥ Option, ⌃ Control. For a gentler introduction to the handful worth learning first, read [Shortcuts worth learning](/guides/shortcuts/).

## Change a shortcut
Open **Settings → Shortcuts** to see every command with its description and rebind or clear it. Custom bindings take precedence over the defaults below, and the native menus, the command palette and the hold-⌘ hints all follow them.

### Linux
On Linux, ⌘ becomes Ctrl and ⌃⌘ becomes Ctrl+Shift. A few bindings move to avoid collisions:

| Command | macOS | Linux |
| --- | --- | --- |
| Toggle inspector | ⌥⌘I | Ctrl+Alt+I |
| Previous / next session | ⇧⌘[ / ⇧⌘] | Ctrl+Alt+← / → |
| Delegate session | ⌃⌘D | Ctrl+Alt+D |
| To-dos | ⌃⌘T | Ctrl+Alt+Shift+T |
| Focus pane left / right / up / down | ⌥⌘ + arrow | Ctrl+Alt+H / L / K / J |
| Previous / next pane | ⌘[ / ⌘] | palette only |
| Hide diri | ⌘H | none |

## See shortcuts in place
Hold ⌘ on its own for a moment (700 ms) and the ⌘ shortcuts appear on the controls they operate: ⌘1 … ⌘9 on session rows and tabs, ⌘T on New Agent and the new-tab button, ⌘B on the sidebar toggle, ⌥⌘I on the inspector toggle, ⇧⌘H on search, and ⌘W under the selected tab's close button. Release ⌘ and they fade out.

Pressing another key or clicking while ⌘ is down counts as a shortcut, so the labels never flash during ⌘C or ⌘T. A command rebound away from ⌘ shows nothing. With Reduce Motion they appear and disappear without a fade.

## Sessions
| Shortcut | Action |
| --- | --- |
| <kbd>⌘</kbd><kbd>N</kbd> | Show or hide the launcher: pick a project and agent, then type the first prompt |
| <kbd>⌘</kbd><kbd>T</kbd> | Start a session with the default agent, no launcher |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>T</kbd> | Start a plain terminal session |
| <kbd>⌥</kbd><kbd>⇧</kbd><kbd>⌘</kbd><kbd>N</kbd> | Start a Codex session |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>N</kbd> | New note in the current project |
| <kbd>⌘</kbd><kbd>R</kbd> | Rename the selected session in place |
| <kbd>⌃</kbd><kbd>⌘</kbd><kbd>D</kbd> | Delegate: mark the selected session as a handoff source; select a target and press again to review and send |
| <kbd>⌥</kbd><kbd>⇧</kbd><kbd>⌘</kbd><kbd>W</kbd> | Archive the selected session |
| <kbd>⌘</kbd><kbd>W</kbd> | Close the focused terminal pane; otherwise close the selected session, or the window when none is selected |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>T</kbd> | Reopen the most recently closed session |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>N</kbd> | New window for the current workspace |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>W</kbd> | Close the window; sessions keep running |
| <kbd>⌘</kbd><kbd>Q</kbd> | Quit diri; the Engine keeps sessions alive |
| <kbd>⌘</kbd><kbd>H</kbd> | Hide diri |

Set the default agent for ⌘T in **Settings → General → Default agent**.

## Move between sessions
| Shortcut | Action |
| --- | --- |
| <kbd>⌘</kbd><kbd>1</kbd> … <kbd>⌘</kbd><kbd>8</kbd> | Select the nth session; hold ⌘ to see each row's number |
| <kbd>⌘</kbd><kbd>9</kbd> | Select the last session |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>[</kbd> / <kbd>⇧</kbd><kbd>⌘</kbd><kbd>]</kbd> | Previous / next session in sidebar order, wrapping |
| <kbd>⌘</kbd><kbd>[</kbd> / <kbd>⌘</kbd><kbd>]</kbd>, <kbd>⌥</kbd><kbd>⌘</kbd> + arrow | In a tab with one pane: previous / next session, as before. In a split tab they move between panes |
| <kbd>⌃</kbd><kbd>⌘</kbd><kbd>↑</kbd> / <kbd>⌃</kbd><kbd>⌘</kbd><kbd>↓</kbd> | Move the selected row up or down among its siblings |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>J</kbd> | Jump to the next session that needs you |
| <kbd>⌃</kbd><kbd>⇥</kbd> | Most-recently-used switcher; hold ⌃ and press ⇥ again to advance |
| <kbd>⌃</kbd><kbd>⇧</kbd><kbd>Space</kbd> | Peek tabs: preview sessions across projects without switching |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>B</kbd> | Move keyboard focus to the sidebar |

Selecting a session also focuses its terminal.

While the switcher is open: ⇧⌃⇥ cycles backwards, ← ↑ go back, → ↓ go forward, ↵ commits the highlighted session, Esc cancels, and releasing ⌃ commits. Every other key is swallowed until it closes.

## Surfaces
| Shortcut | Action |
| --- | --- |
| <kbd>⌘</kbd><kbd>K</kbd> | Command palette |
| <kbd>⌘</kbd><kbd>P</kbd> | Open project (the palette's project page) |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>H</kbd> | Search chats (the palette's history page) |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>F</kbd> | Search notes |
| <kbd>⌃</kbd><kbd>⌘</kbd><kbd>T</kbd> | To-dos across every note |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>I</kbd> | Notifications inbox |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>O</kbd> | Session overview |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>W</kbd> | Worktrees overview |
| <kbd>⌘</kbd><kbd>,</kbd> | Settings |
| <kbd>⌘</kbd><kbd>B</kbd> | Show or hide the sidebar (or the top bar with horizontal tabs) |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>S</kbd> | Switch tabs between the sidebar and the top |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>I</kbd> | Show or hide the inspector |
| <kbd>⌘</kbd><kbd>J</kbd> | Show or hide a terminal pane below the focused pane |

⌘K, ⌘P and ⇧⌘H open the same palette on different pages. The palette lists most commands with their shortcuts, so it doubles as a reminder for the ones you use least. **Check for Updates…**, **What's New** and **Review session launches** have no default shortcut and live in the palette.

## Panes
| Shortcut | Action |
| --- | --- |
A tab works like a terminal multiplexer: it holds a split tree of panes, and stays one row in the sidebar. The row shows the most urgent pane's state (needs you, then working) and a small pane count. A new pane is a terminal in the focused pane's directory, on the same host.

| Shortcut | Action |
| --- | --- |
| <kbd>⌘</kbd><kbd>D</kbd> | Split right: a new terminal pane |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>D</kbd> | Split down: a new terminal pane |
| <kbd>⌥</kbd><kbd>⌘</kbd> + arrow | Focus the nearest pane in that direction |
| <kbd>⌘</kbd><kbd>[</kbd> / <kbd>⌘</kbd><kbd>]</kbd> | Focus the previous / next pane, wrapping |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>↵</kbd> | Zoom the focused pane, or restore the layout |
| <kbd>⌃</kbd><kbd>⌘</kbd><kbd>=</kbd> | Equalize: every pane gets an equal share of its row or column |
| <kbd>⌥</kbd><kbd>⇧</kbd><kbd>⌘</kbd> + arrow | Resize the nearest split divider by five percent |
| <kbd>⌘</kbd><kbd>J</kbd> | Show or hide a terminal pane below the focused pane; a hidden terminal keeps running |
| <kbd>⌘</kbd><kbd>W</kbd> | Close the focused terminal pane; on the agent's pane, close the tab |

Drag a divider to resize; panes stop at a minimum size. Quitting an agent (^C^C, /exit) returns its pane to your login shell and never closes it. A pane's title bar only carries a ✕ on terminal panes; splitting, zoom, swapping and moving panes are on the keyboard and in the command palette.

## Inside the palette
| Shortcut | Action |
| --- | --- |
| <kbd>↑</kbd> / <kbd>↓</kbd> | Move the highlight |
| <kbd>⌃</kbd><kbd>P</kbd> / <kbd>⌃</kbd><kbd>N</kbd> | Move the highlight, readline style |
| <kbd>↵</kbd> | Run the highlighted entry |
| <kbd>⌘</kbd><kbd>↵</kbd> | Open project only: open a plain terminal in that folder instead of the default agent |
| <kbd>⌘</kbd><kbd>[</kbd> | Back to the previous palette page |
| <kbd>⌫</kbd> with an empty query | Back to the previous palette page |
| <kbd>Esc</kbd> | Close the palette and cancel any theme preview |

Choose **Settings → Color theme** in the palette, or search for **Color theme**. Arrow keys and hover preview each theme across the app. Enter or a click saves it; Escape, Back or switching pages restores the saved theme.

## Inside the launcher
| Shortcut | Action |
| --- | --- |
| <kbd>⇥</kbd> / <kbd>⇧</kbd><kbd>⇥</kbd> | Cycle the agent forward or backward |
| <kbd>↵</kbd> | Start the session |
| <kbd>⇧</kbd><kbd>↵</kbd> | New line in the prompt |
| <kbd>⌘</kbd><kbd>R</kbd> | Show saved recipes |
| <kbd>⌘</kbd><kbd>S</kbd> | Save the prompt as a recipe, or update the active one |
| <kbd>⌘</kbd><kbd>1</kbd> … <kbd>⌘</kbd><kbd>3</kbd> | With an empty prompt, run the first three recipes |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>A</kbd> | Choose the account (Claude Code and Codex) |
| <kbd>Esc</kbd> | Close the launcher |

When the agent or project picker is open it takes the arrows first: ↑ ↓ move the highlight, ↵ commits it, and Esc closes the picker without closing the launcher. Recipes are covered in [Recipes and schedules](/docs/scheduled-tasks/).

A handoff opens in the same surface with the generated context editable. Nothing is sent until you activate **Send handoff** or press Return. Esc cancels without sending.

## Inside the overview
| Shortcut | Action |
| --- | --- |
| Arrow keys | Move focus between sessions |
| <kbd>↵</kbd> | Activate the focused session |
| <kbd>⌘</kbd><kbd>A</kbd> | Select every session |
| Any character | Add to the filter query |
| <kbd>⌫</kbd> / <kbd>⌦</kbd> | Delete from the query; with an empty query and a selection, close the selected sessions |
| <kbd>Esc</kbd> | Step back, then close |

In Settings and the worktrees overview, Esc closes the surface. In Settings, Esc first dismisses an open menu or the remote-host editor. Inside the inspector, the ask and commit composers take ↵ to submit and Esc to cancel.

## Terminal
| Shortcut | Action |
| --- | --- |
| <kbd>⌘</kbd><kbd>F</kbd> | Find in the terminal |
| <kbd>⌘</kbd><kbd>G</kbd> / <kbd>⇧</kbd><kbd>⌘</kbd><kbd>G</kbd> | Next / previous match |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>F</kbd> | Find the selected text |
| <kbd>⌘</kbd><kbd>C</kbd> | Copy the selection |
| <kbd>⌘</kbd><kbd>V</kbd> | Paste, including images |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>C</kbd> | Keyboard copy mode |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>C</kbd> | Quote the selection into this session's composer |
| <kbd>⌥</kbd><kbd>⇧</kbd><kbd>⌘</kbd><kbd>C</kbd> | Send the selection to another session |
| <kbd>⌘</kbd><kbd>E</kbd> | Insert path: pick a file under the session's folder and type its path |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>E</kbd> | Open the scrollback in your text editor |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>↑</kbd> / <kbd>⇧</kbd><kbd>⌘</kbd><kbd>↓</kbd> | Previous / next shell prompt (OSC 133) |
| <kbd>⌘</kbd><kbd>=</kbd> or <kbd>⌘</kbd><kbd>+</kbd> | Bigger text |
| <kbd>⌘</kbd><kbd>-</kbd> | Smaller text |
| <kbd>⌘</kbd><kbd>0</kbd> | Reset text size |

With the find bar open, ↵ jumps to the next match, ⇧↵ to the previous one, and Esc closes it.

Apart from ⌃⇥ and the surface shortcuts above, keys without ⌘ go to the running program, modifiers and all. Keys with ⌘ do not reach the program, with one exception: ⌥⌫ and ⌘⌫ still delete a word or a line in the shell.

## Notes
These work while a note's editor has focus.

| Shortcut | Action |
| --- | --- |
| <kbd>⌃</kbd><kbd>⌘</kbd><kbd>↵</kbd> | Start an agent on the to-do under the caret |
| <kbd>⌘</kbd><kbd>↵</kbd> | Tick or untick a to-do |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>↵</kbd> | Fold or unfold |
| <kbd>⌘</kbd><kbd>B</kbd> / <kbd>⌘</kbd><kbd>I</kbd> | Bold / italic |
| <kbd>⌘</kbd><kbd>E</kbd> | Inline code |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>X</kbd> | Strikethrough |
| <kbd>⌘</kbd><kbd>K</kbd> | Link |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>0</kbd> … <kbd>3</kbd> | Turn into text, heading 1, 2 or 3 |
| <kbd>⇧</kbd><kbd>⌘</kbd><kbd>8</kbd> / <kbd>7</kbd> / <kbd>9</kbd> | Turn into bulleted list / numbered list / to-do |
| <kbd>⌥</kbd><kbd>⌘</kbd><kbd>Q</kbd> / <kbd>⌥</kbd><kbd>⌘</kbd><kbd>C</kbd> | Turn into quote / code |
| <kbd>⌥</kbd><kbd>⇧</kbd><kbd>↑</kbd> / <kbd>↓</kbd> | Move the block up or down |
| <kbd>⌃</kbd><kbd>↵</kbd> | Table menu |
| <kbd>⌘</kbd><kbd>Z</kbd> / <kbd>⇧</kbd><kbd>⌘</kbd><kbd>Z</kbd> | Undo / redo |

See [Notes](/docs/notes/) for blocks, mentions and starting agents from to-dos.

## Text fields
The command palette, the terminal find bar, the history filter, the launcher prompt and the inspector composers share one keymap.

| Shortcut | Action |
| --- | --- |
| <kbd>⌘</kbd><kbd>A</kbd> | Select all |
| <kbd>⌘</kbd><kbd>C</kbd> / <kbd>⌘</kbd><kbd>X</kbd> / <kbd>⌘</kbd><kbd>V</kbd> | Copy, cut, paste |
| <kbd>←</kbd> / <kbd>→</kbd> | Move the caret; ⇧ extends the selection |
| <kbd>⌥</kbd><kbd>←</kbd> / <kbd>⌥</kbd><kbd>→</kbd> | Move by word |
| <kbd>⌘</kbd><kbd>←</kbd> / <kbd>⌘</kbd><kbd>→</kbd> | Start or end of the line |
| <kbd>Home</kbd> / <kbd>End</kbd> | Start or end of the line |
| <kbd>⌫</kbd> / <kbd>⌦</kbd> | Delete a character; ⌥ deletes a word, ⌘ deletes to the line edge |
| <kbd>⌃</kbd><kbd>A</kbd> / <kbd>⌃</kbd><kbd>E</kbd> | Start or end of the line |
| <kbd>⌃</kbd><kbd>B</kbd> / <kbd>⌃</kbd><kbd>F</kbd> | Back or forward one character |
| <kbd>⌃</kbd><kbd>H</kbd> / <kbd>⌃</kbd><kbd>D</kbd> | Delete the character before or after the caret |
| <kbd>⌃</kbd><kbd>W</kbd> | Delete the word before the caret |
| <kbd>⌃</kbd><kbd>U</kbd> / <kbd>⌃</kbd><kbd>K</kbd> | Delete to the start or end of the line |

## When two surfaces want the same key
diri asks which surface is in front, then falls back to the terminal.
- **An unhandled global shortcut is not swallowed.** ⌘R or ⌘J with nothing selected, or ⌘9 with no sessions, leaves the keystroke alone.
- **The launcher takes everything while it is open**, except ⌘N, which closes it.
- **Settings and the worktrees overview take everything** except ⇧⌘H, ⌘K, ⌘P and ⌘,.
- **The switcher and the overview own the arrow keys** while they are visible, so ⌥⌘↑, ⇧⌘[ and ⌃⌘↑ stand down.
- **Esc is shared.** With the overview closed, Esc clears a multi-session sidebar selection but still reaches the focused terminal, so Esc in vim never depends on the sidebar.
- **⌘W is three commands.** It closes the focused terminal pane first, then the selected session, and only with no session selected the window.
- **⌘J and ⇧⌘J are unrelated.** ⌘J toggles a terminal pane; ⇧⌘J jumps to the next session that needs you.
