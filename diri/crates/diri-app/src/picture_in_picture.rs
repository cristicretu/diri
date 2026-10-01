//! Picture in Picture: one session's live terminal in a small window that
//! floats above other apps, so an agent can be watched from a browser or an
//! editor.
//!
//! The window is view-only. A session has exactly one controller lease, and
//! a view that takes it also sizes the PTY: a 480-point PiP typing into a
//! session would reflow the agent's TUI to a few dozen columns in the main
//! window too, and every hand-back would reflow it again. Taking input would
//! also mean making a non-activating panel key, which is the path that
//! raises "active in another view" on the first stray click. So the PiP
//! reads the session through the same receive-only preview connection the
//! tab peek uses ([`LivePreview`]), which never mounts a controller, never
//! resizes, and never sends a byte; the grid is scaled to fit instead. A
//! click on the terminal brings the main window forward on that session,
//! where typing already works.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_proto::{SessionId, SessionRecord};
use diri_ui::{AgentLogo, Fill, Icon, IconName, Motion, Radius, SemanticColors, Typo};
use gpui::{
    AnyWindowHandle, App, Bounds, Context, DisplayId, Global, MouseButton, Pixels, Render, Size,
    Subscription, Task, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, div, point, prelude::*, px, size,
};

use crate::AppServices;
use crate::store::{PipCorner, PipPlacement};
use crate::tab_preview::{LivePreview, PreviewState};

/// Default size: large enough to read an agent's last few lines, small
/// enough to sit in a corner of a laptop screen.
pub(crate) const DEFAULT_WIDTH: f64 = 480.0;
pub(crate) const DEFAULT_HEIGHT: f64 = 300.0;
/// Below this the scaled grid stops being legible.
pub(crate) const MIN_WIDTH: f64 = 280.0;
pub(crate) const MIN_HEIGHT: f64 = 176.0;
/// Gap between the panel and the screen edges it snaps to.
pub(crate) const EDGE_MARGIN: f64 = 16.0;
const RADIUS: f32 = Radius::PANEL;
const STRIP_HEIGHT: f32 = 30.0;
/// Inset of the grid inside the panel.
const GRID_INSET: f32 = 8.0;
/// Size changes from an edge resize are saved once the edge rests this long.
const SIZE_SAVE_DELAY: Duration = Duration::from_millis(600);

/// A rectangle in AppKit's global, y-up coordinates: `y` is the bottom edge.
/// The pure geometry below works in this space; [`flip`] maps GPUI's
/// display-local, y-down bounds into it and back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn center(self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

/// Mirrors a rectangle between y-up and y-down coordinates. Corner geometry
/// is translation invariant, so negating y is enough; the map is its own
/// inverse.
pub(crate) fn flip(rect: Rect) -> Rect {
    Rect {
        y: -(rect.y + rect.height),
        ..rect
    }
}

/// The corner whose quadrant holds the panel's center.
pub(crate) fn nearest_corner(frame: Rect, visible: Rect) -> PipCorner {
    let (x, y) = frame.center();
    let (cx, cy) = visible.center();
    match (x >= cx, y >= cy) {
        (false, true) => PipCorner::TopLeft,
        (true, true) => PipCorner::TopRight,
        (false, false) => PipCorner::BottomLeft,
        (true, false) => PipCorner::BottomRight,
    }
}

/// Bottom-left origin (y-up) that rests a `width` × `height` panel in
/// `corner` of `visible`, [`EDGE_MARGIN`] from both edges. A panel wider than
/// the space between the margins is pinned to the leading margin instead of
/// hanging off the other side.
pub(crate) fn corner_origin(
    corner: PipCorner,
    width: f64,
    height: f64,
    visible: Rect,
) -> (f64, f64) {
    let left = visible.x + EDGE_MARGIN;
    let right = (visible.x + visible.width - EDGE_MARGIN - width).max(left);
    let bottom = visible.y + EDGE_MARGIN;
    let top = (visible.y + visible.height - EDGE_MARGIN - height).max(bottom);
    match corner {
        PipCorner::TopLeft => (left, top),
        PipCorner::TopRight => (right, top),
        PipCorner::BottomLeft => (left, bottom),
        PipCorner::BottomRight => (right, bottom),
    }
}

