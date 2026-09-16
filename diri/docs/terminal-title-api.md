# Read the terminal title

`dirijor session terminal-title ID` reads the local emulator's current OSC title.
It is separate from the conversation name shown in the sidebar and tabs. This
read-only operation does not rename the session, send input, resize the PTY, or
attach another client.

```sh
dirijor session terminal-title SESSION_ID
dirijor session terminal-title SESSION_ID --json
```

For example, a shell that sets its terminal title to `build — anara` returns:

```json
{"sessionID":"SESSION_ID","title":"build — anara"}
```

The control method is `session.terminal_title`, with parameters
`{"sessionID":"SESSION_ID"}` and a `SessionTerminalTitleResult` response. The
typed Rust client exposes `DaemonClient::terminal_title`.

`title: null` means the current emulator has no title, including before the child
sets an OSC title and after a title reset. An explicitly empty title remains
`""`. JSON preserves the raw string;
plain CLI output escapes control characters and prints `(no terminal title)`
for null. The session's prompt-derived display title is never a fallback.

## Availability

- Local Engine-owned terminal state is required. An exited session remains
  readable while that state is retained.
- A known session without terminal state returns `terminal_title_unavailable`.
  An unknown session returns `not_found`.
- Remote sessions return `terminal_title_unsupported`. Remote snapshots do not
  currently carry an authoritative title across reconnects, so previously
  observed remote output cannot establish the current title. This method does
  not change the Remote Helper protocol or claim remote title parity.
- The result describes the title after output processed by the emulator at the
  time of the read; it does not wait for pending child output or recover titles
  absent from the Engine's restored terminal state.
