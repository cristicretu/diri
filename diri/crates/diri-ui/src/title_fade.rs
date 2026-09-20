//! A single-line title that fades out where it overflows instead of ending
//! in an ellipsis.
//!
//! The fade is alpha on the glyphs themselves, not a gradient painted over
//! them, so it needs no knowledge of what is behind the text: hover, selected
//! and lifted fills, a theme crossfade and the translucent glass material all
//! show through the same way they show through the rest of the title.
//!
//! GPUI has no gradient mask, so the fade zone is painted as device-pixel
//! columns, each clipped by a content mask and drawn at that column's alpha.
//! Only glyphs that reach the zone are painted more than once, and a title
//! that fits takes GPUI's ordinary line painter untouched.
//!
//! The zone is described from the box's leading edge. Nothing in the app is
//! right-to-left today, so the painter maps leading to left; a mirrored
//! painter is the only piece an RTL layout would need to add.
use gpui::{
    App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Pixels, ShapedLine, SharedString, Style, Window,
    point, px, size,
};

/// Full length of the fade: about three characters of row text. Rendered at
/// 16, 20, 24 and 28: 16 still reads as a cut, and every step past 20 costs
/// a legible character without looking any softer.
pub const TITLE_FADE_WIDTH: f32 = 20.0;
/// The fade grows with the overflow until it reaches its full length, so a
/// title that is a pixel too long loses a pixel's worth of ink rather than
/// three characters, and a resize eases the fade in instead of popping it.
const FADE_PER_OVERFLOW: f32 = 3.0;
/// The fade never takes more than this share of a very narrow box.
const MAX_BOX_SHARE: f32 = 0.5;
/// Ink drawn outside a glyph's advance (overhangs, italics) still belongs to
/// the columns it lands in.
const GLYPH_OVERHANG: f32 = 2.0;
/// Width of one alpha column in logical pixels, before device snapping.
const COLUMN_WIDTH: f32 = 1.0;
/// Content masks cover outward to whole device pixels. Columns sit on device
/// pixel edges and are inset by this much so float error cannot make two
/// neighbours cover the same pixel and paint it twice.
const MASK_INSET_DEVICE: f32 = 0.05;

/// The faded span of a title's box, as distances from the box's leading edge
/// (the left edge for left-to-right text).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeZone {
    /// Where the text stops being fully opaque.
    pub start: f32,
    /// Where the text is fully transparent: the trailing edge of the box.
    pub end: f32,
}

impl FadeZone {
    pub fn width(self) -> f32 {
        self.end - self.start
    }
}

/// The fade for `text_width` of text in a box `box_width` wide, or `None`
/// when the text fits and must be painted exactly as plain text.
pub fn fade_zone(text_width: f32, box_width: f32) -> Option<FadeZone> {
    let overflow = text_width - box_width;
    if overflow.is_nan() || overflow <= 0.0 || box_width <= 0.0 {
        return None;
    }
    let width = TITLE_FADE_WIDTH
        .min(overflow * FADE_PER_OVERFLOW)
        .min(box_width * MAX_BOX_SHARE);
    Some(FadeZone {
        start: box_width - width,
        end: box_width,
    })
}

/// Text opacity at `t` of the way through the zone (0 at its start, 1 at the
/// box edge). A straight ramp, as browsers use: rendered next to smoothstep
/// and ease-in curves it kept the most of the last characters readable, and
/// with alpha sampled per column neither end of the zone shows an edge.
pub fn fade_alpha(t: f32) -> f32 {
    1.0 - t.clamp(0.0, 1.0)
}

/// The fade zone cut into device-pixel columns.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Columns {
    /// Device x of the first column's leading edge.
    first: f32,
    /// Device pixels per column.
    step: f32,
    count: usize,
}

impl Columns {
    fn new(zone: FadeZone, origin_x: f32, scale: f32) -> Self {
        let first = ((origin_x + zone.start) * scale).round();
        let last = ((origin_x + zone.end) * scale).round();
        let step = (COLUMN_WIDTH * scale).round().max(1.0);
        Self {
            first,
            step,
            count: ((last - first) / step).ceil().max(0.0) as usize,
        }
    }

    fn edge(self, index: usize) -> f32 {
        self.first + self.step * index as f32
    }

    /// Alpha is sampled at the column's centre, so the first column is
    /// already slightly faded and the last is not quite zero.
    fn alpha(self, index: usize) -> f32 {
        fade_alpha((index as f32 + 0.5) / self.count as f32)
    }

