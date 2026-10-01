//! The window caption on platforms that do not draw it into diri's toolbar.
//!
//! On macOS the traffic lights float over the top-left corner of diri's own
//! toolbar. Windows has no such hybrid: its native caption is a separate
//! strip above the app. So on Windows the window opens with a transparent
//! titlebar, which makes GPUI hide that strip and keep only the resize frame.
//! Diri then draws minimize, maximize and close into the top-right corner of
//! whichever toolbar reaches it, the way the traffic lights sit in the
//! top-left one, and every toolbar in the title row marks itself as the
//! caption's drag area.
//!
//! Both are `WindowControlArea`s, so Windows still drives them: dragging and
//! double-clicking the toolbar, Aero Snap, the system menu on right-click and
//! the Snap Layouts flyout on hovering maximize all behave natively. A control
//! area only claims the pointer where its own hitbox is the frontmost one
//! (`vendor/gpui/DIRI_PATCHES.md`), so toolbar buttons drawn over a drag area
//! keep working without occluding it.

use diri_ui::{Fill, IconName, Metrics, SemanticColors};
use gpui::{AnyElement, InteractiveElement, Rgba, Window, WindowControlArea, div, prelude::*, px, svg};

/// Windows 11's caption buttons are 46 px wide whatever the title height.
pub(crate) const CAPTION_BUTTON_WIDTH: f32 = 46.0;

/// The close button's hover fill on Windows 11, in both light and dark mode.
const CLOSE_HOVER: Rgba = Rgba {
    r: 196.0 / 255.0,
    g: 43.0 / 255.0,
    b: 28.0 / 255.0,
    a: 1.0,
};

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Lays the window out as Windows does from any platform, so layout tests and
/// screenshot fixtures can exercise the caption on a Mac.
#[cfg(test)]
pub(crate) fn force_caption_buttons(forced: bool) {
    FORCED.with(|cell| cell.set(forced));
}

/// Whether diri draws the window's caption buttons itself.
pub(crate) fn draws_caption_buttons() -> bool {
    #[cfg(test)]
    if FORCED.with(std::cell::Cell::get) {
        return true;
    }
    cfg!(windows)
}

/// Width that the toolbar reaching the window's top-right corner leaves free
/// for the caption buttons.
pub(crate) fn caption_lane() -> f32 {
    if draws_caption_buttons() {
        3.0 * CAPTION_BUTTON_WIDTH
    } else {
        0.0
    }
}

/// Windows places the 16 px window icon about 16 px from the left edge.
/// Diri centres it on the sidebar's row glyph column instead (the project
/// chevron box spans 18–36 pt), so the icon heads that column.
const WINDOW_ICON_SIZE: f32 = 16.0;
const WINDOW_ICON_INSET: f32 = 27.0 - WINDOW_ICON_SIZE / 2.0;
const WINDOW_ICON_LANE: f32 = WINDOW_ICON_INSET + WINDOW_ICON_SIZE + 4.0;

/// Whether macOS draws its traffic lights over the window's top-left corner.
pub(crate) fn traffic_lights_visible() -> bool {
    cfg!(target_os = "macos") && !draws_caption_buttons()
}

/// Width that a vertical-tabs toolbar reaching the window's top-left corner
/// leaves free: macOS's traffic lights, or the Windows window icon.
pub(crate) fn leading_lane() -> f32 {
    if draws_caption_buttons() {
        WINDOW_ICON_LANE
    } else if traffic_lights_visible() {
        Metrics::TOOLBAR_TRAFFIC_LIGHT_LANE
    } else {
        0.0
    }
}

static WINDOW_ICON: std::sync::LazyLock<std::sync::Arc<gpui::Image>> =
    std::sync::LazyLock::new(|| {
        // `assets/icon.png` at 64 px, the same art `build.rs` packs into
        // `diri.ico` for the taskbar.
        std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            include_bytes!("../../diri-ui/assets/brand/window-icon.png").to_vec(),
        ))
    });

