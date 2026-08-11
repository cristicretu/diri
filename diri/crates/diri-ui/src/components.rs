use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, BoxShadow, FontWeight, IntoElement, RenderOnce, Rgba,
    SharedString, TextRun, Transformation, Window, div, ease_out_quint, font, percentage, point,
    prelude::*, px, svg,
};

use crate::{Fill, IconName, Radius, SemanticColors, rgba_f32};

/// Shared, platform-independent activity mark for bounded asynchronous work.
/// Repeating GPUI animations automatically become static when Reduce Motion
/// is enabled, so callers do not need their own timer or accessibility path.
#[derive(IntoElement)]
pub struct LoadingIndicator {
    id: SharedString,
    size: f32,
    color: Rgba,
}

impl LoadingIndicator {
    pub fn new(id: impl Into<SharedString>, size: f32, color: Rgba) -> Self {
        Self {
            id: id.into(),
            size,
            color,
        }
    }
}

impl RenderOnce for LoadingIndicator {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        svg()
            .path(IconName::Refresh.asset_path())
            .flex_none()
            .size(px(self.size))
            .text_color(self.color)
            .with_animation(
                self.id,
                Animation::new(Duration::from_millis(850)).repeat(),
                |icon, delta| icon.with_transformation(Transformation::rotate(percentage(delta))),
            )
    }
}

/// Single-line text that stays ellipsized until an actual overflow is hovered,
/// then reveals the complete value with a bounded horizontal marquee.
///
/// The caller supplies the width available to the label because flex siblings
/// (badges, shortcuts, controls) own that layout knowledge. Text measurement
/// itself is exact and uses GPUI's shaping cache. At most the hovered label
/// schedules animation frames; short labels and Reduce Motion remain static.
#[derive(IntoElement)]
pub struct HoverMarquee {
    id: SharedString,
    text: SharedString,
    active: bool,
    available_width: f32,
    font_size: f32,
    font_weight: FontWeight,
    color: Rgba,
}

impl HoverMarquee {
    pub fn new(
        id: impl Into<SharedString>,
        text: impl Into<SharedString>,
        active: bool,
        available_width: f32,
        font_size: f32,
        color: Rgba,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            active,
            available_width: available_width.max(1.0),
            font_size,
            font_weight: FontWeight::NORMAL,
            color,
        }
    }

    pub const fn font_weight(mut self, weight: FontWeight) -> Self {
        self.font_weight = weight;
        self
    }
}

impl RenderOnce for HoverMarquee {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut text_font = font(".SystemUIFont");
        text_font.weight = self.font_weight;
        let run = TextRun {
            len: self.text.len(),
            font: text_font,
            color: self.color.into(),
            ..TextRun::default()
        };
        let text_width = f32::from(
            window
                .text_system()
                .shape_line(self.text.clone(), px(self.font_size), &[run], None)
                .width(),
        );
        let base = div()
            .min_w(px(0.0))
            .flex_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(self.font_size))
            .font_weight(self.font_weight)
            .text_color(self.color);
        if !self.active || cx.reduce_motion() || text_width <= self.available_width {
            return base.text_ellipsis().child(self.text).into_any_element();
        }

        const GAP: f32 = 24.0;
        const PIXELS_PER_SECOND: f32 = 42.0;
        const LEADING_PAUSE_SECONDS: f32 = 0.8;
        const TRAILING_PAUSE_SECONDS: f32 = 0.7;
        let distance = text_width + GAP;
        let travel_seconds = distance / PIXELS_PER_SECOND;
        let total_seconds = LEADING_PAUSE_SECONDS + travel_seconds + TRAILING_PAUSE_SECONDS;
        let leading_end = LEADING_PAUSE_SECONDS / total_seconds;
        let trailing_start = (LEADING_PAUSE_SECONDS + travel_seconds) / total_seconds;
        let animation =
            Animation::new(Duration::from_secs_f32(total_seconds.clamp(2.0, 16.0))).repeat();
        let first = self.text.clone();
        let second = self.text;
        let track = div()
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(GAP))
            .child(div().flex_none().whitespace_nowrap().child(first))
            .child(div().flex_none().whitespace_nowrap().child(second))
            .with_animation(self.id, animation, move |track, delta| {
                track.left(px(
                    -distance * marquee_progress(delta, leading_end, trailing_start)
                ))
            });
        base.flex().items_center().child(track).into_any_element()
    }
}

