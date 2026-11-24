# Structured GUI: protocol foundation status

Status of the Function-First Native GUI's engine and protocol layer. The GUI
(composer, thread view, pickers, `⌥⌘L` PTY flip) is not built yet.

## Verified against installed CLIs (2026-11-24)

| CLI | Version | Check | Result |
| --- | --- | --- | --- |
| `claude` | 2.1.331 | `-p --input-format stream-json --output-format stream-json --include-partial-messages --verbose --session-id <uuid>` | Streams `stream_event` text deltas, a complete `assistant` tool_use block, a `user` tool_result, and a `result` with usage and cost. |
| `claude` | 2.1.331 | `--resume <uuid>` on a session made with `--session-id` | Reattaches the same session id and recalls prior turns. The restart path depends on this. |
| `codex` | 0.171.0 | `app-server` stdio: `initialize`, `initialized`, `thread/start`, `turn/start` | Streams `item/agentMessage/delta`, `item/started|completed`, `thread/tokenUsage/updated`. |
| `codex` | 0.171.0 | `model/list` | Returns the catalog with `supportedReasoningEfforts` and `isDefault`. |
| `agy` | 1.5.15 | `acp` initialize, `session/new`, `session/prompt` | Works without a login: streams `agent_message_chunk`, ends with `stopReason: end_turn` and usage. `session/new` lists models. |
| `claude` | 2.1.331 | `--permission-prompt-tool stdio` with an `initialize` control request, a prompt that needs approval | Emits `control_request` `can_use_tool`. Answering `control_response` `behavior: allow` runs the tool. Without `--permission-prompt-tool stdio` the CLI denies silently. |
| `codex` | 0.171.0 | `approvalPolicy: on-request` or `untrusted`, `approvalsReviewer: user` | Emits `item/commandExecution/requestApproval`; answering `{"decision":"accept"}` runs the command. The user's config defaults to an automatic reviewer, which approves without asking, so the Engine must set `approvalsReviewer: user`. |

Surprise: on Codex 0.171.0 a completed turn sent **no `turn/completed`
notification** within 150 s. It ended with `thread/status/changed` to
`idle`. `parse_codex_line` treats both as turn end.

Not verified live: Codex `thread/resume` and `sandbox: workspace-write` (only
`read-only` was run), Claude `thinking_delta` (no probe model thought), Codex
`fileChange/requestApproval` params, and the agy `session/load` path. The agy
tool-call and permission shapes are not recorded yet, so agy permission
requests are refused rather than rendered.

## Built

- `diri-proto/src/structured.rs`: pure codecs for Claude stream-json (text,
  thinking, tool use, results, `control_request` permissions), Codex JSON-RPC
  (deltas, items, idle turn end, approvals), and agy ACP (message chunks,
  models, prompt usage). Tests in `tests/structured_codecs.rs` run on sanitized
  recordings in `tests/fixtures/structured/`.
- `diri-engine/src/structured.rs`: `StructuredConversation`. It spawns nothing
  until `send_turn`. Model and effort changes before turn 1 are field writes.
  Claude restarts on the same UUID after a mid-conversation model change; Codex
  passes the model per turn. Prompts are held until answered
  (`answer_permission`); unanswerable server requests are refused with a
  JSON-RPC error so the turn cannot stall. Stdout lines over 8 MiB are dropped
  with a notice and the turn continues. Events are sequenced and tool starts
  are delivered once.
- `diri-engine/src/structured_registry.rs` and the `structured.*` RPCs in
  `control.rs`: `open`, `send`, `configure`, `poll` (events and open prompts),
  `answer_permission`, `stop`, `close`, `models`, `switch_to_pty`,
  `switch_to_gui`. Model catalogs are probed once: Codex via `model/list`,
  agy via `session/new`. Claude's list is static. `switch_to_pty` stops the
  worker and returns the resume argv (`claude --resume <id>`,
  `codex resume <id>`); while the PTY holds the conversation, sends and
  answers are refused with `pty_attached`.
- Antigravity runs as a driver: `agy acp`, `session/prompt` per turn.
- `formal/StructuredGui.lean`: monotonic sequencing, harness-switch
  continuity, no spawn without a turn, and the lease rule the registry follows.

Tests: 17 codec tests, 16 engine conversation tests against fake CLIs (one
opt-in real-`claude` test, which passes), and 6 RPC tests. Workspace clippy with
`-D warnings` and `fmt` pass.

## Not built (open)

- The GUI. `diri-app` has no structured thread view, composer, pickers, diff
  cards, or permission cards yet. Nothing here renders anything.
- `⌥⌘L` key binding and the actual PTY Holder attach. The lease and argv exist;
  nothing spawns the Holder with them yet.
- Push delivery. Events are drained by `structured.poll`, not streamed.
- Conversation identity in `SessionRecord`. A structured conversation is
  addressed by its own id, not by a Session.
- Interrupt and steer. A second `send_turn` during a turn is refused.
- Agy permissions and tool calls: the wire shapes are unrecorded.
- agy resume after `stop`: `session/load` is unverified, so a restart creates a
  new session. The PTY handoff is refused for Antigravity for that reason.
- Verification through `dirijord-rs` and `diri dev`. Not run.
- The Visual PR. There is no UI to screenshot, so none was opened.
