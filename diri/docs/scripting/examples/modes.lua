-- Key modes: ⌘⇧R enters "resize", where h/j/k/l resize the focused pane
-- and every other key is ignored until Escape or Enter. ⌃B then a key is a
-- one-shot prefix: the next key runs, and the mode ends either way.

diri.keymap.mode("resize", { exclusive = true, desc = "Resize panes" })
diri.keymap.set("cmd-shift-r", { mode = "resize" })
diri.keymap.set("resize/h", "pane-shrink-width")
diri.keymap.set("resize/l", "pane-grow-width")
diri.keymap.set("resize/j", "pane-grow-height")
diri.keymap.set("resize/k", "pane-shrink-height")
diri.keymap.set("resize/enter", { mode = false })

diri.keymap.mode("prefix", { once = true })
diri.keymap.set("ctrl-b", { mode = "prefix" })
diri.keymap.set("prefix/c", "new-terminal")
diri.keymap.set("prefix/z", "toggle-pane-zoom")
-- Callbacks get where the key was pressed: {session, pane, window, origin}.
diri.keymap.set("prefix/g", function(ctx)
  if ctx.session then
    diri.session.send_text({ sessionID = ctx.session, text = "git status", submit = true })
  end
end)
