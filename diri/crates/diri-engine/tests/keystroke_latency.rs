//! Keystroke-to-grid latency of a local session, hop by hop.
//!
//! Drives the exact path a pane uses: a control server with a Holder, a data
//! channel attach, one input frame per keystroke, and the grid diff that
//! carries its echo. The engine records when each hop fires (see
//! `diri_engine::latency_trace`); this harness pairs those with the moment it
//! sent the key and the moment the diff arrived.
//!
//! Ignored and feature-gated because it measures rather than asserts:
//!
//! ```sh
//! cargo test -p diri-engine --features latency-trace --test keystroke_latency \
//!     -- --ignored --nocapture
//! ```
//!
//! `DIRI_KEY_LATENCY_CHILD` picks the echoing child: `cat` (the default; the
//! TTY line discipline echoes, so this is pure Diri path) or `zsh` (a real
//! line editor echoing in raw mode). `DIRI_KEY_LATENCY_SAMPLES` sets the key
//! count (default 200). Keys are paced like typing, 40 ms apart, so each one
//! is a lone echo rather than part of a stream; every third is a backspace.
//!
//! Cleanup: the session is killed and the temp dir removed on exit.

#![cfg(unix)]

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::latency_trace::{self, Hop};
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::grid::GridUpdate;
use serde_json::json;

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

struct GridReader {
    stream: UnixStream,
    codec: FrameCodec,
    queue: std::collections::VecDeque<Frame>,
}

impl GridReader {
    /// The next grid update, and when its bytes were read off the socket.
    fn next_grid(&mut self, deadline: Instant) -> Option<(GridUpdate, Instant)> {
        let mut chunk = [0u8; 64 << 10];
        loop {
            while let Some(frame) = self.queue.pop_front() {
                if frame.frame_type == FrameType::Grid
                    && let Ok(Some(update)) = frame.grid_payload()
                {
                    return Some((update, Instant::now()));
                }
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            self.stream
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .ok()?;
            let count = self.stream.read(&mut chunk).ok()?;
            if count == 0 {
                return None;
            }
            self.queue
                .extend(self.codec.feed(&chunk[..count]).expect("valid frames"));
        }
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * q).round() as usize]
}

fn report(label: &str, mut samples: Vec<f64>) {
    if samples.is_empty() {
        println!("{label:<34} no samples");
        return;
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{label:<34} p50 {:>7.3} ms   p95 {:>7.3} ms   max {:>7.3} ms   (n={})",
        percentile(&samples, 0.5),
        percentile(&samples, 0.95),
        percentile(&samples, 1.0),
        samples.len()
    );
}

fn first_after(events: &[(Hop, Instant)], hop: Hop, after: Instant) -> Option<Instant> {
    events
        .iter()
        .find(|(kind, at)| *kind == hop && *at >= after)
        .map(|(_, at)| *at)
}

fn ms(from: Instant, to: Instant) -> f64 {
    to.saturating_duration_since(from).as_secs_f64() * 1000.0
}