/// The app icon in the top-left corner, where Windows 11 apps with their own
/// title bar put it (vertical tabs only: with horizontal tabs the strip is
/// the title bar, the tab pattern Microsoft documents, and has none). A click
/// or right-click opens the window's system menu. Windows' older
/// double-click-to-close is left out: it fights the menu the first click
/// opens.
pub(crate) fn window_icon(window: &Window) -> Option<AnyElement> {
    if !draws_caption_buttons() || window.is_fullscreen() {
        return None;
    }
    let hit = 28.0;
    let left = WINDOW_ICON_INSET - (hit - WINDOW_ICON_SIZE) / 2.0;
    let top = (Metrics::TITLE_BAR - hit) / 2.0;
    // The menu drops from the icon's bottom-left, as the native one does.
    let anchor = gpui::point(px(WINDOW_ICON_INSET), px(top + hit));
    let open_menu = move |window: &mut Window, cx: &mut gpui::App| {
        // Not from inside this input callback: the menu runs a modal loop.
        window
            .spawn(cx, async move |cx| {
                let _ = cx.update(|window, _| show_system_menu(window, anchor));
            })
            .detach();
        cx.stop_propagation();
    };
    Some(
        div()
            .id("window-icon")
            .debug_selector(|| "window-icon".into())
            .role(gpui::Role::Button)
            .aria_label("Window menu")
            .absolute()
            .left(px(left))
            .top(px(top))
            .size(px(hit))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                open_menu(window, cx)
            })
            .on_mouse_down(gpui::MouseButton::Right, move |_, window, cx| {
                open_menu(window, cx)
            })
            .child(
                gpui::img(WINDOW_ICON.clone())
                    .size(px(WINDOW_ICON_SIZE))
                    .flex_none(),
            )
            .into_any_element(),
    )
}

/// The window's system menu (Restore, Move, Size, Minimize, Maximize,
/// Close) at `at` in window coordinates. GPUI's `show_window_menu` does
/// nothing on Windows, so this asks Win32 directly.
#[cfg(windows)]
fn show_system_menu(window: &Window, at: gpui::Point<gpui::Pixels>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnableMenuItem, GetSystemMenu, IsZoomed, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED,
        PostMessageW, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE, SC_RESTORE, SC_SIZE,
        SetMenuDefaultItem, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN,
        TrackPopupMenu, WM_SYSCOMMAND,
    };
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
    let scale = window.scale_factor();
    let mut point = POINT {
        x: (f32::from(at.x) * scale).round() as i32,
        y: (f32::from(at.y) * scale).round() as i32,
    };
    // SAFETY: `hwnd` is this live window's handle, read on its UI thread;
    // the menu belongs to the window and is not destroyed here.
    unsafe {
        let _ = ClientToScreen(hwnd, &mut point);
        let menu = GetSystemMenu(hwnd, false);
        if menu.is_invalid() {
            return;
        }
        // DefWindowProc only updates these for its own caption, which this
        // window does not have.
        let maximized = IsZoomed(hwnd).as_bool();
        for (command, enabled) in [
            (SC_RESTORE, maximized),
            (SC_MOVE, !maximized),
            (SC_SIZE, !maximized),
            (SC_MINIMIZE, true),
            (SC_MAXIMIZE, !maximized),
            (SC_CLOSE, true),
        ] {
            let state = if enabled { MF_ENABLED } else { MF_GRAYED };
            let _ = EnableMenuItem(menu, command, MF_BYCOMMAND | state);
        }
        let _ = SetMenuDefaultItem(menu, SC_CLOSE, 0);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        if command.0 != 0 {
            let _ = PostMessageW(
                Some(hwnd),
                WM_SYSCOMMAND,
                WPARAM(command.0 as usize),
                LPARAM(0),
            );
        }
    }
}

#[cfg(not(windows))]
fn show_system_menu(_: &Window, _: gpui::Point<gpui::Pixels>) {}

