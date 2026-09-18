//! Blurred popup panels for floating chrome under the glass material.
//!
//! GPUI cannot blur behind an element, so a menu drawn inside the main window
//! can only be tinted: whatever sits beneath it stays sharp. The sidebar gets
//! its glass from the window's own backdrop, and so can a menu if it is a
//! window too. On macOS a popover therefore opens as a non-activating panel
//! whose backdrop WindowServer blurs, positioned exactly where the in-window
//! element would have been. The main window keeps key status the whole time,
//! so keyboard handling, Escape, and the dismiss scrim stay where they are,
//! and the in-window path remains the one tests exercise.
//!
//! A host view declares a [`Target`] and, while its dropdown is open, renders
//! [`host_element`] in place of the surface. That measures the content and
//! opens, moves, or closes the panel; a global registry keyed by entity and
//! target keeps the panel windows, so the host carries no state of its own.
//! Panels close themselves when their target stops wanting them, and every
//! panel over a window closes when that window loses key status, with the
//! target's `dismiss` restoring the host's own state.

use std::collections::HashMap;
use std::rc::Rc;

use diri_ui::{Glass, Radius, SemanticColors};
use gpui::{
    Anchor, AnyElement, AnyWindowHandle, App, Bounds, BoxShadow, Context, DisplayId, Div, Entity,
    EntityId, Global, Pixels, Point, Render, Size, Subscription, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, size,
};

/// Present once the running app may open panels. Tests and previews never set
/// it, so they keep rendering popovers inside the window they can inspect.
struct FloatingPanels;

impl Global for FloatingPanels {}

pub(crate) fn enable(cx: &mut App) {
    cx.set_global(FloatingPanels);
    cx.set_global(Registry::default());
}

pub(crate) fn enabled(cx: &App) -> bool {
    cfg!(target_os = "macos") && cx.has_global::<FloatingPanels>()
}

/// Whether a view should host its floating chrome in panels: never in
/// previews and fixtures, and only under the glass material, since an opaque
/// window has nothing for a panel's blur to show.
pub(crate) fn uses_panels(preview: bool, colors: SemanticColors, cx: &App) -> bool {
    !preview && enabled(cx) && colors.material() == diri_ui::Material::Glass
}

/// One floating surface a view can host in a panel.
pub(crate) struct Target<T: 'static> {
    /// Distinguishes this surface from the view's other panels.
    pub key: &'static str,
    pub radius: f32,
    /// Builds the pixels the panel paints, or `None` once the surface is
    /// closed, which closes the panel.
    pub content: fn(&mut T, &mut Context<T>) -> Option<AnyElement>,
    /// Closes the surface from the host's side: what a click outside would
    /// do. Runs in the host's window when the window loses key status.
    pub dismiss: fn(&mut T, &mut Window, &mut Context<T>),
}

impl<T: 'static> Clone for Target<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Target<T> {}

/// Screen placement of a panel, in GPUI's global coordinates (the same space
/// `Window::bounds` reports for the main window).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelFrame {
    pub origin: Point<Pixels>,
    pub size: Size<Pixels>,
    pub radius: f32,
}

/// Resolves where a popover anchored inside `main` lands on screen, mirroring
/// `anchored().snap_to_window_with_margin(margin)` so the two hosts agree.
pub(crate) fn frame_in(
    main: Bounds<Pixels>,
    position: Point<Pixels>,
    anchor: Anchor,
    size: Size<Pixels>,
    radius: f32,
    margin: f32,
) -> PanelFrame {
    let margin = px(margin);
    let mut origin = position;
    match anchor {
        Anchor::TopLeft => {}
        Anchor::TopRight => origin.x -= size.width,
        Anchor::BottomLeft => origin.y -= size.height,
        Anchor::BottomRight => {
            origin.x -= size.width;
            origin.y -= size.height;
        }
        Anchor::TopCenter => origin.x -= size.width / 2.0,
        Anchor::BottomCenter => {
            origin.x -= size.width / 2.0;
            origin.y -= size.height;
        }
        Anchor::LeftCenter => origin.y -= size.height / 2.0,
        Anchor::RightCenter => {
            origin.x -= size.width;
            origin.y -= size.height / 2.0;
        }
    }
    let max_x = (main.size.width - margin - size.width).max(margin);
    let max_y = (main.size.height - margin - size.height).max(margin);
    origin.x = origin.x.min(max_x).max(margin);
    origin.y = origin.y.min(max_y).max(margin);
    PanelFrame {
        origin: main.origin + origin,
        size,
        radius,
    }
}

