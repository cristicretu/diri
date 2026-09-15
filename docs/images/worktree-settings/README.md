# Worktrees in Settings

Settings → System → Worktrees inventories local projects and session repositories,
including worktrees created outside Diri. It shows PR state/link, checkout age,
local changes, session protection and cleanup candidates. All, Ready to clean,
and Older than 30 days filters narrow the inventory. At most 40 rows are built
per page; Previous/Next navigates the rest. The existing delegation sheet stays
available.

## Large inventories and connection responsiveness

The original overview ran synchronously on the control connection. Git/PR/disk
work prevented the same connection from answering Hello, so larger inventories
could cause heartbeat timeouts and reconnects before any rows appeared.

`worktree.scan` now starts or joins one on-demand Engine worker, shared across
windows. It publishes discovery rows before PR/status enrichment. Each reply
contains at most 32 row updates, with a generation and cursor; unchanged rows
are not transferred again. The app applies updates by path, drains available
pages at 16 ms intervals, and checks progress every 500 ms only while Worktrees
is visible. Checked rows remain protected from cleanup until the scan completes.
The worker stops remaining work when no view has polled for ten seconds (after
its current bounded operation); it has no idle timer or background rescan.
The update journal is replaced on refresh and contains at most two entries per
checkout. Repeated refreshes join an existing worker. Legacy overview requests
also join that worker, on the existing bounded background request path, keeping
Hello readable. Cleanup uses that background request path too.

Default-ref resolution and merged-branch reachability are computed once per
repository, and local session paths are canonicalized/indexed once per scan.
Discovered repositories are reused for session subdirectories, while nested
Git boundaries are respected. Nested worktree protection uses sorted paths
rather than an all-pairs comparison. PR inspection uses existing GitHub CLI
access and the newest 1,000 repository PRs; unavailable access is explicit.

Refresh does **not** run `du`. **Measure cleanup size** requests allocated-space
estimates only for verified cleanup candidates, with a two-second per-checkout
budget and no symlink traversal. Unmeasured/timed-out values are excluded;
shared filesystem blocks can affect actual recovered space. Age means checkout
creation age, not last use. Errors retain prior rows and never present zero
worktrees as a successful empty inventory.

## Cleanup

Cleanup requires an inline confirmation and `worktree.cleanup`. The Engine
rechecks membership, confirmed HEAD and branch, local sessions (including
unknown states, subdirectories and symlinks), protected checkouts and merge
evidence. Main/default/locked/nested/detached checkouts, local changes, and open
PRs remain protected. Merge evidence is Git ancestry against the available
default ref or a merged PR with the exact checkout HEAD and default target,
including squash merges. Non-force Git removal rechecks changes and locks.
Branches/commits remain; ignored build files are removed. There is no fetch,
force removal, branch deletion, or remote cleanup. An old engine rejects the
new scan operation; no slow fallback silently reruns the original pipeline.

## Reproduction and measurements

From `diri/`, using the checked-in Rust toolchain and a warm debug build on
macOS arm64:

```sh
cargo test -p diri-engine worktree_inventory_does_not_block_hello --lib -- --nocapture
cargo test -p diri-engine worktree_scan_10000 --lib -- --nocapture
cargo test -p diri-engine worktree_discovery --lib -- --nocapture
cargo test -p diri-app worktree_settings_bounds_rendering -- --nocapture
```

The same-connection test builds 50 real worktrees, sends overview followed by
Hello, and requires Hello within 250 ms, before the full inventory. Before the
fix it failed at 251 ms; inventory took 3.20 s. Afterward Hello arrived in
0.1–0.3 ms and inventory took 0.78–0.88 s. Fixtures have no GitHub remote and
these timings do not claim network latency or large build-directory throughput.

The synthetic 10,000-entry test drains bounded in-memory pages in about 1.1 ms
and proves repeated refreshes use one worker. This is not an end-to-end wire
benchmark. The GPUI fixture with 10,000 entries renders the first and last pages
in about 121 ms including setup, and verifies off-page rows are absent.

The earlier tests covered cleanup safety and static screenshots; they missed
the shared control connection under scan load. These same-connection, progress,
concurrent-refresh, cancellation and bounded-rendering regressions cover that gap.

## Native captures

Synthetic data only, from `diri/`:

```sh
DIRI_VISUAL_SETTINGS_TAB=worktrees DIRI_VISUAL_OUTPUT=/tmp/worktrees.png cargo test -p diri-app render_settings_shell_preview_screenshot -- --ignored
DIRI_VISUAL_SETTINGS_TAB=worktrees DIRI_VISUAL_WORKTREE_PROGRESS=1 DIRI_VISUAL_OUTPUT=/tmp/worktrees-progress.png cargo test -p diri-app render_settings_shell_preview_screenshot -- --ignored
DIRI_VISUAL_SETTINGS_TAB=worktrees DIRI_VISUAL_WORKTREE_ERROR=1 DIRI_VISUAL_OUTPUT=/tmp/worktrees-error.png cargo test -p diri-app render_settings_shell_preview_screenshot -- --ignored
```

`DIRI_VISUAL_LIGHT=1` captures the light theme. No production dependencies added.
