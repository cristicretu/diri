//! Output-to-photon through the real app window, hop by hop.
//!
//! `diri-term`'s `output_photon_probe` times one byte written to a terminal's
//! TTY through the Engine, the client and a bare `TerminalElement`. This
//! harness keeps that path and replaces the bare element with the window the
//! app opens: a full `RootView` (workspace workbench, split panes, title bar,
//! tab strip and, optionally, the sidebar) in a window with the app's own
//! title-bar options and material, at the size of a full-screen window.
//!
//! The window is a non-activating panel. It never takes focus, the process
//! stays out of the Dock, and it sits almost entirely off the screen (a
//! strip a few points wide stays on at the right edge so macOS keeps drawing
//! it). A guard aborts the run if the app ever becomes active or the panel
//! becomes key.
//! Nothing is posted through the OS event system: the probe writes to the
//! child's TTY, the way a background program prints.
//!
//! Each write starts a `diri_client::latency_trace` record; the pane, the
//! transport and GPUI's frame observer stamp the rest. Write times are also
//! logged as `CLOCK_UPTIME_RAW` nanoseconds so a ScreenCaptureKit film of
//! the panel can be joined against them.
//!
//! ```sh
//! cargo build --release -p diri-engine --bin diri-holder
//! DIRI_TEST_APP_PHOTON=1 DIRI_HOLDER_BIN=$CARGO_TARGET_DIR/release/diri-holder \
//!     cargo test --release -p diri-app --test app_photon_appkit
//! ```
//!
//! Like every test build, this one links GPUI's `test-support`, which leaves
//! the Metal layer readable (`framebufferOnly = NO`); the shipped app's is
//! not. A/B runs with that flag flipped showed no difference beyond noise.
//!
//! Knobs (environment): `DIRI_PHOTON_SAMPLES` (default 120),
//! `DIRI_PHOTON_SIZE` (`2056x1290`), `DIRI_PHOTON_MATERIAL` (`glass` or
//! `opaque`), `DIRI_PHOTON_SIDEBAR` (`1`), `DIRI_PHOTON_LOG` (a CSV path),
//! `DIRI_PHOTON_WARMUP_MS` (`2500`, time for a film to start),
//! `DIRI_PHOTON_VISIBLE` (points left on screen, `6`).
//!
//! Cleanup: every session is terminated and the fixture's temp root removed.

use std::time::{Duration, Instant};

use diri_client::latency_trace::{self, HOPS, Hop};
use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowKind, WindowOptions};

use super::*;
use crate::store::WindowMaterial;

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn uptime_ns() -> u64 {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: plain clock read into a stack value.
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut spec) };
    spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64
}

/// 1 = accessory (no Dock icon), 2 = prohibited (never activated).
fn set_activation_policy(policy: isize) {
    use objc2::msg_send;
    use objc2_app_kit::NSApplication;
    let mtm = objc2::MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let _: bool = unsafe { msg_send![&*app, setActivationPolicy: policy] };
}

/// Whether this process is active or owns the key window: either would mean
/// the probe took keyboard focus from the person using this Mac.
fn holds_focus() -> Option<String> {
    use objc2_app_kit::{NSApplication, NSWorkspace};
    let mtm = objc2::MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let front = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|front| -> i32 { unsafe { objc2::msg_send![&*front, processIdentifier] } });
    let ours = front == Some(std::process::id() as i32);
    let key = app.keyWindow().is_some();
    (app.isActive() || key || ours).then(|| {
        format!(
            "active={} key_window={key} frontmost_is_probe={ours}",
            app.isActive()
        )
    })
}

/// Prints whether macOS considers the probe's windows visible: an occluded
/// window gets no display-link frames, so its run would measure nothing.
fn report_occlusion() {
    use objc2_app_kit::{NSApplication, NSWindowOcclusionState};
    let mtm = objc2::MainThreadMarker::new().expect("main thread");
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        let visible = window.occlusionState().contains(NSWindowOcclusionState::Visible);
        if window.frame().size.width > 100.0 {
            println!(
                "app photon probe: window {:?} on screen: {visible}",
                window.frame()
            );
        }
    }
}

