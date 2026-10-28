# Terminal interactions

- Hover a URL or file reference to underline it and show the pointer cursor.
  Hover feedback stays on the link without a bottom banner covering output.
  Cmd-click opens on release; dragging cancels opening. Named OSC 8 links work
  when the session's Helper supports terminal annotations.
  Local file links open in the Mac's default app, including HTML previews and
  images outside the workspace. Relative paths use the session's working
  directory; line/column suffixes are removed before opening the file.
  Cmd-Shift-D continues to open the code sidebar explicitly.
- Right-click for Open/Copy Link, Copy Selection, Find Selection, Paste,
  Keyboard Copy Mode, Open Scrollback in Editor, and message/prompt navigation.
  In mouse-aware terminal applications, Option-right-click opens Diri's menu.
- Double-click then drag selects whole words. Triple-click then drag selects
  complete lines. Option-Shift-drag selects a rectangular column. Option-drag
  and Shift-drag continue to select locally in mouse-aware applications;
  Shift with the middle or right button still reaches the application.
- The wheel follows the program: Diri scrolls its own history unless the
  program reads the mouse, which then gets xterm wheel reports (sideways
  scrolling as buttons 6/7, Shift/Option/Control as modifier bits). A program
  on the alternate screen that does not read the mouse (`less`, `man`, an
  editor without mouse support) gets Up/Down arrow keys per line instead,
  unless it turns alternate scroll (DECSET 1007) off. Mouse encodings
  supported: X10-compatible (1000/1002/1003) and SGR (1006). The obsolete
  X10 press-only mode (9) is ignored, and UTF-8 (1005), urxvt (1015) and
  SGR-pixel (1016) coordinates fall back to the X10-compatible or SGR cell
  form; a DECRQM query reports 1016 as permanently reset.
- Drag beyond the top or bottom edge to extend a selection through retained
  history. Soft-wrapped lines copy without invented line breaks; hard newlines
  remain. Successful copies show a brief confirmation.
- Cmd-Option-F searches the first selected line. Searches containing uppercase
  letters match case; lowercase queries ignore case.
- Cmd-Option-C enters copy mode. Use arrows or h/j/k/l, w/b, Home/End or 0/$,
  PageUp/PageDown. Press v or Space to select, y or Enter to copy, and Esc or q
  to exit. No copy-mode key or committed IME text is sent to the child.
- Cmd-Shift-E opens retained output in the default text editor. The private
  temporary file is owned by the app until the session view is released; save
  it from the editor to keep a permanent copy. Output changing across pages
  produces a retry message rather than mixing revisions.
- Cmd-Shift-Up/Down jumps between the messages you sent to an Agent; the
  terminal menu offers **Previous message** and **Next message**. The message
  lands at the top of the view and is marked briefly. Previous starts from the
  top of what is on screen, so a message you can already see is skipped. Past
  the newest message, Next returns to the latest output. The view stays an
  ordinary terminal: scroll, select or type at any time. Any of these ends a
  jump in progress, and typing returns to the composer as usual.
  Full-screen Agents (Claude Code, Codex and OpenCode, which keep their
  transcript on the alternate screen) are moved to the message with the
  wheel input a trackpad sends, and with PageUp/PageDown in Claude Code,
  while the pane keeps showing the screen until the message is in place:
  the jump appears as one cut. Inline Agents are found in Diri's retained
  scrollback. Claude Code, Codex, OpenCode,
  Copilot, Kimi and Gemini gutters are recognized; other Agents can use
  explicit `You:` or `User:` labels. Composers and drafts, numbered choosers,
  pinned headers, tool output and soft-wrap continuations are not stops.
  In shells the same shortcuts jump between retained prompt marks (OSC 133
  A; see shell integration below). A shell running a recognized foreground
  Agent uses message navigation. An inline session with no message that way
  (a generic command, or an Agent that exited to its shell) falls back to
  its prompt marks.
- Shells that report command boundaries with OSC 133 (A prompt, B input,
  C output, D end with exit status) get a short mark in the left margin of each
  finished command's prompt: green when it exited 0, red otherwise. Running
  commands and ends without a status stay unmarked. Right-click a finished
  command's prompt, command line or output for **Copy command output** and
  **Select command output**; Cmd-Option-O copies the output of the last
  finished command in retained history. fish 4 emits these marks by default.
  Local zsh and bash terminals get them from Diri's shell integration with no
  setup and no change to the user's files: zsh starts with `ZDOTDIR` pointing
  at a `.zshenv` shim that puts `ZDOTDIR` back before any of the user's files
  run; bash starts as the same `bash -l` with a one-shot `PROMPT_COMMAND` that
  loads the hooks at the first prompt, after every startup file. Both chain
  around prompt frameworks (oh-my-zsh, powerlevel10k, starship,
  bash-preexec, a user's own DEBUG trap) and keep `$?` for them. Settings →
  Appearance → Shell integration turns it off for terminals opened afterwards.
  Agents' login-shell wrappers and remote shells are not changed; remote
  shells need their own terminal integration. Marks work in remote sessions
  whose Helper was installed by this version; older live Helpers show no
  marks until the session restarts.

Settings → Terminal contains Copy on Selection (off by default), Hide Pointer
While Typing (on), and Review Command Pastes (off by default). Paste review applies to
multiline text outside bracketed paste and to control characters. Confirmed
pastes replace unsafe control characters with spaces; ordinary bracketed Agent prompts
remain immediate. A reconnect or terminal mode change invalidates a pending
paste review, so the user must paste again into the current terminal.

All history features operate on retained output, not an unlimited session log.
Message navigation in inline Agents reads retained history in bounded pages,
including messages older than the recent Find capture.
Oversized link annotations remain plain text. Existing live Helpers remain
attachable after an upgrade; annotations become available in new Helpers.

The interaction audit used [Ghostty source at 5252b19](https://github.com/ghostty-org/ghostty/tree/5252b193cfd52b4bcd868135e21e4563f2f326ec)
and [Herdr source at bafbc09](https://github.com/herdrdev/herdr/tree/bafbc0949dd996cf7fd0848c8965e254348cc11e).
Diri implements these behaviors in its Rust Engine, shared parser and GPUI client.

Visual fixtures: [link hover](screenshots/terminal-qol/hover.png),
[terminal menu](screenshots/terminal-qol/menu.png), command marks
([gutter](screenshots/terminal-qol/commands.png),
[menu](screenshots/terminal-qol/commands-menu.png),
[selection](screenshots/terminal-qol/commands-select.png)), and paste review in
[dark](screenshots/paste-review/dark.png) and
[narrow light](screenshots/paste-review/light-narrow.png) appearances.
To regenerate a scene on macOS,
run the ignored `terminal_pane::tests::render_terminal_qol_screenshot` test with
`DIRI_QOL_SCREENSHOT` set to an output PNG and `DIRI_QOL_SCENE` set to `hover`,
`menu`, `paste`, `copy`, `commands`, `commands-menu` or `commands-select`. For paste review, the fixture explicitly enables the
opt-in setting. `DIRI_QOL_THEME`, `DIRI_QOL_WIDTH`, `DIRI_QOL_HEIGHT`, and
`DIRI_QOL_PASTE` override the theme, window size, and clipboard text.

Paste review uses a scrollable preview of up to 4,000 characters, with an explicit
label when truncated. Enter pastes, Escape cancels, and Tab switches between
Paste and Cancel. Saved preferences retain their value when upgrading; new
preferences and files without the setting default to off.