/// The panel's own material: the sidebar's settled glass, because that is
/// what a menu is meant to match, plus the same hairline and rim a floating
/// surface carries inside the window.
pub(crate) fn surface(
    colors: SemanticColors,
    radius: f32,
    width: f32,
    content: impl IntoElement,
) -> Div {
    div()
        .w(px(width))
        .rounded(px(radius))
        .overflow_hidden()
        .bg(Glass::panel_fill(colors))
        .border_1()
        .border_color(colors.floating_stroke())
        .shadow(vec![BoxShadow {
            color: Glass::rim(colors).into(),
            offset: point(px(0.0), px(1.0)),
            blur_radius: px(0.0),
            spread_radius: px(0.0),
            inset: true,
        }])
        .child(content)
}

/// Default corner radius for a menu panel.
pub(crate) const MENU_RADIUS: f32 = Radius::FLOATING_MENU;

/// Measures `content` the way the panel will lay it out, so the window can
/// open at its final size instead of resizing after a first frame. The
/// height is offered as a definite bound rather than min-content: a scroll
/// container asked for its min-content height answers with its `max_h`,
/// which left the account menu's window twice as tall as its rows.
pub(crate) fn measure(
    content: &mut AnyElement,
    width: f32,
    max_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) -> Size<Pixels> {
    let measured = content.layout_as_root(
        size(
            gpui::AvailableSpace::Definite(px(width)),
            gpui::AvailableSpace::Definite(max_height),
        ),
        window,
        cx,
    );
    size(px(width), measured.height.min(max_height))
}

/// Every open panel, keyed by the host entity and the target's key, plus the
/// window each host renders in.
#[derive(Default)]
struct Registry {
    panels: HashMap<(EntityId, &'static str), Entry>,
    mains: HashMap<EntityId, AnyWindowHandle>,
    /// One activation watcher per host entity; each closes every panel over
    /// its window, so any live watcher covers the whole window.
    watchers: HashMap<EntityId, Subscription>,
}

impl Global for Registry {}

struct Entry {
    panel: Panel,
    main: AnyWindowHandle,
    dismiss: Rc<dyn Fn(&mut App)>,
}

/// A zero-size element that lays `probe` out at `width` during prepaint and
/// then, outside this frame, opens or resizes `target`'s panel to that size
/// at `position`, anchored inside the host's window like `anchored()` would
/// with `margin` to the window edge. Render it only while the surface is
/// open; the panel closes itself once `target.content` returns `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn host_element<T: 'static>(
    target: Target<T>,
    mut probe: AnyElement,
    width: f32,
    position: Point<Pixels>,
    anchor: Anchor,
    margin: f32,
    window: &mut Window,
    cx: &mut Context<T>,
) -> AnyElement {
    let host = cx.weak_entity();
    let main_bounds = window.bounds();
    let display = window.display(cx).map(|display| display.id());
    register_host(window, cx);
    gpui::canvas(
        move |_, window, cx| {
            let max_height = (main_bounds.size.height - px(2.0 * margin)).max(px(120.0));
            let size = measure(&mut probe, width, max_height, window, cx);
            let frame = frame_in(main_bounds, position, anchor, size, target.radius, margin);
            cx.defer(move |cx| sync(&host, target, frame, display, cx));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
    .into_any_element()
}

/// Like [`host_element`], but the panel opens where this element sits: give
/// it the absolute offsets the in-window surface used and it reads its own
/// window position during prepaint, so a dropdown nested in a control needs
/// no anchor bookkeeping. `width: None` uses the element's laid-out width,
/// for surfaces that stretch between two edges of their container.
pub(crate) fn host_here<T: 'static>(
    target: Target<T>,
    mut probe: AnyElement,
    width: Option<f32>,
    anchor: Anchor,
    margin: f32,
    cx: &mut Context<T>,
) -> gpui::Canvas<()> {
    let host = cx.weak_entity();
    gpui::canvas(
        move |bounds, window, cx| {
            let Some(strong) = host.upgrade() else {
                return;
            };
            strong.update(cx, |_, cx| register_host(window, cx));
            let main_bounds = window.bounds();
            let display = window.display(cx).map(|display| display.id());
            let width = width.unwrap_or_else(|| f32::from(bounds.size.width));
            let position = match anchor {
                Anchor::TopLeft | Anchor::LeftCenter => bounds.origin,
                Anchor::TopRight | Anchor::RightCenter => point(bounds.right(), bounds.top()),
                Anchor::TopCenter => point(bounds.center().x, bounds.top()),
                Anchor::BottomLeft => point(bounds.left(), bounds.bottom()),
                Anchor::BottomRight => point(bounds.right(), bounds.bottom()),
                Anchor::BottomCenter => point(bounds.center().x, bounds.bottom()),
            };
            let max_height = (main_bounds.size.height - px(2.0 * margin)).max(px(120.0));
            let size = measure(&mut probe, width, max_height, window, cx);
            let frame = frame_in(main_bounds, position, anchor, size, target.radius, margin);
            cx.defer(move |cx| sync(&host, target, frame, display, cx));
        },
        |_, _, _, _| {},
    )
}

/// Records which window `cx`'s view renders in and, once per view, watches
/// that window's activation so a lost key status closes the view's panels.
fn register_host<T: 'static>(window: &mut Window, cx: &mut Context<T>) {
    let id = cx.entity_id();
    let main = window.window_handle();
    let needs_watcher = {
        let registry = cx.global_mut::<Registry>();
        registry.mains.insert(id, main);
        !registry.watchers.contains_key(&id)
    };
    if needs_watcher {
        // A panel is not part of its host's window; that window losing key
        // status is the only "click outside" it can observe, and the window
        // may stop drawing right after, so close here rather than on render.
        let subscription = cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() {
                close_all_over(window.window_handle(), cx);
            }
        });
        cx.global_mut::<Registry>()
            .watchers
            .insert(id, subscription);
    }
}

