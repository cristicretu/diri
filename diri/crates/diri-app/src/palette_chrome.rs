//! Shared chrome for compact floating lists.
use diri_ui::{Radius, SemanticColors};
use gpui::{
    Bounds, Context, IntoElement, Render, UniformListScrollHandle, Window, canvas, div, fill,
    linear_color_stop, linear_gradient, point, prelude::*, px, size,
};

/// Keyboard hints share a footprint and border, so the header and every row
/// end on the same vertical axis regardless of their text or icon contents.
pub(crate) fn keycap(colors: SemanticColors) -> gpui::Div {
    div()
        .flex_none()
        .w(px(28.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .border_1()
        .border_color(colors.floating_stroke())
        .text_size(px(11.0))
        .text_color(colors.secondary)
}

pub(crate) struct PaletteTooltip(pub(crate) String, pub(crate) SemanticColors);

impl Render for PaletteTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(440.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(Radius::ROW))
            .bg(self.1.floating_surface())
            .border_1()
            .border_color(self.1.floating_stroke())
            .text_size(px(11.0))
            .text_color(self.1.primary)
            .child(self.0.clone())
    }
}

/// Paint after the virtual list has laid out: both wheel scrolling and deferred
/// keyboard selection then use this frame's offset. A canvas adds no hitbox, so
/// the fade never intercepts clicks or scrolling at the edges.
pub(crate) fn scroll_fades(
    scroll: UniformListScrollHandle,
    colors: SemanticColors,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let handle = &scroll.0.borrow().base_handle;
            let scrolled = f32::from(handle.offset().y).min(0.0).abs();
            let remaining = (f32::from(handle.max_offset().y) - scrolled).max(0.0);
            for (distance, angle, top) in [(scrolled, 180.0, true), (remaining, 0.0, false)] {
                let strength = (distance / 14.0).min(1.0);
                if strength <= 0.01 {
                    continue;
                }
                let height = px(16.0);
                let origin = if top {
                    bounds.origin
                } else {
                    point(bounds.left(), bounds.bottom() - height)
                };
                let color: gpui::Hsla = colors.floating_surface().alpha(strength).into();
                window.paint_quad(fill(
                    Bounds::new(origin, size(bounds.size.width, height)),
                    linear_gradient(
                        angle,
                        linear_color_stop(color, 0.0),
                        linear_color_stop(color.opacity(0.0), 1.0),
                    ),
                ));
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full()
}
