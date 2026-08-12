use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, BoxShadow, FontWeight, IntoElement, RenderOnce, Rgba,
    SharedString, TextRun, Transformation, Window, div, ease_out_quint, font, percentage, point,
    prelude::*, px, svg,
};

use crate::{Chip, Fill, IconName, Ink, Radius, SemanticColors, Typo, rgba_f32};

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

/// Compact row chip used by the sidebar (and matched by the menubar AppKit
/// surface via [`Chip`] tokens). One shape for every quiet state label.
#[derive(IntoElement)]
pub struct StateChip {
    label: SharedString,
    tint: Rgba,
    colors: SemanticColors,
}

impl StateChip {
    pub fn new(label: impl Into<SharedString>, tint: Rgba, colors: SemanticColors) -> Self {
        Self {
            label: label.into(),
            tint,
            colors,
        }
    }
}

impl RenderOnce for StateChip {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .flex_none()
            .px(px(Chip::PAD_X))
            .py(px(Chip::PAD_Y))
            .rounded(px(Radius::CHIP))
            .bg(Fill::subtle(self.colors))
            .text_size(px(Chip::font_size()))
            .line_height(px(Chip::LINE_H))
            .font_weight(Typo::META.weight)
            .text_color(self.tint)
            .whitespace_nowrap()
            .child(self.label)
    }
}

/// Same geometry as [`StateChip`], danger tint for blockers that must outrank
/// the rest of the chip lane.
#[derive(IntoElement)]
pub struct AlertChip {
    label: SharedString,
}

impl AlertChip {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl RenderOnce for AlertChip {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .flex_none()
            .px(px(Chip::PAD_X))
            .py(px(Chip::PAD_Y))
            .rounded(px(Radius::CHIP))
            .bg(Ink::DANGER.alpha(0.12))
            .text_size(px(Chip::font_size()))
            .line_height(px(Chip::LINE_H))
            .font_weight(Typo::META.weight)
            .text_color(Ink::DANGER)
            .whitespace_nowrap()
            .child(self.label)
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
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = self.colors;
        let surface = div()
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
            .child(self.child);
        if floating_surface_motion(cx.reduce_motion()) == FloatingSurfaceMotion::Immediate {
            surface.into_any_element()
        } else {
            surface
                .with_animation(
                    // Animation state belongs to this mounted element. The stable
                    // key prevents ordinary parent repaints from restarting it;
                    // closing unmounts the element and drops that state, so a
                    // rapid close/open creates a fresh, fully hit-testable surface
                    // rather than resuming stale partial opacity.
                    "floating-surface-entry",
                    Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                    |surface, delta| surface.opacity(floating_surface_opacity(false, delta)),
                )
                .into_any_element()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatingSurfaceMotion {
    Immediate,
    EntryFade,
}

fn floating_surface_motion(reduce_motion: bool) -> FloatingSurfaceMotion {
    if reduce_motion {
        FloatingSurfaceMotion::Immediate
    } else {
        FloatingSurfaceMotion::EntryFade
    }
}

fn floating_surface_opacity(reduce_motion: bool, progress: f32) -> f32 {
    if reduce_motion {
        1.0
    } else {
        0.76 + 0.24 * progress.clamp(0.0, 1.0)
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

    #[test]
    fn floating_surface_motion_policy_is_immediate_when_reduced() {
        assert_eq!(
            floating_surface_motion(true),
            FloatingSurfaceMotion::Immediate
        );
        assert_eq!(
            floating_surface_motion(false),
            FloatingSurfaceMotion::EntryFade
        );
        // Immediate policy cannot inherit a stale mid-animation opacity from
        // an open/close/open sequence. Normal motion keeps the old endpoints.
        for stale_progress in [-1.0, 0.0, 0.4, 1.0, 2.0] {
            assert_eq!(floating_surface_opacity(true, stale_progress), 1.0);
        }
        assert_eq!(floating_surface_opacity(false, 0.0), 0.76);
        assert_eq!(floating_surface_opacity(false, 1.0), 1.0);
    }
}
