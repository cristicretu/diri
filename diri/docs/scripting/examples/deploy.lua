-- Run from a terminal: dirijor script deploy.lua branch=main [--allow-exec git]
-- Arguments arrive in diri.args; diri.session_id is the tab it runs in.
-- diri.exec runs only what the person grants, from an argv, never a shell.

local branch = diri.args.branch or "main"

if not diri.require_grant({ exec = { "git" } }) then
  print("git was not granted; nothing to do")
  return
end

local fetch = diri.exec({ "git", "fetch", "origin", branch }, {
  timeout = 60,
  on_output = function(line, stream)
    print(stream, line)
  end,
})
if not fetch.ok then
  diri.notify("git fetch failed", { level = "error", detail = fetch.stderr })
  diri.stop(1)
  return
end

-- Open a shell that runs the tests, and wait for it to finish.
local tests = diri.session.spawn({ kind = "shell", cwd = "/tmp", name = "tests " .. branch })
local exited = diri.wait("session.exited", { session = tests.id }, 10 * 60 * 1000)
if exited == nil then
  print("still running after ten minutes")
elseif exited.code == 0 then
  diri.notify("tests passed on " .. branch, { level = "success" })
else
  diri.stop(exited.code or 1)
end
