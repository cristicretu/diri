# Find input and geometry verification

These synthetic Diri scenes render real disposable shell PTY output. The fixture first updates both shells at nominal 50 Hz, then pauses output before entering each query. They verify geometry and Unicode highlight placement after output settles; they do not demonstrate sustained-output search correctness.

- `wide-dark-*.png`: 900 × 520 point pane, 17 px terminal font.
- `narrow-light-*.png`: 440 × 520 point pane, 21 px terminal font.
- `wide-glyph` searches for `界`; `combining` searches for `e` followed by U+0301.
- The floating field moves below the active first-row match. PTY rows/columns and process IDs are checked before Find opens and after it closes.

Render with the opt-in `terminal_pane::find_workflow_tests::real_pty_find_preserves_geometry_across_fonts_and_widths` test and `DIRI_FIND_CAPTURES` set to an output directory.

The separate harness-free `find_native_appkit` test, enabled by `DIRI_TEST_NATIVE_FIND=1`, sends actual Cocoa marked-text, selected-range, committed-text and candidate-rectangle messages to the installed native input handler. It checks a real PTY positive control with Find closed, no terminal bytes while Find owns composition, UTF-16 selections, candidate relocation, cancellation, stale callbacks, and restored input. The unbundled test explicitly claims its disposable controller because OS window activation is unavailable in this environment. Physical IME-device behavior and genuine OS activation are not certified by that fixture.
