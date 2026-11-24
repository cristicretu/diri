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
| `agy` | 1.5.15 | `--help` | Has `acp` (stdio) and `--input-format stream-json`. No driver yet. |

Surprise: on Codex 0.171.0 a completed turn sent **no `turn/completed`
notification** within 150 s. It ended with `thread/status/changed` to
`idle`. `parse_codex_line` treats both as turn end.

Not verified live: `thread/resume` for Codex, `sandbox: workspace-write`
(only `read-only` was run), Claude `thinking_delta` (the probe model did
not think), and every approval request shape (Claude `control_request`,
Codex `item/*/requestApproval`).

## Built

- `diri-proto/src/structured.rs`: pure codecs. Claude stream-json and Codex
  JSON-RPC parsers, encoders, and the `model/list` decoder. Tests in
  `tests/structured_codecs.rs` run against sanitized recordings in
  `tests/fixtures/structured/`.
- `diri-engine/src/structured.rs`: `StructuredConversation`. It spawns nothing
  until `send_turn`. Model and effort changes before turn 1 are field writes.
  A mid-conversation Claude model change restarts on the same UUID. Codex
  passes the model per turn. A harness switch keeps the conversation and drops
  the native id. Antigravity fails closed with `Unsupported`. Events are
  sequenced monotonically, and repeated tool starts are delivered once.
- `diri-engine/tests/structured_conversation.rs`: 10 tests against scripted
  stand-ins that replay the recorded lines through real pipes. One opt-in test
  drives the real `claude` (`DIRI_REAL_CLAUDE=1`, `--ignored`).
- `formal/StructuredGui.lean`: proves monotonic sequencing, that switching
  harness keeps one conversation with one divider, that no process starts
  without a turn, and that the lease holder keeps the conversation. Build with
  `lake build StructuredGui` in `formal/`.

## Not built (open)

- Permissions. Claude runs without a permission-prompt tool, so prompts are
  denied. Codex runs with `approvalPolicy: never`. Any server request is
  surfaced as an `Error` event and not answered.
- Interrupt and steer. A second `send_turn` during a turn is refused.
- Engine integration: `SessionRecord`, registry, GUI events, the `⌥⌘L` PTY
  flip, and the lease are not wired to the existing Session model.
- Antigravity ACP driver.
- A bound on a single stdout line. The frame queue is bounded, but one
  oversized line is still read whole.
- The GUI, and the Visual PR with light/dark screenshots and latency numbers.