/// A saved or requested size, kept above the legible minimum and inside the
/// screen's margins.
pub(crate) fn fit_size(width: f64, height: f64, visible: Rect) -> (f64, f64) {
    let valid = |value: f64, fallback: f64| {
        if value.is_finite() && value > 0.0 {
            value
        } else {
            fallback
        }
    };
    let max_width = (visible.width - 2.0 * EDGE_MARGIN).max(MIN_WIDTH);
    let max_height = (visible.height - 2.0 * EDGE_MARGIN).max(MIN_HEIGHT);
    (
        valid(width, DEFAULT_WIDTH).clamp(MIN_WIDTH, max_width),
        valid(height, DEFAULT_HEIGHT).clamp(MIN_HEIGHT, max_height),
    )
}

/// Font size that fits a `cols` × `rows` grid into `width` × `height`. The
/// cell ratios match the tab peek's scaled previews, so both read the same.
pub(crate) fn fit_font_size(width: f32, height: f32, cols: u16, rows: u16) -> f32 {
    let by_width = width / (f32::from(cols.max(1)) * 0.65);
    let by_height = height / (f32::from(rows.max(1)) * 1.5);
    by_width.min(by_height).clamp(1.0, 13.0)
}

/// Position `progress` of the way through the eased corner settle.
pub(crate) fn settle_position(from: (f64, f64), to: (f64, f64), progress: f32) -> (f64, f64) {
    let eased = f64::from(Motion::SNAP.settle(progress));
    (
        from.0 + (to.0 - from.0) * eased,
        from.1 + (to.1 - from.1) * eased,
    )
}

/// Open PiP windows, one per session.
#[derive(Default)]
struct PipWindows(HashMap<SessionId, WindowHandle<PipView>>);

impl Global for PipWindows {}

/// Opens a PiP for `id`, or closes it when one is already open. `main` is the
/// window "return to window" goes back to; `display` the screen to open on.
pub(crate) fn toggle(
    id: SessionId,
    services: Arc<AppServices>,
    main: AnyWindowHandle,
    display: Option<DisplayId>,
    cx: &mut App,
) {
    // Opening a window paints its first frame at once; never do that inside
    // the caller's own view update.
    cx.defer(move |cx| {
        let open = cx
            .try_global::<PipWindows>()
            .and_then(|windows| windows.0.get(&id).copied());
        if let Some(handle) = open {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            if cx.has_global::<PipWindows>() {
                cx.global_mut::<PipWindows>().0.remove(&id);
            }
            return;
        }
        open_window(id, services, main, display, cx);
    });
}

fn open_window(
    id: SessionId,
    services: Arc<AppServices>,
    main: AnyWindowHandle,
    display: Option<DisplayId>,
    cx: &mut App,
) {
    let (placement, material) = {
        let store = services
            .store
            .store
            .read()
            .expect("session store lock poisoned");
        let Some(session) = store.sessions().get(&id) else {
            return;
        };
        if session.is_archived() {
            return;
        }
        (
            store.preferences().picture_in_picture,
            store.preferences().window_material,
        )
    };
    let display = display
        .and_then(|id| cx.find_display(id))
        .or_else(|| cx.primary_display());
    let display_id = display.as_ref().map(|display| display.id());
    // GPUI takes a display-local, y-down frame; flip it into the y-up space
    // the geometry works in, and the result back.
    let visible = display.map_or(
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        },
        |display| {
            let bounds = display.visible_bounds();
            flip(Rect {
                x: f64::from(f32::from(bounds.origin.x)),
                y: f64::from(f32::from(bounds.origin.y)),
                width: f64::from(f32::from(bounds.size.width)),
                height: f64::from(f32::from(bounds.size.height)),
            })
        },
    );
    let placement = placement.unwrap_or(PipPlacement {
        corner: PipCorner::BottomRight,
        width: DEFAULT_WIDTH as f32,
        height: DEFAULT_HEIGHT as f32,
    });
    let (width, height) = fit_size(
        f64::from(placement.width),
        f64::from(placement.height),
        visible,
    );
    let (x, y) = corner_origin(placement.corner, width, height, visible);
    let frame = flip(Rect {
        x,
        y,
        width,
        height,
    });
    let bounds = Bounds {
        origin: point(px(frame.x as f32), px(frame.y as f32)),
        size: size(px(width as f32), px(height as f32)),
    };
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            // Opening never takes focus from the app the user is in.
            focus: false,
            show: true,
            // GPUI's non-activating NSPanel; `pip_panel::prepare` lowers it to
            // floating level and makes it follow Spaces.
            kind: WindowKind::PopUp,
            // The strip moves the window itself so it can snap on release.
            is_movable: false,
            app_owns_titlebar_drag: true,
            is_resizable: true,
            is_minimizable: false,
            window_min_size: Some(size(px(MIN_WIDTH as f32), px(MIN_HEIGHT as f32))),
            window_background: match material {
                crate::store::WindowMaterial::Glass => WindowBackgroundAppearance::Blurred,
                crate::store::WindowMaterial::Opaque => WindowBackgroundAppearance::Opaque,
            },
            display_id,
            ..Default::default()
        },
        {
            let id = id.clone();
            move |window, cx| {
                crate::macos::pip_panel::prepare(window, RADIUS);
                cx.new(|cx| PipView::new(id, services, main, placement.corner, window, cx))
            }
        },
    );
    if let Ok(handle) = handle {
        if !cx.has_global::<PipWindows>() {
            cx.set_global(PipWindows::default());
        }
        cx.global_mut::<PipWindows>().0.insert(id, handle);
    }
}

