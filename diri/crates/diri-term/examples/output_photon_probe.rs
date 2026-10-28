//! Output-to-photon probe: one byte written to a terminal's TTY, timed hop by
//! hop until the compositor reports the frame that shows it on screen.
//!
//! Everything on the path is production code except the app chrome: a
//! private in-process Engine, a real Holder (`DIRI_HOLDER_BIN`, otherwise the
//! direct PTY path), the data-channel grid diffs, `TerminalElement`, the
//! vendored GPUI window with its `CVDisplayLink`, and Metal presentation.
//! The child is `cat` with echo off, so the only output is what the probe
//! writes to its TTY from outside, the way a background program prints.
//!
//! The window is a non-activating panel that never takes focus and stays out
//! of the Dock. It is placed almost entirely off screen (a few points stay
//! visible so macOS keeps it un-occluded and the display link running).
//!
//! ```sh
//! cargo build --release -p diri-engine --bin diri-holder
//! DIRI_HOLDER_BIN=$CARGO_TARGET_DIR/release/diri-holder \
//!     cargo run --release -p diri-term --example output_photon_probe -- \
//!     --samples 200 --frames link
//! ```
//!
//! - `--frames link` draws on the next display-link tick after a grid lands
//!   (`cx.notify()` alone); `--frames immediate` also asks for an immediate
//!   frame, as the terminal pane does for output in an idle window.
//! - `--serve <seconds>` writes nothing itself: it prints the TTY path and
//!   waits, so an external capture harness can write to the TTY and film the
//!   window.
//!
//! Cleanup: the session is killed, this run's Holders (matched by the unique
//! temp root) are stopped, and the root removed.

// objc 0.2's macros test a cfg (`cargo-clippy`) newer toolchains do not declare.
#![allow(unexpected_cfgs)]

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::grid::GridUpdate;
use diri_term::buffer::GridBuffer;
use diri_term::element::TerminalElement;
use gpui::{
    App, AppContext, Bounds, Context, FrameStage, IntoElement, ParentElement, Render, Styled,
    Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px,
    size,
};

const COLS: u16 = 60;
const ROWS: u16 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stamp {
    Decoded,
    Applied,
    Draw(FrameStage),
}

static STAMPS: Mutex<Vec<(Stamp, Instant)>> = Mutex::new(Vec::new());

fn stamp_at(stamp: Stamp, at: Instant) {
    if let Ok(mut stamps) = STAMPS.lock()
        && stamps.len() < 1 << 20
    {
        stamps.push((stamp, at));
    }
}

fn stamp(stamp: Stamp) {
    stamp_at(stamp, Instant::now());
}

struct Engine {
    root: PathBuf,
    socket: PathBuf,
    control: UnixStream,
    _server: Arc<ControlServer>,
}

