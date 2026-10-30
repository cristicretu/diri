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

-- A mode: ⌘⇧R, then h/l resize until Escape.
diri.keymap.mode("resize", { exclusive = true })
diri.keymap.set("cmd-shift-r", { mode = "resize" })
diri.keymap.set("resize/h", "pane-shrink-width")
diri.keymap.set("resize/l", "pane-grow-width")
```

Save the file and Diri reloads it. From a terminal:

```sh
dirijor script -e 'print(#diri.session.list().sessions)'
dirijor script watch.lua --for 60      # runs while it has handlers, timers or waits
dirijor script deploy.lua branch=main  # KEY=VALUE (or --args JSON) fill diri.args
dirijor script --check init.lua        # validate without running; exit 1 on an error
dirijor script                         # interactive prompt
dirijor keymap                         # every key, what it runs, and init.lua:LINE
dirijor do script.tidy                 # a palette command, run by name
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
- **Coroutines, not threads.** Every entry into Lua (the file's top level, a
  handler, a key, a command, a timer) runs as a coroutine the runtime owns.
  `diri.sleep`, `diri.wait` and `diri.exec` suspend that coroutine and
  return to the script thread's loop, which keeps handling events, keys and
  other callbacks; the coroutine resumes when its time, event or process
  comes, with a fresh CPU budget. Nothing ever blocks the UI thread, and a
  waiting script costs no CPU.

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
| `diri.keymap.set(keys, target, {desc?}) → id` | `target` is an action name, `{action=, args=}`, `{mode = "name"}` (enter a mode), `{mode = false}` (leave it), or a function. `"mode/key"` binds a key inside a mode. |
| `diri.keymap.del(keys)` | Free a key from its default command (or a mode's key). |
| `diri.keymap.mode(name, {exclusive?, once?, desc?})` | Declare a key mode. |
| `diri.keymap.enter(name)`, `diri.keymap.exit()` | Switch modes from a callback (app only). |
| `diri.command{name, title?, run, args?, desc?, destructive?} → id` | A palette row, also the window action `script.NAME`; `run` is an action name or a function whose return value is the action's result. |
| `diri.notify(message, {level?, detail?})` | A toast (`info`, `success`, `warning`, `error`). |
| `diri.log(...)`, `print(...)` | The script log. |
| `diri.after(s, fn)`, `diri.every(s, fn)` | Timers (at most 256; repeats at least 0.5 s apart). |
| `diri.sleep(ms)` | Pause this callback, or the top level, without blocking anything else. |
| `diri.wait(event, filter?, timeout_ms?) → payload \| nil` | Pause until `event` happens with a matching payload; `nil` after the timeout. |
| `diri.require_grant{exec = {...}} → bool` | Ask to run programs (see Sandbox). |
| `diri.exec(argv, {timeout?, cwd?, env?, stdin?, on_output?}) → result` | Run a granted program. |
| `diri.stop(code?)` | End a `dirijor script` run, with this exit code. |
| `diri.json.encode/decode`, `diri.inspect` | Values. |
| `diri.args` | What `dirijor script FILE k=v` or `--args JSON` passed; `{}` otherwise. |
| `diri.session_id` | The session the script runs in (`$DIRIJOR_SESSION_ID`, or `--session`), else `nil`. |
| `diri.methods`, `diri.events`, `diri.context`, `diri.version` | What is available here. |

**Callback context.** A key or palette command function receives one table:
`{session, pane, window, origin}`, the focused session and the pane that
shows it, the window's number, and what started it (`key`, `mode`,
`palette`, `cli`, `agent`, or `script` when a script ran its own command).
A command run by name also gets `args`, and `actor` when an Agent asked.

```lua
diri.keymap.set("cmd-alt-g", function(ctx)
  diri.session.send_text({ sessionID = ctx.session, text = "git status", submit = true })
end)
```

**Waiting.** `diri.sleep` and `diri.wait` turn a callback into a sequence
of steps. They work directly in a callback or the file's top level, not in
a coroutine the script made itself. A `filter` table matches when each of
its fields equals the payload's, or the `id` of the payload's record, so
`{session = id}` matches `event.session.id`; a function filter decides for
itself.

