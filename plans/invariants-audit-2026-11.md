# Unenforced-invariant audit, 2026-11

An audit of `origin/main` @ `36c2b0041` looked for invariants Diri relies on
without enforcing them. Each of the top 20 findings has its own fix PR, and each
PR adds a regression test.

| # | Invariant now enforced | PR |
|---|---|---|
| 1 | Schedules/tasks/receipts SQLite stays out of the Linux runtime tmpfs | [#1027](https://github.com/cristicretu/diri/pull/1027) |
| 2 | Receipts expire; a failure before any effect frees the id | [#1044](https://github.com/cristicretu/diri/pull/1044) |
| 3 | A Holder transport error is not Agent death | [#1036](https://github.com/cristicretu/diri/pull/1036) |
| 4 | Child output cannot fake a Holder exit marker | [#1030](https://github.com/cristicretu/diri/pull/1030) |
| 5 | The output log keeps writing after a failed rewrite | [#1028](https://github.com/cristicretu/diri/pull/1028) |
| 6 | Every remote command resolves the same socket root; launch never resets a live Holder | [#1031](https://github.com/cristicretu/diri/pull/1031) |
| 7 | Disk/quota/fd exhaustion degrades the Holder instead of killing the Agent | [#1051](https://github.com/cristicretu/diri/pull/1051) |
| 8 | Remove/archive rollback restores the post-terminate record | [#1043](https://github.com/cristicretu/diri/pull/1043) |
| 9 | Unknown record fields survive a downgrade; a newer `state.json` is never overwritten | [#1029](https://github.com/cristicretu/diri/pull/1029) |
| 10 | Grid and scrollback decodes are bounded before they allocate | [#1032](https://github.com/cristicretu/diri/pull/1032) |
| 11 | Terminal size is capped at 4096×4096 and 1M cells | [#1046](https://github.com/cristicretu/diri/pull/1046) |
| 12 | Every control reply fits the frame limit | [#1037](https://github.com/cristicretu/diri/pull/1037) |
| 13 | A corrupt or newer `prefs.json` never panics at launch | [#1034](https://github.com/cristicretu/diri/pull/1034) |
| 14 | core-v2 style ids are reclaimed, not wrapped at 65,535 | [#1047](https://github.com/cristicretu/diri/pull/1047) |
| 15 | At most 32 combining marks per cell, in both cores | [#1033](https://github.com/cristicretu/diri/pull/1033) |
| 16 | Large remote pastes and recoverable Holder errors keep the session alive | [#1035](https://github.com/cristicretu/diri/pull/1035) |
| 17 | A partial PTY write reports exactly how much input arrived | [#1045](https://github.com/cristicretu/diri/pull/1045) |
| 18 | Every launch path holds the account gate; resume never kills a live remote run | [#1050](https://github.com/cristicretu/diri/pull/1050) |
| 19 | Saved sidebar layout is kept until the session list arrives | [#1048](https://github.com/cristicretu/diri/pull/1048) |
| 20 | A stale session list cannot resurrect closed tabs | [#1049](https://github.com/cristicretu/diri/pull/1049) |

## Merge notes

- Files several PRs touch, so later merges need a rebase:
  - `core_v2/page.rs`: #1033 and #1047.
  - `diri-app/src/store/mod.rs`: #1048 and #1049.
  - `holder/server.rs`: #1028, #1030, #1036 and #1045.
  - `diri-remote/src/holder.rs`: #1035 and #1051.
  - `registry.rs`: #1029 and #1043.
- #1032 and #1046 share the 4096×4096 / 1M-cell limits.
- Lints and the Windows `file_trigger` test already fail on `main`, so those red
  checks on these PRs are not caused by the fixes.
