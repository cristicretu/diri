# Right sidebar declutter screenshots

Native GPUI renders with deterministic fixture data, captured headlessly.
The `-before` images come from `origin/main` at `13630d15`; the `-after`
images show this change.

Render from `diri/` on macOS (paths must be absolute; the fixtures run with
the crate directory as their working directory):

```sh
OUT=$PWD/docs/screenshots/right-sidebar-focus
# Header: Review with a change count, Details reopened from preferences, Preview placeholder.
DIRI_VISUAL_OUTPUT=$OUT/inspector-header-after.png DIRI_VISUAL_REVIEW_COUNT=1 \
  cargo test -p diri-app render_workspace_preview_screenshot -- --ignored
# The overflow menu open.
DIRI_VISUAL_OUTPUT=$OUT/inspector-menu-after.png DIRI_VISUAL_REVIEW_COUNT=1 DIRI_VISUAL_MENU=1 \
  cargo test -p diri-app render_workspace_preview_screenshot -- --ignored
# Condensed Details with the Artifacts fixture session.
DIRI_VISUAL_OUTPUT=$OUT/inspector-details-after.png DIRI_VISUAL_DETAILS=1 \
  cargo test -p diri-app render_workspace_preview_screenshot -- --ignored
# Two Preview tabs.
DIRI_VISUAL_OUTPUT=$OUT/inspector-browser-after.png DIRI_VISUAL_BROWSER=1 DIRI_VISUAL_BROWSER_TABS=1 \
  cargo test -p diri-app render_workspace_preview_screenshot -- --ignored
# The 300pt minimum width, where the Preview placeholder keeps only its icon.
DIRI_VISUAL_OUTPUT=$OUT/inspector-header-narrow.png DIRI_VISUAL_WIDTH=300 DIRI_VISUAL_REVIEW_COUNT=1 \
  cargo test -p diri-app render_workspace_preview_screenshot -- --ignored
# Terminal header with the session strip.
DIRI_QOL_SCREENSHOT=$OUT/terminal-strip-after.png DIRI_QOL_SCENE=strip \
  cargo test -p diri-app render_terminal_qol_screenshot -- --ignored
# Settings › Diagnostics with the Session status section.
DIRI_VISUAL_OUTPUT=$OUT/diagnostics-after.png \
  cargo test -p diri-app render_diagnostics_preview_screenshot -- --ignored
# Sidebar hover card (host and memory rows appear when the session has them).
DIRI_VISUAL_OUTPUT=$OUT/hover-card-after.png \
  cargo test -p diri-app render_session_hover_card_screenshot -- --ignored
```

The before images used the same fixtures on `main`: no switches for the
surface chooser, `DIRI_VISUAL_FILES=1` for the Files surface,
`DIRI_VISUAL_BROWSER=1 DIRI_VISUAL_BROWSER_TABS=1` for browser tabs, and no
scene for the plain terminal header.
