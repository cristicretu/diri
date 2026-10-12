//! Inline images through the whole local pipeline: real Kitty and iTerm2
//! escape sequences into the Engine's emulator, its grid into the renderer,
//! the stored bytes through the client decoder.
//!
//! ```sh
//! DIRI_VISUAL_OUTPUT=/tmp/images cargo test -p diri-term --test inline_images -- --ignored
//! ```
//!
//! `DIRI_VISUAL_STREAM=<file>` also renders a captured 72×22 byte stream (a
//! real tool's output, say `kitten icat`) as `stream.png`.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use diri_proto::inline_image::ImageAnchor;
use diri_term::buffer::GridBuffer;
use diri_term::element::TerminalElement;
use diri_term::inline_images::decode;
use diri_terminal_state::HeadlessScreen;
use gpui::{AppContext as _, Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

const COLS: usize = 72;
const ROWS: usize = 22;

struct Fixture(TerminalElement);

impl Render for Fixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// A dusk landscape: sky gradient, a sun, two ridges.
fn landscape(width: u32, height: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(width, height, |x, y| {
        let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
        let ridge = |phase: f32, base: f32, amplitude: f32| {
            base + amplitude * ((fx * 6.0 + phase).sin() * 0.6 + (fx * 13.0 + phase).sin() * 0.4)
        };
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let (sun_x, sun_y) = (0.68, 0.55);
        let sun = ((fx - sun_x) * width as f32 / height as f32).hypot(fy - sun_y);
        let color = if fy > ridge(1.3, 0.78, 0.05) {
            [34.0, 30.0, 52.0]
        } else if fy > ridge(0.2, 0.64, 0.07) {
            [74.0, 52.0, 86.0]
        } else if sun < 0.16 {
            [255.0, 214.0, 140.0]
        } else {
            let t = fy / 0.7;
            [
                lerp(52.0, 250.0, t),
                lerp(60.0, 132.0, t),
                lerp(120.0, 110.0, t),
            ]
        };
        image::Rgba([color[0] as u8, color[1] as u8, color[2] as u8, 255])
    })
}

/// A bar chart with a transparent background.
fn chart(width: u32, height: u32) -> image::RgbaImage {
    let bars = [0.35, 0.6, 0.45, 0.8, 0.7, 0.95];
    image::RgbaImage::from_fn(width, height, |x, y| {
        let slot = width / bars.len() as u32;
        let index = (x / slot) as usize;
        let inside = x % slot > slot / 6 && x % slot < slot - slot / 6;
        let top = height as f32 * (1.0 - bars[index.min(bars.len() - 1)]);
        if inside && y as f32 >= top {
            let hue = index as f32 / bars.len() as f32;
            image::Rgba([
                (90.0 + 140.0 * hue) as u8,
                (200.0 - 90.0 * hue) as u8,
                (230.0 - 40.0 * hue) as u8,
                255,
            ])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    })
}

fn png(image: &image::RgbaImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// Kitty's chunked direct transmission, as `kitten icat` sends it.
fn kitty(control: &str, payload: &[u8]) -> Vec<u8> {
    let encoded = base64(payload);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4096)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect();
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let keys = if index == 0 {
            format!("{control},m={more}")
        } else {
            format!("m={more}")
        };
        out.extend_from_slice(format!("\x1b_G{keys};{chunk}\x1b\\").as_bytes());
    }
    out
}

fn prompt(command: &str) -> Vec<u8> {
    format!("\x1b[1;32m~/diri\x1b[0m \x1b[1;34m$\x1b[0m {command}").into_bytes()
}

/// The screen a user sees after showing two images, plus `extra` lines.
fn screen(extra: usize) -> HeadlessScreen {
    let mut screen = HeadlessScreen::new(COLS, ROWS).with_inline_images();
    // A Retina Menlo 13 cell; the client measures its own.
    screen.set_cell_pixels(16, 32);
    screen.feed(&prompt("kitten icat dusk.png\r\n"));
    screen.feed(&kitty("a=T,f=100,q=2", &png(&landscape(448, 224))));
    screen.feed(b"\r\n");
    screen.feed(&prompt("imgcat --width 24 revenue.png\r\n"));
    let chart = png(&chart(384, 160));
    screen.feed(
        format!(
            "\x1b]1337;File=name={};size={};width=24;inline=1:{}\x07\r\n",
            base64(b"revenue.png"),
            chart.len(),
            base64(&chart)
        )
        .as_bytes(),
    );
    screen.feed(b"\x1b[2mq3 revenue by region, inline via OSC 1337\x1b[0m\r\n");
    for line in 0..extra {
        screen.feed(format!("build step {line:02} \x1b[32mok\x1b[0m\r\n").as_bytes());
    }
    screen.feed(&prompt(""));
    screen
}

/// The element for `screen`, its images decoded the way the app does.
fn element(screen: &HeadlessScreen) -> TerminalElement {
    let element = TerminalElement::with_buffer(GridBuffer::new(COLS as u16, ROWS as u16))
        .focused(true)
        .font(gpui::font("Menlo"));
    let grid = screen.full_snapshot();
    let cache = element.inline_images();
    for row in &grid.changed_rows {
        for link in &row.metadata.links {
            let anchor = ImageAnchor::parse(&link.uri).expect("only image links");
            if cache.lookup(&anchor.key).is_none() {
                let stored = screen.inline_image(&anchor.key).expect("stored");
                let image = decode(
                    stored.format,
                    stored.compressed,
                    stored.width,
                    stored.height,
                    &stored.data,
                )
                .expect("decodes");
                cache.insert(&anchor.key, image);
            }
        }
    }
    element.apply_damage(grid);
    element
}

#[test]
fn every_image_row_reaches_the_renderer_with_its_anchor() {
    let screen = screen(0);
    let grid = screen.full_snapshot();
    let anchored: Vec<_> = grid
        .changed_rows
        .iter()
        .flat_map(|row| row.metadata.links.iter().map(move |link| (row.y, link)))
        .map(|(y, link)| (y, ImageAnchor::parse(&link.uri).unwrap()))
        .collect();
    // 448×224 px at 16×32 px cells is 28×7; the chart is 24 columns wide and
    // 384×160 px keeps its aspect: 5 rows.
    let first: Vec<_> = anchored.iter().filter(|(_, a)| a.cols == 28).collect();
    let second: Vec<_> = anchored.iter().filter(|(_, a)| a.cols == 24).collect();
    assert_eq!((first.len(), first[0].1.rows), (7, 7));
    assert_eq!((second.len(), second[0].1.rows), (5, 5));
    assert_eq!(first[0].0, 1, "below its prompt");
}

fn capture(element: TerminalElement, output: PathBuf) {
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    let window = cx
        .open_window(gpui::size(px(600.0), px(380.0)), |_, cx| {
            cx.new(|_| Fixture(element))
        })
        .expect("headless window");
    cx.run_until_parked();
    // A second frame, as the paint fixture takes: the first can land before
    // the window has content.
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    image.save(output).expect("write screenshot");
}

fn output(name: &str) -> PathBuf {
    let directory = std::env::var_os("DIRI_VISUAL_OUTPUT")
        .map(PathBuf::from)
        .expect("DIRI_VISUAL_OUTPUT names the output directory");
    std::fs::create_dir_all(&directory).expect("output directory");
    directory.join(name)
}

// One test: two headless platforms cannot start concurrently.
#[test]
#[ignore = "renders PNG screenshots of inline images"]
fn render_inline_images_screenshot() {
    if let Some(stream) = std::env::var_os("DIRI_VISUAL_STREAM") {
        let stream = std::fs::read(stream).expect("stream");
        let mut screen = HeadlessScreen::new(COLS, ROWS).with_inline_images();
        screen.set_cell_pixels(16, 34);
        screen.feed(&stream);
        capture(element(&screen), output("stream.png"));
    }
    capture(element(&screen(0)), output("inline-images.png"));
    // Output pushed the first image half into history: only its rows still
    // on screen draw, clipped at the top edge.
    capture(element(&screen(7)), output("inline-images-scrolled.png"));
}