    /// Columns touched by the device-space span `from..to`.
    fn touched(self, from: f32, to: f32) -> std::ops::Range<usize> {
        let start = ((from - self.first) / self.step).floor().max(0.0) as usize;
        let end = ((to - self.first) / self.step).ceil().max(0.0) as usize;
        start.min(self.count)..end.min(self.count)
    }
}

/// A title that fades out at its trailing edge when it does not fit. Use it
/// as the child of a box that has a definite width (`flex_1().min_w(0)`),
/// where `.text_ellipsis().child(title)` would otherwise go. Font, size and
/// color are inherited from the surrounding text style.
pub fn title_fade(text: impl Into<SharedString>) -> TitleFade {
    TitleFade { text: text.into() }
}

pub struct TitleFade {
    text: SharedString,
}

pub struct TitleFadeLayout {
    line: ShapedLine,
    line_height: Pixels,
    color: Hsla,
}

impl IntoElement for TitleFade {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TitleFade {
    type RequestLayoutState = TitleFadeLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        // A title is one line; anything after a line break is not shown.
        let text = match self.text.find('\n') {
            Some(end) => SharedString::from(self.text[..end].to_string()),
            None => self.text.clone(),
        };
        // Shaping is served from the window's line cache while the title,
        // font and size are unchanged, so an idle row costs a lookup.
        let line = window.text_system().shape_line(
            text.clone(),
            font_size,
            &[style.to_run(text.len())],
            None,
        );
        let text_width = line.width.ceil();
        let layout_id =
            window.request_measured_layout(Style::default(), move |known, available, _, _| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(available) => text_width.min(available),
                    _ => text_width,
                });
                size(width, line_height)
            });
        (
            layout_id,
            TitleFadeLayout {
                line,
                line_height,
                color: style.color,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(zone) = fade_zone(f32::from(layout.line.width), f32::from(bounds.size.width))
        else {
            let align = window.text_style().text_align;
            let _ = layout.line.paint(
                bounds.origin,
                layout.line_height,
                align,
                Some(bounds.size.width),
                window,
                cx,
            );
            return;
        };
        window.paint_layer(bounds, |window| {
            paint_faded(bounds, zone, layout, window);
        });
    }
}

