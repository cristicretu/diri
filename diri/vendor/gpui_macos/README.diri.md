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

`src/shaders.metallib` is `src/shaders.metal` compiled with upstream's flags.
Bundling it keeps ordinary Diri builds reproducible on machines where Xcode's
separately downloaded Metal Toolchain is not installed. The shader source
differs from upstream only by the continuous-corner patch below, so after
editing `src/shaders.metal` regenerate the library (needs
`xcodebuild -downloadComponent MetalToolchain` once). `scene.h` is the cbindgen
header the `runtime_shaders` build script writes to its `OUT_DIR`, for example
by `cargo check`ing a scratch crate that depends on this path with that feature:

```sh
xcrun -sdk macosx metal -gline-tables-only -mmacosx-version-min=10.15.7 -MO \
  -c src/shaders.metal -include scene.h -o shaders.air
xcrun -sdk macosx metallib shaders.air -o src/shaders.metallib
```

The continuous-corner patch replaces the quad shader's quarter-circle corner
with a curvature-continuous quarter superellipse that starts 1.528x the radius
from the corner and crosses the diagonal where the circle did, the way AppKit
draws windows, menus and buttons. It applies to every rounded quad, border,
rounded image sprite and unblurred shadow through `quad_sdf`; blurred shadows
keep the analytic circular falloff, where the difference is invisible. Radius 0
keeps the sharp fast path, and corners that have no room to grow (capsules,
radius >= half the short side) stay exactly circular. The math is documented at
`DIRI_CONTINUOUS_CORNER_EXTENT` in `src/shaders.metal`; setting it to `1.0`
restores upstream's corners. Linux and other renderers keep circular corners.

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

The scene capture patch (`MetalRenderer::capture_scene_region`, used by
`MacWindow::capture_scene_region` and `MetalHeadlessRenderer`) implements
GPUI's `Window::capture_region` (see `vendor/gpui/DIRI_PATCHES.md` section 4).
It re-renders the last frame's scene into a transient private offscreen
target the size of the drawable (it never touches the layer's drawables or
presents), blits the requested region into a private mipmapped texture,
lets the GPU generate the mip levels, copies every level linearly into one
shared buffer, and waits for that one command buffer. Reading back is then a
plain copy rather than a CPU detile of a managed texture. Nothing is cached
between captures, so it adds no resident memory.

`MetalHeadlessRenderer::transparent()` (test support only) builds the headless
renderer cleared to transparent, as a transparent window's is, so a capture
keeps the alpha of glass chrome. The site clip recorder uses it; GPUI's test
window ignores `set_background_appearance`, so there is no other way to ask.

The glyph raster style patch (see `vendor/gpui/DIRI_PATCHES.md`) adds a
second `CGTextStroke` pass when `RenderGlyphParams::stroke` is non-zero, with
one more device pixel of raster margin, and a `font_faces` listing plus a
sub-1.0 weight snap in `font_id`. With `stroke == 0` the rasterizer runs
upstream's single fill unchanged.
