//! Headless probe of the full glyph pipeline: resolve -> shape -> rasterize.
//! Diagnoses the packaged-app "no text anywhere" bug without needing a window.

use gpui::{FontRun, Point, RenderGlyphParams, font, px};

fn main() {
    let ts = gpui_platform::current_platform(true).text_system();
    println!(
        "available font families: {} total",
        ts.all_font_names().len()
    );

    // What family name does font-kit assign the real macOS UI font?
    for path in [
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/SFCompact.ttf",
    ] {
        let Ok(bytes) = std::fs::read(path) else {
            println!("{path}: unreadable");
            continue;
        };
        let before: std::collections::HashSet<String> = ts.all_font_names().into_iter().collect();
        if let Err(error) = ts.add_fonts(vec![std::borrow::Cow::Owned(bytes)]) {
            println!("{path}: add_fonts failed: {error}");
            continue;
        }
        let after = ts.all_font_names();
        let new: Vec<_> = after
            .iter()
            .filter(|name| !before.contains(*name))
            .collect();
        println!("{path}: new families {new:?}");
    }

    for family in [
        ".SystemUIFont",
        ".SF NS",
        ".SF NS Mono",
        "SF Mono",
        "Menlo",
        "Helvetica",
    ] {
        let descriptor = font(family);
        match ts.font_id(&descriptor) {
            Err(error) => println!("{family}: font_id FAILED: {error:?}"),
            Ok(font_id) => {
                let metrics = ts.font_metrics(font_id);
                println!(
                    "{family}: id={font_id:?} units_per_em={}",
                    metrics.units_per_em
                );

                let layout = ts.layout_line("Hello", px(14.0), &[FontRun { len: 5, font_id }]);
                println!(
                    "  layout 'Hello': width={:?} runs={} glyphs={}",
                    layout.width,
                    layout.runs.len(),
                    layout.runs.iter().map(|r| r.glyphs.len()).sum::<usize>()
                );

                let Some(glyph_id) = ts.glyph_for_char(font_id, 'M') else {
                    println!("  glyph_for_char('M'): NONE");
                    continue;
                };
                let params = RenderGlyphParams {
                    font_id,
                    glyph_id,
                    font_size: px(14.0),
                    subpixel_variant: Point::default(),
                    scale_factor: 2.0,
                    is_emoji: false,
                    subpixel_rendering: false,
                    dilation: 0,
                    stroke: 0,
                };
                match ts.glyph_raster_bounds(&params) {
                    Err(error) => println!("  raster_bounds FAILED: {error:?}"),
                    Ok(bounds) => {
                        println!("  raster_bounds: {bounds:?}");
                        match ts.rasterize_glyph(&params, bounds) {
                            Err(error) => println!("  rasterize FAILED: {error:?}"),
                            Ok((size, bytes)) => println!(
                                "  rasterize: size={size:?} bytes={} nonzero={}",
                                bytes.len(),
                                bytes.iter().filter(|&&b| b > 0).count()
                            ),
                        }
                    }
                }
            }
        }
    }
}
