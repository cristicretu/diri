# Interface cleanup screenshots

Native GPUI renders with deterministic fixture data. The before images were
captured from sidebar PR #222; the after images show this change. Composer
fixtures show the project as `diri` and intentionally disable launching Agents.

Render from `diri/` on macOS:

```sh
DIRI_VISUAL_OUTPUT=docs/screenshots/interface-cleanup/palette-after.png cargo test -p diri-app render_command_palette_preview_screenshot -- --ignored --nocapture
DIRI_VISUAL_OUTPUT=docs/screenshots/interface-cleanup/composer-after.png DIRI_RECIPE_VISUAL_SCENARIO=composer cargo test -p diri-app render_launch_recipes_preview_screenshot -- --ignored --nocapture
DIRI_VISUAL_OUTPUT=docs/screenshots/interface-cleanup/appearance-after.png cargo test -p diri-app render_appearance_settings_preview_screenshot -- --ignored --nocapture
```

For the empty light composer, set `DIRI_RECIPE_VISUAL_SCENARIO=shortcuts` and
`DIRI_RECIPE_VISUAL_THEME=dirijor-light`. For light Appearance, set
`DIRI_APPEARANCE_THEME=dirijor-light`. These are included as `composer-light.png`
and `appearance-light.png`.

The account picker was also inspected with `DIRI_VISUAL_ACCOUNTS=1`, and the
recipe library with `DIRI_RECIPE_VISUAL_SCENARIO` unset. Both remain available
through the compact toolbar.