#[test]
#[ignore = "measurement: run with --features latency-trace -- --ignored --nocapture"]
fn keystroke_to_grid_latency_by_hop() {
    let child = std::env::var("DIRI_KEY_LATENCY_CHILD").unwrap_or_else(|_| "cat".into());
    let samples: usize = std::env::var("DIRI_KEY_LATENCY_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let argv = match child.as_str() {
        "zsh" => vec!["/bin/zsh".to_string(), "-f".into(), "-i".into()],
        _ => vec![
            "/bin/sh".to_string(),
            "-c".into(),
            "printf 'ready\\n'; exec cat".into(),
        ],
    };

    // Short root: Holder sockets live under it and must fit SUN_LEN.
    let root = PathBuf::from(format!("/tmp/diri-keylat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let registry = Arc::new(Mutex::new(Registry::new(engine(), root.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: root.join("holders"),
                executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
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
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": argv })),
    });
    let mut replies = std::io::BufReader::new(control.try_clone().expect("clone"));
    let id = loop {
        let mut line = String::new();
        replies.read_line(&mut line).expect("spawn reply");
        if let Ok(ControlMessage::Response { result, .. }) = serde_json::from_str(&line) {
            match result {
                Ok(result) => break result["id"].as_str().expect("id").to_string(),
                Err(error) => panic!("spawn failed: {error:?}"),
            }
        }
    };
    std::thread::sleep(Duration::from_millis(800));

    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");
    let mut grids = GridReader {
        stream: data.try_clone().expect("clone"),
        codec: FrameCodec::new(),
        queue: Default::default(),
    };
    let (seed, _) = grids
        .next_grid(Instant::now() + Duration::from_secs(10))
        .expect("seed");
    let mut cursor = (seed.cursor_col, seed.cursor_row);

    let send_key = |data: &mut UnixStream, byte: u8| {
        let frame = FrameCodec::encode(&Frame::input(vec![byte])).expect("encode");
        let sent = Instant::now();
        data.write_all(&frame).expect("send key");
        sent
    };
    // Waits for the grid that moves the cursor off `from`, i.e. the echo.
    let await_echo = |grids: &mut GridReader, from: (u16, u16)| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some((update, at)) = grids.next_grid(deadline) {
            let now = (update.cursor_col, update.cursor_row);
            if now != from {
                return Some((now, at));
            }
        }
        None
    };

    // Warm-up: let every thread, stream and QoS promotion settle first.
    for _ in 0..10 {
        send_key(&mut data, b'w');
        cursor = await_echo(&mut grids, cursor).expect("warm-up echo").0;
        std::thread::sleep(Duration::from_millis(40));
    }
    latency_trace::drain();

    let mut end_to_end = Vec::new();
    let mut end_to_end_erase = Vec::new();
    let mut hops: Vec<(&str, Vec<f64>)> = vec![
        ("send -> input decoded", Vec::new()),
        ("decoded -> PTY write acked", Vec::new()),
        ("acked -> input handler done", Vec::new()),
        ("decoded -> output received", Vec::new()),
        ("received -> grid published", Vec::new()),
        ("published -> frame enqueued", Vec::new()),
        ("enqueued -> client decoded", Vec::new()),
    ];
    let mut typed_on_line = 0;
    for index in 0..samples {
        if typed_on_line >= 60 {
            // Start a fresh line so the cursor never parks at the margin.
            // Not measured: cat echoes the line back, a different shape.
            send_key(&mut data, b'\r');
            std::thread::sleep(Duration::from_millis(150));
            while let Some((update, _)) =
                grids.next_grid(Instant::now() + Duration::from_millis(50))
            {
                cursor = (update.cursor_col, update.cursor_row);
            }
            latency_trace::drain();
            typed_on_line = 0;
        }
        // Every third key is a backspace: an editing key's echo removes
        // cells, which is the shape the Engine must not mistake for a
        // half-erased repaint.
        let erase = index % 3 == 2;
        let key = if erase {
            0x7f
        } else {
            b'a' + (index % 26) as u8
        };
        let sent = send_key(&mut data, key);
        let Some((now, received)) = await_echo(&mut grids, cursor) else {
            panic!("no echo for sample {index}");
        };
        cursor = now;
        typed_on_line += 1;
        if erase {
            end_to_end_erase.push(ms(sent, received));
        } else {
            end_to_end.push(ms(sent, received));
        }
        // Let trailing hops (a second publication) land before draining.
        std::thread::sleep(Duration::from_millis(40));
        let events = latency_trace::drain();
        let decoded = first_after(&events, Hop::InputDecoded, sent);
        // The echo can overtake the write's acknowledgement, so output is
        // paired with the decode, not the ack.
        let written = decoded.and_then(|at| first_after(&events, Hop::InputWritten, at));
        let handled = written.and_then(|at| first_after(&events, Hop::InputHandled, at));
        let output = decoded.and_then(|at| first_after(&events, Hop::OutputReceived, at));
        let published = output.and_then(|at| first_after(&events, Hop::GridPublished, at));
        let enqueued = published.and_then(|at| first_after(&events, Hop::FrameEnqueued, at));
        let pairs = [
            (Some(sent), decoded),
            (decoded, written),
            (written, handled),
            (decoded, output),
            (output, published),
            (published, enqueued),
            (enqueued, Some(received)),
        ];
        for (slot, pair) in pairs.iter().enumerate() {
            if let (Some(from), Some(to)) = pair {
                hops[slot].1.push(ms(*from, *to));
            }
        }
    }

    println!("\nlocal keystroke latency, child={child}, holder-backed");
    for (label, samples) in hops {
        report(label, samples);
    }
    report("END TO END printable key", end_to_end);
    report("END TO END backspace", end_to_end_erase);

    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&root);
}

