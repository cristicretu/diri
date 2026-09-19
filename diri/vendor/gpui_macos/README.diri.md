# Diri GPUI macOS patch

This directory is copied from `zed-industries/zed` revision
`dc2a339d5d043da448a3f7ddc7c0a85c63864aad`, crate `gpui_macos`.

Diri's patch makes the renderer's full-window path and 4x MSAA textures lazy.
Normal Diri scenes use quads, glyph sprites, and CoreGraphics-rasterized brand
marks, so eagerly allocating these textures consumed substantial unified memory
without rendering any paths. Scenes that do contain a path still allocate the
same textures on demand and retain upstream antialiasing quality.

The main and path-sprite pipelines also use source-over alpha blending. Upstream
adds destination alpha unchanged, making glyph coverage and translucent badges
too opaque and producing dark fringes over bright window backdrops. RGB blending
is unchanged; opaque surfaces retain their existing appearance.

To check transparent glyphs, rounded badges, and paths against opaque references
over white, gray, and black, run from Diri's workspace on a Mac with GPU access.
Selecting `diri-term` enables the existing GPUI test-support dev-dependencies
for this patched crate, which is not a workspace member:

```sh
cargo test -p diri-term -p gpui_macos compositing_tests
```

`src/shaders.metallib` is the unchanged upstream shader source compiled from
that pinned revision. Bundling it keeps ordinary Diri builds reproducible on
machines where Xcode's separately downloaded Metal Toolchain is not installed.

The native key-dispatch patch exposes read-only hardware key codes during the
matching synchronous callback. A nested/unwind-safe scope restores prior state
and never alters GPUI's logical Keystroke or IME text. Diri's terminal alone uses
this metadata for DEC numeric-keypad identity; ordinary text fields and global
shortcuts retain their existing behavior. The app adds a direct edge to this
already-pinned crate, avoiding an unsafe ABI shim or another event monitor.

The alert patch (`MacWindow::prompt`) also moves the initial keyboard focus
onto the default button when an alert has only a default and a Cancel
button. Upstream leaves focus on Cancel there, and newer macOS routes Return
to the focused button, so Return cancelled a "Close / Cancel" alert instead
of closing. Three-button alerts keep upstream's focus on the middle button.
