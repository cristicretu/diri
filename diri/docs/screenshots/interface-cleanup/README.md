# Interface cleanup screenshots

Native GPUI renders with deterministic fixture data. The before images were
captured from sidebar PR #222; the after images show this change. The composer
fixture intentionally disables launching Agents. Its project label reflects the
worktree used to render it.

Render from `diri/` on macOS:

```sh
DIRI_VISUAL_OUTPUT=docs/screenshots/interface-cleanup/palette-after.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored --nocapture
DIRI_VISUAL_OUTPUT=docs/screenshots/interface-cleanup/composer-after.png DIRI_RECIPE_VISUAL_SCENARIO=composer cargo test -p diri-app render_launch_recipes_preview_screenshot -- --ignored --nocapture
DIRI_VISUAL_OUTPUT=docs/screenshots/interface-cleanup/appearance-after.png cargo test -p diri-app render_appearance_settings_preview_screenshot -- --ignored --nocapture
```

The account picker was also inspected with `DIRI_VISUAL_ACCOUNTS=1`. Its project
label truncates to leave room for the account and checkout controls.
