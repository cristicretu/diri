# Completed local terminal storage: groundwork

This slice adds `completed_terminal::{CompletedRunKey, CompletedTerminalStore}`.
It has no runtime lifecycle or pane call sites yet. It is not completed-output
recovery acceptance and must not raise the recovery parity score.

## Contract

Capture a run key from the existing local record and an alive HolderStat with
verified host-native child identity and `epochOffset`. Keep that binding across
Engine restart; never capture a new key from an unverified PID or infer one from
an old artifact. The current primitive deliberately does not invent that missing
Registry persistence transaction.

The caller supplies a pinned expected key to `load`, together with the local
exited record. The loader rejects identity/exit mismatches and remote records.
It reads the exact key's immutable filename only. A stale record plus a stale key
is not current-run authority: lifecycle integration must revalidate both before
returning data to a user. No public API accepts a caller-provided key or path.

`publish` requires observed Exited/Signaled facts, a fully drained final offset,
a complete visible grid and bounded retained history. It refuses partial exit
marker buffers. It cannot establish drain or observation by itself. No raw replay,
parser replies, status reduction, notification delivery or input occurs.

The format is `DIRICMP1` (8 bytes), big-endian metadata length u32, checkpoint
length u64, bounded JSON metadata, then four u32-length-prefixed sections: grid, history rows, JSON history annotations,
and optional keyboard snapshot. Metadata binds
version 1, exact run key, exit facts and SHA-256 of the payload plus terminal mode/offset/history-count fields. Limits are
4 KiB metadata, 16 MiB checkpoint, 10,000 history rows, 1,048,576 decoded cells,
and two concurrent storage operations. File headers/size are admitted before
JSON or cell decoding; RLE expanded dimensions are admitted before cell allocation.
No plist parser is used, so shared-object or nested plist expansion is excluded.
Annotation JSON is separately limited to 256 KiB per section; keyboard state uses
the existing bounded 8,198-byte decoder. Existing visible-cache files are untouched.

Publication uses mode 0600, a private existing directory opened once by descriptor,
nonce create-new, file fsync, same-directory no-replace link, nonce unlink and
directory fsync. A second publication of an existing run returns AlreadyExists
inside `StorageError::Io`; it never overwrites the first artifact. A post-publication
sync failure reports failure rather than claiming crash durability. Future retry
handling must inspect exact existing content before claiming idempotent success.

## Exact remaining integration seams

1. `Session::spawn_held` and `spawn_held_deferred`: reserve a durable new run before
   launch effects, without deleting the prior immutable artifact. Resolve known
   failure vs uncertain launch outcomes; never restore stale current-run authority.
2. `Session::attach` / `process_facts::capture_holder`: bind the verified birth and
   epoch to the reserved record run before exposing it. Missing old-Holder identity
   remains unsupported. Adoption must check the persisted binding, not replace it.
3. `pump_held` final drain/exit: capture and publish this run's retained checkpoint
   outside Registry, including an observed matching exit. Background/crash durability
   needs an explicit resource policy; no extra worker was added in this slice.
4. Registry persistence/load: preserve the run binding, capture a read handle under
   lock, load outside it, then reject replacement/removal/resume races.
5. Attach/read_screen/scrollback/Find and pane presentation: share a read-only source;
   reject input and process mutation against completed views. Remote support remains
   a separate authenticated Helper operation, not a local-file fallback.
6. Resource retention/GC: bound the number of immutable artifacts before enabling
   publication. Never delete the artifact currently being inspected.

## Acceptance still required

Unit tests in `completed_terminal.rs` cover the primitive in isolation: exact-run
round trip of a real emulator capture (history, hyperlink annotations, enhanced
keyboard snapshot, modes); capture rejection for remote records, dead Holders,
unverified or inconsistent identity and impossible epochs; wrong session, reused
ID, old birth/epoch and mismatched exits; duplicate publication leaving the first
artifact intact; partial marker buffers, epoch regressions and oversized captures;
tampered, truncated and oversized files; RLE expansion bombs rejected before cell
allocation; symlink, FIFO, group-readable and shared-directory refusal; and the
two-operation admission bound.

Still required before this counts as recovery acceptance: actual PTY natural exit
and Stop; exact Engine/app-upgrade reopen; pending resume races; input queries and
OSC notification suppression on completed views; failed file/directory fsync;
failed launch preserving prior artifacts; slow storage not blocking Registry/live
input; immutable-artifact retention/GC; and native before/after pane evidence.
