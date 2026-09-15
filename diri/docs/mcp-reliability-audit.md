# MCP reliability audit — 2026-09-15

Baseline: `945f99d`, after the durable message-delivery fix in PR #298.
Implementation: the Rust `dirijor-mcp` crate. The audit uses disposable Unix
sockets, real MCP subprocesses, and the existing Engine/PTY integration fixtures.
No live Forge session or production Agent was used for verification.

## Findings and changes

| Priority | Finding | Result |
| --- | --- | --- |
| High | A blocking tool occupied the stdio input loop; even ping and cancellation stalled. | Eight bounded read calls can overlap an ordered mutation worker. Ping and protocol control remain on the input loop. |
| High | Cancelling a wait left its Engine connection alive. | Request-scoped cancellation shuts down read sockets, drops subscriptions, and suppresses cancelled responses. Queued mutations can be cancelled before dispatch; started mutations finish without pretending their effects were undone. |
| High | Losing one selected child marked the entire group settled while another child was working. | Every remaining child's state is evaluated; missing IDs are returned in `removed`. |
| High | Completion between the last snapshot and subscription registration was lost until timeout. | Subscribe first, refresh immediately after acknowledgement, and refresh again on events. Single-session waits use the same subscription pattern instead of a blocking Engine long poll. |
| High | Invalid optional arguments silently selected defaults. For example, the string `"false"` for `submit` became `true`; malformed host arguments could select the local host. | Validate types, required fields, enum values, routing strings, and unknown fields before any Engine interaction. The regular CLI omits absent options instead of emitting null fields. |
| High | A large `timeout_s` panicked during floating-point duration conversion. | Wait timeouts are validated as finite numbers in the advertised 0–600 second range. |
| High | MCP requests did not verify the Rust Engine's identity before mutations. | Every bridge connection checks the Engine's explicit identity and control version. An unidentified peer receives no mutation. |
| Medium | Unknown delivery was returned as a successful MCP tool envelope despite `ok: false`. | The MCP `isError` flag now agrees with that result. The durable receipt still prevents replay. |
| Medium | Malformed JSON-RPC requests could disappear without a response; unsupported protocol versions were echoed as supported; tools could execute before startup completed. | Explicit request/parameter errors, known-version negotiation, and initialize/initialized gating. |
| Medium | Stdio frames were unbounded, and socket timeouts restarted across reads/events. | Bounded frame reading/recovery and monotonic absolute request deadlines, including partial frames and event traffic. Protocol errors do not echo arbitrary daemon payloads. |
| Medium | Tool definitions were cached for the entire MCP process lifetime. | Each tools/list request reads the current Engine catalog. No listChanged capability is advertised. |

The first nine rows have regression tests that were run failing before their
fixes. Additional tests cover frame limits, absolute deadlines, queue capacity,
intentional repeats, routing validation, and compatibility with the earlier
delivery fixes. Existing authorization tests still cover direct lineage,
cross-host children, unrelated sessions, self-targeting, and protected parents.

## Contracts

- **Message delivery:** the existing durable message identity is unchanged.
  Identical retries return the same receipt. Sending is not Agent acknowledgement.
- **Mutations:** one worker preserves arrival order within each MCP process.
  At most eight mutations wait behind the running mutation. Busy errors mean the
  rejected call was never dispatched. Across processes, the Engine remains the
  authority for input serialization and durable deduplication.
- **Reads:** at most eight run concurrently. Cancellation closes only that
  request's connections. It does not terminate an Agent or revoke its controller.
- **Waits:** already matching statuses return immediately. A single wait also
  stops on exit/removal and returns `matched` and `removed`; callers must inspect
  those fields. Group waits report missing IDs separately. An omitted selection
  means all direct children; `session_ids: []` means none.
- **Deadlines:** ping remains responsive while another call is waiting. A stream
  of unrelated events or slow JSON bytes cannot restart the operation's timeout.
- **Input validation:** wrong optional types, empty routing fields, unsupported
  enum values, and misspelled arguments are errors, not alternative instructions.
- **Startup:** MCP initialization negotiates known versions, then requires the
  initialized notification before tools can run. The bridge independently
  verifies the local Rust Engine identity on its control socket.

These startup and cancellation choices follow the
[MCP lifecycle rules](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)
and [cancellation rules](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation).
The server implements the existing tools surface, not every optional MCP feature.

## Verification

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p dirijor-mcp` | 44 passed |
| `cargo test --workspace` | 1,582 passed, 32 ignored |
| `cargo build --workspace --release` | Passed |

The subprocess tests exercise blocked calls, live ping, cancellation, overload,
mutation ordering, initialization, malformed requests, and oversized-frame
recovery. Engine socket fixtures control exact state transitions and failures.
Existing integration tests execute actual local PTY children and the standalone
CLI, including a large initial prompt, duplicate sends, a lost response, and
parent reports after sender renaming.

A debug run of 50 sequential pings during a blocked Engine request measured
approximately 37 microseconds median and 77 microseconds p95. This measures the
MCP dispatch boundary on this Mac, not Agent response time or WAN latency. The
new queue and cancellation resources are outside the terminal/Holder hot path.

## Next reliability work

1. **Correlate completion with a logical task.** An idle status can predate a
   newly sent prompt. Native Agent turn identities/acknowledgements, where
   available, should connect a delivery receipt to its eventual result. Current
   waits are explicitly status waits; callers must verify task-specific output.
2. **Extend durable request identity beyond text delivery.** Session/worktree
   creation currently has no durable operation receipt. If its reply is lost,
   inspect existing sessions/worktrees before repeating the operation. Retrying
   a spawn blindly can create another session. A tracked-spawn design should
   expose the created session identity before expensive bootstrap/prompt work.
3. **Run an opt-in Forge soak.** Exercise actual Agent startup, idle/busy input,
   network interruption, cancellation, and Engine/app restart. Deterministic
   fixtures validate these code paths but do not establish provider-version or
   real-network behavior.

Those are remaining product boundaries, not guarantees provided by this change.
Remote MCP forwarding, additional Holders/controllers, and native Agent task
queues were not added.
