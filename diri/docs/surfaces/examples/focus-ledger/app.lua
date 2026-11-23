-- Focus Ledger: a personal tracker an agent might write for someone.
-- Commits per day come from `git log` (sync, once approved); habits are the
-- app's own state. Everything you see is drawn from these lines.

app = {
  title = "Focus Ledger",
  subtitle = "Deep-work streak, straight from git",
  icon = "🔥",
  accent = "#22c55e",
  style = { density = "compact", canvas = "tinted", radius = 14 },
}

function init()
  return { habits = { walk = true, read = false, inbox = false } }
end

-- 20 weeks of days, one cell each, brighter with more commits.
local function heatmap(days)
  local c = ui.canvas{ width = 520, height = 136 }
  for i, n in ipairs(days) do
    local col, row = (i - 1) // 7, (i - 1) % 7
    local color = "fill"
    if n > 0 then
      color = string.format("#22c55e%02x", math.floor(70 + math.min(n / 6, 1) * 185))
    end
    c.shapes[#c.shapes + 1] = ui.rect(col * 26, row * 19, 22, 16, { color = color, radius = 4 })
  end
  return c
end

local function tile(t)
  t.padding = t.padding or 16
  t.radius = 16
  t.gap = t.gap or 4
  t.bg = t.bg or "surface"
  t.border = t.border or "border"
  return ui.col(t)
end

function render(state, data)
  data = data or {}
  local habits = state.habits
  local done = 0
  for _, on in pairs(habits) do if on then done = done + 1 end end
  return ui.col{ gap = 12,
    ui.row{ gap = 12, align = "stretch",
      tile{ grow = 3, bg = "#0f2a1c", border = "#22c55e55",
        ui.text("TODAY", { size = 11, weight = "semibold", color = "#86efac" }),
        ui.row{ gap = 10, align = "end",
          ui.text(tostring(data.commits_today or 0), { size = 40, weight = "bold", color = "#ecfdf5" }),
          ui.text("commits", { size = 14, color = "#86efac" }),
          ui.spacer(),
          ui.sparkline(data.week or {}, { width = 220, height = 48, color = "#4ade80", fill = "#22c55e33" }),
        },
      },
      tile{ grow = 2,
        ui.text("STREAK", { size = 11, weight = "semibold", color = "secondary" }),
        ui.text((data.streak or 0) .. " days", { size = 28, weight = "bold" }),
        ui.text("best " .. (data.best or 0) .. " · " .. done .. "/3 habits today", { size = 12, color = "tertiary" }),
      },
    },
    tile{ gap = 10,
      ui.row{
        ui.text("Last 20 weeks", { weight = "semibold" }),
        ui.spacer(),
        ui.text(data.repo or "", { mono = true, size = 11, color = "tertiary" }),
      },
      heatmap(data.days or {}),
    },
    ui.row{ gap = 12, align = "stretch",
      tile{ grow = 1, gap = 8,
        ui.text("Habits", { weight = "semibold" }),
        ui.checkbox{ id = "walk", label = "20-minute walk", value = habits.walk },
        ui.checkbox{ id = "read", label = "Read one chapter", value = habits.read },
        ui.checkbox{ id = "inbox", label = "Inbox to zero", value = habits.inbox },
      },
      tile{ grow = 1, gap = 8,
        ui.text("Busiest hours", { weight = "semibold" }),
        ui.bars(data.hours or {}, { width = 400, height = 84, color = "#22c55e66",
          highlight = data.peak_hour, highlight_color = "#22c55e" }),
        ui.text("Peak around " .. (data.peak_label or "-"), { size = 12, color = "tertiary" }),
      },
    },
  }
end

function on_event(state, event)
  if event.type == "checkbox" then
    state.habits[event.id] = event.value
  end
  return state
end

-- Runs on open, on Refresh, and every 15 minutes while on screen. `ctx.exec`
-- works once the person approves this exact file.
function sync(ctx)
  local repo = (ctx.read("repo.txt"):gsub("%s+$", ""))
  local log = ctx.exec({ "git", "-C", repo, "log", "--since=140 days ago", "--format=%cd", "--date=format:%Y-%m-%d %H" })
  local per_day, hours = {}, {}
  for h = 1, 24 do hours[h] = 0 end
  for day, hour in log.stdout:gmatch("(%d+%-%d+%-%d+) (%d+)") do
    per_day[day] = (per_day[day] or 0) + 1
    local h = tonumber(hour) + 1
    hours[h] = hours[h] + 1
  end
  local days, week = {}, {}
  local today = ctx.now // 1000
  for i = 139, 0, -1 do
    local day = os_date(today - i * 86400)
    days[#days + 1] = per_day[day] or 0
  end
  for i = #days - 6, #days do week[#week + 1] = days[i] end
  local streak, best, run = 0, 0, 0
  for _, n in ipairs(days) do
    if n > 0 then run = run + 1 else run = 0 end
    if run > best then best = run end
  end
  streak = run
  local peak, peak_n = 1, -1
  for h, n in ipairs(hours) do if n > peak_n then peak, peak_n = h, n end end
  return {
    repo = repo:match("[^/]+$"), commits_today = days[#days], week = week, days = days,
    streak = streak, best = best, hours = hours, peak_hour = peak,
    peak_label = string.format("%02d:00", peak - 1),
  }
end

-- YYYY-MM-DD for a Unix time, without `os` (the sandbox has none).
function os_date(t)
  local days = t // 86400
  local z = days + 719468
  local era = z // 146097
  local doe = z - era * 146097
  local yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
  local y = yoe + era * 400
  local doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
  local mp = (5 * doy + 2) // 153
  local d = doy - (153 * mp + 2) // 5 + 1
  local m = mp < 10 and mp + 3 or mp - 9
  if m <= 2 then y = y + 1 end
  return string.format("%04d-%02d-%02d", y, m, d)
end
