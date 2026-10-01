# Diri GPUI patches

This directory is `crates/gpui` from `zed-industries/zed` revision
`dc2a339d5d043da448a3f7ddc7c0a85c63864aad`, routed in through
`[patch."https://github.com/zed-industries/zed.git"]` in `diri/Cargo.toml`, the
same way as `vendor/gpui_macos`. The first commit that added this directory
is byte-identical to upstream `src/`. Every change since is marked in the code
with `DIRI PATCH`.

The manifest spells out Zed's workspace-inherited dependencies at the versions
Zed's workspace pins at that revision. Zed's own crates point at the same git
revision. Examples, tests, docs and dev-dependencies are omitted: this crate is
not a Diri workspace member, so Cargo never builds them. Its behavior is tested
from `diri-app` (`gpui_view_cache_tests`).

## 1. Nested cached views survive a re-rendering cached ancestor

**Upstream behavior.** When a cached view (`Entity::cached` / `AnyView::cached`)
misses its cache, `ViewElement::prepaint` renders its subtree with
`window.refreshing = true`. That forces every cached view nested inside it to
re-render, even when it is not dirty. Upstream added this in #21165 (Nov 2024,
"Do not reuse render cache for nested items whose parents are re-rendered") to
fix a panic.

The panic came from stale indices. A cached view stores its prepaint and paint
ranges as absolute indices into the previous frame (hitboxes, tooltips,
deferred draws, dispatch nodes, element states, line layouts, scene
operations, listeners, input handlers, cursor styles, tab stops). Suppose the
parent is reused wholesale for a frame. Nothing inside it is visited, so the
nested view keeps indices from two frames ago. If the parent then re-renders
and the nested view reuses those indices, it reads the wrong ranges.

Notify also marks every ancestor dirty. Together, these rules meant a single
row's spinner tick re-rendered the entire cached sidebar.

**Patch.** Each cached view stores its ranges relative to the start of its
nearest cached ancestor's ranges. A view with no cached ancestor is relative
to the frame, which matches upstream's absolute indices. Every reuse primitive
copies element for element:

- `extend` for vectors;
- `DispatchTree::reuse_subtree`, one node per node;
- `LineLayoutCache::reuse_layouts`, one key pushed per key;
- `Scene::replay`, where recorded primitives are non-empty and replay with the
  same bounds and mask;
- `TabStopMap::replay`, one operation per operation.

So when an ancestor is reused wholesale, every relative offset inside it stays
valid. `Window` keeps a stack of `CachedViewBase { owner, previous_start, start }`
for the prepaint pass and another for the paint pass. When a cached view
misses, it pushes itself with the position its own ranges had last frame. A
nested view then reuses only if all of these hold:

- it was measured against the same owner;
- that owner knows its previous start;
- the cache key matches: bounds, content mask, text style, and now opacity;
- it is not dirty;
- `window.refreshing` is false.

When it reuses, it copies `previous_start + relative` and records its new
position relative to the new start. Paint mirrors prepaint: a view reused in
prepaint replays its paint range from its ancestor's previous paint start.

**What stays conservative.**

- `window.refresh()` still re-renders everything.
- A fresh deferred draw is a base with no previous start, so cached views
  inside it re-render.
- A cached view whose base owner changed re-renders. For example, a parent that
  switched between cached and uncached mounting.
- While an accessibility tree is being built (`window.a11y.is_active()`), the
  upstream forcing is kept, because a reused subtree contributes no
  accessibility nodes. This matches upstream for nested views. Upstream already
  drops a11y nodes for reused top-level cached views.

**Opacity.** `Div` applied `opacity` only during paint, and painted alpha is
baked into the primitives that reuse replays. `Interactivity::prepaint` now
also applies the element opacity, which only the paint functions read. The
view cache key includes the opacity seen at prepaint. Hover, active and
drag-over styles, the only ones computed differently at paint, already
refresh the window when they change.

**Test support.** `debug_bounds` are now also recorded in paint order and
replayed with a reused paint range (`Frame::debug_bounds_history`). Upstream
lost the `debug_selector` bounds of any reused cached view.

**Tests.** `crates/diri-app/src/gpui_view_cache_tests.rs`:

