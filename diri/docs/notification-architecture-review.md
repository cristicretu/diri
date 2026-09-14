# Durable notification lifecycle

Date: 2026-09-14. Implementation branch: `fix/durable-notification-lifecycle`,
based on Diri main `65b2b05`. This replaces the notification admission lifecycle,
not the terminal renderer or Remote PTY Holder transport.

## Why the earlier fixes kept recurring

The old app decided a prompt was new when its text-derived `blocker_key` changed.
The native identifier also included its observation timestamp. An animated Codex
`[!]` / `[·] Action Required` title therefore became a succession of distinct
alerts. The full manifest → reducer → SessionRecord → store → native-request
regression produced **20 interruption requests for 20 redraws** before this change.
It now produces **one request and one inbox entry**.

## What the GitHub history establishes

| Diri change | What it addressed | What remained |
| --- | --- | --- |
| [#66](https://github.com/cristicretu/diri/pull/66), August 12 | Borrowed Herdr's settle window and late state validation | Delay filters short blips; it does not identify requests |
| [#181](https://github.com/cristicretu/diri/pull/181), September 5 | Persistent inbox, read/mute/focus, withdrawal, native navigation, terminal ingress | The app still derives attention events from session snapshots |
| [#219](https://github.com/cristicretu/diri/pull/219), September 7 | Claude work-hook authority and pending background work | Better completion evidence for one provider |
| [#239](https://github.com/cristicretu/diri/pull/239), September 10 | Capture Codex's title instead of changing terminal output | Assumed the title was stable |
| [#242](https://github.com/cristicretu/diri/pull/242), September 10 | Recognize nonblocking queued questions | More provider-specific screen interpretation |

These changes contain useful behavior to preserve. The recurrence comes from a
shared design weakness: mutable observations are being used to identify events.

## Source comparison and decision

The source review covered Herdr `c77af189` and cmux `e3951d51`. It did not run
those applications or verify their native delivery experimentally.

Herdr reconciles effective agent state before forwarding a candidate; its client
settles, replaces and validates pending work against current state and focus.
Diri keeps that useful settle-and-recheck behavior. A timer cannot establish
request identity. [Herdr server][h-server], [Herdr policy][h-policy]

cmux's agent journal tracks turn/request identities and reserves effects through
SQLite receipts. Its receipts outlive display-history pruning. This is the
stronger model for replay safety, and the basis of this implementation's ownership
and receipt boundaries. Diri does not copy cmux's provider-specific adapters or
claim equivalent source coverage. [Reconciler][c-reconciler], [receipts][c-receipts]

## Implemented ownership

1. `diri-engine/src/attention.rs` owns causal attention state. A new process gets
   a random namespace; each event gets a monotonic sequence. Holder adoption
   reopens the same journal. Text, timestamps, read state and settle deadlines
   cannot create a new identity.
2. Existing status reduction remains the authority for execution evidence,
   including Claude parent/background work and manifest priority. Attention
   requests can remain open independently of an execution-status repaint.
3. `diri-app/src/notification_feed.rs` is the single owner of admission, bounded
   inbox history, interruption receipts, settling and revocation. The store
   forwards Engine events instead of deriving events from status/text changes.
4. The macOS bridge receives the same revocable token. Reading, resolving,
   muting, clearing and pruning revoke queued requests, including requests waiting
   on native authorization. Unread/unmute does not revive a canceled effect.

## Evidence and identity

- Codex completion IDs are scoped by native conversation and turn ID. Duplicate
  completion callbacks cannot reopen a finished turn.
- Claude native tool IDs are scoped by conversation. PreToolUse records bounded
  unfinished tool identities; PostToolUse/PostToolUseFailure resolves only its
  matching request. Replayed starts and resolutions are receipted too.
- Claude PermissionRequest currently does not guarantee a `tool_use_id`.
  Correlation is permitted only when exactly one unfinished native tool has the
  reported tool name. Ambiguous parallel calls remain inferred; another tool's
  completion must not resolve them.
- Without a usable native ID, repeated hooks/screens share one inferred wait.
  Confirmed continuation after accepted response input, a new submitted user
  turn, an authoritative completion, or process exit retires it. Mere typing,
  screen misses, changed copy and elapsed time do not rearm it.
- Queued questions remain in the inbox without interruption while work continues.
  Becoming blocking can reserve one interruption for the existing event.
- The privacy-filtered hook recovery seed now retains optional native IDs.
  Adoption also rejects a seed older than the attention journal's committed
  observation, preserving the existing same-machine timestamp recovery boundary.

## Persistence and crash semantics

Engine journals are `<session>.attention.sqlite` beside local session logs.
State and native receipts commit in one SQLite transaction before publication.
The event snapshot holds at most 200 display events, 32 active requests and 32
unfinished native tools. Receipts are independent of display pruning.

The app uses `notifications.sqlite` beside preferences. Version-1
`notifications.json` history and dismissed IDs migrate transactionally on first
successful initialization. The JSON file is left intact. New databases and
SQLite journals have owner-only file permissions. No new production dependency
was needed: both crates already use rusqlite.

Inbox admission and interruption admission have separate receipts: an optional
question can enter the inbox before it becomes blocking. An interruption attempt
is reserved durably before its one-second settle window and before native effects.
Restart/reconnect hydrates history silently. Clearing or pruning cannot erase a
receipt and cause a replayed event to interrupt again.

This provides **at most one interruption attempt per event**, not exactly-once
native delivery. A crash after reservation may lose a banner/chime; its inbox
entry remains durable. macOS authorization and Focus can also suppress delivery.
A storage failure suppresses new event publication/delivery and emits a generic
diagnostic rather than falling back to ephemeral identities.

## Verification and remaining boundaries

Regression coverage includes animated titles through the real manifest and
reducer, identical prompts after continuation, native duplicates after reopening,
independent request resolution, ambiguous parallel tools, parent/background
completion, fresh launch versus adoption, clear/restart/pruning, optional-to-blocking
questions, silent hydration, cancellation during native authorization, migration,
private permissions and failed writes. Existing workspace session/PTY/remote
regressions are also run.

Validation on macOS arm64: `cargo fmt --all -- --check`, strict workspace
Clippy, all **1,540 workspace tests passed** (32 explicitly ignored), and
`cargo build --workspace --release` passed. The signed, isolated development
bundle launched against its own Engine; the notification tray rendered and its
Test alert action was exercised. Actual macOS banner visibility is unverified:
automatic approval review blocked opening Notification Center because it could
expose unrelated private notifications. This is not a claim of completed native
delivery validation. The installed app was not replaced.

A provider that exposes only a screen cannot reveal every request's true identity.
The conservative fallback may keep an ambiguous prompt open until response or
completion rather than guess from text. This design prevents the reproduced
redraw/replay failure mechanism; it cannot promise perfect detection for every
future agent UI.

The hook recovery file retains the latest callback, not a complete offline event
stream. Full transcript/event-stream adapters, reliable offline remote events,
mobile forwarding and native-delivery acknowledgements remain separate work.
Nothing was added to the Remote Helper protocol or Holder product responsibilities.

[d-policy]: https://github.com/cristicretu/diri/blob/8f131291d2bdff59291d7891f71f34bbad1ddd3f/diri/crates/diri-app/src/notifications.rs
[d-feed]: https://github.com/cristicretu/diri/blob/8f131291d2bdff59291d7891f71f34bbad1ddd3f/diri/crates/diri-app/src/notification_feed.rs
[d-store]: https://github.com/cristicretu/diri/blob/8f131291d2bdff59291d7891f71f34bbad1ddd3f/diri/crates/diri-app/src/store/mod.rs
[h-server]: https://github.com/herdrdev/herdr/blob/c77af1892ff121736ecb103b32d504d6f1b31805/src/server/headless/notifications.rs
[h-policy]: https://github.com/herdrdev/herdr/blob/c77af1892ff121736ecb103b32d504d6f1b31805/src/client/shell/notification_policy.rs
[h-codex]: https://github.com/herdrdev/herdr/blob/c77af1892ff121736ecb103b32d504d6f1b31805/src/detect/manifests/codex.toml
[c-reconciler]: https://github.com/manaflow-ai/cmux/blob/e3951d51b3b60bfb210e73dbeb45c3ee17cfd019/Packages/macOS/CmuxAgentJournal/Sources/CmuxAgentJournal/AgentNotificationReconciler.swift
[c-receipts]: https://github.com/manaflow-ai/cmux/blob/e3951d51b3b60bfb210e73dbeb45c3ee17cfd019/Packages/macOS/CmuxAgentJournal/Sources/CmuxAgentJournal/AgentJournalStore%2BAttention.swift
[c-admission]: https://github.com/manaflow-ai/cmux/blob/e3951d51b3b60bfb210e73dbeb45c3ee17cfd019/Sources/AgentJournalLifecycleCenter%2BNotifications.swift
[c-tests]: https://github.com/manaflow-ai/cmux/blob/e3951d51b3b60bfb210e73dbeb45c3ee17cfd019/Packages/macOS/CmuxAgentJournal/Tests/CmuxAgentJournalTests/AgentNotificationReconcilerTests.swift
