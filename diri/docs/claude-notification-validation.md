# Live Claude notification validation

Validated on 2026-09-07 with Claude Code 2.1.263, using the release Rust Engine
from this PR. The Engine used a separate `DIRIJOR_APP_SUPPORT` directory and
real Claude PTY sessions in a temporary working directory. The installed app
and its sessions were left running separately.

The probe polled `session.list` every 100 ms, tracking status and
`lastTurnCompletedAt`, and inspected the test terminal and aggregate recovery
seed. Harmless shell scripts slept before creating completion marker files.
A completion timestamp must remain unchanged while work is pending and advance
once after the final response.

| Scenario | Live result |
| --- | --- |
| Foreground Bash call sleeping 20 seconds | Working throughout the call; one completion after the marker and final response, at 23.6 seconds from submission. |
| Background Bash call sleeping 40 seconds; parent replies immediately | Real parent `Stop` persisted `claudePendingWork: true`; no completion while the job ran. One completion after the marker and final response, at 43.2 seconds. |
| Background general-purpose Agent running a 25-second shell call; parent replies immediately | Parent seed remained `Stop` with pending work despite child activity. Working while the terminal said “Waiting for 1 background agent to finish”; one completion after the child and final parent response, at 45.8 seconds. |
| Bash permission request in manual mode | Needs-input persisted for roughly 46 seconds with no completion. One-time approval resumed Working, then completed once after the response. |
| Real idle reminder after completion | An `idle_prompt` notification arrived about 60 seconds after the background-agent turn completed. Status stayed Idle and the completion timestamp was unchanged. |

No new failure was found in these live scenarios. These observations validate
the Engine signals consumed by the notification policy. Native macOS banner
presentation was not exercised; it requires a packaged app. This is not a
claim of complete cmux/Herdr feature parity. Scheduled work, recovery, duplicate
callbacks and older hook payloads are covered by deterministic Rust regressions,
not by this live run.

## Repeat the check

1. Build the PR's release workspace. Start its `dirijord-rs` with
   `DIRIJOR_APP_SUPPORT` pointing at a fresh temporary directory. Keep the real
   home unchanged so the installed Claude CLI can authenticate normally.
2. Verify `hello.engineKind` is `diri-rust-engine`. Spawn `claude-code` through
   `session.spawn` in a temporary directory. For a controlled run, pass
   `--setting-sources '' --strict-mcp-config --no-chrome`; Diri still injects its
   own hooks. Use `--permission-mode manual` for the permission scenario.
3. Create harmless scripts for the durations above. Send prompts through
   `session.send_text`; request foreground execution with a sufficiently long
   timeout, then background execution with `run_in_background: true`, and then
   a background Agent. For background cases, ask the parent to reply immediately
   and respond again when the completion arrives.
4. Observe `session.read_screen`, `session.list`, marker files, and only the
   aggregate fields of the session's `last-activity.json`. Do not capture full
   hook payloads. Check completion timestamps before/after markers and after an
   idle reminder. Leave the permission request unanswered before approving its
   one-time Yes choice.
5. Kill only the test sessions with `session.kill`, then shut down the isolated
   Engine with `daemon.shutdown_if_idle`.
