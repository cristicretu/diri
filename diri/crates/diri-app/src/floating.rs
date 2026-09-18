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

use std::rc::Rc;

use diri_ui::{Glass, Radius, SemanticColors};
use gpui::{
    Anchor, AnyElement, AnyWindowHandle, App, Bounds, BoxShadow, Context, DisplayId, Div, Entity,
    Global, Pixels, Point, Render, Size, WeakEntity, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, div, point, prelude::*, px, size,
};

/// Present once the running app may open panels. Tests and previews never set
/// it, so they keep rendering popovers inside the window they can inspect.
struct FloatingPanels;

impl Global for FloatingPanels {}

pub(crate) fn enable(cx: &mut App) {
    cx.set_global(FloatingPanels);
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

/// One floating surface a view can host in a panel: where the panel lives on
/// the view, whether it should be open, and the pixels it paints.
pub(crate) struct Target<T: 'static> {
    pub radius: f32,
    pub slot: fn(&mut T) -> &mut Option<Panel>,
    pub wanted: fn(&T) -> bool,
    pub content: fn(&mut T, &mut Context<T>) -> Option<AnyElement>,
}

impl<T: 'static> Clone for Target<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Target<T> {}

/// Closes `target`'s panel if it is open.
pub(crate) fn close<T: 'static>(host: &mut T, target: Target<T>, cx: &mut App) {
    if let Some(panel) = (target.slot)(host).take() {
        panel.close(cx);
    }
}

/// A zero-size element that lays `probe` out at `width` during prepaint and
/// then, outside this frame, opens or resizes `target`'s panel to that size
/// at `position`, anchored inside `main_bounds` like `anchored()` would with
/// `margin` to the window edge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn measure_element<T: 'static>(
    host: WeakEntity<T>,
    target: Target<T>,
    mut probe: AnyElement,
    width: f32,
    main_bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    anchor: Anchor,
    margin: f32,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let display = window.display(cx).map(|display| display.id());
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

/// Opens, moves, or closes `target`'s panel to match `frame`. Runs at App
/// level on purpose: opening a window draws its first frame at once, and
/// that frame renders the host, so the host must not be inside an update of
/// its own while the panel comes up.
pub(crate) fn sync<T: 'static>(
    host: &WeakEntity<T>,
    target: Target<T>,
    frame: PanelFrame,
    display: Option<DisplayId>,
    cx: &mut App,
) {
    let Some(strong) = host.upgrade() else {
        return;
    };
    let (open, panel) = strong.update(cx, |this, _| {
        ((target.wanted)(this), (target.slot)(this).take())
    });
    let panel = match (open, panel) {
        (false, Some(panel)) => {
            panel.close(cx);
            None
        }
        (false, None) => None,
        (true, Some(mut panel)) => {
            panel.set_frame(frame, cx);
            Some(panel)
        }
        (true, None) => {
            let source = strong.clone();
            let render = move |_: &mut Window, cx: &mut App| -> AnyElement {
                source.update(cx, |this, cx| {
                    (target.content)(this, cx).unwrap_or_else(|| div().into_any_element())
                })
            };
            Panel::open(&strong, frame, display, render, cx)
        }
    };
    // A dismissal may have landed while the window came up.
    let stale = strong.update(cx, |this, _| {
        if (target.wanted)(this) {
            *(target.slot)(this) = panel;
            None
        } else {
            panel
        }
    });
    if let Some(panel) = stale {
        panel.close(cx);
    }
}

/// Runs `f` against the view's own window. Handlers inside a panel receive
/// the panel's `Window`, which owns none of the view's focus or actions.
pub(crate) fn in_main_window<T: 'static>(
    this: &mut T,
    main: Option<AnyWindowHandle>,
    window: &mut Window,
    cx: &mut Context<T>,
    f: impl FnOnce(&mut T, &mut Window, &mut Context<T>) + 'static,
) {
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

/// Screen placement of a panel, in GPUI's global coordinates (the same space
/// `Window::bounds` reports for the main window).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelFrame {
    pub origin: Point<Pixels>,
    pub size: Size<Pixels>,
    pub radius: f32,
}

/// Resolves where a popover anchored inside `main` lands on screen, mirroring
/// `anchored().snap_to_window_with_margin(8)` so the two hosts agree.
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

type RenderFn = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// An open popup panel. Dropping the value does not close the window; call
/// [`Panel::close`] so the platform window is removed.
pub(crate) struct Panel {
    handle: AnyWindowHandle,
    frame: PanelFrame,
}

impl Panel {
    /// Opens a blurred, non-activating panel at `frame` that paints
    /// `render` and repaints whenever `source` notifies.
    pub(crate) fn open<T: 'static>(
        source: &Entity<T>,
        frame: PanelFrame,
        display: Option<DisplayId>,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
        cx: &mut App,
    ) -> Option<Self> {
        let render: RenderFn = Rc::new(render);
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
                    cx.new(|cx| {
                        cx.observe(&source, |_, _, cx| cx.notify()).detach();
                        PanelView {
                            render,
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
    pub(crate) fn set_frame(&mut self, frame: PanelFrame, cx: &mut App) {
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
            window.resize(frame.size);
        });
    }

    pub(crate) fn close(self, cx: &mut App) {
        let _ = cx.update_window(self.handle, |_, window, _| window.remove_window());
    }
}

struct PanelView {
    render: RenderFn,
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
            .child((self.render)(window, cx))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scroll container must measure at its rows, not at its `max_h`.
    #[cfg(target_os = "macos")]
    #[test]
    fn scroll_containers_measure_at_their_content() {
        use std::cell::Cell;
        struct Probe(Rc<Cell<Option<(Pixels, Pixels, Pixels)>>>);
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let out = self.0.clone();
                gpui::canvas(
                    move |_, window, cx| {
                        let build = || {
                            div()
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
                                .into_any_element()
                        };
                        let definite = measure(&mut build(), 200.0, px(700.0), window, cx).height;
                        let min_content = build()
                            .layout_as_root(
                                size(
                                    gpui::AvailableSpace::Definite(px(200.0)),
                                    gpui::AvailableSpace::MinContent,
                                ),
                                window,
                                cx,
                            )
                            .height;
                        let max_content = build()
                            .layout_as_root(
                                size(
                                    gpui::AvailableSpace::Definite(px(200.0)),
                                    gpui::AvailableSpace::MaxContent,
                                ),
                                window,
                                cx,
                            )
                            .height;
                        out.set(Some((definite, min_content, max_content)));
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
        let (definite, min_content, max_content) = out.get().unwrap();
        eprintln!("definite={definite:?} min_content={min_content:?} max_content={max_content:?}");
        assert_eq!(definite, px(150.0));
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
