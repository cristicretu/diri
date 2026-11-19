# Diri reliability model

Run from this directory with the pinned Lean 4.36.0 toolchain. The project uses Lean's standard library and has no package dependencies.

```sh
export PATH="$HOME/.elan/bin:$PATH"
lake build
lake env lean --run ReceiptTraces.lean | diff - ../crates/diri-engine/tests/fixtures/receipt-model.tsv
lake env lean --run TaskTraces.lean | diff - ../crates/diri-engine/tests/fixtures/task-model.tsv
```

## What the proofs cover

| Property | Lean result | Rust boundary |
| --- | --- | --- |
| One effect per retained identity | `at_most_once` proves a bound over every finite trace from an empty ledger. `sent_survives_app_restart` proves the confirmed row survives an app restart with exactly one historical effect. | `message_delivery::deliver_at` and `operations::{reserve, finish}` commit a reservation before an input write or launch. Only the original caller proceeds. |
| Task acknowledgement across app and Engine restart | `task_ack_survives` preserves the fact of acknowledgement across a trace. `task_terminal_stable` preserves a terminal task. | `tasks::{reserve, report, cancel, get}` reopen the SQLite journal. An assignee reports acknowledgement explicitly. A sender can cancel before that acknowledgement. |
| Surviving Holder | `live_holder_never_reported_dead` proves the specification given an initially sound view. It permits an unknown view during reconnection. | `Registry::{restore, adopt_live_holders, reap_orphans}` reconstruct local session state from Holder evidence. The mismatch at commit `323a10ea` is reproducible below. |
| New controller revokes older epochs | `attach_revokes` proves strict increase and rejection of the previous epoch on successful admission. `stale_rejected` covers every earlier epoch. | `Holder::handshake` assigns the epoch and `Holder::handle_message` checks it. A full u64 counter refuses another lease. |

The delivery proof projects an arbitrary message or operation onto its key. Message keys include sender, target and message ID. Spawn keys include sender and operation ID. Distinct keys have disjoint rows. The ghost counter counts attempts with a possible effect, including partial writes. Reservation, effect and confirmation are separate transitions. A crash before confirmation leaves the database outcome unknown and removes the original caller's ability to act. Only a failure known to precede the first effect can release a reservation. On a failed operation the durable `failed` outcome likewise prevents a new launch. A `sent` receipt certifies a transport write. Agent acknowledgement of a tracked task lives in a separate record.

The model assumes SQLite serializes immediate reservations, committed rows survive process restarts on intact storage, identity hashes do not collide, and an Engine crash halts its pending invocation. A Holder keeps its PTY and epoch across client restarts. An authenticated positive/negative observation refers to the same session incarnation, and an exit fact refers to the owned PTY child. After that child exits a Holder may remain to drain output. Task history in the model includes whether acknowledgement ever happened. The on-disk audit trail retains only its most recent 32 updates. The proofs use standard Lean foundations and introduce no custom axioms or proof placeholders.

## Two counterexamples on main at 323a10ea

1. `REMOTE_PORT.md` specifies permanent receipts and refusing new identities at 100,000 entries. Commit `0169b68a6` changed `message_delivery::reserved_count` to prune entries aged seven days. A successful send, a sweep triggered by a different identity, and a retry of the original identity can produce two writes. The same applies to tracked spawn operations. The executable `expire` example in Lean proves the count is two in that trace. An app restart itself never prunes those rows. Task submission has its own permanent journal. The bounded permanent-receipt design can enforce lifetime deduplication by refusing new IDs once full. Supporting unlimited new arbitrary IDs also requires more durable storage or a different identity protocol (for example ordered sender sequence numbers with a persistent watermark and explicit rejection of old requests).
2. A local Holder can own a live child while a single `stat` fails. The baseline labels the unadopted record exited. `baselineRestoreOnFailedStat` falsifies the safety property for that transition. The Rust test `ambiguous_stat_during_restore_cannot_end_a_surviving_holder` checks the real child PID on both sides of a failed stat and reads the status recorded by `Registry::restore`.

## Rust conformance checks

Run from `diri/` with a private `CARGO_TARGET_DIR`.

```sh
cargo test -p diri-engine --lib match_lean_restart_traces
cargo test -p diri-remote --lib controller_epoch_model_rejects_old_claims_and_exhaustion
```

`ReceiptTraces.lean` enumerates all 25 pairs of confirmed, rejected-before-write, interrupted-before-write, interrupted-after-write and uncertain actions, with app and Engine restart between attempts. Rust drives the real message and spawn receipt functions and compares the persisted outcome and number of effect callbacks against the generated table. Reopening the database represents an Engine restart at the storage boundary. A dropped invocation has no volatile ownership on retry. The companion task table enumerates 27 three-action traces combining acknowledgement, completion and cancellation. Rust reads each fresh journal view and verifies the acknowledgement history, terminal classification and immutability of an already terminal record.

The Holder test uses a real PTY and authenticated connections, starting at zero and two epochs below the maximum. After each attempted attach it exercises the input path of every prior connection, including the lease kept when the next epoch cannot be represented.
