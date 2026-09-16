//! Scripted input / native Metal processing profile, not display-link or hardware measurement.
use super::RootView;
use crate::tab_peek::GestureFrame;
use gpui::{
    HeadlessAppContext, WindowHandle,
    profiler::{FrameTimingCollector, set_frame_trace_enabled},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

struct FrameTraceGuard(bool);
impl Drop for FrameTraceGuard {
    fn drop(&mut self) {
        if self.0 {
            set_frame_trace_enabled(false);
        }
    }
}

fn distribution(values: &[f64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |fraction: f64| sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize];
    serde_json::json!({ "median": percentile(0.5), "p90": percentile(0.9), "p95": percentile(0.95), "max": sorted.last().unwrap() })
}

pub(super) fn run(cx: &mut HeadlessAppContext, window: WindowHandle<RootView>, output: &Path) {
    assert!(
        std::env::var_os("DIRI_PEEK_LIVE").is_some(),
        "profile requires seeded live previews"
    );
    let _trace = FrameTraceGuard(set_frame_trace_enabled(true));
    let mut collector = FrameTimingCollector::new();
    let mut draw_ms = Vec::new();
    const WARMUP: usize = 150;
    const SAMPLES: usize = 300;
    const HZ: f64 = 120.0;
    let invariant = cx
        .update_window(window.into(), |root, _, cx| {
            let root = root.downcast::<RootView>().unwrap();
            let root = root.read(cx);
            (
                root.terminal.as_ref().unwrap().read(cx).geometry_for_test(),
                root.services
                    .store
                    .store
                    .read()
                    .unwrap()
                    .selected_session_id()
                    .cloned(),
            )
        })
        .unwrap();
    cx.update_window(window.into(), |root, _, cx| {
        root.downcast::<RootView>().unwrap().update(cx, |root, cx| {
            root.session_surfaces
                .as_ref()
                .unwrap()
                .update(cx, |surface, cx| surface.cancel_tab_peek_immediately(cx));
        });
    })
    .unwrap();
    let mut update_ms = Vec::with_capacity(SAMPLES);
    let mut render_readback_ms = Vec::with_capacity(SAMPLES);
    let mut total_ms = Vec::with_capacity(SAMPLES);
    let mut live_counts = Vec::with_capacity(SAMPLES);
    let mut subscribed_counts = Vec::with_capacity(SAMPLES);
    for index in 0..WARMUP + SAMPLES {
        let phase = index % 150;
        let frame = match phase {
            0..=59 => Some(GestureFrame::Tracking(phase as f32 / 59.0 * 320.0)),
            60 => Some(GestureFrame::Released(320.0)),
            85..=119 => Some(GestureFrame::Tracking(
                -((phase - 85) as f32 / 34.0 * 340.0),
            )),
            120 => Some(GestureFrame::Cancelled),
            _ => None,
        };
        cx.advance_clock(Duration::from_secs_f64(1.0 / HZ));
        let began = Instant::now();
        cx.update_window(window.into(), |root, window, cx| {
            window.simulate_next_frame(cx);
            if let Some(frame) = frame {
                root.downcast::<RootView>().unwrap().update(cx, |root, cx| {
                    root.session_surfaces
                        .as_ref()
                        .unwrap()
                        .update(cx, |surface, cx| surface.tab_gesture(frame, cx));
                });
            }
        })
        .unwrap();
        cx.run_until_parked();
        let updated = Instant::now();
        let image = cx.capture_screenshot(window.into()).unwrap();
        let rendered = Instant::now();
        let timings = collector.collect_unseen();
        if index >= WARMUP {
            draw_ms.extend(
                timings
                    .iter()
                    .filter(|timing| timing.window_id == window.window_id())
                    .map(|timing| timing.draw_duration().as_secs_f64() * 1000.0),
            );
        }
        // Do not write PNGs inside measured work; readback is still reported
        // separately because it is not part of ordinary onscreen presentation.
        if let Some(directory) = std::env::var_os("DIRI_PEEK_PROFILE_FRAMES") {
            let path = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&path).unwrap();
            if index >= WARMUP {
                image
                    .save(path.join(format!("{:04}.png", index - WARMUP)))
                    .unwrap();
            }
        }
        if index >= WARMUP {
            update_ms.push((updated - began).as_secs_f64() * 1000.0);
            render_readback_ms.push((rendered - updated).as_secs_f64() * 1000.0);
            total_ms.push((rendered - began).as_secs_f64() * 1000.0);
        }
        let source_counts = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                let surface = root.read(cx).session_surfaces.as_ref().unwrap().read(cx);
                let states = surface.preview_fixture_states();
                (
                    states.len(),
                    states
                        .iter()
                        .filter(|state| *state.borrow() == crate::tab_preview::PreviewState::Live)
                        .count(),
                )
            })
            .unwrap();
        if index >= WARMUP {
            subscribed_counts.push(source_counts.0);
            live_counts.push(source_counts.1);
        }
        let current = cx
            .update_window(window.into(), |root, _, cx| {
                let root = root.downcast::<RootView>().unwrap();
                let root = root.read(cx);
                (
                    root.terminal.as_ref().unwrap().read(cx).geometry_for_test(),
                    root.services
                        .store
                        .store
                        .read()
                        .unwrap()
                        .selected_session_id()
                        .cloned(),
                )
            })
            .unwrap();
        assert_eq!(
            current, invariant,
            "scripted frame {index} changed terminal identity/geometry"
        );
    }
    assert!(!draw_ms.is_empty(), "GPUI must report actual window draws");
    assert!(
        live_counts.iter().any(|count| *count > 0),
        "profile must paint seeded inactive grids"
    );
    let report = serde_json::json!({
        "schemaVersion": 1, "kind": "simulated-input-headless-metal", "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "concurrentLoad": std::env::var("DIRI_PEEK_PROFILE_LOAD").unwrap_or_else(|_| "Not recorded".into()),
        "physicalTrackpadVerified": false, "displayVsyncMeasured": false,
        "scriptCadenceHz": HZ, "warmupFrames": WARMUP, "sampleFrames": SAMPLES,
        "workload": "Six single-pane cards; one resident 100x36 grid, receive-only 80x24 grids opened only while visible",
        "previewOutput": "One synthetic full snapshot per connection, idle afterward; output-heavy performance is a separate workload",
        "subscribedPreviewRange": [subscribed_counts.iter().min(), subscribed_counts.iter().max()],
        "livePreviewRange": [live_counts.iter().min(), live_counts.iter().max()],
        "windowDrawMs": distribution(&draw_ms), "drawnFrames": draw_ms.len(),
        "cpuUpdateMs": distribution(&update_ms), "metalRenderAndReadbackMs": distribution(&render_readback_ms),
        "combinedProcessingMs": distribution(&total_ms),
        "processingOver120HzBudget": total_ms.iter().filter(|duration| **duration > 1000.0 / 120.0).count(),
        "processingOver60HzBudget": total_ms.iter().filter(|duration| **duration > 1000.0 / 60.0).count(),
        "terminalIdentityAndGeometryPreserved": true,
        "note": "Scripted 120Hz clock, no physical input or display-link pacing. Metal readback is included separately; deadline counts are processing comparisons, not measured dropped display frames."
    });
    std::fs::write(output, serde_json::to_string_pretty(&report).unwrap()).unwrap();
}
