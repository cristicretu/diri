# Delayed gesture delivery and opaque overview

These native GPUI captures use the same eight synthetic saved tabs and two real
disposable shell PTYs as `../gesture-acceptance/`. The earlier captures remain
unchanged there.

`delayed-strokes-peek.png` follows two reverse strokes whose release and next
stroke are consumed together. The first release settles to peek; the next
stroke keeps its own origin. The fixture verifies the selected tab and PTY
geometry remain unchanged in normal and reduced motion.

`vertical-overview.png` shows the fully settled overview. Its backdrop now
reaches full opacity so underlying terminal text cannot show through the gaps.
Intermediate motion continues to interpolate the backdrop.

Reproduce from `diri/` on macOS:

```sh
DIRI_TEST_APPKIT_GESTURE=1 cargo test -p diri-app --test tab_gesture_appkit
cargo test -p diri-app gesture_delivery
DIRI_GESTURE_LIVE_OUTPUT=1 DIRI_GESTURE_SCREENSHOTS=/tmp/gesture-dark cargo test -p diri-app --release live_gesture_orientation_cancel_and_saved_tab_selection -- --ignored --nocapture
```

Add `DIRI_VISUAL_LIGHT=1` for light captures. The AppKit regression includes
withheld consumption across strokes and bounded overflow: cancellation closes
the presentation, held contacts cannot reopen it, and a fresh stroke after lift
works. Deterministic delivery tests cover all 256 consumer partitions in both
motion modes and a 100,000-update motion flood. These are functional checks;
they do not establish physical trackpad behavior or display timing.
