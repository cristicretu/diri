//! The terminal font controls change pixels the way the settings say:
//! stroke thickening adds ink at every level, smoothing off removes the
//! luminance dilation, and a widened cell spaces glyphs by the new width and
//! centres each one in its cell.
#![cfg(target_os = "macos")]

use std::sync::Arc;

use diri_proto::grid::{ChangedRow, GridCell, GridUpdate, TermColor, TermStyle};
use diri_term::{buffer::GridBuffer, element::TerminalElement};
use gpui::{
    AppContext as _, Context, GlyphRasterStyle, IntoElement, ParentElement, Point, Render,
    RenderGlyphParams, Styled, Window, div, font, px,
};

/// Summed coverage of one glyph rasterized at 1x with `stroke` and
/// `dilation`, and its bitmap width.
fn coverage(text: &dyn gpui::PlatformTextSystem, stroke: u8, dilation: u8) -> (u64, i32) {
    let font_id = text.font_id(&font("Menlo")).expect("Menlo");
    let glyph_id = text.glyph_for_char(font_id, 'l').expect("glyph");
    let params = RenderGlyphParams {
        font_id,
        glyph_id,
        font_size: px(13.0),
        subpixel_variant: Point::default(),
        scale_factor: 1.0,
        is_emoji: false,
        subpixel_rendering: false,
        dilation,
        stroke,
    };
    let bounds = text.glyph_raster_bounds(&params).expect("bounds");
    let (size, bytes) = text.rasterize_glyph(&params, bounds).expect("raster");
    (
        bytes.iter().map(|&byte| u64::from(byte)).sum(),
        size.width.0,
    )
}

fn every_thickening_level_adds_ink_and_fits_its_tile(text: &dyn gpui::PlatformTextSystem) {
    let levels: Vec<(u64, i32)> = (0..=GlyphRasterStyle::MAX_STROKE)
        .map(|stroke| coverage(text, stroke, 4))
        .collect();
    for pair in levels.windows(2) {
        assert!(
            pair[1].0 > pair[0].0,
            "a heavier stroke drew less ink: {levels:?}"
        );
    }
    // The stroked tile has a pixel more margin per side for the outline.
    assert_eq!(levels[1].1, levels[0].1 + 2, "{levels:?}");
    // Light text on dark gets the platform's smoothing dilation unless
    // smoothing is off (dilation 0).
    let (smoothed, _) = coverage(text, 0, 4);
    let (plain, _) = coverage(text, 0, 0);
    assert!(smoothed > plain, "smoothing {smoothed} vs off {plain}");
}

const COLS: u16 = 8;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

/// Columns of the leftmost ink of each `l` in "l l l l" at `cell_width`.
fn glyph_columns(cx: &mut gpui::HeadlessAppContext, cell_width: f32) -> Vec<u32> {
    let mut cells: Vec<GridCell> = "l l l l"
        .chars()
        .map(|ch| {
            GridCell::new(
                u32::from(ch),
                TermColor::Default,
                TermColor::DefaultInverted,
                TermStyle::empty(),
            )
        })
        .collect();
    cells.resize(usize::from(COLS), GridCell::BLANK);
    let terminal = TerminalElement::with_buffer(GridBuffer::new(COLS, 1))
        .focused(false)
        .font(font("Menlo"))
        .font_size(px(20.0))
        .cell_width_scale(cell_width);
    terminal.apply_damage(GridUpdate {
        cols: COLS,
        rows: 1,
        cursor_col: 0,
        cursor_row: 0,
        cursor_visible: false,
        is_full_snapshot: true,
        changed_rows: vec![ChangedRow::new(0, cells)],
        cursor_style: Default::default(),
        history: None,
    });
    let window = cx
        .open_window(gpui::size(px(240.0), px(30.0)), move |_, cx| {
            cx.new(|_| Fixture(terminal))
        })
        .expect("headless window");
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    let background = *image.get_pixel(image.width() - 1, 0);
    let inked: Vec<bool> = (0..image.width())
        .map(|x| (0..image.height()).any(|y| *image.get_pixel(x, y) != background))
        .collect();
    let mut starts = Vec::new();
    for x in 0..inked.len() {
        if inked[x] && (x == 0 || !inked[x - 1]) {
            starts.push(x as u32);
        }
    }
    starts
}

// One test: two headless platforms cannot start concurrently.
#[test]
fn font_controls_change_the_pixels_they_promise() {
    let platform = gpui_platform::current_platform(true);
    every_thickening_level_adds_ink_and_fits_its_tile(platform.text_system().as_ref());
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let natural = glyph_columns(&mut cx, 1.0);
    let wide = glyph_columns(&mut cx, 1.5);
    assert_eq!(natural.len(), 4, "{natural:?}");
    assert_eq!(wide.len(), 4, "{wide:?}");
    // Two cells between glyphs: their spacing grows by the scale.
    let step = |starts: &[u32]| f64::from(starts[3] - starts[0]) / 3.0;
    let ratio = step(&wide) / step(&natural);
    assert!((ratio - 1.5).abs() < 0.03, "spacing ratio {ratio}");
    // The first glyph moves right by half of what its cell gained.
    let cell = step(&natural) / 2.0;
    let inset = f64::from(wide[0]) - f64::from(natural[0]);
    assert!(
        (inset - cell * 0.25).abs() <= 1.5,
        "inset {inset} px, expected {}",
        cell * 0.25
    );
}
