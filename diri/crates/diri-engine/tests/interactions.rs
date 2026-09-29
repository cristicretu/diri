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

/// A process's CPU time (user+sys) as `ps` reports it, at its 10 ms grain.
fn process_cpu(pid: &str) -> Duration {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "time=", "-p", pid])
        .output()
        .expect("ps");
    let text = String::from_utf8_lossy(&output.stdout);
    // `[[dd-]hh:]mm:ss[.ff]`; days never occur in a measurement.
    let seconds = text.trim().split(':').fold(0.0, |total, part| {
        total * 60.0 + part.parse::<f64>().unwrap_or(0.0)
    });
    Duration::from_secs_f64(seconds)
}

struct PasteResult {
    /// Until the program had read every byte, or `None` if it never did.
    consumed: Option<Duration>,
    /// Whether what the program read equals what was pasted, byte for byte.
    exact: bool,
    echo_p50: Duration,
    echo_max: Duration,
    echoes: usize,
    engine_cpu: Duration,
    holder_cpu: Duration,
}

/// A large paste into a program that reads it all, while another session is
/// typed into: time until the program has consumed every byte, whether it
/// read exactly what was pasted, the other session's echo latency meanwhile,
/// and the CPU the Engine (this process) and the Holder manager spent.
fn paste(tag: &str, bytes: usize, busy_ms: u32) -> PasteResult {
    let mut harness = Harness::new(tag);
    let received = harness.root.join("received");
    // `busy_ms` models a program that is busy when the paste lands (an agent
    // mid-turn) and reads its input only afterwards.
    let reader = harness.spawn(&format!(
        "stty raw -echo; printf 'waiting\\n'; sleep {}; head -c {} > '{}'; printf 'PASTED\\n'; exec cat",
        f64::from(busy_ms) / 1000.0,
        bytes,
        received.display()
    ));
    let typed = harness.spawn("printf 'ready\\n'; exec cat");
    let reader_data = harness.attach(&reader);
    // Drained so the attach never backs up; its frames are not measured.
    let _reader_watch = watch(reader_data.try_clone().unwrap());
    let typed_data = harness.attach(&typed);
    let typed_watch = watch(typed_data.try_clone().unwrap());
    std::thread::sleep(Duration::from_millis(500));
    // Holders are threads of one manager process; its pid is in every
    // session's pid file once that session's Holder runs.
    let holder_pid = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = std::fs::read_dir(harness.root.join("holders"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pid"))
                .find_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .map(|pid| pid.trim().to_owned())
                .filter(|pid| !pid.is_empty());
            if let Some(pid) = found {
                break pid;
            }
            assert!(Instant::now() < deadline, "no Holder pid file");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let echoes = echo_probe(&typed_data, &typed_watch, &stop, Duration::from_millis(10));
    std::thread::sleep(Duration::from_millis(100));
    let mut text = Vec::with_capacity(bytes);
    text.extend_from_slice(b"\x1b[200~");
    let mut line = 0_usize;
    while text.len() < bytes - 6 {
        line += 1;
        text.extend_from_slice(format!("pasted line {line} of a large clipboard\n").as_bytes());
    }
    text.truncate(bytes - 6);
    text.extend_from_slice(b"\x1b[201~");
    let engine_before = cpu_time();
    let holder_before = process_cpu(&holder_pid);
    let started = Instant::now();
    let mut writer = reader_data.try_clone().unwrap();
    writer
        .write_all(&FrameCodec::encode(&Frame::input(text.clone())).unwrap())
        .unwrap();
    // A paste that has not fully arrived after twenty seconds was lost.
    let deadline = Instant::now() + Duration::from_secs(20) + Duration::from_millis(busy_ms.into());
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
    let engine_cpu = cpu_time() - engine_before;
    let holder_cpu = process_cpu(&holder_pid).saturating_sub(holder_before);
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::SeqCst);
    let mut echo = echoes.join().expect("echo thread");
    let exact = std::fs::read(&received).is_ok_and(|read| read == text);
    for id in [&reader, &typed] {
        harness.request("session.kill", json!({ "sessionID": id }));
    }
    let echoes = echo.len();
    PasteResult {
        consumed,
        exact,
        echo_p50: percentile(&mut echo, 0.5),
        echo_max: percentile(&mut echo, 1.0),
        echoes,
        engine_cpu,
        holder_cpu,
    }
}

/// Opt-in measurement: `cargo test --release -p diri-engine --test
/// interactions -- --ignored --nocapture --test-threads=1 paste`.
/// `DIRI_PASTE_CASE=<bytes>,<busy_ms>` runs one case, e.g. `102400,1500`
/// for a program that reads its input only after 1.5 s. Sizes include the
/// bracketed-paste markers; one attach frame carries at most 16 MiB.
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
    let runs = std::env::var("DIRI_PASTE_RUNS").map_or(3, |runs| runs.parse().unwrap());
    for (bytes, busy_ms) in cases {
        for run in 0..runs {
            let result = paste(&format!("paste{run}"), bytes, busy_ms);
            eprintln!(
                "paste {} KiB into a program busy {busy_ms} ms, run {run}: {}; exact {}; other-session echo p50 {:?} max {:?} (n={}); engine cpu {:?}, holder cpu {:?}",
                bytes >> 10,
                result
                    .consumed
                    .map_or("LOST (never fully delivered)".to_string(), |at| format!(
                        "consumed in {at:?}"
                    )),
                result.exact,
                result.echo_p50,
                result.echo_max,
                result.echoes,
                result.engine_cpu,
                result.holder_cpu,
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

/// Printable text with newlines and tabs, different at every offset, so a
/// dropped, duplicated or reordered chunk cannot compare equal.
fn paste_payload(size: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..size)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            match index % 97 {
                96 => b'\n',
                48 => b'\t',
                _ => b' ' + (state % 95) as u8,
            }
        })
        .collect()
}

/// A paste into a program that is busy when it lands and reads it seconds
/// later, through the desktop's attach channel. On `main` the Holder gave up
/// after one second of a full PTY, the Engine dropped the attach and the
/// unread tail was lost, and for that second every other session's input
/// waited on the Registry lock the write held.
#[test]
fn a_paste_into_a_busy_program_arrives_whole_without_stalling_other_sessions() {
    let mut harness = Harness::new("busypaste");
    let received = harness.root.join("received");
    let paste = paste_payload(4 << 20);
    let tail = b"<typed after the paste>";
    let reader = harness.spawn(&format!(
        "stty raw -echo; printf 'busy\\n'; sleep 3; head -c {} > '{}'; printf 'PASTED\\n'; exec cat",
        paste.len() + tail.len(),
        received.display()
    ));
    let typed = harness.spawn("printf 'ready\\n'; exec cat");
    let reader_data = harness.attach(&reader);
    let _reader_watch = watch(reader_data.try_clone().unwrap());
    let typed_data = harness.attach(&typed);
    let typed_watch = watch(typed_data.try_clone().unwrap());
    let screen_has = |harness: &mut Harness, id: &str, text: &str| {
        harness.request("session.read_screen", json!({ "sessionID": id }))["text"]
            .as_str()
            .is_some_and(|screen| screen.contains(text))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_has(&mut harness, &reader, "busy") || !screen_has(&mut harness, &typed, "ready") {
        assert!(Instant::now() < deadline, "sessions never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let echoes = echo_probe(&typed_data, &typed_watch, &stop, Duration::from_millis(10));
    std::thread::sleep(Duration::from_millis(100));
    let mut writer = reader_data.try_clone().unwrap();
    writer
        .write_all(&FrameCodec::encode(&Frame::input(paste.clone())).unwrap())
        .unwrap();
    writer
        .write_all(&FrameCodec::encode(&Frame::input(tail.to_vec())).unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    while !screen_has(&mut harness, &reader, "PASTED") {
        assert!(
            Instant::now() < deadline,
            "the paste never fully arrived: {} of {} bytes",
            std::fs::metadata(&received).map_or(0, |meta| meta.len()),
            paste.len() + tail.len()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::SeqCst);
    let mut echo = echoes.join().expect("echo thread");
    for id in [&reader, &typed] {
        harness.request("session.kill", json!({ "sessionID": id }));
    }

    let mut expected = paste;
    expected.extend_from_slice(tail);
    let received = std::fs::read(&received).expect("received");
    assert_eq!(received.len(), expected.len(), "every byte arrived once");
    assert!(received == expected, "byte for byte, and in order");
    let worst = percentile(&mut echo, 1.0);
    assert!(
        worst < Duration::from_millis(500),
        "another session's echo waited {worst:?} behind the paste (n={})",
        echo.len()
    );
}
