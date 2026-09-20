//! Renders `title_fade` headlessly and checks the pixels it promises: a
//! title that fits is plain text, the opaque part of a long title is plain
//! text, the fade zone follows the alpha ramp, and nothing but glyph ink is
//! ever painted, whatever is behind the row.
#![cfg(target_os = "macos")]
use std::sync::Arc;

use diri_ui::title_fade;
use diri_ui::title_fade::{TITLE_FADE_WIDTH, fade_alpha, fade_zone};
use gpui::{
    AnyElement, AppContext as _, Context, HeadlessAppContext, Hsla, IntoElement, ParentElement,
    Render, Styled, Window, div, linear_color_stop, linear_gradient, px, rgb, size,
};

const BOX_WIDTH: f32 = 92.0;
const INSET: f32 = 8.0;
const LONG: &str = "Polish the left sidebar hierarchy";
const SHORT: &str = "Release 0.8.5";

#[derive(Clone, Copy, PartialEq)]
enum Label {
    Ellipsis,
    /// Plain text clipped by its box, with no truncation at all.
    Clipped,
    Fade,
    /// Only the backdrop.
    Nothing,
}

#[derive(Clone, Copy)]
enum Backdrop {
    Flat,
    /// A stand-in for glass, a hover fill or a theme mid-crossfade: anything
    /// the row cannot describe to the title as one color.
    Gradient,
}

struct Fixture {
    text: &'static str,
    label: Label,
    backdrop: Backdrop,
}

impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let title = div().flex_1().min_w(px(0.0)).overflow_hidden();
        let title: AnyElement = match self.label {
            Label::Ellipsis => title.text_ellipsis().child(self.text).into_any_element(),
            Label::Clipped => title
                .whitespace_nowrap()
                .child(self.text)
                .into_any_element(),
            Label::Fade => title.child(title_fade(self.text)).into_any_element(),
            Label::Nothing => title.into_any_element(),
        };
        let root = div()
            .size_full()
            .font_family(".SystemUIFont")
            .text_size(px(13.0))
            .text_color(rgb(0xe8e8ea))
            .p(px(INSET));
        match self.backdrop {
            Backdrop::Flat => root.bg(rgb(0x121317)),
            Backdrop::Gradient => root.bg(linear_gradient(
                90.0,
                linear_color_stop(Hsla::from(rgb(0x121317)), 0.0),
                linear_color_stop(Hsla::from(rgb(0x5a3d7a)), 1.0),
            )),
        }
        .child(div().w(px(BOX_WIDTH)).h(px(20.0)).flex().child(title))
    }
}

struct Shot {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Shot {
    fn pixel(&self, x: u32, y: u32) -> [i32; 3] {
        let at = ((y * self.width + x) * 4) as usize;
        [
            i32::from(self.rgba[at]),
            i32::from(self.rgba[at + 1]),
            i32::from(self.rgba[at + 2]),
        ]
    }

    /// How far column `x` departs from the same column of `backdrop`.
    fn ink(&self, backdrop: &Shot, x: u32) -> i32 {
        (0..self.height)
            .map(|y| {
                let (ours, theirs) = (self.pixel(x, y), backdrop.pixel(x, y));
                (0..3).map(|c| (ours[c] - theirs[c]).abs()).sum::<i32>()
            })
            .sum()
    }
}

/// `None` where there is no GPU to render with.
fn render(cx: &mut HeadlessAppContext, fixture: Fixture) -> Option<Shot> {
    let window = cx
        .open_window(size(px(BOX_WIDTH + 2.0 * INSET), px(36.0)), |_, cx| {
            cx.new(|_| fixture)
        })
        .ok()?;
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).ok();
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .ok()?;
    cx.run_until_parked();
    let image = image?;
    Some(Shot {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

fn headless() -> HeadlessAppContext {
    let platform = gpui_platform::current_platform(true);
    HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    )
}

macro_rules! render_or_skip {
    ($cx:expr, $text:expr, $label:expr, $backdrop:expr) => {
        match render(
            $cx,
            Fixture {
                text: $text,
                label: $label,
                backdrop: $backdrop,
            },
        ) {
            Some(shot) => shot,
            None => {
                eprintln!("no headless renderer here; skipping");
                return;
            }
        }
    };
}

/// One test, because the macOS platform cannot be created twice in a
/// process and cargo runs a file's tests on parallel threads.
#[test]
fn title_fade_pixels() {
    let mut cx = headless();
    a_title_that_fits_is_pixel_identical_to_plain_text(&mut cx);
    a_long_title_is_plain_text_until_the_fade_and_then_follows_the_ramp(&mut cx);
}

fn a_title_that_fits_is_pixel_identical_to_plain_text(cx: &mut HeadlessAppContext) {
    let plain = render_or_skip!(cx, SHORT, Label::Ellipsis, Backdrop::Flat);
    let faded = render_or_skip!(cx, SHORT, Label::Fade, Backdrop::Flat);
    assert!(plain.rgba == faded.rgba, "a fitting title changed pixels");
    let empty = render_or_skip!(cx, SHORT, Label::Nothing, Backdrop::Flat);
    assert!(plain.rgba != empty.rgba, "the fixture painted no text");
}

fn a_long_title_is_plain_text_until_the_fade_and_then_follows_the_ramp(
    cx: &mut HeadlessAppContext,
) {
    for backdrop in [Backdrop::Flat, Backdrop::Gradient] {
        let empty = render_or_skip!(cx, LONG, Label::Nothing, backdrop);
        let clipped = render_or_skip!(cx, LONG, Label::Clipped, backdrop);
        let faded = render_or_skip!(cx, LONG, Label::Fade, backdrop);
        let scale = faded.width as f32 / (BOX_WIDTH + 2.0 * INSET);
        let zone = fade_zone(BOX_WIDTH * 4.0, BOX_WIDTH).expect("the long title overflows");
        assert_eq!(zone.width(), TITLE_FADE_WIDTH);
        let zone_start = ((INSET + zone.start) * scale).round() as u32;
        let zone_end = ((INSET + zone.end) * scale).round() as u32;

        for x in 0..zone_start {
            for y in 0..faded.height {
                assert_eq!(
                    faded.pixel(x, y),
                    clipped.pixel(x, y),
                    "opaque text at {x},{y}"
                );
            }
        }
        for x in zone_end..faded.width {
            assert_eq!(faded.ink(&empty, x), 0, "ink past the box at column {x}");
        }
        let mut compared = 0;
        for x in zone_start..zone_end {
            // Whatever is behind the text shows through untouched wherever
            // plain text would have left it alone: there is no overlay.
            for y in 0..faded.height {
                if clipped.pixel(x, y) == empty.pixel(x, y) {
                    assert_eq!(faded.pixel(x, y), empty.pixel(x, y), "overlay at {x},{y}");
                }
            }
            let full = clipped.ink(&empty, x);
            if full < 600 {
                continue;
            }
            let t = (x as f32 + 0.5 - zone_start as f32) / (zone_end - zone_start) as f32;
            let kept = faded.ink(&empty, x) as f32 / full as f32;
            assert!(
                (kept - fade_alpha(t)).abs() < 0.08,
                "column {x}: kept {kept:.2} of the ink, expected {:.2}",
                fade_alpha(t)
            );
            compared += 1;
        }
        assert!(compared >= 8, "too few inked columns to judge the ramp");
    }
}
