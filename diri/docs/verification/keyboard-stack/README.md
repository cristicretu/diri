# Keyboard-mode stack overflow regression

With Kitty keyboard handling enabled, the 4,097th push removed index 0 from the
window title stack. An empty title stack panicked; a populated title stack lost
an unrelated entry and left the keyboard stack unbounded.

The fix removes only the oldest keyboard mode. The regression checks the empty
title case, preserved title contents, the 4,096-mode cap, oldest-mode eviction,
and pop restoring the previous keyboard flags. The vendored parser suite passes
144 tests and 1 doctest. The screenshot is a synthetic test report, not app UI.

Run from `diri/` with the vendored VTE patch:

```sh
cargo test --manifest-path vendor/alacritty_terminal/Cargo.toml \
  --features compact-history \
  --config 'patch.crates-io.vte.path="ABSOLUTE_DIRI_PATH/vendor/vte"' \
  keyboard_stack_overflow
```
