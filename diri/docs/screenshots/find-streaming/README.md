# Find while terminal output continues

These synthetic Diri captures come from real disposable local PTYs and the native Metal renderer. The shell receives a new line nominally every 20 ms throughout the workflow. Its output wraps at the actual PTY width.

- `wide-dark-wide-glyph.png`, `wide-dark-combining.png`: 900 × 520 point pane, 17 point font.
- `narrow-light-wide-glyph.png`, `narrow-light-combining.png`: 440 × 520 point pane, 21 point font.
- `wide-dark-paused.png`, `narrow-light-paused.png`: explicit navigation into retained history. Further output leaves the selected text and source unchanged.

The fixture also checks automatic refresh before explicit navigation, Return to live / Refresh results, source release on Find close, and unchanged process identity and PTY rows/columns. These are synthetic UI events and screenshot readbacks, not physical IME or display-presentation latency measurements.

Reproduce on macOS:

```sh
DIRI_FIND_CAPTURES=/tmp/diri-find-captures cargo test -p diri-app real_pty_find_preserves_geometry_across_fonts_and_widths -- --ignored --nocapture
```
