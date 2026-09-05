# Terminal panes

A desktop workspace can show up to eight sessions in nested right/below splits.
Each pane controls its own existing Engine session. Selecting a session in the
sidebar restores its saved workspace and focuses that pane.

- **⌘D** creates a terminal to the right of the focused pane.
- **⌥⌘D** creates a terminal below it. New terminals inherit that session's
  directory and host.
- **Add Existing Session to Split**, in the command palette, opens the session
  picker. Choose Right or Below, then an existing agent or terminal. Moving a
  session from another split workspace removes its old pane first.
- **⇧⌥⌘← / → / ↑ / ↓** moves focus in that direction. **⌥⌘Tab** cycles through
  panes. Clicking a terminal also selects its session for the inspector and
  attention state.
- Drag a divider to resize either branch; double-click it to reset that split
  to equal sizes. Nested ratios are saved when the drag ends.
- **⌘W**, **Close Pane**, or the pane's × removes that pane while keeping the
  session running. Reopen it from the sidebar or the existing-session picker.
  The sidebar's session-close action still ends/removes the session.
- **⌘J** keeps its auxiliary-terminal workflow. An auxiliary already visible
  when splitting becomes part of the layout; hiding it keeps its shell alive.

The picker supports Up/Down and Enter, Left/Right to choose the split direction,
and Escape to dismiss it. Shortcuts can be changed in Settings → Shortcuts.

Layouts are versioned desktop preferences, bounded to 64 saved workspaces. A
restart restores session identities and split ratios after the Engine session
list arrives. Missing/archived sessions are pruned and empty branches collapse.
The existing Holder owns every process; no transport or Helper changes are
needed, and a session has at most one mounted terminal attachment owner.