```lua
diri.command({ name = "build", run = function(ctx)
  local run = diri.session.spawn({ kind = "shell", cwd = "/src/app", name = "build" })
  local done = diri.wait("session.exited", { session = run.id }, 5 * 60 * 1000)
  return { code = done and done.code }
end })
```

```sh
dirijor do script.build        # prints {"code": 0} when the build ends
```

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
`session.notification` (OSC 9/777 and agent hooks), `session.cwd` (a
shell's live directory after `cd`), `session.program` (a shell's
foreground program, `to = nil` back at the prompt), `session.bell`,
`workspace.updated`, `schedule.updated`, and `focus` (the window's selected
session; app only). Every `session.*` event that carries a record also
carries `kind`, the Agent's manifest id.

```lua
diri.on("session.program", function(e)
  if e.to == "npm" then diri.notify(e.session.title .. " is installing") end
end)
diri.on("session.bell", function(e) diri.log("bell in", e.session_id) end)
```

`session.cwd` and `session.program` come from fields the Engine already
publishes in each record (`terminalCwd`, `foregroundProgram`). A bell is a
flag the parser sets (one relaxed store, never the inbox lock); the
Engine's 150 ms publish tick turns it into one `session.bell` event per
session per tick, sent only to subscribers that ask for it by name.

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

**Modes (now).** `diri.keymap.mode(name, {exclusive, once})` declares a
mode; `"name/key"` binds a single keystroke inside it; a binding to
`{mode = "name"}` enters it and `{mode = false}` leaves it, in the window,
without a round trip to the script thread. While a mode is on, its keys are
read before every other binding. `exclusive` swallows every key the mode
does not bind; `once` ends the mode after the next key, bound or not, which
makes a tmux-style prefix. Escape leaves any mode that does not bind it
itself. A mode that is not `once` shows a short notice naming it.

The mode keys are read by one keystroke interceptor, installed the first
time a script declares a mode. With no mode on it costs one relaxed atomic
load per key; with no mode declared it does not exist.

**Where a key came from.** `dirijor keymap` prints every binding the window
has: `KEY  ACTION  ARGS  SOURCE`, where SOURCE is `default`, `recorded`
(Settings › Shortcuts) or the script line that bound it (`keys.lua:12`),
followed by the script's modes. `--json` prints the same as data. It is
the window action `list-keys`, so Agents can read it too.

Next: showing a script key in the palette and menus next to its command.

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
thread. Each is also published in the window's action catalog as
`script.NAME`, beside the built-in actions, so `dirijor do script.NAME
key=value`, `dirijor actions` and an Agent's `perform_action` reach it. The
command's function receives the arguments as `ctx.args`, and what it
returns (after any waits) is the action's result. `desc` becomes the
action's description, and `destructive = true` publishes it as destructive,
so an Agent needs the same permission it needs to close a tab. The catalog
is published again whenever the script's commands change; a script that
runs one of its own commands by name runs it directly, not through the
window. Reload Scripts is a regular command, so it is also an action:
`dirijor do reload-scripts`.

## Sandbox and limits

- **Libraries:** `string` (no `string.dump`), `table`, `math`, `utf8`,
  `coroutine`, and an `os` with only `time`, `clock`, `date` (UTC) and
  `difftime`. No `io`, `debug`, `package`, `dofile`, `loadfile`,
  `os.execute`, `os.getenv`, `os.remove`. `load` accepts text only: a binary
  chunk could break out of the sandbox.
