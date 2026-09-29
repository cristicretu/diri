//! How long a keystroke's echo waits for its frame on this Mac's display link.
//!
//! No window: a real `CVDisplayLink` on the main display ticks a dispatch
//! source the way a window's frame source does, and a typist thread lands
//! echoes on the frame queue at random moments. Each echo starts a cursor
//! glide that keeps frames coming for [`GLIDE`], as Diri's terminal does.
//! Two policies are compared on the same ticks, alternating runs:
//!
//! * `display link`: the echo waits for the next tick (GPUI's only pacer);
//! * `immediate`: the echo asks for a frame at once the way
//!   `WindowFrameSource::request_now` does (one merge into the tick source),
//!   under the window's own `immediate_frame_allowed` rule: refused while a
//!   frame drawn in the last two refresh intervals may still be queued.
//!
//! The reported hop is "echo applied → frame starts drawing". It is the part
//! of the path the policy changes; drawing, the GPU and the compositor come
//! after it and are the same for both.
//!
//! ```sh
//! cargo test -p gpui_macos --release --lib echo_frame_scheduling -- --ignored --nocapture
//! ```

use super::sys;
use core_graphics::display::CGDisplay;
use dispatch2::{
    _dispatch_source_type_data_add, DispatchObject, DispatchQueue, DispatchQueueAttr,
    DispatchRetained, DispatchSource,
};
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GLIDE: Duration = Duration::from_millis(80);

#[derive(Default)]
struct Pacing {
    immediate: bool,
    interval: Duration,
    /// When the frame queue applied the echo that has not been drawn yet.
    pending: Option<Instant>,
    /// Frames keep coming until then (the cursor glide).
    animating_until: Option<Instant>,
    last_frame: Option<Instant>,
    waits: Vec<Duration>,
    immediate_frames: usize,
    ticks: usize,
}

static PACING: Mutex<Option<Pacing>> = Mutex::new(None);
static SOURCE: Mutex<Option<DispatchRetained<DispatchSource>>> = Mutex::new(None);

unsafe extern "C" fn tick(
    _link: *mut sys::CVDisplayLink,
    _now: *const sys::CVTimeStamp,
    _output: *const sys::CVTimeStamp,
    _flags_in: i64,
    _flags_out: *mut i64,
    _context: *mut c_void,
) -> i32 {
    if let Some(source) = SOURCE.lock().unwrap().as_ref() {
        source.merge_data(1);
    }
    0
}

/// The frame handler, on the frame queue: draws if an echo is pending or an
/// animation runs, as GPUI's `on_request_frame` draws only a dirty window.
extern "C" fn frame(_context: *mut c_void) {
    let now = Instant::now();
    let mut guard = PACING.lock().unwrap();
    let Some(pacing) = guard.as_mut() else {
        return;
    };
    pacing.ticks += 1;
    let animating = pacing.animating_until.is_some_and(|until| now < until);
    if let Some(applied) = pacing.pending.take() {
        pacing.waits.push(now.saturating_duration_since(applied));
        pacing.animating_until = Some(now + GLIDE);
        pacing.last_frame = Some(now);
    } else if animating {
        pacing.last_frame = Some(now);
    }
}

/// The display's refresh interval, from its ticks over half a second.
fn measure_interval() -> Duration {
    *PACING.lock().unwrap() = Some(Pacing::default());
    let started = Instant::now();
    std::thread::sleep(Duration::from_millis(500));
    let ticks = PACING.lock().unwrap().take().unwrap().ticks.max(1);
    started.elapsed() / ticks as u32
}

fn run(
    immediate: bool,
    interval: Duration,
    keys: usize,
    queue: &DispatchQueue,
    seed: &mut u64,
) -> Pacing {
    *PACING.lock().unwrap() = Some(Pacing {
        immediate,
        interval,
        ..Pacing::default()
    });
    for _ in 0..keys {
        // Typing, not a stream: 60 to 240 ms between keys.
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        std::thread::sleep(Duration::from_millis(60 + *seed % 180));
        queue.exec_async(|| {
            let now = Instant::now();
            let mut guard = PACING.lock().unwrap();
            let pacing = guard.as_mut().unwrap();
            pacing.pending = Some(now);
            let allowed =
                crate::window::immediate_frame_allowed(pacing.last_frame, pacing.interval, now);
            if pacing.immediate && allowed {
                pacing.immediate_frames += 1;
                drop(guard);
                if let Some(source) = SOURCE.lock().unwrap().as_ref() {
                    source.merge_data(1);
                }
            }
        });
    }
    std::thread::sleep(Duration::from_millis(100));
    PACING.lock().unwrap().take().unwrap()
}

fn percentile(sorted: &[Duration], q: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * q).round() as usize].as_secs_f64() * 1000.0
}

#[test]
#[ignore = "measurement against this Mac's display link; run explicitly"]
fn echo_frame_scheduling_against_the_display_link() {
    let keys: usize = std::env::var("DIRI_PACING_KEYS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(150);
    let queue = DispatchQueue::new("dev.diri.pacing", DispatchQueueAttr::SERIAL);
    let source = unsafe {
        let source = DispatchSource::new(
            &raw const _dispatch_source_type_data_add as *mut _,
            0,
            0,
            Some(&queue),
        );
        source.set_event_handler_f(frame);
        source.resume();
        source
    };
    *SOURCE.lock().unwrap() = Some(source);
    let display = CGDisplay::main().id;
    let mut link =
        unsafe { sys::DisplayLink::new(display, tick, std::ptr::null_mut()) }.expect("link");
    unsafe { link.start() }.expect("start");

    let interval = measure_interval();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut results: [Vec<Pacing>; 2] = [Vec::new(), Vec::new()];
    for round in 0..6 {
        let immediate = round % 2 == 1;
        let pacing = run(immediate, interval, keys / 3, &queue, &mut seed);
        let mut waits = pacing.waits.clone();
        waits.sort();
        println!(
            "run {round} {:<13} p50 {:>6.3} ms  p95 {:>6.3} ms",
            if immediate {
                "immediate"
            } else {
                "display link"
            },
            percentile(&waits, 0.5),
            percentile(&waits, 0.95),
        );
        results[usize::from(immediate)].push(pacing);
    }
    unsafe { link.stop() }.expect("stop");
    *SOURCE.lock().unwrap() = None;

    for (label, runs) in [("display link", &results[0]), ("immediate", &results[1])] {
        let mut waits: Vec<Duration> = runs.iter().flat_map(|run| run.waits.clone()).collect();
        waits.sort();
        let immediate: usize = runs.iter().map(|run| run.immediate_frames).sum();
        let ticks: usize = runs.iter().map(|run| run.ticks).sum();
        println!(
            "{label:<13} echo applied -> frame start  p50 {:>6.3} ms  p95 {:>6.3} ms  max {:>6.3} ms  \
             (n={}, immediate frames {immediate}, ticks {ticks}, refresh {:.2} ms)",
            percentile(&waits, 0.5),
            percentile(&waits, 0.95),
            percentile(&waits, 1.0),
            waits.len(),
            runs[0].interval.as_secs_f64() * 1000.0,
        );
    }
}
