# Worktrees in Settings

Settings → System → Worktrees inventories local projects and session repositories,
including worktrees created outside Diri. It groups compact rounded rows by
repository, with PR state/link, checkout age, disk usage, and cleanup protection.
All, Ready to clean, and Older than 30 days filters keep review focused. The
existing worktree delegation sheet remains available.

Refresh runs Git, GitHub CLI, and disk inspection in the Engine, off the UI
thread. GitHub CLI uses existing authentication; missing access or network
failures show PR unavailable. The newest 1,000 repository PRs are considered;
no match is labeled No recent PR. An open PR takes precedence over merged
history for a reused branch. Age means filesystem creation age (unknown where
unsupported), never last use. `du` reports allocated space without following
symlinks, with a two-second per-checkout budget. Unavailable sizes are excluded
from estimates; shared/cloned filesystem blocks can affect actual recovered space.

Cleanup is an explicit per-worktree confirmation. The additive `worktree.cleanup`
operation checks membership, the confirmed HEAD and branch, current local
sessions (including unknown states, subdirectories and symlinks), main/default/
locked/nested/detached checkouts, and merge evidence. Merge evidence is either
Git ancestry against the locally available default ref or a merged PR with the
exact checkout HEAD and default target (including squash merges). Local changes
or an open PR block suggestions. Age alone never authorizes deletion. Final
non-force Git removal rechecks changes and locks. Branches and commits remain;
ignored build files in the checkout are deleted. No fetch, force removal, branch
deletion, or remote cleanup is performed. Older engines cannot accept the new
cleanup operation; missing health data disables the action.

Synthetic native captures, from `diri/`:

```sh
DIRI_VISUAL_SETTINGS_TAB=worktrees DIRI_VISUAL_OUTPUT=/tmp/worktrees-dark.png cargo test -p diri-app render_settings_shell_preview_screenshot -- --ignored
DIRI_VISUAL_SETTINGS_TAB=worktrees DIRI_VISUAL_LIGHT=1 DIRI_VISUAL_OUTPUT=/tmp/worktrees-light.png cargo test -p diri-app render_settings_shell_preview_screenshot -- --ignored
```

These use the existing Settings transition, typography, rounded surfaces and
semantic colors. No production dependencies were added.