/// [`surface`] for a panel whose width the measurement chose: the panel
/// window is exactly that wide, so the surface fills it.
pub(crate) fn surface_full(colors: SemanticColors, radius: f32, content: impl IntoElement) -> Div {
    surface(colors, radius, 0.0, content).w_full()
}

/// Runs `f` against the view's own window. Handlers inside a panel receive
/// the panel's `Window`, which owns none of the view's focus or actions.
pub(crate) fn in_main_window<T: 'static>(
    this: &mut T,
    window: &mut Window,
    cx: &mut Context<T>,
    f: impl FnOnce(&mut T, &mut Window, &mut Context<T>) + 'static,
) {
    let main = cx
        .try_global::<Registry>()
        .and_then(|registry| registry.mains.get(&cx.entity_id()).copied());
    match main {
        Some(main) if main != window.window_handle() => {
            let host = cx.entity();
            App::defer(cx, move |cx| {
                let _ = cx.update_window(main, |_, window, cx| {
                    host.update(cx, |this, cx| f(this, window, cx));
                });
            });
        }
        _ => f(this, window, cx),
    }
}

/// Opens, moves, or closes `target`'s panel to match `frame`. Runs at App
/// level on purpose: opening a window draws its first frame at once, and
/// that frame renders the host, so the host must not be inside an update of
/// its own while the panel comes up.
fn sync<T: 'static>(
    host: &WeakEntity<T>,
    target: Target<T>,
    frame: PanelFrame,
    display: Option<DisplayId>,
    cx: &mut App,
) {
    let Some(strong) = host.upgrade() else {
        return;
    };
    let id = strong.entity_id();
    let slot = (id, target.key);
    let existing = cx.global_mut::<Registry>().panels.remove(&slot);
    let entry = match existing {
        Some(mut entry) => {
            entry.panel.set_frame(frame, cx);
            entry
        }
        None => {
            let Some(main) = cx.global::<Registry>().mains.get(&id).copied() else {
                return;
            };
            let render = {
                let source = strong.clone();
                move |_: &mut Window, cx: &mut App| -> Option<AnyElement> {
                    source.update(cx, |this, cx| (target.content)(this, cx))
                }
            };
            let on_empty = move |cx: &mut App| close_entry(slot, cx);
            let Some(panel) = Panel::open(&strong, frame, display, render, on_empty, cx) else {
                return;
            };
            let dismiss: Rc<dyn Fn(&mut App)> = {
                let host = host.clone();
                Rc::new(move |cx| {
                    let _ = cx.update_window(main, |_, window, cx| {
                        let _ = host.update(cx, |this, cx| (target.dismiss)(this, window, cx));
                    });
                })
            };
            Entry {
                panel,
                main,
                dismiss,
            }
        }
    };
    cx.global_mut::<Registry>().panels.insert(slot, entry);
}

