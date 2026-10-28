# dirijor CLI

> Reference for the dirijor command-line tool. List, read, start and drive diri sessions, manage worktrees, layouts and notes, and script diri from a shell.

`dirijor` is diri's command-line tool. It talks to the same background Engine as the app, so anything you script with it shows up in the window. Agents use it too: diri's hooks and [MCP server](/docs/mcp/) run through it.

## Where it lives

| Platform | Path |
| --- | --- |
| macOS | `/Applications/diri.app/Contents/Resources/bin/dirijor` |
| Linux | `dirijor` on your `PATH`, installed by the `.deb` package |

The Engine also keeps a copy in `~/Library/Application Support/Dirijor/bin/dirijor` on macOS. Agents that diri starts get its path in `$DIRIJOR_CLI`, along with `$DIRIJOR_SESSION_ID` for their own session.

To use it from your own terminal on macOS, link it onto your `PATH`:

```sh
mkdir -p ~/.local/bin
ln -sf /Applications/diri.app/Contents/Resources/bin/dirijor ~/.local/bin/dirijor
```

The CLI finds the Engine through its socket. Set `DIRIJOR_SOCKET` to point at a different one.

## Conventions
- **Targets.** Most commands take a target: a full session id, a unique id prefix, or a unique part of the session title (case-insensitive). Commands documented with `ID` need the id itself. Every terminal diri opens has its own id in `$DIRIJOR_SESSION_ID`.
- **`--json`.** Prints one line of JSON instead of a table.
- **Arguments are literal.** `session run` passes everything after `--` as separate arguments. Nothing is run through a shell.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Failure, including bad arguments |
| `2` | Timed out |
| `3` | Session not found |
| `4` | Engine unreachable |
| `5` | Ambiguous target: the id prefix or title matches more than one session |

`run --wait` is the exception: it exits with the command's own status (`128 + N` when signal `N` ended it), `124` when `--timeout` passes, and `125` when `dirijor` itself fails.

## Overview

