//! Terminal interactions over real held sessions through the binary attach
//! channel the desktop uses.
//!
//! Drag: a live window/split drag over 10,000 rows of wrapped, coloured
//! history. The client paces one Resize frame per 8 ms (120 Hz), exactly as the
//! desktop does; each resize reflows the Engine's terminal mirror.
//!
//! Measures, per drag:
//! - settle: time from the last Resize frame to the first grid published at
//!   that final size (what the user waits for when the mouse stops);
//! - a second session's keystroke echo latency while the drag runs (the
//!   Engine must not stall unrelated terminals behind one reflow);
//! - the Engine's CPU time (this process: user+sys) across the drag.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::json;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

struct Harness {
    root: PathBuf,
    control: UnixStream,
    server: Arc<ControlServer>,
    next_id: u64,
}

impl Harness {
    fn new(tag: &str) -> Self {
        // Short root: Holder sockets live under it and must fit SUN_LEN.
        let root = PathBuf::from(format!("/tmp/diri-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let registry = Arc::new(Mutex::new(Registry::new(engine(), root.join("state.json"))));
        let server = Arc::new(
            ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
                .with_logs_dir(root.join("logs"))
                .with_holder(diri_engine::session::HolderConfig {
                    holders_dir: root.join("holders"),
                    executable: env!("CARGO_BIN_EXE_diri-holder").into(),
                }),
        );
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
        let control = UnixStream::connect(server.socket_path()).expect("connect control");
        Self {
            root,
            control,
            server,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::io::BufRead;
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .expect("encode");
        bytes.push(b'\n');
        self.control.write_all(&bytes).expect("write");
        let mut reader = std::io::BufReader::new(self.control.try_clone().expect("clone"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("reply");
            if let Ok(ControlMessage::Response {
                id: reply, result, ..
            }) = serde_json::from_str(&line)
                && reply == id
            {
                return result.unwrap_or_else(|error| panic!("{method} failed: {error:?}"));
            }
        }
    }

    fn spawn(&mut self, script: &str) -> String {
        self.request(
            "session.spawn",
            json!({
                "kind": { "shell": {} },
                "cwd": "/tmp",
                "argv": ["/bin/sh", "-c", script],
                "initialCols": 160,
                "initialRows": 50,
            }),
        )["id"]
            .as_str()
            .expect("id")
            .to_string()
    }

    fn attach(&self, id: &str) -> UnixStream {
        let mut data = UnixStream::connect(self.server.socket_path()).expect("connect data");
        let mut line = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
        line.push(b'\n');
        data.write_all(&line).expect("attach");
        data
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Reads grid frames on a thread, recording when each (cols, rows) first
/// arrived and the latest cursor column.
type SizeLog = Arc<Mutex<Vec<((u16, u16), Instant)>>>;

struct GridWatch {
    sizes: SizeLog,
    cursor: Arc<Mutex<Option<(u16, Instant)>>>,
}

fn watch(stream: UnixStream) -> GridWatch {
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let cursor = Arc::new(Mutex::new(None));
    {
        let sizes = Arc::clone(&sizes);
        let cursor = Arc::clone(&cursor);
        let mut stream = stream;
        std::thread::spawn(move || {
            let mut codec = FrameCodec::new();
            let mut chunk = vec![0u8; 256 << 10];
            while let Ok(count) = stream.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                let Ok(frames) = codec.feed(&chunk[..count]) else {
                    break;
                };
                let now = Instant::now();
                for frame in frames {
                    if frame.frame_type != FrameType::Grid {
                        continue;
                    }
                    if let Ok(Some(update)) = frame.grid_payload() {
                        sizes
                            .lock()
                            .unwrap()
                            .push(((update.cols, update.rows), now));
                        *cursor.lock().unwrap() = Some((update.cursor_col, now));
                    }
                }
            }
        });
    }
    GridWatch { sizes, cursor }
}

fn history_file(root: &Path) -> PathBuf {
    let path = root.join("history.log");
    let mut out = Vec::with_capacity(10_000 * 120);
    for line in 0..10_000usize {
        let color = 31 + line % 6;
        let module = "x".repeat(10 + line % 50);
        let tail = if line.is_multiple_of(4) {
            " with an extra long explanation that wraps on narrow panes and splits"
        } else {
            ""
        };
        out.extend_from_slice(
            format!(
                "\x1b[{color}m[{line:010}] building crate_{} v0.{}.0\x1b[0m  Compiling module {module}{tail}\r\n",
                line % 997,
                line % 9
            )
            .as_bytes(),
        );
    }
    std::fs::write(&path, out).expect("history");
    path
}

/// Types into a quiet `cat` session until `stop`, one key per `interval`,
/// and returns each key's echo latency: the time until the next grid frame.
fn echo_probe(
    data: &UnixStream,
    watch: &GridWatch,
    stop: &Arc<AtomicBool>,
    interval: Duration,
) -> std::thread::JoinHandle<Vec<Duration>> {
    let stop = Arc::clone(stop);
    let cursor = Arc::clone(&watch.cursor);
    let mut data = data.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut latencies = Vec::new();
        let mut keys = 0;
        while !stop.load(Ordering::SeqCst) {
            let sent = Instant::now();
            data.write_all(&FrameCodec::encode(&Frame::input(b"k".to_vec())).unwrap())
                .unwrap();
            let deadline = sent + Duration::from_secs(60);
            // The session prints nothing else: any grid after the key is
            // its echo.
            while !cursor.lock().unwrap().is_some_and(|(_, at)| at >= sent) {
                assert!(Instant::now() < deadline, "echo never arrived");
                std::thread::sleep(Duration::from_micros(200));
            }
            latencies.push(cursor.lock().unwrap().expect("echo").1 - sent);
            keys += 1;
            if keys % 60 == 0 {
                data.write_all(&FrameCodec::encode(&Frame::input(b"\r".to_vec())).unwrap())
                    .unwrap();
                std::thread::sleep(Duration::from_millis(30));
            }
            std::thread::sleep(interval);
        }
        latencies
    })
}

fn cpu_time() -> Duration {
    // SAFETY: getrusage writes into the zeroed struct we pass.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let tv = |tv: libc::timeval| {
        Duration::from_secs(tv.tv_sec as u64) + Duration::from_micros(tv.tv_usec as u64)
    };
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

fn percentile(samples: &mut [Duration], q: f64) -> Duration {
    samples.sort_unstable();
    samples[((samples.len() as f64 - 1.0) * q).round() as usize]
}

struct DragResult {
    settle: Duration,
    echo_p50: Duration,
    echo_p95: Duration,
    echo_max: Duration,
    cpu: Duration,
    published_sizes: usize,
}

fn drag(tag: &str) -> DragResult {
    let mut harness = Harness::new(tag);
    let history = history_file(&harness.root);
    let dragged = harness.spawn(&format!("cat '{}'; exec cat", history.display()));
    let typed = harness.spawn("printf 'ready\\n'; exec cat");

    let mut drag_data = harness.attach(&dragged);
    drag_data
        .write_all(&FrameCodec::encode(&Frame::resize(160, 50)).unwrap())
        .unwrap();
    let dragged_watch = watch(drag_data.try_clone().unwrap());
    let mut typed_data = harness.attach(&typed);
    typed_data
        .write_all(&FrameCodec::encode(&Frame::resize(100, 30)).unwrap())
        .unwrap();
    let typed_watch = watch(typed_data.try_clone().unwrap());

    // Wait until all history has been parsed: the last line is on screen.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let screen = harness.request("session.read_screen", json!({ "sessionID": dragged }));
        if screen["text"]
            .as_str()
            .is_some_and(|text| text.contains("[0000009999]"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "history never finished");
    }
    std::thread::sleep(Duration::from_millis(300));

    // Echo probe: a key every 20 ms into the other session while dragging.
    let stop = Arc::new(AtomicBool::new(false));
    let echoes = echo_probe(&typed_data, &typed_watch, &stop, Duration::from_millis(20));

    let cpu_before = cpu_time();
    let started = Instant::now();
    let mut steps = Vec::new();
    for _ in 0..2 {
        steps.extend((0..40u16).map(|step| (160 - step, 50)));
        steps.extend((0..40u16).map(|step| (120 + step, 50)));
    }
    steps.push((150, 49));
    let mut last_sent = started;
    for (index, (cols, rows)) in steps.iter().enumerate() {
        let due = started + Duration::from_millis(8) * index as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        last_sent = Instant::now();
        drag_data
            .write_all(&FrameCodec::encode(&Frame::resize(*cols, *rows)).unwrap())
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let settle = loop {
        if let Some((_, at)) = dragged_watch
            .sizes
            .lock()
            .unwrap()
            .iter()
            .find(|(size, at)| *size == (150, 49) && *at >= last_sent)
        {
            break *at - last_sent;
        }
        assert!(Instant::now() < deadline, "final size never published");
        std::thread::sleep(Duration::from_micros(200));
    };
    let cpu = cpu_time() - cpu_before;
    stop.store(true, Ordering::SeqCst);
    let mut echo = echoes.join().expect("echo thread");
    let published_sizes = dragged_watch
        .sizes
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, at)| *at >= started)
        .count();
    for id in [&dragged, &typed] {
        harness.request("session.kill", json!({ "sessionID": id }));
    }
    DragResult {
        settle,
        echo_p50: percentile(&mut echo, 0.5),
        echo_p95: percentile(&mut echo, 0.95),
        echo_max: percentile(&mut echo, 1.0),
        cpu,
        published_sizes,
    }
}

/// Opt-in measurement: `cargo test --release -p diri-engine --test
/// interactions -- --ignored --nocapture --test-threads=1 drag`.
#[test]
#[ignore = "measurement; run explicitly"]
fn drag_resize_measurement() {
    for run in 0..3 {
        let result = drag(&format!("drag{run}"));
        eprintln!(
            "drag run {run}: settle {:?}, other-session echo p50 {:?} p95 {:?} max {:?}, engine cpu {:?}, grids published {}",
            result.settle,
            result.echo_p50,
            result.echo_p95,
            result.echo_max,
            result.cpu,
            result.published_sizes,
        );
    }
}

/// A large paste into a program that reads it all, while another session is
/// typed into: time until the program has consumed every byte, and the other
/// session's echo latency meanwhile.
fn paste(tag: &str, bytes: usize, busy_ms: u32) -> (Option<Duration>, Duration, Duration, usize) {
    let mut harness = Harness::new(tag);
    // `busy_ms` models a program that is busy when the paste lands (an agent
    // mid-turn) and reads its input only afterwards.
    let reader = harness.spawn(&format!(
        "stty -echo -icanon; printf 'waiting\\n'; sleep {}; head -c {} > /dev/null; printf 'PASTED\\n'; exec cat",
        f64::from(busy_ms) / 1000.0,
        bytes + 12
    ));
    let typed = harness.spawn("printf 'ready\\n'; exec cat");
    let reader_data = harness.attach(&reader);
    // Drained so the attach never backs up; its frames are not measured.
    let _reader_watch = watch(reader_data.try_clone().unwrap());
    let mut typed_data = harness.attach(&typed);
    let typed_watch = watch(typed_data.try_clone().unwrap());
    std::thread::sleep(Duration::from_millis(500));

    let stop = Arc::new(AtomicBool::new(false));
    let echoes = echo_probe(&typed_data, &typed_watch, &stop, Duration::from_millis(10));
    std::thread::sleep(Duration::from_millis(100));
    let mut text = Vec::with_capacity(bytes + 12);
    text.extend_from_slice(b"\x1b[200~");
    while text.len() < bytes + 6 {
        text.extend_from_slice(b"pasted line of text for a large clipboard\n");
    }
    text.truncate(bytes + 6);
    text.extend_from_slice(b"\x1b[201~");
    let started = Instant::now();
    let mut writer = reader_data.try_clone().unwrap();
    writer
        .write_all(&FrameCodec::encode(&Frame::input(text)).unwrap())
        .unwrap();
    // A paste that has not fully arrived after ten seconds was lost.
    let deadline = Instant::now() + Duration::from_secs(10) + Duration::from_millis(busy_ms.into());
    let consumed = loop {
        let screen = harness.request("session.read_screen", json!({ "sessionID": reader }));
        if screen["text"]
            .as_str()
            .is_some_and(|text| text.contains("PASTED"))
        {
            break Some(started.elapsed());
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::SeqCst);
    let mut echo = echoes.join().expect("echo thread");
    let _ = typed_data.flush();
    for id in [&reader, &typed] {
        harness.request("session.kill", json!({ "sessionID": id }));
    }
    let count = echo.len();
    (
        consumed,
        percentile(&mut echo, 0.5),
        percentile(&mut echo, 1.0),
        count,
    )
}

/// Opt-in measurement: `cargo test --release -p diri-engine --test
/// interactions -- --ignored --nocapture --test-threads=1 paste`.
/// `DIRI_PASTE_CASE=<bytes>,<busy_ms>` runs one case, e.g. `102400,1500`
/// for a program that reads its input only after 1.5 s.
#[test]
#[ignore = "measurement; run explicitly"]
fn paste_measurement() {
    let cases: Vec<(usize, u32)> = match std::env::var("DIRI_PASTE_CASE") {
        Ok(case) => {
            let (bytes, busy) = case.split_once(',').expect("bytes,busy_ms");
            vec![(bytes.parse().unwrap(), busy.parse().unwrap())]
        }
        Err(_) => vec![(100 << 10, 0), (1 << 20, 0)],
    };
    for (bytes, busy_ms) in cases {
        for run in 0..3 {
            let (consumed, p50, max, count) = paste(&format!("paste{run}"), bytes, busy_ms);
            eprintln!(
                "paste {} KiB into a program busy {busy_ms} ms, run {run}: {}; other-session echo p50 {p50:?} max {max:?} (n={count})",
                bytes >> 10,
                consumed.map_or("LOST (never fully delivered)".to_string(), |at| format!(
                    "consumed in {at:?}"
                )),
            );
        }
    }
}

/// Scrolling history and switching to a session over the real control and
/// attach sockets, against 10,000 rows of coloured history: the round trip
/// of one 50-row `read_scrollback_cells` page as a wheel steps 3 rows, a
/// jump to the top, and the time from an attach request to its seed grid.
#[test]
#[ignore = "measurement; run explicitly"]
fn scroll_and_switch_measurement() {
    let mut harness = Harness::new("scroll");
    let history = history_file(&harness.root);
    let id = harness.spawn(&format!("cat '{}'; exec cat", history.display()));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let screen = harness.request("session.read_screen", json!({ "sessionID": id }));
        if screen["text"]
            .as_str()
            .is_some_and(|text| text.contains("[0000009999]"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "history never finished");
    }
    let page = |harness: &mut Harness, first: i64| {
        harness.request(
            "session.read_scrollback_cells",
            json!({ "sessionID": id, "firstRow": first, "maxRows": 50 }),
        )
    };
    let live = page(&mut harness, 0)["liveStartRow"]
        .as_i64()
        .expect("live start");
    let mut wheel = Vec::new();
    let mut bytes = 0;
    for step in 0..300 {
        let started = Instant::now();
        let result = page(&mut harness, live - 50 - step * 3);
        wheel.push(started.elapsed());
        bytes = result["payload"].as_str().map_or(0, str::len);
    }
    let mut jumps = Vec::new();
    for jump in 0..40 {
        let started = Instant::now();
        page(&mut harness, if jump % 2 == 0 { 0 } else { live - 50 });
        jumps.push(started.elapsed());
    }
    let mut attaches = Vec::new();
    let mut seed_bytes = 0;
    for _ in 0..30 {
        let started = Instant::now();
        let mut data = harness.attach(&id);
        let mut codec = FrameCodec::new();
        let mut chunk = vec![0u8; 256 << 10];
        let mut read = 0;
        'seed: loop {
            let count = data.read(&mut chunk).expect("read");
            assert!(count > 0, "attach closed");
            read += count;
            for frame in codec.feed(&chunk[..count]).expect("frames") {
                if frame.frame_type == FrameType::Grid {
                    attaches.push(started.elapsed());
                    seed_bytes = read;
                    break 'seed;
                }
            }
        }
    }
    harness.request("session.kill", json!({ "sessionID": id }));
    for (label, samples) in [
        ("wheel page round trip", &mut wheel),
        ("jump top/bottom round trip", &mut jumps),
        ("attach to seed grid", &mut attaches),
    ] {
        eprintln!(
            "{label}: p50 {:?} p95 {:?} max {:?} (n={})",
            percentile(samples, 0.5),
            percentile(samples, 0.95),
            percentile(samples, 1.0),
            samples.len()
        );
    }
    eprintln!("page payload {bytes} base64 bytes; seed read {seed_bytes} bytes");
}

/// A shell transcript of `commands` commands, each a prompt row followed by
/// `output` rows of build output, with OSC 133 marks as fish, zsh and bash
/// emit them.
fn prompted_history(root: &Path, commands: usize, output: usize) -> PathBuf {
    let path = root.join("prompted.log");
    let mut out = Vec::new();
    for command in 0..commands {
        out.extend_from_slice(
            format!("\x1b]133;A\x07~/work $ \x1b]133;B\x07make step{command}\r\n\x1b]133;C\x07")
                .as_bytes(),
        );
        for line in 0..output {
            out.extend_from_slice(
                format!(
                    "\x1b[32m[{command:05}:{line:03}]\x1b[0m Compiling crate_{line} v0.1.0\r\n"
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(b"\x1b]133;D;0\x07");
    }
    std::fs::write(&path, out).expect("history");
    path
}

/// Reads the rows a prompt search reads and reports the prompt rows it saw.
/// `pages` is the list of (first, max) requests; each page is decoded the way
/// the desktop decodes it.
fn prompt_rows_in(
    harness: &mut Harness,
    id: &str,
    first: i64,
    max: i64,
) -> (Vec<i64>, i64, i64, i64) {
    use base64::Engine as _;
    let page = harness.request(
        "session.read_scrollback_cells",
        json!({ "sessionID": id, "firstRow": first, "maxRows": max, "styles": true }),
    );
    let count = page["rowCount"].as_i64().expect("rows");
    let payload = base64::engine::general_purpose::STANDARD
        .decode(page["payload"].as_str().expect("payload"))
        .expect("base64");
    let rows = diri_proto::grid::GridRowCodec::decode_rows(&payload, count as usize).expect("rows");
    let prompts = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.iter().any(|cell| {
                cell.style
                    .contains(diri_proto::grid::TermStyle::PROMPT_START)
            })
        })
        .map(|(index, _)| first + index as i64)
        .collect();
    (
        prompts,
        count,
        page["liveStartRow"].as_i64().expect("live"),
        page["totalRows"].as_i64().expect("total"),
    )
}

/// Time to find the previous shell prompt from the live screen over 10,000
/// rows of history, by the two read patterns the desktop has used: reading
/// every retained row from the oldest (the export path), and reading outward
/// from the window, widening pages until the first prompt.
#[test]
#[ignore = "measurement; run explicitly"]
fn prompt_jump_measurement() {
    let mut harness = Harness::new("prompts");
    // Close prompts (every 25 rows) and one far prompt (a 9,000-row build).
    for (label, commands, output) in [("near", 400, 24), ("far", 2, 5_000)] {
        let history = prompted_history(&harness.root, commands, output);
        let id = harness.spawn(&format!("cat '{}'; exec cat", history.display()));
        let last = format!("[{:05}:{:03}]", commands - 1, output - 1);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let screen = harness.request("session.read_screen", json!({ "sessionID": id }));
            if screen["text"]
                .as_str()
                .is_some_and(|text| text.contains(&last))
            {
                break;
            }
            assert!(Instant::now() < deadline, "history never finished");
        }
        let cols = 160_i64;
        let limit = (diri_proto::FIND_CAPTURE_MAX_CELLS as i64 / cols).clamp(1, 1024);
        let mut full = Vec::new();
        let mut outward = Vec::new();
        let mut targets = (None, None);
        for _ in 0..20 {
            // Every row from the oldest, then the newest prompt above the top.
            let started = Instant::now();
            let (mut prompts, _, live, total) = prompt_rows_in(&mut harness, &id, 0, 1);
            let mut first = 1;
            while first < total {
                let (found, count, ..) = prompt_rows_in(&mut harness, &id, first, limit);
                prompts.extend(found);
                if count == 0 {
                    break;
                }
                first += count;
            }
            let target = prompts.into_iter().rev().find(|row| *row < live);
            full.push(started.elapsed());

            // Outward from the window: a probe, then 64, 256, 1024 rows.
            let started = Instant::now();
            let (_, _, live, _) = prompt_rows_in(&mut harness, &id, 0, 1);
            let mut end = live;
            let mut page = 64;
            let mut near = None;
            while end > 0 && near.is_none() {
                let first = (end - page).max(0);
                near = prompt_rows_in(&mut harness, &id, first, end - first)
                    .0
                    .last()
                    .copied();
                end = first;
                page = (page * 4).min(limit);
            }
            outward.push(started.elapsed());
            targets = (target, near);
        }
        assert_eq!(targets.0, targets.1, "both reads must find the same prompt");
        harness.request("session.kill", json!({ "sessionID": id }));
        for (pattern, samples) in [("read all", &mut full), ("outward", &mut outward)] {
            eprintln!(
                "{label} prompt {:?}: {pattern} p50 {:?} p95 {:?} (n={})",
                targets.0,
                percentile(samples, 0.5),
                percentile(samples, 0.95),
                samples.len()
            );
        }
    }
}
