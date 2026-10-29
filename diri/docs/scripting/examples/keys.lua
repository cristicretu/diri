-- Loaded from init.lua with require("keys").
-- Keys are written like cmd-shift-d, ctrl-alt-left, or a sequence "ctrl-a c".
-- They sit over Diri's defaults and under anything recorded in
-- Settings > Shortcuts, which always wins.

-- Another key for a window action (dirijor actions lists them).
diri.keymap.set("cmd-shift-e", "split-pane-best-fit", { desc = "Split where there is room" })

-- A tmux-style sequence.
diri.keymap.set("ctrl-a c", "new-terminal")
diri.keymap.set("ctrl-a z", "toggle-pane-zoom")

-- An action with arguments.
diri.keymap.set("cmd-alt-1", { action = "select-tab", args = { index = 1 } })

-- A Lua function: type a command into the selected session.
local selected
diri.on("focus", function(event)
  selected = event.session
end)
diri.keymap.set("cmd-alt-g", function()
  if selected then
    diri.session.send_text({ sessionID = selected, text = "git status", submit = true })
  end
end)

-- Free a default key (⇧⌘K clears scrollback) so nothing happens on it.
diri.keymap.del("cmd-shift-k")
