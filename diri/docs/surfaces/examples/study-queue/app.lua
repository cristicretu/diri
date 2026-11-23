-- Study Queue: a lecture player an agent might build for someone learning.
-- Thumbnails are YouTube's own images; clicking a card opens the video.
-- sync() refreshes the list from a playlist feed once the person approves.

app = {
  title = "Study Queue",
  subtitle = "This week's lectures",
  icon = "🎧",
  accent = "#f97316",
  style = { header = "minimal", radius = 18 },
}

function init()
  return { playing = 1, speed = "1.5x" }
end

-- A progress ring from two polylines: the track and the arc.
local function ring(fraction, size)
  local c = ui.canvas{ width = size, height = size }
  local cx, cy, r = size / 2, size / 2, size / 2 - 4
  local function arc(to)
    local points, steps = {}, math.max(2, math.floor(72 * to))
    for i = 0, steps do
      local a = -math.pi / 2 + (i / 72) * 2 * math.pi
      points[#points + 1] = { cx + r * math.cos(a), cy + r * math.sin(a) }
    end
    return points
  end
  c.shapes[#c.shapes + 1] = ui.line(arc(1), { color = "#f9731633", width = 7 })
  c.shapes[#c.shapes + 1] = ui.line(arc(fraction), { color = "accent", width = 7 })
  return c
end

function render(state, data, ctx)
  data = data or {}
  local narrow = ctx.layout == "panel"
  local split = narrow and ui.col or ui.row
  local queue = data.queue or {}
  local now = queue[state.playing] or { title = "Nothing queued", channel = "", length = "" }
  local watched = data.watched or 0
  local next_up = {}
  for i, video in ipairs(queue) do
    if i ~= state.playing then
      next_up[#next_up + 1] = ui.box{ width = narrow and 176 or 214,
        ui.media{ image = video.thumb, title = video.title, subtitle = video.channel,
                  badge = video.length, url = video.url },
      }
    end
  end
  return ui.col{ gap = 18,
    split{ gap = 18, align = narrow and "stretch" or "start",
      ui.box{ grow = 3,
        ui.media{ image = now.thumb, title = now.title, url = now.url,
                  subtitle = now.channel .. " · " .. now.length .. " · " .. state.speed, badge = "NOW" },
      },
      ui.col{ grow = 2, gap = 14, padding = 18, radius = 18, bg = "#1f130b", border = "#f9731655",
        ui.row{ gap = 14,
          ring(watched, 72),
          ui.col{ gap = 2,
            ui.text(math.floor(watched * 100 + 0.5) .. "%", { size = 30, weight = "bold", color = "#fed7aa" }),
            ui.text("of this week's list", { size = 12, color = "#fdba74" }),
          },
        },
        ui.text("MINUTES WATCHED", { size = 10.5, weight = "semibold", color = "#fdba74" }),
        ui.bars(data.minutes or {}, { width = 240, height = 64, color = "#f9731655",
          highlight = #(data.minutes or {}), highlight_color = "#fb923c", radius = 4 }),
        ui.row{ gap = 8,
          ui.button{ id = "next", label = "Next lecture", variant = "primary" },
          ui.select{ id = "speed", options = { "1x", "1.5x", "2x" }, value = state.speed },
        },
      },
    },
    ui.text("Up next", { size = 15, weight = "semibold" }),
    ui.row{ gap = 14, align = "start", table.unpack(next_up) },
  }
end

function on_event(state, event, data)
  local queue = (data or {}).queue or {}
  if event.type == "action" and event.name == "next" and #queue > 0 then
    state.playing = state.playing % #queue + 1
  elseif event.type == "select" and event.id == "speed" then
    state.speed = event.value
  end
  return state
end

-- Refreshes the queue from a YouTube playlist feed (needs approval to fetch).
function sync(ctx)
  local playlist = (ctx.read("playlist.txt"):gsub("%s+$", ""))
  local feed = ctx.fetch("https://www.youtube.com/feeds/videos.xml?playlist_id=" .. playlist)
  local data = ctx.data or {}
  local queue = {}
  for entry in feed.body:gmatch("<entry>(.-)</entry>") do
    local id = entry:match("<yt:videoId>(.-)</yt:videoId>")
    queue[#queue + 1] = {
      title = entry:match("<title>(.-)</title>"),
      channel = entry:match("<name>(.-)</name>"),
      url = "https://www.youtube.com/watch?v=" .. id,
      thumb = "https://i.ytimg.com/vi/" .. id .. "/mqdefault.jpg",
      length = "",
    }
  end
  data.queue = queue
  return data
end
