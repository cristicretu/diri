//! Measures what the held pump's erase hold costs and what it protects, on
//! real programs: how long each keystroke or tty write takes to reach an
//! attached client, and how many published frames were half a repaint.
//!
//! Ignored because it measures rather than asserts. From `diri/`:
//!
//! ```sh
//! cargo test -p diri-engine --release --test erase_hold_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Workloads run in a private Engine with a real Holder at 100x30: `cat`
//! (written to its tty from outside, so no keystroke is involved), bash,
//! zsh and fish line editing (fish with a scratch HOME, so its history and
//! autosuggestions are the probe's own), `vim -u NONE`, `less`, `top` and
//! `clear`. `DIRI_ERASE_PROBE_CLAUDE` names an executable that starts an
//! isolated Claude Code (scratch config, unreachable API); without it that
//! row is skipped. `DIRI_ERASE_PROBE_ONLY` picks workloads by name, comma
//! separated. `DIRI_ERASE_PROBE_DUMP` names a directory for every recorded
//! frame as JSON.
//!
//! Nothing reaches the network or the developer's own shells' history: the
//! shells start without rc files and fish with a scratch HOME. Sessions are
//! killed and the temporary directory removed at the end.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use serde_json::{Value, json};

const COLS: u16 = 100;
const ROWS: u16 = 30;

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
    socket: PathBuf,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let message = ControlMessage::Request {
            id: self.next,
            method: method.into(),
            params: Some(params),
        };
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).map_err(|e| e.to_string())?;
        loop {
            let mut line = String::new();
            if self
                .reader
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("Engine closed control socket".into());
            }
            if let ControlMessage::Response { id, result, .. } =
                serde_json::from_str::<ControlMessage>(&line).map_err(|e| e.to_string())?
                && id == self.next
            {
                return result.map_err(|error| format!("{error:?}"));
            }
        }
    }

    fn screen(&mut self, id: &str) -> String {
        self.call("session.read_screen", json!({ "sessionID": id }))
            .map(|result| result["text"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    fn wait_screen(&mut self, id: &str, text: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.screen(id).contains(text) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

fn start(temp: &Path) -> Client {
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(engine),
        temp.join("state.json"),
    )));
    let socket = temp.join("daemon.sock");
    let server = Arc::new(
        ControlServer::new(Arc::clone(&registry), socket.clone())
            .with_logs_dir(temp.join("logs"))
            .with_holder(HolderConfig {
                holders_dir: temp.join("holders"),
                executable: PathBuf::from(env!("CARGO_BIN_EXE_diri-holder")),
            }),
    );
    let listener = server.bind().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = server.clone();
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        next: 0,
        socket,
    }
}

/// One published grid as an attached pane saw it.
#[derive(Clone)]
struct Shot {
    at: Instant,
    cols: u16,
    rows: Vec<String>,
}

