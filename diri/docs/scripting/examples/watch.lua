-- Run from a terminal: dirijor script watch.lua [--for SECONDS]
-- Prints every session status change until Ctrl-C. Works the same against
-- remote sessions: scripts run here and reach them through the Engine.

for _, session in ipairs(diri.session.list().sessions) do
  print(session.id, session.title)
end

diri.on("session.status", function(event)
  print(os.date("!%H:%M:%S"), event.session.title, event.from .. " -> " .. event.to)
end)

diri.on("session.exited", function(event)
  print(os.date("!%H:%M:%S"), event.session.title, "exited", event.code or event.signal or "")
end)
