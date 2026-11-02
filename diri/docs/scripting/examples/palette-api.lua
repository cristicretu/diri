-- Palette commands are also window actions named script.NAME, so a shell
-- or an Agent runs them by name and gets back what `run` returned:
--   dirijor do script.count-shells
--   dirijor do script.rename-here title=build

diri.command({
  name = "count-shells",
  title = "Count Shells",
  desc = "How many shell tabs are open",
  run = function()
    local shells = 0
    for _, session in ipairs(diri.session.list().sessions or {}) do
      if session.kind and session.kind.shell then
        shells = shells + 1
      end
    end
    return { shells = shells }
  end,
})

diri.command({
  name = "rename-here",
  title = "Rename This Tab",
  run = function(ctx)
    local title = ctx.args and ctx.args.title
    if ctx.session and title then
      diri.session.rename({ sessionID = ctx.session, title = title })
      return { renamed = ctx.session }
    end
  end,
})

-- Bells, directory changes and foreground programs are events too.
diri.on("session.bell", function(event)
  diri.log("bell in", event.session_id)
end)
diri.on("session.program", function(event)
  diri.log(event.session.title, "now runs", event.to or "its prompt")
end)
