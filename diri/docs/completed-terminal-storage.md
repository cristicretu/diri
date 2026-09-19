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

## Lifecycle integration (current state)

1. **Binding.** `Registry::bind_completed_run` writes `completed-run.json` in
   the session's recovery directory the first time a local held Session
   reports a verified child birth and Holder epoch (`Session::holder_run`,
   captured only from an alive verified stat at launch/adoption). It runs from
   spawn, adoption and the events watcher fold, once per run. A Holder that
   never reported a verified identity leaves the run unbound and its output
   explicitly unavailable; nothing is inferred from PIDs, timestamps or logs.
2. **Final capture.** `pump_held` reaches the Holder's exit marker only after
   the log was drained to it. With an empty marker buffer it samples the
   emulator once (the same sampler as the restart checkpoint) together with
   the marker's exit facts, and hands that `CompletedCapture` to the Registry
   exactly once. A partial marker retains nothing rather than a screen that
   may be missing its tail. Detach/stop never captures.
3. **Publication.** The events watcher takes `take_completed_publications()`
   under the Registry lock and publishes each outside it. The directory is
   `completed-terminals/` beside the state file, created owner-only on first
   use. A duplicate run is refused by the store and logged.
4. **Reading.** `Registry::completed_run(id)` returns a handle only for a local,
   exited record with no live Session, from the in-memory binding or the
   persisted `completed-run.json`. `session.read_screen` and
   `session.read_scrollback` load it after releasing the Registry, then
   revalidate that the record is unchanged before answering; a resume or
   removal that raced the read answers `completed_terminal_stale`. Input,
   resize and process facts remain impossible for such records.
5. **Removal.** `Registry::remove` discards the record's artifact with its
   binding, so nothing can resolve it afterwards.

Still unwired: pane attachment and Find (`session.read_scrollback_cells` and
the attach stream still require a live Session), remote records (a separate
authenticated Helper operation), artifact count/GC beyond removal, and an
explicit reservation of the *next* run before launch. A resume of a completed
record replaces the binding when the new Holder reports its identity.

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

`tests/holder_session.rs::completed_terminal_survives_engine_replacement` runs a
real Holder to exit, publishes off the lock, replaces the Registry, restores with
no Holder to adopt, loads the exact run and removes it with the record. A control
test serves `session.read_screen`/`read_scrollback` from a retained artifact and
refuses another exit of the same record.

Still required before this counts as recovery acceptance: Stop (explicit) as a
completion; exact Engine/app-upgrade reopen through the desktop; native pane
presentation of a retained terminal; failed file/directory fsync; slow storage
under live input; retention/GC beyond removal; and native before/after evidence.