fn paint_faded(
    bounds: Bounds<Pixels>,
    zone: FadeZone,
    layout: &TitleFadeLayout,
    window: &mut Window,
) {
    let line = &layout.line;
    let scale = window.scale_factor();
    let columns = Columns::new(zone, f32::from(bounds.left()), scale);
    // Vertically the text is clipped only by whatever already clips the row.
    let top = bounds.top() - layout.line_height;
    let bottom = bounds.bottom() + layout.line_height;
    let mask = |from_device: f32, to_device: f32| {
        Some(ContentMask {
            bounds: Bounds::from_corners(
                point(px((from_device + MASK_INSET_DEVICE) / scale), top),
                point(px((to_device - MASK_INSET_DEVICE) / scale), bottom),
            ),
        })
    };
    let opaque_from = (f32::from(bounds.left()) - f32::from(line.font_size)) * scale;
    let opaque = mask(opaque_from.floor(), columns.first);
    let box_end = columns.edge(columns.count);

    // The same baseline and pen arithmetic as GPUI's line painter, so the
    // opaque part of the title lands on the pixels plain text would.
    let padding_top = (layout.line_height - line.ascent - line.descent) / 2.0;
    let baseline = bounds.top() + padding_top + line.ascent;
    let mut pen_x = bounds.left();
    let mut previous_x = px(0.0);
    let mut glyphs = line
        .runs
        .iter()
        .flat_map(|run| run.glyphs.iter().map(move |glyph| (run.font_id, glyph)))
        .peekable();
    while let Some((font_id, glyph)) = glyphs.next() {
        pen_x += glyph.position.x - previous_x;
        previous_x = glyph.position.x;
        let advance = glyphs
            .peek()
            .map_or(line.width, |(_, next)| next.position.x)
            - glyph.position.x;
        let ink_from = (f32::from(pen_x) - GLYPH_OVERHANG) * scale;
        let ink_to = (f32::from(pen_x + advance) + GLYPH_OVERHANG) * scale;
        if ink_from >= box_end {
            // Left-to-right glyphs only move further past the box from here.
            break;
        }
        let origin = point(pen_x, baseline);
        let paint = |window: &mut Window, alpha: f32| {
            if glyph.is_emoji {
                // Emoji carry their own color; there is no alpha to scale.
                let _ = window.paint_emoji(origin, font_id, glyph.id, line.font_size);
            } else {
                let _ = window.paint_glyph(
                    origin,
                    font_id,
                    glyph.id,
                    line.font_size,
                    layout.color.opacity(alpha),
                );
            }
        };
        if ink_from < columns.first {
            window.with_content_mask(opaque, |window| paint(window, 1.0));
        }
        for column in columns.touched(ink_from, ink_to) {
            let clip = mask(columns.edge(column), columns.edge(column + 1));
            window.with_content_mask(clip, |window| paint(window, columns.alpha(column)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_that_fits_has_no_fade() {
        assert_eq!(fade_zone(40.0, 92.0), None);
        assert_eq!(fade_zone(92.0, 92.0), None);
        assert_eq!(fade_zone(0.0, 92.0), None);
        assert_eq!(fade_zone(10.0, 0.0), None);
        assert_eq!(fade_zone(f32::NAN, 92.0), None);
    }

    #[test]
    fn an_overflowing_title_fades_into_the_trailing_edge() {
        let zone = fade_zone(180.0, 92.0).expect("overflow fades");
        assert_eq!(zone.end, 92.0);
        assert_eq!(zone.width(), TITLE_FADE_WIDTH);
    }

    #[test]
    fn the_fade_grows_with_the_overflow_instead_of_popping_in() {
        let mut previous = 0.0;
        for tenth in 1..=120 {
            let overflow = tenth as f32 / 10.0;
            let width = fade_zone(92.0 + overflow, 92.0).expect("fade").width();
            assert!(width >= previous, "fade shrank at overflow {overflow}");
            assert!(
                width - previous <= FADE_PER_OVERFLOW * 0.1 + 1e-4,
                "fade jumped at overflow {overflow}"
            );
            previous = width;
        }
        assert_eq!(previous, TITLE_FADE_WIDTH);
    }

    #[test]
    fn a_narrow_box_keeps_half_its_text_opaque() {
        let zone = fade_zone(200.0, 30.0).expect("fade");
        assert_eq!(zone.start, 15.0);
        assert_eq!(zone.end, 30.0);
    }

    #[test]
    fn alpha_falls_monotonically_from_opaque_to_clear() {
        assert_eq!(fade_alpha(0.0), 1.0);
        assert_eq!(fade_alpha(1.0), 0.0);
        assert_eq!(fade_alpha(-1.0), 1.0);
        assert_eq!(fade_alpha(2.0), 0.0);
        let mut previous = 1.0;
        for step in 1..=100 {
            let alpha = fade_alpha(step as f32 / 100.0);
            assert!(alpha <= previous);
            previous = alpha;
        }
        assert!((fade_alpha(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn columns_tile_the_zone_on_device_pixels_without_overlap() {
        for scale in [1.0_f32, 2.0, 3.0] {
            for origin in [0.0_f32, 61.0, 61.25, 61.5, 203.3] {
                let zone = fade_zone(180.0, 92.0).expect("fade");
                let columns = Columns::new(zone, origin, scale);
                assert!(columns.count > 0);
                assert_eq!(columns.first.fract(), 0.0);
                assert_eq!(columns.step.fract(), 0.0);
                let end = ((origin + zone.end) * scale).round();
                assert!(columns.edge(columns.count) >= end);
                assert!(columns.edge(columns.count) - end < columns.step);
                let mut previous = 1.0;
                for index in 0..columns.count {
                    let alpha = columns.alpha(index);
                    assert!(alpha < previous && alpha > 0.0);
                    previous = alpha;
                }
            }
        }
    }

    #[test]
    fn a_glyph_is_painted_only_into_the_columns_it_reaches() {
        let columns = Columns {
            first: 100.0,
            step: 2.0,
            count: 24,
        };
        assert_eq!(columns.touched(0.0, 90.0), 0..0);
        assert_eq!(columns.touched(90.0, 104.5), 0..3);
        assert_eq!(columns.touched(110.0, 120.0), 5..10);
        assert_eq!(columns.touched(140.0, 400.0), 20..24);
        assert_eq!(columns.touched(148.0, 400.0), 24..24);
    }
}
