# Remote usage preview

Deterministic fixture data: local usage, a refreshed SSH host, and an unavailable
host retaining its saved totals. No live account or transcript data is used.

From `diri/` on macOS:

```sh
DIRI_VISUAL_REMOTE=1 DIRI_VISUAL_OUTPUT="$PWD/docs/screenshots/remote-usage/dark.png" \
  cargo test -p diri-app render_usage_settings_preview_screenshot -- --ignored
DIRI_VISUAL_REMOTE=1 DIRI_VISUAL_LIGHT=1 DIRI_VISUAL_OUTPUT="$PWD/docs/screenshots/remote-usage/light.png" \
  cargo test -p diri-app render_usage_settings_preview_screenshot -- --ignored
```
