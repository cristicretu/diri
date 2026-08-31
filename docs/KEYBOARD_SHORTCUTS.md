# Keyboard shortcuts

Diri is keyboard-first. This page groups every user-facing binding by what you
are trying to do, rather than by the file it lives in.

Modifiers use the macOS glyphs — ⌘ Command, ⇧ Shift, ⌥ Option, ⌃ Control — with
a plain-language name wherever the glyph alone is ambiguous. Unless a row says
otherwise, a shortcut works whenever the main window is focused, including while
you are typing in a terminal.

## Sessions

| Shortcut | Keys | Action |
| --- | --- | --- |
| ⌘N | Command-N | Show or hide the launcher: pick a project and agent, then type the first prompt |
| ⌘T | Command-T | Start a session with the default agent immediately, no launcher |
| ⌥⌘T | Option-Command-T | Start a plain shell session |
| ⇧⌘N | Shift-Command-N | Start a Codex session |
| ⌘R | Command-R | Rename the selected session in place |
| ⌃⌘D | Control-Command-D | Mark the selected session as a handoff source; select a target and press again to review and send |
| ⇧⌘W | Shift-Command-W | Archive the selected session |
| ⌘W | Command-W | Close a focused auxiliary terminal; otherwise close the selected session, or the window when none is selected |
| ⇧⌘T | Shift-Command-T | Reopen the most recently closed session |
| ⌘Q | Command-Q | Quit Diri (the daemon keeps sessions alive) |
| ⌘H | Command-H | Hide Diri |

## Moving between sessions

| Shortcut | Keys | Action |
| --- | --- | --- |
| ⌘1 … ⌘8 | Command-1 to Command-8 | Select the nth session, matching the row hints in the sidebar |
| ⌘9 | Command-9 | Select the last session, the browser convention |
| ⌘[ / ⌘] | Command-bracket | Previous / next session in sidebar order, wrapping |
| ⌥⌘↑ / ⌥⌘↓ | Option-Command-arrow | Previous / next session |
| ⌥⌘← / ⌥⌘→ | Option-Command-arrow | Previous / next session |
| ⌃⌘↑ / ⌃⌘↓ | Control-Command-arrow | Move the selected row up or down among its siblings within the project group |
| ⇧⌘J | Shift-Command-J | Jump to the next session that needs input |
| ⌃⇥ | Control-Tab | Open the most-recently-used switcher; hold ⌃ and press ⇥ again to advance |

Selecting a session also focuses its terminal.

While the switcher overlay is open: ⇧⌃⇥ cycles backwards, ← ↑ go back, → ↓ go
forward, ↵ commits the highlighted session, Esc cancels, and releasing ⌃ commits.
Every other key is swallowed until it closes.

Session navigation with arrows and brackets stands down while the switcher or
the overview is open, so those surfaces keep the arrow keys.

## Surfaces

| Shortcut | Keys | Action |
| --- | --- | --- |
| ⌘K | Command-K | Command palette |
| ⌘P | Command-P | Quick Open — start a session in a recent directory |
| ⇧⌘O | Shift-Command-O | Overview of all sessions |
| ⇧⌘H | Shift-Command-H | History of past conversations |
| ⌥⌘W | Option-Command-W | Worktrees overview |
| ⌘, | Command-comma | Settings |
| ⌘B | Command-B | Show or hide the sidebar |
| ⇧⌘D | Shift-Command-D | Show or hide the inspector |
| ⌘J | Command-J | Show or hide an auxiliary terminal below the selected agent's workbench |

The command palette always lists ⌘T for the default agent. When Codex is not
the default, its command also shows ⇧⌘N. The palette lists ⌥⌘T, ⌘P, ⇧⌘O,
⌥⌘W, ⌘B and ⌘, too, so it doubles as a reminder for the shortcuts you use
least.

### Inside the command palette and Quick Open

| Shortcut | Keys | Action |
| --- | --- | --- |
| ↑ / ↓ | Arrow keys | Move the highlight |
| ⌃P / ⌃N | Control-P / Control-N | Move the highlight, readline style |
| ↵ | Return | Run the highlighted entry |
| ⌘↵ | Command-Return | Quick Open only: open a plain shell in that directory instead of the default agent |
| Esc | Escape | Close the overlay |

