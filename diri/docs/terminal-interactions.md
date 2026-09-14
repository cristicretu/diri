# Terminal interactions

- Hover a URL or file reference to underline it and preview its destination.
  Cmd-click opens on release; dragging cancels opening. Named OSC 8 links work
  when the session's Helper supports terminal annotations.
- Right-click for Open/Copy Link, Copy Selection, Find Selection, Paste,
  Keyboard Copy Mode, Open Scrollback in Editor, and shell prompt navigation.
  In mouse-aware terminal applications, Option-right-click opens Diri's menu.
- Double-click then drag selects whole words. Triple-click then drag selects
  complete lines. Option-Shift-drag selects a rectangular column. Option-drag
  continues to select locally in mouse-aware applications.
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
- Cmd-Shift-Up/Down jumps between retained shell prompt marks. Your shell must
  emit OSC 133 A (for example through its terminal integration). No shell
  configuration is changed. Older live Helpers and full-screen TUIs may have
  no prompt marks available.

Settings → Terminal contains Copy on Selection (off by default), Hide Pointer
While Typing (on), and Review Command Pastes (off by default). Paste review applies to
multiline text outside bracketed paste and to control characters. Confirmed
pastes replace unsafe control characters with spaces; ordinary bracketed Agent prompts
remain immediate. A reconnect or terminal mode change invalidates a pending
paste review, so the user must paste again into the current terminal.

All history features operate on retained output, not an unlimited session log.
Oversized link annotations remain plain text. Existing live Helpers remain
attachable after an upgrade; annotations become available in new Helpers.

The interaction audit used [Ghostty source at 5252b19](https://github.com/ghostty-org/ghostty/tree/5252b193cfd52b4bcd868135e21e4563f2f326ec)
and [Herdr source at bafbc09](https://github.com/herdrdev/herdr/tree/bafbc0949dd996cf7fd0848c8965e254348cc11e).
Diri implements these behaviors in its Rust Engine, shared parser and GPUI client.

Visual fixtures: [hover and destination preview](screenshots/terminal-qol/hover.png),
[terminal menu](screenshots/terminal-qol/menu.png), and paste review in
[dark](screenshots/paste-review/dark.png) and
[narrow light](screenshots/paste-review/light-narrow.png) appearances.
To regenerate a scene on macOS,
run the ignored `terminal_pane::tests::render_terminal_qol_screenshot` test with
`DIRI_QOL_SCREENSHOT` set to an output PNG and `DIRI_QOL_SCENE` set to `hover`,
`menu`, `paste`, or `copy`. For paste review, the fixture explicitly enables the
opt-in setting. `DIRI_QOL_THEME`, `DIRI_QOL_WIDTH`, `DIRI_QOL_HEIGHT`, and
`DIRI_QOL_PASTE` override the theme, window size, and clipboard text.

Paste review uses a scrollable preview of up to 4,000 characters, with an explicit
label when truncated. Enter pastes, Escape cancels, and Tab switches between
Paste and Cancel. Saved preferences retain their value when upgrading; new
preferences and files without the setting default to off.
