# Diri Apps

A Diri App is a native app you write in Lua; Diri draws it with GPUI inside the
window (no web view, no server). Nothing is built in: every app's title, icon,
colors, layout and data are yours. A saved app (scope global or project) is a
sidebar row and a ⌘K result; a session app (the default) opens in your
session's right panel, and the person can save it to the sidebar.

## Ops
- create {title, scope?: session|global|project, icon?, accent?, style?, lua?, data?, app?: {sync}, pinned?, agents?}
  icon: an emoji ("🎧"), a built-in icon name ("chart.bar"), or an image in the
  app folder ("icon.svg"). accent: "#rrggbb". style: {density: comfortable|compact,
  canvas: plain|tinted, header: full|minimal, radius}.
- update {surface, lua?, data?, title?, subtitle?, icon?, accent?, style?, app?, pinned?, agents?}
  Writing lua returns check: {ok, nodes, render_ms} or the Lua/shape error.
- write_file {surface, path, content} (lua modules, json, svg/txt/md; images via assets),
  read_file {surface, path?}, files {surface}, data {surface, data?}, check {surface}
- get, list, events {surface, after?}, history, restore, promote, delete, schema

## app.lua
```lua
app = { title = "Study queue", icon = "🎧", accent = "#f97316" }   -- optional; overrides the manifest

function init(ctx) return { playing = 1 } end        -- first state

function render(state, data, ctx)                    -- pure: returns components
  return ui.col{ gap = 12, padding = 16,
    ui.text("Up next", { size = 22, weight = "bold" }),
    ui.media{ image = data.videos[state.playing].thumb, title = data.videos[state.playing].title,
              badge = data.videos[state.playing].length, url = data.videos[state.playing].url },
    ui.row{ gap = 8, ui.button{ id = "next", label = "Next", variant = "primary" }, ui.spacer(),
            ui.sparkline(data.minutes, { width = 120, height = 28, fill = "#f9731633" }) },
  }
end

function on_event(state, event, data, ctx)           -- clicks, ticks, choices, text
  if event.type == "action" and event.name == "next" then state.playing = state.playing % #data.videos + 1 end
  return state                                        -- optionally: return state, { action.open(url) }
end

function sync(ctx)                                    -- refreshes data.json, no agent needed
  local r = ctx.fetch("https://api.example.com/queue") -- needs the person's approval
  return { videos = ..., minutes = ... }
end
```
event: {type = "action"|"check"|"toggle"|"select"|"checkbox"|"input"|"tab", id, name, value, payload}.
ctx: {now (ms), theme ("dark"|"light"), layout ("page"|"panel"), width (points, about), sessions =
{{id, title, status, kind}}, data (in sync)}. Lay out for ctx.layout: a session app draws in a
narrow right panel.

## ui
Layout: ui.box/ui.row/ui.col{children..., gap, padding, padding_x, padding_y, bg, border,
border_width, radius, width (points|"fill"), height, min_width, max_width, grow, align
(start|center|end|stretch), justify (start|center|end|between|around), opacity, wrap},
ui.grid{columns, ...}, ui.scroll{height, ...}, ui.card{title, subtitle, ...}, ui.section{title, ...},
ui.tabs{ {id, label, children...}, ... }, ui.spacer(), ui.divider().
Content: ui.text(s, {size 9-64, weight regular|medium|semibold|bold, color, mono, truncate}),
ui.heading(s, level), ui.markdown(s), ui.code(s, lang), ui.mermaid(src), ui.image(src, {height}),
ui.badge(label, {tone}), ui.metric{label, value, delta, trend}, ui.progress{value, max, label},
ui.table{columns, rows}, ui.link(url, {label, detail, icon, badge}), ui.media{image, title,
subtitle, badge, url, aspect}, ui.session(id).
Charts: ui.canvas{width, height, ui.rect(x,y,w,h,{color,radius}), ui.circle(x,y,r,{color}),
ui.line(points,{color,width}), ui.area(points,{color})}; ui.bars(values, {width, height, color,
highlight, highlight_color}); ui.sparkline(values, {width, height, color, fill}).
Controls: ui.button{id, label, variant primary|secondary|danger, on, payload},
ui.checkbox{id, label, value}, ui.toggle{id, label, value}, ui.select{id, options, value},
ui.input{id, placeholder, value}, ui.checklist{id, items = {{id, text, checked}}}.
Colors: primary secondary tertiary accent success warning danger info background surface fill
border, or "#rrggbb[aa]". They follow light and dark.
Actions (from on_event): action.open(url), action.session(id), action.prompt(text, session?),
action.ask_agent(name, payload?). They run only because the person clicked, and prompts go only
to the app's owner or its agents list.

## Sync
app: {sync: {every_s?: 15-3600, on_open?: true, timeout_s?: 1-120}}. sync runs on open, on
Refresh, and every every_s seconds only while the app is on screen, never in the background.
ctx.read(path) reads the app folder; ctx.log(msg); ctx.exec({argv...}, {stdin}) and
ctx.fetch(https_url, {headers}) work only after the person approves this exact app.lua: editing
it asks again. Processes get a clean environment and the app folder as cwd.

## Limits
Each call: 32 MiB (64 for sync), 20M instructions, 250 ms (sync: its timeout). 12 levels,
1500 components, 200 children, 16 KiB per text, 2000 shapes per canvas, 1 MiB per surface and
per data.json. Links: https http mailto diri. Images: https or a file in the app folder.

## Without Lua
A surface can also be plain JSON components (root, or update ops: set/replace/remove/insert by
id). The same components, the same limits; Lua apps return exactly this format.
