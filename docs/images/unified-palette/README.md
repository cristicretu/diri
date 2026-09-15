# Unified palette

Cmd K opens commands, Cmd P opens projects, and Cmd Shift H opens chat history. All three shortcuts use the same 600-point surface, 48-point search header, 36-point virtual rows, aligned keyboard hints, and scroll-edge fades. The history page retains seven visible rows; commands, projects, and themes show up to nine. Settings contracts to two rows.

The back button, Cmd [, or Backspace in an empty query restores the preceding page’s query and selection. Page changes use a 140 ms fade, small horizontal movement, and height transition. Reduce Motion skips these transitions. Typing and selection remain immediate.

Settings → Color theme previews the highlighted theme on keyboard movement or pointer hover. Enter/click saves once. Escape, Back, switching pages, dismissal, and releasing the palette discard the transient preview. The override is separate from persisted preferences, so unrelated preference saves cannot accidentally save a preview. All settings still opens the full settings workspace.

Every page virtualizes its rows. Project scanning and debounced ranking stay on background executors, with canceled/stale rankings unable to change another page. History retains its cached scanner and prepared search index. Unchanged store notifications do not redraw the palette. The GPUI regression fixture verifies keyboard wrapping through 20,000 projects without building offscreen rows (about 6 ms for wrap plus layout in the local debug harness; this is not an end-to-end release latency benchmark).

Screenshots contain synthetic data and require no live daemon. From `diri/`, run native captures sequentially:

```sh
DIRI_VISUAL_PAGE=commands DIRI_VISUAL_OUTPUT=/tmp/commands.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored
DIRI_VISUAL_PAGE=history DIRI_VISUAL_OUTPUT=/tmp/history.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored
DIRI_VISUAL_PAGE=projects DIRI_VISUAL_OUTPUT=/tmp/projects.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored
DIRI_VISUAL_PAGE=settings DIRI_VISUAL_OUTPUT=/tmp/settings.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored
DIRI_VISUAL_PAGE=themes DIRI_VISUAL_OUTPUT=/tmp/themes.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored
DIRI_VISUAL_PAGE=themes DIRI_VISUAL_LIGHT=1 DIRI_VISUAL_OUTPUT=/tmp/themes-light.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored
```

`DIRI_VISUAL_QUERY` optionally filters the selected page.