struct Recorder {
    input: UnixStream,
    frames: Arc<Mutex<Vec<Shot>>>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    fn attach(socket: &Path, id: &str) -> Self {
        let mut stream = UnixStream::connect(socket).unwrap();
        writeln!(
            stream,
            "{}",
            json!({ "attach": id, "enhancedKeyboard": true })
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let input = stream.try_clone().unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let (frames, done) = (frames.clone(), done.clone());
            std::thread::spawn(move || {
                use std::io::Read;
                let mut codec = FrameCodec::new();
                let mut rows: Vec<String> = Vec::new();
                let mut chunk = vec![0u8; 256 << 10];
                while !done.load(Ordering::SeqCst) {
                    let count = match stream.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(count) => count,
                        Err(_) => continue,
                    };
                    let at = Instant::now();
                    for frame in codec.feed(&chunk[..count]).unwrap() {
                        if frame.frame_type != FrameType::Grid {
                            continue;
                        }
                        let Ok(Some(update)) = frame.grid_payload() else {
                            continue;
                        };
                        if update.is_full_snapshot {
                            rows.clear();
                        }
                        rows.resize(usize::from(update.rows), String::new());
                        for row in &update.changed_rows {
                            if let Some(slot) = rows.get_mut(usize::from(row.y)) {
                                *slot = row
                                    .cells
                                    .iter()
                                    .map(|cell| char::from_u32(cell.scalar).unwrap_or(' '))
                                    .collect::<String>()
                                    .trim_end()
                                    .to_string();
                            }
                        }
                        frames.lock().unwrap().push(Shot {
                            at,
                            cols: update.cols,
                            rows: rows.clone(),
                        });
                    }
                }
            })
        };
        Self {
            input,
            frames,
            done,
            thread: Some(thread),
        }
    }

    fn key(&mut self, bytes: &[u8]) -> Instant {
        let at = Instant::now();
        if let Err(error) = self
            .input
            .write_all(&FrameCodec::encode(&Frame::input(bytes.to_vec())).unwrap())
        {
            println!("input refused: {error}");
        }
        at
    }

    fn snapshot(&self) -> Vec<Shot> {
        self.frames.lock().unwrap().clone()
    }

    fn stop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = self.input.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn filled(rows: &[String]) -> usize {
    rows.iter()
        .map(|row| row.chars().filter(|c| !c.is_whitespace()).count())
        .sum()
}

/// How long a frame may stay up and still count as a flash: two 60 Hz
/// display intervals.
const FLASH_LIFETIME: Duration = Duration::from_millis(33);

/// Frames a pane painted as a flicker: a frame holding a whole row less
/// than both neighbours, or a row that vanished (or showed other content
/// in two or more rows) for exactly one short-lived frame and came straight
/// back.
fn flashes(frames: &[Shot]) -> usize {
    let mut found = 0;
    for i in 1..frames.len().saturating_sub(1) {
        let (before, now, after) = (&frames[i - 1].rows, &frames[i].rows, &frames[i + 1].rows);
        let cols = frames[i].cols;
        if before.len() != now.len()
            || now.len() != after.len()
            || frames[i - 1].cols != cols
            || frames[i + 1].cols != cols
        {
            continue;
        }
        // A state the program left on screen is not a flash, however
        // briefly it differs from its neighbours.
        if frames[i + 1].at.duration_since(frames[i].at) > FLASH_LIFETIME {
            continue;
        }
        let dip = filled(now) + usize::from(cols) <= filled(before).min(filled(after));
        let blinked: Vec<usize> = (0..now.len())
            .filter(|&y| !before[y].is_empty() && before[y] == after[y] && now[y] != before[y])
            .collect();
        if dip || blinked.len() >= 2 || blinked.iter().any(|&y| now[y].is_empty()) {
            found += 1;
        }
    }
    found
}

#[derive(Default)]
struct Stats {
    latencies: BTreeMap<String, Vec<f64>>,
    frames: usize,
    flashes: usize,
}

fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return f64::NAN;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

