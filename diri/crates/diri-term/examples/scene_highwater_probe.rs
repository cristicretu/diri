//! Probe: does one glyph-heavy frame leave GPUI's scene vectors at their
//! high-water capacity? Paints `glyphs` visible glyphs for a few frames, then
//! a near-empty frame, then prints the largest heap blocks.
//!
//! `cargo run --release -p diri-term --example scene_highwater_probe -- 120000`

use std::time::Duration;

use gpui::{
    App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowBounds, WindowOptions, div, point, px, size,
};

struct Probe {
    glyphs: usize,
    heavy: bool,
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let root = div().size_full().text_size(px(2.0)).line_height(px(2.0));
        if !self.heavy {
            return root.child("idle");
        }
        // 400 glyphs a row at 2 px stays inside a 1200 px window, so nothing
        // is clipped away and every glyph reaches the scene.
        let row = "abcdefghij".repeat(40);
        root.children((0..self.glyphs / 400).map(move |_| div().child(row.clone())))
    }
}

fn largest_blocks(label: &str) {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("heap")
        .arg(&pid)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let sizes = text
        .lines()
        .find_map(|line| line.split("Sizes: ").nth(1))
        .unwrap_or_default();
    let top: Vec<&str> = sizes.split_whitespace().take(5).collect();
    println!("{label}: {}", top.join(" "));
}

fn main() {
    let glyphs: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(120_000);
    gpui_platform::application().run(move |cx: &mut App| {
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(100.0), px(100.0)),
                        size: size(px(1200.0), px(800.0)),
                    })),
                    ..Default::default()
                },
                move |_, cx| {
                    cx.new(|_| Probe {
                        glyphs,
                        heavy: false,
                    })
                },
            )
            .unwrap();
        cx.spawn(async move |cx| {
            let executor = cx.background_executor().clone();
            let wait = move |ms| executor.timer(Duration::from_millis(ms));
            wait(800).await;
            largest_blocks("before      ");
            let _ = window.update(cx, |probe, _, cx| {
                probe.heavy = true;
                cx.notify();
            });
            wait(1500).await;
            largest_blocks("heavy frame ");
            let _ = window.update(cx, |probe, _, cx| {
                probe.heavy = false;
                cx.notify();
            });
            wait(3000).await;
            largest_blocks("idle again  ");
            println!(
                "expect after {glyphs} glyphs: sprites >= {} KB, paint ops >= {} KB",
                glyphs * 112 / 1024,
                glyphs * 168 / 1024
            );
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