- A nested view is reused under a re-rendering cached parent.
- It stays hit-testable, click-dispatching, key-dispatching and painted across
  interleaved parent re-renders and wholesale parent reuse, while index-shifting
  elements are added ahead of both levels. With the ranges left absolute (the
  naive fix), this test fails.
- It still re-renders on its own notify, on an opacity change above it, and on
  `window.refresh()`.

## 2. `ViewElement::force_render_if`

A parent that passes a cached child view new inputs while it renders cannot
notify the child: a notify during a draw only takes effect in the next frame.
`entity.cached(style).force_render_if(changed)` skips reuse for this frame, as
if the child were dirty, and keeps its cache state for later frames. The sidebar
uses it for rows whose props changed (`crates/diri-app/src/sidebar/view/rows.rs`).

## 3. Floating panels are not throttled as inactive windows

**Upstream behavior.** `Window::new` wires `on_request_frame` so that, while a
frame is actually wanted (forced render, presentation, or a pending
`on_next_frame` callback), a window that is not active draws at most one frame
per 33.3 ms, "to save energy". On macOS "active" means the key window.

Diri's menus, popovers, the command palette and picture-in-picture are
`WindowKind::PopUp` panels that deliberately never become key
(`becomesKeyOnlyIfNeeded`, see `crates/diri-app/src/floating.rs`). So every
animation inside them, including their own appear fade, ran through that
throttle: replaying real 120 Hz display-link ticks through the rule measured
26.3 fps, because jitter often stretches the gap from four vsyncs to five
(PR #540 has the measurement).

**Patch.** `Window::new` records whether the window is a `PopUp`,
`AnchoredPopup` or `Floating` window, and the inactive-window cap skips those.
Normal windows keep upstream's behavior, and the thermal-pressure cap still
applies to every window. A panel only asks for frames while something in it
is moving, so this spends no energy at rest.

A narrower alternative, reporting these panels as active from `gpui_macos`,
was rejected: GPUI treats an active window as hovered and sets the app-wide
cursor from it, so a panel and the window under it would fight over the
cursor (an arrow against an I-beam while a terminal streams beneath the
palette).

## 4. Window control areas survive cached views and yield to buttons

Diri draws its own caption on Windows (`crates/diri-app/src/window_chrome.rs`):
title-row toolbars are `WindowControlArea::Drag` and the caption buttons are
`Min`/`Max`/`Close`, which `gpui_windows` answers `WM_NCHITTEST` with.

**Upstream behavior.** `reuse_paint` did not copy `window_control_hitboxes`,
so a cached view (the sidebar, every terminal pane) lost its drag area on the
first frame it was reused. And the hit-test callback returned the first area
whose hitbox was anywhere under the pointer, so a button inside a drag area
moved the window unless the button occluded.

**Patch.** `PaintIndex` carries `window_control_hitboxes_index` (with
`relative_to`/`rebased_on`) and `reuse_paint` copies the range. The callback
and the new `Window::window_control_area_at` share
`window_control_area_for`: an area claims the pointer only when its own hitbox
is the frontmost one there. Test in `crates/diri-app/src/root.rs`:
`windows_caption_buttons_sit_in_the_toolbar_for_both_tab_orientations`.

## Re-applying on a GPUI bump

1. Replace `src/` (and `build.rs`, `README.md`, `resources/`) with the new
   upstream crate. Commit that alone.
2. Re-diff the manifest against upstream `crates/gpui/Cargo.toml` and Zed's
   workspace `[workspace.dependencies]`. Keep `git`/`rev` for Zed crates in step
   with `gpui`/`gpui_macos`/`gpui_platform` in `diri/Cargo.toml`. Check that
   `git diff Cargo.lock` only drops gpui's `source` line.
3. Re-apply every `DIRI PATCH` hunk: `view.rs` (`ViewElement` prepaint/paint,
   `force_render_if`,
   `ViewElementState`, `ViewElementCacheKey`), `window.rs` (index
   `relative_to`/`rebased_on`, `CachedViewBase*`, base stacks, deferred-draw
   bases, `insert_debug_bounds`, `debug_bounds_history` replay, the
   `exempt_from_inactive_throttle` frame-rate exemption, and the window
   control hitbox reuse and `window_control_area_for`),
   `text_system/line_layout.rs` (`LineLayoutIndex` arithmetic) and
   `elements/div.rs` (prepaint opacity, `insert_debug_bounds`).
4. If upstream added a new per-frame collection to `PrepaintStateIndex` or
   `PaintIndex`, extend `relative_to`/`rebased_on`, and confirm its reuse
   copies element for element.
5. Run `cargo test -p diri-app --bin diri gpui_view_cache` and the sidebar
   tests.

## Scene storage released after sustained sparse frames

`Scene::clear` kept every primitive vector at its high-water capacity, so one
very large frame (the session overview, a huge paste) pinned tens of MB for the
life of the window (≈36 MB measured on the installed app). `clear` now counts
consecutive frames that used under a quarter of the reserved bytes (with at
least 1 MiB reserved) and, after 120 of them, shrinks each vector to twice the
latest frame's length. Steady frames never reallocate. Files: `src/scene.rs`.
Test: `a_scene_gives_back_capacity_a_single_large_frame_left_behind` in
`crates/diri-app/src/gpui_view_cache_tests.rs`. Re-apply on a GPUI bump by
re-adding `release_idle_capacity` and its call at the top of `Scene::clear`.

## Immediate frames and a frame-timing observer

`Window::request_immediate_frame` (and `PlatformWindow::request_immediate_frame`,
a default no-op) lets a latency-critical change, a terminal's keystroke echo,
draw as soon as the main thread is free instead of at the next display-link
tick. `gpui_macos` implements it by merging one request into the window's
display-link dispatch source (`WindowFrameSource::request_now`), refused
while the last present is under two refresh intervals old
(`immediate_frame_allowed`, refresh from `NSScreen.maximumFramesPerSecond`).
`src/frame_observer.rs` adds an optional process-wide observer of draw start
and end (`Window::draw`), Metal commit, GPU completion and present
(`metal_renderer.rs`), installed by Diri only under `DIRI_LATENCY_TRACE=1`.
Files: `src/platform.rs`, `src/window.rs`, `src/frame_observer.rs`,
`src/gpui.rs`; in `vendor/gpui_macos`: `window.rs`, `display_link.rs`,
`metal_renderer.rs`. Tests: `immediate_frames_wait_until_the_last_present_is_on_screen`
(gpui_macos) and `only_a_keystroke_echo_asks_for_an_immediate_frame` (diri-app).

## Frame statistics

`Window::draw` stamps its phases into `FrameStats` (`src/frame_stats.rs`):
`layout` (root and uncached renders, layout requests), `prepaint` (Taffy,
cached views that missed, element prepaint), `paint`, `a11y` (accessibility
tree) and `finish` (scene sort, frame swap), plus the number of views
rendered and cached views replayed, and whether assistive technology was
attached. `Window::last_frame_stats` returns the last finished frame and
`Window::frame_stats_so_far` the frame being drawn, up to the call (Diri's
frame probe reads it from inside the root's paint). The cost is a few
`Instant::now()` calls and counter increments per frame.
`Window::set_accessibility_active_for_test` (test-support) attaches pretend
assistive technology so headless benches can draw frames the way a Mac
running an accessibility client does. Files: `src/frame_stats.rs`,
`src/gpui.rs`, `src/window.rs` (`frame_stats` fields and stamps,
`WindowInvalidator::phase`), `src/view.rs` (render/reuse counts),
`src/window/a11y.rs`. Exercised by `real_use_frame_distribution` in
`crates/diri-app/src/root.rs`.

## Sprite sort by index

`Scene::finish` stable-sorted the monochrome, subpixel and polychrome sprite
vectors by `(order, tile_id)` with `sort_by_key`, moving each 100+ byte
sprite through every merge pass; with a few terminals on screen that is tens
of thousands of glyph sprites per frame (11% of `Window::draw` in a sample of
the installed app). `sort_sprites` sorts `(key, index)` pairs instead, which
breaks ties by position exactly as the stable sort did, then applies the
permutation in place along its cycles, moving each sprite once; a frame
already in order moves nothing. The pairs live in a reused `sort_keys`
scratch that `release_idle_capacity` accounts for and shrinks with the rest.
Draw order, and therefore pixels, are unchanged. Test:
`the_sprite_sort_matches_a_stable_sort_by_key` in
`crates/diri-app/src/gpui_view_cache_tests.rs`. Re-apply on a GPUI bump by
replacing the three sprite `sort_by_key` calls in `Scene::finish`.
