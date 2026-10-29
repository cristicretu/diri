-- Remember which session had focus, and keep a short history of titles.
-- Shows diri.on("focus") (inside the app only) and diri.after.

local history = {}

diri.on("focus", function(event)
  table.insert(history, 1, event.session)
  if #history > 10 then
    table.remove(history)
  end
end)

diri.command({
  name = "previous-focus",
  title = "Back to Previous Session",
  run = function()
    if history[2] then
      diri.action("show-session", { session_id = history[2] })
    end
  end,
})

-- Name a new shell after its directory a moment after it starts.
diri.on("session.created", function(event)
  local session = event.session
  if event.kind ~= "shell" or session.cwd == nil then
    return
  end
  diri.after(0.5, function()
    local folder = session.cwd:match("([^/]+)/?$") or session.cwd
    diri.session.rename({ sessionID = session.id, title = folder })
  end)
end)