fn close_entry(slot: (EntityId, &'static str), cx: &mut App) {
    if let Some(entry) = cx.global_mut::<Registry>().panels.remove(&slot) {
        entry.panel.close(cx);
    }
}

/// Closes every panel hosted over `main` and lets each host dismiss its
/// surface, as a click outside would have.
fn close_all_over(main: AnyWindowHandle, cx: &mut App) {
    let closing: Vec<Entry> = {
        let registry = cx.global_mut::<Registry>();
        let keys: Vec<_> = registry
            .panels
            .iter()
            .filter(|(_, entry)| entry.main == main)
            .map(|(slot, _)| *slot)
            .collect();
        keys.into_iter()
            .filter_map(|slot| registry.panels.remove(&slot))
            .collect()
    };
    for entry in closing {
        entry.panel.close(cx);
        (entry.dismiss)(cx);
    }
}

type RenderFn = Rc<dyn Fn(&mut Window, &mut App) -> Option<AnyElement>>;
type OnEmpty = Rc<dyn Fn(&mut App)>;

/// An open popup panel. Dropping the value does not close the window; call
/// [`Panel::close`] so the platform window is removed.
struct Panel {
    handle: AnyWindowHandle,
    frame: PanelFrame,
}

impl Panel {
    /// Opens a blurred, non-activating panel at `frame` that paints
    /// `render` and repaints whenever `source` notifies. Once `render`
    /// answers `None` the panel runs `on_empty`, which closes it.
    fn open<T: 'static>(
        source: &Entity<T>,
        frame: PanelFrame,
        display: Option<DisplayId>,
        render: impl Fn(&mut Window, &mut App) -> Option<AnyElement> + 'static,
        on_empty: impl Fn(&mut App) + 'static,
        cx: &mut App,
    ) -> Option<Self> {
        let render: RenderFn = Rc::new(render);
        let on_empty: OnEmpty = Rc::new(on_empty);
        let source = source.clone();
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: frame.origin,
                        size: frame.size,
                    })),
                    titlebar: None,
                    focus: false,
                    show: true,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Blurred,
                    display_id: display,
                    ..Default::default()
                },
                move |window, cx| {
                    #[cfg(target_os = "macos")]
                    crate::macos::floating_panel::prepare(window, frame.radius);
                    #[cfg(not(target_os = "macos"))]
                    let _ = window;
                    cx.new(|cx| {
                        cx.observe(&source, |_, _, cx| cx.notify()).detach();
                        PanelView {
                            render,
                            on_empty,
                            frames: 0,
                            settled_frames: 0,
                            revealed: false,
                        }
                    })
                },
            )
            .ok()?;
        Some(Self {
            handle: handle.into(),
            frame,
        })
    }

    /// Moves or resizes the panel; a no-op when nothing changed.
    fn set_frame(&mut self, frame: PanelFrame, cx: &mut App) {
        if frame == self.frame {
            return;
        }
        self.frame = frame;
        let _ = cx.update_window(self.handle, |_, window, cx| {
            #[cfg(target_os = "macos")]
            crate::macos::floating_panel::set_frame(
                window,
                frame.origin,
                frame.size,
                cx.foreground_executor(),
            );
            #[cfg(not(target_os = "macos"))]
            {
                let _ = cx;
                window.resize(frame.size);
            }
        });
    }

    fn close(self, cx: &mut App) {
        let _ = cx.update_window(self.handle, |_, window, _| window.remove_window());
    }
}

struct PanelView {
    render: RenderFn,
    on_empty: OnEmpty,
    frames: u32,
    /// Frames painted after AppKit reported the window on screen. Only those
    /// reach a full-size drawable; earlier ones land in GPUI's placeholder
    /// surface for occluded windows and never show.
    settled_frames: u32,
    revealed: bool,
}

