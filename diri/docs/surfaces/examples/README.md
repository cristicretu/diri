# Example Diri Apps

Two apps an agent might write, used by Diri's tests and screenshots. They are
examples only: Diri ships no built-in app, and agents start from a blank
`app.lua` (see `dirijor surface schema`).

- `focus-ledger/`: a personal tracker. `sync` reads `git log` (after approval)
  into a streak, a 20-week heatmap and an hour histogram drawn with
  `ui.canvas`; habits are the app's own state.
- `study-queue/`: a lecture player. Media cards with YouTube thumbnails, a
  progress ring and a minutes chart drawn as vector shapes, a Next button and
  a speed choice handled by `on_event`; `sync` refreshes the list from a
  playlist feed (after approval).

Try one: `dirijor surface create "Focus Ledger" --global --lua focus-ledger/app.lua --data focus-ledger/data.json`.
