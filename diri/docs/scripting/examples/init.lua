-- ~/.config/diri/init.lua: a starting point. Copy it (and keys.lua) into
-- ~/.config/diri/; Diri reloads it on save. Errors show as a toast and in
-- the script log (~/Library/Application Support/... see docs/scripting.md).

-- Other files beside init.lua load with require.
require("keys")

-- Tell me when an Agent stops working: it is done or it needs me.
diri.on("session.status", function(event)
  local session = event.session
  if event.kind == "shell" then
    return
  end
  if event.from == "working" and event.to == "needs_input" then
    diri.notify(session.title .. " needs you", { level = "warning" })
  elseif event.from == "working" and event.to == "idle" then
    diri.notify(session.title .. " finished", { level = "success" })
  end
end)

-- A failed command in a plain shell tab.
diri.on("session.exited", function(event)
  if event.code ~= nil and event.code ~= 0 then
    diri.notify(event.session.title .. " exited with " .. event.code, { level = "error" })
  end
end)

-- A palette command (⌘K, "Tidy Panes") built from window actions.
diri.command({
  name = "tidy-panes",
  title = "Tidy Panes",
  run = function()
    diri.action("equalize-panes")
    diri.action("toggle-sidebar")
  end,
})

-- A palette command that runs one action with arguments.
diri.command({ name = "first-tab", title = "Go to First Tab", run = "select-tab", args = { index = 1 } })
