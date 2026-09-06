# Past conversations

The history picker uses a single search header, 36-point single-line rows, and the shared panel and row corner radii. Unboxed agent marks and conversation titles lead each row, with quieter project names and ages aligned on the right. Hover for the full title, agent, and path; the selected project's path and a Resume hint appear in the footer. Search still matches titles, projects, and agents.

The result list fades at edges with more content and remains clear when all results fit. The shared floating surface provides its entrance fade and respects Reduce Motion.

The screenshots use synthetic history without a live daemon. Regenerate from `diri/`:

```sh
DIRI_VISUAL_OUTPUT=/tmp/history-dark.png cargo test -p diri-app render_history_preview_screenshot -- --ignored
DIRI_VISUAL_OUTPUT=/tmp/history-light.png DIRI_VISUAL_LIGHT=1 DIRI_VISUAL_NARROW=1 cargo test -p diri-app render_history_preview_screenshot -- --ignored
```

Additional fixture options: `DIRI_VISUAL_QUERY`, `DIRI_VISUAL_OPENING=1`, `DIRI_VISUAL_HISTORY_COUNT=2`, and `DIRI_VISUAL_HISTORY_SCROLL=51`. Run native captures sequentially.
