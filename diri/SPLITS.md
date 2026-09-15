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

## Vertical split tabs and dragging

A split is **one vertical tab in the existing sidebar**, representing all its
member sessions. It uses the ordinary sidebar row height and selection fill,
with a combined title and pane count. Click it to restore the layout and last
focused pane. Groups stay under their first member's project (or in recency).
Inactive panes are dimmed by just 4%. Pointer focus updates during the input
event, before terminal mouse reporting, with no focus animation or terminal
remount.

- Drag a sidebar session onto the center of another session to create a split
  group. The narrow top/bottom insertion bands keep their existing reorder
  behavior. Hold Option/Alt for the existing handoff gesture instead.
- Drag a session or a grouped row into the terminal. Left, right,
  top, and bottom targets choose where to insert it. Moving a whole group
  preserves its nested arrangement and divider ratios.
- Hover another vertical tab during a group drag to reveal its workspace, then
  continue onto the desired pane edge. Drag a pane's numbered terminal-header
  handle to move only that pane. Its × removes it from the split and returns it to the sidebar,
  keeping its Engine session alive.
- The lifted card depicts the pane arrangement. Edge targets expand and follow
  nearby pointer motion, retargeting from their current position. Reduced Motion
  removes target travel and the lift fade.
- Escape cancels. Releasing in the center of a terminal or outside a destination
  leaves layouts unchanged. Showing the drag preview does not launch a session,
  resize a terminal, or mutate a layout.

The eight-pane limit includes every incoming pane. Invalid moves are atomic.
Saved layouts and focus restore after the Engine's session list arrives;
missing members are pruned. Existing split commands and the session picker
provide keyboard alternatives. On Linux, Split Below uses Ctrl+Alt+Shift+D to
preserve the existing Ctrl+Alt+D delegation shortcut.
