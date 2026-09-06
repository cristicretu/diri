# Past conversations

The history picker is a search header and 36-point single-line rows. Agent marks and titles lead the list; ages stay quiet. There is no count strip, repeated project column, or footer. Hover a row for its full title, agent, path, and any folder warning. Search still matches titles, projects, and agents.

A thin SVG return-key cue replaces the age on the selected or hovered row without shifting its title. Enter and Esc share a 28 × 20-point outlined keycap, corner radius, and right edge; the search icon and text align with the row icons and titles. Rows use the shared seven-point row radius. Pressed rows give immediate feedback. Refreshing and opening use the shared activity indicator, which respects Reduce Motion. The result list fades at edges with more content, and the floating surface retains its short entrance fade.

The screenshots use synthetic history without a live daemon. Regenerate from `diri/`:

```sh
DIRI_VISUAL_OUTPUT=/tmp/history-dark.png cargo test -p diri-app render_history_preview_screenshot -- --ignored
DIRI_VISUAL_OUTPUT=/tmp/history-light.png DIRI_VISUAL_LIGHT=1 DIRI_VISUAL_NARROW=1 cargo test -p diri-app render_history_preview_screenshot -- --ignored
```

Additional fixture options: `DIRI_VISUAL_QUERY`, `DIRI_VISUAL_OPENING=1`, `DIRI_VISUAL_HISTORY_COUNT=2`, and `DIRI_VISUAL_HISTORY_SCROLL=51`. Run native captures sequentially.
