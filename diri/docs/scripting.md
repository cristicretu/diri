# Scripting Diri

Diri can be driven by Lua scripts: an `init.lua` that binds keys, adds palette
commands and reacts to what sessions do, and one-off scripts run with
`dirijor script`. Scripts use the same API as agents and the CLI: every window
action, and every Engine method that has a JSON Schema. Nothing about a
script's API is written by hand; it is generated from those catalogs, so it
cannot drift from the Engine.

- Generated reference (events, every method and action): [scripting-api.md](scripting-api.md)
- Editor type stubs for the Lua language server: [scripting/types/diri.lua](scripting/types/diri.lua)
- Examples: [scripting/examples/](scripting/examples/)

This document is the design. "Now" marks what the first version ships; "Next"
marks the parts designed here and built later.

## Quick start

```lua
-- ~/.config/diri/init.lua
diri.keymap.set("cmd-shift-e", "split-pane-best-fit")
diri.keymap.set("ctrl-a c", "new-terminal")

diri.on("session.status", function(e)
  if e.kind ~= "shell" and e.from == "working" and e.to == "needs_input" then
    diri.notify(e.session.title .. " needs you", { level = "warning" })
  end
end)

diri.command({ name = "tidy", title = "Tidy Panes", run = function()
  diri.action("equalize-panes")
end })
```

Save the file and Diri reloads it. From a terminal:

```sh
dirijor script -e 'print(#diri.session.list().sessions)'
dirijor script watch.lua --for 60      # runs while it has handlers or timers
dirijor script                         # interactive prompt
dirijor do reload-scripts              # or ⌘K → Reload Scripts
```

## Language: Lua 5.4