/// How much of the caption lane a toolbar must leave free at its trailing
/// edge, given where it starts and how far its right edge stops short of the
/// window's. It grows continuously as a neighbour such as the inspector slides
/// away, so trailing actions never jump.
pub(crate) fn caption_inset(top: f32, right_gap: f32) -> f32 {
    if top > 0.5 {
        0.0
    } else {
        (caption_lane() - right_gap.max(0.0)).max(0.0)
    }
}

/// How opaque the title row's controls and text are. Windows dims an inactive
/// window's whole title bar ("all title bar elements should be
/// semi-transparent when the window is inactive"); toolbar backgrounds keep
/// their color, only what sits on them fades.
pub(crate) fn title_row_opacity(window: &Window) -> f32 {
    if draws_caption_buttons() && !window.is_window_active() {
        INACTIVE_TITLE_ROW_OPACITY
    } else {
        1.0
    }
}

const INACTIVE_TITLE_ROW_OPACITY: f32 = 0.5;

pub(crate) trait TitlebarDragArea: InteractiveElement + Sized {
    /// Marks this title-row toolbar as somewhere the window can be dragged
    /// from. Its buttons stay buttons.
    fn titlebar_drag_area(self) -> Self {
        if draws_caption_buttons() {
            self.window_control_area(WindowControlArea::Drag)
        } else {
            self
        }
    }
}

impl<E: InteractiveElement> TitlebarDragArea for E {}

/// Minimize, maximize or restore, and close, laid out for the top-right
/// corner of the window and as tall as the title row.
pub(crate) fn caption_buttons(window: &Window, colors: SemanticColors) -> Option<AnyElement> {
    if !draws_caption_buttons() || window.is_fullscreen() {
        return None;
    }
    // Windows dims an inactive window's caption glyphs; the toolbar around
    // them stays as it is.
    let ink = if window.is_window_active() {
        colors.primary
    } else {
        colors.tertiary
    };
    let maximized = window.is_maximized();
    let button = |id: &'static str, label: &'static str, icon: IconName, area: WindowControlArea| {
        let close = area == WindowControlArea::Close;
        div()
            .id(id)
            .debug_selector(move || id.into())
            .role(gpui::Role::Button)
            .aria_label(label)
            .w(px(CAPTION_BUTTON_WIDTH))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .group(id)
            .window_control_area(area)
            .map(|button| {
                if close {
                    button
                        .hover(|button| button.bg(CLOSE_HOVER))
                        .active(|button| button.bg(CLOSE_HOVER.alpha(0.9)))
                } else {
                    button
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        .active(move |button| button.bg(colors.primary.alpha(0.04)))
                }
            })
            .child(
                svg()
                    .path(icon.asset_path())
                    .flex_none()
                    .size(px(10.0))
                    .text_color(ink)
                    // White over the red fill, as Windows draws it. Group
                    // hover gives the glyph a hitbox of its own in front of
                    // the button's, so it reports the button's area too.
                    .when(close, |glyph| {
                        glyph
                            .group_hover(id, |glyph| glyph.text_color(gpui::white()))
                            .window_control_area(area)
                    }),
            )
    };
    Some(
        div()
            .id("window-caption-buttons")
            .debug_selector(|| "window-caption-buttons".into())
            .absolute()
            .top_0()
            .right_0()
            .h(px(Metrics::TITLE_BAR))
            .flex()
            .child(button(
                "window-minimize",
                "Minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
            ))
            .child(if maximized {
                button(
                    "window-restore",
                    "Restore",
                    IconName::WindowRestore,
                    WindowControlArea::Max,
                )
            } else {
                button(
                    "window-maximize",
                    "Maximize",
                    IconName::WindowMaximize,
                    WindowControlArea::Max,
                )
            })
            .child(button(
                "window-close",
                "Close",
                IconName::WindowClose,
                WindowControlArea::Close,
            ))
            .into_any_element(),
    )
}