/// Output nobody typed for: one byte written to the session's TTY from
/// outside, the way a background program prints, timed to the grid diff that
/// shows it. No input frame is involved, so nothing on the keystroke fast
/// path applies; this is the leading edge of ordinary output.
///
/// ```sh
/// cargo test -p diri-engine --release --features latency-trace \
///     --test keystroke_latency output_to_grid -- --ignored --nocapture
/// ```
///
/// `DIRI_OUTPUT_LATENCY_SAMPLES` sets the sample count (default 200). Writes
/// are 30–60 ms apart, so each one starts after quiet. Cleanup as above.
#[test]
#[ignore = "measurement: run with --features latency-trace -- --ignored --nocapture"]
fn output_to_grid_latency_by_hop() {
    let samples: usize = std::env::var("DIRI_OUTPUT_LATENCY_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let root = PathBuf::from(format!("/tmp/diri-outlat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let tty_file = root.join("tty");
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".into(),
        format!(
            "tty > '{}'; stty -echo; printf 'ready\\n'; exec cat",
            tty_file.display()
        ),
    ];
    let registry = Arc::new(Mutex::new(Registry::new(engine(), root.join("state.json"))));
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: root.join("holders"),
                executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
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
    let send = |message: &ControlMessage| {
        let mut bytes = serde_json::to_vec(message).expect("encode");
        bytes.push(b'\n');
        (&control).write_all(&bytes).expect("write");
    };
    send(&ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(json!({ "kind": { "shell": {} }, "cwd": "/tmp", "argv": argv })),
    });
    let mut replies = std::io::BufReader::new(control.try_clone().expect("clone"));
    let id = loop {
        let mut line = String::new();
        replies.read_line(&mut line).expect("spawn reply");
        if let Ok(ControlMessage::Response { result, .. }) = serde_json::from_str(&line) {
            match result {
                Ok(result) => break result["id"].as_str().expect("id").to_string(),
                Err(error) => panic!("spawn failed: {error:?}"),
            }
        }
    };
    let mut data = UnixStream::connect(server.socket_path()).expect("connect data");
    let mut attach = serde_json::to_vec(&json!({ "attach": id })).expect("encode");
    attach.push(b'\n');
    data.write_all(&attach).expect("attach");
    let mut grids = GridReader {
        stream: data.try_clone().expect("clone"),
        codec: FrameCodec::new(),
        queue: Default::default(),
    };
    let (seed, _) = grids
        .next_grid(Instant::now() + Duration::from_secs(10))
        .expect("seed");
    let mut cursor = (seed.cursor_col, seed.cursor_row);
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
    std::thread::sleep(Duration::from_millis(800));
    while let Some((update, _)) = grids.next_grid(Instant::now() + Duration::from_millis(100)) {
        cursor = (update.cursor_col, update.cursor_row);
    }
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open(&tty)
        .expect("open tty");
    let mut write_byte = |bytes: &[u8]| {
        let sent = Instant::now();
        terminal.write_all(bytes).expect("tty write");
        sent
    };
    let await_change = |grids: &mut GridReader, from: (u16, u16)| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some((update, at)) = grids.next_grid(deadline) {
            let now = (update.cursor_col, update.cursor_row);
            if now != from {
                return Some((now, at));
            }
        }
        None
    };
    for _ in 0..10 {
        write_byte(b"w");
        cursor = await_change(&mut grids, cursor).expect("warm-up").0;
        std::thread::sleep(Duration::from_millis(40));
    }
    latency_trace::drain();

    let mut end_to_end = Vec::new();
    let mut hops: Vec<(&str, Vec<f64>)> = vec![
        ("tty write -> output received", Vec::new()),
        ("received -> grid published", Vec::new()),
        ("published -> frame enqueued", Vec::new()),
        ("enqueued -> client decoded", Vec::new()),
    ];
    for index in 0..samples {
        if index % 60 == 59 {
            write_byte(b"\r\n");
            std::thread::sleep(Duration::from_millis(60));
            while let Some((update, _)) =
                grids.next_grid(Instant::now() + Duration::from_millis(50))
            {
                cursor = (update.cursor_col, update.cursor_row);
            }
            latency_trace::drain();
        }
        let pause = 30 + (index * 7919 % 31) as u64;
        std::thread::sleep(Duration::from_millis(pause));
        let sent = write_byte(b"x");
        let Some((now, received)) = await_change(&mut grids, cursor) else {
            panic!("no grid for sample {index}");
        };
        cursor = now;
        end_to_end.push(ms(sent, received));
        std::thread::sleep(Duration::from_millis(20));
        let events = latency_trace::drain();
        let output = first_after(&events, Hop::OutputReceived, sent);
        let published = output.and_then(|at| first_after(&events, Hop::GridPublished, at));
        let enqueued = published.and_then(|at| first_after(&events, Hop::FrameEnqueued, at));
        let pairs = [
            (Some(sent), output),
            (output, published),
            (published, enqueued),
            (enqueued, Some(received)),
        ];
        for (slot, pair) in pairs.iter().enumerate() {
            if let (Some(from), Some(to)) = pair {
                hops[slot].1.push(ms(*from, *to));
            }
        }
    }

    println!("\nlocal output latency (tty write -> grid at client), holder-backed");
    for (label, samples) in hops {
        report(label, samples);
    }
    report("END TO END tty write -> grid", end_to_end);

    send(&ControlMessage::Request {
        id: 2,
        method: "session.kill".into(),
        params: Some(json!({ "sessionID": id })),
    });
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&root);
}
