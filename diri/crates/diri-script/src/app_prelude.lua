-- The `ui` table a Diri App builds its view with. Every function returns a
-- plain table in Diri's component format; Diri validates and draws it.
-- Positional entries of a container's table are its children:
--   ui.row{ gap = 8, ui.text("a"), ui.text("b") }
local A = __array_mt
__array_mt = nil

local function list(t)
  return setmetatable(t or {}, A)
end

-- Moves the array part of `t` into `t.children`.
local function container(kind, t)
  t = t or {}
  local children = list(t.children)
  for i, child in ipairs(t) do
    children[#children + 1] = child
    t[i] = nil
  end
  t.children = children
  t.type = kind
  return t
end

local function with(kind, t)
  t = t or {}
  t.type = kind
  return t
end

-- Style keys accepted at the top level of any component table, moved into
-- its `style` so `ui.box{ padding = 12, bg = "accent" }` reads naturally.
local STYLE = {
  padding = true, padding_x = true, padding_y = true, gap = true, bg = true,
  border = true, border_width = true, radius = true, width = true, height = true,
  min_width = true, max_width = true, grow = true, align = true, justify = true,
  opacity = true,
}

local function styled(t)
  local style = t.style or {}
  for key in pairs(STYLE) do
    if t[key] ~= nil and not (key == "gap" and (t.type == "stack" or t.type == "row"))
      and not (key == "height" and (t.type == "scroll" or t.type == "image" or t.type == "canvas"))
      and not (key == "width" and t.type == "canvas") then
      style[key] = t[key]
      t[key] = nil
    end
  end
  if next(style) ~= nil then
    t.style = style
  end
  return t
end

ui = {}

function ui.box(t) return styled(container("box", t)) end
function ui.row(t) t = ui.box(t); t.direction = "row"; return t end
function ui.col(t) t = ui.box(t); t.direction = "column"; return t end
function ui.stack(t) return styled(container("stack", t)) end
function ui.card(t) return styled(container("card", t)) end
function ui.section(t) return styled(container("section", t)) end
function ui.grid(t)
  t = styled(container("grid", t))
  t.columns = t.columns or 2
  return t
end
function ui.scroll(t)
  t = styled(container("scroll", t))
  t.height = t.height or 240
  return t
end
function ui.spacer() return { type = "spacer" } end
function ui.divider() return { type = "divider" } end

function ui.text(text, t)
  t = with("text", t)
  t.text = tostring(text)
  return styled(t)
end
function ui.heading(text, level, t)
  t = with("heading", t)
  t.text = tostring(text)
  t.level = level or 2
  return styled(t)
end
function ui.markdown(text, t) t = with("markdown", t); t.text = tostring(text); return styled(t) end
function ui.code(text, language, t)
  t = with("code", t)
  t.text = tostring(text)
  t.language = language
  return styled(t)
end
function ui.mermaid(source, t) t = with("mermaid", t); t.source = source; return styled(t) end
function ui.image(src, t) t = with("image", t); t.src = src; return styled(t) end
function ui.badge(label, t) t = with("badge", t); t.label = tostring(label); return styled(t) end
function ui.metric(t) return styled(with("metric", t)) end
function ui.progress(t) return styled(with("progress", t)) end
function ui.link(url, t) t = with("link", t); t.url = url; return styled(t) end
function ui.session(id, t) t = with("session", t); t.session = id; return styled(t) end
function ui.media(t) return styled(with("media", t)) end

-- Controls. `id` names them in events; `on` names a button's event.
function ui.button(t)
  t = with("button", t)
  if t.action == nil then
    t.action = { kind = "app", name = t.on or t.id, payload = t.payload }
  end
  t.on = nil
  t.payload = nil
  return styled(t)
end
function ui.checkbox(t) return styled(with("checkbox", t)) end
function ui.toggle(t) return styled(with("toggle", t)) end
function ui.input(t) return styled(with("input", t)) end
function ui.select(t)
  t = with("select", t)
  local options = list()
  for i, option in ipairs(t.options or {}) do
    if type(option) == "string" then
      options[i] = { value = option, label = option }
    else
      options[i] = option
    end
  end
  t.options = options
  return styled(t)
end
function ui.checklist(t)
  t = with("checklist", t)
  t.items = list(t.items)
  return styled(t)
end
function ui.tabs(t)
  t = with("tabs", t)
  local tabs = list()
  for i, tab in ipairs(t) do
    tabs[i] = container(nil, tab)
    tabs[i].type = nil
    t[i] = nil
  end
  t.tabs = tabs
  return styled(t)
end
function ui.table(t)
  t = with("table", t)
  t.columns = list(t.columns)
  t.rows = list(t.rows)
  return styled(t)
end

-- Vector drawing. Shapes use the canvas's own points, origin top-left.
function ui.canvas(t)
  t = with("canvas", t)
  local shapes = list(t.shapes)
  for i, shape in ipairs(t) do
    shapes[#shapes + 1] = shape
    t[i] = nil
  end
  t.shapes = shapes
  return styled(t)
end
function ui.rect(x, y, w, h, o)
  o = o or {}
  return { shape = "rect", x = x, y = y, w = w, h = h, color = o.color, radius = o.radius }
end
function ui.circle(x, y, r, o)
  o = o or {}
  return { shape = "circle", x = x, y = y, r = r, color = o.color }
end
local function points(ps)
  local out = list()
  for i, p in ipairs(ps) do out[i] = list({ p[1], p[2] }) end
  return out
end
function ui.line(ps, o)
  o = o or {}
  return { shape = "line", points = points(ps), color = o.color, width = o.width }
end
function ui.area(ps, o)
  o = o or {}
  return { shape = "area", points = points(ps), color = o.color }
end

-- Charts made of the shapes above, for the common cases.
function ui.bars(values, o)
  o = o or {}
  local w, h, gap = o.width or 160, o.height or 40, o.gap or 3
  local max = o.max or 0
  for _, v in ipairs(values) do if v > max then max = v end end
  if max <= 0 then max = 1 end
  local n = math.max(#values, 1)
  local bw = (w - gap * (n - 1)) / n
  local c = ui.canvas{ width = w, height = h }
  for i, v in ipairs(values) do
    local bh = math.max(1, h * v / max)
    local color = o.color
    if o.highlight == i then color = o.highlight_color or "accent" end
    c.shapes[#c.shapes + 1] = ui.rect((i - 1) * (bw + gap), h - bh, bw, bh,
      { color = color, radius = o.radius or 2 })
  end
  return c
end
function ui.sparkline(values, o)
  o = o or {}
  local w, h = o.width or 160, o.height or 36
  local lo, hi = math.huge, -math.huge
  for _, v in ipairs(values) do
    if v < lo then lo = v end
    if v > hi then hi = v end
  end
  if hi == lo then hi = lo + 1 end
  local ps, n = {}, math.max(#values - 1, 1)
  for i, v in ipairs(values) do
    ps[i] = { (i - 1) * w / n, h - 2 - (v - lo) / (hi - lo) * (h - 4) }
  end
  local c = ui.canvas{ width = w, height = h }
  if o.fill then
    local fill = { { 0, h } }
    for i, p in ipairs(ps) do fill[i + 1] = p end
    fill[#fill + 1] = { w, h }
    c.shapes[#c.shapes + 1] = ui.area(fill, { color = o.fill })
  end
  c.shapes[#c.shapes + 1] = ui.line(ps, { color = o.color or "accent", width = o.stroke or 2 })
  if #ps > 0 then
    local last = ps[#ps]
    c.shapes[#c.shapes + 1] = ui.circle(last[1], last[2], 3, { color = o.color or "accent" })
  end
  return c
end

-- Actions an `on_event` handler may return (they pass Diri's click gate).
action = {}
function action.open(url) return { kind = "open_url", url = url } end
function action.session(id) return { kind = "open_session", session = id } end
function action.prompt(text, session) return { kind = "send_prompt", text = text, session = session } end
function action.ask_agent(name, payload) return { kind = "event", name = name, payload = payload } end

-- A list table that stays a list when empty.
function ui.list(t) return list(t) end
