# Native gesture acceptance

These captures render synthetic Diri workspaces backed by two disposable local
shell PTYs. Eight saved tabs share those sessions, including a split tab. The
shell output changes while the gesture and selection tests run.

The native GPUI test covers normal and reduced motion, reveal, reverse to peek,
orientation changes while revealed, Escape, offscreen keyboard selection,
pointer selection, focus restoration, stable session/grid identity, and unchanged
PTY geometry during each gesture. Orientation changes legitimately resize the
settled workbench.

Run from `diri/` on macOS:

```sh
DIRI_TEST_APPKIT_GESTURE=1 cargo test -p diri-app --test tab_gesture_appkit
DIRI_GESTURE_LIVE_OUTPUT=1 cargo test -p diri-app --release live_gesture_orientation_cancel_and_saved_tab_selection -- --ignored --nocapture
```

Set `DIRI_GESTURE_SCREENSHOTS=docs/screenshots/gesture-acceptance/dark` to capture
the five states. Add `DIRI_VISUAL_LIGHT=1` and use the `light` directory for the
other theme. Set `DIRI_GESTURE_PROFILE=/tmp/diri-gesture-scheduling.json` in a
separate run to collect draw and offscreen Metal submission timings without
pixel readback. The profile needs `DIRI_GESTURE_LIVE_OUTPUT=1`.

The AppKit test runs on the actual main thread and verifies cancellation
forwarding and responder cleanup. Contact coordinates are synthetic. GPUI frame
callbacks use synthetic ticks scheduled against wall time; draw timings measure
actual UI work, and submission timings measure CPU encoding/submission to a
reused offscreen Metal target. Neither test verifies physical trackpad input,
display vsync, or photon latency.