| Command | What it does |
| --- | --- |
| `dirijor list`, `new`, `send`, `capture`, `run` | Short scripting verbs. See [Scripting](#scripting). |
| `dirijor status [--json]` | List every session, archived ones included. |
| `dirijor activity [--limit N] [--json]` | Recent activity. `N` is 1 to 300, default 50. |
| `dirijor session ...` | List, read, drive, start and stop sessions. |
| `dirijor worktree ...` | List, create and remove git worktrees. |
| `dirijor workspace`, `tab`, `pane` | Change the window layout. |
| `dirijor artifacts SESSION [--json]` | Links, pull requests and listening ports found for a session. |
| `dirijor ports [--json]` | Listening ports across all sessions. |
| `dirijor events ...` | Stream or wait for Engine events. |
| `dirijor note ...` | Write and edit [notes](/docs/notes/). `notes` works too. |
| `dirijor notify ...` | Post a notification from a terminal. |
| `dirijor doctor` | Check the Engine, agents and state file. |
| `dirijor mcp-tools` | Print the MCP tool definitions as JSON. |
| `dirijor mcp-call --tool NAME` | Call one MCP tool with JSON from stdin. |
| `dirijor help` | Print usage. |

`dirijor hook`, `dirijor notify JSON` and `dirijor mcp-stdio` are called by agents that diri launches. You do not need to run them.

## Scripting
Short verbs for shell scripts, modelled on tmux. They do the same things as the `session` commands below.

| Command | Flags | What it does |
| --- | --- | --- |
| `list` | `--all`, `--json` | List sessions. Same as `session list`. |
| `new` | `--kind KIND` (default `shell`), `--cwd PATH`, `--name TEXT`, `--prompt TEXT`, `--host ID`, `--json` | Open a shell or agent tab. Prints only the new session id. |
| `send TARGET [--] TOKEN...` | `-l`, `--json` | Type into a session, like `tmux send-keys`. Each token is a key name or literal text; text tokens are joined without spaces. Reads stdin as literal text when no tokens are given. `-l` types every token literally. |
| `capture TARGET` | `--lines N`, `--scrollback`, `--json` | Print the visible screen as plain text, or the whole scrollback. Trailing blanks and empty rows are dropped, so `--lines N` gives the last N rows of output. |
| `run -- PROGRAM [ARG...]` | `--wait`, `--timeout SECONDS`, `--rm`, `--cwd PATH`, `--name TEXT`, `--host ID`, `--json` | Run a command in a new terminal tab and print its id. With `--wait`, block until it exits, print its output and exit with its status. `--rm` closes the tab afterwards. |

`send` key names (case-insensitive): `Enter`, `Tab`, `Escape`, `BSpace`, `Space`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Insert`, `Delete`, `F1` to `F12`. Prefix `C-` for Control, `M-` for Alt and `S-` for Shift, for example `C-c`, `M-b` or `S-Tab`. Anything else is typed as text.

```sh
id=$(dirijor new --cwd ~/code/app)
dirijor send "$id" "npm run dev" Enter
dirijor capture "$id" --lines 5
dirijor send "$id" C-c
dirijor run --wait --rm -- cargo test --workspace && echo green
```

## Sessions
`dirijor session` with no action is the same as `dirijor session list`.

| Command | Flags | What it does |
| --- | --- | --- |
| `session list` | `--all`, `--status PREFIX`, `--json` | List sessions. Archived ones appear only with `--all`. `--status` filters by status, for example `needsInput`. |
| `session get TARGET` | `--json` | Show id, title, agent, status, folder and host. |
| `session read TARGET` | `--source screen` or `scrollback`, `--lines N`, `--json` | Print the visible screen (default) or scrollback. `--lines` keeps the last N lines. |
| `session send TARGET [TEXT...]` | `--no-submit`, `--json` | Type text and press Return. Reads stdin when no text is given. `--no-submit` leaves it unsent. |
| `session key ID KEY` | `--ctrl`, `--alt`, `--shift`, `--cmd`, `--repeat`, `--json` | Press one key. `send` takes any target and several keys. |
| `session wait TARGET` | `--until STATUS` (repeatable), `--timeout SECONDS`, `--json` | Block until the session reaches a status. Default `--until done`, default timeout 600 seconds. |
| `session spawn KIND` | `--cwd PATH`, `--worktree`, `--branch NAME`, `--prompt TEXT`, `--title TEXT`, `--host ID`, `--json` | Start an agent, for example `claude` or `codex`. The folder defaults to the current one. |
| `session run -- PROGRAM [ARG...]` | `--cwd PATH`, `--host ID`, `--title TEXT`, `--json` | Run a command in a new terminal session. A remote run needs `--cwd` with an absolute path on that host. |
| `session fork TARGET` | `--json` | Fork the conversation into a new session (agents that support fork). |
| `session release TARGET` | `--remove`, `--json` | Stop the session. `--remove` also deletes its record. |
| `session archive TARGET` | `--undo`, `--json` | Archive the session, or unarchive it with `--undo`. |
| `session reconnect ID` | `--json` | Reconnect a remote session whose connection failed. |
| `session process ID` | `--json` | Show the process id, group, executable, folder and account. |
| `session terminal-title ID` | `--json` | Print the terminal title the program set. Local sessions only. |
| `session reset-terminal ID` | | Clear the screen, history, modes and title. The process is not touched. |

`--name` is accepted as an alias for `--title`. `--host` takes a host id from your [remote hosts](/docs/remote-hosts/).

For spawn usage, all supported options, and a local example, run
`dirijor session spawn --help` (or `-h`). `--help` anywhere in a spawn command
prints help instead, so it never starts a session or creates a worktree, and
works without a running Engine. Unknown options are rejected rather than ignored.

### Wait statuses

| `--until` value | Matches |
| --- | --- |
| `done`, `idle` | Idle |
| `working` | Working |
| `needsInput` (also `blocked`) | Waiting for you |
| `exited` | The process ended |
| `starting`, `unknown` | Those states |

### Keys
`KEY` is a single character or one of `enter` (or `return`), `escape` (or `esc`), `tab`, `backspace`, `delete`, `insert`, `home`, `end`, `up`, `down`, `left`, `right`, `pageup`, `pagedown`, `f1` to `f12`. Use `keypad:0` to `keypad:9` for the numeric keypad.

## Worktrees
`REPO` defaults to the current folder.

| Command | Flags | What it does |
| --- | --- | --- |
| `worktree list [REPO]` | `--json` | List the repository's worktrees. |
| `worktree create [REPO]` | `--branch NAME`, `--base REF`, `--json` | Create a worktree. Prints its branch and path. |
| `worktree remove REPO PATH` | `--force`, `--json` | Remove a worktree. |

See [Worktrees and review](/docs/worktrees/).

## Workspaces, tabs and panes
These commands change the layout the app shows. Each one prints the new layout as JSON. Add `--revision N` to fail if someone else changed the layout first.

| Command | What it does |
| --- | --- |
| `workspace list` | Print the current layout, with ids. |
| `workspace create NAME` | Add a workspace. |
| `workspace rename ID NAME`, `workspace remove ID`, `workspace move ID INDEX` | Rename, remove or reorder a workspace. |
| `workspace apply` | Apply a mutation read as JSON from stdin. |
| `tab create WORKSPACE SESSION` | Open a session as a tab. |
| `tab rename TAB TITLE`, `tab remove TAB`, `tab move TAB WORKSPACE INDEX`, `tab select WORKSPACE TAB` | Manage tabs. |
| `pane split TAB PANE SESSION EDGE` | Split a pane and put a session in the new half. `EDGE` is `left`, `right`, `above` or `below`. |
| `pane remove TAB PANE`, `pane focus TAB PANE`, `pane zoom TAB PANE` | Close, focus or zoom a pane. Use `none` as the pane to unzoom. |
| `pane move SOURCE_TAB PANE DEST_TAB TARGET_PANE EDGE` | Move a pane. `pane move-group` moves a whole split. |
| `pane swap TAB PANE TAB PANE` | Swap two panes. |
| `pane resize TAB SPLIT FRACTION` | Resize a split. |

## Events

| Command | Flags | What it does |
| --- | --- | --- |
| `events subscribe` | `--session TARGET` (repeatable), `--kind NAME` (repeatable), `--since-seq N`, `--count N`, `--timeout SECONDS`, `--json` | Print events as they happen. Stops after `--count` events or the timeout (default one day). |
| `events wait` | `--session TARGET`, `--until STATUS` or `--kind NAME`, `--timeout SECONDS`, `--json` | Wait for one status or event. `--until` needs `--session`. You cannot combine `--until` and `--kind`. |

## Notes
`dirijor note TEXT` writes a quick note for the current project. The first line is its title.

| Command | What it does |
| --- | --- |
| `note add [TEXT]` | Write a note. Reads stdin when TEXT is left out. Flags: `--title T`, `--project FOLDER` or `--inbox`, `--pin`, `--open`. |
| `note append NOTE TEXT` | Add Markdown to the end of a note. |
| `note todo TEXT` | Add a to-do to the project's "To-dos" note, or another note with `--to NOTE`. |
| `note list` | List notes. Flags: `--project FOLDER`, `--mentions SESSION` or `me`, `--all`, `--json`. |
| `note check NOTE TODO` | Tick a to-do. `--undo` unticks it. |
| `note link NOTE TODO SESSION` | Attach a session to a to-do. |
| `note show NOTE` | Print a note as Markdown. |
| `note edit NOTE --old TEXT --new TEXT` | Replace exact text. `--all` replaces every match. An empty `--new` deletes. |
| `note replace-section NOTE HEADING` | Replace everything under a heading with stdin. |
| `note history NOTE [VERSION]` | List earlier versions, or print one. |
| `note restore NOTE VERSION` | Bring back an earlier version. The current text stays in history. |
| `note path` | Print the notes folder. |

`NOTE` is a note id or part of its title.

## Notifications
`dirijor notify --title TEXT --body TEXT` prints a notification escape sequence to the terminal, so it works in local and remote sessions without a socket. diri shows it like an agent notification.

## MCP from the shell
`dirijor mcp-tools` prints every tool the MCP server offers. `dirijor mcp-call --tool NAME` reads the tool's input as JSON from stdin and prints `{"ok": ...}` or `{"error": ...}`. Write tools need a live diri session identity, so call them from inside a diri session. See the [MCP tool reference](/docs/mcp-tools/) and the [security model](/docs/security/).

## Examples
These work in fish, zsh and bash.

Find sessions waiting for you:

```sh
dirijor session list --status needsInput
```

Start Codex in a new worktree with a task, then wait for it to finish:

```sh
dirijor session spawn codex --worktree --branch fix-login --title fix-login --prompt "Fix the failing login test"
dirijor session wait fix-login --until done --until needsInput --timeout 1800
```

Read the last 20 lines of a session's screen:

```sh
dirijor session read fix-login --lines 20
```

Run tests in their own terminal session and wait until the command exits:

```sh
dirijor session run --title tests -- cargo test --workspace
dirijor session wait tests --until exited
```

Dismiss a prompt with Escape, then send a follow-up. `session key` needs the exact id from `session list`; replace `SESSION_ID` with it:

```sh
dirijor session key SESSION_ID escape
dirijor session send fix-login "Use the existing helper instead"
```

Pipe a file into a new note:

```sh
cat plan.md | dirijor note add --title "Release plan" --pin
```