Anything else edits the query through the [shared text keymap](#text-fields).

### Inside the launcher

| Shortcut | Keys | Action |
| --- | --- | --- |
| ⇥ / ⇧⇥ | Tab / Shift-Tab | Cycle the agent forward or backward |
| ↵ | Return | Submit and start the session |
| ⇧↵ | Shift-Return | Insert a newline in the prompt |
| ↑ / ↓ | Arrow keys | Move within the prompt; ⇧ extends the selection |
| Esc | Escape | Close the launcher |

When the agent or project picker is open it takes the arrows first: ↑ ↓ move the
highlight, ↵ commits it, and Esc closes the picker without closing the launcher.

A handoff opens in the same surface with the complete generated context editable.
The source and target remain visible, remote targets carry a Remote badge, and
nothing is sent until you activate **Send handoff** or press Return. Esc cancels
without sending and restores any unfinished Command-N draft.

### Inside the overview

| Shortcut | Keys | Action |
| --- | --- | --- |
| ← → ↑ ↓ | Arrow keys | Move focus between sessions |
| ↵ | Return | Activate the focused session |
| ⌘A | Command-A | Select every session |
| Any character | — | Append to the filter query, space included |
| ⌫ / ⌦ | Delete | Delete back through the filter query; with an empty query and a selection, close the selected sessions |
| Esc | Escape | Step back, then close |

### Inside history, settings and worktrees

| Shortcut | Keys | Action |
| --- | --- | --- |
| ↑ / ↓ | Arrow keys | Move the highlight in history |
| ↵ | Return | Open the highlighted conversation |
| Esc | Escape | Close the surface |

In history, other keys filter the list. In settings, Esc first dismisses an open
menu or the remote-host editor and only then closes the surface; inside that
editor ⇥ and ⇧⇥ move between fields and ↵ saves the host.

While one of these surfaces is open, only ⌘H, ⌘K, ⌘P and ⌘, still reach the app.
Everything else belongs to the surface.

### Inside the inspector

Ask and commit composers both take ↵ to submit and Esc to cancel.

## Terminal

| Shortcut | Keys | Action |
| --- | --- | --- |
| ⌘F | Command-F | Open find |
| ⌘G | Command-G | Next match |
| ⇧⌘G | Shift-Command-G | Previous match |
| ⌘C | Command-C | Copy the selection |
| ⌘V | Command-V | Paste, including images |
| ⌘= or ⌘+ | Command-plus | Zoom in |
| ⌘- | Command-minus | Zoom out |
| ⌘0 | Command-zero | Reset zoom |

With the find bar open, ↵ jumps to the next match, ⇧↵ to the previous one, and
Esc closes it. Typing edits the query through the [shared text keymap](#text-fields);
⌘V stays the paste action rather than a find-bar edit, so it never inserts twice.

Apart from ⌃⇥ and the active-surface shortcuts above, keys without ⌘ go to the
running program, modifiers and all. Keys with ⌘ do not reach the program — ⌫ is the
one exception, so ⌥⌫ and ⌘⌫ still delete a word or a line inside the shell.

## Text fields

The command palette, Quick Open, the terminal find bar, the history filter, the
launcher prompt and the inspector composers share one keymap.

| Shortcut | Keys | Action |
| --- | --- | --- |
| ⌘A | Command-A | Select all |
| ⌘C / ⌘X / ⌘V | Command-C / X / V | Copy, cut, paste |
| ← / → | Arrow keys | Move the caret; ⇧ extends the selection |
| ⌥← / ⌥→ | Option-arrow | Move by word |
| ⌘← / ⌘→ | Command-arrow | Move to the start or end of the line |
| Home / End | — | Start or end of the line |
| ⌫ / ⌦ | Delete | Delete a character; ⌥ deletes a word, ⌘ deletes to the line edge |
| ⌃A / ⌃E | Control-A / E | Start or end of the line, readline style |
| ⌃B / ⌃F | Control-B / F | Back or forward one character |
| ⌃H / ⌃D | Control-H / D | Delete the character before or after the caret |
| ⌃W | Control-W | Delete the word before the caret |
| ⌃U / ⌃K | Control-U / K | Delete to the start or end of the line |

## Context and conflicts

Diri resolves an ambiguous keystroke by asking which surface is in front, then
falling back to the terminal:

- **A global shortcut that is not handled is not swallowed.** ⌘R or ⌘J with
  nothing selected, and ⌘9 with no sessions, leave the keystroke alone rather
  than eating it.
- **The launcher takes everything while it is open**, except ⌘N, which stays
  available.
- **History, settings and worktrees take everything** except ⌘H, ⌘K, ⌘P and ⌘,.
- **The switcher and the overview own the arrow keys** while they are visible, so
  ⌥⌘↑, ⌘[ and ⌃⌘↑ stand down for as long as either is up.
- **Esc is shared.** With the overview closed, Esc clears a multi-session sidebar
  selection but is deliberately not swallowed — the focused terminal still gets
  it, so pressing Esc in vim never depends on what the sidebar is doing.
- **⌘W is three commands.** It closes a focused auxiliary terminal first;
  otherwise the sidebar closes the selected session, and only with no session
  selected does it reach the window and close it.
- **⌘J and ⇧⌘J are unrelated.** ⌘J toggles an auxiliary workbench terminal;
  ⇧⌘J is the attention jump that used to live on plain ⌘J.

The following bindings are internal or contextual rather than user-facing, and
are documented above only where they surface in the UI: the switcher's swallowing
of stray keys while it is open, the key-up and modifiers-changed fallbacks that
commit the switcher when ⌃ is released, and the per-field ⇥ traversal inside the
remote-host editor.