/// Runs `f` on the main diri window that should show `id`: the one the PiP
/// was opened from, or any other when that one has closed.
fn in_root_window(
    main: AnyWindowHandle,
    cx: &mut App,
    f: impl FnOnce(&mut crate::root::RootView, &mut Window, &mut Context<crate::root::RootView>)
    + 'static,
) {
    let mut candidates = vec![main];
    candidates.extend(cx.windows().into_iter().filter(|handle| *handle != main));
    for handle in candidates {
        let Some(root) = handle
            .update(cx, |view, _, _| {
                view.downcast::<crate::root::RootView>().ok()
            })
            .ok()
            .flatten()
        else {
            continue;
        };
        let _ = handle.update(cx, |_, window, cx| {
            root.update(cx, |root, cx| f(root, window, cx));
        });
        return;
    }
}

/// An eased move of the window to its snapped corner, in AppKit coordinates.
#[derive(Clone, Copy, Debug)]
struct Settle {
    from: (f64, f64),
    to: (f64, f64),
    started: Instant,
}

pub(crate) struct PipView {
    id: SessionId,
    services: Arc<AppServices>,
    main: AnyWindowHandle,
    preview: LivePreview,
    corner: PipCorner,
    hovered: bool,
    /// When `hovered` last changed, for the strip's fade.
    hover_changed: Option<Instant>,
    drag: Option<crate::macos::pip_panel::DragAnchor>,
    settle: Option<Settle>,
    last_size: Size<Pixels>,
    saved_size: Size<Pixels>,
    save_size: Option<Task<()>>,
    /// First-draw bookkeeping shared with the menu panels: frames painted
    /// before AppKit reports the window on screen land in a placeholder
    /// drawable, so content stays hidden until one reaches the screen.
    frames: u32,
    settled_frames: u32,
    revealed: bool,
    _store_changes: Task<()>,
    _bounds: Subscription,
    _release: Subscription,
}