impl Engine {
    fn start() -> Self {
        let dir = diri_engine::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load manifests");
        // Short root: Holder sockets live under it and must fit SUN_LEN.
        let root = PathBuf::from(format!("/tmp/diri-photon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let registry = Arc::new(Mutex::new(Registry::new(
            Arc::new(engine),
            root.join("state.json"),
        )));
        let mut server = ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"));
        if let Some(holder) = std::env::var_os("DIRI_HOLDER_BIN") {
            server = server.with_holder(HolderConfig {
                holders_dir: root.join("holders"),
                executable: PathBuf::from(holder),
            });
        } else {
            eprintln!("probe: DIRI_HOLDER_BIN unset, using the direct PTY path");
        }
        let server = Arc::new(server);
        let listener = server.bind().expect("bind");
        {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    let server = Arc::clone(&server);
                    std::thread::spawn(move || {
                        let _ = server.serve(stream);
                    });
                }
            });
        }
        let socket = server.socket_path().to_path_buf();
        let control = UnixStream::connect(&socket).expect("connect control");
        Self {
            root,
            socket,
            control,
            _server: server,
        }
    }

    fn spawn_cat(&mut self) -> (String, PathBuf) {
        let tty_file = self.root.join("tty");
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            format!(
                "tty > '{}'; stty -echo; printf 'ready\\n'; exec cat",
                tty_file.display()
            ),
        ];
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id: 1,
            method: "session.spawn".into(),
            params: Some(
                serde_json::json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": argv }),
            ),
        })
        .expect("encode");
        bytes.push(b'\n');
        self.control.write_all(&bytes).expect("spawn");
        let mut replies = std::io::BufReader::new(self.control.try_clone().expect("clone"));
        let id = loop {
            let mut line = String::new();
            replies.read_line(&mut line).expect("spawn reply");
            if let Ok(ControlMessage::Response { result, .. }) = serde_json::from_str(&line) {
                break result.expect("spawn")["id"]
                    .as_str()
                    .expect("id")
                    .to_string();
            }
        };
        (id, tty_file)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(self.root.join("holders"))
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Reads grid frames off the data channel on its own thread, like the
/// client's transport task, and hands each one to the window.
fn read_grids(mut data: UnixStream, sink: tokio::sync::mpsc::UnboundedSender<GridUpdate>) {
    std::thread::spawn(move || {
        let mut codec = FrameCodec::new();
        let mut chunk = vec![0u8; 256 << 10];
        loop {
            let Ok(count) = data.read(&mut chunk) else {
                return;
            };
            if count == 0 {
                return;
            }
            for frame in codec.feed(&chunk[..count]).expect("valid frames") {
                if frame.frame_type == FrameType::Grid
                    && let Ok(Some(update)) = frame.grid_payload()
                {
                    stamp(Stamp::Decoded);
                    if sink.send(update).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

struct Probe {
    terminal: TerminalElement,
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(gpui::rgba(0x101010e0))
            .child(self.terminal.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frames {
    Link,
    Immediate,
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * q).round() as usize]
}

fn report(label: &str, mut samples: Vec<f64>) {
    if samples.is_empty() {
        println!("  {label:<32} no samples");
        return;
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "  {label:<32} p50 {:>7.2} ms  p90 {:>7.2} ms  min {:>6.2}  max {:>7.2}  (n={})",
        percentile(&samples, 0.5),
        percentile(&samples, 0.9),
        samples[0],
        percentile(&samples, 1.0),
        samples.len()
    );
}

fn first_after(stamps: &[(Stamp, Instant)], kind: Stamp, after: Instant) -> Option<Instant> {
    stamps
        .iter()
        .filter(|(stamp, at)| *stamp == kind && *at >= after)
        .map(|(_, at)| *at)
        .min()
}

fn ms(from: Instant, to: Instant) -> f64 {
    to.saturating_duration_since(from).as_secs_f64() * 1000.0
}

/// Writes `samples` bytes to the TTY at typing-like pauses and prints each
/// hop's distribution.
fn drive(tty: String, samples: usize, label: &'static str) {
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open(&tty)
        .expect("open tty");
    std::thread::sleep(Duration::from_millis(1500));
    let mut seed = 0x2545_f491_u64;
    let mut rows: Vec<[Option<Instant>; 7]> = Vec::with_capacity(samples);
    for index in 0..samples {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_millis(150 + seed % 250));
        STAMPS.lock().unwrap().clear();
        // `x`, then erase it, so the line never fills.
        let bytes: &[u8] = if index % 2 == 0 { b"x" } else { b"\x08 \x08" };
        let sent = Instant::now();
        terminal.write_all(bytes).expect("tty write");
        std::thread::sleep(Duration::from_millis(120));
        let stamps = STAMPS.lock().unwrap().clone();
        let decoded = first_after(&stamps, Stamp::Decoded, sent);
        let applied = decoded.and_then(|at| first_after(&stamps, Stamp::Applied, at));
        let draw =
            applied.and_then(|at| first_after(&stamps, Stamp::Draw(FrameStage::DrawStart), at));
        let drawn = draw.and_then(|at| first_after(&stamps, Stamp::Draw(FrameStage::DrawEnd), at));
        let committed =
            drawn.and_then(|at| first_after(&stamps, Stamp::Draw(FrameStage::Committed), at));
        let presented =
            committed.and_then(|at| first_after(&stamps, Stamp::Draw(FrameStage::Presented), at));
        rows.push([
            Some(sent),
            decoded,
            applied,
            draw,
            drawn,
            committed,
            presented,
        ]);
    }
    let labels = [
        "tty write -> grid decoded",
        "decoded -> applied (GPUI thread)",
        "applied -> draw start",
        "draw start -> draw end",
        "draw end -> committed",
        "committed -> presented",
    ];
    println!(
        "\noutput-to-photon probe, frames={label}, holder={}",
        std::env::var_os("DIRI_HOLDER_BIN").is_some()
    );
    for (hop, name) in labels.iter().enumerate() {
        report(
            name,
            rows.iter()
                .filter_map(|row| Some(ms(row[hop]?, row[hop + 1]?)))
                .collect(),
        );
    }
    report(
        "TOTAL tty write -> presented",
        rows.iter()
            .filter_map(|row| Some(ms(row[0]?, row[6]?)))
            .collect(),
    );
    report(
        "     tty write -> committed",
        rows.iter()
            .filter_map(|row| Some(ms(row[0]?, row[5]?)))
            .collect(),
    );
    for (parity, kind) in [(0, "x"), (1, "erase")] {
        report(
            &format!("  {kind}: tty write -> decoded"),
            rows.iter()
                .enumerate()
                .filter(|(index, _)| index % 2 == parity)
                .filter_map(|(_, row)| Some(ms(row[0]?, row[1]?)))
                .collect(),
        );
        report(
            &format!("  {kind}: tty write -> presented"),
            rows.iter()
                .enumerate()
                .filter(|(index, _)| index % 2 == parity)
                .filter_map(|(_, row)| Some(ms(row[0]?, row[6]?)))
                .collect(),
        );
    }
    let missing = rows.iter().filter(|row| row[6].is_none()).count();
    println!("  samples without a presented frame: {missing}");
}

/// Streams output faster than the display for `seconds` and reports how
/// committed frames reached the screen: how many were never presented, and
/// the spacing of those that were.
fn stream(tty: String, seconds: u64, label: &'static str) {
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open(&tty)
        .expect("open tty");
    std::thread::sleep(Duration::from_millis(1500));
    STAMPS.lock().unwrap().clear();
    let started = Instant::now();
    let mut count = 0u64;
    while started.elapsed() < Duration::from_secs(seconds) {
        count += 1;
        let bytes: &[u8] = if count.is_multiple_of(50) {
            b"\r\n"
        } else {
            b"x"
        };
        terminal.write_all(bytes).expect("tty write");
        std::thread::sleep(Duration::from_millis(3));
    }
    std::thread::sleep(Duration::from_millis(200));
    let stamps = STAMPS.lock().unwrap().clone();
    let committed = stamps
        .iter()
        .filter(|(stamp, _)| *stamp == Stamp::Draw(FrameStage::Committed))
        .count();
    let mut presented: Vec<Instant> = stamps
        .iter()
        .filter(|(stamp, _)| *stamp == Stamp::Draw(FrameStage::Presented))
        .map(|(_, at)| *at)
        .collect();
    presented.sort();
    let intervals: Vec<f64> = presented
        .windows(2)
        .map(|pair| ms(pair[0], pair[1]))
        .collect();
    let refresh = 1000.0 / 120.0;
    let doubled = intervals.iter().filter(|gap| **gap > refresh * 1.5).count();
    println!(
        "\nstream {seconds}s frames={label}: committed {committed}, presented {}, never presented {}, presented gaps > 1.5 refresh: {doubled} of {}",
        presented.len(),
        committed.saturating_sub(presented.len()),
        intervals.len()
    );
    report("presented interval", intervals);
}

/// Accessory activation policy: no Dock icon, never activated.
#[cfg(target_os = "macos")]
fn stay_out_of_the_dock() {
    use objc::{class, msg_send, sel, sel_impl};
    // SAFETY: plain AppKit messages to the shared application on the main
    // thread. 1 = NSApplicationActivationPolicyAccessory.
    unsafe {
        let app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setActivationPolicy: 1_i64];
    }
}

fn main() {
    let frames = match arg("--frames").as_deref() {
        Some("immediate") => Frames::Immediate,
        _ => Frames::Link,
    };
    let samples: usize = arg("--samples")
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let serve: Option<u64> = arg("--serve").and_then(|value| value.parse().ok());

    let mut engine = Engine::start();
    let (id, tty_file) = engine.spawn_cat();
    let mut data = UnixStream::connect(&engine.socket).expect("connect data");
    let mut attach = serde_json::to_vec(&serde_json::json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");
    data.write_all(&FrameCodec::encode(&Frame::resize(COLS, ROWS)).expect("resize"))
        .expect("resize");
    let deadline = Instant::now() + Duration::from_secs(10);
    let tty = loop {
        if let Ok(path) = std::fs::read_to_string(&tty_file)
            && path.trim().starts_with("/dev/")
        {
            break path.trim().to_string();
        }
        assert!(Instant::now() < deadline, "child never reported its tty");
        std::thread::sleep(Duration::from_millis(20));
    };
    let (sink, mut grids) = tokio::sync::mpsc::unbounded_channel();
    read_grids(data, sink);
    gpui::set_frame_observer(|stage, at| stamp_at(Stamp::Draw(stage), at));

    let engine = Arc::new(Mutex::new(Some(engine)));
    gpui_platform::application().run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        stay_out_of_the_dock();
        let display = cx.primary_display().expect("display").bounds();
        // A few points stay on screen so the window counts as visible.
        let origin = point(
            display.origin.x + display.size.width
                - px(if std::env::args().any(|arg| arg == "--onscreen") {
                    490.0
                } else {
                    6.0
                }),
            display.origin.y + display.size.height - px(120.0),
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: size(px(480.0), px(90.0)),
            })),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            window_background: if std::env::args().any(|arg| arg == "--blur") {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Opaque
            },
            ..Default::default()
        };
        let handle = cx
            .open_window(options, |_, cx| {
                cx.new(|_| Probe {
                    terminal: TerminalElement::with_buffer(GridBuffer::default())
                        .font_size(px(16.0)),
                })
            })
            .expect("window");
        cx.spawn(async move |cx| {
            while let Some(update) = grids.recv().await {
                let result = handle.update(cx, |probe, window, cx| {
                    if probe.terminal.apply_damage(update).changed {
                        stamp(Stamp::Applied);
                        cx.notify();
                        if frames == Frames::Immediate {
                            window.request_immediate_frame();
                        }
                    }
                });
                if result.is_err() {
                    return;
                }
            }
        })
        .detach();

        let engine = Arc::clone(&engine);
        let label = if frames == Frames::Immediate {
            "immediate"
        } else {
            "link"
        };
        cx.spawn(async move |cx| {
            let done = cx.background_executor().spawn(async move {
                match (serve, arg("--stream").and_then(|value| value.parse().ok())) {
                    (_, Some(seconds)) => stream(tty, seconds, label),
                    (Some(seconds), None) => {
                        println!("probe: tty={tty} pid={}", std::process::id());
                        std::thread::sleep(Duration::from_secs(seconds));
                    }
                    (None, None) => drive(tty, samples, label),
                }
            });
            done.await;
            drop(engine.lock().unwrap().take());
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