fn marquee_progress(delta: f32, leading_end: f32, trailing_start: f32) -> f32 {
    if delta <= leading_end {
        0.0
    } else if delta >= trailing_start {
        1.0
    } else {
        (delta - leading_end) / (trailing_start - leading_end)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowFill {
    #[default]
    Clear,
    Hover,
    MultiSelected,
    Selected,
}

impl RowFill {
    pub fn color(self, colors: SemanticColors) -> Rgba {
        match self {
            Self::Clear => colors.primary.alpha(0.0),
            Self::Hover => colors.primary.alpha(Fill::HOVER_OPACITY),
            Self::MultiSelected => colors.primary.alpha(Fill::MULTI_SELECTED_OPACITY),
            Self::Selected => colors.primary.alpha(Fill::SELECTED_OPACITY),
        }
    }
}

/// Shared panel recipe for palettes, popovers, and find surfaces.
#[derive(IntoElement)]
pub struct FloatingSurface {
    colors: SemanticColors,
    child: AnyElement,
}

impl FloatingSurface {
    pub fn new(colors: SemanticColors, child: impl IntoElement) -> Self {
        Self {
            colors,
            child: child.into_any_element(),
        }
    }
}

impl RenderOnce for FloatingSurface {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = self.colors;
        div()
            .relative()
            .rounded(px(Radius::PANEL))
            // Floating chrome keeps the sidebar hue but uses a denser material
            // so live terminal content never competes with labels or controls.
            .bg(colors.floating_surface())
            .border_1()
            .border_color(colors.floating_stroke())
            .shadow(vec![
                BoxShadow {
                    color: rgba_f32(0.0, 0.0, 0.0, 0.32).into(),
                    offset: point(px(0.0), px(14.0)),
                    blur_radius: px(32.0),
                    spread_radius: px(0.0),
                    inset: false,
                },
                BoxShadow {
                    color: colors.primary.alpha(0.035).into(),
                    offset: point(px(0.0), px(1.0)),
                    blur_radius: px(0.0),
                    spread_radius: px(0.0),
                    inset: true,
                },
            ])
            .child(self.child)
            .with_animation(
                "floating-surface-entry",
                Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                |surface, delta| surface.opacity(0.76 + 0.24 * delta),
            )
    }
}

/// A one-point divider using the foreground color at six percent opacity.
#[derive(IntoElement)]
pub struct HairlineDivider {
    colors: SemanticColors,
    vertical: bool,
}

impl HairlineDivider {
    pub const fn horizontal(colors: SemanticColors) -> Self {
        Self {
            colors,
            vertical: false,
        }
    }

    pub const fn vertical(colors: SemanticColors) -> Self {
        Self {
            colors,
            vertical: true,
        }
    }
}

impl RenderOnce for HairlineDivider {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .flex_none()
            .bg(self.colors.primary.alpha(0.06))
            .when(self.vertical, |element| element.w(px(1.0)).h_full())
            .when(!self.vertical, |element| element.h(px(1.0)).w_full())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_fill_scale_is_shared() {
        let colors = SemanticColors::dark();
        assert_eq!(RowFill::Hover.color(colors).a, 0.06);
        assert_eq!(RowFill::MultiSelected.color(colors).a, 0.08);
        assert_eq!(RowFill::Selected.color(colors).a, 0.10);
        assert_eq!(RowFill::Clear.color(colors).a, 0.0);
    }

    #[test]
    fn marquee_pauses_at_both_ends_and_moves_linearly_between_them() {
        assert_eq!(marquee_progress(0.10, 0.20, 0.80), 0.0);
        assert!((marquee_progress(0.50, 0.20, 0.80) - 0.5).abs() < f32::EPSILON);
        assert_eq!(marquee_progress(0.90, 0.20, 0.80), 1.0);
    }
}