/// Runs only through the harness-free test executable on the AppKit main thread.
pub(crate) fn run_native() {
    let samples: usize = env_or("DIRI_PHOTON_SAMPLES", 120);
    let size_spec: String = env_or("DIRI_PHOTON_SIZE", "2056x1290".to_owned());
    let (width, height) = size_spec
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?)))
        .expect("DIRI_PHOTON_SIZE is WxH");
    let material = match std::env::var("DIRI_PHOTON_MATERIAL").as_deref() {
        Ok("opaque") => WindowMaterial::Opaque,
        _ => WindowMaterial::Glass,
    };
    let sidebar = std::env::var("DIRI_PHOTON_SIDEBAR").map_or(true, |value| value != "0");
    let warmup = Duration::from_millis(env_or("DIRI_PHOTON_WARMUP_MS", 2500));
    let visible: f32 = env_or("DIRI_PHOTON_VISIBLE", 6.0);
    let log = std::env::var_os("DIRI_PHOTON_LOG").map(std::path::PathBuf::from);

    // `cat` with echo off: the only output is what the probe writes to the
    // TTY from outside.
    // A real Holder, like the app's sessions, when `DIRI_HOLDER_BIN` names one.
    // Short root: Holder sockets live under it and must fit SUN_LEN.
    let holders = std::path::PathBuf::from(format!("/tmp/diri-ap-{}", std::process::id()));
    let holder = std::env::var_os("DIRI_HOLDER_BIN").map(|executable| {
        diri_engine::session::HolderConfig {
            holders_dir: holders.clone(),
            executable: executable.into(),
        }
    });
    if holder.is_none() {
        eprintln!("app photon probe: DIRI_HOLDER_BIN unset, using the direct PTY path");
    }
    let fixture = crate::workspace_fixture::LiveWorkspace::start_with_options(
        "tty > tty; stty -echo; printf 'photon probe\\r\\n'; printf ready > ready; exec cat",
        diri_proto::AgentKind::SHELL,
        holder,
    );
    let tty_path = std::fs::read_to_string(fixture.directory.path().join("build/tty"))
        .expect("child tty")
        .trim()
        .to_owned();
    let services = fixture.services.clone();
    services
        .store
        .store
        .write()
        .unwrap()
        .update_preferences(|prefs| {
            prefs.terminal_theme = "dirijor-dark".into();
            prefs.window_material = material;
            prefs.sidebar_visible = sidebar;
        })
        .unwrap();
    latency_trace::set_enabled(true);
    crate::telemetry::install_latency_trace();

    let fixture = std::sync::Mutex::new(Some(fixture));
    gpui_platform::application()
        .with_assets(diri_ui::IconAssets)
        .run(move |cx: &mut App| {
            // AppKit activates a launching app and makes its first window key
            // when it finishes launching (`_sendFinishLaunchingNotification`).
            // Prohibited until then; the panel opens only afterwards.
            set_activation_policy(2);
            load_system_fonts(cx);
            floating::enable(cx);
            app_theme::live::enable();
            diri_ui::set_mark_rasterizer(macos::brand_raster::raster_mark);
            macos::observe_scroller_style(cx);
            macos::observe_reduce_motion(cx);
            commands::bind_keys(cx, &Default::default());
            let services = services.clone();
            let tty_path = tty_path.clone();
            let log = log.clone();
            let fixture = std::sync::Mutex::new(fixture.lock().unwrap().take());
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if let Some(taken) = cx.update(|_| holds_focus()) {
                    eprintln!("app photon probe: active before any window ({taken}); aborting");
                    std::process::exit(3);
                }
                // Accessory: no Dock icon, never activated by the system.
                cx.update(|_| set_activation_policy(1));
                let window = cx.update(|cx| {
                    open_probe_window(cx, services, material, width, height, visible)
                });
                let frame = window
                    .update(cx, |_, window, _| window.bounds())
                    .expect("bounds");
                println!(
                    "app photon probe: pid={} tty={tty_path} window={frame:?} material={material:?} sidebar={sidebar}",
                    std::process::id()
                );
                cx.background_executor()
                    .timer(Duration::from_millis(1000))
                    .await;
                cx.update(|_| report_occlusion());
                // Focus guard: abort before the person's next keystroke could land here.
                cx.spawn(async move |cx| {
                    loop {
                        if let Some(taken) = cx.update(|_| holds_focus()) {
                            eprintln!("app photon probe: took focus ({taken}); aborting");
                            std::process::exit(3);
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(10))
                            .await;
                    }
                })
                .detach();
                let report = cx
                    .background_executor()
                    .spawn(async move { drive(&tty_path, samples, warmup, log.as_deref()) })
                    .await;
                println!("{report}");
                let _ = cx.update_window(window.into(), |_, window, _| window.remove_window());
                drop(fixture.lock().unwrap().take());
                // Holder managers outlive the in-process Engine by design; this
                // root is unique to the run, so only its own Holders match.
                let _ = std::process::Command::new("pkill")
                    .arg("-f")
                    .arg(&holders)
                    .status();
                let _ = std::fs::remove_dir_all(&holders);
                std::process::exit(0);
            })
            .detach();
        });
}