- **No processes, file system or network unless granted.** Default deny.
  A script declares what it needs with `diri.require_grant{exec = {"git"}}`
  and gets back whether it was granted. Who answers:
  - **`init.lua` and the files it requires**: the person's own config. The
    line that asks is the grant; the script log records it.
  - **`dirijor script`**: asked once at the terminal, "this run", "always
    for this version of the file", or no. "Always" is stored, owner-only,
    in `script-grants.json` in Diri's state directory with the file's
    SHA-256, so any edit asks again. Without a terminal nothing is granted
    unless the command line says so: `--allow-exec git`.
  - **`--check`** records what a file asks for and grants nothing.

  `diri.exec(argv, opts)` then runs a granted program, matched by
  `argv[1]` exactly: an argv, never a shell string; `timeout` in seconds
  (default 60, at most 600; past it the process group is killed); `cwd`
  (absolute, or `~/...`); `env` added to the inherited environment;
  `stdin` text; `on_output(line, stream)` called per line as output
  arrives. Output is read on threads of its own through a bounded channel
  and kept up to 1 MiB per stream (`truncated` says when it was cut). It
  returns `{ok, code, signal, stdout, stderr, timed_out, truncated,
  error?}`. In a callback or the top level the coroutine waits for the
  process and the script thread goes on; elsewhere (the REPL) the call
  blocks, with the wait not counted against the budget. At most 16
  processes run at once per script, and a reload or `diri.stop()` kills
  what the old script started.

  ```lua
  if diri.require_grant({ exec = { "git" } }) then
    local log = diri.exec({ "git", "log", "-1", "--format=%s" }, { cwd = "~/src/app" })
    diri.notify(log.ok and log.stdout or log.stderr)
  end
  ```

  Next: `fs = {"~/notes"}` and `net = {"api.github.com"}` grants, asked the
  same way.
- **CPU:** each entry into Lua (the file itself: 1 s; one handler, key,
  command or timer: 100 ms) has a wall-clock budget checked every thousand VM
  instructions. Time spent waiting on the Engine is added back, so a script
  that makes several calls is not penalised for the round trips. A callback
  past its budget is stopped with an error. A coroutine resumed after
  `diri.sleep`, `diri.wait` or `diri.exec` gets a fresh budget for the next
  step; at most 256 can be waiting at once.
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
- `dirijor script --check FILE` validates a file without running it: it
  compiles the file, runs its top level in a throwaway state, and stops at
  the first call that would reach outside the script (an Engine method, a
  window action, a process); parameters are still checked against their
  schemas before that. It prints what the file set up and where it stopped,
  and exits 1 on an error:

  ```
  $ dirijor script --check ~/.config/diri/init.lua
  /Users/me/.config/diri/init.lua: ok
    9 keys, 1 mode, 3 commands, 2 handlers, 0 timers
    asks to run: git
  ```

## Footprint

- **No `init.lua`:** no thread, no Lua state, no file watcher, no Engine
  connection, no key interceptor. The only per-event cost is one relaxed
  atomic load when the selected session changes. The Engine's bell flag is
  one relaxed store per ring and one swap per session per 150 ms publish
  tick, whether or not a script listens.
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
  sending, generated functions, events (including cwd, program and bell),
  keys, modes and commands with their source lines and context, timers,
  sleeps and waits (filters, timeouts, only in the runtime's coroutines),
  command answers after a wait, `--check` stopping at the first call, exec
  denied until granted, streaming, timeouts, the grant store tied to the
  file's hash, error de-duplication, and that every file in
  `scripting/examples/` loads and handles events.
- `diri-app`: the key layer's precedence and validation, a script key and a
  palette command reaching the script thread with their context in a
  headless GPUI window, modes (exclusive, once, Escape, mode keys kept out
  of the keymap), script commands published and answered through
  `app.perform`, `list-keys`, and that `scripting-api.md` and the type stubs
  match the catalogs (`DIRI_UPDATE_DOCS=1 cargo test -p diri-app
  scripting_reference`).
- `diri-terminal-state`: a bell is one flag; a BEL that ends an OSC is not.
- `dirijor-mcp`: `dirijor script` against a real Engine and a stand-in app,
  calling methods and actions, receiving a live event, `diri.args`,
  `diri.session_id`, `diri.stop(code)`, top-level waits, `--check`,
  `keymap`, exec with and without `--allow-exec`, and a real terminal's
  bell reaching `diri.wait`.