/// One step of a workload: a key (or a tty write) and what to call it.
enum Step {
    Key(&'static str, Vec<u8>),
    Tty(&'static str, Vec<u8>),
    /// Sent but not timed: setup typing.
    Quiet(Vec<u8>),
    Sleep(u64),
}

struct Workload {
    name: &'static str,
    argv: Vec<String>,
    ready: &'static str,
    steps: Vec<Step>,
}

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

fn keys(text: &str) -> Vec<Step> {
    text.bytes().map(|b| Step::Quiet(vec![b])).collect()
}

fn workloads(temp: &Path) -> Vec<Workload> {
    let tty = temp.join("tty");
    let fish_home = temp.join("fish-home");
    std::fs::create_dir_all(fish_home.join(".config/fish")).unwrap();
    std::fs::write(
        fish_home.join(".config/fish/config.fish"),
        "set -g fish_greeting\nfunction fish_prompt; echo -n '> '; end\nfunction fish_right_prompt; end\n",
    )
    .unwrap();
    let text = temp.join("lines.txt");
    let body: String = (1..=4000)
        .map(|n| {
            format!(
                "line {n:05} the quick brown fox jumps over the lazy dog {}\n",
                "#".repeat(n % 37)
            )
        })
        .collect();
    std::fs::write(&text, body).unwrap();
    let text = text.display().to_string();

    let mut list = Vec::new();

    // Unsolicited writes, as #863's window probe made them.
    // A prompt first, as a remote shell's echo has: a lone character on an
    // empty row is indistinguishable from a row cleared for its rewrite.
    let mut steps = vec![Step::Sleep(300), Step::Tty("tty prompt", b"$ ".to_vec())];
    for _ in 0..40 {
        steps.push(Step::Tty("tty x", b"x".to_vec()));
        steps.push(Step::Tty("tty \\b \\b", b"\x08 \x08".to_vec()));
    }
    // A progress line rewritten shorter in one write, and as erase then
    // text in two writes 0.3 ms apart.
    for n in (0..30).rev() {
        steps.push(Step::Tty(
            "tty progress (1 write)",
            format!("\r\x1b[Kitem {n} of 30 {}", "=".repeat(n)).into_bytes(),
        ));
    }
    list.push(Workload {
        name: "cat",
        argv: sh(&format!("tty > '{}'; stty -echo; exec cat", tty.display())),
        ready: "",
        steps,
    });

    for (name, argv) in [
        (
            "bash",
            vec![
                "/usr/bin/env".into(),
                "PS1=$ ".into(),
                "HISTFILE=/dev/null".into(),
                "/bin/bash".into(),
                "--norc".into(),
                "--noprofile".into(),
                "-i".into(),
            ],
        ),
        (
            "zsh",
            vec![
                "/usr/bin/env".into(),
                "PS1=$ ".into(),
                "HISTFILE=/dev/null".into(),
                "/bin/zsh".into(),
                "-f".into(),
                "-i".into(),
            ],
        ),
    ] {
        let mut steps = Vec::new();
        steps.extend(keys(
            "echo a-long-command-line-recalled-from-history-zzzzzzzzzzz\r",
        ));
        steps.extend(keys("true\r"));
        steps.push(Step::Sleep(300));
        for round in 0..6 {
            steps.extend(keys("echo hello world from the probe"));
            for _ in 0..6 {
                steps.push(Step::Key("letter", b"q".to_vec()));
                steps.push(Step::Key("backspace", b"\x7f".to_vec()));
            }
            steps.push(Step::Key("ctrl-w", b"\x17".to_vec()));
            steps.push(Step::Key("ctrl-u", b"\x15".to_vec()));
            steps.extend(keys("echo zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
            // Up recalls `true`, shorter than the line it replaces.
            steps.push(Step::Key("up (shorter line)", b"\x1b[A".to_vec()));
            steps.push(Step::Quiet(b"\x15".to_vec()));
            if round % 2 == 1 {
                steps.extend(keys("seq 1 60\r"));
                steps.push(Step::Sleep(200));
                steps.extend(keys("clear"));
                steps.push(Step::Key("clear\\r", b"\r".to_vec()));
            }
        }
        list.push(Workload {
            name,
            argv,
            ready: "$",
            steps,
        });
    }

    {
        let mut steps = Vec::new();
        steps.extend(keys("echo autosuggest-target-line-for-the-probe\r"));
        steps.extend(keys("echo another-entry\r"));
        steps.push(Step::Sleep(300));
        for _ in 0..12 {
            steps.extend(keys("echo aut"));
            steps.push(Step::Sleep(60));
            // Diverges from the grey suggestion, which fish then erases.
            steps.push(Step::Key("letter killing suggestion", b"Q".to_vec()));
            steps.push(Step::Key("backspace", b"\x7f".to_vec()));
            steps.push(Step::Key("letter", b"q".to_vec()));
            steps.push(Step::Key("backspace", b"\x7f".to_vec()));
            steps.push(Step::Key("ctrl-u", b"\x15".to_vec()));
        }
        list.push(Workload {
            name: "fish",
            argv: vec![
                "/usr/bin/env".into(),
                format!("HOME={}", fish_home.display()),
                format!("XDG_CONFIG_HOME={}/.config", fish_home.display()),
                format!("XDG_DATA_HOME={}/.local/share", fish_home.display()),
                "/opt/homebrew/bin/fish".into(),
                "-i".into(),
            ],
            ready: ">",
            steps,
        });
    }

    {
        let mut steps = vec![Step::Sleep(300)];
        for _ in 0..12 {
            steps.push(Step::Key("vim ctrl-f (page)", b"\x06".to_vec()));
        }
        for _ in 0..12 {
            steps.push(Step::Key("vim x (delete char)", b"x".to_vec()));
        }
        for _ in 0..12 {
            steps.push(Step::Key("vim dd (delete line)", b"dd".to_vec()));
        }
        for _ in 0..8 {
            steps.push(Step::Key("vim ctrl-l (redraw)", b"\x0c".to_vec()));
        }
        steps.push(Step::Quiet(b"\x1b:q!\r".to_vec()));
        list.push(Workload {
            name: "vim",
            argv: vec![
                "/usr/bin/vim".into(),
                "-u".into(),
                "NONE".into(),
                "-N".into(),
                "-n".into(),
                text.clone(),
            ],
            ready: "line 00001",
            steps,
        });
    }

    {
        let mut steps = vec![Step::Sleep(300)];
        for _ in 0..20 {
            steps.push(Step::Key("less space (page)", b" ".to_vec()));
        }
        list.push(Workload {
            name: "less",
            argv: vec!["/usr/bin/less".into(), text.clone()],
            ready: "line 00001",
            steps,
        });
    }

    list.push(Workload {
        name: "top",
        argv: vec![
            "/usr/bin/top".into(),
            "-s".into(),
            "1".into(),
            "-n".into(),
            "20".into(),
        ],
        ready: "Processes",
        steps: vec![Step::Sleep(8000)],
    });

    if let Some(claude) = std::env::var_os("DIRI_ERASE_PROBE_CLAUDE") {
        let mut steps = vec![Step::Sleep(1500)];
        for _ in 0..4 {
            steps.extend(keys("refactor the parser"));
            steps.push(Step::Sleep(100));
            for _ in 0..6 {
                steps.push(Step::Key("letter", b"q".to_vec()));
                steps.push(Step::Key("backspace", b"\x7f".to_vec()));
            }
            for _ in 0..19 {
                steps.push(Step::Key("backspace", b"\x7f".to_vec()));
            }
        }
        list.push(Workload {
            name: "claude",
            argv: vec![claude.to_string_lossy().into_owned()],
            ready: "❯",
            steps,
        });
    }
    list
}

fn run(client: &mut Client, temp: &Path, workload: &Workload, dump: Option<&Path>) -> Stats {
    let tty_path = temp.join("tty");
    let _ = std::fs::remove_file(&tty_path);
    let record = client
        .call(
            "session.spawn",
            json!({
                "kind": { "shell": {} },
                "cwd": temp,
                "argv": workload.argv,
                "initialCols": COLS,
                "initialRows": ROWS,
            }),
        )
        .expect("spawn");
    let id = record["id"].as_str().unwrap().to_string();
    let mut recorder = Recorder::attach(&client.socket.clone(), &id);
    assert!(
        client.wait_screen(&id, workload.ready, Duration::from_secs(20)),
        "{}: never ready\n{}",
        workload.name,
        client.screen(&id)
    );
    std::thread::sleep(Duration::from_millis(500));
    let mut tty = None;
    let mut stats = Stats::default();
    let mut timed: Vec<(String, Instant, usize)> = Vec::new();
    for step in &workload.steps {
        match step {
            Step::Sleep(ms) => std::thread::sleep(Duration::from_millis(*ms)),
            Step::Quiet(bytes) => {
                recorder.key(bytes);
                std::thread::sleep(Duration::from_millis(40));
            }
            Step::Key(label, bytes) => {
                std::thread::sleep(Duration::from_millis(120));
                let before = recorder.snapshot().len();
                let at = recorder.key(bytes);
                timed.push((label.to_string(), at, before));
                std::thread::sleep(Duration::from_millis(150));
            }
            Step::Tty(label, bytes) => {
                let file = tty.get_or_insert_with(|| {
                    let path = std::fs::read_to_string(&tty_path).expect("tty path");
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(path.trim())
                        .expect("open tty")
                });
                std::thread::sleep(Duration::from_millis(120));
                let before = recorder.snapshot().len();
                let at = Instant::now();
                file.write_all(bytes).unwrap();
                timed.push((label.to_string(), at, before));
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let frames = recorder.snapshot();
    if std::env::var_os("DIRI_ERASE_PROBE_SCREEN").is_some() {
        println!("{} final screen:\n{}", workload.name, client.screen(&id));
    }
    recorder.stop();
    for (label, at, before) in timed {
        // The first publication after the input whose screen differs from
        // what the pane showed before it.
        let previous = before.checked_sub(1).map(|i| &frames[i].rows);
        if let Some(shot) = frames[before..]
            .iter()
            .find(|shot| shot.at >= at && Some(&shot.rows) != previous)
        {
            stats
                .latencies
                .entry(label)
                .or_default()
                .push(shot.at.duration_since(at).as_secs_f64() * 1000.0);
        }
    }
    stats.frames = frames.len();
    stats.flashes = flashes(&frames);
    if let Some(dir) = dump {
        let first = frames.first().map(|shot| shot.at);
        let dump: Vec<Value> = frames
            .iter()
            .map(|shot| {
                json!({
                    "ms": first.map(|first| shot.at.duration_since(first).as_secs_f64() * 1000.0),
                    "rows": shot.rows,
                })
            })
            .collect();
        std::fs::write(
            dir.join(format!("{}.json", workload.name)),
            serde_json::to_string(&dump).unwrap(),
        )
        .unwrap();
    }
    let _ = client.call("session.kill", json!({ "sessionID": id }));
    stats
}

#[test]
#[ignore = "measures real programs; run with --ignored --nocapture"]
fn erase_hold_workloads() {
    let dump = std::env::var_os("DIRI_ERASE_PROBE_DUMP").map(PathBuf::from);
    let only: Option<Vec<String>> = std::env::var("DIRI_ERASE_PROBE_ONLY")
        .ok()
        .map(|list| list.split(',').map(str::to_string).collect());
    // Short root: Holder sockets live under it and must fit SUN_LEN.
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mut client = start(temp.path());
    println!("| workload | event | n | p50 ms | p90 ms | max ms |");
    println!("|---|---|---|---|---|---|");
    let mut totals = Vec::new();
    for workload in workloads(temp.path()) {
        if only
            .as_ref()
            .is_some_and(|only| !only.iter().any(|name| name == workload.name))
        {
            continue;
        }
        let stats = run(&mut client, temp.path(), &workload, dump.as_deref());
        for (label, values) in &stats.latencies {
            println!(
                "| {} | {label} | {} | {:.1} | {:.1} | {:.1} |",
                workload.name,
                values.len(),
                percentile(values, 0.5),
                percentile(values, 0.9),
                percentile(values, 1.0),
            );
        }
        totals.push((workload.name, stats.frames, stats.flashes));
    }
    println!();
    println!("| workload | frames | half-repaint frames |");
    println!("|---|---|---|");
    for (name, frames, flashes) in totals {
        println!("| {name} | {frames} | {flashes} |");
    }
}