impl PipView {
    fn new(
        id: SessionId,
        services: Arc<AppServices>,
        main: AnyWindowHandle,
        corner: PipCorner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let socket = services.store.client().socket_path().to_path_buf();
        let preview = LivePreview::open(services.tokio.handle(), socket, id.clone(), cx);
        let mut changes = services.store.changes();
        let store_changes = cx.spawn(async move |this, cx| {
            loop {
                match changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this.update(cx, |this, cx| this.store_changed(cx)).is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let bounds = cx.observe_window_bounds(window, |this, window, cx| {
            this.bounds_changed(window, cx);
        });
        let release = cx.on_release({
            let id = id.clone();
            move |_, cx| {
                if cx.has_global::<PipWindows>() {
                    cx.global_mut::<PipWindows>().0.remove(&id);
                }
            }
        });
        let size = window.bounds().size;
        Self {
            id,
            services,
            main,
            preview,
            corner,
            hovered: false,
            hover_changed: None,
            drag: None,
            settle: None,
            last_size: size,
            saved_size: size,
            save_size: None,
            frames: 0,
            settled_frames: 0,
            revealed: false,
            _store_changes: store_changes,
            _bounds: bounds,
            _release: release,
        }
    }

    fn session(&self) -> Option<Arc<SessionRecord>> {
        self.services
            .store
            .store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .get(&self.id)
            .cloned()
    }

    /// Closing or archiving the session closes its PiP; anything else (a new
    /// title, a status change) repaints the strip.
    fn store_changed(&mut self, cx: &mut Context<Self>) {
        if self.session().is_none_or(|session| session.is_archived()) {
            let id = self.id.clone();
            cx.defer(move |cx| {
                let handle = cx
                    .try_global::<PipWindows>()
                    .and_then(|windows| windows.0.get(&id).copied());
                if let Some(handle) = handle {
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                }
            });
            return;
        }
        cx.notify();
    }

    fn bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let size = window.bounds().size;
        if size == self.last_size {
            return;
        }
        self.last_size = size;
        // An edge resize reports every step; save once it rests.
        self.save_size = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SIZE_SAVE_DELAY).await;
            let _ = this.update(cx, |this, _| this.save_placement());
        }));
        cx.notify();
    }

    fn save_placement(&mut self) {
        self.save_size = None;
        let size = self.last_size;
        let placement = PipPlacement {
            corner: self.corner,
            width: f32::from(size.width),
            height: f32::from(size.height),
        };
        self.saved_size = size;
        let _ = self
            .services
            .store
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.picture_in_picture = Some(placement));
    }

    fn set_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.hovered == hovered {
            return;
        }
        self.hovered = hovered;
        self.hover_changed = Some(Instant::now());
        cx.notify();
    }

    fn begin_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settle = None;
        self.drag = crate::macos::pip_panel::begin_drag(window);
        cx.notify();
    }

    fn drag_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(anchor) = self.drag {
            crate::macos::pip_panel::drag_to(window, anchor, cx.foreground_executor());
        }
    }

    /// Drops the panel into the nearest corner of the screen it ended up on.
    fn end_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_none() {
            return;
        }
        let Some((frame, visible)) = crate::macos::pip_panel::frame_and_visible(window) else {
            return;
        };
        self.corner = nearest_corner(frame, visible);
        let to = corner_origin(self.corner, frame.width, frame.height, visible);
        if cx.reduce_motion() {
            crate::macos::pip_panel::set_origin(window, to.0, to.1, cx.foreground_executor());
            self.save_placement();
        } else {
            self.settle = Some(Settle {
                from: (frame.x, frame.y),
                to,
                started: Instant::now(),
            });
        }
        cx.notify();
    }

    /// Advances the corner settle by one frame.
    fn advance_settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settle) = self.settle else {
            return;
        };
        let duration = Motion::SNAP.response;
        let progress = (settle.started.elapsed().as_secs_f32() / duration).min(1.0);
        let (x, y) = settle_position(settle.from, settle.to, progress);
        crate::macos::pip_panel::set_origin(window, x, y, cx.foreground_executor());
        if progress >= 1.0 {
            self.settle = None;
            self.save_placement();
        } else {
            window.request_animation_frame();
        }
    }

    /// 0 (hidden) to 1 (shown), eased over the shared overlay fade.
    fn strip_opacity(&self, window: &mut Window, cx: &App) -> f32 {
        let target = if self.hovered || self.drag.is_some() {
            1.0
        } else {
            0.0
        };
        let Some(changed) = self.hover_changed else {
            return target;
        };
        if cx.reduce_motion() {
            return target;
        }
        let progress = changed.elapsed().as_secs_f32() / Motion::OVERLAY_FADE;
        if progress >= 1.0 {
            return target;
        }
        window.request_animation_frame();
        let eased = diri_ui::motion::settle(progress);
        if target > 0.5 { eased } else { 1.0 - eased }
    }

    fn return_to_window(&mut self, close: bool, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.id.clone();
        let main = self.main;
        cx.defer(move |cx| {
            cx.activate(true);
            in_root_window(main, cx, move |root, window, cx| {
                root.reveal_session(id, window, cx);
            });
        });
        if close {
            window.remove_window();
        }
    }

    fn strip(
        &self,
        session: Option<&SessionRecord>,
        colors: SemanticColors,
        opacity: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (kind, title, state) = session.map_or(
            (
                diri_ui::AgentKind::Generic,
                String::new(),
                diri_ui::StatusState::None,
            ),
            |session| {
                (
                    crate::session_presentation::ui_agent_kind(session.effective_kind()),
                    crate::switcher::display_title(session),
                    crate::session_presentation::status_state(session, false),
                )
            },
        );
        let button = |id: &'static str, icon: IconName, label: &'static str| {
            div()
                .id(id)
                .size(px(22.0))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(move |style| style.bg(Fill::hover(colors, true)))
                .role(gpui::Role::Button)
                .aria_label(label)
                .child(Icon::new(icon, 13.0, colors.secondary))
                // A press on a control is not the start of a drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        div()
            .id("pip-strip")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(STRIP_HEIGHT))
            .pl(px(10.0))
            .pr(px(4.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .opacity(opacity)
            .bg(diri_ui::Glass::panel_fill(colors))
            .border_b_1()
            .border_color(colors.floating_stroke())
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.begin_drag(window, cx)),
            )
            .child(AgentLogo::new(kind, 14.0, colors).badged(false))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.primary)
                    .child(title),
            )
            .child(crate::session_presentation::activity_mark(state, 0, colors))
            .child(
                button("pip-return", IconName::Expand, "Return to window").on_click(
                    cx.listener(|this, _, window, cx| this.return_to_window(true, window, cx)),
                ),
            )
            .child(
                button("pip-close", IconName::Close, "Close").on_click(cx.listener(
                    |_, _, window, _| {
                        window.remove_window();
                    },
                )),
            )
    }
}

