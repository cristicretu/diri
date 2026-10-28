//! Headless renders of the terminal font controls: the regular, bold and
//! italic faces, stroke thickening, font smoothing and cell width, at 1x
//! (a non-Retina external display) and 2x:
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/fonts DIRI_VISUAL_SCALE=1 \
//!   cargo test -p diri-term --test font_controls_goldens -- --ignored
//! ```
//!
//! Writes `<font>-<variant>@<scale>x.png` for SF Mono, JetBrains Mono and
//! JetBrains Mono Light (a thin face) when they are installed.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::grid::GridUpdate;
use diri_term::{buffer::GridBuffer, element::TerminalElement, glyphs::TerminalFaces};
use diri_terminal_state::HeadlessScreen;
use gpui::{
    AppContext as _, Context, Font, GlyphRasterStyle, IntoElement, ParentElement, Render, Styled,
    Window, div, px,
};

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

/// A shell listing, a diff and a prompt: regular, bold, italic, bold italic
/// and dim text, light on dark and dark on light.
fn sample() -> GridUpdate {
    let lines = [
        "\x1b[1;38;2;137;180;250m~/fun/diri\x1b[0m on \x1b[1;35m main\x1b[0m \x1b[2mvia rust 1.95\x1b[0m",
        "\x1b[32m❯\x1b[0m cargo test -p diri-term --test glyphs",
        "\x1b[38;2;205;214;244mfn thicken(px: u8) -> Result<(), Error> { il1| O0 }",
        "\x1b[3;38;2;166;173;200m// italic comment: the quick brown fox jumps\x1b[0m",
        "\x1b[1;3;38;2;249;226;175mbold italic: WARNING thin strokes\x1b[0m",
        "\x1b[48;2;239;241;245;38;2;76;79;105m dark on light: let width = 0.6 * em; \x1b[0m",
        "\x1b[48;2;239;241;245;1;38;2;30;102;245m bold on light: #[derive(Debug)]      \x1b[0m",
    ];
    let mut screen = HeadlessScreen::new(60, lines.len());
    screen.feed(b"\x1b[H");
    for (index, line) in lines.iter().enumerate() {
        screen.feed(line.as_bytes());
        if index + 1 < lines.len() {
            screen.feed(b"\x1b[0m\r\n");
        }
    }
    screen.feed(b"\x1b[0m\x1b[?25l");
    screen.full_snapshot()
}

struct Variant {
    name: &'static str,
    weight: u16,
    faces: TerminalFaces,
    raster: GlyphRasterStyle,
    cell_width: f32,
}

fn variants() -> Vec<Variant> {
    let plain = |name, raster, cell_width| Variant {
        name,
        weight: 400,
        faces: TerminalFaces::default(),
        raster,
        cell_width,
    };
    let stroke = |stroke| GlyphRasterStyle {
        smoothing: true,
        stroke,
    };
    vec![
        plain("default", GlyphRasterStyle::default(), 1.0),
        plain("thicken1", stroke(1), 1.0),
        plain("thicken2", stroke(2), 1.0),
        plain("thicken3", stroke(3), 1.0),
        plain("thicken4", stroke(4), 1.0),
        plain(
            "smoothing-off",
            GlyphRasterStyle {
                smoothing: false,
                stroke: 0,
            },
            1.0,
        ),
        plain("cell110", GlyphRasterStyle::default(), 1.1),
        plain("cell90", GlyphRasterStyle::default(), 0.9),
        Variant {
            name: "faces",
            weight: 500,
            faces: TerminalFaces {
                bold: 800,
                italic: Some(300),
            },
            raster: GlyphRasterStyle::default(),
            cell_width: 1.0,
        },
    ]
}

fn capture(cx: &mut gpui::HeadlessAppContext, font: &Font, variant: &Variant, name: &str) {
    let update = sample();
    let (cols, rows) = (update.cols, update.rows);
    let font_size = 13.0;
    let window_size = gpui::size(
        px(f32::from(cols) * font_size * 0.6 * 1.1 + 4.0),
        px(f32::from(rows) * font_size * 1.5),
    );
    let mut regular = font.clone();
    regular.weight = gpui::FontWeight(f32::from(variant.weight));
    let element = TerminalElement::with_buffer(GridBuffer::new(cols, rows))
        .focused(false)
        .font(regular)
        .font_size(px(font_size))
        .faces(variant.faces)
        .glyph_raster(variant.raster)
        .cell_width_scale(variant.cell_width);
    element.apply_damage(update);
    let window = cx
        .open_window(window_size, |_, cx| cx.new(|_| Fixture(element)))
        .expect("headless window");
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    image.save(output(name)).expect("write png");
}

fn output(name: &str) -> PathBuf {
    let directory = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(PathBuf::from)
        .expect("DIRI_VISUAL_OUTPUT names the output directory");
    std::fs::create_dir_all(&directory).expect("output directory");
    directory.join(name)
}

#[test]
#[ignore = "writes PNG renders for review"]
fn render_font_control_goldens() {
    let scale: f32 = std::env::var("DIRI_VISUAL_SCALE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2.0);
    gpui::set_test_window_scale_factor(scale);
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| diri_term::glyphs::register_bundled_fonts(cx))
        .expect("bundled fonts");
    // A headless process can't resolve the system's SF Mono by name; load
    // the file the system ships it in.
    let sf_mono = std::fs::read("/System/Library/Fonts/SFNSMono.ttf").ok();
    if let Some(bytes) = sf_mono {
        cx.update(|cx| cx.text_system().add_fonts(vec![bytes.into()]))
            .expect("SF Mono");
    }
    let names = cx.update(|cx| cx.text_system().all_font_names());
    let mut fonts: Vec<(&str, &str, u16)> = Vec::new();
    for (label, family, weight) in [
        ("sfmono", ".SF NS Mono", 400u16),
        ("sfmono", "SF Mono", 400),
        ("jetbrains", "JetBrains Mono", 400),
        ("jetbrains-light", "JetBrains Mono", 300),
        ("menlo", "Menlo", 400),
    ] {
        if names.iter().any(|name| name == family)
            && !fonts.iter().any(|(existing, ..)| *existing == label)
        {
            fonts.push((label, family, weight));
        }
    }
    eprintln!("fonts: {fonts:?}");
    for (label, family, weight) in fonts {
        let font = diri_term::glyphs::terminal_font_with_weight(family, true, weight);
        for mut variant in variants() {
            if variant.weight == 400 {
                variant.weight = weight;
            }
            capture(
                &mut cx,
                &font,
                &variant,
                &format!("{label}-{}@{scale}x.png", variant.name),
            );
        }
    }
}