fn open_probe_window(
    cx: &mut App,
    services: Arc<AppServices>,
    material: WindowMaterial,
    width: f32,
    height: f32,
    visible: f32,
) -> gpui::WindowHandle<RootView> {
    let display = cx.primary_display().expect("display").bounds();
    // A strip `visible` points wide stays on screen at the right edge, the
    // window's top below the menu bar, so macOS keeps it un-occluded.
    let origin = point(
        display.origin.x + display.size.width - px(visible),
        display.origin.y + px(40.0),
    );
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: size(px(width), px(height)),
            })),
            // The app window's own options (`open_window`), except that it is
            // a panel that neither activates nor takes focus.
            window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
            window_background: crate::root::window_background(material),
            app_id: Some("com.dirijor.diri.photon-probe".into()),
            app_owns_titlebar_drag: true,
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(point(px(20.0), px(17.0))),
            }),
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            ..Default::default()
        },
        move |window, cx| {
            cx.new(|cx| RootView::new(services, false, PreviewScenario::Empty, window, cx))
        },
    )
    .expect("probe window")
}

/// Writes `samples` alternating `x` / `\b \b` to the TTY 150–400 ms apart and
/// returns the hop report.
fn drive(
    tty: &str,
    samples: usize,
    warmup: Duration,
    log: Option<&std::path::Path>,
) -> String {
    use std::io::Write;
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open(tty)
        .expect("open tty");
    std::thread::sleep(warmup);
    latency_trace::drain();
    let mut writes = Vec::with_capacity(samples);
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    for index in 0..samples {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_millis(150 + seed % 250));
        let bytes: &[u8] = if index % 2 == 0 { b"x" } else { b"\x08 \x08" };
        let at = Instant::now();
        let ns = uptime_ns();
        latency_trace::mark_at(Hop::KeyDown, at);
        // Output nobody typed for: no input frame, no socket write.
        latency_trace::mark_at(Hop::InputQueued, at);
        latency_trace::mark_at(Hop::SocketWritten, at);
        tty.write_all(bytes).expect("write tty");
        writes.push((index, ns, at));
    }
    std::thread::sleep(Duration::from_millis(500));
    let records = latency_trace::drain();
    if let Some(path) = log {
        let mut out = String::from("i,kind,write_ns");
        for hop in &HOPS[3..] {
            out.push(',');
            out.push_str(&hop.label().replace(' ', "_"));
        }
        out.push('\n');
        for (index, ns, at) in &writes {
            let record = records.iter().find(|record| record[0] == Some(*at));
            out.push_str(&format!(
                "{index},{},{ns}",
                if index % 2 == 0 { "x" } else { "erase" }
            ));
            for hop in &HOPS[3..] {
                out.push(',');
                if let Some(stamp) = record.and_then(|record| record[*hop as usize]) {
                    out.push_str(&format!(
                        "{:.3}",
                        stamp.saturating_duration_since(*at).as_secs_f64() * 1000.0
                    ));
                }
            }
            out.push('\n');
        }
        std::fs::write(path, out).expect("write log");
    }
    format!(
        "{}\n(writes {}, finished records {})",
        latency_trace::render(&records),
        writes.len(),
        records.len()
    )
}