impl Render for PipView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames += 1;
        if crate::macos::floating_panel::is_on_screen(window) {
            self.settled_frames += 1;
        }
        if !self.revealed {
            if self.settled_frames >= 1 || self.frames >= 120 {
                self.revealed = true;
                window.on_next_frame(|window, _| crate::macos::floating_panel::reveal(window));
            } else {
                window.request_animation_frame();
            }
        }
        self.advance_settle(window, cx);

        let (colors, theme, font) = {
            let store = self
                .services
                .store
                .store
                .read()
                .expect("session store lock poisoned");
            (
                crate::app_theme::colors_in(&store),
                crate::app_theme::terminal_theme(store.theme_id()),
                // The user's terminal font, as the panes paint it.
                crate::fonts::terminal_font(&store.preferences().terminal_font_family),
            )
        };
        let session = self.session();
        let opacity = self.strip_opacity(window, cx);
        let bounds = window.bounds().size;
        let grid_width = f32::from(bounds.width) - 2.0 * GRID_INSET;
        let grid_height = f32::from(bounds.height) - 2.0 * GRID_INSET;
        let element = &self.preview.element;
        let grid = if element.grid_cols() > 0 {
            element
                .clone()
                .font(font)
                .font_size(px(fit_font_size(
                    grid_width,
                    grid_height,
                    element.grid_cols(),
                    element.grid_rows(),
                )))
                .theme(theme)
                .into_any_element()
        } else {
            let label = match *self.preview.state.borrow() {
                PreviewState::Loading => "Loading…",
                PreviewState::Live | PreviewState::Disconnected => "",
                PreviewState::Unavailable => "Preview unavailable",
            };
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(Typo::ROW.size))
                .text_color(colors.tertiary)
                .child(label)
                .into_any_element()
        };

        div()
            .id("pip")
            .relative()
            .size_full()
            .opacity(if self.revealed { 1.0 } else { 0.0 })
            .rounded(px(RADIUS))
            .overflow_hidden()
            .bg(colors.work_surface())
            .border_1()
            .border_color(colors.floating_stroke())
            .text_color(colors.primary)
            .on_mouse_move(cx.listener(|this, _, window, cx| {
                this.set_hovered(true, cx);
                this.drag_moved(window, cx);
            }))
            .on_mouse_exit(cx.listener(|this, _, _, cx| {
                if this.drag.is_none() {
                    this.set_hovered(false, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.end_drag(window, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.end_drag(window, cx)),
            )
            .child(
                div()
                    .id("pip-grid")
                    .size_full()
                    .p(px(GRID_INSET))
                    .overflow_hidden()
                    .cursor_pointer()
                    // View-only: a click hands the session to the main window,
                    // where typing goes to the controller.
                    .on_click(
                        cx.listener(|this, _, window, cx| this.return_to_window(false, window, cx)),
                    )
                    .child(grid),
            )
            .when(opacity > 0.0, |pip| {
                pip.child(self.strip(session.as_deref(), colors, opacity, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1512×944 laptop screen's visible area, above a 38 pt Dock.
    const VISIBLE: Rect = Rect {
        x: 0.0,
        y: 38.0,
        width: 1512.0,
        height: 906.0,
    };

    fn at(x: f64, y: f64) -> Rect {
        Rect {
            x,
            y,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }

    #[test]
    fn a_panel_snaps_to_the_corner_of_its_quadrant() {
        assert_eq!(nearest_corner(at(40.0, 600.0), VISIBLE), PipCorner::TopLeft);
        assert_eq!(
            nearest_corner(at(1000.0, 600.0), VISIBLE),
            PipCorner::TopRight
        );
        assert_eq!(
            nearest_corner(at(40.0, 60.0), VISIBLE),
            PipCorner::BottomLeft
        );
        assert_eq!(
            nearest_corner(at(1000.0, 60.0), VISIBLE),
            PipCorner::BottomRight
        );
    }

    #[test]
    fn corners_keep_the_margin_from_the_visible_edges() {
        let (w, h) = (DEFAULT_WIDTH, DEFAULT_HEIGHT);
        assert_eq!(
            corner_origin(PipCorner::BottomRight, w, h, VISIBLE),
            (1512.0 - EDGE_MARGIN - w, 38.0 + EDGE_MARGIN)
        );
        assert_eq!(
            corner_origin(PipCorner::TopLeft, w, h, VISIBLE),
            (EDGE_MARGIN, 38.0 + 906.0 - EDGE_MARGIN - h)
        );
    }

    #[test]
    fn a_panel_wider_than_the_screen_pins_to_the_leading_margin() {
        let narrow = Rect {
            width: 300.0,
            ..VISIBLE
        };
        assert_eq!(
            corner_origin(PipCorner::BottomRight, 480.0, 300.0, narrow).0,
            EDGE_MARGIN
        );
    }

    #[test]
    fn flipping_is_its_own_inverse_and_swaps_top_for_bottom() {
        let rect = at(10.0, 20.0);
        assert_eq!(flip(flip(rect)), rect);
        // In y-down space the bottom-right corner is the one with the larger y.
        let down = flip(VISIBLE);
        let (_, y) = corner_origin(PipCorner::BottomRight, 480.0, 300.0, VISIBLE);
        let placed = flip(Rect {
            x: 0.0,
            y,
            width: 480.0,
            height: 300.0,
        });
        assert_eq!(
            placed.y + placed.height,
            down.y + down.height - EDGE_MARGIN,
            "the panel's bottom edge sits one margin above the visible bottom"
        );
    }

    #[test]
    fn saved_sizes_are_clamped_to_legible_and_on_screen() {
        assert_eq!(fit_size(100.0, 50.0, VISIBLE), (MIN_WIDTH, MIN_HEIGHT));
        assert_eq!(
            fit_size(5000.0, 5000.0, VISIBLE),
            (1512.0 - 2.0 * EDGE_MARGIN, 906.0 - 2.0 * EDGE_MARGIN)
        );
        assert_eq!(
            fit_size(f64::NAN, -1.0, VISIBLE),
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        );
    }

    #[test]
    fn the_grid_scales_to_whichever_axis_is_tighter() {
        // 120 columns in 464 points: width bound (~5.9 pt) beats height.
        let size = fit_font_size(464.0, 284.0, 120, 30);
        assert!((size - 464.0 / (120.0 * 0.65)).abs() < 0.001, "{size}");
        // Never larger than the terminal's own default size.
        assert_eq!(fit_font_size(2000.0, 2000.0, 10, 5), 13.0);
    }

    #[test]
    fn the_settle_starts_at_the_release_and_ends_in_the_corner() {
        let from = (100.0, 200.0);
        let to = (1000.0, 54.0);
        assert_eq!(settle_position(from, to, 0.0), from);
        assert_eq!(settle_position(from, to, 1.0), to);
        let (x, _) = settle_position(from, to, 0.5);
        assert!(x > 550.0, "front-loaded like every shared settle: {x}");
    }
}
