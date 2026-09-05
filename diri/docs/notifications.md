# Agent notifications

Open **Notifications** from the sidebar or with **⌘⇧I**. **⌘⇧J** jumps to the
latest unread session notification, then falls back to sessions currently
needing attention. The tray supports unread/all views, read/unread, clear,
session mute, and keyboard navigation with ↑, ↓, Return and Escape.

Execution and unread state are separate. A completion remains in the inbox
when an agent starts more work. Reading it does not stop the agent or answer a
question. A resolved or replaced prompt is marked read and its Mac alert is
withdrawn. Closing or archiving a session resolves its notifications; the
history remains available. Failed exits notify; clean exits and sessions
closed through the app do not.

A notification for the selected, visible session in the active app is recorded
as read and makes no sound or desktop banner. A selected session behind
Settings, the launcher or an overlay can still notify. Delivery is checked
again on the main thread so reading or resolving a queued event suppresses it.
**Sounds off** is silent; it does not substitute the system sound. **Alerts
off** and per-session **Mute** suppress interruption while preserving history.

On macOS, clicking a notification or **Open session** activates Diri and
selects its originating session. Cold-start clicks wait for the session list.
Unavailable sessions produce an explanation. Old Approve/Deny notifications
also open the session: notification callbacks never type into a terminal.
Unread indicators appear in the sidebar, menu-bar session list and Dock.
Reading, resolving, muting or clearing removes the corresponding native alerts.

The tray shows macOS authorization status and includes **Test alert**. macOS
Focus mode and system notification settings still control delivery. A bare
Cargo binary has no application bundle and cannot deliver native alerts; the
in-app inbox remains available.

## Terminal and CLI integration

An agent or command can send a notification through its current terminal:

```sh
dirijor notify --title 'Tests finished' --body 'All checks passed'
printf '\033]777;notify;Tests finished;All checks passed\007'
printf '\033]9;Review is ready\007'
```

The existing `dirijor notify '<Codex JSON>'` completion callback is preserved.
The Engine accepts OSC 9, OSC 777 and plain-text OSC 99 title/body notifications,
including BEL/ST terminators and fragmented writes. OSC 9;4 remains progress.
Kitty queries, icons, encoded payloads and callback actions are ignored.
Inputs, multipart state and event queues are bounded; repeated identical
terminal messages within five seconds are coalesced.

The same terminal sequences work over Diri's Remote PTY Holder transport while
the local Engine is connected. The local Engine derives events from live raw
output; the Holder does not run hooks or store product notifications. Replay
cannot redeliver terminal alerts. Structured remote Claude/Codex hooks and
reliable notification delivery while the Engine is disconnected remain
separate enhancements; see [REMOTE_PORT.md](../REMOTE_PORT.md).

History is app-local `notifications.json`, beside preferences: versioned,
atomically saved, owner-only on Unix and bounded to 200 events. It retains
bounded user-visible titles/bodies. Notification payloads are not added to
operational logs. Read state belongs to this app installation, not to remote
Holders or other clients.

## Source comparison with cmux

Compared against cmux commit
[`7d5d308`](https://github.com/manaflow-ai/cmux/tree/7d5d308450eac2991e748c6387d8718704be891a)
and Diri main `1c56e5f`. This is a source comparison, not a claim that every
cmux notification feature or delivery condition has been reproduced.

The main gaps addressed here are ordinary native-click navigation, durable
unread history, resolved-alert withdrawal, focused-session quiet, real mute,
unseen-completion navigation, same-level event deduplication, failure alerts,
terminal notification ingress and delivery diagnostics. References:
[delivery policy](https://github.com/manaflow-ai/cmux/blob/7d5d308450eac2991e748c6387d8718704be891a/Sources/TerminalNotificationDeliveryDecision.swift),
[notification lifecycle](https://github.com/manaflow-ai/cmux/blob/7d5d308450eac2991e748c6387d8718704be891a/Sources/TerminalNotificationStore.swift),
[session opening](https://github.com/manaflow-ai/cmux/blob/7d5d308450eac2991e748c6387d8718704be891a/Sources/AppDelegate%2BNotificationOpen.swift).

Remaining notification differences include transcript-derived completion
summaries, agent background-work metadata, notification webhooks/mobile
forwarding, full Kitty notification protocol support, and reliable offline
remote events. Terminal-scraped statuses still depend on each agent manifest.

Other product gaps, in priority order:

| Gap | Diri baseline | cmux comparison |
| --- | --- | --- |
| Flexible splits | Main plus auxiliary terminal; addressed in a separate PR | Arbitrary horizontal/vertical surfaces and directional focus |
| Embedded browser | Playwright automation exists; no browser pane in the desktop workbench | Browser surfaces and remote localhost routing |
| At-a-glance context | Branch/cwd/ports often require hover; PR/artifact UI exists | More context directly in sidebar rows |
| Shared project workflows | Saved launch recipes exist | Repo-owned commands and inherited configuration |
| Mobile alerts | Phone access exists | Optional notification forwarding |

Remote uploads, worktrees, PR tracking, port metadata, CLI/MCP orchestration and
session persistence already exist in Diri. Terminal quality and performance
need a measured comparison, not an inference from renderer names.
