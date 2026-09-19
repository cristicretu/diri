//! Leak probe for closed windows: opens and closes N popup windows, then
//! counts the GPUIViews and CAMetalLayers still on the heap. One main window
//! stays open, so a leak-free run reports 1 of each; a leaking build reports
//! N + 1 and a footprint that grows about 5 MB per popup.
//!
//! `cargo run -p diri-term --example popup_leak_probe -- 30`

use std::time::Duration;

use gpui::{
    App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};

struct Probe;

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child("probe")
    }
}

fn options(kind: WindowKind) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(200.0), px(200.0)),
            size: size(px(280.0), px(112.0)),
        })),
        titlebar: None,
        focus: false,
        show: true,
        kind,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    }
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(30);
    gpui_platform::application().run(move |cx: &mut App| {
        cx.open_window(options(WindowKind::Normal), |_, cx| cx.new(|_| Probe))
            .unwrap();
        cx.spawn(async move |cx| {
            let timer = |ms| cx.background_executor().timer(Duration::from_millis(ms));
            for _ in 0..rounds {
                let handle = cx.update(|cx| {
                    cx.open_window(options(WindowKind::PopUp), |_, cx| cx.new(|_| Probe))
                        .unwrap()
                });
                timer(120).await;
                cx.update(|cx| {
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                });
                timer(60).await;
            }
            timer(1500).await;
            let pid = std::process::id().to_string();
            let heap = std::process::Command::new("heap")
                .arg(&pid)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&heap.stdout);
            for line in text.lines() {
                if line.contains("GPUIView")
                    || line.contains(" CAMetalLayer ")
                    || line.contains("Physical footprint:")
                {
                    println!("{}", line.trim());
                }
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