**Choice:** Lua 5.4 through [`mlua`](https://github.com/mlua-rs/mlua), built
from the vendored C sources (`features = ["lua54", "vendored", "serialize"]`).

Why Lua:

- **People already write it for this.** Terminal and editor configs (wezterm,
  Neovim, Hammerspoon, mpv) made Lua the language people reach for when they
  configure a tool from a dotfile. A keymap in Lua reads the same as the one
  they already have.
- **It is small and sandboxable by construction.** The interpreter is about
  250 KB of C. Nothing reaches the file system, processes or the network
  unless the host opens a library, so the sandbox is "open only `string`,
  `table`, `math`, `utf8`, `coroutine`" rather than a deny list to keep
  complete. An empty state costs about 60 KB.
- **It can be stopped.** Lua's instruction hook lets the host stop a callback
  that runs too long, and its allocator hook caps memory. Both are enforced
  from Rust; a script cannot turn them off.

Alternatives considered:

| Option | Why not (now) |
| --- | --- |
| Luau (also through `mlua`) | Built-in sandbox and interrupts, gradual types. A dialect people know less, and `mlua` makes it a feature switch later if typed scripts matter more than familiarity. |
| JavaScript (QuickJS, Boa) | Larger runtime, async-first semantics that fit config badly, no dotfile tradition. |
| WebAssembly | Right for compiled third-party plugins (see External plugins), wrong for a config file: it needs a toolchain to write one line. |
| Rhai, Starlark | Safe, but a language nobody has written before using Diri. |
| Python | Heavy to embed and cannot be sandboxed in-process. |

**The dependency.** `mlua` is the maintained, widely used Rust binding for
Lua. It reduces risk compared to the alternatives: the only `unsafe` FFI lives in one
audited crate instead of bindings written for Diri; `vendored` compiles a
pinned Lua with `cc`, so no system Lua is linked and every build runs the same
interpreter; and its `serialize` feature converts values through `serde`, the
same types the Engine decodes, instead of a converter of our own. Only
`diri-script` depends on it. The Engine, `diri-remote` and the Holder never
link Lua.

## Where scripts live

| Path | Trust | Status |
| --- | --- | --- |
| `~/.config/diri/init.lua`, or `$DIRI_CONFIG_DIR/init.lua`, or `$XDG_CONFIG_HOME/diri/init.lua` | The person's own config: trusted. | Now |
| Other `.lua` files beside it, loaded with `require("name")` or `require("dir.name")` | Same as `init.lua`. | Now |
| `<project>/.diri/*.lua` | Untrusted until the person allows it. | Next |

`require` takes only plain module names and resolves them inside the config
directory; `..`, absolute paths and symlinks that lead outside are refused.

**Project scripts (next).** A repository can carry `.diri/*.lua` (a layout, a
"run tests" command, a status rule). They never run on their own. The first
time Diri opens a project with scripts, the window asks once, naming the
files; the answer is stored with the project path and the files' SHA-256, and
any change to a file asks again. Project scripts run in their own Lua state,
see only sessions in that project, and can add palette commands and events but
not global keys. A project script cannot be trusted by another script.

## Where scripts run

```
 Diri app process                                   Engine process
 ┌───────────────────────────────┐                  ┌──────────────────────┐
 │ UI thread (GPUI)              │                  │ control socket       │
 │  keymap ─ RunScriptKey ─┐     │  layer, toasts   │  api.* / app.perform │
 │  palette ─ Script{id} ──┤     │◀───────────────┐ │  events.subscribe    │
 │  focus change ──────────┤     │                │ └──────────▲───────────┘
 │                         ▼     │                │            │
 │  diri-script thread: one Lua state ────────────┘            │
 │    diri.call / diri.action ───────── own connection ────────┤
 │    diri.on (Engine events) ◀── reader thread, own connection┘
 └───────────────────────────────┘
```

- **One thread, one Lua state per app.** Engine events, key presses, palette
  commands, timers and reloads arrive on one channel, so callbacks never race
  and never run on the UI thread.
- **Never on the hot path.** Scripts see Engine events after the Engine has
  published them, on their own connection. Nothing a script does sits between
  the PTY read, the terminal parser, diff construction or the attach writer,
  and a slow script cannot delay any of them. The Engine never runs Lua.
- **Window actions** go through the Engine's `app.perform`, the same route as
  `dirijor do` and agents, so arguments are checked against the action's
  schema and the window runs the same GPUI action its key would. A key bound
  to a window command with no arguments skips the round trip: it is bound to
  that command's GPUI action directly.
- **`dirijor script`** runs the same runtime in the CLI process, with stdout as
  the log and stderr for notices. Keys and palette commands only apply inside
  the app.

## API

### Core (now)

| Function | |
| --- | --- |
| `diri.on(event, fn) → id` | Run `fn(payload)` on an event. Unknown names are an error that lists the valid ones. |
| `diri.off(id)` | Remove a handler or timer. |
| `diri.call(method, params?, {timeout?}) → result` | Any Engine method; params are checked against its JSON Schema before sending. |
| `diri.<namespace>.<method>(params?, opts?)` | Generated per method: `diri.session.list()`, `diri.worktree.create{...}`. |
| `diri.action(name, args?) → result` | A window action (needs the app). |
| `diri.actions.<name>(args?)` | Generated access: `diri.actions.split_pane_right()`. |
| `diri.keymap.set(keys, target, {desc?}) → id` | `target` is an action name, `{action=, args=}`, or a function. |
| `diri.keymap.del(keys)` | Free a key from its default command. |
| `diri.command{name, title?, run, args?} → id` | A palette row; `run` is an action name or a function. |
| `diri.notify(message, {level?, detail?})` | A toast (`info`, `success`, `warning`, `error`). |
| `diri.log(...)`, `print(...)` | The script log. |
| `diri.after(s, fn)`, `diri.every(s, fn)` | Timers (at most 256; repeats at least 0.5 s apart). |
| `diri.stop()` | End a `dirijor script` run. |
| `diri.json.encode/decode`, `diri.inspect` | Values. |
| `diri.methods`, `diri.events`, `diri.context`, `diri.version` | What is available here. |

JSON `null` reads as `nil`. Whole numbers made by arithmetic (`80 * 1.0`) are
sent as integers.

### Engine methods

Every method in the Engine's catalog (`dirijor api list`) except the ones that
would let a script take over the app's or the Engine's own role:
`hello`, `events.subscribe`/`events.wait` (use `diri.on`),
`app.actions.publish`/`app.reply` (only the window answers actions),
`client.set_active`, `hook.report`, and the `daemon.*shutdown` methods.
That covers sessions (spawn, send text and keys, read the screen, rename,
archive, fork, resume), panes and tabs (`workspace.mutate`), worktrees, notes
through their sessions, schedules, tasks, hosts and accounts. The list, with
parameters and result types, is in [scripting-api.md](scripting-api.md).

### Events

Now: `session.created`, `session.status` (`from`/`to` in `starting`, `idle`,
`working`, `needs_input`, `exited`, `unknown`), `session.exited` (`code`,
`signal`, `reason`), `session.title`, `session.removed`,
`session.notification` (OSC 9/777 and agent hooks), `workspace.updated`,
`schedule.updated`, and `focus` (the window's selected session; app only).
Every `session.*` event carries the record and `kind`, the Agent's manifest id.

The Engine publishes whole session records; the script thread keeps the last
status and title per session and turns each update into the transitions it
contains, so a handler fires once per change, not once per record.

Next:

- **`session.output`** with `{session?, pattern}`: a match on new output
  lines. Matching runs in the Engine on lines the parser has already
  committed, on a separate low-priority worker fed by a bounded queue, using
  the `regex` crate (linear time). A slow or flooding matcher drops matches
  and reports it; it never delays the PTY reader. Patterns are registered per
  subscription, so with no subscriber there is no matching at all.
- **`session.bell`**, **`session.cwd`** and **`session.program`** (the
  foreground program the tab title already tracks).
- **`window.*`**: window focus, tab and pane focus inside a tab.

### Keymap layering

From bottom to top; the newest binding on a key wins within a layer:

1. Diri's shipped defaults (`commands.rs`).
2. `init.lua` (now).
3. Trusted project scripts, while a session of that project is selected (next).
4. Shortcuts the person records in Settings › Shortcuts or from the palette.

Script keys are bound without a context after every default, which in GPUI
ranks them over context-bound defaults, and a key the person recorded is
skipped (and logged), so recorded shortcuts always win. Keys are written the
way Settings stores them: `cmd-shift-d`, `ctrl-alt-left`, or a sequence
`"ctrl-a c"`. A misspelled key name is reported instead of being bound to
something that never fires.

Next: modes (`diri.keymap.mode("resize", {exclusive = true})`, bindings that
apply only inside a mode, entered and left by actions), and showing a script
key in the palette and menus next to its command.

### Decorations (next)

`diri.decorate{session = id, badge = "3", tint = "warning", text = "tests ✓"}`
sets data the sidebar row and tab render: a short badge, a tint from the
theme's semantic colors and one line of text, all cleared with
`diri.decorate{session = id}`. Decorations are values, not drawing code, so the
UI stays in Rust and renders them in its normal frame; updates are coalesced to
one per session per frame. A statusline segment works the same way
(`diri.status{id = "branch", text = ...}`).

### Palette commands

`diri.command` rows appear in ⌘K with a "Script" chip and run on the script
thread. Reload Scripts is a regular command, so it is also an action:
`dirijor do reload-scripts`.

## Sandbox and limits

- **Libraries:** `string` (no `string.dump`), `table`, `math`, `utf8`,
  `coroutine`, and an `os` with only `time`, `clock`, `date` (UTC) and
  `difftime`. No `io`, `debug`, `package`, `dofile`, `loadfile`,
  `os.execute`, `os.getenv`, `os.remove`. `load` accepts text only: a binary
  chunk could break out of the sandbox.
- **No file system or network unless granted.** Now nothing is granted.
  Next: a script declares what it needs (`diri.require_grant{exec = {"git"}}`,
  `fs = {"~/notes"}`, `net = {"api.github.com"}`); the window asks the person
  once and stores the answer with the file's hash. Granted processes run with
  an argv (never a shell string), a timeout and bounded output, from the
  script thread.
- **CPU:** each entry into Lua (the file itself: 1 s; one handler, key,
  command or timer: 100 ms) has a wall-clock budget checked every thousand VM
  instructions. Time spent waiting on the Engine is added back, so a script
  that makes several calls is not penalised for the round trips. A callback
  past its budget is stopped with an error.
- **Memory:** 64 MiB per Lua state.
- **Authority:** a script acts as the person, like the CLI: its window
  actions are not attributed to an agent and destructive actions are allowed.
  It cannot do anything the CLI cannot.

## Reload and errors

- The config directory is watched (only while a script is running). A change
  to any `.lua` file reloads after 120 ms of quiet: a new Lua state loads the
  file, and only if that succeeds does it replace the running one. A broken
  edit leaves the last good script running.
- Errors are shown in a toast ("Script error in diri.on(\"focus\")" with the
  first line of the message) and written in full, with a traceback, to
  `scripts.log` in Diri's logs directory (owner-only, rolled over past
  1 MiB). The same error toasts at most once per 10 seconds; the log keeps
  every occurrence. `dirijor script` prints them instead.
- Reload Scripts (⌘K) starts a newly created `init.lua` without restarting
  the app.

## Footprint

- **No `init.lua`:** no thread, no Lua state, no file watcher, no Engine
  connection. The only per-event cost is one relaxed atomic load when the
  selected session changes.
- **With a script:** one script thread, one Engine connection for requests,
  one directory watcher and about 60 KB plus what the script allocates. A
  second thread and connection exist only while a script listens to Engine
  events.
- **Per session:** nothing: no process, thread or connection. A script that
  listens to session events keeps the last status and title of each session
  (a few bytes) to report transitions.

## Remote sessions

Scripts always run on the local machine. Remote sessions are ordinary records
in the local Engine, so `diri.session.send_text`, `read_screen` or events about
a session on another host go through the local Engine, which reaches the
remote Helper over its existing transport. No Lua, script file or new
capability is ever installed or run on a remote host, and the Helper protocol
does not change.

## External plugins (next)

A plugin is a directory with a `plugin.toml` (name, version, entry file,
requested grants) installed under `~/.config/diri/plugins/`. It loads in its
own Lua state with the same API, its grants are asked for on install, and
`require` resolves inside the plugin only. Plugins that need speed or another
language can ship a WebAssembly module behind the same manifest later; the API
surface they get is the same generated catalog.

## Testing

- `diri-script`: the sandbox (no files, processes or bytecode; `require` stays
  in its directory), the CPU and memory limits, schema checks before
  sending, generated functions, events, keys and commands, timers, error
  de-duplication, and that every file in `scripting/examples/` loads and
  handles events.
- `diri-app`: the key layer's precedence and validation, a script key and a
  palette command reaching the script thread in a headless GPUI window, and
  that `scripting-api.md` and the type stubs match the catalogs
  (`DIRI_UPDATE_DOCS=1 cargo test -p diri-app scripting_reference`).
- `dirijor-mcp`: `dirijor script` against a real Engine and a stand-in app,
  calling methods and actions and receiving a live event.