/// On-screen frames painted before the panel is shown.
const SETTLED_FRAMES: u32 = 1;
/// Give up waiting for the occlusion state after this many frames so a panel
/// AppKit never reports (for instance one placed off screen) still appears.
const REVEAL_DEADLINE_FRAMES: u32 = 120;

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let Some(content) = (self.render)(window, cx) else {
            let on_empty = self.on_empty.clone();
            App::defer(cx, move |cx| on_empty(cx));
            return div().opacity(0.0);
        };
        self.frames += 1;
        #[cfg(target_os = "macos")]
        let on_screen = crate::macos::floating_panel::is_on_screen(window);
        #[cfg(not(target_os = "macos"))]
        let on_screen = true;
        if on_screen {
            self.settled_frames += 1;
        }
        if !self.revealed {
            if self.settled_frames >= SETTLED_FRAMES || self.frames >= REVEAL_DEADLINE_FRAMES {
                // This frame is the first one that can reach the screen. Paint
                // it with the content visible and let AppKit show the blur
                // once it is presented, so both arrive together.
                self.revealed = true;
                window.on_next_frame(move |window, _| {
                    #[cfg(target_os = "macos")]
                    crate::macos::floating_panel::reveal(window);
                    #[cfg(not(target_os = "macos"))]
                    let _ = window;
                });
            } else {
                window.request_animation_frame();
            }
        }
        // Content stays invisible while frames still land in the off-screen
        // placeholder drawable, so the first visible frame is a complete one.
        div()
            .opacity(if self.revealed { 1.0 } else { 0.0 })
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scroll container must measure at its rows, not at its `max_h`.
    #[cfg(target_os = "macos")]
    #[test]
    fn scroll_containers_measure_at_their_content() {
        use std::cell::Cell;
        struct Probe(Rc<Cell<Option<Pixels>>>);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let out = self.0.clone();
                gpui::canvas(
                    move |_, window, cx| {
                        let mut probe = div()
                            .w(px(200.0))
                            .child(
                                div()
                                    .id("probe-scroll")
                                    .max_h(px(600.0))
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .child(div().h(px(100.0)))
                                    .child(div().h(px(50.0))),
                            )
                            .into_any_element();
                        out.set(Some(
                            measure(&mut probe, 200.0, px(700.0), window, cx).height,
                        ));
                    },
                    |_, _, _, _| {},
                )
                .size_full()
            }
        }
        let platform = gpui_platform::current_platform(true);
        let mut cx = gpui::HeadlessAppContext::with_platform(
            platform.text_system(),
            std::sync::Arc::new(diri_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        let out = Rc::new(Cell::new(None));
        let probe = out.clone();
        let window = cx
            .open_window(size(px(400.0), px(700.0)), move |_, cx| {
                cx.new(|_| Probe(probe))
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(out.get().unwrap(), px(150.0));
    }

    fn main() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(100.0), px(50.0)),
            size: size(px(1000.0), px(700.0)),
        }
    }

    #[test]
    fn top_left_anchor_offsets_into_the_main_window() {
        let frame = frame_in(
            main(),
            point(px(12.0), px(40.0)),
            Anchor::TopLeft,
            size(px(276.0), px(300.0)),
            16.0,
            8.0,
        );
        assert_eq!(frame.origin, point(px(112.0), px(90.0)));
        assert_eq!(frame.size, size(px(276.0), px(300.0)));
    }

    #[test]
    fn bottom_left_anchor_opens_upward_like_the_account_menu() {
        let frame = frame_in(
            main(),
            point(px(12.0), px(656.0)),
            Anchor::BottomLeft,
            size(px(276.0), px(300.0)),
            16.0,
            8.0,
        );
        assert_eq!(frame.origin, point(px(112.0), px(406.0)));
    }

    #[test]
    fn frames_snap_inside_the_window_with_the_same_margin_as_anchored() {
        let frame = frame_in(
            main(),
            point(px(900.0), px(600.0)),
            Anchor::TopLeft,
            size(px(276.0), px(300.0)),
            16.0,
            8.0,
        );
        assert_eq!(frame.origin, point(px(100.0 + 716.0), px(50.0 + 392.0)));
        let cramped = frame_in(
            main(),
            point(px(-30.0), px(-30.0)),
            Anchor::TopLeft,
            size(px(276.0), px(300.0)),
            16.0,
            8.0,
        );
        assert_eq!(cramped.origin, point(px(108.0), px(58.0)));
    }
}
