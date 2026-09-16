//! Real wall-clock UI processing with scheduled synthetic frame ticks. Metal
//! submission excludes readback; this does not measure display presentation.
use super::*;
use crate::{tab_peek::GestureFrame, workspace_fixture::OutputDriver};
use gpui::{
    HeadlessAppContext, WindowHandle,
    profiler::{FrameTimingCollector, set_frame_trace_enabled},
};
use std::path::Path;

struct TraceGuard(bool);
impl Drop for TraceGuard {
    fn drop(&mut self) {
        set_frame_trace_enabled(self.0);
    }
}
fn distribution(values: &[f64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p = |fraction: f64| sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize];
    serde_json::json!({"median":p(0.5),"p90":p(0.9),"p95":p(0.95),"max":sorted.last().unwrap()})
}
fn generations(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<RootView>,
) -> std::collections::HashMap<SessionId, u64> {
    cx.update_window(window.into(), |view, _, cx| {
        view.downcast::<RootView>()
            .unwrap()
            .read(cx)
            .preview_buffers(cx)
            .into_iter()
            .map(|(id, grid)| (id, grid.read().unwrap().generation()))
            .collect()
    })
    .unwrap()
}
pub(super) fn run(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<RootView>,
    output: &OutputDriver,
    path: &Path,
) {
    cx.update(|cx| cx.set_reduce_motion(false));
    let _trace = TraceGuard(set_frame_trace_enabled(true));
    let mut collector = FrameTimingCollector::new();
    let before = generations(cx, window);
    let start = Instant::now();
    let mut previous = start;
    let ticks_before = output.ticks();
    let mut draws = Vec::new();
    let mut dirty_to_draw = Vec::new();
    let mut submission = Vec::new();
    let mut lateness = Vec::new();
    let mut invalidations = Vec::new();
    let mut callbacks = Vec::new();
    const FRAMES: usize = 360;
    const WARMUP: usize = 120;
    const HZ: f64 = 60.0;
    for index in 0..FRAMES {
        let target = start + Duration::from_secs_f64(index as f64 / HZ);
        if let Some(delay) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(delay);
        }
        let now = Instant::now();
        cx.advance_clock(now.saturating_duration_since(previous));
        previous = now;
        let phase = index % 180;
        let frame = match phase {
            0..=59 => Some(GestureFrame::Tracking(phase as f32 / 59.0 * 320.0)),
            60 => Some(GestureFrame::Released(320.0)),
            90..=149 => Some(GestureFrame::Tracking(
                -((phase - 90) as f32 / 59.0 * 340.0),
            )),
            150 => Some(GestureFrame::Released(-340.0)),
            _ => None,
        };
        let count = cx
            .update_window(window.into(), |view, native, cx| {
                let count = native.simulate_next_frame(cx);
                if let Some(frame) = frame {
                    view.downcast::<RootView>().unwrap().update(cx, |root, cx| {
                        root.session_surfaces
                            .as_ref()
                            .unwrap()
                            .update(cx, |surface, cx| surface.tab_gesture(frame, cx))
                    });
                }
                count
            })
            .unwrap();
        cx.run_until_parked();
        let began = Instant::now();
        cx.update_window(window.into(), |_, native, _| native.present_if_needed())
            .unwrap();
        let submitted = Instant::now();
        let timing = collector.collect_unseen();
        if index >= WARMUP {
            callbacks.push(count);
            lateness.push(now.saturating_duration_since(target).as_secs_f64() * 1000.0);
            submission.push((submitted - began).as_secs_f64() * 1000.0);
            for frame in timing
                .into_iter()
                .filter(|frame| frame.window_id == window.window_id())
            {
                draws.push(frame.draw_duration().as_secs_f64() * 1000.0);
                if let Some(duration) = frame.dirty_to_draw_duration() {
                    dirty_to_draw.push(duration.as_secs_f64() * 1000.0);
                }
                invalidations.push(frame.invalidations);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let after = generations(cx, window);
    for (session, generation) in &before {
        assert!(
            after
                .get(session)
                .is_some_and(|current| current > generation),
            "live PTY grid did not advance for {session}"
        );
    }
    assert!(!draws.is_empty());
    assert!(
        callbacks.iter().any(|count| *count > 0),
        "profile must execute scheduled settle callbacks"
    );
    assert!(
        callbacks.iter().all(|count| *count <= 2),
        "settle callbacks must stay bounded per tick"
    );
    let ticks = output.ticks() - ticks_before;
    assert!(ticks > 100, "fixture did not sustain output");
    let report = serde_json::json!({"schemaVersion":1,"kind":"wall-clock-scheduled-headless-metal-submission","build":if cfg!(debug_assertions){"debug"}else{"release"},"physicalTrackpadVerified":false,"displayVsyncMeasured":false,"pixelReadbackInMeasurement":false,"scriptFrameHz":HZ,"warmupTicks":WARMUP,"sampleTicks":FRAMES-WARMUP,"observedDraws":draws.len(),"elapsedSeconds":elapsed,"outputRequestsPerSession":ticks,"observedOutputRequestHz":ticks as f64/elapsed,"workload":"Eight saved tabs sharing two real shell PTYs, including a split tab; both visible grids change at nominal 50Hz.","drawMs":distribution(&draws),"dirtyToDrawEndMs":distribution(&dirty_to_draw),"metalSubmissionCpuMs":distribution(&submission),"scriptTickLatenessMs":distribution(&lateness),"callbacksPerTickMax":callbacks.iter().max(),"coalescedInvalidationsMax":invalidations.iter().max(),"note":"on_next_frame callbacks use synthetic ticks scheduled against wall time. Draw timings are actual GPUI work; submission timings encode/submit a reused offscreen Metal target without pixel readback. The test runtime draws invalidations eagerly, so draw count is not display frame count. Neither is photon latency or observed hardware presentation cadence."});
    std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
}
