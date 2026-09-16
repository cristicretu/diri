//! Find is a presentation overlay: it never participates in terminal sizing.
//!
//! Measure the real bar (including the alternate-screen hint), then place it
//! after the terminal has prepainted its current highlight. No previous-frame
//! font metrics, assumed row heights, or engine geometry requests are needed.

use diri_term::element::TerminalElement;
use gpui::{
    AnyElement, AvailableSpace, Bounds, Pixels, Point, Size, canvas, div, point, prelude::*, px,
    size,
};

const EDGE: f32 = 6.0;
const RIGHT: f32 = 16.0;
const WIDTH: f32 = 360.0;
const CLEARANCE: f32 = 8.0;

pub(super) fn render(terminal: TerminalElement, content: AnyElement) -> AnyElement {
    canvas(
        move |bounds, window, cx| {
            let width = px(WIDTH).min((bounds.size.width - px(RIGHT + EDGE)).max(px(0.0)));
            let mut bar = div().w(width).child(content).into_any_element();
            let measured = bar.layout_as_root(
                size(AvailableSpace::Definite(width), AvailableSpace::MinContent),
                window,
                cx,
            );
            let origin = placement(bounds, measured, terminal.current_find_match_bounds());
            bar.prepaint_at(origin, window, cx);
            bar
        },
        |_, mut bar, window, cx| bar.paint(window, cx),
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// Prefer the top-right anchor, then below the active match. If there is too
/// little vertical space, try above it and then the left corner. A viewport
/// too small for either object retains reachable controls at the normal anchor.
/// Inactive matches never move the bar. Clearance makes subpixel edges stable.
fn placement(
    viewport: Bounds<Pixels>,
    bar_size: Size<Pixels>,
    active: Option<Bounds<Pixels>>,
) -> Point<Pixels> {
    let top = viewport.top() + px(EDGE);
    let left = viewport.left() + px(EDGE);
    let right = (viewport.right() - px(RIGHT) - bar_size.width).max(left);
    let anchor = point(right, top);
    let Some(active) = active.filter(|rect| rect.intersects(&viewport)) else {
        return anchor;
    };
    let avoid = Bounds::from_corners(
        active.origin - point(px(CLEARANCE), px(CLEARANCE)),
        active.bottom_right() + point(px(CLEARANCE), px(CLEARANCE)),
    );
    if !Bounds::new(anchor, bar_size).intersects(&avoid) {
        return anchor;
    }
    for candidate in [
        point(right, avoid.bottom()),
        point(right, avoid.top() - bar_size.height),
        point(left, top),
    ] {
        let rect = Bounds::new(candidate, bar_size);
        if rect.top() >= top
            && rect.bottom() <= viewport.bottom() - px(EDGE)
            && rect.right() <= viewport.right()
            && !rect.intersects(&avoid)
        {
            return candidate;
        }
    }
    anchor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    #[test]
    fn active_match_is_uncovered_and_scrolling_restores_anchor() {
        let viewport = rect(120.0, 40.0, 800.0, 500.0);
        let bar = size(px(360.0), px(38.0));
        let normal = placement(viewport, bar, None);
        assert_eq!(normal, point(px(544.0), px(46.0)));
        let active = rect(800.0, 50.0, 40.0, 18.0);
        let moved = placement(viewport, bar, Some(active));
        assert_eq!(moved, point(px(544.0), px(76.0)));
        assert!(!Bounds::new(moved, bar).intersects(&active));
        assert_eq!(
            placement(viewport, bar, Some(rect(800.0, 140.0, 40.0, 18.0))),
            normal
        );
        assert_eq!(
            placement(viewport, bar, Some(rect(140.0, 50.0, 40.0, 18.0))),
            normal
        );
        assert_eq!(
            placement(viewport, bar, Some(rect(800.0, 0.0, 40.0, 18.0))),
            normal
        );
    }

    #[test]
    fn measured_tall_bar_and_narrow_viewport_stay_inside() {
        let viewport = rect(30.0, 50.0, 280.0, 300.0);
        let bar = size(px(258.0), px(64.0));
        let active = rect(160.0, 92.0, 70.0, 26.0);
        let origin = placement(viewport, bar, Some(active));
        assert_eq!(origin, point(px(36.0), px(126.0)));
        assert!(Bounds::new(origin, bar).right() <= viewport.right());
        assert!(Bounds::new(origin, bar).bottom() <= viewport.bottom());
    }

    #[test]
    fn short_viewport_uses_left_corner_when_below_does_not_fit() {
        let viewport = rect(0.0, 0.0, 800.0, 70.0);
        let bar = size(px(360.0), px(38.0));
        let active = rect(700.0, 12.0, 40.0, 18.0);
        assert_eq!(
            placement(viewport, bar, Some(active)),
            point(px(6.0), px(6.0))
        );
    }

    #[test]
    fn impossible_fit_is_deterministic_and_keeps_top_controls_reachable() {
        let viewport = rect(0.0, 0.0, 280.0, 50.0);
        let bar = size(px(258.0), px(38.0));
        let active = rect(0.0, 0.0, 280.0, 50.0);
        let anchor = placement(viewport, bar, None);
        for _ in 0..10 {
            assert_eq!(placement(viewport, bar, Some(active)), anchor);
        }
    }
}
